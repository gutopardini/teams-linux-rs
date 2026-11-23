//! Native person/group avatars recovered from the pinned Teams Windows MSIX.
//! Graph photos, regional image-cookie auth and merged/custom group images.
//! Credentials and photos remain in memory; see docs/research/group-avatars.md.

use crate::{auth::TeamsSession, graph};
use iced::widget::image::Handle;
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const GRAPH_ROOT: &str = "https://graph.microsoft.com/v1.0";
const PHOTO_EDGE: u32 = 128;
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ENTRIES: usize = 384;
const CONCURRENCY: usize = 4;
const MAX_PHOTO_REDIRECTS: usize = 4;

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Person(String),
    App(String),
    Bot(String),
    Chat(String),
    Group(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupMember {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupHint {
    pub picture: Option<String>,
    pub meeting: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupInfo {
    /// Three selected participants for avatar composition and photo requests.
    pub members: Vec<GroupMember>,
    /// Bounded names already returned by the thread; never a photo request list.
    pub roster_names: Vec<GroupMember>,
    pub picture: Option<String>,
    pub meeting: bool,
    /// `totalMemberCount` from the thread payload. Absent when the service
    /// did not send it; never inferred from the truncated roster.
    pub member_count: Option<u32>,
}

#[derive(Clone, Default)]
pub struct GroupAsset {
    pub info: GroupInfo,
    pub photo: Option<Handle>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub enum Asset {
    Photo(Option<Handle>),
    Peer(Option<String>),
    Group(GroupAsset),
}

#[derive(Clone)]
pub enum FetchResult {
    Ready(Asset),
    RetryAfter {
        delay: Duration,
        error: Option<String>,
    },
}

struct Entry {
    asset: Asset,
    composite: Option<Handle>,
    circular: Option<Handle>,
    error: Option<String>,
    fresh_until: Instant,
    used: Instant,
    observed_group_hint: Option<GroupHint>,
}

#[derive(Default)]
pub struct Cache {
    entries: HashMap<Target, Entry>,
    running: HashSet<Target>,
}

impl Cache {
    pub fn photo(&self, identity: &str) -> Option<&Handle> {
        let entry = self.entries.get(&photo_target(identity)?)?;
        entry.circular.as_ref().or_else(|| match &entry.asset {
            Asset::Photo(photo) => photo.as_ref(), _ => None,
        })
    }

    fn raw_person_photo(&self, identity: &str) -> Option<&Handle> {
        match &self.entries.get(&Target::Person(person_id(identity)?))?.asset {
            Asset::Photo(photo) => photo.as_ref(), _ => None,
        }
    }

    pub fn group_info(&self, chat_id: &str) -> Option<&GroupInfo> {
        match &self.entries.get(&Target::Group(chat_id.into()))?.asset {
            Asset::Group(group) => Some(&group.info),
            _ => None,
        }
    }

    pub fn group_photo(&self, chat_id: &str) -> Option<&Handle> {
        let entry = self.entries.get(&Target::Group(chat_id.into()))?;
        match &entry.asset {
            Asset::Group(group) => if group.info.picture.is_some() {
                entry.circular.as_ref().or(group.photo.as_ref())
            } else {
                entry.composite.as_ref().or(entry.circular.as_ref()).or(group.photo.as_ref())
            },
            _ => None,
        }
    }

    pub fn group_needs_members(&self, chat_id: &str) -> bool {
        self.entries.get(&Target::Group(chat_id.into())).is_some_and(|entry| {
            matches!(&entry.asset, Asset::Group(group) if group.info.picture.is_none() && !group.info.meeting)
        })
    }

    pub fn group_error(&self, chat_id: &str) -> Option<&str> {
        self.entries.get(&Target::Group(chat_id.into()))?.error.as_deref()
    }

    pub fn observe_group_hint(&mut self, id: &str, hint: &GroupHint) {
        if let Some(picture) = hint.picture.as_deref().filter(|value| !value.trim().is_empty()) {
            if let Some(entry) = self.entries.get_mut(&Target::Group(id.into())) {
                if entry.observed_group_hint.as_ref() == Some(hint) { return; }
                entry.observed_group_hint = Some(hint.clone());
                if matches!(&entry.asset, Asset::Group(group) if group.info.picture.as_deref() != Some(picture)) {
                    entry.fresh_until = Instant::now();
                    entry.composite = None;
                }
            }
        }
    }

    pub fn photo_error(&self, identity: &str) -> Option<&str> {
        self.entries.get(&photo_target(identity)?)?.error.as_deref()
    }

    /// Called after state changes, not from view(). Only visible rows and
    /// authors from the current conversation enter the bounded worker set.
    pub fn schedule(&mut self, wanted: Vec<Target>) -> Vec<Target> {
        let now = Instant::now();
        let mut seen = HashSet::new();
        let wanted: Vec<_> = wanted
            .into_iter()
            .filter(|target| seen.insert(target.clone()))
            .take(128)
            .collect();
        for target in &wanted {
            if let Some(entry) = self.entries.get_mut(target) {
                entry.used = now;
            }
        }
        let mut tasks = Vec::new();
        for target in wanted {
            if self.running.len() >= CONCURRENCY {
                break;
            }
            if self.running.contains(&target) {
                continue;
            }
            if let Some(entry) = self.entries.get_mut(&target) {
                entry.used = now;
                if entry.fresh_until > now {
                    continue;
                }
            }
            self.running.insert(target.clone());
            tasks.push(target);
        }
        tasks
    }

    pub fn complete(&mut self, target: Target, result: FetchResult) {
        self.running.remove(&target);
        let now = Instant::now();
        let changed = target.clone();
        let observed_group_hint = self.entries.get(&target).and_then(|entry| entry.observed_group_hint.clone());
        match result {
            FetchResult::Ready(mut asset) => {
                if let Asset::Group(group) = &mut asset {
                    if group.photo.is_none() && group.error.is_some() {
                        if let Some(Entry { asset: Asset::Group(previous), .. }) = self.entries.get(&target) {
                            if group.info.picture == previous.info.picture {
                                group.photo.clone_from(&previous.photo);
                            }
                        }
                    }
                }
                let circular = match &asset {
                    Asset::Photo(Some(photo)) if matches!(&target, Target::Person(_)) => circle_crop(photo),
                    Asset::Group(group) => group.photo.as_ref().and_then(circle_crop),
                    _ => None,
                };
                let present = matches!(&asset, Asset::Photo(Some(_)) | Asset::Peer(Some(_)));
                let (ttl, error) = match &asset {
                    Asset::Group(group) => (
                        if group.error.is_some() { 120 } else { 30 * 60 },
                        group.error.clone(),
                    ),
                    _ => (if present { 24 * 3600 } else { 6 * 3600 }, None),
                };
                self.entries.insert(
                    target,
                    Entry {
                        asset,
                        composite: None,
                        circular,
                        error,
                        used: now,
                        observed_group_hint,
                        fresh_until: now + Duration::from_secs(ttl),
                    },
                );
            }
            FetchResult::RetryAfter { delay, error } => {
                let empty = match &target {
                    Target::Person(_) | Target::App(_) | Target::Bot(_) => Asset::Photo(None),
                    Target::Chat(_) => Asset::Peer(None),
                    Target::Group(_) => Asset::Group(GroupAsset::default()),
                };
                let entry = self.entries.entry(target).or_insert(Entry {
                    asset: empty,
                    composite: None,
                    circular: None,
                    error: None,
                    fresh_until: now,
                    used: now,
                    observed_group_hint,
                });
                // Preserve a previous image during temporary network failures.
                entry.fresh_until = now + delay;
                if matches!(&entry.asset, Asset::Photo(None) | Asset::Group(_)) {
                    entry.error = error;
                }
                entry.used = now;
            }
        }
        self.refresh_composites(&changed);
        while self.entries.len() > MAX_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }

    fn refresh_composites(&mut self, changed: &Target) {
        let groups: Vec<_> = self.entries.iter().filter_map(|(key, entry)| {
            let Asset::Group(group) = &entry.asset else { return None; };
            if group.info.picture.is_some() || group.info.meeting { return None; }
            let affected = key == changed || matches!(changed, Target::Person(id)
                if group.info.members.iter().any(|member| &member.id == id));
            affected.then(|| (key.clone(), group.info.members.clone()))
        }).collect();
        for (key, members) in groups {
            let photos: Vec<_> = members.iter().filter_map(|member| self.raw_person_photo(&member.id)).collect();
            let composite = compose_photos(&photos).as_ref().and_then(circle_crop);
            if let Some(entry) = self.entries.get_mut(&key) { entry.composite = composite; }
        }
    }
}

/// Only AAD GUIDs are accepted as URL path segments. Names and emails are
/// never guessed, so two people with the same name cannot share a photo.
pub fn person_id(value: &str) -> Option<String> {
    let value = value.trim();
    let value = if value.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("8:orgid:")) {
        &value[8..]
    } else { value };
    if value.len() != 36 {
        return None;
    }
    let valid = value.bytes().enumerate().all(|(index, byte)| {
        if [8, 13, 18, 23].contains(&index) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    });
    valid.then(|| value.to_ascii_lowercase())
}

pub fn application_id(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= 128
        && value.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
        && value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && value.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric))
        .then(|| value.to_ascii_lowercase())
}

pub fn photo_target(identity: &str) -> Option<Target> {
    if let Some(id) = identity.strip_prefix("app:") {
        return application_id(id).map(Target::App);
    }
    if let Some(id) = identity.strip_prefix("bot:") { return person_id(id).map(Target::Bot); }
    person_id(identity).map(Target::Person)
}

fn trusted_application_icon(value: &str) -> Option<url::Url> {
    crate::avatar_assets::validated_url(value)
}

fn load_bot(session: TeamsSession, bot: &str, owner: &str) -> FetchResult {
    match graph::bot_icon_url(&session, bot) {
        Ok(url) => download_application_icon(&session, owner, &url),
        Err(_) => {
            crate::live_trace::record("avatar_bot_catalog_missing", 0, 0, 0);
            retry_after(Duration::from_secs(120), "Não foi possível resolver o ícone pelo identificador do bot.")
        }
    }
}

fn load_application(session: TeamsSession, app: &str, owner: &str) -> FetchResult {
    let value = match graph::application_icon_url(&session, app) {
        Ok(value) => value,
        Err(_) => return retry_after(Duration::from_secs(120), "Não foi possível consultar o ícone do aplicativo."),
    };
    download_application_icon(&session, owner, &value)
}

fn download_application_icon(session: &TeamsSession, owner: &str, value: &str) -> FetchResult {
    if crate::images::valid_media_url(value) {
        crate::live_trace::record("avatar_app_ams", 0, 0, 0);
        return match crate::images::download(session.clone(), value) {
            Ok(image) => FetchResult::Ready(Asset::Photo(Some(image.handle))),
            Err(_) => retry_after(Duration::from_secs(120), "Não foi possível carregar o ícone protegido do aplicativo."),
        };
    }
    let Some(url) = trusted_application_icon(value) else {
        crate::live_trace::record("avatar_app_icon_host_blocked", 0, 0, 0);
        return retry_after(Duration::from_secs(300), "O endereço do ícone do aplicativo não é permitido.");
    };
    // Only the discovered MT image service can receive its account cookie.
    if let Ok(credentials) = session.profile_image_credentials(owner) {
        if let Ok(root) = url::Url::parse(&credentials.root) {
            if url.origin() == root.origin() && url.path().starts_with(&format!("{}/beta/", root.path().trim_end_matches('/'))) {
                crate::live_trace::record("avatar_app_mt_image", 0, 0, 0);
                return fetch_native_photo(value, &credentials);
            }
        }
    }
    match graph::public_application_icon_bytes(value) {
        Ok(bytes) => match decode(&bytes) {
            Some(photo) => FetchResult::Ready(Asset::Photo(Some(photo))),
            None => {
                crate::live_trace::record("avatar_decode_failed", 0, bytes.len(), 3);
                retry_after(Duration::from_secs(300), "O ícone público tem formato inválido.")
            }
        },
        Err(_) => {
            crate::live_trace::record("avatar_public_icon_failed", 0, 0, 0);
            retry_after(Duration::from_secs(120), "Não foi possível carregar o ícone público do aplicativo.")
        }
    }
}

/// The native 1:1 thread ID contains both AAD IDs. Resolve only when the
/// current account is one of them; other thread shapes need a roster read.
pub fn direct_peer(chat_id: &str, owner: &str) -> Option<String> {
    ost::api::direct_peer_id(chat_id, owner)
}

pub fn load(session: TeamsSession, target: &Target, owner: &str, person_name: &str, group_hint: GroupHint) -> FetchResult {
    match target {
        Target::Person(user_id) => load_photo(session, user_id, owner, person_name),
        Target::App(app) => load_application(session, app, owner),
        Target::Bot(bot) => load_bot(session, bot, owner),
        Target::Chat(chat_id) => match graph::chat_peer(&session, chat_id, owner) {
            Ok(peer) => FetchResult::Ready(Asset::Peer(peer)),
            Err(_) => retry_after(Duration::from_secs(300), "Não foi possível identificar o contato desta conversa."),
        },
        Target::Group(chat_id) => load_group(session, chat_id, owner, group_hint),
    }
}

fn load_photo(session: TeamsSession, user_id: &str, owner: &str, person_name: &str) -> FetchResult {
    let graph = fetch_photo_with_fallback(
        GRAPH_ROOT,
        user_id,
        session.photo_access_token(),
        || session.access_token(),
    );
    person_photo_result(graph, || {
        let credentials = match session.profile_image_credentials(owner) {
            Ok(credentials) => credentials,
            Err(error) => return Some(retry_after(Duration::from_secs(120), error)),
        };
        let url = person_image_url(&credentials.root, owner, user_id, person_name).ok()?;
        Some(fetch_native_photo(&url, &credentials))
    })
}

fn person_photo_result(graph: FetchResult, native: impl FnOnce() -> Option<FetchResult>) -> FetchResult {
    // Graph uses 404 to report a missing photo. The UI draws initials for this result.
    if matches!(&graph, FetchResult::Ready(Asset::Photo(_))) {
        return graph;
    }
    match native() {
        Some(result @ (FetchResult::Ready(Asset::Photo(_)) | FetchResult::RetryAfter { .. })) => result,
        _ => graph,
    }
}

fn fetch_photo_with_fallback(
    root: &str,
    user_id: &str,
    primary_token: Result<String, String>,
    fallback_token: impl FnOnce() -> Result<String, String>,
) -> FetchResult {
    let first = match primary_token {
        Ok(token) => fetch_photo(root, &token, user_id),
        Err(_) => {
            return fallback_token()
                .map(|token| fetch_photo(root, &token, user_id))
                .unwrap_or_else(|_| {
                    retry_after(
                        Duration::from_secs(300),
                        "Graph não autorizou a leitura das fotos de perfil. Confira a permissão de leitura de perfis no aplicativo.",
                    )
                });
        }
    };
    if !photo_permission_rejected(&first) {
        return first;
    }
    fallback_token()
        .map(|token| fetch_photo(root, &token, user_id))
        .unwrap_or(first)
}

fn photo_permission_rejected(result: &FetchResult) -> bool {
    matches!(
        result,
        FetchResult::RetryAfter {
            error: Some(error),
            ..
        } if error.contains("HTTP 401") || error.contains("HTTP 403")
    )
}

fn retry_after(delay: Duration, error: impl Into<String>) -> FetchResult {
    FetchResult::RetryAfter {
        delay,
        error: Some(error.into()),
    }
}

fn new_image_agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0)
        .timeout_global(Some(Duration::from_secs(12))).build().new_agent()
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(new_image_agent)
}

