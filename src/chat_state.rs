use crate::graph::GraphMessage;
use sha2::Digest;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

const MAX_RECENT_CHAT_EVENT_FINGERPRINTS: usize = 512;
const MAX_RECENT_MESSAGE_EDITS: usize = 64;
const MAX_RECENT_MESSAGE_EDIT_BODY_BYTES: usize = 16 * 1024;
const RECENT_MESSAGE_EDIT_TTL: Duration = Duration::from_secs(60);
const MAX_CHAT_EVENT_CONVERSATIONS: usize = 64;
const MAX_CHAT_EVENT_VISITED_NODES: usize = 4096;
const MAX_CHAT_EVENT_DEPTH: usize = 48;
const MAX_CHAT_CONVERSATION_ID_CHARS: usize = 512;
const MAX_TYPING_INDICATORS: usize = 128;
const TYPING_INDICATOR_TTL: Duration = Duration::from_secs(22);

pub fn new_client_message_id() -> String {
    ost::api::new_client_message_id()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatEventImpact {
    pub has_message_change: bool,
    pub has_unscoped_message_change: bool,
    pub conversation_ids: Vec<String>,
    pub message_updates: Vec<(String, String)>,
}

/// A typed ChatService start signal, before matching it to a calendar event.
#[derive(Clone, PartialEq, Eq)]
pub struct MeetingStartedSignal {
    pub conversation_id: String,
    pub message_id: String,
    pub timestamp: Option<String>,
    pub participants: Vec<String>,
    pub participant_count: usize,
}

fn call_content_ended(content: &str) -> bool {
    let lower = content.trim_start().to_ascii_lowercase();
    lower.starts_with("<ended")
        || lower.contains("type='ended'")
        || lower.contains("type=\"ended\"")
}

fn part_tag_count(content: &str) -> usize {
    let lower = content.to_ascii_lowercase();
    lower.matches("<part ").count() + lower.matches("<part>").count()
}

fn partlist_names(content: &str) -> Vec<String> {
    let mut names = Vec::new();
    let lower = content.to_ascii_lowercase();
    let mut search_from = 0;
    while names.len() < 8 {
        let Some(relative) = lower[search_from..].find("<name>") else { break };
        let start = search_from + relative + "<name>".len();
        let Some(end_relative) = lower[start..].find("</name>") else { break };
        let raw = content.get(start..start + end_relative).unwrap_or("");
        let name = raw.trim();
        if !name.is_empty() && name.len() <= 80 && !names.iter().any(|have: &String| have == name) {
            names.push(name.to_owned());
        }
        search_from = start + end_relative + "</name>".len();
    }
    names
}

/// A typed call-end marker on the same conversation. A partlist that is still
/// open is not an end.
pub fn meeting_ended_threads(payload_json: &str) -> Vec<String> {
    if payload_json.len() > 1024 * 1024 { return Vec::new(); }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload_json) else { return Vec::new(); };
    let mut threads = Vec::new();
    let mut visited = 0;
    visit_meeting_ended(&value, 0, &mut visited, &mut threads);
    threads
}

fn visit_meeting_ended(value: &serde_json::Value, depth: usize, visited: &mut usize, threads: &mut Vec<String>) {
    if depth > MAX_CHAT_EVENT_DEPTH || *visited >= MAX_CHAT_EVENT_VISITED_NODES
        || threads.len() >= MAX_CHAT_EVENT_CONVERSATIONS { return; }
    *visited += 1;
    match value {
        serde_json::Value::Object(object) => {
            if event_type(object).is_some_and(|kind| kind.eq_ignore_ascii_case("NewMessage")) {
                if let Some(message) = owned_message_resource(value) {
                    let ended = object_string(&message, "messagetype") == Some("Event/Call")
                        && object_string(&message, "content").is_some_and(call_content_ended);
                    if ended {
                        if let Some(conversation_id) = typing_conversation_id(&message) {
                            if !threads.contains(&conversation_id) { threads.push(conversation_id); }
                        }
                    }
                }
            }
            for child in object.values() { visit_meeting_ended(child, depth + 1, visited, threads); }
        }
        serde_json::Value::Array(items) => for child in items { visit_meeting_ended(child, depth + 1, visited, threads); },
        serde_json::Value::String(encoded) => {
            if let Ok(decoded) = serde_json::from_str::<serde_json::Value>(encoded) {
                visit_meeting_ended(&decoded, depth + 1, visited, threads);
            }
        }
        _ => {}
    }
}

/// The original meeting-start publisher examines NewMessage resources with
/// Event/Call and an opening partlist. Ordinary chat text is never sufficient.
pub fn meeting_started_signals(payload_json: &str) -> Vec<MeetingStartedSignal> {
    if payload_json.len() > 1024 * 1024 { return Vec::new(); }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload_json) else { return Vec::new(); };
    let mut signals = Vec::new();
    let mut visited = 0;
    visit_meeting_started(&value, 0, &mut visited, &mut signals);
    signals
}

fn visit_meeting_started(value: &serde_json::Value, depth: usize, visited: &mut usize, signals: &mut Vec<MeetingStartedSignal>) {
    if depth > MAX_CHAT_EVENT_DEPTH || *visited >= MAX_CHAT_EVENT_VISITED_NODES
        || signals.len() >= MAX_CHAT_EVENT_CONVERSATIONS { return; }
    *visited += 1;
    match value {
        serde_json::Value::Object(object) => {
            if event_type(object).is_some_and(|kind| kind.eq_ignore_ascii_case("NewMessage")) {
                if let Some(message) = owned_message_resource(value) {
                    let content = object_string(&message, "content");
                    let is_start = object_string(&message, "messagetype") == Some("Event/Call")
                        && content.is_some_and(|content| content.starts_with("<partlist") && !call_content_ended(content));
                    if is_start {
                        if let (Some(conversation_id), Some(message_id)) = (
                            typing_conversation_id(&message),
                            object_string(&message, "id").filter(|id| valid_conversation_id(id)),
                        ) {
                            let timestamp = object_string(&message, "composetime")
                                .or_else(|| object_string(&message, "originalarrivaltime"))
                                .filter(|value| value.len() <= 128).map(str::to_owned);
                            let participants = content.map(partlist_names).unwrap_or_default();
                            let participant_count = participants.len().max(content.map(part_tag_count).unwrap_or(0));
                            let signal = MeetingStartedSignal {
                                conversation_id, message_id: message_id.to_owned(), timestamp, participants, participant_count,
                            };
                            if !signals.contains(&signal) { signals.push(signal); }
                        }
                    }
                }
            }
            for child in object.values() { visit_meeting_started(child, depth + 1, visited, signals); }
        }
        serde_json::Value::Array(items) => for child in items { visit_meeting_started(child, depth + 1, visited, signals); },
        serde_json::Value::String(encoded) => {
            if let Ok(decoded) = serde_json::from_str::<serde_json::Value>(encoded) {
                visit_meeting_started(&decoded, depth + 1, visited, signals);
            }
        }
        _ => {}
    }
}

