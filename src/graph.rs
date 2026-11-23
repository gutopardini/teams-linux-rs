//! Adapters from Better Teams' Rust protocol core into the Iced view models.
//! Chats use the upstream native chat API; team directories use Graph.

use crate::auth::TeamsSession;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
pub use ost::api::{ReactionCount, Reactor};
use std::collections::HashMap;
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;
use ureq::Agent;

const GRAPH_ROOT: &str = "https://graph.microsoft.com/v1.0";

#[derive(Clone, Default)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    /// Direct-chat title confirmed by the peer's exact identity.
    pub peer_name_verified: bool,
    pub subtitle: String,
    pub preview: String,
    pub last_message_time: Option<String>,
    /// Ownership is established from the message sender MRI, never its name.
    pub last_message_is_mine: Option<bool>,
    pub person_id: Option<String>,
    pub is_group: bool,
    /// Shared thread metadata from the conversation list, retained for avatars.
    pub group_avatar: crate::avatars::GroupHint,
    pub muted: Option<bool>,
    pub hidden: Option<bool>,
    /// Explicit service marker only; None means this response did not expose it.
    pub unread: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersonSearchResult {
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub job_title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageSearchResult {
    pub message_id: String,
    pub chat_id: String,
    pub team_id: Option<String>,
    pub channel_id: Option<String>,
    pub sender: String,
    pub timestamp: String,
    pub preview: String,
    pub subject: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageSearchPage {
    pub hits: Vec<MessageSearchResult>,
    pub total: Option<i64>,
    pub next_from: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMemberSummary {
    pub mri: String,
    pub person_id: Option<String>,
    pub display_name: String,
    pub role: Option<String>,
}

#[derive(Clone)]
pub struct TeamSummary {
    pub id: String,
    pub name: String,
}

#[derive(Clone)]
pub struct ChannelSummary {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct GraphMessage {
    pub id: String,
    pub client_message_id: Option<String>,
    pub timestamp: String,
    pub date: String,
    pub author: String,
    pub author_id: Option<String>,
    pub time: String,
    pub body: String,
    pub rich: Option<std::sync::Arc<crate::message_body::Document>>,
    pub is_mine: bool,
    pub is_deleted: bool,
    pub mentions_me: bool,
    pub quoted_context: Option<crate::quoted_reply::Context>,
    pub quoted_contexts: Vec<crate::quoted_reply::Context>,
    pub images: Vec<crate::images::InlineImage>,
    pub reply_to: Option<String>,
    pub quoted_author: Option<String>,
    pub quoted_body: Option<String>,
    pub forwarded: Option<ForwardedMessagePreview>,
    pub reactions: Vec<ost::api::ReactionCount>,
    pub files: Vec<ost::api::ChatFileRef>,
    pub importance: ost::api::MessageImportance,
    pub edited: bool,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct ForwardedMessagePreview {
    pub message_id: Option<String>,
    pub sender_mri: Option<String>,
    pub author_id: Option<String>,
    pub author: Option<String>,
    pub timestamp: Option<String>,
    pub subject: Option<String>,
    pub message_type: Option<String>,
    pub announcement: bool,
    pub deleted: bool,
    pub content: String,
    pub rich: Option<std::sync::Arc<crate::message_body::Document>>,
}

#[derive(Clone)]
pub struct MessagePage {
    pub messages: Vec<GraphMessage>,
    pub before: Option<String>,
}

#[derive(Clone)]
pub struct LoadedChats {
    pub session: TeamsSession,
    pub user_id: String,
    pub user_name: String,
    pub user_email: String,
    pub company_name: String,
    pub chats: Vec<ChatSummary>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OwnProfile {
    id: String,
    display_name: Option<String>,
    mail: Option<String>,
    user_principal_name: Option<String>,
    company_name: Option<String>,
}

/// Private feeds (48:) are notification/call-log streams, not user chats.
pub fn is_private_feed_thread(id: &str) -> bool {
    id.trim().starts_with("48:")
}

/// Order by last message, collapse duplicate thread IDs, and retain the selected
/// user conversation if it fell outside the latest bounded directory snapshot.
pub fn reconcile_chat_directory(
    previous: &[ChatSummary],
    latest: Vec<ChatSummary>,
    selected_id: Option<&str>,
) -> Vec<ChatSummary> {
    let mut indices: HashMap<String, usize> = HashMap::with_capacity(latest.len() + 1);
    let mut chats: Vec<ChatSummary> = Vec::with_capacity(latest.len() + 1);
    for mut chat in latest {
        if chat.id.is_empty() || is_private_feed_thread(&chat.id) {
            continue;
        }
        if !chat.is_group && !chat.peer_name_verified
            && let Some(prior)=previous.iter().find(|prior|prior.id==chat.id && !prior.is_group
                && prior.peer_name_verified && prior.person_id.is_some() && prior.person_id==chat.person_id)
        {
            chat.title=prior.title.clone();chat.peer_name_verified=true;
        }
        if let Some(index) = indices.get(&chat.id).copied() {
            if chat.last_message_time.is_none() {
                chat.last_message_time = chats[index].last_message_time.clone();
            }
            if chat.muted.is_none() {
                chat.muted = chats[index].muted;
            }
            if chat.hidden.is_none() {
                chat.hidden = chats[index].hidden;
            }
            if chat.unread.is_none() {
                chat.unread = chats[index].unread;
            }
            chats[index] = chat;
        } else {
            if chat.muted.is_none() {
                chat.muted = previous
                    .iter()
                    .find(|prior| prior.id == chat.id)
                    .and_then(|prior| prior.muted);
            }
            if chat.hidden.is_none() {
                chat.hidden = previous
                    .iter()
                    .find(|prior| prior.id == chat.id)
                    .and_then(|prior| prior.hidden);
            }
            if chat.unread.is_none() {
                chat.unread = previous
                    .iter()
                    .find(|prior| prior.id == chat.id)
                    .and_then(|prior| prior.unread);
            }
            indices.insert(chat.id.clone(), chats.len());
            chats.push(chat);
        }
    }
    if let Some(selected_id) = selected_id
        && !is_private_feed_thread(selected_id)
        && !indices.contains_key(selected_id)
        && let Some(selected) = previous.iter().find(|chat| chat.id == selected_id)
    {
        indices.insert(selected.id.clone(), chats.len());
        chats.push(selected.clone());
    }
    for hidden in previous.iter().filter(|chat| chat.hidden == Some(true) && !is_private_feed_thread(&chat.id)) {
        if !indices.contains_key(&hidden.id) {
            indices.insert(hidden.id.clone(), chats.len());
            chats.push(hidden.clone());
        }
    }
    let previous_order: HashMap<&str, usize> = previous.iter().enumerate()
        .map(|(index, chat)| (chat.id.as_str(), index)).collect();
    for chat in &mut chats {
        if chat.last_message_time.as_deref().and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok()).is_none() {
            chat.last_message_time = previous.iter().find(|prior| prior.id == chat.id)
                .and_then(|prior| prior.last_message_time.clone());
        }
    }
    // The service may move a thread after a read/property update. Retain prior
    // tie order, then sort solely on actual last-message time, never selection.
    // New entries retain their service-provided slots when no activity time is
    // known. Only restore the relative order of entries seen previously.
    let mut slots: Vec<Option<ChatSummary>> = chats.into_iter().map(Some).collect();
    let mut known = Vec::new();
    for slot in &mut slots {
        if slot.as_ref().is_some_and(|chat| previous_order.contains_key(chat.id.as_str())) {
            known.push(slot.take().unwrap());
        }
    }
    known.sort_by_key(|chat| previous_order[chat.id.as_str()]);
    let mut known = known.into_iter();
    let mut chats: Vec<ChatSummary> = slots.into_iter()
        .map(|slot| slot.unwrap_or_else(|| known.next().unwrap())).collect();
    sort_chats_by_last_message(&mut chats);
    chats
}

pub fn sort_chats_by_last_message(chats: &mut [ChatSummary]) {
    chats.sort_by_cached_key(|chat| std::cmp::Reverse(
        chat.last_message_time.as_deref()
            .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
    ));
}

#[derive(Deserialize)]
struct GraphPage<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
struct TeamDto {
    id: String,
    #[serde(rename = "displayName")]
    display_name: String,
}

#[derive(Deserialize)]
struct ChannelDto {
    id: String,
    #[serde(rename = "displayName")]
    display_name: String,
}

#[derive(Deserialize)]
struct PeopleSearchPage {
    #[serde(default)]
    value: Vec<PersonDto>,
}

#[derive(Deserialize)]
struct PersonDto {
    id: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    mail: Option<String>,
    #[serde(rename = "userPrincipalName")]
    user_principal_name: Option<String>,
    #[serde(rename = "jobTitle")]
    job_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreadCreateReply {
    status: u16,
    location: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreadMutationReply {
    status: u16,
}

#[derive(Clone, Debug, PartialEq)]
enum ThreadMutation {
    Put(Value),
    Delete,
}

const PEOPLE_SEARCH_LIMIT: usize = 20;
const PEOPLE_SEARCH_MAX_QUERY_CHARS: usize = 100;

/// Build the Graph people-directory query observed in the original Teams
/// people picker. Query values are OData-escaped and URL-encoded separately.
fn people_search_path(query: &str, limit: usize) -> Result<String, String> {
    let query = query.trim();
    let char_count = query.chars().count();
    if char_count < 2 || char_count > PEOPLE_SEARCH_MAX_QUERY_CHARS {
        return Err("Digite entre 2 e 100 caracteres para pesquisar pessoas.".into());
    }
    if query.chars().any(char::is_control) {
        return Err("A pesquisa contém caracteres inválidos.".into());
    }

    let escaped = query.replace('\'', "''");
    let filter = format!(
        "(startswith(mail,'{escaped}') or startswith(userPrincipalName,'{escaped}') or startswith(displayName,'{escaped}') or startswith(surname,'{escaped}') or startswith(givenName,'{escaped}')) and accountEnabled eq true"
    );
    let mut url = url::Url::parse(&format!("{GRAPH_ROOT}/users"))
        .map_err(|_| "Não foi possível preparar a pesquisa de pessoas.".to_owned())?;
    url.query_pairs_mut()
        .append_pair("$top", &limit.clamp(1, PEOPLE_SEARCH_LIMIT).to_string())
        .append_pair("$filter", &filter)
        .append_pair("$count", "true")
        .append_pair(
            "$select",
            "id,displayName,mail,jobTitle,userPrincipalName",
        );
    // graph_get_consistent joins this path onto the v1.0 root. Keeping the
    // version segment here produces /v1.0/v1.0/users and Graph returns 400.
    Ok(format!("/users?{}", url.query().unwrap_or_default()))
}

fn people_directory_error(error: impl std::fmt::Display) -> String {
    let text = error.to_string();
    if text.contains("HTTP 401") || text.contains("HTTP 403") {
        return "O diretório não autorizou esta pesquisa.".into();
    }
    if text.contains("HTTP ") {
        return "Não foi possível pesquisar pessoas no diretório.".into();
    }
    native_error(text)
}

fn parse_people_search_response(value: &Value) -> Result<Vec<PersonSearchResult>, String> {
    let page = serde_json::from_value::<PeopleSearchPage>(value.clone())
        .map_err(|_| "O diretório retornou uma resposta de pessoas inválida.".to_owned())?;
    let mut seen = std::collections::HashSet::with_capacity(page.value.len());
    Ok(page
        .value
        .into_iter()
        .filter_map(|person| {
            let id = person.id.as_deref().and_then(crate::avatars::person_id)?;
            if !seen.insert(id.clone()) {
                return None;
            }
            let email = person
                .mail
                .filter(|value| !value.trim().is_empty())
                .or(person.user_principal_name)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_default();
            let display_name = person
                .display_name
                .filter(|value| !value.trim().is_empty())
                .or_else(|| (!email.is_empty()).then(|| email.clone()))?;
            Some(PersonSearchResult {
                id,
                display_name,
                email,
                job_title: person
                    .job_title
                    .filter(|value| !value.trim().is_empty()),
            })
        })
        .collect())
}

async fn search_people_with<F, Fut>(
    query: &str,
    limit: usize,
    get: F,
) -> Result<Vec<PersonSearchResult>, String>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let path = people_search_path(query, limit)?;
    let value = get(path).await?;
    parse_people_search_response(&value)
}

pub fn search_people(
    session: TeamsSession,
    query: &str,
) -> Result<(TeamsSession, Vec<PersonSearchResult>), String> {
    let client = session.native_client()?;
    let people = runtime()?.block_on(search_people_with(
        query,
        PEOPLE_SEARCH_LIMIT,
        |path| async move {
            let response = client
                .graph_get_consistent(&path)
                .await
                .map_err(people_directory_error)?;
            response.json::<Value>().await.map_err(people_directory_error)
        },
    ))?;
    Ok((session, people))
}

pub async fn search_people_cancellable(
    session: TeamsSession,
    query: &str,
    cancellation: tokio::sync::oneshot::Receiver<()>,
) -> Result<(TeamsSession, Vec<PersonSearchResult>), String> {
    let client = session.native_client()?;
    let people = with_search_cancellation(
        cancellation,
        search_people_with(query, PEOPLE_SEARCH_LIMIT, |path| async move {
            let response = client
                .graph_get_consistent(&path)
                .await
                .map_err(people_directory_error)?;
            response.json::<Value>().await.map_err(people_directory_error)
        }),
    )
    .await?;
    Ok((session, people))
}

/// Search Teams messages while allowing a newer query to drop the active
/// Graph request. Dropping the request future releases the HTTP operation.
pub async fn search_messages_cancellable(
    session: TeamsSession,
    query: &str,
    from: usize,
    size: usize,
    cancellation: tokio::sync::oneshot::Receiver<()>,
) -> Result<(TeamsSession, MessageSearchPage), String> {
    let query = query.trim();
    if query.chars().take(257).count() > 256 {
        return Err("A busca deve ter no máximo 256 caracteres.".into());
    }
    let client = session.native_client()?;
    let page = with_search_cancellation(cancellation, async {
        ost::api::search_messages_data(&client, query, from, size)
            .await
            .map_err(native_error)
    })
    .await?;
    Ok((session, map_message_search_page(from, page)))
}

async fn with_search_cancellation<T, F>(
    cancellation: tokio::sync::oneshot::Receiver<()>,
    request: F,
) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    tokio::select! {
        biased;
        _ = cancellation => Err("Busca cancelada.".into()),
        result = request => result,
    }
}

fn map_message_search_page(from: usize, page: ost::api::SearchPage) -> MessageSearchPage {
    let next_from = ost::api::next_from(from, &page);
    let page = MessageSearchPage {
        hits: page
            .hits
            .into_iter()
            .map(|hit| MessageSearchResult {
                message_id: hit.message_id,
                chat_id: hit.chat_id,
                team_id: hit.team_id,
                channel_id: hit.channel_id,
                sender: hit.sender,
                timestamp: hit.timestamp,
                preview: hit.preview,
                subject: hit.subject,
            })
            .collect(),
        total: page.total,
        next_from,
    };
    page
}

fn direct_thread_id(owner: &str, peer: &str) -> Result<String, String> {
    let owner = crate::avatars::person_id(owner)
        .ok_or("A identidade da conta atual é inválida.")?;
    let peer = crate::avatars::person_id(peer).ok_or("A identidade da pessoa é inválida.")?;
    if owner == peer {
        return Err("Não é possível iniciar uma conversa com a própria conta.".into());
    }
    let (first, second) = if owner <= peer {
        (owner, peer)
    } else {
        (peer, owner)
    };
    Ok(format!("19:{first}_{second}@unq.gbl.spaces"))
}

fn direct_thread_body(owner: &str, peer: &str) -> Result<Value, String> {
    let thread_id = direct_thread_id(owner, peer)?;
    let pair = thread_id
        .strip_prefix("19:")
        .and_then(|value| value.strip_suffix("@unq.gbl.spaces"))
        .and_then(|value| value.split_once('_'))
        .ok_or("Não foi possível construir o roster 1:1.")?;
    let properties = serde_json::json!({
        "threadType": "chat",
        "chatFilesIndexId": "2",
        "fixedRoster": "true",
        "uniquerosterthread": "true",
    });
    Ok(serde_json::json!({
        "members": [
            {"id": format!("8:orgid:{}", pair.0), "role": "Admin"},
            {"id": format!("8:orgid:{}", pair.1), "role": "Admin"},
        ],
        "properties": properties,
    }))
}

fn validated_chat_service_url(service_url: &str) -> Result<url::Url, String> {
    let service = url::Url::parse(service_url)
        .map_err(|_| "O serviço de chat retornou um endereço inválido.".to_owned())?;
    let local_http = cfg!(test)
        && service.scheme() == "http"
        && service
            .host_str()
            .is_some_and(|host| host == "localhost" || host == "127.0.0.1" || host == "[::1]");
    if (service.scheme() != "https" && !local_http)
        || service.host_str().is_none()
        || !service.username().is_empty()
        || service.password().is_some()
        || service.query().is_some()
        || service.fragment().is_some()
    {
        return Err("O endereço do serviço de chat não é seguro.".into());
    }
    Ok(service)
}

fn parse_direct_thread_location(
    service_url: &str,
    location: &str,
    expected_id: &str,
) -> Result<String, String> {
    let service = validated_chat_service_url(service_url)?;
    let location = service
        .join(location)
        .map_err(|_| "O serviço de chat retornou um endereço de conversa inválido.".to_owned())?;
    let local_http = cfg!(test)
        && location.scheme() == "http"
        && location
            .host_str()
            .is_some_and(|host| host == "localhost" || host == "127.0.0.1" || host == "[::1]");
    if (location.scheme() != "https" && !local_http)
        || location.origin() != service.origin()
        || !location.username().is_empty()
        || location.password().is_some()
    {
        return Err("O serviço de chat retornou um servidor inesperado.".into());
    }
    let expected_path = format!("/v1/threads/{expected_id}");
    let decoded_path = urlencoding::decode(location.path())
        .map_err(|_| "O serviço de chat retornou um caminho inválido.".to_owned())?;
    if decoded_path != expected_path {
        return Err("A resposta não corresponde à conversa 1:1 solicitada.".into());
    }
    Ok(expected_id.to_owned())
}

async fn create_direct_chat_with<F, Fut>(
    service_url: &str,
    owner: &str,
    person: &PersonSearchResult,
    post: F,
) -> Result<ChatSummary, String>
where
    F: FnOnce(String, Value) -> Fut,
    Fut: Future<Output = Result<ThreadCreateReply, String>>,
{
    let thread_id = direct_thread_id(owner, &person.id)?;
    let body = direct_thread_body(owner, &person.id)?;
    let service = validated_chat_service_url(service_url)?;
    let endpoint = service
        .join("/v1/threads")
        .map_err(|_| "Não foi possível preparar a criação da conversa.".to_owned())?
        .to_string();
    let reply = post(endpoint, body).await?;
    if !(200..300).contains(&reply.status) {
        return Err(format!("O serviço de chat retornou HTTP {}.", reply.status));
    }
    let location = reply
        .location
        .as_deref()
        .ok_or("O serviço de chat não identificou a conversa criada.")?;
    let id = parse_direct_thread_location(service_url, location, &thread_id)?;
    Ok(ChatSummary {
        id,
        title: person.display_name.clone(),
        peer_name_verified: true,
        subtitle: "Conversa direta".into(),
        preview: "Sem mensagens".into(),
        person_id: Some(person.id.clone()),
        is_group: false,
        group_avatar: Default::default(),
        last_message_time: None,
        last_message_is_mine: None,
        muted: None,
        hidden: None,
        unread: None,
    })
}

pub fn create_direct_chat(
    session: TeamsSession,
    owner: &str,
    person: &PersonSearchResult,
) -> Result<(TeamsSession, ChatSummary), String> {
    let client = session.native_client()?;
    let service_url = client.chat_service_url();
    let chat = runtime()?.block_on(create_direct_chat_with(
        &service_url,
        owner,
        person,
        |url, body| async move {
            let response = client
                .chat_post(&url, &body)
                .await
                .map_err(native_error)?;
            Ok(ThreadCreateReply {
                status: response.status().as_u16(),
                location: response
                    .headers()
                    .get("Location")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            })
        },
    ))?;
    Ok((session, chat))
}

const GROUP_CHAT_MAX_TITLE_CHARS: usize = 100;

fn normalized_group_title(title: &str) -> Result<String, String> {
    let title = title.trim();
    let chars = title.chars().count();
    if chars == 0 || chars > GROUP_CHAT_MAX_TITLE_CHARS || title.chars().any(char::is_control) {
        return Err("O nome do grupo deve ter entre 1 e 100 caracteres.".into());
    }
    Ok(title.to_owned())
}

fn validated_thread_id(thread_id: &str) -> Result<String, String> {
    let thread_id = thread_id.trim();
    if thread_id.len() > 512
        || thread_id.chars().any(char::is_control)
        || !thread_id.starts_with("19:")
        || !thread_id.contains('@')
        || thread_id.contains('/')
        || thread_id.contains('\\')
    {
        return Err("A identidade da conversa é inválida.".into());
    }
    Ok(thread_id.to_owned())
}

fn normalized_group_peers(owner: &str, peers: &[PersonSearchResult]) -> Result<Vec<PersonSearchResult>, String> {
    let owner = crate::avatars::person_id(owner).ok_or("A identidade da conta atual é inválida.")?;
    let mut seen = std::collections::HashSet::with_capacity(peers.len());
    let mut unique = Vec::with_capacity(peers.len());
    for peer in peers {
        let id = crate::avatars::person_id(&peer.id)
            .ok_or("Uma das pessoas selecionadas tem identidade inválida.")?;
        if id != owner && seen.insert(id) {
            unique.push(PersonSearchResult {
                id: crate::avatars::person_id(&peer.id).unwrap_or_default(),
                display_name: peer.display_name.trim().to_owned(),
                email: peer.email.trim().to_owned(),
                job_title: peer.job_title.clone(),
            });
        }
    }
    if unique.len() < 2 {
        return Err("Selecione pelo menos duas pessoas para criar um grupo.".into());
    }
    Ok(unique)
}

fn group_thread_body(
    owner: &str,
    peers: &[PersonSearchResult],
    title: &str,
) -> Result<Value, String> {
    let owner = crate::avatars::person_id(owner).ok_or("A identidade da conta atual é inválida.")?;
    let title = normalized_group_title(title)?;
    let peers = normalized_group_peers(&owner, peers)?;
    let member = |id: &str| {
        serde_json::json!({
            "id": format!("8:orgid:{id}"),
            "role": "Admin",
        })
    };
    let members = std::iter::once(member(&owner))
        .chain(peers.iter().map(|peer| member(&peer.id)))
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "members": members,
        "properties": {
            "threadType": "chat",
            "chatFilesIndexId": "2",
            "topic": title,
        },
    }))
}

fn group_thread_endpoint(service_url: &str, thread_id: &str, suffix: &str) -> Result<String, String> {
    let service = validated_chat_service_url(service_url)?;
    let thread_id = validated_thread_id(thread_id)?;
    if !matches!(suffix, "members" | "properties") {
        let encoded_mri = suffix
            .strip_prefix("members/")
            .ok_or("A operação de roster é inválida.")?;
        let mri = urlencoding::decode(encoded_mri)
            .map_err(|_| "A identidade do participante é inválida.")?;
        let id = crate::avatars::person_id(&mri)
            .ok_or("Só é possível atualizar participantes identificados por GUID.")?;
        if mri != format!("8:orgid:{id}") {
            return Err("A identidade do participante é inválida.".into());
        }
    }
    let encoded = urlencoding::encode(&thread_id);
    service
        .join(&format!("/v1/threads/{encoded}/{suffix}"))
        .map(|url| url.to_string())
        .map_err(|_| "Não foi possível preparar a operação do grupo.".to_owned())
}

fn group_title_endpoint(service_url: &str, thread_id: &str) -> Result<String, String> {
    let mut url = url::Url::parse(&group_thread_endpoint(service_url, thread_id, "properties")?)
        .map_err(|_| "Não foi possível preparar o nome do grupo.".to_owned())?;
    url.query_pairs_mut().append_pair("name", "topic");
    Ok(url.to_string())
}

fn group_members_endpoint(service_url: &str, thread_id: &str) -> Result<String, String> {
    let mut url = url::Url::parse(&group_thread_endpoint(service_url, thread_id, "members")?)
        .map_err(|_| "Não foi possível preparar a lista de participantes.".to_owned())?;
    url.query_pairs_mut()
        .append_pair("view", "msnp24Equivalent")
        .append_pair("pageSize", "100");
    Ok(url.to_string())
}

fn parse_created_group_location(service_url: &str, location: &str) -> Result<String, String> {
    let service = validated_chat_service_url(service_url)?;
    let location = service
        .join(location)
        .map_err(|_| "O serviço de chat retornou um endereço de grupo inválido.".to_owned())?;
    let local_http = cfg!(test)
        && location.scheme() == "http"
        && location
            .host_str()
            .is_some_and(|host| host == "localhost" || host == "127.0.0.1" || host == "[::1]");
    if (location.scheme() != "https" && !local_http)
        || location.origin() != service.origin()
        || !location.username().is_empty()
        || location.password().is_some()
        || location.query().is_some()
        || location.fragment().is_some()
    {
        return Err("O serviço de chat retornou um servidor inesperado.".into());
    }
    let path = urlencoding::decode(location.path())
        .map_err(|_| "O serviço de chat retornou um caminho inválido.".to_owned())?;
    let thread_id = path
        .strip_prefix("/v1/threads/")
        .ok_or("A resposta não corresponde à criação do grupo.")?;
    validated_thread_id(thread_id)
}

async fn create_group_chat_with<F, Fut>(
    service_url: &str,
    owner: &str,
    title: &str,
    peers: &[PersonSearchResult],
    post: F,
) -> Result<ChatSummary, String>
where
    F: FnOnce(String, Value) -> Fut,
    Fut: Future<Output = Result<ThreadCreateReply, String>>,
{
    let title = normalized_group_title(title)?;
    let body = group_thread_body(owner, peers, &title)?;
    let service = validated_chat_service_url(service_url)?;
    let endpoint = service
        .join("/v1/threads")
        .map_err(|_| "Não foi possível preparar a criação do grupo.".to_owned())?
        .to_string();
    let reply = post(endpoint, body).await?;
    if !(200..300).contains(&reply.status) {
        return Err(format!("O serviço de chat retornou HTTP {}.", reply.status));
    }
    let location = reply
        .location
        .as_deref()
        .ok_or("O serviço de chat não identificou o grupo criado.")?;
    let id = parse_created_group_location(service_url, location)?;
    Ok(ChatSummary {
        id,
        title,
        peer_name_verified: false,
        subtitle: "Conversa em grupo".into(),
        preview: "Sem mensagens".into(),
        person_id: None,
        is_group: true,
        group_avatar: Default::default(),
        last_message_time: None,
        last_message_is_mine: None,
        muted: None,
        hidden: None,
        unread: None,
    })
}

pub fn create_group_chat(
    session: TeamsSession,
    owner: &str,
    title: &str,
    peers: &[PersonSearchResult],
) -> Result<(TeamsSession, ChatSummary), String> {
    let client = session.native_client()?;
    let service_url = client.chat_service_url();
    let chat = runtime()?.block_on(create_group_chat_with(
        &service_url,
        owner,
        title,
        peers,
        |url, body| async move {
            let response = client.chat_post(&url, &body).await.map_err(native_error)?;
            Ok(ThreadCreateReply {
                status: response.status().as_u16(),
                location: response
                    .headers()
                    .get("Location")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            })
        },
    ))?;
    Ok((session, chat))
}

fn named_field(value: &Value) -> Option<String> {
    for key in ["friendlyName", "displayName", "name"] {
        if let Some(name) = value.get(key).and_then(Value::as_str).map(str::trim).filter(|name| !name.is_empty()) {
            if crate::sender_names::known(name).is_some() {
                return Some(name.to_owned());
            }
        }
    }
    None
}

fn member_display_name(member: &Value) -> Option<String> {
    named_field(member)
        .or_else(|| member.get("properties").and_then(named_field))
        .or_else(|| member.get("user").and_then(named_field))
}

fn parse_chat_members(value: &Value) -> Result<Vec<ChatMemberSummary>, String> {
    let members = value
        .get("members")
        .or_else(|| value.get("value"))
        .and_then(Value::as_array)
        .ok_or("O serviço de chat retornou uma lista de participantes inválida.")?;
    let mut seen = std::collections::HashSet::with_capacity(members.len());
    Ok(members
        .iter()
        .filter_map(|member| {
            let mri = member
                .get("id")
                .or_else(|| member.get("mri"))
                .and_then(Value::as_str)?
                .trim()
                .to_owned();
            if mri.is_empty() || !seen.insert(mri.to_ascii_lowercase()) {
                return None;
            }
            let person_id = crate::avatars::person_id(&mri);
            let display_name = member_display_name(member).unwrap_or_default();
            let role = member
                .get("role")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|role| !role.is_empty())
                .map(str::to_owned);
            Some(ChatMemberSummary {
                mri,
                person_id,
                display_name,
                role,
            })
        })
        .collect())
}

pub fn load_group_members(
    session: TeamsSession,
    thread_id: &str,
) -> Result<(TeamsSession, Vec<ChatMemberSummary>), String> {
    let client = session.native_client()?;
    let url = group_members_endpoint(&client.chat_service_url(), thread_id)?;
    let members = runtime()?.block_on(async {
        let response = client.chat_get(&url).await.map_err(native_error)?;
        let value = response.json::<Value>().await.map_err(native_error)?;
        parse_chat_members(&value)
    })?;
    Ok((session, members))
}

const MENTION_NAME_FILL_MAX: usize = 60;

/// Graph lookup for one roster id. The chat-service roster usually omits `friendlyName`.
pub fn mention_directory_path(id: &str) -> Option<String> {
    let id = crate::avatars::person_id(id)?;
    Some(format!("/users/{id}?$select=displayName"))
}

/// A directory answer may name a blank roster row only when the id matches.
pub fn directory_display_name(user: &Value, expected_id: &str) -> Option<String> {
    let expected = crate::avatars::person_id(expected_id)?;
    let returned = user.get("id").and_then(Value::as_str).and_then(crate::avatars::person_id)?;
    if returned != expected {
        return None;
    }
    crate::sender_names::known(user.get("displayName").and_then(Value::as_str)?).map(str::to_owned)
}

fn mention_name_targets(members: &[ChatMemberSummary]) -> Vec<(usize, String, String)> {
    members.iter().enumerate().filter_map(|(index, member)| {
        if crate::sender_names::known(&member.display_name).is_some() {
            return None;
        }
        let id = member.person_id.as_deref().and_then(crate::avatars::person_id)?;
        let path = mention_directory_path(&id)?;
        Some((index, path, id))
    }).take(MENTION_NAME_FILL_MAX).collect()
}

async fn fill_mention_display_names(client: &ost::api::client::TeamsClient, members: &mut [ChatMemberSummary]) {
    let targets = mention_name_targets(members);
    for chunk in targets.chunks(8) {
        let reads = chunk.iter().cloned().map(|(index, path, id)| {
            let client = client;
            async move {
                let value = match client.graph_get(&path).await {
                    Ok(response) => response.json::<Value>().await.ok(),
                    Err(_) => None,
                };
                (index, id, value)
            }
        });
        for (index, id, value) in futures::future::join_all(reads).await {
            let Some(value) = value else { continue };
            let Some(member) = members.get_mut(index) else { continue };
            if member.person_id.as_deref().and_then(crate::avatars::person_id).as_deref() != Some(id.as_str()) {
                continue;
            }
            if let Some(name) = directory_display_name(&value, &id) {
                member.display_name = name;
            }
        }
    }
}

/// Same roster as group details, with blank names filled from Graph `displayName`.
/// A directory failure leaves those rows blank; it does not drop the roster.
pub fn load_mention_members(
    session: TeamsSession,
    thread_id: &str,
) -> Result<(TeamsSession, Vec<ChatMemberSummary>), String> {
    let (session, mut members) = load_group_members(session, thread_id)?;
    if let Ok(client) = session.native_client() {
        if let Ok(runtime) = runtime() {
            runtime.block_on(fill_mention_display_names(&client, &mut members));
        }
    }
    Ok((session, members))
}

async fn mutate_group_with<F, Fut>(
    service_url: &str,
    endpoint: String,
    operation: ThreadMutation,
    send: F,
) -> Result<(), String>
where
    F: FnOnce(String, ThreadMutation) -> Fut,
    Fut: Future<Output = Result<ThreadMutationReply, String>>,
{
    let _ = validated_chat_service_url(service_url)?;
    let reply = send(endpoint, operation).await?;
    if !(200..300).contains(&reply.status) {
        return Err(format!("O serviço de chat retornou HTTP {}.", reply.status));
    }
    Ok(())
}

fn run_group_mutation(
    session: TeamsSession,
    service_url: String,
    endpoint: String,
    operation: ThreadMutation,
) -> Result<TeamsSession, String> {
    let client = session.native_client()?;
    runtime()?.block_on(mutate_group_with(
        &service_url,
        endpoint,
        operation,
        |url, operation| async move {
            let response = match operation {
                ThreadMutation::Put(body) => client.chat_put(&url, &body).await,
                ThreadMutation::Delete => client.chat_delete(&url, None).await,
            }
            .map_err(native_error)?;
            Ok(ThreadMutationReply {
                status: response.status().as_u16(),
            })
        },
    ))?;
    Ok(session)
}

pub fn rename_group_chat(
    session: TeamsSession,
    thread_id: &str,
    title: &str,
) -> Result<TeamsSession, String> {
    let title = normalized_group_title(title)?;
    let client = session.native_client()?;
    let service_url = client.chat_service_url();
    let endpoint = group_title_endpoint(&service_url, thread_id)?;
    run_group_mutation(
        session,
        service_url,
        endpoint,
        ThreadMutation::Put(serde_json::json!({"topic": title})),
    )
}

fn add_members_body(owner: &str, ids: &[String]) -> Result<Value, String> {
    let owner = crate::avatars::person_id(owner).ok_or("A identidade da conta atual é inválida.")?;
    let mut seen = std::collections::HashSet::with_capacity(ids.len());
    let mut members = Vec::with_capacity(ids.len());
    for id in ids {
        let id = crate::avatars::person_id(id).ok_or("A identidade de um participante é inválida.")?;
        if id != owner && seen.insert(id.clone()) {
            members.push(serde_json::json!({
                "id": format!("8:orgid:{id}"),
                "role": "Admin",
            }));
        }
    }
    if members.is_empty() {
        return Err("Selecione pelo menos uma pessoa nova para adicionar.".into());
    }
    Ok(serde_json::json!({"members": members}))
}

pub fn add_group_members(
    session: TeamsSession,
    thread_id: &str,
    owner: &str,
    ids: &[String],
) -> Result<TeamsSession, String> {
    let body = add_members_body(owner, ids)?;
    let client = session.native_client()?;
    let service_url = client.chat_service_url();
    let endpoint = group_thread_endpoint(&service_url, thread_id, "members")?;
    run_group_mutation(session, service_url, endpoint, ThreadMutation::Put(body))
}

pub fn remove_group_member(
    session: TeamsSession,
    thread_id: &str,
    owner: &str,
    member_mri: &str,
) -> Result<TeamsSession, String> {
    let owner = crate::avatars::person_id(owner).ok_or("A identidade da conta atual é inválida.")?;
    let member_id = crate::avatars::person_id(member_mri)
        .ok_or("Só é possível remover participantes da organização identificados por GUID.")?;
    if owner == member_id {
        return Err("Use Sair do grupo para remover a sua própria conta.".into());
    }
    let client = session.native_client()?;
    let service_url = client.chat_service_url();
    let mri = format!("8:orgid:{member_id}");
    let endpoint = group_thread_endpoint(
        &service_url,
        thread_id,
        &format!("members/{}", urlencoding::encode(&mri)),
    )?;
    run_group_mutation(session, service_url, endpoint, ThreadMutation::Delete)
}

fn group_leave_endpoint(service_url: &str, thread_id: &str, owner: &str) -> Result<String, String> {
    let owner = crate::avatars::person_id(owner).ok_or("A identidade da conta atual é inválida.")?;
    let own_mri = format!("8:orgid:{owner}");
    group_thread_endpoint(
        service_url,
        thread_id,
        &format!("members/{}", urlencoding::encode(&own_mri)),
    )
}

pub fn leave_group_chat(
    session: TeamsSession,
    thread_id: &str,
    owner: &str,
) -> Result<(), String> {
    let client = session.native_client()?;
    let service_url = client.chat_service_url();
    let endpoint = group_leave_endpoint(&service_url, thread_id, owner)?;
    runtime()?.block_on(mutate_group_with(
        &service_url,
        endpoint,
        ThreadMutation::Delete,
        |url, _| async move {
            let response = client.chat_delete(&url, None).await.map_err(native_error)?;
            Ok(ThreadMutationReply {
                status: response.status().as_u16(),
            })
        },
    ))
}

fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .new_agent()
    })
}