fn fetch_photo(root: &str, token: &str, user_id: &str) -> FetchResult {
    let Some(user_id) = person_id(user_id) else {
        return FetchResult::Ready(Asset::Photo(None));
    };
    let Ok(url) = url::Url::parse(&format!(
        "{}/users/{user_id}/photo/$value", root.trim_end_matches('/'),
    )) else { return retry_after(Duration::from_secs(120), "O endereço da foto é inválido."); };
    fetch_image_request(agent(), url, &[("Authorization", format!("Bearer {token}"))], "Graph")
}

fn fetch_native_photo(url: &str, credentials: &crate::auth::ProfileImageCredentials) -> FetchResult {
    let Ok(url) = url::Url::parse(url) else {
        return retry_after(Duration::from_secs(120), "O endereço da foto do Teams é inválido.");
    };
    let images = &credentials.images;
    if let Err(error) = images.ensure_cookie(credentials) {
        return retry_after(Duration::from_secs(120), error);
    }
    let first = fetch_image_request(&images.agent, url.clone(), &[], "Teams");
    if photo_permission_rejected(&first) {
        if let Ok(root) = url::Url::parse(&credentials.root) {
            let prefix = format!("{}/", root.path().trim_end_matches('/'));
            if url.origin() == root.origin() && url.path().starts_with(&prefix) {
                crate::live_trace::record("avatar_image_header_fallback", 0, 0, 0);
                // Separate, initially empty jar: an unusable image cookie must
                // not override the explicit MT authorization on this request.
                let header_agent = new_image_agent();
                return fetch_image_request(&header_agent, url, &[
                    ("Authorization", format!("Bearer {}", credentials.aad)),
                    ("X-Skypetoken", credentials.skype.clone()),
                    ("x-ms-client-type", "desktop".into()),
                ], "Teams");
            }
        }
    }
    first
}