/// Extract target conversations from known chat push envelopes. Unknown
/// message payloads fall back to refreshing the selected chat; event payloads
/// never mutate message content directly.
pub fn chat_event_impact(payload_json: &str) -> ChatEventImpact {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload_json) else {
        return ChatEventImpact {
            has_unscoped_message_change: true,
            ..ChatEventImpact::default()
        };
    };

    let mut impact = ChatEventImpact::default();
    let mut visited = 0;
    visit_chat_event(&value, 0, &mut visited, &mut impact);
    impact
}

/// Returns true only when every typed resource in this payload is a recognized
/// NewMessage Control/Typing resource. Unknown or mixed payloads retain the
/// normal chat reconciliation path.
pub fn chat_event_is_typing_only(payload_json: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload_json) else {
        return false;
    };
    let mut event_count = 0;
    let mut typing_only = true;
    let mut visited = 0;
    visit_typing_only_events(&value, 0, &mut visited, &mut event_count, &mut typing_only);
    event_count > 0 && typing_only
}

/// Finds conversation IDs only in the original client's typed thread updates.
/// Member objects are deliberately not applied from push data; these IDs only
/// invalidate the open roster so the native ChatService remains authoritative.
pub fn group_roster_event_conversations(payload_json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload_json) else {
        return Vec::new();
    };
    let mut conversations = Vec::new();
    let mut visited = 0;
    visit_group_roster_events(&value, 0, &mut visited, &mut conversations);
    conversations
}

fn visit_group_roster_events(
    value: &serde_json::Value,
    depth: usize,
    visited: &mut usize,
    conversations: &mut Vec<String>,
) {
    if depth > MAX_CHAT_EVENT_DEPTH
        || *visited >= MAX_CHAT_EVENT_VISITED_NODES
        || conversations.len() >= MAX_CHAT_EVENT_CONVERSATIONS
    {
        return;
    }
    *visited += 1;
    match value {
        serde_json::Value::Object(object) => {
            if event_type(object).is_some_and(is_group_roster_event_type) {
                if let Some(id) = group_roster_event_conversation_id(value) {
                    if !conversations.contains(&id) {
                        conversations.push(id);
                    }
                }
            }
            for child in object.values() {
                visit_group_roster_events(child, depth + 1, visited, conversations);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                visit_group_roster_events(child, depth + 1, visited, conversations);
            }
        }
        serde_json::Value::String(encoded) => {
            if let Ok(decoded) = serde_json::from_str::<serde_json::Value>(encoded) {
                visit_group_roster_events(&decoded, depth + 1, visited, conversations);
            }
        }
        _ => {}
    }
}

fn is_group_roster_event_type(event_type: &str) -> bool {
    matches!(
        event_type.to_ascii_lowercase().as_str(),
        "threadupdate" | "conversationfullupdate"
    )
}

fn group_roster_event_conversation_id(event: &serde_json::Value) -> Option<String> {
    fn resource_conversation_id(resource: &serde_json::Value, depth: usize) -> Option<String> {
        if depth > 4 {
            return None;
        }
        if let Some(encoded) = resource.as_str() {
            let decoded = serde_json::from_str::<serde_json::Value>(encoded).ok()?;
            return resource_conversation_id(&decoded, depth + 1);
        }
        let object = resource.as_object()?;
        if let Some(conversation) = object
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("conversation"))
            .map(|(_, value)| value)
        {
            if let Some(encoded) = conversation.as_str() {
                let decoded = serde_json::from_str::<serde_json::Value>(encoded).ok()?;
                if let Some(id) = object_string(&decoded, "id")
                    .filter(|id| valid_conversation_id(id))
                {
                    return Some(id.to_owned());
                }
                return resource_conversation_id(&decoded, depth + 1);
            }
            if let Some(id) = object_string(conversation, "id")
                .filter(|id| valid_conversation_id(id))
            {
                return Some(id.to_owned());
            }
        }
        conversation_id(resource)
    }

    let object = event.as_object()?;
    let resource = object
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("resource"))
        .map(|(_, value)| value)
        .unwrap_or(event);
    resource_conversation_id(resource, 0)
}

fn visit_typing_only_events(
    value: &serde_json::Value,
    depth: usize,
    visited: &mut usize,
    event_count: &mut usize,
    typing_only: &mut bool,
) {
    if depth > MAX_CHAT_EVENT_DEPTH || *visited >= MAX_CHAT_EVENT_VISITED_NODES {
        *typing_only = false;
        return;
    }
    *visited += 1;
    match value {
        serde_json::Value::Object(object) => {
            if event_type(object).is_some() {
                *event_count += 1;
                if !is_typing_event(value) {
                    *typing_only = false;
                }
            }
            for child in object.values() {
                visit_typing_only_events(child, depth + 1, visited, event_count, typing_only);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                visit_typing_only_events(child, depth + 1, visited, event_count, typing_only);
            }
        }
        serde_json::Value::String(encoded) => {
            if let Ok(decoded) = serde_json::from_str::<serde_json::Value>(encoded) {
                visit_typing_only_events(&decoded, depth + 1, visited, event_count, typing_only);
            }
        }
        _ => {}
    }
}

fn is_typing_event(event: &serde_json::Value) -> bool {
    let Some(kind) = event.as_object().and_then(event_type) else {
        return false;
    };
    if !kind.eq_ignore_ascii_case("NewMessage") {
        return false;
    }
    owned_message_resource(event)
        .as_ref()
        .and_then(|message| object_string(message, "messagetype"))
        .is_some_and(|message_type| message_type.to_ascii_lowercase().contains("typing"))
}

fn visit_chat_event(
    value: &serde_json::Value,
    depth: usize,
    visited: &mut usize,
    impact: &mut ChatEventImpact,
) {
    if depth > MAX_CHAT_EVENT_DEPTH || *visited >= MAX_CHAT_EVENT_VISITED_NODES {
        impact.has_unscoped_message_change = true;
        return;
    }
    *visited += 1;

    match value {
        serde_json::Value::Object(object) => {
            if let Some(kind) = event_type(object).filter(|kind| is_message_change_type(kind))
                && !is_typing_event(value)
            {
                impact.has_message_change = true;
                let target = kind.eq_ignore_ascii_case("MessageUpdate")
                    .then(|| message_update_target(value))
                    .flatten();
                let conversation = target.as_ref().map(|(chat, _)| chat.clone())
                    .or_else(|| conversation_id(value));
                if let Some(target) = target
                    && impact.message_updates.len() < MAX_CHAT_EVENT_CONVERSATIONS
                    && !impact.message_updates.contains(&target)
                {
                    impact.message_updates.push(target);
                }
                match conversation {
                    Some(id) if !impact.conversation_ids.contains(&id) => {
                        if impact.conversation_ids.len() < MAX_CHAT_EVENT_CONVERSATIONS {
                            impact.conversation_ids.push(id);
                        } else {
                            impact.has_unscoped_message_change = true;
                        }
                    }
                    Some(_) => {}
                    None => impact.has_unscoped_message_change = true,
                }
            }
            for child in object.values() {
                visit_chat_event(child, depth + 1, visited, impact);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                visit_chat_event(child, depth + 1, visited, impact);
            }
        }
        serde_json::Value::String(encoded) => {
            if let Ok(decoded) = serde_json::from_str::<serde_json::Value>(encoded) {
                visit_chat_event(&decoded, depth + 1, visited, impact);
            }
        }
        _ => {}
    }
}

