//! Work-account authentication ported from Better Teams.
//! See third_party/better-teams/PORT.md and LICENSE for attribution.

use crate::i18n::tr;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
#[cfg(test)]
use std::io::{Read, Write};
#[cfg(test)]
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use ureq::Agent;
use url::Url;

pub(crate) mod calling;
#[cfg(test)]
use calling::{CallingTokenError, CallingTokenRequest, CallingTokenSource, CallingTokenType};

pub(crate) const NATIVE_REDIRECT: &str =
    "https://login.microsoftonline.com/common/oauth2/nativeclient";
const TEAMS_SCOPE: &str = "https://api.spaces.skype.com/.default offline_access";
const GRAPH_SCOPE: &str = "https://graph.microsoft.com/.default offline_access";
// The Teams MSIX image loader asks Graph for this delegated resource scope
// when it retrieves /users/{id}/photo/$value. Keep it separate from the
// Graph .default token used by directory and native API helpers.
const GRAPH_PHOTO_SCOPE: &str = "https://graph.microsoft.com/files.readWrite.all offline_access";

#[derive(Clone)]
pub struct AuthConfig {
    client_id: String,
    redirect_uri: String,
    scope: String,
    authorize_url: String,
    token_url: String,
    device_url: String,
    authz_url: String,
}

impl AuthConfig {
    pub fn work(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into().trim().to_owned(),
            redirect_uri: NATIVE_REDIRECT.to_owned(),
            scope: TEAMS_SCOPE.to_owned(),
            authorize_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".into(),
            token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token".into(),
            device_url: "https://login.microsoftonline.com/common/oauth2/v2.0/devicecode".into(),
            authz_url: "https://teams.microsoft.com/api/authsvc/v1.0/authz".into(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.client_id.is_empty() {
            Err("Configure o client ID da conta de trabalho antes de entrar.".into())
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
pub struct TeamsSession {
    config: AuthConfig,
    tokens: Arc<Mutex<TokenSet>>,
    persistent: Arc<AtomicBool>,
    profile_images: Arc<crate::avatars::ProfileImages>,
}

pub(crate) struct ProfileImageCredentials {
    pub root: String,
    pub aad: String,
    pub skype: String,
    pub cookie_root: String,
    pub owner: String,
    pub valid_for: Duration,
    pub images: Arc<crate::avatars::ProfileImages>,
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    refresh_token: String,
}

/// Short-lived native calendar credentials; deliberately has no Debug output.
pub(crate) struct CalendarCredentials {
    pub root: String,
    pub aad: String,
    pub skype: String,
}

struct AccessToken {
    value: String,
    expires_at: Instant,
}

impl AccessToken {
    fn new(value: String, expires_in: Option<u64>) -> Self {
        Self {
            value,
            expires_at: Instant::now() + Duration::from_secs(expires_in.unwrap_or(3600)),
        }
    }

    fn valid(&self) -> bool {
        self.expires_at > Instant::now() + Duration::from_secs(60)
    }

    fn remaining(&self) -> u64 {
        self.expires_at
            .saturating_duration_since(Instant::now())
            .as_secs()
    }
}

struct TokenSet {
    aad: AccessToken,
    refresh: Option<String>,
    graph: Option<AccessToken>,
    graph_photo: Option<AccessToken>,
    skype: Option<AccessToken>,
    region_gtms: Option<Value>,
    call_token_fallbacks: HashMap<calling::CallingTokenSource, calling::CallingTokenType>,
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
    #[serde(default)]
    error_codes: Vec<u32>,
}

#[derive(Clone)]
pub struct DeviceCodeInfo {
    pub user_code: String,
    pub verification_uri: String,
    /// Page opened in the system browser. May already carry the device code.
    pub browser_url: String,
    pub message: String,
}

#[derive(Clone)]
pub struct DeviceCodeFlow {
    config: AuthConfig,
    device_code: String,
    interval: Duration,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct DeviceCodeReply {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    verification_uri_complete: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Clone)]
pub enum BrowserSignInOutcome {
    Session(TeamsSession),
    UseDeviceCode,
    Cancelled,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WebViewRequest {
    pub authorize_url: String,
    pub redirect_uri: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum WebViewReply {
    Callback { url: String },
    UseDeviceCode,
    Cancelled,
    Failed { message: String },
}

fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(25)))
            .build()
            .new_agent()
    })
}

fn oauth_error(reply: &TokenReply, status: u16, stage: &str) -> String {
    if reply.error_codes.contains(&65002)
        || reply
            .error_description
            .as_deref()
            .is_some_and(|s| s.contains("AADSTS65002"))
    {
        return format!(
            "{stage}: AADSTS65002 — Microsoft refused to authorize this app for the requested resource."
        );
    }
    let detail = reply
        .error_description
        .as_deref()
        .or(reply.error.as_deref())
        .unwrap_or(tr("unrecognized response"));
    format!("{stage} (HTTP {status}): {detail}")
}

fn token_grant(
    config: &AuthConfig,
    form: &[(&str, &str)],
    stage: &str,
) -> Result<TokenReply, String> {
    let mut response = agent()
        .post(&config.token_url)
        .send_form(form.iter().copied())
        .map_err(|_| format!("{stage}: Could not connect to Microsoft."))?;
    let status = response.status().as_u16();
    let reply: TokenReply = response
        .body_mut()
        .read_json()
        .map_err(|_| format!("{stage}: Invalid authentication response."))?;
    if !(200..300).contains(&status) {
        return Err(oauth_error(&reply, status, stage));
    }
    if reply.access_token.as_deref().is_none_or(str::is_empty) {
        return Err(format!(
            "{stage}: Microsoft did not return an access token."
        ));
    }
    Ok(reply)
}

struct BrowserAuth {
    config: AuthConfig,
    verifier: String,
    state: String,
    expires_at: Instant,
}

impl BrowserAuth {
    fn start(config: AuthConfig) -> Result<Self, String> {
        config.validate()?;
        let mut verifier = [0u8; 48];
        let mut state = [0u8; 16];
        OsRng.fill_bytes(&mut verifier);
        OsRng.fill_bytes(&mut state);
        Ok(Self {
            config,
            verifier: URL_SAFE_NO_PAD.encode(verifier),
            state: URL_SAFE_NO_PAD.encode(state),
            expires_at: Instant::now() + Duration::from_secs(600),
        })
    }

    fn authorize_url(&self) -> Result<String, String> {
        self.authorize_url_with(&self.config.redirect_uri)
    }

    fn authorize_url_with(&self, redirect_uri: &str) -> Result<String, String> {
        let mut url = Url::parse(&self.config.authorize_url)
            .map_err(|_| tr("Invalid authorization URL.").to_owned())?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        url.query_pairs_mut()
            .append_pair("client_id", &self.config.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &self.config.scope)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &self.state);
        Ok(url.into())
    }

    fn complete(self, callback: &str) -> Result<TeamsSession, String> {
        let redirect = self.config.redirect_uri.clone();
        self.complete_with(callback, &redirect)
    }