fn fetch_image_request(client: &ureq::Agent, mut url: url::Url, headers: &[(&str, String)], provider: &str) -> FetchResult {
    let auth_origin = url.origin();
    let Some(image_host) = url.host_str().map(str::to_owned) else {
        return retry_after(Duration::from_secs(120), "O endereço da foto não tem um servidor válido.");
    };
    let mut response = None;
    for redirect_count in 0..=MAX_PHOTO_REDIRECTS {
        let mut request = client
            .get(url.as_str())
            .header("Accept", "image/jpeg, image/png");
        if url.origin() == auth_origin {
            for (name, value) in headers { request = request.header(*name, value); }
        }
        let Ok(reply) = request.call() else {
            return retry_after(Duration::from_secs(120), "Falha de rede ao carregar a foto.");
        };
        if matches!(reply.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            if redirect_count == MAX_PHOTO_REDIRECTS {
                return retry_after(Duration::from_secs(300), "O servidor da foto excedeu o limite de redirecionamentos.");
            }
            let Some(location) = reply
                .headers()
                .get("Location")
                .and_then(|location| location.to_str().ok())
            else {
                return retry_after(Duration::from_secs(300), "O servidor da foto omitiu o endereço de redirecionamento.");
            };
            let Some(next) = trusted_photo_redirect(&url, location, &image_host) else {
                return retry_after(Duration::from_secs(300), "O endereço de redirecionamento da foto foi bloqueado por segurança.");
            };
            url = next;
            continue;
        }
        response = Some(reply);
        break;
    }
    let Some(mut response) = response else {
        return retry_after(Duration::from_secs(300), "O serviço não retornou a foto.");
    };
    let status = response.status().as_u16();
    crate::live_trace::record("avatar_image_http", 0, usize::from(status), match provider { "Graph" => 1, "Teams" => 2, _ => 3 });
    if status == 404 {
        return FetchResult::Ready(Asset::Photo(None));
    }
    if status != 200 {
        let delay = response
            .headers()
            .get("Retry-After")
            .and_then(|header| header.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(if status == 403 { 15 * 60 } else { 300 })
            .clamp(30, 24 * 3600);
        let error = match status {
            401 => format!("{provider} recusou a sessão da foto (HTTP 401)."),
            403 => format!("{provider} recusou acesso à foto (HTTP 403)."),
            _ => "Não foi possível carregar a foto do perfil.".into(),
        };
        return retry_after(Duration::from_secs(delay), error);
    }
    let mut bytes = Vec::new();
    if response
        .body_mut()
        .as_reader()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_BYTES
    {
        return retry_after(Duration::from_secs(3600), "A resposta da foto ultrapassou o limite de tamanho.");
    }
    match decode(&bytes) {
        Some(photo) => FetchResult::Ready(Asset::Photo(Some(photo))),
        None => {
            crate::live_trace::record("avatar_decode_failed", 0, bytes.len(), 0);
            retry_after(Duration::from_secs(3600), "O serviço retornou um arquivo de foto inválido.")
        },
    }
}

fn trusted_photo_redirect(current: &url::Url, location: &str, graph_host: &str) -> Option<url::Url> {
    let next = current.join(location).ok()?;
    let host = next.host_str()?;
    let microsoft_download_host = host == "microsoft.com"
        || host.ends_with(".microsoft.com")
        || host == "office.com"
        || host.ends_with(".office.com")
        || host == "microsoftusercontent.com"
        || host.ends_with(".microsoftusercontent.com");
    (next.scheme() == "https"
        && next.username().is_empty()
        && next.password().is_none()
        && next.port_or_known_default() == Some(443)
        && (host.eq_ignore_ascii_case(graph_host) || microsoft_download_host))
    .then_some(next)
}

/// Cache one antialiased circular display texture. The square source remains
/// available for mosaics; no masking or image decoding happens during view().
fn circle_crop(photo: &Handle) -> Option<Handle> {
    let Handle::Rgba { width, height, pixels, .. } = photo else { return None; };
    if *width == 0 || *height == 0 || *width > 2048 || *height > 2048 { return None; }
    if pixels.len() != (*width as usize).checked_mul(*height as usize)?.checked_mul(4)? { return None; }
    let edge = PHOTO_EDGE as usize;
    let mut output = if *width == PHOTO_EDGE && *height == PHOTO_EDGE {
        pixels.to_vec()
    } else {
        let source = image_codec::RgbaImage::from_raw(*width, *height, pixels.to_vec())?;
        image_codec::DynamicImage::ImageRgba8(source)
            .resize_to_fill(PHOTO_EDGE, PHOTO_EDGE, image_codec::imageops::FilterType::Triangle)
            .to_rgba8().into_raw()
    };
    let radius = PHOTO_EDGE as f32 / 2.0;
    for y in 0..edge {
        for x in 0..edge {
            let dx = x as f32 + 0.5 - radius;
            let dy = y as f32 + 0.5 - radius;
            let coverage = (radius + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            let pixel = &mut output[(y * edge + x) * 4..][..4];
            pixel[3] = (f32::from(pixel[3]) * coverage).round() as u8;
            if pixel[3] == 0 { pixel[..3].fill(0); }
        }
    }
    Some(Handle::from_rgba(PHOTO_EDGE, PHOTO_EDGE, output))
}

fn decode(bytes: &[u8]) -> Option<Handle> {
    let mut reader = image_codec::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image_codec::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let rgba = reader
        .decode()
        .ok()?
        .resize_to_fill(
            PHOTO_EDGE,
            PHOTO_EDGE,
            image_codec::imageops::FilterType::Triangle,
        )
        .to_rgba8();
    Some(Handle::from_rgba(PHOTO_EDGE, PHOTO_EDGE, rgba.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;
    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const PEER: &str = "22222222-2222-2222-2222-222222222222";

    #[test]
    fn missing_graph_photo_reaches_initials_and_authorization_failure_uses_the_native_service() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let root = format!("http://{}", server.server_addr());
        let worker = std::thread::spawn(move || {
            let request = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(request.url(), format!("/users/{PEER}/photo/$value"));
            request.respond(tiny_http::Response::empty(404)).unwrap();
        });
        let missing = fetch_photo(&root, "fixture-token", PEER);
        let result = person_photo_result(missing, || panic!("Graph already reported the absence"));
        let mut cache = Cache::default();
        cache.complete(Target::Person(PEER.into()), result);
        assert!(cache.photo(PEER).is_none());
        worker.join().unwrap();

        let denied = retry_after(Duration::from_secs(300), "Graph HTTP 403");
        let photo = Handle::from_rgba(1, 1, vec![80, 120, 160, 255]);
        let recovered = person_photo_result(denied, || Some(FetchResult::Ready(Asset::Photo(Some(photo)))));
        cache.complete(Target::Person(PEER.into()), recovered);
        assert!(cache.photo(PEER).is_some());
    }

    #[test]
    fn circular_display_mask_keeps_square_source_for_mosaic_and_antialiases_edge() {
        let mut cache = Cache::default();
        cache.complete(Target::Person(PEER.into()), FetchResult::Ready(Asset::Photo(Some(
            Handle::from_rgba(128, 128, [80, 120, 160, 255].repeat(128 * 128))
        ))));
        let Handle::Rgba { pixels: display, .. } = cache.photo(PEER).unwrap() else { panic!("display"); };
        let Handle::Rgba { pixels: raw, .. } = cache.raw_person_photo(PEER).unwrap() else { panic!("source"); };
        assert_eq!(raw[3], 255);
        assert_eq!(display[3], 0);
        assert_eq!(display[(64 * 128 + 64) * 4 + 3], 255);
        assert!(display.chunks_exact(4).any(|pixel| pixel[3] > 0 && pixel[3] < 255));
    }

    #[test]
    fn unchanged_group_hint_does_not_erase_failure_backoff() {
        let mut cache = Cache::default();
        let key = Target::Group("19:fixture@thread.v2".into());
        cache.complete(key.clone(), FetchResult::Ready(Asset::Group(GroupAsset::default())));
        let hint = GroupHint { picture: Some("image@fixture".into()), ..Default::default() };
        cache.observe_group_hint("19:fixture@thread.v2", &hint);
        cache.complete(key.clone(), retry_after(Duration::from_secs(120), "temporary failure"));
        let deadline = cache.entries[&key].fresh_until;
        for _ in 0..100 { cache.observe_group_hint("19:fixture@thread.v2", &hint); }
        assert_eq!(cache.entries[&key].fresh_until, deadline);
        assert!(cache.schedule(vec![key.clone()]).is_empty());
        let changed = GroupHint { picture: Some("image@new-fixture".into()), ..Default::default() };
        cache.observe_group_hint("19:fixture@thread.v2", &changed);
        assert!(cache.schedule(vec![key]) == vec![Target::Group("19:fixture@thread.v2".into())]);
    }

    #[test]
    fn custom_picture_compatibility_route_uses_null_account_and_roster() {
        let info = GroupInfo { picture: Some("image@https://example.test/custom.png?v=2".into()), ..Default::default() };
        let url = url::Url::parse(&legacy_custom_group_url("https://teams.microsoft.com/api/mt/emea", "19:room@thread.v2", &info).unwrap()).unwrap();
        assert!(url.path().contains("/users/null/threads/19%3Aroom%40thread.v2/properties/pictureV2"));
        let pairs: HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(pairs.get("usersInfo").map(|s| s.as_ref()), Some("null"));
        assert_eq!(pairs.get("documentUrl").map(|s| s.as_ref()), Some("https://example.test/custom.png?v=2"));
    }

    #[test]
    fn public_application_icon_download_has_no_account_headers() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = url::Url::parse(&format!("http://{}/icon.png", listener.local_addr().unwrap())).unwrap();
        let mut png = Vec::new();
        image_codec::DynamicImage::ImageRgb8(image_codec::RgbImage::from_pixel(2, 2, image_codec::Rgb([20, 80, 160])))
            .write_to(&mut Cursor::new(&mut png), image_codec::ImageFormat::Png).unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|b| b == b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let count = socket.read(&mut chunk).unwrap();
                assert!(count > 0); request.extend_from_slice(&chunk[..count]);
            }
            let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(!request.contains("authorization:"));
            assert!(!request.contains("x-skypetoken:"));
            assert!(!request.contains("cookie:"));
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n", png.len()).unwrap();
            socket.write_all(&png).unwrap();
        });
        assert!(matches!(fetch_image_request(agent(), url, &[], "Aplicativo"), FetchResult::Ready(Asset::Photo(Some(_)))));
        server.join().unwrap();
    }

    #[test]
    fn application_identity_and_public_icon_hosts_are_separate_from_people() {
        assert!(matches!(photo_target(&format!("app:{PEER}")), Some(Target::App(_))));
        assert!(person_id(&format!("app:{PEER}")).is_none());
        assert!(matches!(photo_target("app:com.microsoft.teamspace.tab.flow"), Some(Target::App(_))));
        assert!(application_id("../not-an-app").is_none());
        assert!(trusted_application_icon("https://statics.teams.cdn.office.net/app/icon.png").is_some());
        for url in ["http://statics.teams.cdn.office.net/a", "https://127.0.0.1/a", "https://[::1]/a", "https://user:pass@statics.teams.cdn.office.net/a"] {
            assert!(trusted_application_icon(url).is_none());
        }
    }

    #[test]
    fn custom_group_picture_does_not_fall_back_to_composite_and_hint_expires_cache() {
        let mut cache = Cache::default();
        let id = "19:fixture@thread.v2";
        let target = Target::Group(id.into());
        cache.complete(target.clone(), FetchResult::Ready(Asset::Group(GroupAsset::default())));
        let entry = cache.entries.get_mut(&target).unwrap();
        entry.composite = Some(Handle::from_rgba(1, 1, vec![20, 40, 60, 255]));
        let hint = GroupHint { picture: Some("image@https://example.test/custom.png".into()), meeting: false };
        cache.observe_group_hint(id, &hint);
        assert!(cache.entries[&target].fresh_until <= Instant::now());
        assert!(cache.entries[&target].composite.is_none());
        cache.complete(target.clone(), FetchResult::Ready(Asset::Group(GroupAsset {
            info: GroupInfo { picture: hint.picture, ..Default::default() },
            error: Some("unavailable".into()), ..Default::default()
        })));
        assert!(!cache.group_needs_members(id));
        assert!(cache.group_photo(id).is_none());
    }

    #[test]
    fn identity_is_bound_to_user_id_and_the_current_account() {
        assert_eq!(person_id(&format!("8:orgid:{PEER}")), Some(PEER.into()));
        assert_eq!(
            direct_peer(&format!("19:{ME}_{PEER}@unq.gbl.spaces"), ME),
            Some(PEER.into())
        );
        assert_eq!(
            direct_peer(
                &format!("19:{ME}_{PEER}@unq.gbl.spaces"),
                "33333333-3333-3333-3333-333333333333"
            ),
            None
        );
        assert_eq!(person_id("Ana Paula"), None);
        assert_eq!(person_id("../../me"), None);
    }

    #[test]
    fn cache_deduplicates_bounds_workers_and_backs_off_missing_photos() {
        let mut cache = Cache::default();
        let target = Target::Person(PEER.into());
        let wanted = vec![target.clone(); 20];
        assert_eq!(cache.schedule(wanted.clone()).len(), 1);
        assert!(cache.schedule(wanted.clone()).is_empty());
        cache.complete(target.clone(), FetchResult::Ready(Asset::Photo(None)));
        assert!(cache.schedule(wanted).is_empty());
        let wanted = (0..100)
            .map(|n| Target::Chat(format!("chat-{n}")))
            .collect();
        assert_eq!(cache.schedule(wanted).len(), CONCURRENCY);
    }

    #[test]
    fn photo_cache_keeps_a_visible_permission_error_until_a_photo_arrives() {
        let mut cache = Cache::default();
        let target = Target::Person(PEER.into());
        cache.complete(
            target.clone(),
            retry_after(
                Duration::from_secs(60),
                "Graph recusou a foto (HTTP 403).",
            ),
        );
        assert_eq!(cache.photo_error(PEER), Some("Graph recusou a foto (HTTP 403)."));

        let photo = Handle::from_rgba(1, 1, vec![20, 40, 60, 255]);
        cache.complete(
            target,
            FetchResult::Ready(Asset::Photo(Some(photo))),
        );
        assert!(cache.photo(PEER).is_some());
        assert!(cache.photo_error(PEER).is_none());
    }

    #[test]
    fn photo_request_decodes_and_bounds_the_image_off_the_ui_thread() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let mut png = Vec::new();
        image_codec::DynamicImage::ImageRgb8(image_codec::RgbImage::from_pixel(
            300,
            150,
            image_codec::Rgb([40, 100, 160]),
        ))
        .write_to(&mut Cursor::new(&mut png), image_codec::ImageFormat::Png)
        .unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|b| b == b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let count = socket.read(&mut chunk).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
            }
            let request = String::from_utf8_lossy(&request);
            assert!(request.starts_with(&format!("GET /users/{PEER}/photo/$value HTTP/1.1")));
            assert!(
                request
                    .to_lowercase()
                    .contains("authorization: bearer fixture-token")
            );
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", png.len()).unwrap();
            socket.write_all(&png).unwrap();
        });
        let result = fetch_photo(&root, "fixture-token", PEER);
        server.join().unwrap();
        let FetchResult::Ready(Asset::Photo(Some(handle))) = result else {
            panic!("expected decoded photo");
        };
        let Handle::Rgba {
            width,
            height,
            pixels,
            ..
        } = handle
        else {
            panic!("expected bounded RGBA");
        };
        assert_eq!(
            (width, height, pixels.len()),
            (
                PHOTO_EDGE,
                PHOTO_EDGE,
                (PHOTO_EDGE * PHOTO_EDGE * 4) as usize
            )
        );
        assert!(decode(b"<html>sign in</html>").is_none());
    }

    #[test]
    fn denied_photo_scope_falls_back_to_the_session_graph_token() {
        use std::io::Write;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let mut png = Vec::new();
        image_codec::DynamicImage::ImageRgb8(image_codec::RgbImage::from_pixel(
            2,
            2,
            image_codec::Rgb([80, 120, 160]),
        ))
        .write_to(&mut Cursor::new(&mut png), image_codec::ImageFormat::Png)
        .unwrap();
        let server = std::thread::spawn(move || {
            for (expected_token, status, body) in [
                ("photo-scope-token", 403, Vec::new()),
                ("graph-default-token", 200, png),
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut chunk = [0; 1024];
                    let count = socket.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                }
                let request = String::from_utf8_lossy(&request);
                assert!(request
                    .to_lowercase()
                    .contains(&format!("authorization: bearer {expected_token}")));
                let reason = if status == 200 { "OK" } else { "Forbidden" };
                write!(
                    socket,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                socket.write_all(&body).unwrap();
            }
        });

        let result = fetch_photo_with_fallback(
            &root,
            PEER,
            Ok("photo-scope-token".into()),
            || Ok("graph-default-token".into()),
        );
        server.join().unwrap();
        assert!(matches!(result, FetchResult::Ready(Asset::Photo(Some(_)))));
    }

    #[test]
    fn photo_redirects_stay_https_and_never_send_graph_tokens_to_download_hosts() {
        let current = url::Url::parse(
            "https://graph.microsoft.com/v1.0/users/22222222-2222-2222-2222-222222222222/photo/$value",
        )
        .unwrap();
        let graph = trusted_photo_redirect(
            &current,
            "/v1.0/users/22222222-2222-2222-2222-222222222222/photo/$value?size=HR48x48",
            "graph.microsoft.com",
        )
        .unwrap();
        assert_eq!(graph.host_str(), Some("graph.microsoft.com"));

        let download = trusted_photo_redirect(
            &current,
            "https://photos.microsoft.com/profile.jpg?sig=fixture",
            "graph.microsoft.com",
        )
        .unwrap();
        assert_eq!(download.host_str(), Some("photos.microsoft.com"));
        assert!(trusted_photo_redirect(
            &current,
            "http://photos.microsoft.com/profile.jpg",
            "graph.microsoft.com",
        )
        .is_none());
        assert!(trusted_photo_redirect(
            &current,
            "https://attacker.example/profile.jpg",
            "graph.microsoft.com",
        )
        .is_none());
        assert!(trusted_photo_redirect(
            &current,
            "https://graph.microsoft.com.attacker.example/profile.jpg",
            "graph.microsoft.com",
        )
        .is_none());
    }
}