fn message_update_target(event: &serde_json::Value) -> Option<(String, String)> {
    let resource = event.get("resource").unwrap_or(event);
    let decoded;
    let resource = if let Some(encoded) = resource.as_str() {
        decoded = serde_json::from_str(encoded).ok()?;
        &decoded
    } else {
        resource
    };
    let message = resource.get("message").unwrap_or(resource);
    let chat = typing_conversation_id(message)
        .or_else(|| typing_conversation_id(resource))
        .or_else(|| conversation_id(event))?;
    let id = object_string(message, "id")
        .or_else(|| object_string(message, "messageId"))
        .filter(|id| valid_conversation_id(id) && !id.starts_with("local:"))?;
    Some((chat, id.to_owned()))
}

fn event_type(object: &serde_json::Map<String, serde_json::Value>) -> Option<&str> {
    object.iter().find_map(|(key, value)| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "resourcetype" | "__typename" | "eventtype"
        )
        .then(|| value.as_str())
        .flatten()
    })
}

fn is_message_change_type(event_type: &str) -> bool {
    matches!(
        event_type.to_ascii_lowercase().as_str(),
        "newmessage"
            | "messageupdate"
            | "partialmessageupdate"
            | "newlivemessages"
            | "livemessageupdates"
    )
}

fn conversation_id(value: &serde_json::Value) -> Option<String> {
    fn search(value: &serde_json::Value, depth: usize) -> Option<String> {
        if depth > 6 {
            return None;
        }
        let object = value.as_object()?;
        for (key, value) in object {
            if matches!(
                key.to_ascii_lowercase().as_str(),
                "convid" | "conversationid" | "chatid"
            ) {
                if let Some(id) = value.as_str().filter(|id| valid_conversation_id(id)) {
                    return Some(id.to_owned());
                }
            }
        }
        for (key, value) in object {
            if matches!(
                key.to_ascii_lowercase().as_str(),
                "conversation" | "resource" | "data" | "body" | "event"
            ) {
                if let Some(id) = search(value, depth + 1) {
                    return Some(id);
                }
            }
        }
        None
    }

    search(value, 0)
}

fn valid_conversation_id(id: &str) -> bool {
    !id.trim().is_empty()
        && id.len() <= MAX_CHAT_CONVERSATION_ID_CHARS
        && !id.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTypingUpdate {
    pub conversation_id: String,
    pub sender_mri: String,
    /// Name carried by the typing frame (`imdisplayname`). Absent when the
    /// frame only identified the sender by MRI.
    pub display_name: Option<String>,
    pub active: bool,
    pub expires_in: Option<Duration>,
}

/// Reads only NewMessage resources with the fields used by the original
/// client's static typing resolver. This is advisory UI state: message bodies
/// are never copied from push payloads into chat history.
pub fn chat_typing_updates(payload_json: &str) -> Vec<ChatTypingUpdate> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload_json) else {
        return Vec::new();
    };
    let mut updates = Vec::new();
    let mut visited = 0;
    visit_typing_events(&value, 0, &mut visited, &mut updates);
    updates
}

fn visit_typing_events(
    value: &serde_json::Value,
    depth: usize,
    visited: &mut usize,
    updates: &mut Vec<ChatTypingUpdate>,
) {
    if depth > MAX_CHAT_EVENT_DEPTH
        || *visited >= MAX_CHAT_EVENT_VISITED_NODES
        || updates.len() >= MAX_CHAT_EVENT_CONVERSATIONS
    {
        return;
    }
    *visited += 1;

    match value {
        serde_json::Value::Object(object) => {
            if event_type(object).is_some_and(|kind| kind.eq_ignore_ascii_case("NewMessage")) {
                if let Some(update) = typing_update_from_event(value) {
                    updates.push(update);
                }
            }
            for child in object.values() {
                visit_typing_events(child, depth + 1, visited, updates);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                visit_typing_events(child, depth + 1, visited, updates);
            }
        }
        serde_json::Value::String(encoded) => {
            if let Ok(decoded) = serde_json::from_str::<serde_json::Value>(encoded) {
                visit_typing_events(&decoded, depth + 1, visited, updates);
            }
        }
        _ => {}
    }
}

fn typing_update_from_event(event: &serde_json::Value) -> Option<ChatTypingUpdate> {
    let message = owned_message_resource(event)?;
    let message_type = object_string(&message, "messagetype")?;
    let conversation_id =
        typing_conversation_id(&message).or_else(|| conversation_id(event))?;
    let sender_mri = typing_sender(&message)?;
    let active = message_type.to_ascii_lowercase().contains("typing");
    let expires_in = if active {
        match object_string(&message, "originalarrivaltime").and_then(parse_arrival) {
            Some(arrived) => {
                let deadline = arrived + chrono::Duration::from_std(TYPING_INDICATOR_TTL).ok()?;
                match (deadline - chrono::Utc::now()).to_std() {
                    Ok(remaining) if !remaining.is_zero() => {
                        Some(remaining.min(TYPING_INDICATOR_TTL))
                    }
                    _ => None,
                }
            }
            None => Some(Duration::from_secs(8)),
        }
    } else {
        None
    };

    Some(ChatTypingUpdate {
        conversation_id,
        sender_mri,
        display_name: typing_display_name(&message),
        active,
        expires_in,
    })
}

fn typing_display_name(message: &serde_json::Value) -> Option<String> {
    for key in ["imdisplayname", "displayname"] {
        if let Some(name) = object_string(message, key).and_then(clean_typing_name) {
            return Some(name);
        }
    }
    let from = message
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("from"))
        .map(|(_, value)| value)?;
    party_display_name(from, 0)
}

fn party_display_name(value: &serde_json::Value, depth: usize) -> Option<String> {
    if depth > 3 {
        return None;
    }
    let object = value.as_object()?;
    for key in ["imdisplayname", "displayname", "name"] {
        if let Some(name) = object
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
            .and_then(|(_, value)| value.as_str())
            .and_then(clean_typing_name)
        {
            return Some(name);
        }
    }
    for nested in ["user", "application"] {
        if let Some(child) = object
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(nested))
            .map(|(_, value)| value)
            && let Some(name) = party_display_name(child, depth + 1)
        {
            return Some(name);
        }
    }
    None
}

fn clean_typing_name(value: &str) -> Option<String> {
    let name: String = value.trim().chars().take(80).collect();
    let name = name.trim();
    if name.is_empty()
        || name.chars().any(char::is_control)
        || name.starts_with("http://")
        || name.starts_with("https://")
        || name.contains("8:orgid:")
        || crate::avatars::person_id(name).is_some()
        || name == "?"
    {
        return None;
    }
    Some(name.to_owned())
}

fn owned_message_resource(event: &serde_json::Value) -> Option<serde_json::Value> {
    let object = event.as_object()?;
    if let Some(resource) = object
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("resource"))
        .map(|(_, value)| value)
    {
        if let Some(encoded) = resource.as_str() {
            let parsed = serde_json::from_str::<serde_json::Value>(encoded).ok()?;
            return find_message_resource(&parsed, 0)
                .cloned()
                .or(Some(parsed));
        }
        if let Some(found) = find_message_resource(resource, 0) {
            return Some(found.clone());
        }
    }
    object_string(event, "messagetype").map(|_| event.clone())
}

