//! Pinned original commercial relay profile and bounded TRAP credentials.
//! No inferred FlightProxy endpoints, redirects, cookie/browser or Graph grants.
use crate::{
    auth::calling::{CallingToken, CallingTokenType},
    call_turn::Credentials,
};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderValue},
};
use serde_json::Value;
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

const RELAY_HOST: &str = "default.relay.teams.microsoft.com";
pub const STUN_SERVICE: &str = "default.relay.teams.microsoft.com:3478";
const TOKEN_URL: &str = "https://teams.microsoft.com/trap/tokens";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayError {
    UnsupportedCloud,
    Credential,
    Network,
    TimedOut,
    Http(u16),
    TooLarge,
    InvalidResponse,
    Expired,
}

pub struct Grant {
    pub credentials: Credentials,
    expires: Instant,
}
impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelayGrant([REDACTED])")
    }
}
impl Grant {
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn into_webrtc_server(self) -> Result<teams_av::rtc::prelude::IceServer, RelayError> {
        self.remaining()?;
        Ok(self.credentials.into_webrtc_server(RELAY_HOST))
    }
    pub fn parse(bytes: &[u8]) -> Result<Self, RelayError> {
        if bytes.len() > 65536 {
            return Err(RelayError::TooLarge);
        }
        let body: Value = serde_json::from_slice(bytes).map_err(|_| RelayError::InvalidResponse)?;
        let expires = body
            .get("expires")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 30.0 && *v <= 30.0 * 86400.0)
            .ok_or(RelayError::InvalidResponse)?;
        let tokens = match body.get("tokens") {
            None => vec![&body],
            Some(Value::Array(tokens)) if !tokens.is_empty() && tokens.len() <= 8 => {
                tokens.iter().collect()
            }
            _ => return Err(RelayError::InvalidResponse),
        };
        let mut selected = None;
        for token in tokens {
            if token
                .get("realm")
                .and_then(Value::as_str)
                .map(|realm| realm.trim_matches('"'))
                != Some("rtcmedia")
            {
                continue;
            }
            if selected.is_some() {
                return Err(RelayError::InvalidResponse);
            }
            let field = |name| {
                token
                    .get(name)
                    .and_then(Value::as_str)
                    .ok_or(RelayError::InvalidResponse)
            };
            let credentials =
                Credentials::new(field("username")?, field("password")?, field("realm")?)
                    .map_err(|_| RelayError::InvalidResponse)?;
            selected = Some(credentials);
        }
        Ok(Self {
            credentials: selected.ok_or(RelayError::InvalidResponse)?,
            expires: Instant::now() + Duration::from_secs_f64(expires),
        })
    }
    pub fn remaining(&self) -> Result<Duration, RelayError> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|v| *v >= Duration::from_secs(10))
            .ok_or(RelayError::Expired)
    }
}