fn request_json<T: DeserializeOwned>(session: &TeamsSession, url: &str) -> Result<T, String> {
    let token = session.access_token()?;
    let mut response = agent()
        .get(url)
        .header("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|_| "Não foi possível consultar o Microsoft Graph.".to_owned())?;
    let status = response.status();
    let value: Value = response
        .body_mut()
        .read_json()
        .map_err(|_| "Resposta inválida do Microsoft Graph.".to_owned())?;
    if !status.is_success() {
        return Err(value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Microsoft Graph retornou HTTP {}.", status.as_u16())));
    }
    serde_json::from_value(value)
        .map_err(|_| "Formato de resposta Graph não reconhecido.".to_owned())
}

pub(crate) fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Runtime::new()
                .map_err(|_| "Não foi possível iniciar o serviço de rede Teams.".to_owned())
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn quoted_reply_details(raw: &str) -> Option<(String, String)> {
    let document = scraper::Html::parse_fragment(raw);
    let quote = document
        .select(&scraper::Selector::parse("quote").ok()?)
        .next()?;
    let author = quote.value().attr("author")?.trim();
    let body = quote.text().collect::<String>();
    let excerpt = ost::api::reply_snippet(&body);
    (!author.is_empty() && !excerpt.is_empty()).then(|| (author.to_owned(), excerpt))
}

fn own_message_identity(sender: &str, own: &str) -> bool {
    if let Some((sender, own)) = crate::avatars::person_id(sender).zip(crate::avatars::person_id(own)) {
        return sender == own;
    }
    !own.is_empty() && sender.strip_prefix("8:orgid:").is_some_and(|id| id.eq_ignore_ascii_case(own))
}

fn message_from_native(message: ost::api::MessageInfo, user_id: &str) -> GraphMessage {
    message_from_native_with_authors(message, user_id, &HashMap::new())
}

fn message_from_native_with_authors(
    message: ost::api::MessageInfo,
    user_id: &str,
    known_authors: &HashMap<String, String>,
) -> GraphMessage {
    if let Some(shape) = message.card_wire_shape {
        crate::live_trace::record("workflow_card_wire_shape_v1", 0, usize::from(shape), 0);
    }
    if crate::avatars::person_id(&message.sender_mri).is_none() {
        crate::live_trace::record("avatar_sender_non_person", 0,
            usize::from(message.sender_mri.starts_with("28:")), usize::from(message.application_id.is_some()));
    }
    let typed_quotes = crate::quoted_reply::split_all(&message.raw);
    let quoted_contexts = typed_quotes
        .as_ref()
        .map(|(contexts, _)| contexts.clone())
        .unwrap_or_default();
    let typed_quote = typed_quotes.and_then(|(contexts, body)| {
        contexts
            .into_iter()
            .next()
            .map(|context| (context, body))
    });
    let (quoted_parent, quoted_html) = typed_quote.as_ref().map(|(context,body)|(Some(context.message_id.clone()),body.clone()))
        .unwrap_or_else(||ost::api::split_reply_quote(&message.raw));
    let has_files = !message.files.is_empty();
    let raw_body = if quoted_parent.is_some() {
        quoted_html
    } else {
        message.raw.clone()
    };
    let quote_details = typed_quote.as_ref().map(|(context,_)|(context.author.clone(),context.preview.clone())).or_else(||quoted_parent
        .as_ref()
        .and_then(|_| quoted_reply_details(&message.raw)));
    let mut current_rich = crate::message_body::Document::from_html_with_files(&raw_body, &message.files)
        .filter(|document| !document.blocks.is_empty());
    if crate::live_trace::enabled() {
        let raw_has_table = raw_body.to_ascii_lowercase().contains("<table");
        let parsed_table_blocks = current_rich.as_ref().map_or(0, |document| {
            document
                .blocks
                .iter()
                .filter(|block| block.table.is_some())
                .count()
        });
        if raw_has_table || parsed_table_blocks > 0 {
            crate::live_trace::record(
                "message_table_parse_v1",
                0,
                parsed_table_blocks,
                usize::from(raw_has_table),
            );
        }
    }
    let images = current_rich.as_ref().map(crate::message_body::Document::inline_images).unwrap_or_default();
    if let Some(document)=&mut current_rich{document.resolve_mentions(message.mention_properties.as_ref(),user_id);}
    let is_mine = own_message_identity(&message.sender_mri, user_id);
    let mentions_me=current_rich.as_ref().is_some_and(|document|match document.mention_badge(){
        crate::mentions::Badge::Person=>true,
        crate::mentions::Badge::Everyone=>!is_mine,
        crate::mentions::Badge::None=>false,
    });
    let forwarded = message.forwarded.map(|context| {
        let author_id = context
            .sender_mri
            .as_deref()
            .and_then(crate::avatars::person_id);
        let author = author_id
            .as_ref()
            .and_then(|id| known_authors.get(id))
            .cloned();
        let timestamp = context.arrival_time;
        let raw_content = context.content.unwrap_or_default();
        let rich = crate::message_body::Document::from_html(&raw_content)
            .filter(|document| !document.blocks.is_empty())
            .map(std::sync::Arc::new);
        let content = rich.as_ref().map_or_else(
            || raw_content.clone(),
            |document| document.clipboard_payload().text,
        );
        ForwardedMessagePreview {
            message_id: context.message_id,
            sender_mri: context.sender_mri,
            author_id,
            author,
            timestamp,
            subject: context.channel_subject,
            message_type: context.message_type,
            announcement: context.announcement == Some(true),
            deleted: context.deleted == Some(true),
            content,
            rich,
        }
    });
    let duplicate_forward_content = forwarded.as_ref().is_some_and(|context| {
        !context.content.trim().is_empty()
            && context.content.trim().eq_ignore_ascii_case(message.content.trim())
    });
    let quoted_plain = if typed_quote.is_some() {
        current_rich.as_ref().map(|doc| doc.clipboard_payload().text.trim().to_owned())
    } else {
        None
    };
    let rich = if duplicate_forward_content {
        None
    } else {
        current_rich.map(std::sync::Arc::new)
    };
    let local = chrono::DateTime::parse_from_rfc3339(&message.timestamp)
        .ok()
        .map(|time| time.with_timezone(&chrono::Local));
    GraphMessage {
        id: message.id,
        mentions_me,
        quoted_context: typed_quote.as_ref().map(|(context,_)|context.clone()),
        quoted_contexts,
        client_message_id: message.client_message_id,
        timestamp: local
            .as_ref()
            .map(|time| time.with_timezone(&chrono::Utc).to_rfc3339())
            .unwrap_or_else(|| message.timestamp.clone()),
        date: local
            .as_ref()
            .map(|time| time.format("%d/%m/%Y").to_string())
            .unwrap_or_default(),
        author: crate::sender_names::resolve(&message.sender,Some(&message.sender_mri),known_authors),
        author_id: crate::avatars::person_id(&message.sender_mri).or_else(|| {
            message.application_id.as_deref().and_then(crate::avatars::application_id)
                .map(|id| format!("app:{id}"))
        }).or_else(|| message.sender_mri.strip_prefix("28:")
            .and_then(crate::avatars::person_id).map(|id| format!("bot:{id}"))),
        time: local
            .as_ref()
            .map(|time| time.format("%H:%M").to_string())
            .unwrap_or_else(|| message.timestamp.clone()),
        body: if duplicate_forward_content {
            String::new()
        } else if has_files {
            rich.as_ref().map(|doc| doc.clipboard_payload().text.trim().to_owned()).unwrap_or_default()
        } else if typed_quote.is_some() {
            quoted_plain.unwrap_or_default()
        } else if message.content.is_empty() && images.is_empty() && forwarded.is_none() {
            "[Anexo]".into()
        } else {
            message.content
        },
        rich,
        is_mine,
        is_deleted: false,
        images,
        reply_to: message.reply_to.or(quoted_parent),
        quoted_author: quote_details.as_ref().map(|(author, _)| author.clone()),
        quoted_body: quote_details.map(|(_, body)| body),
        forwarded,
        reactions: message.reaction_properties.as_ref().map(crate::reactions::from_properties).unwrap_or(message.reactions),
        files: message.files,
        importance: message.importance,
        edited: message.edited,
    }
}

fn native_error(error: impl std::fmt::Display) -> String {
    format!("Teams: {error}")
}

pub fn load_chats(session: TeamsSession) -> Result<LoadedChats, String> {
    let client = session.native_client()?;
    let (fallback_id, fallback_name) = session.account_hint();
    let (chats, user) = runtime()?.block_on(async {
        // Reuse the same /me response already loaded with the chat directory;
        // profile display fields do not need another request or token scope.
        let user = match client.graph_get("/me").await {
            Ok(response) => response.json::<OwnProfile>().await.ok(),
            Err(_) => None,
        };
        let owner = user
            .as_ref()
            .map(|user| user.id.as_str())
            .unwrap_or(&fallback_id);
        let chats = ost::api::list_chats_data_for_owner(&client, 100, owner)
            .await
            .map_err(native_error)?;
        Ok::<_, String>((chats, user))
    })?;
    let (user_id, user_name, user_email, company_name) = match user {
        Some(user) => {
            let email = user
                .mail
                .filter(|value| !value.trim().is_empty())
                .or(user.user_principal_name)
                .unwrap_or_default();
            (
                user.id,
                user.display_name.unwrap_or(fallback_name),
                email,
                user.company_name.unwrap_or_default(),
            )
        }
        None => (fallback_id, fallback_name, String::new(), String::new()),
    };
    let chats = chats
        .into_iter()
        .map(|chat| chat_summary(chat, &user_id, &user_name))
        .collect();
    let chats = reconcile_chat_directory(&[], chats, None);
    Ok(LoadedChats {
        session,
        user_id,
        user_name,
        user_email,
        company_name,
        chats,
    })
}

fn chat_summary(chat: ost::api::ChatInfo, owner: &str, own_name: &str) -> ChatSummary {
    let mine = chat.last_message_sender_mri.as_deref().and_then(|sender| {
        let owner = crate::avatars::person_id(owner)?;
        let sender = crate::avatars::person_id(sender)?;
        Some(sender == owner)
    });
    let preview = chat
        .last_message_preview
        .as_deref()
        .map(|body| {
            if mine == Some(true) {
                let body = if body.trim().is_empty() { "Mensagem" } else { body };
                format!("Você: {body}")
            } else {
                // A known other sender must not become "Você" just because the
                // display name matches the account. Without an id, the name remains the fallback.
                conversation_preview(
                    chat.is_group,
                    chat.last_message_sender.as_deref(),
                    body,
                    if mine == Some(false) { "" } else { own_name },
                )
            }
        })
        .unwrap_or_else(|| "Sem mensagens".into());
    ChatSummary {
        person_id: if chat.is_group {
            None
        } else {
            crate::avatars::direct_peer(&chat.id, owner)
        },
        is_group: chat.is_group,
        group_avatar: crate::avatars::GroupHint {
            picture: chat.picture,
            meeting: chat.is_meeting,
        },
        last_message_time: chat.last_message_time,
        last_message_is_mine: mine,
        muted: chat.muted,
        hidden: chat.hidden,
        unread: chat.unread,
        id: chat.id,
        title: chat.name,
        peer_name_verified: chat.peer_name_verified,
        subtitle: if chat.is_group {
            "Conversa em grupo".into()
        } else {
            "Conversa direta".into()
        },
        preview,
    }
}

/// Resolve a direct chat's peer without guessing from a display name. The
/// native roster supplies identities; no Graph chat-members grant is needed.
pub fn conversation_preview(group: bool, sender: Option<&str>, body: &str, own_name: &str) -> String {
    let sender = sender.unwrap_or_default().trim();
    let body = if body.trim().is_empty() { "Mensagem" } else { body };
    if !sender.is_empty() && sender == own_name.trim() {
        format!("Você: {body}")
    } else if group && !sender.is_empty() {
        format!("{}: {body}", sender.split_whitespace().next().unwrap_or(sender))
    } else { body.to_owned() }
}

/// The list paints the sender and the message in different colors.
/// Only the prefix written by [`conversation_preview`] splits: "Você" anywhere,
/// and a single-word sender in a group. A direct chat keeps "Reunião: amanhã" whole.
pub fn split_preview_sender(preview: &str, group: bool) -> Option<(&str, &str)> {
    let (name, body) = preview.split_once(": ")?;
    if name.is_empty() || body.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    (name == "Você" || group).then_some((name, body))
}

/// Read-only avatar metadata from the native thread response. Unlike the
/// directory view model, this retains picture and the roster for group images.
pub fn avatar_group_info(session: &TeamsSession, thread_id: &str, owner: &str) -> Result<crate::avatars::GroupInfo, String> {
    let client = session.native_client()?;
    runtime()?.block_on(avatar_group_info_with(&client, thread_id, owner))
}

async fn avatar_json(response: reqwest::Response) -> Result<Value, String> {
    avatar_json_limited(response, 1024 * 1024).await
}

async fn avatar_json_limited(mut response: reqwest::Response, limit: usize) -> Result<Value, String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(native_error)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err("Os dados do avatar excedem o limite local.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "Os dados do avatar são inválidos.".into())
}

fn merge_roster_into_thread(thread: &mut Value, roster: &Value) {
    if let Some(members) = roster.get("members").and_then(Value::as_array) {
        thread["members"] = Value::Array(members.clone());
    }
    if thread.get("totalMemberCount").is_none()
        && let Some(count) = roster.get("totalMemberCount").filter(|count| {
            count.as_u64().is_some_and(|value| value > 0)
                || count.as_i64().is_some_and(|value| value > 0)
        })
    {
        thread["totalMemberCount"] = count.clone();
    }
}

async fn avatar_group_info_with(client: &ost::api::client::TeamsClient, thread_id: &str, owner: &str) -> Result<crate::avatars::GroupInfo, String> {
    let service = validated_chat_service_url(&client.chat_service_url())?;
    let thread_id = validated_thread_id(thread_id)?;
    let url = service.join(&format!("/v1/threads/{}?view=msnp24Equivalent", urlencoding::encode(&thread_id)))
        .map_err(|_| "Não foi possível preparar o perfil do grupo.".to_owned())?.to_string();
    let mut value = avatar_json(client.chat_get(&url).await.map_err(native_error)?).await?;
    let thread = if value.get("thread").is_some() { &mut value["thread"] } else { &mut value };
    let preview = crate::avatars::parse_group_info(thread, owner)?;
    if !preview.meeting && thread.get("members").and_then(Value::as_array).is_none() {
        let url = group_members_endpoint(&client.chat_service_url(), &thread_id)?;
        let roster = match client.chat_get(&url).await {
            Ok(response) => avatar_json(response).await,
            Err(error) => Err(native_error(error)),
        };
        match roster {
            Ok(roster) => merge_roster_into_thread(thread, &roster),
            Err(_) if preview.picture.is_some() => return Ok(preview),
            Err(error) => return Err(error),
        }
    }
    crate::avatars::parse_group_info(&value, owner)
}

/// Resolves a bot/application icon by catalog identity, never by display name.
pub fn public_application_icon_bytes(value: &str) -> Result<Vec<u8>, String> {
    runtime()?.block_on(crate::avatar_assets::download(value))
}

pub fn application_icon_url(session: &TeamsSession, app: &str) -> Result<String, String> {
    let id = crate::avatars::application_id(app).ok_or("A identidade do aplicativo é inválida.")?;
    let client = session.native_client()?;
    runtime()?.block_on(async {
        let endpoint = ost::api::apps::definitions_url(&client.middle_tier_url());
        let response = client.mt_post(&endpoint, &serde_json::json!([id]))
            .await.map_err(native_error)?;
        crate::live_trace::record("avatar_app_catalog_http", 0, usize::from(response.status().as_u16()), 0);
        let value = avatar_json(response).await?;
        application_icon_from_catalog(&value, &id)
            .ok_or_else(|| "O catálogo não informou o ícone do aplicativo.".into())
    })
}

pub fn bot_icon_url(session: &TeamsSession, bot: &str) -> Result<String, String> {
    let bot = crate::avatars::person_id(bot).ok_or("A identidade do bot é inválida.")?;
    let client = session.native_client()?;
    runtime()?.block_on(async {
        let endpoint = format!("{}/beta/users/apps/definitionsv2", client.middle_tier_url().trim_end_matches('/'));
        let response = client.mt_get(&endpoint).await.map_err(|error| {
            let error = native_error(error);
            let code = [401, 403, 404, 429, 500, 502, 503, 504].into_iter().find(|code| error.contains(&code.to_string())).unwrap_or(0);
            crate::live_trace::record("avatar_bot_catalog_failed", 0, code, 0);
            error
        })?;
        crate::live_trace::record("avatar_bot_catalog_http", 0, usize::from(response.status().as_u16()), 0);
        let value = avatar_json_limited(response, 4 * 1024 * 1024).await?;
        bot_icon_from_catalog(&value, &bot).ok_or_else(|| "Não há definição única para o bot.".into())
    })
}

fn bot_icon_from_catalog(value: &Value, bot: &str) -> Option<String> {
    fn visit(value: &Value, bot: &str, matches: &mut HashMap<String, String>) {
        match value {
            Value::Object(object) => {
                let matched = object.get("bots").and_then(Value::as_array).is_some_and(|bots| bots.iter().any(|item| {
                    item.get("id").and_then(Value::as_str).is_some_and(|id| id.strip_prefix("28:").unwrap_or(id).eq_ignore_ascii_case(bot))
                }));
                if matched {
                    if let (Some(id), Some(icon)) = (object.get("id").and_then(Value::as_str), object.get("largeImageUrl").and_then(Value::as_str).or_else(|| object.get("icons")?.get("color")?.as_str())) {
                        if icon.len() <= 8192 { matches.insert(id.into(), icon.into()); }
                    }
                }
                for child in object.values() { visit(child, bot, matches); }
            }
            Value::Array(items) => for child in items { visit(child, bot, matches); },
            _ => {}
        }
    }
    let mut matches = HashMap::new();
    visit(value, bot, &mut matches);
    (matches.len() == 1).then(|| matches.into_values().next()).flatten()
}

fn application_icon_from_catalog(value: &Value, app: &str) -> Option<String> {
    let definition = ost::api::apps::definition_for(value, app)?;
    definition.get("largeImageUrl").and_then(Value::as_str)
        .or_else(|| definition.get("icons")?.get("color")?.as_str())
        .filter(|url| url.len() <= 8192).map(str::to_owned)
}

pub fn chat_peer(
    session: &TeamsSession,
    chat_id: &str,
    owner: &str,
) -> Result<Option<String>, String> {
    let Some(owner) = crate::avatars::person_id(owner) else {
        return Ok(None);
    };
    let client = session.native_client()?;
    let members = runtime()?.block_on(async {
        let url = format!(
            "{}/v1/threads/{}/members",
            client.chat_service_url(),
            urlencoding::encode(chat_id)
        );
        let response = client.chat_get(&url).await.map_err(native_error)?;
        let value = response.json::<Value>().await.map_err(native_error)?;
        ost::api::parse_thread_members(&value).map_err(native_error)
    })?;
    let mut peers: Vec<String> = members
        .into_iter()
        .filter_map(|member| {
            crate::avatars::person_id(member.user_id.as_deref().unwrap_or(&member.mri))
        })
        .filter(|id| id != &owner)
        .collect();
    peers.sort();
    peers.dedup();
    Ok(if peers.len() == 1 { peers.pop() } else { None })
}

pub fn load_chat_page(
    session: TeamsSession,
    chat_id: &str,
    user_id: &str,
    before: Option<&str>,
) -> Result<(TeamsSession, MessagePage), String> {
    load_chat_page_with_updates(session, chat_id, user_id, before, &[])
}

pub fn refresh_chat_page(
    session: TeamsSession,
    chat_id: &str,
    user_id: &str,
    message_updates: &[String],
) -> Result<(TeamsSession, MessagePage), String> {
    load_chat_page_with_updates(session, chat_id, user_id, None, message_updates)
}

fn load_chat_page_with_updates(
    session: TeamsSession,
    chat_id: &str,
    user_id: &str,
    before: Option<&str>,
    message_updates: &[String],
) -> Result<(TeamsSession, MessagePage), String> {
    let client = session.native_client()?;
    if let Some(before) = before {
        validate_history_link(before, &client.chat_service_url(), chat_id)?;
    }
    let page = runtime()?.block_on(read_chat_page(&client, chat_id, before, message_updates))?;
    let known_authors = page
        .messages
        .iter()
        .filter_map(|message| {
            let id = crate::avatars::person_id(&message.sender_mri)?;
            crate::sender_names::known(&message.sender)?;
            Some((id, message.sender.clone()))
        })
        .collect::<HashMap<_, _>>();
    let messages: Vec<_> = page.messages.into_iter()
        .map(|message| message_from_native_with_authors(message, user_id, &known_authors)).collect();
    let own_count = messages.iter().filter(|message| message.is_mine).count();
    crate::live_trace::record("message_ownership_totals", 0, own_count, messages.len() - own_count);
    Ok((
        session,
        MessagePage {
            messages,
            before: page.backward_link,
        },
    ))
}

async fn read_chat_page(
    client: &ost::api::client::TeamsClient,
    chat_id: &str,
    before: Option<&str>,
    message_updates: &[String],
) -> Result<ost::api::MessagesPage, String> {
    let mut page = ost::api::read_messages_page(client, chat_id, 50, before)
        .await
        .map_err(native_error)?;
    let responses = futures::future::join_all(
        message_updates.iter().map(|id| ost::api::read_message_with_client(client, chat_id, id)),
    ).await;
    let failed = responses.iter().filter(|result| result.is_err()).count();
    if failed != 0 {
        crate::live_trace::record("message_update_reads_failed", 0, failed, message_updates.len());
    }
    for message in responses.into_iter().filter_map(|result| result.ok().flatten()) {
        if let Some(recent) = page.messages.iter_mut().find(|recent| recent.id == message.id) {
            *recent = message;
        } else {
            page.messages.push(message);
        }
    }
    Ok(page)
}

fn validate_history_link(link: &str, service: &str, chat_id: &str) -> Result<(), String> {
    let link = url::Url::parse(link).map_err(|_| "Link de histórico inválido.".to_owned())?;
    let base = url::Url::parse(service).map_err(|_| "Servidor de chat inválido.".to_owned())?;
    let expected = format!("/v1/users/ME/conversations/{chat_id}/messages");
    let path =
        urlencoding::decode(link.path()).map_err(|_| "Link de histórico inválido.".to_owned())?;
    if link.origin() != base.origin()
        || path != expected
        || !link.username().is_empty()
        || link.password().is_some()
    {
        return Err("O histórico retornou um endereço inesperado.".into());
    }
    Ok(())
}

pub fn load_teams(session: TeamsSession) -> Result<(TeamsSession, Vec<TeamSummary>), String> {
    let mut next = Some(format!("{GRAPH_ROOT}/me/joinedTeams"));
    let mut teams = Vec::new();
    while let Some(url) = next {
        validate_graph_page(&url)?;
        let page: GraphPage<TeamDto> = request_json(&session, &url)?;
        teams.extend(page.value.into_iter().map(|t| TeamSummary {
            id: t.id,
            name: t.display_name,
        }));
        next = page.next_link;
    }
    Ok((session, teams))
}

pub fn load_channels(
    session: TeamsSession,
    team_id: &str,
) -> Result<(TeamsSession, Vec<ChannelSummary>), String> {
    let id = urlencoding::encode(team_id);
    let mut next = Some(format!(
        "{GRAPH_ROOT}/teams/{id}/channels?$select=id,displayName,membershipType"
    ));
    let mut channels = Vec::new();
    while let Some(url) = next {
        validate_graph_page(&url)?;
        let page: GraphPage<ChannelDto> = request_json(&session, &url)?;
        channels.extend(page.value.into_iter().map(|c| ChannelSummary {
            id: c.id,
            name: c.display_name,
        }));
        next = page.next_link;
    }
    Ok((session, channels))
}

fn validate_graph_page(url: &str) -> Result<(), String> {
    if !url.starts_with(&format!("{GRAPH_ROOT}/")) {
        Err("Microsoft Graph retornou um link de paginação inválido.".into())
    } else {
        Ok(())
    }
}

pub fn load_channel_messages(
    session: TeamsSession,
    _team_id: &str,
    channel_id: &str,
    user_id: &str,
) -> Result<(TeamsSession, MessagePage), String> {
    load_chat_page(session, channel_id, user_id, None)
}

pub fn send_channel_thread_reply_with_id(
    session: TeamsSession,
    channel_id: &str,
    root_message_id: &str,
    content: &str,
    document: Option<crate::message_body::Document>,
    client_id: &str,
) -> Result<(TeamsSession, GraphMessage), String> {
    let client = session.native_client()?;
    let receipt = if let Some(document) = &document {
        runtime()?.block_on(ost::api::thread_reply_rich_with_client_id(
            &client,
            channel_id,
            root_message_id,
            &document.to_html(),
            client_id,
        ))
    } else {
        runtime()?.block_on(ost::api::thread_reply_with_client_id(
            &client,
            channel_id,
            root_message_id,
            content,
            client_id,
        ))
    }
    .map_err(native_error)?;
    let now = chrono::Local::now();
    Ok((
        session,
        GraphMessage {
            id: receipt.id.unwrap_or_else(|| format!("local:{client_id}")),
            client_message_id: Some(client_id.to_owned()),
            timestamp: now.with_timezone(&chrono::Utc).to_rfc3339(),
            date: now.format("%d/%m/%Y").to_string(),
            author: "Você".into(),
            time: now.format("%H:%M").to_string(),
            body: content.to_owned(),
            rich: document.map(std::sync::Arc::new),
            is_mine: true,
            reply_to: Some(root_message_id.to_owned()),
            ..Default::default()
        },
    ))
}

pub fn send_message_with_id(
    session: TeamsSession,
    chat_id: &str,
    content: &str,
    client_id: &str,
) -> Result<(TeamsSession, GraphMessage), String> {
    let client = session.native_client()?;
    let receipt = runtime()?
        .block_on(ost::api::send_message_with_client_id(
            &client, chat_id, content, client_id,
        ))
        .map_err(native_error)?;
    let now = chrono::Local::now();
    Ok((
        session,
        GraphMessage {
            id: receipt.id.unwrap_or_else(|| format!("local:{client_id}")),
            client_message_id: Some(client_id.to_owned()),
            timestamp: now.with_timezone(&chrono::Utc).to_rfc3339(),
            date: now.format("%d/%m/%Y").to_string(),
            author: "Você".into(),
            time: now.format("%H:%M").to_string(),
            body: content.to_owned(),
            is_mine: true,
            ..Default::default()
        },
    ))
}

pub fn send_rich_message_with_id(
    session: TeamsSession,
    chat_id: &str,
    content: &str,
    document: crate::message_body::Document,
    client_id: &str,
) -> Result<(TeamsSession, GraphMessage), String> {
    let client = session.native_client()?;
    let html = document.to_html();
    let metadata=crate::mentions::outgoing_metadata(&document,chat_id);
    let receipt_id=if metadata.is_some() {
        let mut payload=ost::api::send_message_html_body(&html);
        crate::mentions::with_metadata(&mut payload,metadata);
        runtime()?.block_on(post_chat_payload(&client,chat_id,payload,client_id))?
    } else {
        runtime()?.block_on(ost::api::send_rich_message_with_client_id(&client,chat_id,&html,client_id)).map_err(native_error)?.id
    };
    let now = chrono::Local::now();
    Ok((
        session,
        GraphMessage {
            id: receipt_id.unwrap_or_else(|| format!("local:{client_id}")),
            client_message_id: Some(client_id.to_owned()),
            timestamp: now.with_timezone(&chrono::Utc).to_rfc3339(),
            date: now.format("%d/%m/%Y").to_string(),
            author: "Você".into(),
            time: now.format("%H:%M").to_string(),
            body: content.to_owned(),
            rich: Some(std::sync::Arc::new(document)),
            is_mine: true,
            ..Default::default()
        },
    ))
}

/// Upload one document and publish its native chat file reference.
/// Progress is reported as uploaded bytes over total bytes.
pub async fn upload_file_with_progress(
    session: TeamsSession,
    chat_id: String,
    local_path: String,
    expected_sha256: String,
    client_message_id: String,
    verify_first: bool,
    progress: impl Fn(u64, u64) + Sync,
) -> Result<(TeamsSession, GraphMessage), String> {
    let client = session.native_client()?;
    let file = ost::api::upload_file_data_idem_verified(
        &client,
        &chat_id,
        &local_path,
        Some(&progress),
        &client_message_id,
        verify_first,
        &expected_sha256,
    )
    .await
    .map_err(|error| {
        if error.downcast_ref::<ost::api::UploadFileChanged>().is_some() {
            "O arquivo foi alterado depois de anexado. Remova e anexe novamente.".to_owned()
        } else {
            native_error(error)
        }
    })?;
    let now = chrono::Local::now();
    let object_url = file.web_url.unwrap_or_default();
    let file_ref = ost::api::ChatFileRef {
            preview: None,
        attachment_id: file.attachment_id,
        name: file.name.clone(),
        file_type: file
            .name
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase()),
        object_url,
        share_url: file.share_url,
        sender: Some("Você".to_owned()),
        time: Some(now.with_timezone(&chrono::Utc).to_rfc3339()),
    };
    Ok((
        session,
        GraphMessage {
            id: format!("local:{client_message_id}"),
            client_message_id: Some(client_message_id),
            timestamp: now.with_timezone(&chrono::Utc).to_rfc3339(),
            date: now.format("%d/%m/%Y").to_string(),
            author: "Você".into(),
            time: now.format("%H:%M").to_string(),
            body: String::new(),
            is_mine: true,
            files: vec![file_ref],
            ..Default::default()
        },
    ))
}

async fn post_chat_payload(client:&ost::api::client::TeamsClient,chat_id:&str,mut payload:Value,client_id:&str)->Result<Option<String>,String> {
    payload["clientmessageid"]=serde_json::json!(client_id);
    let url=format!("{}/v1/users/ME/conversations/{}/messages",client.chat_service_url(),chat_id);
    let response=client.chat_post(&url,&payload).await.map_err(native_error)?;
    let location=response.headers().get(reqwest::header::LOCATION).and_then(|value|value.to_str().ok()).map(str::to_owned);
    let text=response.text().await.unwrap_or_default();
    Ok(ost::api::sent_id_from_response(location.as_deref(),&text))
}
async fn post_quote_with_client(client:&ost::api::client::TeamsClient,chat_id:&str,context:&crate::quoted_reply::Context,html:&str,client_id:&str,metadata:Option<Value>)->Result<Option<String>,String> {
    let mut payload=context.body(html)?;
    crate::mentions::with_metadata(&mut payload,metadata);
    post_chat_payload(client,chat_id,payload,client_id).await
}

pub fn send_reply_message_with_id(
    session: TeamsSession,
    chat_id: &str,
    parent_id: &str,
    parent_author: &str,
    parent_body: &str,
    context: Option<&crate::quoted_reply::Context>,
    content: &str,
    document: Option<crate::message_body::Document>,
    client_id: &str,
) -> Result<(TeamsSession, GraphMessage), String> {
    let context=context.ok_or("Não foi possível identificar o autor da citação. Reabra a conversa e tente novamente.")?;
    let html=document.as_ref().map(|doc|doc.to_html()).unwrap_or_else(||ost::api::build_message_html(content));
    let client=session.native_client()?;
    let receipt=runtime()?.block_on(post_quote_with_client(&client,chat_id,context,&html,client_id,document.as_ref().and_then(|doc|crate::mentions::outgoing_metadata(doc,chat_id))))?;
    let now = chrono::Local::now();
    let quoted_body = ost::api::reply_snippet(parent_body);
    Ok((
        session,
        GraphMessage {
            id: receipt.unwrap_or_else(|| format!("local:{client_id}")),
            client_message_id: Some(client_id.to_owned()),
            timestamp: now.with_timezone(&chrono::Utc).to_rfc3339(),
            date: now.format("%d/%m/%Y").to_string(),
            author: "Você".into(),
            time: now.format("%H:%M").to_string(),
            body: content.to_owned(),
            rich: document.map(std::sync::Arc::new),
            is_mine: true,
            reply_to: Some(parent_id.to_owned()),
            quoted_author: Some(parent_author.to_owned()),
            quoted_body: Some(quoted_body),
            quoted_context: Some(context.clone()),
            ..Default::default()
        },
    ))
}

pub fn mutate_chat_reaction(
    session: TeamsSession,
    chat_id: &str,
    message_id: &str,
    emoji: &str,
    remove: bool,
) -> Result<(TeamsSession, ()), String> {
    let client = session.native_client()?;
    let request = crate::reactions::request(
        &client.chat_service_url(), chat_id, message_id, emoji, remove,
        chrono::Utc::now().timestamp_millis(),
    )?;
    runtime()?.block_on(send_chat_reaction(&client, request))?;
    Ok((session, ()))
}

async fn send_chat_reaction(client: &ost::api::client::TeamsClient, request: crate::reactions::Request) -> Result<(), String> {
    let result = if request.remove {
        client.chat_delete(&request.url, Some(&request.body)).await
    } else {
        client.chat_put(&request.url, &request.body).await
    };
    result.map(|_| ()).map_err(crate::reactions::request_error)
}

pub fn edit_chat_message(
    session: TeamsSession,
    chat_id: &str,
    message_id: &str,
    content: &str,
    formatting: &crate::message_body::ComposerFormatting,
    context: Option<&crate::quoted_reply::Context>,
) -> Result<(TeamsSession, ()), String> {
    let client=session.native_client()?;
    let body=crate::quoted_reply::edited_body(message_id,content,formatting,context)?;
    let url=ost::api::message_url(&client.chat_service_url(),chat_id,message_id);
    runtime()?.block_on(client.chat_put(&url,&body)).map_err(native_error)?;
    Ok((session,()))
}

pub fn delete_chat_message(
    session: TeamsSession,
    chat_id: &str,
    message_id: &str,
) -> Result<(TeamsSession, ()), String> {
    let client = session.native_client()?;
    runtime()?
        .block_on(ost::api::delete_message_with_client(
            &client, chat_id, message_id,
        ))
        .map_err(native_error)?;
    Ok((session, ()))
}

pub fn set_chat_muted(
    session: TeamsSession,
    chat_id: &str,
    muted: bool,
) -> Result<(), String> {
    let client = session.native_client()?;
    runtime()?
        .block_on(ost::api::set_chat_muted_with_client(&client, chat_id, muted))
        .map_err(native_error)?;
    Ok(())
}

pub fn set_chat_hidden(
    session: TeamsSession,
    chat_id: &str,
    hidden: bool,
) -> Result<(), String> {
    let client = session.native_client()?;
    runtime()?
        .block_on(ost::api::set_chat_hidden_with_client(&client, chat_id, hidden))
        .map_err(native_error)
}

pub fn send_chat_typing(session: TeamsSession, chat_id: &str) -> Result<(), String> {
    let client = session.native_client()?;
    runtime()?
        .block_on(ost::api::send_typing_indicator_with_client(&client, chat_id))
        .map_err(native_error)
}

pub fn mark_chat_read(
    session: TeamsSession,
    chat_id: &str,
    message_id: &str,
) -> Result<(TeamsSession, ()), String> {
    let client = session.native_client()?;
    runtime()?
        .block_on(ost::api::mark_read_with_client(&client, chat_id, message_id))
        .map_err(native_error)?;
    Ok((session, ()))
}

pub fn load_chat_read_receipts(
    session: TeamsSession,
    chat_id: &str,
) -> Result<(TeamsSession, Vec<ost::api::ReadReceipt>), String> {
    let client = session.native_client()?;
    let receipts = runtime()?
        .block_on(ost::api::read_receipts_data(&client, chat_id))
        .map_err(native_error)?;
    Ok((session, receipts))
}

pub fn reply_excerpt(content: &str) -> String {
    ost::api::reply_snippet(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_summary_identifies_last_sender_by_mri_not_display_name() {
        let owner = "11111111-1111-1111-1111-111111111111";
        let info = |sender: Option<&str>| ost::api::ChatInfo {
            id: "chat".into(), name: "Chat".into(), peer_name_verified: false,
            is_group: false, picture: None, is_meeting: false, muted: None,
            hidden: None, unread: None, last_message_time: None,
            last_message_sender: Some("Identical display name".into()),
            last_message_sender_mri: sender.map(str::to_owned), last_message_preview: None,
        };
        assert_eq!(chat_summary(info(Some("8:orgid:11111111-1111-1111-1111-111111111111")), owner, "Identical display name").last_message_is_mine, Some(true));
        assert_eq!(chat_summary(info(Some("8:orgid:22222222-2222-2222-2222-222222222222")), owner, "Identical display name").last_message_is_mine, Some(false));
        assert_eq!(chat_summary(info(None), owner, "Identical display name").last_message_is_mine, None);
    }

    #[test]
    fn own_last_message_preview_says_voce_from_the_sender_id() {
        let owner = "11111111-1111-1111-1111-111111111111";
        let info = |sender: Option<&str>, group: bool| ost::api::ChatInfo {
            id: "chat".into(), name: "Chat".into(), peer_name_verified: false,
            is_group: group, picture: None, is_meeting: false, muted: None,
            hidden: None, unread: None, last_message_time: None,
            last_message_sender: Some("Nome Da Conta".into()),
            last_message_sender_mri: sender.map(str::to_owned),
            last_message_preview: Some("tá usando?".into()),
        };
        assert_eq!(
            chat_summary(info(Some("8:orgid:11111111-1111-1111-1111-111111111111"), false), owner, "Outro rótulo").preview,
            "Você: tá usando?"
        );
        assert_eq!(
            chat_summary(info(Some("8:orgid:22222222-2222-2222-2222-222222222222"), false), owner, "Nome Da Conta").preview,
            "tá usando?"
        );
        assert_eq!(
            chat_summary(info(Some("8:orgid:22222222-2222-2222-2222-222222222222"), true), owner, "Nome Da Conta").preview,
            "Nome: tá usando?"
        );
        assert_eq!(
            chat_summary(info(None, false), owner, "Nome Da Conta").preview,
            "Você: tá usando?"
        );
    }

    #[test]
    fn roster_total_survives_when_the_thread_view_has_no_member_count() {
        let mut thread = serde_json::json!({"threadProperties": {"threadType": "chat"}});
        merge_roster_into_thread(&mut thread, &serde_json::json!({
            "totalMemberCount": 16,
            "members": [{"id": "8:orgid:22222222-2222-2222-2222-222222222222", "friendlyName": "Ana"}]
        }));
        let group = crate::avatars::parse_group_info(&thread, "11111111-1111-1111-1111-111111111111").unwrap();
        assert_eq!(group.member_count, Some(16));
        assert_eq!(group.roster_names.len(), 1);
        let mut counted = serde_json::json!({"totalMemberCount": 9});
        merge_roster_into_thread(&mut counted, &serde_json::json!({"totalMemberCount": 16, "members": []}));
        assert_eq!(counted["totalMemberCount"], 9);
    }

    #[test]
    fn own_message_identity_accepts_canonical_and_prefixed_account_ids() {
        let id = "11111111-1111-1111-1111-111111111111";
        assert!(own_message_identity(&format!("8:orgid:{id}"), id));
        assert!(own_message_identity(&format!("8:ORGID:{id}"), &format!("8:orgid:{id}")));
        assert!(own_message_identity(id, &format!("8:orgid:{id}")));
        assert!(!own_message_identity(&format!("28:{id}"), id));
        assert!(!own_message_identity("22222222-2222-2222-2222-222222222222", id));
    }

    #[test]
    fn bot_without_app_metadata_retains_a_downloadable_identity() {
        let bot = "11111111-1111-1111-1111-111111111111";
        let message = ost::api::MessageInfo {
            reaction_properties: None,
            mention_properties: None,
            card_wire_shape: None,
            id: "fixture".into(), sender_mri: format!("28:{bot}"), application_id: None,
            sender: "Application".into(), timestamp: String::new(), content: "hello".into(),
            raw: "hello".into(), reactions: vec![], reply_to: None, client_message_id: None,
            forwarded: None, files: vec![], importance: ost::api::MessageImportance::Normal, edited: false,
        };
        let message = message_from_native(message, "owner");
        assert_eq!(message.author_id.as_deref(), Some(format!("bot:{bot}").as_str()));
        assert!(matches!(message.author_id.as_deref().and_then(crate::avatars::photo_target), Some(crate::avatars::Target::Bot(_))));
    }

    #[test]
    fn bot_catalog_resolution_requires_one_exact_bot_match() {
        let bot = "11111111-1111-1111-1111-111111111111";
        let definition = serde_json::json!({"id":"app-a", "bots":[{"id":bot}], "largeImageUrl":"https://statics.teams.cdn.office.net/a.png"});
        assert!(bot_icon_from_catalog(&serde_json::json!({"value":[definition.clone()]}), bot).is_some());
        let mut second = definition.clone(); second["id"] = "app-b".into();
        assert!(bot_icon_from_catalog(&serde_json::json!([definition, second]), bot).is_none());
        assert!(bot_icon_from_catalog(&serde_json::json!({"name":"Workflows"}), bot).is_none());
    }

    #[test]
    fn group_preview_names_sender_and_direct_preview_does_not_repeat_peer() {
        assert_eq!(conversation_preview(true, Some("Rodrigo Silva"), "Já estamos validando", "Alex"), "Rodrigo: Já estamos validando");
        assert_eq!(conversation_preview(false, Some("Rodrigo Silva"), "Olá", "Alex"), "Olá");
        assert_eq!(conversation_preview(false, Some("Alex"), "Olá", "Alex"), "Você: Olá");
        assert_eq!(
            split_preview_sender("Rodrigo: Já estamos validando", true),
            Some(("Rodrigo", "Já estamos validando"))
        );
        assert_eq!(
            split_preview_sender("Ana: Reunião: amanhã", true),
            Some(("Ana", "Reunião: amanhã"))
        );
        assert_eq!(split_preview_sender("Você: sextou", false), Some(("Você", "sextou")));
        assert_eq!(split_preview_sender("Olá", false), None);
        assert_eq!(split_preview_sender("Reunião: amanhã", false), None);
    }

    #[test]
    fn application_icon_uses_matching_catalog_identity_not_display_name() {
        let value = serde_json::json!({"definitions":[
            {"id":"com.microsoft.teamspace.tab.flow", "largeImageUrl":"https://statics.teams.cdn.office.net/fixture/icon.png"},
            {"id":"another-app", "name":"Workflows", "largeImageUrl":"https://example.test/wrong.png"}
        ]});
        assert_eq!(application_icon_from_catalog(&value, "com.microsoft.teamspace.tab.flow").as_deref(), Some("https://statics.teams.cdn.office.net/fixture/icon.png"));
        assert!(application_icon_from_catalog(&value, "missing-app").is_none());
    }

    #[test]
    fn cancelling_graph_search_drops_the_in_flight_request_future() {
        struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = runtime().unwrap().block_on(async {
            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let request_dropped = std::sync::Arc::clone(&dropped);
            let request = async move {
                let _drop_flag = DropFlag(request_dropped);
                let _ = started_tx.send(());
                std::future::pending::<Result<(), String>>().await
            };
            let task = tokio::spawn(with_search_cancellation(cancel_rx, request));
            started_rx.await.unwrap();
            cancel_tx.send(()).unwrap();
            task.await.unwrap()
        });

        assert_eq!(result, Err("Busca cancelada.".into()));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn sender_identity_uses_mri_instead_of_display_name() {
        let message = ost::api::MessageInfo {
            reaction_properties: None,
            mention_properties: None,
            card_wire_shape: None,
            id: "1".into(),
            sender_mri: "8:orgid:ME".into(),
            application_id: None,
            sender: "Same display name".into(),
            timestamp: "2026-10-26T12:30:00Z".into(),
            content: "Hello".into(),
            raw: "Hello".into(),
            reactions: vec![],
            reply_to: None,
            client_message_id: None,
            forwarded: None,
            files: vec![], importance: ost::api::MessageImportance::Normal, edited: false,
        };
        let message = message_from_native(message, "me");
        assert!(message.is_mine);
        assert_eq!(message.time.len(), 5);
    }

    #[test]
    fn native_message_reactions_reach_the_chat_view_model() {
        let message = ost::api::MessageInfo {
            reaction_properties: None,
            mention_properties: None,
            card_wire_shape: None,
            id: "message-1".into(),
            sender_mri: "8:orgid:PEER".into(),
            application_id: None,
            sender: "Ana".into(),
            timestamp: "2026-10-26T12:30:00Z".into(),
            content: "Ship it".into(),
            raw: "Ship it".into(),
            reactions: vec![ost::api::ReactionCount {
                emoji: "👍".into(),
                count: 2,
                reactors: vec![ost::api::Reactor {
                    id: "8:orgid:ME".into(),
                    name: "Current user".into(),
                }],
            }],
            reply_to: None,
            client_message_id: None,
            forwarded: None,
            files: vec![], importance: ost::api::MessageImportance::High, edited: true,
        };

        let adapted = message_from_native(message, "ME");

        assert_eq!(adapted.importance, ost::api::MessageImportance::High);
        assert!(adapted.edited);
        assert_eq!(adapted.reactions.len(), 1);
        assert_eq!(adapted.reactions[0].emoji, "👍");
        assert_eq!(adapted.reactions[0].count, 2);
        assert_eq!(adapted.reactions[0].reactors[0].name, "Current user");
    }

    #[test]
    fn forwarded_context_preserves_original_author_and_channel_metadata() {
        let original_id = "aaaaaaaa-1111-2222-3333-444444444444";
        let known_authors = HashMap::from([(original_id.to_owned(), "Ana Paula".to_owned())]);
        let message = ost::api::MessageInfo {
            reaction_properties: None,
            mention_properties: None,
            card_wire_shape: None,
            id: "forward-1".into(),
            sender_mri: "8:orgid:bbbbbbbb-1111-2222-3333-444444444444".into(),
            application_id: None,
            sender: "Rui".into(),
            timestamp: "2026-10-26T13:00:00Z".into(),
            content: "Veja esta publicação".into(),
            raw: "<p>Veja esta publicação</p>".into(),
            reactions: vec![],
            reply_to: None,
            client_message_id: None,
            forwarded: Some(ost::api::ForwardedMessageContext {
                message_id: Some("original-42".into()),
                sender_mri: Some(format!("8:orgid:{original_id}")),
                arrival_time: Some("2026-10-26T12:30:00Z".into()),
                content: Some("<p>Release aprovado</p>".into()),
                message_type: Some("RichText/Html".into()),
                channel_subject: Some("Publicação da versão".into()),
                announcement: Some(true),
                deleted: Some(false),
            }),
            files: vec![], importance: ost::api::MessageImportance::Normal, edited: false,
        };

        let adapted = message_from_native_with_authors(message, "ME", &known_authors);
        let forwarded = adapted.forwarded.unwrap();
        assert_eq!(forwarded.message_id.as_deref(), Some("original-42"));
        assert_eq!(forwarded.author.as_deref(), Some("Ana Paula"));
        assert_eq!(
            forwarded.sender_mri.as_deref(),
            Some("8:orgid:aaaaaaaa-1111-2222-3333-444444444444")
        );
        assert_eq!(forwarded.author_id.as_deref(), Some(original_id));
        assert_eq!(forwarded.subject.as_deref(), Some("Publicação da versão"));
        assert_eq!(forwarded.message_type.as_deref(), Some("RichText/Html"));
        assert!(forwarded.announcement);
        assert!(!forwarded.deleted);
        assert_eq!(forwarded.content, "Release aprovado");
        assert!(forwarded.rich.is_some());
        assert_eq!(
            forwarded.timestamp.as_deref(),
            Some("2026-10-26T12:30:00Z")
        );
    }

    #[test]
    fn native_quote_reply_keeps_parent_fields_without_duplicating_quote_body() {
        let message = ost::api::MessageInfo {
            reaction_properties: None,
            mention_properties: None,
            card_wire_shape: None,
            id: "reply-2".into(),
            sender_mri: "8:orgid:ME".into(),
            application_id: None,
            sender: "Current user".into(),
            timestamp: "2026-10-26T12:30:00Z".into(),
            content: "I will take it".into(),
            raw: "<quote author=\"Ana &amp; Co\" guid=\"message-1\"><p>Ship &amp; verify</p></quote><p>I will take it</p>".into(),
            reactions: vec![],
            reply_to: Some("message-1".into()),
            client_message_id: None,
            forwarded: None,
            files: vec![], importance: ost::api::MessageImportance::Normal, edited: false,
        };
        let message = message_from_native(message, "ME");
        assert_eq!(message.reply_to.as_deref(), Some("message-1"));
        assert_eq!(message.quoted_author.as_deref(), Some("Ana & Co"));
        assert_eq!(message.quoted_body.as_deref(), Some("Ship & verify"));
        assert_eq!(message.body, "I will take it");
        let quoted_text = message.rich.unwrap().clipboard_payload().text;
        assert_eq!(quoted_text.trim(), "I will take it");
    }

    #[test]
    fn reply_excerpt_matches_protocol_quote_length() {
        let excerpt = reply_excerpt(&"a ".repeat(100));
        assert!(excerpt.chars().count() <= ost::api::REPLY_SNIPPET_MAX + 1);
        assert!(excerpt.ends_with('…'));
    }

    #[test]
    fn pagination_cannot_send_graph_token_to_another_host() {
        assert!(validate_graph_page("https://graph.microsoft.com.evil.test/v1.0/me").is_err());
        assert!(validate_graph_page("https://graph.microsoft.com/v1.0/me").is_ok());
    }

    #[test]
    fn history_link_is_bound_to_the_service_and_conversation() {
        let base = "https://amer.ng.msg.teams.microsoft.com";
        let path = "/v1/users/ME/conversations/test-chat/messages?start=10";
        assert!(validate_history_link(&format!("{base}{path}"), base, "test-chat").is_ok());
        assert!(
            validate_history_link(&format!("https://example.test{path}"), base, "test-chat")
                .is_err()
        );
        assert!(validate_history_link(&format!("{base}{path}"), base, "other-chat").is_err());
    }

    #[test]
    fn private_feeds_never_become_chats_or_return_through_selected_and_hidden_state() {
        let feed = |id: &str| ChatSummary {
            id: id.into(), title: "Notifications".into(), hidden: Some(true), unread: Some(true),
            ..Default::default()
        };
        let direct = ChatSummary { id: "19:a_b@unq.gbl.spaces".into(), title: "Notifications".into(), ..Default::default() };
        let group = ChatSummary { id: "19:project@thread.v2".into(), title: "Notifications".into(), is_group: true, ..Default::default() };
        let previous = vec![feed("48:notifications"), feed("48:calllogs"), feed("48:notes")];
        let latest = vec![feed("48:notifications"), direct.clone(), feed("48:mentions"), group.clone()];
        let chats = reconcile_chat_directory(&previous, latest, Some("48:notifications"));
        assert_eq!(chats.iter().map(|chat| chat.id.as_str()).collect::<Vec<_>>(), [direct.id.as_str(), group.id.as_str()]);
        assert!(chats.iter().all(|chat| chat.title == "Notifications"));
        assert!(reconcile_chat_directory(&previous, Vec::new(), Some("48:notes")).is_empty());
    }

    #[test]
    fn chat_refresh_deduplicates_by_thread_id_and_keeps_selected_outside_snapshot() {
        let chat = |id: &str, title: &str, preview: &str| ChatSummary {
            peer_name_verified: false,
            id: id.into(),
            title: title.into(),
            preview: preview.into(),
            subtitle: "Conversa direta".into(),
            person_id: Some(id.into()),
            is_group: false,
            group_avatar: Default::default(),
        last_message_time: None,
        last_message_is_mine: None,
            muted: None,
            hidden: None,
            unread: None,
        };
        let mut current_chat = chat("thread:current", "Current", "Last seen");
        current_chat.muted = Some(true);
        let mut hidden_chat = chat("thread:hidden", "Hidden", "Out of sight");
        hidden_chat.hidden = Some(true);
        hidden_chat.unread = Some(true);
        let previous = vec![current_chat, hidden_chat];
        let refreshed = reconcile_chat_directory(
            &previous,
            vec![
                chat("thread:new", "New", "Old preview"),
                chat("thread:new", "New updated", "Latest preview"),
            ],
            Some("thread:current"),
        );

        assert_eq!(refreshed.len(), 3);
        assert_eq!(refreshed[0].id, "thread:new");
        assert_eq!(refreshed[0].title, "New updated");
        assert_eq!(refreshed[0].preview, "Latest preview");
        assert_eq!(refreshed[1].id, "thread:current");
        assert_eq!(refreshed[2].id, "thread:hidden");
        assert_eq!(refreshed[2].hidden, Some(true));
        assert_eq!(refreshed[2].unread, Some(true));

        let refreshed = reconcile_chat_directory(
            &refreshed,
            vec![chat("thread:hidden", "Hidden", "Fresh preview")],
            None,
        );
        assert_eq!(refreshed[0].unread, Some(true));

        let current_is_present = reconcile_chat_directory(
            &previous,
            vec![chat("thread:current", "Renamed", "Updated")],
            Some("thread:current"),
        );
        assert_eq!(current_is_present.len(), 2);
        assert_eq!(current_is_present[0].title, "Renamed");
        assert_eq!(current_is_present[0].muted, Some(true));
        assert_eq!(current_is_present[1].id, "thread:hidden");
    }

    #[test]
    fn native_message_transport_uses_skype_token_and_upstream_parser() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|v| v == b"\r\n\r\n") {
                let mut chunk = [0u8; 2048];
                let count = socket.read(&mut chunk).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let request = String::from_utf8(bytes).unwrap().to_lowercase();
            assert!(
                request
                    .starts_with("get /v1/users/me/conversations/test-chat/messages?pagesize=50&view=")
            );
            assert!(request.contains("supportsmessageproperties"));
            assert!(request.contains("authentication: skypetoken=skype-fixture"));
            assert!(!request.contains("bearer graph-fixture"));
            let body = serde_json::json!({"messages":[{
                "id":"123", "messagetype":"RichText/Html", "content":"<p>Native <strong>message</strong></p>",
                "imdisplayname":"Same name", "originalarrivaltime":"2026-10-26T12:30:00Z",
                "from":"https://example.test/v1/users/ME/contacts/8:orgid:me"
            }]}).to_string();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let mut config = ost::config::Config::default();
        config.set_skype_token("skype-fixture".into(), Some(3600));
        config.set_graph_token("graph-fixture".into(), Some(3600));
        config.set_region_gtms(serde_json::json!({"chatService":base}));
        let client = ost::api::client::TeamsClient::from_config(config);
        let messages = runtime()
            .unwrap()
            .block_on(ost::api::read_messages_data(&client, "test-chat", 50))
            .unwrap();
        let message = message_from_native(messages.into_iter().next().unwrap(), "me");
        assert_eq!(message.body, "Native message");
        assert!(
            message.rich.as_ref().unwrap().blocks[0]
                .runs
                .iter()
                .any(|run| run.text == "message" && run.style.bold)
        );
        assert!(message.is_mine);
        server.join().unwrap();
    }

    #[test]
    fn typed_quote_is_posted_with_native_metadata_and_survives_server_echo() {
        let server=tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base=format!("http://{}",server.server_addr());
        let owner="aaaaaaaa-1111-2222-3333-444444444444";
        let peer="bbbbbbbb-1111-2222-3333-444444444444";
        let context=crate::quoted_reply::Context{message_id:"1793400000000".into(),sender:format!("8:orgid:{peer}"),author:"Ana & QA".into(),timestamp_ms:Some(1793400000000),preview:"Original <safe>".into()};
        let worker=std::thread::spawn(move|| {
            let mut request=server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(request.method(),&tiny_http::Method::Post);
            assert_eq!(request.url(),"/v1/users/ME/conversations/test-chat/messages");
            assert!(request.headers().iter().any(|h|h.field.equiv("Authentication")&&h.value.as_str()=="skypetoken=skype-fixture"));
            let body:Value=serde_json::from_reader(request.as_reader()).unwrap();
            assert_eq!(body["clientmessageid"],"quote-client-key");
            let quoted:Value=serde_json::from_str(body["properties"]["qtdMsgs"].as_str().unwrap()).unwrap();
            assert_eq!(quoted[0]["messageId"],"1793400000000");assert_eq!(quoted[0]["sender"],format!("8:orgid:{peer}"));
            request.respond(tiny_http::Response::from_string("{\"OriginalArrivalTime\":1793400000001}").with_status_code(201)).unwrap();
            let request=server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(request.method(),&tiny_http::Method::Get);
            let echo=serde_json::json!({"messages":[{"id":"1793400000001","messagetype":"RichText/Html","content":body["content"],"imdisplayname":"Current user","originalarrivaltime":"2026-10-31T11:41:00Z","from":format!("https://example.test/v1/users/ME/contacts/8:orgid:{owner}"),"properties":body["properties"]}]});
            request.respond(tiny_http::Response::from_string(echo.to_string())).unwrap();
        });
        let mut config=ost::config::Config::default();config.set_skype_token("skype-fixture".into(),Some(3600));
        config.set_region_gtms(serde_json::json!({"chatService":base}));let client=ost::api::client::TeamsClient::from_config(config);
        let receipt=runtime().unwrap().block_on(post_quote_with_client(&client,"test-chat",&context,"<p>Teste</p>","quote-client-key",None)).unwrap();
        assert_eq!(receipt.as_deref(),Some("1793400000001"));
        let messages=runtime().unwrap().block_on(ost::api::read_messages_data(&client,"test-chat",50)).unwrap();
        let message=message_from_native(messages.into_iter().next().unwrap(),owner);
        assert_eq!(message.body,"Teste");assert_eq!(message.reply_to.as_deref(),Some("1793400000000"));
        assert_eq!(message.quoted_author.as_deref(),Some("Ana & QA"));
        assert_eq!(message.quoted_contexts, vec![context.clone()]);
        assert_eq!(message.quoted_context,Some(context));
        assert!(message.is_mine);worker.join().unwrap();
    }

    #[test]
    fn people_search_query_is_bounded_escaped_and_targets_identity_fields() {
        let path = people_search_path("  O'Neil ", 200).unwrap();
        let url = url::Url::parse(&format!("https://graph.microsoft.com/v1.0{path}")).unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.path(), "/v1.0/users");
        assert!(!path.starts_with("/v1.0/"));
        assert_eq!(query.get("$top").map(String::as_str), Some("20"));
        assert_eq!(query.get("$count").map(String::as_str), Some("true"));
        assert_eq!(
            query.get("$select").map(String::as_str),
            Some("id,displayName,mail,jobTitle,userPrincipalName")
        );
        let filter = query.get("$filter").unwrap();
        assert!(filter.contains("startswith(mail,'O''Neil')"));
        assert!(filter.contains("startswith(userPrincipalName,'O''Neil')"));
        assert!(filter.contains("startswith(displayName,'O''Neil')"));
        assert!(filter.contains("accountEnabled eq true"));
        assert!(people_search_path("x", 20).is_err());
        assert!(people_search_path(&"x".repeat(PEOPLE_SEARCH_MAX_QUERY_CHARS + 1), 20).is_err());
        assert!(people_search_path("ab\ncd", 20).is_err());
    }

    #[test]
    fn people_results_keep_duplicate_names_disambiguated_by_stable_ids() {
        let first_id = "aaaaaaaa-1111-2222-3333-444444444444";
        let second_id = "bbbbbbbb-1111-2222-3333-444444444444";
        let people = parse_people_search_response(&serde_json::json!({
            "value": [
                {"id": first_id, "displayName": "Alex Silva", "mail": "alex.one@example.test"},
                {"id": second_id, "displayName": "Alex Silva", "userPrincipalName": "alex.two@example.test", "jobTitle": "Analista"},
                {"id": "cccccccc-1111-2222-3333-444444444444", "displayName": "Sam Silva"},
                {"id": "dddddddd-1111-2222-3333-444444444444"},
                {"id": first_id, "displayName": "Duplicate id", "mail": "ignored@example.test"},
                {"id": "not-an-object-id", "displayName": "Invalid"}
            ]
        }))
        .unwrap();

        assert_eq!(people.len(), 3);
        assert_ne!(people[0].id, people[1].id);
        assert_eq!(people[0].display_name, "Alex Silva");
        assert_eq!(people[0].email, "alex.one@example.test");
        assert_eq!(people[1].display_name, "Alex Silva");
        assert_eq!(people[1].email, "alex.two@example.test");
        assert_eq!(people[1].job_title.as_deref(), Some("Analista"));
        assert_eq!(people[2].display_name, "Sam Silva");
        assert!(people[2].email.is_empty());
    }

    #[test]
    fn people_search_uses_consistent_graph_header_against_local_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (request_tx, request_rx) = mpsc::channel();
        let body = serde_json::json!({"value": [{
            "id":"bbbbbbbb-1111-2222-3333-444444444444",
            "displayName":"Alex Silva",
            "mail":"alex@example.test"
        }]}).to_string();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|value| value == b"\r\n\r\n") {
                let mut chunk = [0u8; 2048];
                let count = socket.read(&mut chunk).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            request_tx.send(String::from_utf8(bytes).unwrap()).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });

        let results = runtime()
            .unwrap()
            .block_on(search_people_with("alex", 10, |path| async move {
                let response = ureq::get(&format!("{base}{path}"))
                    .header("ConsistencyLevel", "eventual")
                    .call()
                    .map_err(|_| "local Graph fixture request failed".to_owned())?;
                response
                    .into_body()
                    .read_json::<Value>()
                    .map_err(|_| "local Graph fixture JSON failed".to_owned())
            }))
            .unwrap();

        let request = request_rx.recv().unwrap().to_lowercase();
        assert!(request.starts_with("get /users?"));
        assert!(request.contains("consistencylevel: eventual"));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].email, "alex@example.test");
        server.join().unwrap();
    }

    #[test]
    fn direct_chat_creation_uses_local_native_thread_transport_and_checks_identity() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;

        let owner = "aaaaaaaa-1111-2222-3333-444444444444";
        let peer = "bbbbbbbb-1111-2222-3333-444444444444";
        let person = PersonSearchResult {
            id: peer.into(),
            display_name: "Alex Silva".into(),
            email: "alex@example.test".into(),
            job_title: None,
        };
        let expected_id = direct_thread_id(owner, peer).unwrap();
        let encoded_id = expected_id.replace(':', "%3A").replace('@', "%40");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let location = format!("{base}/v1/threads/{encoded_id}");
        let (request_tx, request_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            loop {
                let header_end = bytes
                    .windows(4)
                    .position(|value| value == b"\r\n\r\n")
                    .map(|index| index + 4);
                if let Some(header_end) = header_end {
                    let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
                    let content_length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|length| length.trim().parse::<usize>().ok())
                        .unwrap_or_default();
                    if bytes.len() >= header_end + content_length {
                        break;
                    }
                }
                let mut chunk = [0u8; 2048];
                let count = socket.read(&mut chunk).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let request = String::from_utf8(bytes).unwrap();
            let lowered = request.to_lowercase();
            assert!(lowered.contains("authentication: skypetoken=chat-fixture"));
            request_tx.send(request).unwrap();
            write!(socket, "HTTP/1.1 201 Created\r\nLocation: {location}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
        });

        let mut config = ost::config::Config::default();
        config.set_skype_token("chat-fixture".into(), Some(3600));
        config.set_region_gtms(serde_json::json!({"chatService":base}));
        let client = ost::api::client::TeamsClient::from_config(config);
        let chat = runtime()
            .unwrap()
            .block_on(create_direct_chat_with(&base, owner, &person, |url, body| async move {
                let response = client.chat_post(&url, &body).await.map_err(native_error)?;
                Ok(ThreadCreateReply {
                    status: response.status().as_u16(),
                    location: response
                        .headers()
                        .get("Location")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned),
                })
            }))
            .unwrap();

        let request = request_rx.recv().unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("POST /v1/threads HTTP/1.1"));
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["properties"]["threadType"], "chat");
        assert_eq!(body["properties"]["chatFilesIndexId"], "2");
        assert_eq!(body["properties"]["fixedRoster"], "true");
        assert_eq!(body["properties"]["uniquerosterthread"], "true");
        assert_eq!(body["members"].as_array().unwrap().len(), 2);
        assert!(body["members"].as_array().unwrap().iter().any(|member| {
            member["id"] == format!("8:orgid:{owner}")
        }));
        assert!(body["members"].as_array().unwrap().iter().any(|member| {
            member["id"] == format!("8:orgid:{peer}")
        }));
        assert_eq!(chat.id, expected_id);
        assert_eq!(chat.person_id.as_deref(), Some(peer));
        assert!(direct_thread_id(owner, owner).is_err());
        assert!(direct_thread_body(owner, owner).is_err());
        server.join().unwrap();
    }

    #[test]
    fn direct_chat_location_must_match_service_and_expected_roster() {
        let owner = "aaaaaaaa-1111-2222-3333-444444444444";
        let peer = "bbbbbbbb-1111-2222-3333-444444444444";
        let expected = direct_thread_id(owner, peer).unwrap();
        let valid = format!("/v1/threads/{}", expected.replace(':', "%3A").replace('@', "%40"));
        assert_eq!(
            parse_direct_thread_location("https://chat.example.test", &valid, &expected).unwrap(),
            expected
        );
        assert!(parse_direct_thread_location(
            "https://chat.example.test",
            "https://evil.example.test/v1/threads/19%3Atest%40unq.gbl.spaces",
            &expected
        )
        .is_err());
        assert!(parse_direct_thread_location(
            "https://chat.example.test",
            "/v1/threads/19%3Aother%40unq.gbl.spaces",
            &expected
        )
        .is_err());
    }

    fn fixture_person(id: &str, name: &str) -> PersonSearchResult {
        PersonSearchResult {
            id: id.into(),
            display_name: name.into(),
            email: format!("{}@example.test", name.to_lowercase().replace(' ', ".")),
            job_title: None,
        }
    }

    #[test]
    fn group_create_body_keeps_owner_and_unique_selected_guids() {
        let owner = "aaaaaaaa-1111-2222-3333-444444444444";
        let first = "bbbbbbbb-1111-2222-3333-444444444444";
        let second = "cccccccc-1111-2222-3333-444444444444";
        let body = group_thread_body(
            owner,
            &[
                fixture_person(first, "Alex One"),
                fixture_person(second, "Alex Two"),
                fixture_person(first, "Duplicate"),
                fixture_person(owner, "Current user"),
            ],
            "  Projeto X  ",
        )
        .unwrap();

        assert_eq!(body["properties"]["threadType"], "chat");
        assert_eq!(body["properties"]["chatFilesIndexId"], "2");
        assert_eq!(body["properties"]["topic"], "Projeto X");
        assert_eq!(body["members"].as_array().unwrap().len(), 3);
        assert_eq!(body["members"][0]["id"], format!("8:orgid:{owner}"));
        assert_eq!(body["members"][1]["id"], format!("8:orgid:{first}"));
        assert_eq!(body["members"][2]["id"], format!("8:orgid:{second}"));
        assert!(body["members"].as_array().unwrap().iter().all(|member| member["role"] == "Admin"));
        assert!(body["properties"].get("fixedRoster").is_none());
        assert!(body["properties"].get("uniquerosterthread").is_none());
    }

    #[test]
    fn group_validation_rejects_invalid_names_and_insufficient_people() {
        let owner = "aaaaaaaa-1111-2222-3333-444444444444";
        let first = fixture_person("bbbbbbbb-1111-2222-3333-444444444444", "Alex");
        assert!(normalized_group_title("  \n ").is_err());
        assert!(normalized_group_title(&"x".repeat(GROUP_CHAT_MAX_TITLE_CHARS + 1)).is_err());
        assert!(normalized_group_title("Grupo\ninválido").is_err());
        assert!(group_thread_body(owner, std::slice::from_ref(&first), "Grupo").is_err());
        assert!(group_thread_body(owner, &[first.clone(), first], "Grupo").is_err());
        assert!(group_thread_body(owner, &[], "Grupo").is_err());
    }

    #[test]
    fn group_routes_encode_thread_and_member_ids_and_bind_topic_property() {
        let base = "https://chat.example.test/";
        let thread = "19:room@thread.v2";
        assert_eq!(
            group_thread_endpoint(base, thread, "members").unwrap(),
            "https://chat.example.test/v1/threads/19%3Aroom%40thread.v2/members"
        );
        let title = url::Url::parse(&group_title_endpoint(base, thread).unwrap()).unwrap();
        assert_eq!(title.path(), "/v1/threads/19%3Aroom%40thread.v2/properties");
        assert_eq!(title.query_pairs().find(|(key, _)| key == "name").unwrap().1, "topic");
        let roster = url::Url::parse(&group_members_endpoint(base, thread).unwrap()).unwrap();
        let roster_query: HashMap<_, _> = roster.query_pairs().into_owned().collect();
        assert_eq!(roster_query.get("view").map(String::as_str), Some("msnp24Equivalent"));
        assert_eq!(roster_query.get("pageSize").map(String::as_str), Some("100"));
        let mri = "8:orgid:bbbbbbbb-1111-2222-3333-444444444444";
        assert_eq!(
            group_thread_endpoint(base, thread, &format!("members/{}", urlencoding::encode(mri))).unwrap(),
            "https://chat.example.test/v1/threads/19%3Aroom%40thread.v2/members/8%3Aorgid%3Abbbbbbbb-1111-2222-3333-444444444444"
        );
        assert_eq!(
            group_leave_endpoint(base, thread, "aaaaaaaa-1111-2222-3333-444444444444").unwrap(),
            "https://chat.example.test/v1/threads/19%3Aroom%40thread.v2/members/8%3Aorgid%3Aaaaaaaaa-1111-2222-3333-444444444444"
        );
        assert!(group_leave_endpoint(base, thread, "invalid-owner").is_err());
        assert!(group_thread_endpoint(base, "../../evil", "members").is_err());
        assert!(group_thread_endpoint(base, thread, "members/../../evil").is_err());
    }

    #[test]
    fn group_location_is_bound_to_same_service_and_thread_route() {
        assert_eq!(
            parse_created_group_location(
                "https://chat.example.test",
                "/v1/threads/19%3Aroom%40thread.v2"
            ).unwrap(),
            "19:room@thread.v2"
        );
        for invalid in [
            "https://evil.example.test/v1/threads/19%3Aroom%40thread.v2",
            "/v2/threads/19%3Aroom%40thread.v2",
            "/v1/threads/../evil",
            "/v1/threads/19%3Aroom%40thread.v2?other=1",
        ] {
            assert!(parse_created_group_location("https://chat.example.test", invalid).is_err());
        }
    }

    #[test]
    fn native_member_parser_keeps_identity_role_and_deduplicates() {
        let first = "8:orgid:aaaaaaaa-1111-2222-3333-444444444444";
        let second = "8:orgid:bbbbbbbb-1111-2222-3333-444444444444";
        let members = parse_chat_members(&serde_json::json!({"members": [
            {"id": first, "role": "Admin", "friendlyName": "Ari"},
            {"id": second, "role": "User", "properties": {"friendlyName": "Alex Silva"}},
            {"id": "8:orgid:cccccccc-1111-2222-3333-444444444444"},
            {"id": first, "role": "User", "friendlyName": "duplicate"},
            {"id": "", "role": "Admin"}
        ]})).unwrap();
        assert_eq!(members.len(), 3);
        assert_eq!(members[0].mri, first);
        assert_eq!(members[0].person_id.as_deref(), Some("aaaaaaaa-1111-2222-3333-444444444444"));
        assert_eq!(members[0].display_name, "Ari");
        assert_eq!(members[0].role.as_deref(), Some("Admin"));
        assert_eq!(members[1].display_name, "Alex Silva");
        assert_eq!(members[1].role.as_deref(), Some("User"));
        assert_eq!(members[2].display_name, "");
        assert_eq!(members[2].role, None);
        assert!(parse_chat_members(&serde_json::json!({"unexpected": []})).is_err());
    }

    #[test]
    fn mention_directory_name_matches_the_roster_id_and_skips_named_rows() {
        let ana = "bbbbbbbb-2222-2222-2222-222222222222";
        let bruno = "cccccccc-3333-3333-3333-333333333333";
        let owner = "aaaaaaaa-1111-1111-1111-111111111111";
        assert_eq!(mention_directory_path(&format!("8:ORGID:{ana}")).as_deref(), Some(format!("/users/{ana}?$select=displayName").as_str()));
        assert!(mention_directory_path("bot").is_none());
        assert_eq!(directory_display_name(&serde_json::json!({"id": ana, "displayName": "Ana Beatriz"}), ana).as_deref(), Some("Ana Beatriz"));
        assert!(directory_display_name(&serde_json::json!({"id": bruno, "displayName": "Outra"}), ana).is_none());
        assert!(directory_display_name(&serde_json::json!({"displayName": "Ana Beatriz"}), ana).is_none());
        assert!(directory_display_name(&serde_json::json!({"id": ana, "displayName": ""}), ana).is_none());
        let members = vec![
            ChatMemberSummary { mri: format!("8:orgid:{ana}"), person_id: Some(ana.into()), display_name: String::new(), role: None },
            ChatMemberSummary { mri: format!("8:orgid:{bruno}"), person_id: Some(bruno.into()), display_name: "Bruno".into(), role: None },
            ChatMemberSummary { mri: format!("8:orgid:{owner}"), person_id: Some(owner.into()), display_name: String::new(), role: None },
            ChatMemberSummary { mri: "8:bot".into(), person_id: None, display_name: String::new(), role: None },
        ];
        let targets = mention_name_targets(&members);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].2, ana);
        assert_eq!(targets[1].2, owner);
        assert!(!targets.iter().any(|target| target.2 == bruno));
    }

    #[test]
    fn group_create_posts_to_native_chat_service_and_checks_location() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let owner = "aaaaaaaa-1111-2222-3333-444444444444";
        let peers = vec![
            fixture_person("bbbbbbbb-1111-2222-3333-444444444444", "Alex One"),
            fixture_person("cccccccc-1111-2222-3333-444444444444", "Alex Two"),
        ];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut bytes = Vec::new();
            loop {
                let header_end = bytes.windows(4).position(|value| value == b"\r\n\r\n").map(|i| i + 4);
                if let Some(header_end) = header_end {
                    let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
                    let content_length = headers.lines().find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|length| length.trim().parse::<usize>().ok()).unwrap_or_default();
                    if bytes.len() >= header_end + content_length { break; }
                }
                let mut chunk = [0u8; 2048];
                let count = socket.read(&mut chunk).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let request = String::from_utf8(bytes).unwrap();
            assert!(request.to_ascii_lowercase().contains("authentication: skypetoken=group-create-fixture"));
            request_tx.send(request).unwrap();
            let location = "/v1/threads/19%3Agrouproom%40thread.v2";
            write!(socket, "HTTP/1.1 201 Created\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let mut config = ost::config::Config::default();
        config.set_skype_token("group-create-fixture".into(), Some(3600));
        config.set_region_gtms(serde_json::json!({"chatService":base}));
        let client = ost::api::client::TeamsClient::from_config(config);
        let chat = runtime().unwrap().block_on(create_group_chat_with(
            &base,
            owner,
            "Projeto X",
            &peers,
            |url, body| async move {
                let response = client.chat_post(&url, &body).await.map_err(native_error)?;
                Ok(ThreadCreateReply {
                    status: response.status().as_u16(),
                    location: response.headers().get("Location").and_then(|v| v.to_str().ok()).map(str::to_owned),
                })
            },
        )).unwrap();
        let request = request_rx.recv().unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(headers.to_ascii_lowercase().starts_with("post /v1/threads http/1.1"));
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["properties"]["topic"], "Projeto X");
        assert_eq!(body["properties"]["threadType"], "chat");
        assert_eq!(body["members"].as_array().unwrap().len(), 3);
        assert!(body["properties"].get("fixedRoster").is_none());
        assert_eq!(chat.id, "19:grouproom@thread.v2");
        assert!(chat.is_group);
        server.join().unwrap();
    }

    #[test]
    fn native_roster_read_uses_chat_service_query_and_skype_auth() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (request_tx, request_rx) = mpsc::channel();
        let body = serde_json::json!({"members":[{
            "id":"8:orgid:aaaaaaaa-1111-2222-3333-444444444444",
            "role":"Admin",
            "friendlyName":"Ari Silva"
        }]}).to_string();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|value| value == b"\r\n\r\n") {
                let mut chunk = [0u8; 2048];
                let count = socket.read(&mut chunk).unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let request = String::from_utf8(bytes).unwrap();
            let lowered = request.to_ascii_lowercase();
            assert!(lowered.starts_with("get /v1/threads/19%3aroom%40thread.v2/members?"));
            assert!(lowered.contains("view=msnp24equivalent"));
            assert!(lowered.contains("pagesize=100"));
            assert!(lowered.contains("authentication: skypetoken=roster-fixture"));
            request_tx.send(()).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let mut config = ost::config::Config::default();
        config.set_skype_token("roster-fixture".into(), Some(3600));
        config.set_region_gtms(serde_json::json!({"chatService":base.clone()}));
        let client = ost::api::client::TeamsClient::from_config(config);
        let url = group_members_endpoint(&base, "19:room@thread.v2").unwrap();
        let members = runtime().unwrap().block_on(async {
            let response = client.chat_get(&url).await.map_err(native_error)?;
            let value = response.json::<Value>().await.map_err(native_error)?;
            parse_chat_members(&value)
        }).unwrap();
        request_rx.recv().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].display_name, "Ari Silva");
        assert_eq!(members[0].role.as_deref(), Some("Admin"));
        server.join().unwrap();
    }

    #[test]
    fn group_rename_roster_and_leave_use_native_verbs_and_skype_auth() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let expected = [
                ("put", "properties?name=topic", "new title"),
                ("put", "members", "bbbbbbbb-1111-2222-3333-444444444444"),
                ("delete", "members/8%3aorgid%3abbbbbbbb-1111-2222-3333-444444444444", ""),
                ("delete", "members/8%3aorgid%3aaaaaaaaa-1111-2222-3333-444444444444", ""),
            ];
            for (method, suffix, body_match) in expected {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut bytes = Vec::new();
                loop {
                    let header_end = bytes.windows(4).position(|value| value == b"\r\n\r\n").map(|i| i + 4);
                    if let Some(header_end) = header_end {
                        let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
                        let content_length = headers.lines().find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|length| length.trim().parse::<usize>().ok()).unwrap_or_default();
                        if bytes.len() >= header_end + content_length { break; }
                    }
                    let mut chunk = [0u8; 2048];
                    let count = socket.read(&mut chunk).unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let request = String::from_utf8(bytes).unwrap();
                let lowered = request.to_ascii_lowercase();
                assert!(lowered.starts_with(&format!("{method} /v1/threads/19%3aroom%40thread.v2/{suffix}")));
                assert!(lowered.contains("authentication: skypetoken=group-mutation-fixture"));
                assert!(lowered.contains(&body_match.to_ascii_lowercase()));
                write!(socket, "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        });
        let mut config = ost::config::Config::default();
        config.set_skype_token("group-mutation-fixture".into(), Some(3600));
        config.set_region_gtms(serde_json::json!({"chatService":base.clone()}));
        let client = ost::api::client::TeamsClient::from_config(config);
        let thread_id = "19:room@thread.v2";
        let owner = "aaaaaaaa-1111-2222-3333-444444444444";
        let mri = "8:orgid:bbbbbbbb-1111-2222-3333-444444444444";
        let operations = [
            (group_title_endpoint(&base, thread_id).unwrap(), ThreadMutation::Put(serde_json::json!({"topic":"new title"}))),
            (group_thread_endpoint(&base, thread_id, "members").unwrap(), ThreadMutation::Put(serde_json::json!({"members":[{"id":mri,"role":"Admin"}]}))),
            (group_thread_endpoint(&base, thread_id, &format!("members/{}", urlencoding::encode(mri))).unwrap(), ThreadMutation::Delete),
            (group_leave_endpoint(&base, thread_id, owner).unwrap(), ThreadMutation::Delete),
        ];
        for (endpoint, operation) in operations {
            let client_ref = &client;
            runtime().unwrap().block_on(mutate_group_with(
                &base,
                endpoint,
                operation,
                |url, operation| async move {
                    let response = match operation {
                        ThreadMutation::Put(body) => client_ref.chat_put(&url, &body).await,
                        ThreadMutation::Delete => client_ref.chat_delete(&url, None).await,
                    }.map_err(native_error)?;
                    Ok(ThreadMutationReply { status: response.status().as_u16() })
                },
            )).unwrap();
        }
        server.join().unwrap();
    }
}