    fn complete_with(self, callback: &str, redirect_uri: &str) -> Result<TeamsSession, String> {
        if Instant::now() >= self.expires_at {
            return Err("A tentativa de login expirou. Entre novamente.".into());
        }
        let code = authorization_code(callback, redirect_uri, &self.state)?;
        // Same token exchange fields as Better Teams' browser_auth.rs.
        let reply = token_grant(
            &self.config,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", redirect_uri),
                ("client_id", &self.config.client_id),
                ("code_verifier", &self.verifier),
            ],
            "Login Microsoft",
        )?;
        TeamsSession::from_login(self.config, reply)
    }
}

#[cfg(test)]
fn await_browser_redirect(
    listener: TcpListener,
    redirect: &str,
    expected_state: &str,
    deadline: Instant,
) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|_| {
        tr("Could not prepare the sign-in callback on this computer.").to_owned()
    })?;
    loop {
        if Instant::now() >= deadline {
            return Err("A tentativa de login expirou. Entre novamente.".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let bytes = read_request(&mut stream);
                let Some(target) = get_target(&bytes) else {
                    let _ = write_html(&mut stream, LOOPBACK_FAIL);
                    continue;
                };
                if target.starts_with("/favicon") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }
                let callback = format!("{redirect}{}", target.trim_start_matches('/'));
                match authorization_code(&callback, redirect, expected_state) {
                    Ok(_) => {
                        let _ = write_html(&mut stream, LOOPBACK_OK);
                        return Ok(callback);
                    }
                    Err(error) => {
                        let _ = write_html(&mut stream, LOOPBACK_FAIL);
                        if target.contains("code=") || target.contains("error=") {
                            return Err(error);
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(40));
            }
            Err(_) => {
                return Err(
                    tr("Could not receive the sign-in callback on this computer.").into(),
                );
            }
        }
    }
}

#[cfg(test)]
const LOOPBACK_OK: &str = "<!DOCTYPE html><meta charset=\"utf-8\"><title>Teams Linux</title><p>Login recebido. Pode fechar esta aba e voltar ao Teams Linux.</p>";
#[cfg(test)]
const LOOPBACK_FAIL: &str = "<!DOCTYPE html><meta charset=\"utf-8\"><title>Teams Linux</title><p>O login não foi concluído. Volte ao Teams Linux e tente de novo.</p>";

#[cfg(test)]
fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 2048];
    while bytes.len() < 8192 && !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
        }
    }
    bytes
}

#[cfg(test)]
fn get_target(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut parts = text.lines().next()?.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    (target.starts_with('/') && !target.starts_with("//") && !target.contains('\\')).then_some(target)
}

#[cfg(test)]
fn write_html(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(header.as_bytes())
}

fn microsoft_login_page(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    let host = url.host_str().unwrap_or("");
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && (host == "login.microsoft.com"
            || host == "login.microsoftonline.com"
            || host == "login.windows.net"
            || host == "microsoft.com"
            || host == "www.microsoft.com"
            || host.ends_with(".microsoft.com")
            || host.ends_with(".microsoftonline.com"))
}

pub(crate) fn is_native_callback(candidate: &str, redirect_uri: &str) -> bool {
    let (Ok(candidate), Ok(expected)) = (Url::parse(candidate), Url::parse(redirect_uri)) else {
        return false;
    };
    candidate.scheme() == expected.scheme()
        && candidate.host_str() == expected.host_str()
        && candidate.port_or_known_default() == expected.port_or_known_default()
        && candidate.path() == expected.path()
        && candidate.username().is_empty()
        && candidate.password().is_none()
}

fn authorization_code(
    callback: &str,
    redirect: &str,
    expected_state: &str,
) -> Result<String, String> {
    if !is_native_callback(callback, redirect) {
        return Err(tr("The sign-in callback came from an unexpected address.").into());
    }
    let url = Url::parse(callback).map_err(|_| tr("Invalid sign-in callback.").to_owned())?;
    let raw = url
        .query()
        .filter(|s| !s.is_empty())
        .or(url.fragment())
        .unwrap_or("");
    let pairs: Vec<_> = url::form_urlencoded::parse(raw.as_bytes()).collect();
    let value = |key: &str| -> Result<Option<String>, String> {
        let mut values = pairs.iter().filter(|(name, _)| name == key);
        let result = values.next().map(|(_, value)| value.to_string());
        if values.next().is_some() {
            return Err(tr("The sign-in callback contains duplicate parameters.").into());
        }
        Ok(result)
    };
    if value("state")?.as_deref() != Some(expected_state) {
        return Err(tr("The sign-in security check failed. Sign in again.").into());
    }
    if let Some(error) = value("error")? {
        let reply = TokenReply {
            access_token: None,
            refresh_token: None,
            expires_in: None,
            error: Some(error),
            error_description: value("error_description")?,
            error_codes: vec![],
        };
        return Err(oauth_error(&reply, 400, "Login Microsoft"));
    }
    value("code")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| tr("The sign-in callback did not contain an authorization code.").into())
}

pub fn browser_sign_in(config: AuthConfig) -> Result<BrowserSignInOutcome, String> {
    let flow = BrowserAuth::start(config)?;
    let reply = crate::auth_webview::capture(WebViewRequest {
        authorize_url: flow.authorize_url()?,
        redirect_uri: flow.config.redirect_uri.clone(),
    })?;
    match reply {
        WebViewReply::Callback { url } => flow.complete(&url).map(BrowserSignInOutcome::Session),
        WebViewReply::UseDeviceCode => Ok(BrowserSignInOutcome::UseDeviceCode),
        WebViewReply::Cancelled => Ok(BrowserSignInOutcome::Cancelled),
        WebViewReply::Failed { message } => Err(message),
    }
}

pub fn start_device_code(config: AuthConfig) -> Result<(DeviceCodeFlow, DeviceCodeInfo), String> {
    config.validate()?;
    let mut response = agent()
        .post(&config.device_url)
        .send_form([
            ("client_id", config.client_id.as_str()),
            ("scope", config.scope.as_str()),
        ])
        .map_err(|_| tr("Could not start Microsoft sign-in.").to_owned())?;
    let status = response.status().as_u16();
    let reply: DeviceCodeReply = response
        .body_mut()
        .read_json()
        .map_err(|_| tr("Invalid authentication response.").to_owned())?;
    if !(200..300).contains(&status) {
        return Err(oauth_error(
            &TokenReply {
                access_token: None,
                refresh_token: None,
                expires_in: None,
                error: reply.error,
                error_description: reply.error_description,
                error_codes: vec![],
            },
            status,
            tr("Device code"),
        ));
    }
    let flow = DeviceCodeFlow {
        config,
        device_code: reply
            .device_code
            .ok_or(tr("Microsoft did not return a device code."))?,
        interval: Duration::from_secs(reply.interval.unwrap_or(5).max(1)),
        expires_at: Instant::now() + Duration::from_secs(reply.expires_in.unwrap_or(900)),
    };
    let verification_uri = reply
        .verification_uri
        .ok_or(tr("Microsoft did not return the sign-in page."))?;
    let browser_url = reply
        .verification_uri_complete
        .filter(|url| microsoft_login_page(url))
        .unwrap_or_else(|| verification_uri.clone());
    let info = DeviceCodeInfo {
        user_code: reply.user_code.ok_or(tr("Microsoft did not return a sign-in code."))?,
        verification_uri,
        browser_url,
        message: tr("This step opens in your default browser. Follow the prompts for your account, password, and Authenticator. The code below is filled in if the Microsoft page provides it.").into(),
    };
    Ok((flow, info))
}