#[derive(Clone)]
pub(crate) struct RelayRequest {
    client: Client,
    url: String,
    headers: HeaderMap,
    #[cfg(test)]
    relay_fixture: Option<SocketAddr>,
}
impl std::fmt::Debug for RelayRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelayRequest([REDACTED])")
    }
}
impl RelayRequest {
    pub(crate) fn commercial(authz: &str, token: CallingToken) -> Result<Self, RelayError> {
        if authz != "https://teams.microsoft.com/api/authsvc/v1.0/authz" {
            return Err(RelayError::UnsupportedCloud);
        }
        if token.token_type != CallingTokenType::Skype {
            return Err(RelayError::Credential);
        }
        Self::build(TOKEN_URL, token.expose_to_transport())
    }
    fn build(url: &str, token: &str) -> Result<Self, RelayError> {
        let mut headers = HeaderMap::new();
        let mut secret = HeaderValue::from_str(token).map_err(|_| RelayError::Credential)?;
        secret.set_sensitive(true);
        headers.insert("x-skypetoken", secret);
        headers.insert("api-version", HeaderValue::from_static("2"));
        let trace = crate::call_context::native_id().replace('-', "");
        let span = crate::call_context::native_id().replace('-', "");
        headers.insert(
            "traceparent",
            HeaderValue::from_str(&format!("00-{trace}-{}-01", &span[..16]))
                .map_err(|_| RelayError::Credential)?,
        );
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| RelayError::Network)?;
        Ok(Self {
            client,
            url: url.into(),
            headers,
            #[cfg(test)]
            relay_fixture: None,
        })
    }
    #[cfg(test)]
    pub(crate) fn fixture(base: &str) -> Self {
        assert!(base.starts_with("http://127.0.0.1:"));
        Self::build(&format!("{base}/trap/tokens"), "synthetic-skype-token").unwrap()
    }
    #[cfg(test)]
    pub(crate) fn with_relay_fixture(mut self, server: SocketAddr) -> Self {
        assert!(server.ip().is_loopback());
        self.relay_fixture = Some(server);
        self
    }
    pub async fn prepare(self) -> Result<crate::call_turn::Allocation, RelayError> {
        #[cfg(test)]
        let fixture = self.relay_fixture;
        #[cfg(not(test))]
        let fixture: Option<SocketAddr> = None;
        let grant = self.fetch().await?;
        let until = grant.expires;
        let server = match fixture {
            Some(server) => server,
            None => resolve().await?,
        };
        let allocation = if fixture.is_some() {
            crate::call_turn::Allocation::allocate(server, grant.credentials).await
        } else {
            allocate_with_fallback(server, grant.credentials, |credentials| async move {
                // Pinned profile only. No clear TCP downgrade after TLS failure.
                crate::call_turn::Allocation::allocate_tls(
                    SocketAddr::new(server.ip(), 443),
                    RELAY_HOST,
                    credentials,
                )
                .await
            })
            .await
        }
        .map_err(|_| RelayError::Network)?;
        allocation
            .limit_credentials(until.saturating_duration_since(Instant::now()))
            .map_err(|_| RelayError::Expired)
    }
    pub async fn fetch(self) -> Result<Grant, RelayError> {
        let mut response = self
            .client
            .get(self.url)
            .headers(self.headers)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    RelayError::TimedOut
                } else {
                    RelayError::Network
                }
            })?;
        if !response.status().is_success() {
            return Err(RelayError::Http(response.status().as_u16()));
        }
        if response.content_length().is_some_and(|len| len > 65536) {
            return Err(RelayError::TooLarge);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| RelayError::Network)? {
            if bytes.len() + chunk.len() > 65536 {
                return Err(RelayError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        Grant::parse(&bytes)
    }
}

/// Credential/protocol rejection must not fan out to more transports.
async fn allocate_with_fallback<F, T>(
    server: SocketAddr,
    credentials: Credentials,
    tls: F,
) -> std::io::Result<crate::call_turn::Allocation>
where
    F: FnOnce(Credentials) -> T,
    T: std::future::Future<Output = std::io::Result<crate::call_turn::Allocation>>,
{
    match crate::call_turn::Allocation::allocate(server, credentials.clone()).await {
        Ok(allocation) => Ok(allocation),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::NetworkUnreachable
                    | std::io::ErrorKind::HostUnreachable
            ) =>
        {
            tls(credentials).await
        }
        Err(error) => Err(error),
    }
}