/// Validate discovery data before attaching Teams credentials. The loopback
/// transport fixtures call URL builders directly, without production discovery.
pub(crate) fn validate_image_root(root: &str) -> Result<String, String> {
    let url = url::Url::parse(root).map_err(|_| "O servidor regional de fotos é inválido.")?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some()
        || url.port_or_known_default() != Some(443) || url.query().is_some() || url.fragment().is_some()
        || !(host == "teams.microsoft.com" || host.ends_with(".teams.microsoft.com")
            || host == "teams.office.com" || host.ends_with(".teams.office.com"))
    { return Err("O Teams informou um servidor de fotos inesperado.".into()); }
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

pub(crate) fn parse_group_info(value: &serde_json::Value, owner: &str) -> Result<GroupInfo, String> {
    let thread = value.get("thread").unwrap_or(value);
    // Conversation responses contain BOTH per-user `properties` and shared
    // `threadProperties`. Thread responses use `properties` for shared data.
    // Select by envelope, never let the per-user object mask the group picture.
    let props = thread.get("threadProperties").or_else(|| thread.get("properties"));
    let picture = props.and_then(|props| props.get("picture")).and_then(serde_json::Value::as_str)
        .map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned);
    let meeting = props.and_then(|props| props.get("threadType")).and_then(serde_json::Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("meeting"));
    let owner = person_id(owner);
    let mut seen = HashSet::new();
    let mut members: Vec<_> = thread.get("members").and_then(serde_json::Value::as_array)
        .into_iter().flatten().filter_map(|member| {
            let id = person_id(member.get("id").or_else(|| member.get("mri"))?.as_str()?)?;
            if owner.as_deref() == Some(&id) || !seen.insert(id.clone()) { return None; }
            let name = member.get("friendlyName").or_else(|| member.get("displayName"))
                .or_else(|| member.get("properties").and_then(|props| props.get("friendlyName")))
                .and_then(serde_json::Value::as_str).unwrap_or_default().trim().to_owned();
            Some(GroupMember { id, name })
        }).collect();
    members.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
    let roster_names = members.iter().take(512).cloned().collect();
    members.truncate(3); // Original avatar generators retain at most three.
    if props.is_none() && thread.get("members").is_none() {
        return Err("A resposta não contém os dados de perfil do grupo.".into());
    }
    Ok(GroupInfo { members, roster_names, picture, meeting, member_count: reported_member_count(thread) })
}