pub fn complete_device_code(flow: DeviceCodeFlow) -> Result<TeamsSession, String> {
    let mut interval = flow.interval;
    while Instant::now() + interval < flow.expires_at {
        thread::sleep(interval);
        let mut response = agent()
            .post(&flow.config.token_url)
            .send_form([
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", flow.config.client_id.as_str()),
                ("device_code", flow.device_code.as_str()),
            ])
            .map_err(|_| tr("Could not check Microsoft sign-in.").to_owned())?;
        let status = response.status().as_u16();
        let reply: TokenReply = response
            .body_mut()
            .read_json()
            .map_err(|_| tr("Invalid authentication response.").to_owned())?;
        if (200..300).contains(&status) {
            return TeamsSession::from_login(flow.config, reply);
        }
        match reply.error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => interval += Duration::from_secs(5),
            Some("authorization_declined" | "access_denied") => {
                return Err("O login foi cancelado ou recusado.".into());
            }
            Some("expired_token") => break,
            _ => return Err(oauth_error(&reply, status, tr("Device code"))),
        }
    }
    Err(tr("The sign-in code expired. Start sign-in again.").into())
}

impl TeamsSession {
    /// Synthetic, nonpersistent session for reducer tests. Never performs a grant.
    #[cfg(test)]
    pub(crate) fn test_fixture() -> Self {
        Self {
            config: AuthConfig::work("fixture-client"),
            persistent: Arc::new(AtomicBool::new(false)),
            profile_images: Arc::new(crate::avatars::ProfileImages::default()),
            tokens: Arc::new(Mutex::new(TokenSet {
                aad: AccessToken::new("fixture-aad".into(), Some(3600)),
                refresh: None,
                graph: None,
                graph_photo: None,
                skype: Some(AccessToken::new("fixture-skype".into(), Some(3600))),
                region_gtms: None,
                call_token_fallbacks: HashMap::new(),
            })),
        }
    }

    fn from_login(config: AuthConfig, reply: TokenReply) -> Result<Self, String> {
        let aad = AccessToken::new(
            reply.access_token.ok_or(tr("Sign-in returned no access token."))?,
            reply.expires_in,
        );
        let session = Self {
            config,
            persistent: Arc::new(AtomicBool::new(false)),
            profile_images: Arc::new(crate::avatars::ProfileImages::default()),
            tokens: Arc::new(Mutex::new(TokenSet {
                aad,
                refresh: reply.refresh_token,
                graph: None,
                graph_photo: None,
                skype: None,
                region_gtms: None,
                call_token_fallbacks: HashMap::new(),
            })),
        };
        // Derived-token failures stay separate from the initial login, as upstream.
        {
            let mut tokens = session
                .tokens
                .lock()
                .map_err(|_| tr("Could not open the session.").to_owned())?;
            let _ = session.ensure_skype(&mut tokens);
            let _ = session.ensure_graph(&mut tokens);
        }
        Ok(session)
    }