/// Chat-service typing frames identify the thread by `threadid` or by the
/// conversation URL, not by `conversationid`.
fn typing_conversation_id(message: &serde_json::Value) -> Option<String> {
    for key in ["threadid", "conversationid", "convid", "chatid"] {
        if let Some(id) = object_string(message, key) {
            let id = decode_conversation_id(id);
            if valid_conversation_id(&id) {
                return Some(id);
            }
        }
    }
    for key in ["conversationlink", "resourcelink"] {
        if let Some(link) = object_string(message, key) {
            if let Some(id) = conversation_id_from_link(link) {
                return Some(id);
            }
        }
    }
    None
}

fn conversation_id_from_link(link: &str) -> Option<String> {
    let marker = "/conversations/";
    let start = link
        .as_bytes()
        .windows(marker.len())
        .position(|window| window.eq_ignore_ascii_case(marker.as_bytes()))?
        + marker.len();
    let rest = link.get(start..)?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let id = decode_conversation_id(rest[..end].trim());
    valid_conversation_id(&id).then_some(id)
}

fn decode_conversation_id(id: &str) -> String {
    urlencoding::decode(id.trim())
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| id.trim().to_owned())
}

fn typing_sender(message: &serde_json::Value) -> Option<String> {
    let from = message
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("from"))
        .map(|(_, value)| value)?;
    match from {
        serde_json::Value::String(value) => sender_mri(value),
        serde_json::Value::Object(object) => {
            for key in ["mri", "id"] {
                if let Some(value) = object
                    .iter()
                    .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
                    .and_then(|(_, value)| value.as_str())
                {
                    if let Some(mri) = sender_mri(value) {
                        return Some(mri);
                    }
                }
            }
            for nested in ["user", "application"] {
                let Some(child) = object
                    .iter()
                    .find(|(candidate, _)| candidate.eq_ignore_ascii_case(nested))
                    .and_then(|(_, value)| value.as_object())
                else {
                    continue;
                };
                for key in ["mri", "id"] {
                    if let Some(value) = child
                        .iter()
                        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
                        .and_then(|(_, value)| value.as_str())
                    {
                        if let Some(mri) = sender_mri(value) {
                            return Some(mri);
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn parse_arrival(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(parsed.with_timezone(&chrono::Utc));
    }
    let (head, fraction) = value.split_once('.')?;
    let split = fraction
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(fraction.len());
    let (digits, tail) = fraction.split_at(split);
    let digits: String = digits.chars().take(6).collect();
    let normalized = format!("{head}.{digits}{tail}");
    chrono::DateTime::parse_from_rfc3339(&normalized)
        .ok()
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
}

fn find_message_resource(value: &serde_json::Value, depth: usize) -> Option<&serde_json::Value> {
    if depth > 6 {
        return None;
    }
    let object = value.as_object()?;
    if object
        .keys()
        .any(|key| key.eq_ignore_ascii_case("messagetype"))
    {
        return Some(value);
    }
    for (key, child) in object {
        if matches!(
            key.to_ascii_lowercase().as_str(),
            "message" | "data" | "body"
        ) {
            if let Some(found) = find_message_resource(child, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

fn object_string<'a>(object: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    object
        .as_object()?
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
        .and_then(|(_, value)| value.as_str())
}

fn sender_mri(value: &str) -> Option<String> {
    let segment = value
        .trim()
        .split(['?', '#'])
        .next()?
        .rsplit('/')
        .next()?
        .trim();
    let decoded = urlencoding::decode(segment).ok()?;
    let segment = decoded.trim();
    let oid = segment.strip_prefix("8:orgid:").unwrap_or(segment);
    crate::avatars::person_id(oid).map(|_| format!("8:orgid:{oid}"))
}

#[derive(Debug, Clone)]
struct ActiveTypingIndicator {
    conversation_id: String,
    person_id: String,
    display_name: Option<String>,
    expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypingSender {
    pub person_id: String,
    pub display_name: Option<String>,
}

/// Bounded, session-local typing state. The person id and the frame's display
/// name stay in memory so the open chat can show who is typing.
#[derive(Debug, Default)]
pub struct ChatTypingIndicators {
    entries: VecDeque<ActiveTypingIndicator>,
}

impl ChatTypingIndicators {
    pub fn apply(&mut self, update: ChatTypingUpdate, now: Instant) -> bool {
        self.prune_expired(now);
        if !valid_conversation_id(&update.conversation_id) {
            return false;
        }
        let Some(oid) = update.sender_mri.strip_prefix("8:orgid:") else {
            return false;
        };
        let Some(person_id) = crate::avatars::person_id(oid) else {
            return false;
        };
        if !update.active {
            self.entries.retain(|entry| {
                entry.conversation_id != update.conversation_id || entry.person_id != person_id
            });
            return true;
        }
        let Some(ttl) = update.expires_in.filter(|ttl| !ttl.is_zero()) else {
            return false;
        };
        let new_expiry = now + ttl.min(TYPING_INDICATOR_TTL);
        if let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.conversation_id == update.conversation_id && entry.person_id == person_id
        }) {
            if new_expiry > entry.expires_at {
                entry.expires_at = new_expiry;
            }
            if update.display_name.is_some() {
                entry.display_name = update.display_name;
            }
            return true;
        }
        while self.entries.len() >= MAX_TYPING_INDICATORS {
            self.entries.pop_front();
        }
        self.entries.push_back(ActiveTypingIndicator {
            conversation_id: update.conversation_id,
            person_id,
            display_name: update.display_name,
            expires_at: new_expiry,
        });
        true
    }

    /// Newest typers first, at most three. Names come only from the frame.
    pub fn senders(&self, conversation_id: &str, now: Instant) -> Vec<TypingSender> {
        let mut found = Vec::new();
        for entry in self.entries.iter().rev() {
            if entry.conversation_id == conversation_id && entry.expires_at > now {
                found.push(TypingSender {
                    person_id: entry.person_id.clone(),
                    display_name: entry.display_name.clone(),
                });
                if found.len() == 3 {
                    break;
                }
            }
        }
        found
    }

    pub fn has_typing_users(&self, conversation_id: &str, now: Instant) -> bool {
        self.entries.iter().any(|entry| {
            entry.conversation_id == conversation_id && entry.expires_at > now
        })
    }

    pub fn any_active(&self, now: Instant) -> bool {
        self.entries.iter().any(|entry| entry.expires_at > now)
    }

    pub fn prune_expired(&mut self, now: Instant) {
        self.entries.retain(|entry| entry.expires_at > now);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Debug, Default)]
pub struct RecentChatEventFingerprints {
    ordered: VecDeque<String>,
    seen: HashSet<String>,
}

#[derive(Debug, Clone)]
struct RecentMessageEdit {
    conversation_id: String,
    message_id: String,
    body: String,
    document: Option<std::sync::Arc<crate::message_body::Document>>,
    recorded_at: Instant,
}

/// Holds a short-lived, bounded overlay after a successful local edit so an
/// older history response cannot immediately restore the previous body.
#[derive(Debug, Default)]
pub struct RecentMessageEdits {
    entries: VecDeque<RecentMessageEdit>,
}

impl RecentMessageEdits {
    pub fn remember_rich(&mut self,conversation_id:&str,message_id:&str,body:&str,document:Option<std::sync::Arc<crate::message_body::Document>>)->bool {
        if document.as_ref().is_some_and(|doc|doc.estimated_heap_bytes()>64*1024) {return false;}
        if !self.remember(conversation_id,message_id,body) {return false;}
        if let Some(entry)=self.entries.back_mut() {entry.document=document;}
        true
    }
    pub fn remember(&mut self, conversation_id: &str, message_id: &str, body: &str) -> bool {
        self.remember_at(conversation_id, message_id, body, Instant::now())
    }

    fn remember_at(
        &mut self,
        conversation_id: &str,
        message_id: &str,
        body: &str,
        now: Instant,
    ) -> bool {
        self.prune_expired(now);
        if conversation_id.is_empty()
            || message_id.is_empty()
            || body.len() > MAX_RECENT_MESSAGE_EDIT_BODY_BYTES
        {
            return false;
        }

        self.entries.retain(|entry| {
            entry.conversation_id != conversation_id || entry.message_id != message_id
        });
        self.entries.push_back(RecentMessageEdit {
            conversation_id: conversation_id.to_owned(),
            message_id: message_id.to_owned(),
            body: body.to_owned(),
            document: None,
            recorded_at: now,
        });
        while self.entries.len() > MAX_RECENT_MESSAGE_EDITS {
            self.entries.pop_front();
        }
        true
    }

    /// Applies overlays only to the matching conversation and message. A
    /// server echo confirms and removes the overlay; a remote deletion also
    /// wins immediately. Unconfirmed edits expire after a minute.
    pub fn reconcile_page(&mut self, conversation_id: &str, messages: &mut [GraphMessage]) {
        self.reconcile_page_at(conversation_id, messages, Instant::now());
    }

    fn reconcile_page_at(
        &mut self,
        conversation_id: &str,
        messages: &mut [GraphMessage],
        now: Instant,
    ) {
        self.prune_expired(now);
        let mut confirmed = HashSet::new();
        for message in messages {
            let Some(index) = self.entries.iter().rposition(|entry| {
                entry.conversation_id == conversation_id && entry.message_id == message.id
            }) else {
                continue;
            };
            let entry = &self.entries[index];
            if message.is_deleted || !message.is_mine || (message.body == entry.body && (entry.document.is_none() || message.rich==entry.document)) {
                confirmed.insert((entry.conversation_id.clone(), entry.message_id.clone()));
            } else {
                message.body.clone_from(&entry.body);
                message.rich.clone_from(&entry.document);
            }
        }
        self.entries.retain(|entry| {
            !confirmed.contains(&(entry.conversation_id.clone(), entry.message_id.clone()))
        });
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn prune_expired(&mut self, now: Instant) {
        self.entries.retain(|entry| {
            now.saturating_duration_since(entry.recorded_at) < RECENT_MESSAGE_EDIT_TTL
        });
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn remember_at_for_test(
        &mut self,
        conversation_id: &str,
        message_id: &str,
        body: &str,
        now: Instant,
    ) -> bool {
        self.remember_at(conversation_id, message_id, body, now)
    }

    #[cfg(test)]
    fn reconcile_page_at_for_test(
        &mut self,
        conversation_id: &str,
        messages: &mut [GraphMessage],
        now: Instant,
    ) {
        self.reconcile_page_at(conversation_id, messages, now);
    }
}

impl RecentChatEventFingerprints {
    /// Returns false for an identical event receipt already handled in this
    /// session. Retains only fixed-size digests, never message payloads.
    pub fn remember(&mut self, payload_json: &str) -> bool {
        let canonical = serde_json::from_str::<serde_json::Value>(payload_json)
            .map(|value| value.to_string())
            .unwrap_or_else(|_| payload_json.to_owned());
        let digest = format!("{:x}", sha2::Sha256::digest(canonical.as_bytes()));
        if !self.seen.insert(digest.clone()) {
            return false;
        }
        self.ordered.push_back(digest);
        while self.ordered.len() > MAX_RECENT_CHAT_EVENT_FINGERPRINTS {
            if let Some(expired) = self.ordered.pop_front() {
                self.seen.remove(&expired);
            }
        }
        true
    }

    pub fn clear(&mut self) {
        self.ordered.clear();
        self.seen.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.ordered.len()
    }
}

/// Merge history/refresh pages and reconcile server copies of local sends.
fn preserve_sender_name(message: &mut GraphMessage, previous: &GraphMessage) {
    if crate::sender_names::known(&message.author).is_none()
        && crate::sender_names::known(&previous.author).is_some()
        && message.author_id.as_deref().zip(previous.author_id.as_deref())
            .is_some_and(|(incoming, current)| crate::reactions::identity(incoming) == crate::reactions::identity(current))
    {
        message.author = previous.author.clone();
    }
}

pub fn merge_messages(current: &mut Vec<GraphMessage>, mut incoming: Vec<GraphMessage>) -> bool {
    // A normal refresh repeats the latest page. Avoid copying and sorting the
    // entire loaded history when that page has no changes. Noncanonical input
    // retains the full reconciliation path below.
    let canonical = current.iter().all(|m| !m.id.is_empty())
        && current.windows(2).all(|pair| {
            (&pair[0].timestamp, &pair[0].id) <= (&pair[1].timestamp, &pair[1].id)
        });
    if canonical {
        let by_id: HashMap<&str, &GraphMessage> = current.iter().map(|m| (m.id.as_str(), m)).collect();
        for message in &mut incoming {
            if let Some(previous) = by_id.get(message.id.as_str()) {
                preserve_sender_name(message, previous);
            }
        }
        let clients: HashSet<&str> = current.iter().filter_map(|m| m.client_message_id.as_deref()).collect();
        let client_count = current.iter().filter(|m| m.client_message_id.is_some()).count();
        if by_id.len() == current.len() && clients.len() == client_count
            && incoming.iter().all(|m| by_id.get(m.id.as_str()).is_some_and(|old| **old == *m))
        {
            return false;
        }
    }
    let old = current.clone();
    let mut merged: Vec<GraphMessage> =
        Vec::with_capacity(current.len() + incoming.len());
    let mut ids = HashMap::<String, usize>::new();
    let mut client_ids = HashMap::<String, usize>::new();
    for mut message in current.drain(..).chain(incoming) {
        let existing = (!message.id.is_empty())
            .then(|| ids.get(&message.id).copied())
            .flatten()
            .or_else(|| {
                message
                    .client_message_id
                    .as_ref()
                    .and_then(|id| client_ids.get(id).copied())
            });
        let index = existing.unwrap_or(merged.len());
        if let Some(previous) = merged.get(index) {
            preserve_sender_name(&mut message, previous);
        }
        if merged.get(index).is_some_and(|previous| previous.is_deleted) {
            message.is_deleted = true;
            message.body.clear();
            message.rich = None;
            message.images.clear();
            message.files.clear();
            message.forwarded = None;
            message.reactions.clear();
        }
        if !message.id.is_empty() {
            ids.insert(message.id.clone(), index);
        }
        if let Some(id) = &message.client_message_id {
            client_ids.insert(id.clone(), index);
        }
        if index == merged.len() {
            merged.push(message);
        } else {
            merged[index] = message;
        }
    }
    merged.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then(a.id.cmp(&b.id)));
    let changed = merged != old;
    *current = merged;
    changed
}

#[derive(Clone)]
pub struct SendAttempt {
    pub conversation: String,
    pub content: String,
    pub fingerprint: String,
    pub client_id: String,
}

impl SendAttempt {
    pub fn prepare(previous: Option<&Self>, conversation: String, content: String) -> Self {
        Self::prepare_with_fingerprint(previous, conversation, content.clone(), content)
    }

    pub fn prepare_with_fingerprint(
        previous: Option<&Self>,
        conversation: String,
        content: String,
        fingerprint: String,
    ) -> Self {
        if let Some(previous) =
            previous.filter(|p| p.conversation == conversation && p.fingerprint == fingerprint)
        {
            return previous.clone();
        }
        Self {
            conversation,
            content,
            fingerprint,
            client_id: new_client_message_id(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(id: &str, timestamp: &str, body: &str, client: Option<&str>) -> GraphMessage {
        GraphMessage {
            id: id.into(),
            timestamp: timestamp.into(),
            body: body.into(),
            client_message_id: client.map(str::to_owned),
            ..Default::default()
        }
    }
    #[test]
    fn refresh_missing_sender_name_preserves_known_name_only_for_the_same_identity() {
        let peer="aaaaaaaa-1111-2222-3333-444444444444";
        let original=GraphMessage{id:"message-1".into(),timestamp:"2026-10-29T12:00:00Z".into(),author:"Ana Paula".into(),author_id:Some(peer.into()),body:"Antes".into(),..Default::default()};
        let mut messages=vec![original.clone()];
        let updated=GraphMessage{author:"?".into(),author_id:Some(format!("8:ORGID:{}",peer.to_uppercase())),body:"Editada".into(),..original.clone()};
        assert!(merge_messages(&mut messages,vec![updated]));
        assert_eq!(messages[0].author,"Ana Paula");assert_eq!(messages[0].body,"Editada");
        let pointer = messages.as_ptr();
        let mut unchanged = messages[0].clone();
        unchanged.author = "?".into();
        assert!(!merge_messages(&mut messages, vec![unchanged]));
        assert_eq!(messages.as_ptr(), pointer);
        let other=GraphMessage{author:"?".into(),author_id:Some("bbbbbbbb-1111-2222-3333-444444444444".into()),..original};
        assert!(merge_messages(&mut messages,vec![other]));
        assert_eq!(messages[0].author,"?");
    }

    #[test]
    fn merges_edits_history_and_send_receipts_without_duplicates() {
        let mut messages = vec![message("local:1", "20", "Sent", Some("1"))];
        merge_messages(
            &mut messages,
            vec![
                message("server:1", "21", "Sent", Some("1")),
                message("older", "10", "Old", None),
            ],
        );
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].id, "server:1");
        merge_messages(
            &mut messages,
            vec![message("server:1", "21", "Edited", Some("1"))],
        );
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].body, "Edited");
        assert!(!merge_messages(&mut messages, vec![]));
    }
    #[test]
    fn retries_reuse_id_only_for_the_same_content_and_conversation() {
        let first = SendAttempt::prepare(None, "chat:a".into(), "Hello".into());
        let retry = SendAttempt::prepare(Some(&first), "chat:a".into(), "Hello".into());
        assert_eq!(first.client_id, retry.client_id);
        assert_ne!(
            first.client_id,
            SendAttempt::prepare(Some(&first), "chat:b".into(), "Hello".into()).client_id
        );
        assert_ne!(
            first.client_id,
            SendAttempt::prepare(Some(&first), "chat:a".into(), "Edited".into()).client_id
        );
    }

    #[test]
    fn rich_send_retries_reuse_id_only_when_html_fingerprint_matches() {
        let first = SendAttempt::prepare_with_fingerprint(
            None,
            "chat:a".into(),
            "Hello".into(),
            "<p>Hello</p>".into(),
        );
        let retry = SendAttempt::prepare_with_fingerprint(
            Some(&first),
            "chat:a".into(),
            "Hello".into(),
            "<p>Hello</p>".into(),
        );
        let changed_format = SendAttempt::prepare_with_fingerprint(
            Some(&first),
            "chat:a".into(),
            "Hello".into(),
            "<p><strong>Hello</strong></p>".into(),
        );
        assert_eq!(first.client_id, retry.client_id);
        assert_ne!(first.client_id, changed_format.client_id);
    }

    #[test]
    fn refresh_does_not_restore_content_after_local_soft_delete() {
        let mut deleted = message("server:1", "21", "", None);
        deleted.is_deleted = true;
        let mut messages = vec![deleted];
        merge_messages(
            &mut messages,
            vec![message("server:1", "21", "Old body from stale refresh", None)],
        );
        assert!(messages[0].is_deleted);
        assert!(messages[0].body.is_empty());
    }

    #[test]
    fn recent_edit_overlay_keeps_a_stale_refresh_from_reverting_success() {
        let now = Instant::now();
        let mut edits = RecentMessageEdits::default();
        assert!(edits.remember_at_for_test("chat-1", "message-1", "Edited body", now));

        let mut page = vec![message("message-1", "21", "Old server body", None)];
        page[0].is_mine = true;
        edits.reconcile_page_at_for_test("chat-1", &mut page, now);

        assert_eq!(page[0].body, "Edited body");
        assert_eq!(edits.len(), 1);
    }

    #[test]
    fn authoritative_edit_echo_confirms_and_releases_overlay() {
        let now = Instant::now();
        let mut edits = RecentMessageEdits::default();
        edits.remember_at_for_test("chat-1", "message-1", "Edited body", now);

        let mut page = vec![message("message-1", "21", "Edited body", None)];
        page[0].is_mine = true;
        edits.reconcile_page_at_for_test("chat-1", &mut page, now);

        assert_eq!(page[0].body, "Edited body");
        assert_eq!(edits.len(), 0);
    }

    #[test]
    fn formatted_edit_keeps_its_document_until_the_server_echo_confirms_it() {
        let document=std::sync::Arc::new(crate::message_body::Document::from_html("<p><b>Changed</b></p>").unwrap());
        let mut edits=RecentMessageEdits::default();
        assert!(edits.remember_rich("chat","message","Changed",Some(document.clone())));
        let mut page=vec![GraphMessage{id:"message".into(),body:"Changed".into(),is_mine:true,..Default::default()}];
        edits.reconcile_page("chat",&mut page);
        assert_eq!(page[0].rich,Some(document));
        assert_eq!(edits.len(),1,"same plain text does not confirm a formatting change");
        edits.reconcile_page("chat",&mut page);
        assert_eq!(edits.len(),0);
    }

    #[test]
    fn recent_edit_overlay_is_scoped_bounded_and_expires() {
        let now = Instant::now();
        let mut edits = RecentMessageEdits::default();
        edits.remember_at_for_test("chat-1", "message-1", "Edited body", now);

        let mut other_chat = vec![message("message-1", "21", "Other chat body", None)];
        other_chat[0].is_mine = true;
        edits.reconcile_page_at_for_test("chat-2", &mut other_chat, now);
        assert_eq!(other_chat[0].body, "Other chat body");
        assert_eq!(edits.len(), 1);

        let mut expired = vec![message("message-1", "21", "Server body", None)];
        expired[0].is_mine = true;
        edits.reconcile_page_at_for_test(
            "chat-1",
            &mut expired,
            now + RECENT_MESSAGE_EDIT_TTL + Duration::from_millis(1),
        );
        assert_eq!(expired[0].body, "Server body");
        assert_eq!(edits.len(), 0);

        for index in 0..(MAX_RECENT_MESSAGE_EDITS + 1) {
            edits.remember_at_for_test("chat-1", &format!("m-{index}"), "x", now);
        }
        assert_eq!(edits.len(), MAX_RECENT_MESSAGE_EDITS);
    }

    #[test]
    fn deleted_or_non_owned_server_messages_remove_the_edit_overlay() {
        let now = Instant::now();
        for (deleted, owned) in [(false, false), (true, true)] {
            let mut edits = RecentMessageEdits::default();
            edits.remember_at_for_test("chat-1", "message-1", "Edited body", now);

            let mut page = vec![message("message-1", "21", "Remote body", None)];
            page[0].is_deleted = deleted;
            page[0].is_mine = owned;
            edits.reconcile_page_at_for_test("chat-1", &mut page, now);

            assert_eq!(page[0].body, "Remote body");
            assert_eq!(edits.len(), 0);
        }
    }

    #[test]
    fn chat_push_targets_only_the_affected_conversation() {
        let impact = chat_event_impact(
            r#"{"eventMessages":[{"resourceType":"MessageUpdate","resource":{"convId":"chat-1","message":{"id":"m-1"}}}]}"#,
        );
        assert!(impact.has_message_change);
        assert!(!impact.has_unscoped_message_change);
        assert_eq!(impact.conversation_ids, vec!["chat-1"]);
    }

    #[test]
    fn chat_push_accepts_graphql_shape_and_falls_back_without_conversation_id() {
        let scoped = chat_event_impact(
            r#"{"__typename":"NewMessage","convId":"chat-2","message":{"id":"m-2"}}"#,
        );
        assert_eq!(scoped.conversation_ids, vec!["chat-2"]);
        assert!(!scoped.has_unscoped_message_change);

        let unscoped = chat_event_impact(
            r#"{"resourceType":"MessageUpdate","resource":{"id":"m-3"}}"#,
        );
        assert!(unscoped.has_message_change);
        assert!(unscoped.has_unscoped_message_change);
        assert!(unscoped.conversation_ids.is_empty());
    }

    #[test]
    fn non_message_chat_events_only_refresh_the_directory() {
        let impact = chat_event_impact(
            r#"{"resourceType":"ConversationUpdate","resource":{"id":"chat-4"}}"#,
        );
        assert!(!impact.has_message_change);
        assert!(!impact.has_unscoped_message_change);
        assert!(impact.conversation_ids.is_empty());
    }

    #[test]
    fn chat_event_fingerprints_dedupe_exact_receipts_and_clear_per_session() {
        let mut recent = RecentChatEventFingerprints::default();
        assert!(recent.remember("{\"id\":\"one\"}"));
        assert!(!recent.remember(" { \"id\" : \"one\" } "));
        assert!(recent.remember("{\"id\":\"one\",\"version\":2}"));
        assert_eq!(recent.len(), 2);
        recent.clear();
        assert_eq!(recent.len(), 0);
    }

    #[test]
    fn chat_event_fingerprint_history_has_a_fixed_bound() {
        let mut recent = RecentChatEventFingerprints::default();
        for id in 0..=MAX_RECENT_CHAT_EVENT_FINGERPRINTS {
            assert!(recent.remember(&format!("{{\"id\":{id}}}")));
        }
        assert_eq!(recent.len(), MAX_RECENT_CHAT_EVENT_FINGERPRINTS);
        assert!(recent.remember("{\"id\":0}"));
    }

    #[test]
    fn typing_parser_reads_only_new_message_control_events() {
        let arrived = chrono::Utc::now().to_rfc3339();
        let payload = serde_json::json!({
            "resourceType": "NewMessage",
            "resource": {
                "conversationid": "chat-typing",
                "from": "https://example.invalid/users/8%3Aorgid%3A11111111-1111-1111-1111-111111111111",
                "messagetype": "Control/Typing",
                "originalarrivaltime": arrived
            }
        })
        .to_string();

        let updates = chat_typing_updates(&payload);

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].conversation_id, "chat-typing");
        assert_eq!(
            updates[0].sender_mri,
            "8:orgid:11111111-1111-1111-1111-111111111111"
        );
        assert!(updates[0].active);
        assert!(updates[0]
            .expires_in
            .is_some_and(|ttl| !ttl.is_zero() && ttl <= TYPING_INDICATOR_TTL));
        assert!(chat_event_is_typing_only(&payload));
        let impact = chat_event_impact(&payload);
        assert!(!impact.has_message_change);
        assert!(!impact.has_unscoped_message_change);

        let update = serde_json::json!({
            "resourceType": "MessageUpdate",
            "resource": {
                "conversationid": "chat-typing",
                "from": "8:orgid:11111111-1111-1111-1111-111111111111",
                "messagetype": "Control/Typing"
            }
        })
        .to_string();
        assert!(chat_typing_updates(&update).is_empty());

        let thread = "19:robson@unq.gbl.spaces";
        let linked = serde_json::json!({
            "resourceType": "NewMessage",
            "resource": {
                "messagetype": "Control/Typing",
                "from": "https://example.invalid/v1/users/ME/contacts/8:orgid:11111111-1111-1111-1111-111111111111",
                "conversationLink": format!("https://example.invalid/v1/users/ME/conversations/{thread}"),
                "originalarrivaltime": arrived
            }
        })
        .to_string();
        let linked = chat_typing_updates(&linked);
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].conversation_id, thread);
        assert!(linked[0].active);

        let by_thread = serde_json::json!({
            "resourceType": "NewMessage",
            "resource": {
                "messagetype": "Control/Typing",
                "threadid": thread,
                "from": {"user": {"mri": "8:orgid:11111111-1111-1111-1111-111111111111"}}
            }
        })
        .to_string();
        let by_thread = chat_typing_updates(&by_thread);
        assert_eq!(by_thread.len(), 1);
        assert_eq!(by_thread[0].conversation_id, thread);
        assert!(by_thread[0].expires_in.is_some());
        assert_eq!(by_thread[0].display_name.as_deref(), None);

        let named = serde_json::json!({
            "resourceType": "NewMessage",
            "resource": {
                "messagetype": "Control/Typing",
                "threadid": thread,
                "imdisplayname": " Ana Paula ",
                "from": {"user": {"mri": "8:orgid:11111111-1111-1111-1111-111111111111", "displayname": "8:orgid:11111111-1111-1111-1111-111111111111"}}
            }
        })
        .to_string();
        let named = chat_typing_updates(&named);
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].display_name.as_deref(), Some("Ana Paula"));

        let nested = serde_json::json!({
            "resourceType": "NewMessage",
            "resource": {
                "messagetype": "Control/Typing",
                "threadid": thread,
                "from": {"user": {"mri": "8:orgid:11111111-1111-1111-1111-111111111111", "displayname": "Ana Paula"}}
            }
        })
        .to_string();
        assert_eq!(
            chat_typing_updates(&nested)[0].display_name.as_deref(),
            Some("Ana Paula")
        );

        let encoded = serde_json::json!({
            "resourceType": "NewMessage",
            "resource": serde_json::json!({
                "messagetype": "Control/Typing",
                "threadid": thread,
                "from": "8:orgid:11111111-1111-1111-1111-111111111111",
                "originalarrivaltime": format!("{}.0000000Z", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S"))
            }).to_string()
        })
        .to_string();
        let encoded = chat_typing_updates(&encoded);
        assert_eq!(encoded.len(), 1);
        assert!(encoded[0].active);
        assert_eq!(encoded[0].conversation_id, thread);
        assert!(!chat_event_is_typing_only(&serde_json::json!({
            "eventMessages": [payload, {"resourceType": "ConversationUpdate"}]
        }).to_string()));
    }

    #[test]
    fn typing_state_is_scoped_hashed_bounded_and_expires() {
        let now = Instant::now();
        let mut indicators = ChatTypingIndicators::default();
        let update = |conversation_id: String, sender: String, active: bool| ChatTypingUpdate {
            conversation_id,
            sender_mri: format!("8:orgid:{sender}"),
            display_name: None,
            active,
            expires_in: active.then_some(Duration::from_secs(22)),
        };
        let sender = "11111111-1111-1111-1111-111111111111";

        assert!(indicators.apply(
            update("chat-1".into(), sender.into(), true),
            now
        ));
        assert_eq!(indicators.len(), 1);
        assert!(indicators.has_typing_users("chat-1", now));
        assert!(indicators.any_active(now));
        assert!(!indicators.has_typing_users("chat-2", now));

        assert!(!indicators.apply(
            ChatTypingUpdate {
                conversation_id: "chat-1".into(),
                sender_mri: format!("8:orgid:{sender}"),
                display_name: None,
                active: true,
                expires_in: Some(Duration::ZERO),
            },
            now + Duration::from_secs(10)
        ));
        let mut older_refresh = update("chat-1".into(), sender.into(), true);
        older_refresh.expires_in = Some(Duration::from_secs(5));
        assert!(indicators.apply(older_refresh, now + Duration::from_secs(10)));
        assert!(indicators.has_typing_users(
            "chat-1",
            now + Duration::from_secs(21)
        ));

        assert!(indicators.apply(
            update("chat-1".into(), sender.into(), false),
            now + Duration::from_secs(1)
        ));
        assert_eq!(indicators.len(), 0);

        indicators.apply(update("chat-1".into(), sender.into(), true), now);
        assert!(!indicators.has_typing_users(
            "chat-1",
            now + TYPING_INDICATOR_TTL
        ));
        indicators.prune_expired(now + TYPING_INDICATOR_TTL);
        assert_eq!(indicators.len(), 0);

        for index in 0..=MAX_TYPING_INDICATORS {
            let sender = format!("00000000-0000-0000-0000-{index:012x}");
            indicators.apply(update(format!("chat-{index}"), sender, true), now);
        }
        assert_eq!(indicators.len(), MAX_TYPING_INDICATORS);
    }

    #[test]
    fn roster_invalidation_reads_only_typed_thread_update_conversations() {
        let payload = serde_json::json!({
            "eventMessages": [
                {
                    "resourceType": "ThreadUpdate",
                    "resource": {
                        "conversation": {"id": "group-1"},
                        "members": {"id": "group-1", "members": []}
                    }
                },
                {
                    "__typename": "ConversationFullUpdate",
                    "resource": serde_json::json!({
                        "conversation": {"id": "group-2"},
                        "members": []
                    })
                    .to_string()
                },
                {
                    "resourceType": "ConversationUpdate",
                    "resource": {"conversation": {"id": "not-a-roster-event"}}
                },
                {
                    "resourceType": "ThreadUpdate",
                    "resource": {"conversation": {"id": "group-1"}}
                }
            ]
        })
        .to_string();

        assert_eq!(
            group_roster_event_conversations(&payload),
            vec!["group-1", "group-2"]
        );
        assert!(group_roster_event_conversations("not-json").is_empty());
    }

    #[test]
    fn meeting_started_requires_typed_new_call_message_and_real_thread() {
        let start = serde_json::json!({"resourceType":"NewMessage", "resource": {
            "id":"fixture-message", "messagetype":"Event/Call", "content":"<partlist type='started'/>",
            "conversationLink":"https://chat.example/v1/users/ME/conversations/19%3Ameeting_fixture%40thread.v2"
        }});
        let wrapped = serde_json::json!({"eventMessages":[start.clone()]}).to_string();
        let signals = meeting_started_signals(&wrapped);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].conversation_id, "19:meeting_fixture@thread.v2");
        assert_eq!(signals[0].message_id, "fixture-message");
        assert_eq!(signals[0].participant_count, 0);
        let named = serde_json::json!({"resourceType":"NewMessage","resource":{
            "id":"named","threadid":"19:meeting_fixture@thread.v2","messagetype":"Event/Call",
            "content":"<partlist><part identity='8:orgid:a'><name>Marina Costa</name></part><part identity='8:orgid:b'><name>Paulo Nunes</name></part></partlist>"
        }});
        let named = meeting_started_signals(&named.to_string());
        assert_eq!(named[0].participants, ["Marina Costa", "Paulo Nunes"]);
        assert_eq!(named[0].participant_count, 2);
        let ended = serde_json::json!({"resourceType":"NewMessage","resource":{
            "id":"end","threadid":"19:meeting_fixture@thread.v2","messagetype":"Event/Call",
            "content":"<partlist type='ended'/>"
        }}).to_string();
        assert!(meeting_started_signals(&ended).is_empty());
        assert_eq!(meeting_ended_threads(&ended), vec!["19:meeting_fixture@thread.v2"]);
        for invalid in [
            serde_json::json!({"resourceType":"MessageUpdate","resource":start["resource"]}),
            serde_json::json!({"resourceType":"NewMessage","resource":{"id":"1","threadid":"19:other@thread.v2","messagetype":"Text","content":"<partlist/>"}}),
            serde_json::json!({"resourceType":"NewMessage","resource":{"id":"1","threadid":"19:other@thread.v2","messagetype":"Event/Call","content":"<ended/>"}}),
            serde_json::json!({"resourceType":"NewMessage","resource":{"id":"1","messagetype":"Event/Call","content":"<partlist/>"}}),
        ] {
            assert!(meeting_started_signals(&invalid.to_string()).is_empty());
        }
        assert!(meeting_started_signals("not-json").is_empty());
    }
}