/// DNS only resolves the pinned profile; no auth material is sent here.
pub async fn resolve() -> Result<SocketAddr, RelayError> {
    tokio::time::timeout(
        Duration::from_secs(1),
        tokio::net::lookup_host(STUN_SERVICE),
    )
    .await
    .map_err(|_| RelayError::TimedOut)?
    .map_err(|_| RelayError::Network)?
    .find(|address| address.is_ipv4())
    .ok_or(RelayError::Network)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "libwebrtc-av")]
    #[test]
    fn webrtc_reuses_validated_relay_credentials_without_clear_tcp_fallback() {
        let body = serde_json::json!({"expires":600,"realm":"rtcmedia","username":"fixture-user","password":"fixture-secret"}).to_string();
        let server = Grant::parse(body.as_bytes()).unwrap().into_webrtc_server().unwrap();
        assert_eq!(server.username, "fixture-user");
        assert_eq!(server.password, "fixture-secret");
        assert_eq!(server.urls.len(), 2);
        assert!(server.urls[0].starts_with("turn:") && server.urls[0].ends_with("transport=udp"));
        assert!(server.urls[1].starts_with("turns:") && server.urls[1].ends_with("transport=tcp"));
    }
    use serde_json::json;
    #[tokio::test]
    async fn blocked_udp_uses_tls_but_authentication_rejection_never_fans_out() {
        let blackhole = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let fixture = crate::call_turn::tests::fixture().await;
        let (endpoint, bridge) = crate::call_turn_io::tests::bridge(fixture.server, true).await;
        let allocation = allocate_with_fallback(
            blackhole.local_addr().unwrap(),
            Credentials::new("fixture-user", "fixture-secret", "rtcmedia").unwrap(),
            |credentials| async move {
                let io = crate::call_turn_io::Connection::tls_with_roots(
                    endpoint,
                    "relay.fixture.invalid",
                    crate::call_turn_io::tests::fixture_roots(),
                )
                .await?;
                crate::call_turn::Allocation::allocate_on(io, endpoint, credentials).await
            },
        )
        .await
        .unwrap();
        allocation.release().await;
        tokio::time::timeout(Duration::from_secs(1), fixture.released)
            .await
            .unwrap()
            .unwrap();
        fixture.worker.await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), bridge)
            .await
            .unwrap()
            .unwrap();
        let fixture = crate::call_turn::tests::fixture().await;
        let result = allocate_with_fallback(
            fixture.server,
            Credentials::new("fixture-user", "fixture-secret", "other-realm").unwrap(),
            |_| async { panic!("credential rejection must not trigger TLS retry") },
        )
        .await;
        assert!(result.is_err());
        fixture.worker.abort();
        let _ = fixture.worker.await;
    }
    #[test]
    fn trap_realm_selection_is_exact_bounded_expiring_and_redacted() {
        for realm in ["rtcmedia", "\"rtcmedia\""] {
            let data=json!({"expires":600,"tokens":[{"realm":"other","username":"wrong","password":"wrong"},{"realm":realm,"username":"fixture-user","password":"fixture-secret"}]}).to_string();
            let grant = Grant::parse(data.as_bytes()).unwrap();
            assert!(grant.remaining().unwrap() > Duration::from_secs(590));
            assert!(!format!("{grant:?}").contains("fixture-secret"));
        }
        for body in [
            json!({"expires":0,"realm":"rtcmedia","username":"a","password":"b"}),
            json!({"expires":600,"tokens":[]}),
            json!({"expires":600,"realm":"other","username":"a","password":"b"}),
            json!({"expires":600,"tokens":[{"realm":"rtcmedia","username":"a","password":"b"},{"realm":"rtcmedia","username":"c","password":"d"}]}),
            json!({"expires":600,"realm":"rtcmedia","username":"a","password":""}),
        ] {
            assert!(Grant::parse(body.to_string().as_bytes()).is_err());
        }
        assert!(matches!(
            Grant::parse(&vec![0; 65537]),
            Err(RelayError::TooLarge)
        ));
    }
    #[tokio::test]
    async fn trap_request_uses_skype_header_and_redirects_never_forward_it() {
        for status in [200, 302] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let request = RelayRequest::fixture(&base);
            let worker = std::thread::spawn(move || {
                let req = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                assert_eq!(req.method(), &tiny_http::Method::Get);
                assert_eq!(req.url(), "/trap/tokens");
                assert!(req.headers().iter().any(|h| h.field.equiv("x-skypetoken")
                    && h.value.as_str() == "synthetic-skype-token"));
                assert!(!req.headers().iter().any(|h| h.field.equiv("authorization")));
                let response=tiny_http::Response::from_string(json!({"expires":600,"realm":"rtcmedia","username":"fixture-user","password":"fixture-secret"}).to_string()).with_status_code(status).with_header(tiny_http::Header::from_bytes("Location","http://127.0.0.1:9/forbidden").unwrap());
                req.respond(response).unwrap();
            });
            let result = request.fetch().await;
            if status == 200 {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(RelayError::Http(302))));
            }
            worker.join().unwrap();
        }
    }
}