#[cfg(test)]
mod reaction_transport_tests {
    use super::*;

    #[test]
    fn chat_update_reads_an_older_message_and_keeps_its_own_like() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let chat = "19:fixture@thread.v2";
        let worker = std::thread::spawn(move || {
            let recent = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(recent.method().as_str(), "GET");
            assert!(recent.url().contains("/messages?pageSize=50"));
            recent.respond(tiny_http::Response::from_string(
                r#"{"messages":[{"id":"99","messagetype":"Text","content":"Recente"}]}"#,
            )).unwrap();

            let changed = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(changed.method().as_str(), "GET");
            let url = url::Url::parse(&format!("http://fixture{}", changed.url())).unwrap();
            assert_eq!(
                url.path(),
                "/v1/users/ME/conversations/19%3Afixture%40thread.v2/messages/42"
            );
            assert!(url.query_pairs().any(|(key, value)| {
                key == "view" && value == "msnp24Equivalent|supportsMessageProperties"
            }));
            let message = serde_json::json!({
                "id": "42", "messagetype": "Text", "content": "Mais antiga",
                "properties": {
                    "emotions": [{"key": "LIKE", "users": [{"mri": "8:orgid:user"}]}],
                    "emotionsSummary": []
                },
                "annotationsSummary": {"emotions": {"like": 1}}
            });
            changed.respond(tiny_http::Response::from_string(message.to_string())).unwrap();
        });
        let mut config = ost::config::Config::default();
        config.set_skype_token("history-reaction-fixture".into(), Some(3600));
        config.set_region_gtms(serde_json::json!({"chatService": base}));
        let client = ost::api::client::TeamsClient::from_config(config);
        let page = runtime().unwrap().block_on(read_chat_page(&client, chat, None, &["42".into()])).unwrap();
        worker.join().unwrap();
        assert_eq!(page.messages.len(), 2);
        let updated = page.messages.into_iter().find(|message| message.id == "42").unwrap();
        let message = message_from_native(updated, "user");
        assert_eq!(message.reactions.len(), 1);
        assert_eq!(message.reactions[0].emoji, "👍");
        assert_eq!(message.reactions[0].count, 1);
        assert!(crate::reactions::contains_own(&message.reactions[0], "user"));
    }

    #[test]
    fn extended_reaction_history_crosses_the_native_http_adapter_without_losing_keys() {
        let server=tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url=format!("http://{}/fixture/messages",server.server_addr());
        let worker=std::thread::spawn(move || {
            let incoming=server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(incoming.method().as_str(),"GET");
            let body=serde_json::json!({"messages":[{
                "id":"42","messagetype":"Text","content":"Mensagem fictícia",
                "from":"https://chat.test/contacts/8:orgid:peer","imdisplayname":"Pessoa",
                "originalarrivaltime":"2026-11-11T10:00:00Z",
                "properties":{"emotions":[{"key":"1f44a_oncomingfist-tone1","users":[{"mri":"8:orgid:user","displayName":"Pessoa atual"}]}],
                    "emotionsSummary":[{"key":"1f44a_oncomingfist-tone1","count":8}]}
            }]});
            incoming.respond(tiny_http::Response::from_string(body.to_string())).unwrap();
        });
        let mut config=ost::config::Config::default();
        config.set_skype_token("history-reaction-fixture".into(),Some(3600));
        let client=ost::api::client::TeamsClient::from_config(config);
        let mut page=runtime().unwrap().block_on(ost::api::read_messages_page(&client,"fixture",50,Some(&url))).unwrap();
        worker.join().unwrap();
        assert_eq!(page.messages.len(),1);
        let adapted=message_from_native(page.messages.pop().unwrap(),"user");
        assert_eq!(adapted.reactions.len(),1);
        assert_eq!(adapted.reactions[0].emoji,"1f44a_oncomingfist-tone1");
        assert_eq!(adapted.reactions[0].count,8);
        assert!(crate::reactions::contains_own(&adapted.reactions[0],"user"));
    }

    #[test]
    fn native_image_files_without_html_slots_survive_and_follow_the_caption() {
        let server=tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url=format!("http://{}/fixture/messages",server.server_addr());
        let worker=std::thread::spawn(move || {
            let request=server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            let files=serde_json::json!([{"id":"native-file","version":2,"title":"capture.png","type":"png","objectUrl":"https://example.sharepoint.com/capture.png","filePreview":{"previewUrl":"https://api.asm.skype.com/v1/objects/0-native/views/imgo","previewWidth":500,"previewHeight":300}}]).to_string();
            let body=serde_json::json!({"messages":[
                {"id":"caption-file","messagetype":"RichText/Html","content":"<p>Oi, equipe.</p><p><a href='https://example.test/build'>Ver build</a></p>","properties":{"files":files.clone()}},
                {"id":"image-only-file","messagetype":"RichText/Html","content":"","properties":{"files":files}}
            ]});
            request.respond(tiny_http::Response::from_string(body.to_string())).unwrap();
        });
        let mut config=ost::config::Config::default();config.set_skype_token("file-no-slot-fixture".into(),Some(3600));
        let client=ost::api::client::TeamsClient::from_config(config);
        let page=runtime().unwrap().block_on(ost::api::read_messages_page(&client,"fixture",50,Some(&url))).unwrap();
        worker.join().unwrap();
        let mut messages=page.messages.into_iter().map(|message|message_from_native(message,"owner")).collect::<Vec<_>>();
        let caption=messages.iter().find(|message|message.id=="caption-file").unwrap();
        assert_eq!(caption.images.len(),1,"native file preview does not require an HTML slot");
        let doc=caption.rich.as_ref().unwrap();
        assert_eq!(doc.blocks[0].runs[0].text,"Oi, equipe.");
        assert!(doc.blocks[1].runs[0].style.link.is_some());
        assert!(doc.blocks[2].media.is_some());
        assert_eq!(messages.len(),2,"image-only file must survive native history filtering");
        let image=messages.iter_mut().find(|message|message.id=="image-only-file").unwrap();
        assert_eq!(image.images.len(),1);
    }

    #[test]
    fn received_mixed_file_messages_keep_caption_links_and_image_order() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/fixture/messages", server.server_addr());
        let worker = std::thread::spawn(move || {
            let request = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            let body = serde_json::json!({"messages":[{"id":"mixed-file", "messagetype":"RichText/Html", "content":"<p>Olá, equipe.</p><p>O build falhou: <a href='https://example.test/build'>ver build</a></p><attachment id='file-1'></attachment><p>Podem conferir?</p>", "imdisplayname":"Ana", "properties":{"files":serde_json::json!([{"id":"file-1","version":2,"title":"captura.png","type":"png","objectUrl":"https://example.sharepoint.com/captura.png","fileInfo":{},"filePreview":{"previewUrl":"https://api.asm.skype.com/v1/objects/0-file/views/imgpsh_fullsize","previewWidth":500,"previewHeight":300}}]).to_string()}}]});
            request.respond(tiny_http::Response::from_string(body.to_string())).unwrap();
        });
        let mut config = ost::config::Config::default();
        config.set_skype_token("mixed-file-fixture".into(), Some(3600));
        let client = ost::api::client::TeamsClient::from_config(config);
        let mut page = runtime().unwrap().block_on(ost::api::read_messages_page(&client,"fixture",50,Some(&url))).unwrap();
        worker.join().unwrap();
        let adapted = message_from_native(page.messages.pop().unwrap(), "owner");
        assert!(adapted.body.contains("Olá, equipe."), "file metadata must not erase the caption");
        let doc = adapted.rich.as_ref().expect("mixed message keeps its rich document");
        assert_eq!(adapted.images.len(), 1, "typed file preview must reach the media loader");
        assert!(doc.blocks[1].runs.iter().any(|run|run.style.link.as_deref()==Some("https://example.test/build")));
        assert!(doc.blocks[2].media.is_some(), "preview occupies the attachment position");
        assert_eq!(doc.blocks[3].runs[0].text, "Podem conferir?");
        assert!(!doc.clipboard_payload().text.contains("Anexo sem prévia"));
    }

    #[test]
    fn ordinal_mentions_in_history_resolve_by_metadata_identity() {
        let server=tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url=format!("http://{}/fixture/messages",server.server_addr());
        let owner="aaaaaaaa-1111-2222-3333-444444444444";
        let worker=std::thread::spawn(move || {
            let request=server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            let mentions=serde_json::json!([{"itemid":"0","mri":format!("8:orgid:{owner}"),"mentionType":"person","displayName":"Pessoa atual"}]);
            let body=serde_json::json!({"messages":[{"id":"mention-fixture","messagetype":"RichText/Html","content":"<p>Olá <span itemtype='http://schema.skype.com/Mention' itemid='0'>Pessoa atual</span></p>","imdisplayname":"Pessoa de teste","properties":{"mentions":mentions.to_string()}}]});
            request.respond(tiny_http::Response::from_string(body.to_string())).unwrap();
        });
        let mut config=ost::config::Config::default();
        config.set_skype_token("mentions-history-fixture".into(),Some(3600));
        let client=ost::api::client::TeamsClient::from_config(config);
        let mut page=runtime().unwrap().block_on(ost::api::read_messages_page(&client,"fixture",50,Some(&url))).unwrap();
        worker.join().unwrap();
        let adapted=message_from_native(page.messages.pop().unwrap(),owner);
        assert!(adapted.mentions_me,"ordinal itemid must resolve to the MRI in properties.mentions");
    }

    fn exercise(remove: bool, status: u16) -> Result<(), String> {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let worker = std::thread::spawn(move || {
            let mut incoming = server.recv_timeout(Duration::from_secs(5)).unwrap().expect("local reaction request");
            assert_eq!(incoming.method().as_str(), if remove { "DELETE" } else { "PUT" });
            assert_eq!(incoming.url(), "/v1/users/ME/conversations/19%3Aroom%40thread.v2/messages/42/properties?name=emotions");
            assert!(incoming.headers().iter().any(|header| {
                header.field.equiv("Authentication") && header.value.as_str() == "skypetoken=reaction-fixture"
            }));
            assert!(!incoming.headers().iter().any(|header| header.field.equiv("Authorization")));
            let mut body = String::new();
            incoming.as_reader().read_to_string(&mut body).unwrap();
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["emotions"]["key"], "surprised");
            if remove { assert!(body["emotions"].get("value").is_none()); }
            else { assert_eq!(body["emotions"]["value"], 1720000000123_i64); }
            incoming.respond(tiny_http::Response::empty(status)).unwrap();
        });
        let mut config = ost::config::Config::default();
        config.set_skype_token("reaction-fixture".into(), Some(3600));
        let client = ost::api::client::TeamsClient::from_config(config);
        let request = crate::reactions::request(&base, "19:room@thread.v2", "42", "😮", remove, 1720000000123).unwrap();
        let result = runtime().unwrap().block_on(send_chat_reaction(&client, request));
        worker.join().unwrap();
        result
    }

    #[test]
    fn native_emotion_add_uses_put_and_accepts_empty_service_ack() {
        exercise(false, 204).unwrap();
    }

    #[test]
    fn native_emotion_remove_uses_delete_with_key_in_body() {
        exercise(true, 204).unwrap();
    }

    #[test]
    fn denied_emotion_is_a_failure_without_exposing_service_url() {
        let error = exercise(false, 403).unwrap_err();
        assert_eq!(error, "O Teams não permitiu alterar esta reação.");
        assert!(!error.contains("http"));
    }
}