    fn refresh_grant(
        &self,
        tokens: &mut TokenSet,
        scope: &str,
        stage: &str,
    ) -> Result<AccessToken, String> {
        let refresh = tokens
            .refresh
            .as_deref()
            .ok_or(tr("This session has no refresh token. Sign in again."))?;
        let reply = token_grant(
            &self.config,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &self.config.client_id),
                ("refresh_token", refresh),
                ("scope", scope),
            ],
            stage,
        )?;
        if let Some(rotated) = reply.refresh_token {
            tokens.refresh = Some(rotated);
            if self.persistent.load(Ordering::Relaxed) {
                if self.save_refresh(tokens).is_err() {
                    self.persistent.store(false, Ordering::Relaxed);
                }
            }
        }
        Ok(AccessToken::new(
            reply.access_token.ok_or(tr("Token refresh returned no access token."))?,
            reply.expires_in,
        ))
    }

    fn ensure_graph(&self, tokens: &mut TokenSet) -> Result<(), String> {
        if tokens.graph.as_ref().is_some_and(AccessToken::valid) {
            return Ok(());
        }
        tokens.graph = Some(self.refresh_grant(tokens, GRAPH_SCOPE, "Token Microsoft Graph")?);
        Ok(())
    }

    fn ensure_graph_photo(&self, tokens: &mut TokenSet) -> Result<(), String> {
        if tokens.graph_photo.as_ref().is_some_and(AccessToken::valid) {
            return Ok(());
        }
        tokens.graph_photo = Some(self.refresh_grant(
            tokens,
            GRAPH_PHOTO_SCOPE,
            "Token Microsoft Graph para fotos",
        )?);
        Ok(())
    }

    fn ensure_skype(&self, tokens: &mut TokenSet) -> Result<(), String> {
        if tokens.skype.as_ref().is_some_and(AccessToken::valid) {
            return Ok(());
        }
        if !tokens.aad.valid() {
            tokens.aad = self.refresh_grant(tokens, &self.config.scope, tr("Teams token refresh"))?;
        }
        let mut response = agent()
            .post(&self.config.authz_url)
            .header("Authorization", &format!("Bearer {}", tokens.aad.value))
            .header("Content-Length", "0")
            .send_empty()
            .map_err(|_| tr("Could not open the Teams/Skype session.").to_owned())?;
        let status = response.status().as_u16();
        let body: Value = response
            .body_mut()
            .read_json()
            .map_err(|_| tr("Invalid Teams/Skype session response.").to_owned())?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "The Teams/Skype session was rejected (HTTP {status})."
            ));
        }
        let token = body
            .pointer("/tokens/skypeToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(tr("Microsoft did not return a Teams/Skype token."))?;
        tokens.skype = Some(AccessToken::new(
            token.to_owned(),
            body.pointer("/tokens/expiresIn").and_then(Value::as_u64),
        ));
        tokens.region_gtms = body.get("regionGtms").cloned();
        Ok(())
    }

    pub(crate) fn access_token(&self) -> Result<String, String> {
        let mut tokens = self
            .tokens
            .lock()
            .map_err(|_| tr("Could not access the session.").to_owned())?;
        self.ensure_graph(&mut tokens)?;
        Ok(tokens
            .graph
            .as_ref()
            .expect("graph grant succeeded")
            .value
            .clone())
    }

    /// The original Teams desktop requests this specific Graph scope for
    /// profile photos. Keep it separate so photo consent does not change the
    /// token used by directory lookups and other Graph-backed features.
    pub(crate) fn photo_access_token(&self) -> Result<String, String> {
        let mut tokens = self
            .tokens
            .lock()
            .map_err(|_| tr("Could not access the session.").to_owned())?;
        self.ensure_graph_photo(&mut tokens)?;
        Ok(tokens
            .graph_photo
            .as_ref()
            .expect("photo Graph grant succeeded")
            .value
            .clone())
    }

    /// Return the Teams middle-tier token and the tenant's discovered UPS
    /// endpoint. Presence uses a Teams service contract in the desktop app;
    /// it must not reuse a Microsoft Graph token for that audience.
    pub(crate) fn presence_credentials(&self) -> Result<(String, String), String> {
        let mut tokens = self
            .tokens
            .lock()
            .map_err(|_| tr("Could not access the session.").to_owned())?;
        if !tokens.aad.valid() {
            tokens.aad = self.refresh_grant(&mut tokens, &self.config.scope, tr("Teams token refresh"))?;
        }
        let base = tokens
            .region_gtms
            .as_ref()
            .and_then(|gtms| {
                gtms.get("unifiedPresence")
                    .or_else(|| gtms.get("presenceUPS"))
            })
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| tr("The session did not identify a regional Teams presence service.").to_owned())?;
        let url = Url::parse(base)
            .map_err(|_| tr("The regional Teams presence URL is invalid.").to_owned())?;
        let host = url.host_str().unwrap_or_default();
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port_or_known_default() != Some(443)
            || !(host == "teams.microsoft.com"
                || host.ends_with(".teams.microsoft.com")
                || host.ends_with(".teams.office.com")
                || host == "skype.com"
                || host.ends_with(".skype.com"))
        {
            return Err(tr("Teams returned an unexpected presence URL.").into());
        }
        Ok((url.to_string().trim_end_matches('/').to_owned(), tokens.aad.value.clone()))
    }

    /// Return the current Teams/Skype token for the embedded Trouter worker.
    /// The token stays in memory and is never formatted or logged here.
    pub(crate) fn skype_token(&self) -> Result<String, String> {
        let mut tokens = self
            .tokens
            .lock()
            .map_err(|_| tr("Could not access the session.").to_owned())?;
        self.ensure_skype(&mut tokens)?;
        Ok(tokens
            .skype
            .as_ref()
            .expect("skype grant succeeded")
            .value
            .clone())
    }

    fn save_refresh(&self, tokens: &TokenSet) -> Result<(), String> {
        let refresh = tokens
            .refresh
            .as_ref()
            .ok_or(tr("This session cannot be saved without a refresh token."))?;
        let secret = serde_json::to_string(&SavedSession {
            refresh_token: refresh.clone(),
        })
        .map_err(|_| tr("Could not prepare the session for the keyring.").to_owned())?;
        crate::credentials::save(
            &crate::credentials::profile_key(&self.config.client_id),
            &secret,
        )
    }

    pub fn remember(&self) -> Result<(), String> {
        if self.is_persistent() {
            return Ok(());
        }
        let tokens = self
            .tokens
            .lock()
            .map_err(|_| tr("Could not access the session.").to_owned())?;
        self.save_refresh(&tokens)?;
        self.persistent.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn is_persistent(&self) -> bool {
        self.persistent.load(Ordering::Relaxed)
    }

    pub fn disable_persistence(&self) {
        self.persistent.store(false, Ordering::Relaxed);
    }

    pub(crate) fn native_client(&self) -> Result<ost::api::client::TeamsClient, String> {
        self.build_native_client(true)
    }

    /// Middle-tier operations use the existing Teams credentials, without
    /// attempting a Graph grant as a side effect of opening the calendar.
    pub(crate) fn native_teams_client(&self) -> Result<ost::api::client::TeamsClient, String> {
        self.build_native_client(false)
    }

    fn build_native_client(&self, include_graph: bool) -> Result<ost::api::client::TeamsClient, String> {
        use ost::auth::TokenStore;
        let mut tokens = self
            .tokens
            .lock()
            .map_err(|_| tr("Could not access the session.").to_owned())?;
        if !tokens.aad.valid() {
            tokens.aad = self.refresh_grant(&mut tokens, &self.config.scope, tr("Teams token refresh"))?;
        }
        self.ensure_skype(&mut tokens)?;
        if include_graph { let _ = self.ensure_graph(&mut tokens); }
        let mut config = ost::config::Config::default();
        config.set_access_token(tokens.aad.value.clone(), Some(tokens.aad.remaining()));
        if let Some(skype) = &tokens.skype {
            config.set_skype_token(skype.value.clone(), Some(skype.remaining()));
        }
        if let Some(graph) = &tokens.graph {
            config.set_graph_token(graph.value.clone(), Some(graph.remaining()));
        }
        if let Some(gtms) = &tokens.region_gtms {
            config.set_region_gtms(gtms.clone());
        }
        Ok(ost::api::client::TeamsClient::from_config(config))
    }

    pub(crate) fn graph_bearer(&self) -> Result<String, String> {
        let mut tokens = self
            .tokens
            .lock()
            .map_err(|_| tr("Could not access the session.").to_owned())?;
        self.ensure_graph(&mut tokens)?;
        tokens
            .graph
            .as_ref()
            .map(|token| token.value.clone())
            .ok_or_else(|| tr("Could not authorize meeting creation.").to_owned())
    }

    pub(crate) fn calendar_credentials(&self) -> Result<CalendarCredentials, String> {
        let client = self.native_teams_client()?;
        let tokens = self.tokens.lock().map_err(|_| tr("Could not access the calendar session.").to_owned())?;
        Ok(CalendarCredentials { root: client.middle_tier_url(), aad: tokens.aad.value.clone(),
            skype: tokens.skype.as_ref().ok_or(tr("The Teams session expired."))?.value.clone() })
    }

    /// In-memory credentials for the regional image service observed in the
    /// original avatar URL generators. No new grant or application ID.
    pub(crate) fn profile_image_credentials(&self, owner: &str) -> Result<ProfileImageCredentials, String> {
        let client = self.native_client()?;
        let mut tokens = self.tokens.lock()
            .map_err(|_| tr("Could not access the photo session.").to_owned())?;
        if !tokens.aad.valid() {
            tokens.aad = self.refresh_grant(&mut tokens, &self.config.scope, tr("Teams photo token refresh"))?;
        }
        let fallback = client.middle_tier_url();
        let root = tokens.region_gtms.as_ref()
            .and_then(|gtms| gtms.get("mtImageService"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&fallback);
        let root = crate::avatars::validate_image_root(root)?;
        Ok(ProfileImageCredentials {
            root,
            cookie_root: crate::avatars::validate_image_root(&fallback)?,
            owner: crate::avatars::person_id(owner).ok_or(tr("Invalid identity for the photo account."))?,
            valid_for: Duration::from_secs(tokens.aad.remaining().min(
                tokens.skype.as_ref().map_or(0, AccessToken::remaining),
            ).saturating_sub(30)),
            images: self.profile_images.clone(),
            aad: tokens.aad.value.clone(),
            skype: tokens.skype.as_ref()
                .ok_or(tr("The photo session expired."))?.value.clone(),
        })
    }

    pub(crate) fn image_credentials(&self) -> Result<(String, String, String), String> {
        let client = self.native_client()?;
        let tokens = self
            .tokens
            .lock()
            .map_err(|_| tr("Could not access the session.").to_owned())?;
        let token = tokens
            .skype
            .as_ref()
            .ok_or_else(|| tr("The media session expired.").to_owned())?
            .value
            .clone();
        let base = tokens
            .region_gtms
            .as_ref()
            .and_then(|gtms| gtms.get("ams").or_else(|| gtms.get("amsV2")))
            .and_then(Value::as_str)
            .unwrap_or("https://api.asm.skype.com");
        let mut url = url::Url::parse(base)
            .map_err(|_| tr("Teams returned an invalid media server.").to_owned())?;
        let host = url.host_str().unwrap_or("");
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port_or_known_default() != Some(443)
            || !(host == "api.asm.skype.com"
                || host.ends_with(".asm.skype.com")
                || host.ends_with(".asyncgw.teams.microsoft.com"))
        {
            return Err(tr("Teams returned an unexpected media server.").into());
        }
        url.set_path("");
        url.set_query(None);
        url.set_fragment(None);
        Ok((
            client.chat_service_url(),
            url.to_string().trim_end_matches('/').to_owned(),
            token,
        ))
    }

    /// Hints affect display only. Service access always requires its own token.
    pub(crate) fn account_hint(&self) -> (String, String) {
        let Ok(tokens) = self.tokens.lock() else {
            return (String::new(), "Conta Microsoft".into());
        };
        let claims = tokens
            .aad
            .value
            .split('.')
            .nth(1)
            .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let id = claims
            .as_ref()
            .and_then(|v| v.get("oid"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let name = claims
            .as_ref()
            .and_then(|v| v.get("name").or(v.get("preferred_username")))
            .and_then(Value::as_str)
            .unwrap_or("Conta Microsoft")
            .to_owned();
        (id, name)
    }
}

pub fn restore_session(config: AuthConfig) -> Result<Option<TeamsSession>, String> {
    let key = crate::credentials::profile_key(&config.client_id);
    let Some(secret) = crate::credentials::load(&key)? else {
        return Ok(None);
    };
    let saved: SavedSession = serde_json::from_str(&secret)
        .map_err(|_| tr("Could not restore the saved session. Sign in again.").to_owned())?;
    let reply = token_grant(
        &config,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &config.client_id),
            ("refresh_token", &saved.refresh_token),
            ("scope", &config.scope),
        ],
        tr("Restore session"),
    )
    .map_err(|_| tr("The saved session expired or requires confirmation. Sign in again.").to_owned())?;
    let session = TeamsSession::from_login(config, reply)?;
    let _ = session.remember();
    Ok(Some(session))
}

pub fn forget_session(session: TeamsSession) -> Result<(), String> {
    session.disable_persistence();
    // Wait for an in-flight token rotation/store before deleting the credential.
    let _tokens = session
        .tokens
        .lock()
        .map_err(|_| tr("Could not end the saved session.").to_owned())?;
    crate::credentials::clear(&crate::credentials::profile_key(&session.config.client_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener, TcpStream};

    struct Step {
        path: &'static str,
        status: u16,
        body: Value,
        form: Vec<(&'static str, &'static str)>,
        bearer: Option<&'static str>,
    }

    fn mock(steps: Vec<Step>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = thread::spawn(move || {
            for step in steps {
                let deadline = Instant::now() + Duration::from_secs(10);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(pair) => break pair,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("mock connection failed: {e}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let (headers_end, body_len) = loop {
                    let mut chunk = [0; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let len = headers
                            .lines()
                            .find_map(|line| {
                                line.split_once(':')
                                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                    .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (end + 4, len);
                    }
                };
                while bytes.len() < headers_end + body_len {
                    let mut chunk = [0; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                assert_eq!(
                    headers.lines().next().unwrap().split_whitespace().nth(1),
                    Some(step.path)
                );
                if let Some(bearer) = step.bearer {
                    assert!(headers.lines().any(|line| {
                        line.eq_ignore_ascii_case(&format!("Authorization: Bearer {bearer}"))
                    }));
                }
                let form: HashMap<_, _> = url::form_urlencoded::parse(&bytes[headers_end..])
                    .into_owned()
                    .collect();
                for (name, expected) in step.form {
                    assert_eq!(form.get(name).map(String::as_str), Some(expected));
                }
                let body = step.body.to_string();
                write!(stream, "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", step.status, body.len(), body).unwrap();
            }
        });
        (base, task)
    }

    fn config_for(base: &str) -> AuthConfig {
        let mut config = AuthConfig::work("test-owned-client");
        config.token_url = format!("{base}/token");
        config.device_url = format!("{base}/devicecode");
        config.authz_url = format!("{base}/authz");
        config
    }

    fn session_with_tokens(
        config: AuthConfig,
        aad: AccessToken,
        refresh: Option<&str>,
        graph: Option<AccessToken>,
        skype: Option<AccessToken>,
    ) -> TeamsSession {
        TeamsSession {
            config,
            persistent: Arc::new(AtomicBool::new(false)),
            profile_images: Arc::new(crate::avatars::ProfileImages::default()),
            tokens: Arc::new(Mutex::new(TokenSet {
                aad,
                refresh: refresh.map(str::to_owned),
                graph,
                graph_photo: None,
                skype,
                region_gtms: None,
                call_token_fallbacks: HashMap::new(),
            })),
        }
    }

    #[test]
    fn native_calendar_client_does_not_attempt_a_graph_grant() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}",server.server_addr());
        let listener = thread::spawn(move || {
            let request = server.recv_timeout(Duration::from_millis(150)).unwrap();
            if let Some(request) = request { request.respond(tiny_http::Response::empty(403)).unwrap(); 1 } else { 0 }
        });
        let session = session_with_tokens(config_for(&base), AccessToken::new("fixture-aad".into(),Some(3600)),
            Some("fixture-refresh"), None, Some(AccessToken::new("fixture-skype".into(),Some(3600))));
        session.native_teams_client().unwrap();
        assert!(session.tokens.lock().unwrap().graph.is_none());
        assert_eq!(listener.join().unwrap(),0,"calendar must not request a Graph grant");
    }

    #[test]
    fn profile_image_cookie_jars_are_shared_only_by_the_same_session() {
        let a = TeamsSession::test_fixture();
        let clone = a.clone();
        let b = TeamsSession::test_fixture();
        assert!(Arc::ptr_eq(&a.profile_images, &clone.profile_images));
        assert!(!Arc::ptr_eq(&a.profile_images, &b.profile_images));
    }

    #[test]
    fn image_cookie_credentials_refresh_an_expired_aad_even_when_skype_is_valid() {
        let (base, worker) = mock(vec![token_step("image-aad-new", "refresh-2", vec![
            ("scope", TEAMS_SCOPE), ("refresh_token", "refresh-1"),
        ])]);
        let session = session_with_tokens(config_for(&base),
            AccessToken::new("aad-expired".into(), Some(0)), Some("refresh-1"),
            Some(AccessToken::new("graph-valid".into(), Some(3600))),
            Some(AccessToken::new("skype-valid".into(), Some(3600))),
        );
        let credentials = session.profile_image_credentials("11111111-1111-1111-1111-111111111111").unwrap();
        assert_eq!(credentials.aad, "image-aad-new");
        assert_eq!(credentials.skype, "skype-valid");
        assert!(credentials.valid_for > Duration::from_secs(60));
        worker.join().unwrap();
    }

    #[test]
    fn profile_photo_uses_the_original_graph_resource_scope_separately() {
        let (base, worker) = mock(vec![token_step(
            "photo-graph-token",
            "refresh-2",
            vec![
                ("scope", GRAPH_PHOTO_SCOPE),
                ("refresh_token", "refresh-1"),
            ],
        )]);
        let session = session_with_tokens(
            config_for(&base),
            AccessToken::new("teams-aad".into(), Some(3600)),
            Some("refresh-1"),
            Some(AccessToken::new("graph-default-token".into(), Some(3600))),
            None,
        );

        assert_eq!(session.photo_access_token().unwrap(), "photo-graph-token");
        let tokens = session.tokens.lock().unwrap();
        assert_eq!(tokens.graph.as_ref().unwrap().value, "graph-default-token");
        assert_eq!(tokens.graph_photo.as_ref().unwrap().value, "photo-graph-token");
        drop(tokens);
        worker.join().unwrap();
    }

    fn token_step(token: &str, refresh: &str, form: Vec<(&'static str, &'static str)>) -> Step {
        Step {
            path: "/token",
            status: 200,
            body: serde_json::json!({
                "access_token": token, "refresh_token": refresh, "expires_in": 3600,
            }),
            form,
            bearer: None,
        }
    }

    fn skype_step() -> Step {
        Step {
            path: "/authz",
            status: 200,
            body: serde_json::json!({"tokens":{"skypeToken":"skype-test","expiresIn":3600}}),
            form: vec![],
            bearer: Some("aad-test"),
        }
    }

    #[test]
    fn calling_token_respects_source_order_and_never_uses_graph() {
        let session = session_with_tokens(
            AuthConfig::work("test-owned-client"),
            AccessToken::new("aad-call".into(), Some(3600)),
            Some("refresh-1"),
            Some(AccessToken::new("graph-only".into(), Some(3600))),
            Some(AccessToken::new("skype-call".into(), Some(3600))),
        );
        let supported = [
            CallingTokenType::Cae,
            CallingTokenType::Skype,
            CallingTokenType::Aad,
        ];
        let token = session
            .calling_token(CallingTokenRequest {
                source: CallingTokenSource::CallStore,
                supported_types: &supported,
                challenge: None,
                force_renew: false,
            })
            .unwrap();

        assert_eq!(token.source, CallingTokenSource::CallStore);
        assert_eq!(token.token_type, CallingTokenType::Skype);
        assert_eq!(token.expose_to_transport(), "skype-call");
        assert!(!format!("{token:?}").contains("skype-call"));
        assert!(format!("{token:?}").contains("[REDACTED]"));
    }

    #[test]
    fn trouter_auth_adapter_uses_the_server_order_and_wire_type() {
        let session = session_with_tokens(
            AuthConfig::work("test-owned-client"),
            AccessToken::new("aad-call".into(), Some(3600)),
            Some("refresh-1"),
            Some(AccessToken::new("graph-only".into(), Some(3600))),
            Some(AccessToken::new("skype-call".into(), Some(3600))),
        );

        let aad_first = crate::call_auth::resolve_trouter_token(
            &session,
            crate::call_auth::TrouterAuthRequest {
                supported_token_types: &["cae", "aad", "skype"],
                www_authenticate_header: None,
                need_fresh: false,
            },
        )
        .unwrap();
        assert_eq!(aad_first.token_type, "aad");
        assert_eq!(aad_first.expose_to_transport(), "aad-call");

        let skype_first = crate::call_auth::resolve_trouter_token(
            &session,
            crate::call_auth::TrouterAuthRequest {
                supported_token_types: &["cae", "skype", "aad"],
                www_authenticate_header: None,
                need_fresh: false,
            },
        )
        .unwrap();
        assert_eq!(skype_first.token_type, "skype");
        assert_eq!(skype_first.expose_to_transport(), "skype-call");
        assert!(!format!("{skype_first:?}").contains("skype-call"));
    }

    #[test]
    fn calling_token_fallback_is_kept_per_source_and_limits_later_types() {
        let session = session_with_tokens(
            AuthConfig::work("test-owned-client"),
            AccessToken::new("aad-call".into(), Some(3600)),
            Some("refresh-1"),
            None,
            Some(AccessToken::new("skype-call".into(), Some(3600))),
        );
        session
            .remember_call_token_fallback(
                CallingTokenSource::TrouterCallingWeb,
                CallingTokenType::Cae,
            )
            .unwrap();
        {
            let tokens = session.tokens.lock().unwrap();
            assert_eq!(
                tokens
                    .call_token_fallbacks
                    .get(&CallingTokenSource::TrouterCallingWeb),
                Some(&CallingTokenType::Cae)
            );
            assert!(!tokens
                .call_token_fallbacks
                .contains_key(&CallingTokenSource::CallStore));
        }

        let supported = [
            CallingTokenType::Cae,
            CallingTokenType::Skype,
            CallingTokenType::Aad,
        ];
        let trouter_result = session.calling_token(CallingTokenRequest {
            source: CallingTokenSource::TrouterCallingWeb,
            supported_types: &supported,
            challenge: None,
            force_renew: false,
        });
        assert!(matches!(
            trouter_result,
            Err(CallingTokenError::NoSupportedCredential)
        ));

        let call_store_token = session
            .calling_token(CallingTokenRequest {
                source: CallingTokenSource::CallStore,
                supported_types: &supported,
                challenge: None,
                force_renew: false,
            })
            .unwrap();
        assert_eq!(call_store_token.token_type, CallingTokenType::Skype);
        assert_eq!(call_store_token.expose_to_transport(), "skype-call");
    }

    #[test]
    fn calling_token_does_not_reuse_credentials_after_a_claims_challenge() {
        let session = session_with_tokens(
            AuthConfig::work("test-owned-client"),
            AccessToken::new("aad-call".into(), Some(3600)),
            Some("refresh-1"),
            None,
            Some(AccessToken::new("skype-call".into(), Some(3600))),
        );
        let supported = [CallingTokenType::Skype, CallingTokenType::Aad];
        let result = session.calling_token(CallingTokenRequest {
            source: CallingTokenSource::TrouterCallingWeb,
            supported_types: &supported,
            challenge: Some("claims=private-fixture"),
            force_renew: false,
        });

        assert!(matches!(
            result,
            Err(CallingTokenError::ClaimsChallengeUnsupported)
        ));
        assert!(!format!("{result:?}").contains("private-fixture"));
    }

    #[test]
    fn calling_token_refreshes_aad_for_teams_scope_without_touching_graph() {
        let (base, task) = mock(vec![token_step(
            "aad-call-new",
            "refresh-2",
            vec![
                ("grant_type", "refresh_token"),
                ("client_id", "test-owned-client"),
                ("refresh_token", "refresh-1"),
                ("scope", TEAMS_SCOPE),
            ],
        )]);
        let session = session_with_tokens(
            config_for(&base),
            AccessToken::new("aad-call-old".into(), Some(0)),
            Some("refresh-1"),
            Some(AccessToken::new(
                "graph-must-remain-separate".into(),
                Some(3600),
            )),
            None,
        );
        let supported = [CallingTokenType::Aad];
        let token = session
            .calling_token(CallingTokenRequest {
                source: CallingTokenSource::TrouterCallingWeb,
                supported_types: &supported,
                challenge: None,
                force_renew: false,
            })
            .unwrap();
        task.join().unwrap();

        assert_eq!(token.token_type, CallingTokenType::Aad);
        assert_eq!(token.expose_to_transport(), "aad-call-new");
        let tokens = session.tokens.lock().unwrap();
        assert_eq!(tokens.refresh.as_deref(), Some("refresh-2"));
        assert_eq!(
            tokens.graph.as_ref().unwrap().value,
            "graph-must-remain-separate"
        );
    }

    #[test]
    fn calling_token_force_renews_skype_without_reusing_the_cached_value() {
        let (base, task) = mock(vec![Step {
            path: "/authz",
            status: 200,
            body: serde_json::json!({"tokens":{"skypeToken":"skype-call-new","expiresIn":3600}}),
            form: vec![],
            bearer: Some("aad-call"),
        }]);
        let mut config = AuthConfig::work("test-owned-client");
        config.authz_url = format!("{base}/authz");
        let session = session_with_tokens(
            config,
            AccessToken::new("aad-call".into(), Some(3600)),
            Some("refresh-1"),
            None,
            Some(AccessToken::new("skype-call-old".into(), Some(3600))),
        );
        let supported = [CallingTokenType::Skype];
        let token = session
            .calling_token(CallingTokenRequest {
                source: CallingTokenSource::TrouterCallingWeb,
                supported_types: &supported,
                challenge: None,
                force_renew: true,
            })
            .unwrap();
        task.join().unwrap();

        assert_eq!(token.expose_to_transport(), "skype-call-new");
    }

    #[test]
    fn calling_token_skips_cae_instead_of_substituting_another_audience() {
        let session = session_with_tokens(
            AuthConfig::work("test-owned-client"),
            AccessToken::new("aad-call".into(), Some(3600)),
            Some("refresh-1"),
            Some(AccessToken::new("graph-only".into(), Some(3600))),
            None,
        );
        let supported = [CallingTokenType::Cae];
        let result = session.calling_token(CallingTokenRequest {
            source: CallingTokenSource::CallStore,
            supported_types: &supported,
            challenge: None,
            force_renew: false,
        });

        assert!(matches!(
            result,
            Err(CallingTokenError::NoSupportedCredential)
        ));
    }

    #[test]
    fn authorize_request_matches_better_teams_pkce_and_native_redirect() {
        let mut flow = BrowserAuth::start(AuthConfig::work("test-owned-client")).unwrap();
        flow.verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into();
        let url = Url::parse(&flow.authorize_url().unwrap()).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.path(), "/common/oauth2/v2.0/authorize");
        assert_eq!(params["redirect_uri"], NATIVE_REDIRECT);
        assert_eq!(params["scope"], TEAMS_SCOPE);
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(
            params["code_challenge"],
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(!params.contains_key("client_secret"));
        assert!(!url.as_str().contains("localhost"));
    }

    #[test]
    fn system_browser_loopback_accepts_the_code_without_echoing_it() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let redirect = format!("http://127.0.0.1:{port}/");
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            stream
                .write_all(
                    b"GET /?code=secret-code&state=s HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            assert!(!response.contains("secret-code"));
            assert!(response.contains("Pode fechar esta aba"));
        });
        let callback = await_browser_redirect(
            listener,
            &redirect,
            "s",
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            authorization_code(&callback, &redirect, "s").unwrap(),
            "secret-code"
        );
        client.join().unwrap();
    }

    #[test]
    fn callback_requires_exact_origin_path_unique_parameters_and_state() {
        let good = format!("{NATIVE_REDIRECT}?code=a%2Bb&state=s");
        assert_eq!(
            authorization_code(&good, NATIVE_REDIRECT, "s").unwrap(),
            "a+b"
        );
        let fragment = format!("{NATIVE_REDIRECT}#code=a&state=s");
        assert_eq!(
            authorization_code(&fragment, NATIVE_REDIRECT, "s").unwrap(),
            "a"
        );
        for bad in [
            good.replace(
                "login.microsoftonline.com",
                "login.microsoftonline.com.evil.test",
            ),
            good.replace("nativeclient?", "nativeclient/extra?"),
            good.replace("state=s", "state=other"),
            format!("{good}&state=s"),
            format!("{NATIVE_REDIRECT}?code=a&code=b&state=s"),
        ] {
            assert!(authorization_code(&bad, NATIVE_REDIRECT, "s").is_err());
        }
    }

    #[test]
    fn browser_exchange_derives_separate_tokens_and_rotates_refresh_token() {
        let (base, server) = mock(vec![
            token_step(
                "aad-test",
                "rt-1",
                vec![
                    ("grant_type", "authorization_code"),
                    ("code", "fake-code"),
                    ("redirect_uri", NATIVE_REDIRECT),
                ],
            ),
            skype_step(),
            token_step(
                "graph-1",
                "rt-2",
                vec![
                    ("grant_type", "refresh_token"),
                    ("scope", GRAPH_SCOPE),
                    ("refresh_token", "rt-1"),
                ],
            ),
            token_step(
                "graph-2",
                "rt-3",
                vec![("scope", GRAPH_SCOPE), ("refresh_token", "rt-2")],
            ),
        ]);
        let flow = BrowserAuth::start(config_for(&base)).unwrap();
        let callback = format!("{NATIVE_REDIRECT}?code=fake-code&state={}", flow.state);
        let session = flow.complete(&callback).unwrap();
        assert_eq!(session.access_token().unwrap(), "graph-1");
        {
            let mut tokens = session.tokens.lock().unwrap();
            assert_eq!(tokens.aad.value, "aad-test");
            assert_eq!(tokens.skype.as_ref().unwrap().value, "skype-test");
            tokens.graph.as_mut().unwrap().expires_at = Instant::now();
        }
        assert_eq!(session.access_token().unwrap(), "graph-2");
        assert_eq!(
            session.tokens.lock().unwrap().refresh.as_deref(),
            Some("rt-3")
        );
        server.join().unwrap();
    }

    type ConcurrentRefreshSummary = (usize, usize, Vec<String>, String, usize);

    fn concurrent_refresh_server() -> (String, thread::JoinHandle<ConcurrentRefreshSummary>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let worker = thread::spawn(move || {
            let mut expected_refresh = "refresh-1".to_owned();
            let mut refresh_number = 2;
            let mut scopes = Vec::new();
            let mut authz_calls = 0;

            for _ in 0..3 {
                let mut request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .expect("concurrent token fixture request");
                match request.url() {
                    "/token" => {
                        let mut body = Vec::new();
                        request.as_reader().read_to_end(&mut body).unwrap();
                        let form: HashMap<_, _> =
                            url::form_urlencoded::parse(&body).into_owned().collect();
                        assert_eq!(
                            form.get("refresh_token").map(String::as_str),
                            Some(expected_refresh.as_str())
                        );
                        let scope = form.get("scope").map(String::as_str).unwrap();
                        let access_token = match scope {
                            GRAPH_SCOPE => "graph-new",
                            TEAMS_SCOPE => "aad-new",
                            other => panic!("unexpected token audience: {other}"),
                        };
                        scopes.push(scope.to_owned());
                        expected_refresh = format!("refresh-{refresh_number}");
                        refresh_number += 1;
                        let reply = serde_json::json!({
                            "access_token": access_token,
                            "refresh_token": expected_refresh,
                            "expires_in": 3600
                        });
                        request
                            .respond(tiny_http::Response::from_string(reply.to_string()))
                            .unwrap();
                    }
                    "/authz" => {
                        let bearer = request
                            .headers()
                            .iter()
                            .find(|header| header.field.equiv("Authorization"))
                            .map(|header| header.value.as_str());
                        assert_eq!(bearer, Some("Bearer aad-new"));
                        authz_calls += 1;
                        let reply = serde_json::json!({
                            "tokens": { "skypeToken": "skype-new", "expiresIn": 3600 },
                            "regionGtms": { "ams": "https://api.asm.skype.com" }
                        });
                        request
                            .respond(tiny_http::Response::from_string(reply.to_string()))
                            .unwrap();
                    }
                    other => panic!("unexpected auth fixture path: {other}"),
                }
            }

            let mut extra_requests = 0;
            while let Some(request) = server.recv_timeout(Duration::from_millis(30)).unwrap() {
                extra_requests += 1;
                request.respond(tiny_http::Response::empty(500)).unwrap();
            }
            (
                scopes.len(),
                authz_calls,
                scopes,
                expected_refresh,
                extra_requests,
            )
        });
        (base, worker)
    }

    #[test]
    fn concurrent_graph_and_teams_refreshes_share_rotated_credentials() {
        use std::sync::Barrier;

        let (base, server) = concurrent_refresh_server();
        let session = TeamsSession {
            config: config_for(&base),
            tokens: Arc::new(Mutex::new(TokenSet {
                aad: AccessToken::new("aad-expired".into(), Some(0)),
                refresh: Some("refresh-1".into()),
                graph: Some(AccessToken::new("graph-expired".into(), Some(0))),
                graph_photo: None,
                skype: Some(AccessToken::new("skype-expired".into(), Some(0))),
                region_gtms: None,
                call_token_fallbacks: HashMap::new(),
            })),
            persistent: Arc::new(AtomicBool::new(false)),
            profile_images: Arc::new(crate::avatars::ProfileImages::default()),
        };

        let callers = 8;
        let start = Arc::new(Barrier::new(callers));
        let workers: Vec<_> = (0..callers)
            .map(|index| {
                let session = session.clone();
                let start = start.clone();
                thread::spawn(move || {
                    start.wait();
                    if index % 2 == 0 {
                        assert_eq!(session.access_token().unwrap(), "graph-new");
                    } else {
                        session.native_client().unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }

        let (token_requests, authz_requests, scopes, final_refresh, extra_requests) =
            server.join().unwrap();
        assert_eq!(token_requests, 2);
        assert_eq!(authz_requests, 1);
        assert_eq!(extra_requests, 0);
        assert_eq!(
            scopes
                .iter()
                .filter(|scope| scope.as_str() == GRAPH_SCOPE)
                .count(),
            1
        );
        assert_eq!(
            scopes
                .iter()
                .filter(|scope| scope.as_str() == TEAMS_SCOPE)
                .count(),
            1
        );
        assert_eq!(final_refresh, "refresh-3");

        let tokens = session.tokens.lock().unwrap();
        assert_eq!(tokens.aad.value, "aad-new");
        assert_eq!(tokens.graph.as_ref().unwrap().value, "graph-new");
        assert_eq!(tokens.skype.as_ref().unwrap().value, "skype-new");
        assert_eq!(tokens.refresh.as_deref(), Some("refresh-3"));
    }

    #[test]
    fn device_fallback_bootstraps_same_teams_resource_as_browser() {
        let (base, server) = mock(vec![
            Step {
                path: "/devicecode",
                status: 200,
                form: vec![("client_id", "test-owned-client"), ("scope", TEAMS_SCOPE)],
                bearer: None,
                body: serde_json::json!({"device_code":"fake-device","user_code":"TEST-CODE","verification_uri":"https://example.test/device","expires_in":30,"interval":1}),
            },
            token_step(
                "aad-test",
                "rt-1",
                vec![
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", "fake-device"),
                ],
            ),
            skype_step(),
            token_step(
                "graph-test",
                "rt-2",
                vec![("scope", GRAPH_SCOPE), ("refresh_token", "rt-1")],
            ),
        ]);
        let (mut flow, info) = start_device_code(config_for(&base)).unwrap();
        assert_eq!(info.user_code, "TEST-CODE");
        assert_eq!(info.browser_url, info.verification_uri);
        flow.interval = Duration::from_millis(1);
        let session = complete_device_code(flow).unwrap();
        assert_eq!(session.access_token().unwrap(), "graph-test");
        server.join().unwrap();
    }

    #[test]
    fn graph_failure_does_not_turn_bootstrap_token_into_a_graph_token() {
        let (base, server) = mock(vec![
            skype_step(),
            Step {
                path: "/token",
                status: 400,
                form: vec![("scope", GRAPH_SCOPE)],
                bearer: None,
                body: serde_json::json!({"error":"invalid_client","error_codes":[65002]}),
            },
        ]);
        let reply: TokenReply = serde_json::from_value(
            serde_json::json!({"access_token":"aad-test","refresh_token":"rt-1"}),
        )
        .unwrap();
        let session = TeamsSession::from_login(config_for(&base), reply).unwrap();
        let tokens = session.tokens.lock().unwrap();
        assert!(tokens.graph.is_none());
        assert!(tokens.skype.is_some());
        server.join().unwrap();
    }
}