fn reported_member_count(thread: &serde_json::Value) -> Option<u32> {
    let raw = thread.get("totalMemberCount")
        .or_else(|| thread.get("threadProperties").and_then(|props| props.get("totalMemberCount")))
        .or_else(|| thread.get("properties").and_then(|props| props.get("totalMemberCount")))?;
    let count = raw.as_u64().or_else(|| raw.as_i64().and_then(|value| u64::try_from(value).ok()))?;
    u32::try_from(count).ok().filter(|count| *count > 0)
}

fn person_image_url(root: &str, owner: &str, person: &str, name: &str) -> Result<String, String> {
    let id = person_id(person).ok_or("A identidade da foto é inválida.")?;
    let owner = person_id(owner).ok_or("A identidade da conta é inválida.")?;
    Ok(format!("{}/beta/users/{owner}/profilepicturev2/{}?displayname={}&size=HR64x64",
        root.trim_end_matches('/'), urlencoding::encode(&format!("8:orgid:{id}")), urlencoding::encode(name)))
}

fn group_image_url(root: &str, chat: &str, owner: &str, info: &GroupInfo) -> Result<String, String> {
    let owner = person_id(owner).ok_or("A identidade da conta é inválida.")?;
    if chat.is_empty() { return Err("A identidade do grupo é inválida.".into()); }
    let users: Vec<_> = info.members.iter().take(3).map(|member| serde_json::json!({
        "userId": format!("8:orgid:{}", member.id), "displayName": member.name,
    })).collect();
    let users = serde_json::to_string(&users).map_err(|_| "Não foi possível preparar o avatar do grupo.")?;
    let path = if info.picture.is_some() {
        format!("beta/users/{owner}/threads/{}/properties/pictureV2", urlencoding::encode(chat))
    } else { format!("beta/users/{owner}/mergedProfilePicturev2") };
    let mut url = format!("{}/{path}?usersInfo={}", root.trim_end_matches('/'), urlencoding::encode(&users));
    if let Some(picture) = &info.picture {
        let document = picture.rsplit('@').next().unwrap_or(picture);
        // The original appends this property as an already escaped query
        // component. Decode an escaped URL once before encoding our query;
        // preserve escapes inside an ordinary signed URL.
        let decoded = urlencoding::decode(document).ok();
        let document = if url::Url::parse(document).is_err() {
            decoded.as_deref().filter(|value| url::Url::parse(value).is_ok()).unwrap_or(document)
        } else { document };
        url.push_str("&documentUrl=");
        url.push_str(&urlencoding::encode(document));
    }
    url.push_str("&size=HR64x64");
    Ok(url)
}

fn legacy_custom_group_url(root: &str, chat: &str, info: &GroupInfo) -> Result<String, String> {
    let owner = "00000000-0000-0000-0000-000000000000";
    let mut url = url::Url::parse(&group_image_url(root, chat, owner, info)?)
        .map_err(|_| "Endereço da imagem inválido.")?;
    let path = url.path().replace(owner, "null");
    url.set_path(&path);
    let pairs: Vec<_> = url.query_pairs().map(|(key, value)| {
        let value = if key == "usersInfo" { "null".into() } else { value.into_owned() };
        (key.into_owned(), value)
    }).collect();
    url.query_pairs_mut().clear().extend_pairs(pairs);
    Ok(url.to_string())
}

fn merge_group_hint(mut info: GroupInfo, hint: GroupHint) -> GroupInfo {
    if let Some(picture) = hint.picture.filter(|picture| !picture.trim().is_empty()) {
        info.picture = Some(picture);
    }
    info.meeting |= hint.meeting;
    info
}

fn load_group(session: TeamsSession, chat_id: &str, owner: &str, hint: GroupHint) -> FetchResult {
    let info = match graph::avatar_group_info(&session, chat_id, owner) {
        Ok(info) => merge_group_hint(info, hint),
        // A custom image does not depend on successfully reading the roster.
        Err(_) if hint.picture.is_some() || hint.meeting => merge_group_hint(GroupInfo::default(), hint),
        Err(_) => {
            crate::live_trace::record("avatar_group_metadata_failed", 0, 0, 0);
            return retry_after(Duration::from_secs(120), "Não foi possível carregar o perfil do grupo.");
        },
    };
    let mut asset = GroupAsset { info, ..GroupAsset::default() };
    if !asset.info.meeting && (asset.info.picture.is_some() || asset.info.members.len() >= 2) {
        let result = session.profile_image_credentials(owner).and_then(|credentials| {
            let url = group_image_url(&credentials.root, chat_id, owner, &asset.info)?;
            let primary = fetch_native_photo(&url, &credentials);
            if asset.info.picture.is_some() && !matches!(&primary, FetchResult::Ready(Asset::Photo(Some(_)))) {
                crate::live_trace::record("avatar_group_try_legacy", 0, 0, 0);
                let alternate = legacy_custom_group_url(&credentials.root, chat_id, &asset.info)?;
                let alternate = fetch_native_photo(&alternate, &credentials);
                if matches!(&alternate, FetchResult::Ready(Asset::Photo(Some(_)))) { return Ok(alternate); }
                if matches!(&alternate, FetchResult::RetryAfter { error: Some(error), .. } if error.contains("HTTP 401")) {
                    credentials.images.reject_unauthorized();
                }
            }
            Ok(primary)
        });
        match result {
            Ok(FetchResult::Ready(Asset::Photo(photo))) => asset.photo = photo,
            Ok(FetchResult::RetryAfter { error, .. }) => asset.error = error,
            Err(error) => asset.error = Some(error),
            _ => {}
        }
    }
    crate::live_trace::record(if asset.photo.is_some() { "avatar_group_ready" } else if asset.error.is_some() { "avatar_group_failed" } else { "avatar_group_empty" }, 0, usize::from(asset.info.picture.is_some()), asset.info.members.len());
    FetchResult::Ready(Asset::Group(asset))
}