#[cfg(test)]
mod avatar_group_transport_tests {
    use super::*;
    const ME: &str = "11111111-1111-1111-1111-111111111111";
    const A: &str = "22222222-2222-2222-2222-222222222222";
    const B: &str = "33333333-3333-3333-3333-333333333333";

    fn exercise(include_members: bool) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let worker = std::thread::spawn(move || {
            let roster = serde_json::json!([
                {"id":format!("8:orgid:{ME}")},
                {"id":format!("8:orgid:{A}"),"friendlyName":"Ana"},
                {"id":format!("8:orgid:{B}"),"friendlyName":"Bruno"}
            ]);
            let mut body = serde_json::json!({"properties":{"threadType":"chat","picture":"image@https://example.test/group.png"}});
            if include_members {
                body["members"] = roster.clone();
            } else {
                body["threadProperties"] = body["properties"].clone();
                body["properties"] = serde_json::json!({"alerts":"false"});
            }
            for index in 0..if include_members { 1 } else { 2 } {
                let request = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                assert_eq!(request.method().as_str(), "GET");
                assert!(request.headers().iter().any(|header| header.field.equiv("Authentication") && header.value.as_str() == "skypetoken=avatar-fixture"));
                assert!(!request.headers().iter().any(|header| header.field.equiv("Authorization")));
                if index == 0 {
                    assert_eq!(request.url(), "/v1/threads/19%3Agroup%40thread.v2?view=msnp24Equivalent");
                    request.respond(tiny_http::Response::from_string(body.to_string())).unwrap();
                } else {
                    assert_eq!(request.url(), "/v1/threads/19%3Agroup%40thread.v2/members?view=msnp24Equivalent&pageSize=100");
                    request.respond(tiny_http::Response::from_string(serde_json::json!({"members":roster}).to_string())).unwrap();
                }
            }
        });
        let mut config = ost::config::Config::default();
        config.set_skype_token("avatar-fixture".into(), Some(3600));
        config.set_region_gtms(serde_json::json!({"chatService":base}));
        let client = ost::api::client::TeamsClient::from_config(config);
        let group = runtime().unwrap().block_on(avatar_group_info_with(&client, "19:group@thread.v2", ME)).unwrap();
        assert_eq!(group.members.len(), 2);
        assert_eq!(group.members[0].id, A);
        assert_eq!(group.picture.as_deref(), Some("image@https://example.test/group.png"));
        worker.join().unwrap();
    }

    #[test]
    fn avatar_thread_response_retains_custom_picture_and_members_without_duplicate_roster_read() { exercise(true); }

    #[test]
    fn avatar_thread_without_roster_reads_native_members_before_building_a_composite() { exercise(false); }
    #[test]
    fn mention_payload_posts_native_metadata_and_keeps_quoted_reply_properties() {
        let server=tiny_http::Server::http("127.0.0.1:0").unwrap();let base=format!("http://{}",server.server_addr());
        let worker=std::thread::spawn(move||{
            let mut request=server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(request.url(),"/v1/users/ME/conversations/test-chat/messages");
            let payload:Value=serde_json::from_reader(request.as_reader()).unwrap();
            assert_eq!(payload["clientmessageid"],"mention-key");
            let metadata:Value=serde_json::from_str(payload["properties"]["mentions"].as_str().unwrap()).unwrap();
            assert_eq!(metadata[0]["mri"],"8:orgid:11111111-1111-1111-1111-111111111111");
            assert!(payload["content"].as_str().unwrap().contains("itemid=\"0\""));
            assert!(payload["properties"]["qtdMsgs"].as_str().is_some());
            request.respond(tiny_http::Response::from_string("{\"id\":\"fixture-result\"}").with_status_code(201)).unwrap();
        });
        let mut config=ost::config::Config::default();config.set_skype_token("skype-fixture".into(),Some(3600));
        config.set_region_gtms(serde_json::json!({"chatService":base}));
        let client=ost::api::client::TeamsClient::from_config(config);
        let formatting=crate::message_body::ComposerFormatting{mentions:vec![crate::mention_composer::Range{start:0,end:3,entry:crate::mention_composer::Entry{id:"11111111-1111-1111-1111-111111111111".into(),name:"Ana".into(),everyone:false}}],..Default::default()};
        let document=crate::message_body::Document::from_editor("Ana, olá",&formatting);
        let context=crate::quoted_reply::Context{message_id:"1793400000000".into(),sender:"8:orgid:22222222-2222-2222-2222-222222222222".into(),author:"Bruno".into(),timestamp_ms:Some(1793400000000),preview:"Original".into()};
        runtime().unwrap().block_on(post_quote_with_client(&client,"test-chat",&context,&document.to_html(),"mention-key",crate::mentions::outgoing_metadata(&document,"test-chat"))).unwrap();
        worker.join().unwrap();
    }

}