/// Small, cached fallback from already decoded profile photos. Decoding and
/// downloads remain in workers; view() only clones the resulting GPU handle.
fn compose_photos(photos: &[&Handle]) -> Option<Handle> {
    let sources: Vec<_> = photos.iter().filter_map(|photo| match photo {
        Handle::Rgba { width, height, pixels, .. }
            if *width > 0 && *height > 0 && pixels.len() == (*width as usize).checked_mul(*height as usize)?.checked_mul(4)? =>
                Some((*width, *height, pixels.as_ref())),
        _ => None,
    }).take(3).collect();
    if sources.len() < 2 { return None; }
    let edge = PHOTO_EDGE as usize;
    let mut output = vec![0u8; edge * edge * 4];
    for (index, (width, height, pixels)) in sources.iter().enumerate() {
        let (left, top, cell_w, cell_h) = if sources.len() == 2 {
            (index * edge / 2, 0, edge / 2, edge)
        } else if index == 0 { (0, 0, edge / 2, edge) }
        else { (edge / 2, (index - 1) * edge / 2, edge / 2, edge / 2) };
        let scale = (cell_w as f32 / *width as f32).max(cell_h as f32 / *height as f32);
        let crop_x = (*width as f32 - cell_w as f32 / scale) / 2.0;
        let crop_y = (*height as f32 - cell_h as f32 / scale) / 2.0;
        for y in 0..cell_h {
            for x in 0..cell_w {
                let sx = (crop_x + x as f32 / scale).floor().clamp(0.0, (*width - 1) as f32) as usize;
                let sy = (crop_y + y as f32 / scale).floor().clamp(0.0, (*height - 1) as f32) as usize;
                let from = (sy * *width as usize + sx) * 4;
                let to = ((top + y) * edge + left + x) * 4;
                output[to..to + 4].copy_from_slice(&pixels[from..from + 4]);
            }
        }
    }
    Some(Handle::from_rgba(PHOTO_EDGE, PHOTO_EDGE, output))
}

#[derive(Default)]
struct CookieState {
    scope: String,
    fresh_until: Option<Instant>,
    retry_at: Option<Instant>,
}

/// A separate cookie jar and connection pool per TeamsSession. Cookie parsing,
/// expiry, Domain, Path, HttpOnly and Secure rules belong to ureq's cookie store.
/// The cookie setup mutex never holds the session's token lock across I/O.
pub(crate) struct ProfileImages {
    agent: ureq::Agent,
    state: std::sync::Mutex<CookieState>,
}

impl Default for ProfileImages {
    fn default() -> Self {
        Self {
            agent: ureq::Agent::config_builder().http_status_as_error(false)
                .max_redirects(0).timeout_global(Some(Duration::from_secs(12)))
                .build().new_agent(),
            state: std::sync::Mutex::new(CookieState::default()),
        }
    }
}

impl ProfileImages {
    fn reject_unauthorized(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.fresh_until = None;
            state.retry_at = Some(Instant::now() + Duration::from_secs(120));
            self.agent.cookie_jar_lock().clear();
            crate::live_trace::record("avatar_image_auth_backoff", 0, 120, 0);
        }
    }

    fn ensure_cookie(&self, credentials: &crate::auth::ProfileImageCredentials) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| "Não foi possível abrir a sessão das fotos.")?;
        let now = Instant::now();
        let scope = format!("{}|{}|{}", credentials.root, credentials.cookie_root, credentials.owner);
        if state.scope != scope {
            self.agent.cookie_jar_lock().clear();
            *state = CookieState { scope, ..CookieState::default() };
        }
        if state.fresh_until.is_some_and(|until| until > now)
            && self.agent.cookie_jar_lock().iter().next().is_some()
        { return Ok(()); }
        if state.retry_at.is_some_and(|until| until > now) {
            return Err("O serviço de imagens do Teams está temporariamente indisponível.".into());
        }
        let root = credentials.cookie_root.trim_end_matches('/');
        let modern = format!("{root}/beta/users/{}/cookiev2", credentials.owner);
        let legacy = format!("{root}/beta/imageauth/cookie");
        let mut accepted = false;
        // The original supports account/unscoped endpoints and an AAD-only
        // people-auth variant. Try these bounded alternatives only on auth/404.
        for (endpoint, include_skype) in [(&modern, true), (&legacy, true), (&modern, false), (&legacy, false)] {
            let mut request = self.agent.post(endpoint)
                .header("Authorization", &format!("Bearer {}", credentials.aad))
                .header("x-ms-client-caller", "imageAuthCookieSetupEM")
                .header("x-ms-client-type", "desktop")
                .header("Origin", "https://teams.microsoft.com")
                .header("Referer", "https://teams.microsoft.com/v2/");
            if include_skype { request = request.header("X-Skypetoken", &credentials.skype); }
            let response = request.send_empty();
            if let Ok(reply) = &response { crate::live_trace::record("avatar_cookie_http", 0, usize::from(reply.status().as_u16()), self.agent.cookie_jar_lock().iter().count()); }
            match response {
                Ok(response) if matches!(response.status().as_u16(), 200 | 204) => {
                    accepted = response.headers().get("Set-Cookie").is_some()
                        && self.agent.cookie_jar_lock().iter().next().is_some();
                    if accepted { break; }
                }
                Ok(response) if matches!(response.status().as_u16(), 401 | 403 | 404) => continue,
                _ => break,
            }
        }
        if accepted {
            state.retry_at = None;
            state.fresh_until = Some(now + credentials.valid_for.min(Duration::from_secs(30 * 60)));
            Ok(())
        } else {
            state.fresh_until = None;
            state.retry_at = Some(now + Duration::from_secs(30));
            Err("O Teams não autorizou a sessão de imagens.".into())
        }
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use std::sync::Arc;
    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const A: &str = "22222222-2222-2222-2222-222222222222";
    const B: &str = "33333333-3333-3333-3333-333333333333";
    const C: &str = "44444444-4444-4444-4444-444444444444";

    fn info() -> GroupInfo {
        GroupInfo { members: vec![
            GroupMember { id: A.into(), name: "Ana".into() },
            GroupMember { id: B.into(), name: "Bruno".into() },
            GroupMember { id: C.into(), name: "Camila".into() },
        ], ..GroupInfo::default() }
    }

    fn photo(color: [u8; 4]) -> Handle {
        Handle::from_rgba(2, 2, color.repeat(4))
    }

    #[test]
    fn thread_retains_reactor_names_beyond_three_mosaic_members() {
        let members: Vec<_> = (1..=520).map(|n| serde_json::json!({
            "id": format!("8:orgid:{n:08}-1111-2222-3333-444444444444"),
            "friendlyName": format!("Member {n:04}")
        })).collect();
        let info = parse_group_info(&serde_json::json!({"members":members}), "").unwrap();
        assert_eq!(info.members.len(), 3);
        assert_eq!(info.roster_names.len(), 512);
        assert!(info.roster_names.iter().any(|member| member.name == "Member 0004"));
        assert_eq!(info.member_count, None);
    }

    #[test]
    fn member_count_uses_the_service_total_and_stays_hidden_without_it() {
        let reported = parse_group_info(&serde_json::json!({
            "totalMemberCount": 16,
            "members": [
                {"id": format!("8:orgid:{ME}")},
                {"id": format!("8:orgid:{A}"), "friendlyName": "Ana"},
                {"id": format!("8:orgid:{B}"), "friendlyName": "Bruno"}
            ]
        }), ME).unwrap();
        assert_eq!(reported.member_count, Some(16));
        assert_eq!(reported.roster_names.len(), 2);
        let nested = parse_group_info(&serde_json::json!({
            "threadProperties": {"totalMemberCount": 16},
            "members": [{"id": format!("8:orgid:{A}"), "friendlyName": "Ana"}]
        }), ME).unwrap();
        assert_eq!(nested.member_count, Some(16));
        let missing = parse_group_info(&serde_json::json!({
            "members": [{"id": format!("8:orgid:{A}"), "friendlyName": "Ana"}]
        }), ME).unwrap();
        assert_eq!(missing.member_count, None);
        for value in [serde_json::json!(0), serde_json::json!(-1), serde_json::json!("16")] {
            let hidden = parse_group_info(&serde_json::json!({
                "totalMemberCount": value,
                "members": [{"id": format!("8:orgid:{A}"), "friendlyName": "Ana"}]
            }), ME).unwrap();
            assert_eq!(hidden.member_count, None);
        }
    }

    #[test]
    fn generic_remote_group_icon_does_not_replace_member_mosaic() {
        let mut cache = Cache::default();
        let key = Target::Group("19:mosaic@thread.v2".into());
        cache.complete(key.clone(), FetchResult::Ready(Asset::Group(GroupAsset {
            info: info(), photo: Some(photo([140, 140, 140, 255])), ..Default::default()
        })));
        assert!(cache.group_needs_members("19:mosaic@thread.v2"));
        cache.complete(Target::Person(A.into()), FetchResult::Ready(Asset::Photo(Some(photo([200, 10, 10, 255])))));
        cache.complete(Target::Person(B.into()), FetchResult::Ready(Asset::Photo(Some(photo([10, 200, 10, 255])))));
        let Handle::Rgba { pixels, .. } = cache.group_photo("19:mosaic@thread.v2").unwrap() else { panic!("mosaic"); };
        let index = ((PHOTO_EDGE / 2 * PHOTO_EDGE + PHOTO_EDGE / 4) * 4) as usize;
        assert_eq!(&pixels[index..index + 4], [200, 10, 10, 255]);
        cache.complete(key, FetchResult::Ready(Asset::Group(GroupAsset {
            info: GroupInfo { picture: Some("image@custom".into()), ..info() },
            photo: Some(photo([20, 30, 200, 255])), ..Default::default()
        })));
        let Handle::Rgba { pixels, .. } = cache.group_photo("19:mosaic@thread.v2").unwrap() else { panic!("custom"); };
        let center = ((PHOTO_EDGE / 2 * PHOTO_EDGE + PHOTO_EDGE / 2) * 4) as usize;
        assert_eq!(&pixels[center..center + 4], [20, 30, 200, 255]);
    }

    #[test]
    fn cookie_auth_401_can_fall_back_to_aad_only_people_variant() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let root = format!("http://{}", server.server_addr());
        let creds = credentials(&root);
        let url = person_image_url(&root, ME, A, "Ana").unwrap();
        let worker = std::thread::spawn(move || {
            for index in 0..3 {
                let request = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                assert_eq!(request.headers().iter().any(|h| h.field.equiv("X-Skypetoken")), index < 2);
                assert!(request.headers().iter().any(|h| h.field.equiv("Authorization")));
                assert!(request.headers().iter().any(|h| h.field.equiv("Origin") && h.value.as_str() == "https://teams.microsoft.com"));
                if index < 2 { request.respond(tiny_http::Response::empty(401)).unwrap(); }
                else { request.respond(tiny_http::Response::empty(204).with_header(
                    tiny_http::Header::from_bytes("Set-Cookie", "photoAuth=aad-only-fixture; Path=/; HttpOnly; Max-Age=3600").unwrap()
                )).unwrap(); }
            }
            let image = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert!(!image.headers().iter().any(|h| h.field.equiv("Authorization") || h.field.equiv("X-Skypetoken")));
            assert!(image.headers().iter().any(|h| h.field.equiv("Cookie") && h.value.as_str().contains("aad-only-fixture")));
            image.respond(tiny_http::Response::from_data(png())).unwrap();
        });
        assert!(matches!(fetch_native_photo(&url, &creds), FetchResult::Ready(Asset::Photo(Some(_)))));
        worker.join().unwrap();
    }

    #[test]
    fn group_metadata_uses_real_distinct_members_without_self_and_keeps_custom_picture() {
        let value = serde_json::json!({"properties":{"picture":"image@https://example.test/pic", "threadType":"meeting"},"members":[
            {"id":format!("8:orgid:{ME}")}, {"id":format!("8:orgid:{B}"),"friendlyName":"Bruno"},
            {"id":format!("8:ORGID:{A}"),"friendlyName":"Ana"},
            {"id":format!("8:orgid:{A}"),"friendlyName":"Repeated"},
            {"id":"person without identity"}, {"id":format!("8:orgid:{C}"),"friendlyName":"Camila"},
            {"id":"55555555-5555-5555-5555-555555555555","friendlyName":"Daniel"}
        ]});
        let group = parse_group_info(&value, ME).unwrap();
        assert_eq!(group.members.iter().map(|member| member.id.as_str()).collect::<Vec<_>>(), [A, B, C]);
        assert_eq!(group.picture.as_deref(), Some("image@https://example.test/pic"));
        assert!(group.meeting);
        assert!(parse_group_info(&serde_json::json!({"error":"no thread"}), ME).is_err());
    }

    #[test]
    fn conversation_user_properties_do_not_mask_shared_group_picture_or_type() {
        let group = parse_group_info(&serde_json::json!({
            "properties": {"alerts":"false", "picture":"user-specific-value"},
            "threadProperties": {"picture":"image@https://example.test/trophy.png", "threadType":"meeting"},
            "members":[]
        }), ME).unwrap();
        assert_eq!(group.picture.as_deref(), Some("image@https://example.test/trophy.png"));
        assert!(group.meeting);
        let group = parse_group_info(&serde_json::json!({
            "properties":{"picture":"not-a-shared-picture"}, "threadProperties":{}
        }), ME).unwrap();
        assert!(group.picture.is_none());
    }

    #[test]
    fn directory_picture_survives_a_thread_without_picture_and_selects_custom_image_route() {
        let thread = parse_group_info(&serde_json::json!({"properties":{"threadType":"chat"}}), ME).unwrap();
        let hint = GroupHint { picture: Some("image@https://example.test/alert.png".into()), meeting: false };
        let group = merge_group_hint(thread, hint);
        let url = url::Url::parse(&group_image_url("https://teams.microsoft.com/api/mt/part", "19:alert@thread.v2", ME, &group).unwrap()).unwrap();
        assert!(url.path().ends_with("/properties/pictureV2"));
        assert_eq!(url.query_pairs().find(|(name,_)| name == "documentUrl").unwrap().1, "https://example.test/alert.png");
        assert!(!url.path().contains("mergedProfilePicture"));
        let newer = merge_group_hint(GroupInfo { picture: Some("newer".into()), ..Default::default() }, GroupHint { picture: Some("older".into()), meeting: true });
        assert_eq!(newer.picture.as_deref(), Some("older"));
        assert!(newer.meeting);
    }

    #[test]
    fn original_image_url_shapes_keep_owner_member_ids_and_signed_document_separate() {
        let root = "https://teams.microsoft.com/api/mt/part";
        let mut group = info();
        let merged = url::Url::parse(&group_image_url(root, "19:room@thread.v2", ME, &group).unwrap()).unwrap();
        assert_eq!(merged.path(), format!("/api/mt/part/beta/users/{ME}/mergedProfilePicturev2"));
        let users = merged.query_pairs().find(|(key, _)| key == "usersInfo").unwrap().1;
        let users: serde_json::Value = serde_json::from_str(&users).unwrap();
        assert_eq!(users[0]["userId"], format!("8:orgid:{A}"));
        assert_eq!(users.as_array().unwrap().len(), 3);
        group.picture = Some("image@https://example.test/image?key=a&v=2".into());
        let custom = url::Url::parse(&group_image_url(root, "19:room@thread.v2", ME, &group).unwrap()).unwrap();
        assert!(custom.path().ends_with("/threads/19%3Aroom%40thread.v2/properties/pictureV2"));
        assert_eq!(custom.query_pairs().find(|(key, _)| key == "documentUrl").unwrap().1, "https://example.test/image?key=a&v=2");
        group.picture = Some("image@https%3A%2F%2Fexample.test%2Fimage%3Fsig%3Dx%2526y%26v%3D2".into());
        let escaped = url::Url::parse(&group_image_url(root, "19:room@thread.v2", ME, &group).unwrap()).unwrap();
        assert_eq!(escaped.query_pairs().find(|(key, _)| key == "documentUrl").unwrap().1, "https://example.test/image?sig=x%26y&v=2");
        let person = person_image_url(root, ME, A, "Ana & B").unwrap();
        assert!(person.contains(&format!("/beta/users/{ME}/profilepicturev2/8%3Aorgid%3A{A}")));
        assert!(person.contains("displayname=Ana%20%26%20B"));
    }

    #[test]
    fn composite_is_cached_from_distinct_photos_and_custom_image_takes_precedence() {
        let mut cache = Cache::default();
        let key = Target::Group("19:group@thread.v2".into());
        cache.complete(key.clone(), FetchResult::Ready(Asset::Group(GroupAsset { info: info(), ..GroupAsset::default() })));
        cache.complete(Target::Person(A.into()), FetchResult::Ready(Asset::Photo(Some(photo([220, 0, 0, 255])))));
        assert!(cache.group_photo("19:group@thread.v2").is_none());
        cache.complete(Target::Person(B.into()), FetchResult::Ready(Asset::Photo(Some(photo([0, 220, 0, 255])))));
        assert!(cache.group_photo("19:group@thread.v2").is_some());
        cache.complete(Target::Person(C.into()), FetchResult::Ready(Asset::Photo(Some(photo([0, 0, 220, 255])))));
        let Handle::Rgba { pixels, .. } = cache.group_photo("19:group@thread.v2").unwrap() else { panic!("RGBA composite"); };
        let sample = |x: usize, y: usize| &pixels[(y * PHOTO_EDGE as usize + x) * 4..][..4];
        assert_eq!(sample(10, 60), [220, 0, 0, 255]);
        assert_eq!(sample(100, 20), [0, 220, 0, 255]);
        assert_eq!(sample(100, 100), [0, 0, 220, 255]);
        cache.complete(key.clone(), FetchResult::Ready(Asset::Group(GroupAsset {
            info: GroupInfo { picture: Some("image@custom-fixture".into()), ..info() }, photo: Some(photo([70, 80, 90, 255])), error: None,
        })));
        cache.complete(key, FetchResult::Ready(Asset::Group(GroupAsset {
            info: GroupInfo { picture: Some("image@custom-fixture".into()), ..info() }, photo: None, error: Some("temporary failure".into()),
        })));
        let Handle::Rgba { pixels, width, .. } = cache.group_photo("19:group@thread.v2").unwrap() else { panic!("custom image retained"); };
        let center = ((width / 2 * width + width / 2) * 4) as usize;
        assert_eq!(&pixels[center..center + 3], [70, 80, 90]);
        assert!(pixels[center + 3] > 0);
    }

    fn credentials(root: &str) -> crate::auth::ProfileImageCredentials {
        crate::auth::ProfileImageCredentials {
            root: root.into(), cookie_root: root.into(), owner: ME.into(),
            aad: "aad-fixture".into(), skype: "skype-fixture".into(),
            valid_for: Duration::from_secs(3600), images: Arc::new(ProfileImages::default()),
        }
    }

    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image_codec::DynamicImage::ImageRgb8(image_codec::RgbImage::from_pixel(2, 2, image_codec::Rgb([40, 100, 160])))
            .write_to(&mut Cursor::new(&mut bytes), image_codec::ImageFormat::Png).unwrap();
        bytes
    }

    #[test]
    fn photo_cookie_401_can_use_headers_on_the_same_image_service() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let root = format!("http://{}", server.server_addr());
        let creds = credentials(&root);
        let url = person_image_url(&root, ME, A, "Ana").unwrap();
        let worker = std::thread::spawn(move || {
            let bootstrap = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            bootstrap.respond(tiny_http::Response::empty(204).with_header(
                tiny_http::Header::from_bytes("Set-Cookie", "photoAuth=unusable-fixture; Path=/; HttpOnly; Max-Age=3600").unwrap()
            )).unwrap();
            let cookie_image = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert!(!cookie_image.headers().iter().any(|h| h.field.equiv("Authorization")));
            cookie_image.respond(tiny_http::Response::empty(401)).unwrap();
            let authorized_image = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert!(authorized_image.url().starts_with("/beta/users/"));
            assert!(!authorized_image.headers().iter().any(|h| h.field.equiv("Cookie")));
            assert!(authorized_image.headers().iter().any(|h| h.field.equiv("Authorization") && h.value.as_str() == "Bearer aad-fixture"));
            assert!(authorized_image.headers().iter().any(|h| h.field.equiv("X-Skypetoken")));
            authorized_image.respond(tiny_http::Response::from_data(png())).unwrap();
        });
        assert!(matches!(fetch_native_photo(&url, &creds), FetchResult::Ready(Asset::Photo(Some(_)))));
        worker.join().unwrap();
    }

    #[test]
    fn native_photo_bootstraps_account_cookie_once_then_gets_images_without_bearer() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let root = format!("http://{}", server.server_addr());
        let creds = credentials(&root);
        let url = group_image_url(&root, "19:group@thread.v2", ME, &info()).unwrap();
        let worker = std::thread::spawn(move || {
            for index in 0..3 {
                let request = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                if index == 0 {
                    assert_eq!(request.method().as_str(), "POST");
                    assert_eq!(request.url(), format!("/beta/users/{ME}/cookiev2"));
                    assert!(request.headers().iter().any(|header| header.field.equiv("Authorization") && header.value.as_str() == "Bearer aad-fixture"));
                    assert!(request.headers().iter().any(|header| header.field.equiv("X-Skypetoken") && header.value.as_str() == "skype-fixture"));
                    request.respond(tiny_http::Response::empty(204).with_header(
                        tiny_http::Header::from_bytes("Set-Cookie", format!("photoAuth=fixture-cookie; Path=/beta/users/{ME}/; HttpOnly; Max-Age=3600")).unwrap(),
                    )).unwrap();
                } else {
                    assert_eq!(request.method().as_str(), "GET");
                    assert!(request.url().contains("mergedProfilePicturev2?usersInfo="));
                    assert!(request.headers().iter().any(|header| header.field.equiv("Cookie") && header.value.as_str().contains("photoAuth=fixture-cookie")));
                    assert!(!request.headers().iter().any(|header| header.field.equiv("Authorization") || header.field.equiv("X-Skypetoken")));
                    request.respond(tiny_http::Response::from_data(png())).unwrap();
                }
            }
        });
        for _ in 0..2 { assert!(matches!(fetch_native_photo(&url, &creds), FetchResult::Ready(Asset::Photo(Some(_))))); }
        worker.join().unwrap();
    }

    #[test]
    fn image_cookie_setup_supports_the_original_unscoped_endpoint_when_v2_is_missing() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let root = format!("http://{}", server.server_addr());
        let creds = credentials(&root);
        let url = person_image_url(&root, ME, A, "Ana").unwrap();
        let worker = std::thread::spawn(move || {
            let modern = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(modern.url(), format!("/beta/users/{ME}/cookiev2"));
            modern.respond(tiny_http::Response::empty(404)).unwrap();
            let legacy = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(legacy.url(), "/beta/imageauth/cookie");
            legacy.respond(tiny_http::Response::empty(204).with_header(
                tiny_http::Header::from_bytes("Set-Cookie", "photoAuth=legacy-fixture; Path=/; HttpOnly; Max-Age=3600").unwrap(),
            )).unwrap();
            let image = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert!(image.headers().iter().any(|header| header.field.equiv("Cookie") && header.value.as_str().contains("photoAuth=legacy-fixture")));
            image.respond(tiny_http::Response::from_data(png())).unwrap();
        });
        assert!(matches!(fetch_native_photo(&url, &creds), FetchResult::Ready(Asset::Photo(Some(_)))));
        worker.join().unwrap();
    }

    #[test]
    fn refused_cookie_is_backed_off_and_does_not_report_a_missing_profile() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let root = format!("http://{}", server.server_addr());
        let creds = credentials(&root);
        let url = person_image_url(&root, ME, A, "Ana").unwrap();
        let worker = std::thread::spawn(move || {
            for _ in 0..4 {
                let request = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                request.respond(tiny_http::Response::empty(403)).unwrap();
            }
            assert!(server.recv_timeout(Duration::from_millis(100)).unwrap().is_none());
        });
        assert!(matches!(fetch_native_photo(&url, &creds), FetchResult::RetryAfter { .. }));
        assert!(matches!(fetch_native_photo(&url, &creds), FetchResult::RetryAfter { .. }));
        worker.join().unwrap();
    }

    #[test]
    fn profile_image_discovery_cannot_attach_tokens_to_a_lookalike_or_insecure_root() {
        assert!(validate_image_root("https://teams.microsoft.com/api/mt/part/").is_ok());
        for root in ["https://teams.microsoft.com.evil.test/", "http://teams.microsoft.com/", "https://user@teams.microsoft.com/", "https://teams.microsoft.com:444/", "https://teams.microsoft.com/?url=external"] {
            assert!(validate_image_root(root).is_err());
        }
    }
}
