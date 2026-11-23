//! Native message emotion contract recovered from the pinned Teams MSIX.
//! See docs/research/chat-reaction-transport.md for source offsets and limits.
use crate::graph::{GraphMessage, ReactionCount, Reactor};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const SUPPORTED: &[(&str, &str)] = &[
    ("👍", "like"), ("❤️", "heart"), ("😂", "laugh"),
    ("😮", "surprised"), ("😢", "sad"), ("😠", "angry"),
];

/// Retain opaque server identities for extended/toned reactions. The six legacy
/// UI tokens remain compatible with existing history and pending operations.
pub fn key(token: &str) -> Option<&str> {
    SUPPORTED.iter().find(|(emoji,_)| *emoji == token).map(|(_,key)| *key)
        .or_else(|| crate::reaction_catalog::resolve_key(token).map(|_| token))
}
pub fn token_for_key(key: &str) -> Option<String> {
    let base = key
        .rsplit_once("-tone")
        .filter(|(_, tone)| tone.len() == 1 && tone.as_bytes()[0].is_ascii_digit())
        .map(|(base, _)| base)
        .unwrap_or(key);
    if let Some((emoji, _)) = SUPPORTED.iter().find(|(_, known)| known.eq_ignore_ascii_case(base)) {
        return Some((*emoji).into());
    }
    crate::reaction_catalog::resolve_key(key).map(|_| key.into())
}

/// Project observed native/Graph reaction properties without losing extended
/// keys, tone variants or explicit summary counts. The catalog is the allowlist.
pub fn from_properties(properties: &Value) -> Vec<ReactionCount> {
    fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
        value.as_object()?.iter().find(|(name,_)| name.eq_ignore_ascii_case(key)).map(|(_,value)| value)
    }
    fn array(value: Option<&Value>) -> Option<std::borrow::Cow<'_, [Value]>> {
        match value? {
            Value::Array(values) => Some(std::borrow::Cow::Borrowed(values)),
            Value::String(encoded) if encoded.len() <= 262_144 => serde_json::from_str::<Vec<Value>>(encoded).ok().map(std::borrow::Cow::Owned),
            _ => None,
        }
    }
    fn label(value: &Value, name: &str) -> Option<String> {
        field(value, name)?.as_str().filter(|value| value.len() <= 512).map(str::to_owned)
    }
    fn person(value: &Value, graph: bool) -> Option<Reactor> {
        let value = if graph { let user = field(value,"user")?; field(user,"user").unwrap_or(user) } else { value };
        let id = label(value,"mri").or_else(|| label(value,"id")).unwrap_or_default();
        let name = label(value,"displayName").unwrap_or_default();
        (!id.is_empty() || !name.is_empty()).then_some(Reactor { id, name })
    }
    let mut output: Vec<ReactionCount> = Vec::new();
    let mut add = |key: &str, count: usize, people: Vec<Reactor>| {
        if count == 0 { return; }
        let Some(token) = token_for_key(key) else { return; };
        if let Some(existing) = output.iter_mut().find(|reaction| reaction.emoji == token) {
            existing.count = existing.count.saturating_add(count);
            for person in people {
                if !existing.reactors.iter().any(|old| !person.id.is_empty() && identity(&old.id) == identity(&person.id)) { existing.reactors.push(person); }
            }
        } else { output.push(ReactionCount { emoji: token, count, reactors: people }); }
    };
    if let Some(graph) = array(field(properties,"reactions")).filter(|items| !items.is_empty()) {
        for entry in graph.iter().take(128) {
            if let Some(key) = field(entry,"reactionType").and_then(Value::as_str) { add(key, 1, person(entry,true).into_iter().collect()); }
        }
        return output;
    }
    let emotions = array(field(properties,"emotions")).unwrap_or_default();
    if let Some(summary) = array(field(properties,"emotionsSummary")) {
        for entry in summary.iter().take(128) {
            let Some(key) = field(entry,"key").and_then(Value::as_str) else { continue; };
            let users = emotions.iter().find(|emotion| {
                field(emotion, "key").and_then(Value::as_str)
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(key))
            })
                .and_then(|emotion| array(field(emotion,"users"))).unwrap_or_default();
            let count = field(entry,"count").and_then(Value::as_u64).and_then(|count| usize::try_from(count).ok()).unwrap_or(users.len());
            add(key, count, users.iter().take(512).filter_map(|user| person(user,false)).collect());
        }
    } else {
        for entry in emotions.iter().take(128) {
            let Some(key) = field(entry,"key").and_then(Value::as_str) else { continue; };
            let users = array(field(entry,"users")).unwrap_or_default();
            add(key, users.len(), users.iter().take(512).filter_map(|user| person(user,false)).collect());
        }
    }
    output
}

pub struct Request {
    pub url: String,
    pub body: Value,
    pub remove: bool,
}

/// Multiple reactions are preserved, matching the original's branch without
/// `replace=true`. A removal sends its key in the JSON body, not in the URL.
pub fn request(base: &str, chat: &str, message: &str, emoji: &str, remove: bool, time_ms: i64) -> Result<Request, String> {
    let key = key(emoji).ok_or("Reação não suportada.")?;
    if chat.is_empty() || message.is_empty() || message.starts_with("local:") {
        return Err("Aguarde a confirmação da mensagem antes de reagir.".into());
    }
    if !remove && time_ms < 0 {
        return Err("O relógio do sistema não permite enviar a reação.".into());
    }
    let url = format!(
        "{}/v1/users/ME/conversations/{}/messages/{}/properties?name=emotions",
        base.trim_end_matches('/'), urlencoding::encode(chat), urlencoding::encode(message),
    );
    let body = if remove { json!({"emotions": {"key": key}}) }
        else { json!({"emotions": {"key": key, "value": time_ms}}) };
    Ok(Request { url, body, remove })
}

pub fn request_error(error: impl std::fmt::Display) -> String {
    let message = error.to_string();
    if message.starts_with("401 ") || message.starts_with("HTTP 401 ") {
        "A sessão expirou. Entre novamente para reagir.".into()
    } else if message.starts_with("HTTP 403 ") {
        "O Teams não permitiu alterar esta reação.".into()
    } else {
        "Não foi possível atualizar a reação. Tente novamente.".into()
    }
}

pub fn identity(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    value.strip_prefix("8:orgid:").unwrap_or(&value).to_owned()
}

pub fn contains_own(reaction: &ReactionCount, user_id: &str) -> bool {
    let user = identity(user_id);
    !user.is_empty() && reaction.reactors.iter().any(|reactor| identity(&reactor.id) == user)
}

/// Apply only an acknowledged operation (or an offline demo action). This is
/// idempotent, and changes only the current user's membership, not other users.
pub fn set_own(message: &mut GraphMessage, emoji: &str, present: bool, user_id: &str, user_name: &str) {
    if message.is_deleted || identity(user_id).is_empty() { return; }
    let current = message.reactions.iter().position(|reaction| reaction.emoji == emoji);
    let has_own = current.is_some_and(|index| contains_own(&message.reactions[index], user_id));
    if has_own == present { return; }
    if let Some(index) = current {
        let reaction = &mut message.reactions[index];
        if present {
            reaction.count = reaction.count.saturating_add(1);
            reaction.reactors.push(Reactor { id: user_id.into(), name: user_name.into() });
        } else {
            let user = identity(user_id);
            reaction.reactors.retain(|reactor| identity(&reactor.id) != user);
            reaction.count = reaction.count.saturating_sub(1).max(reaction.reactors.len());
            if reaction.count == 0 { message.reactions.remove(index); }
        }
    } else if present {
        message.reactions.push(ReactionCount {
            emoji: emoji.into(), count: 1,
            reactors: vec![Reactor { id: user_id.into(), name: user_name.into() }],
        });
    }
}

const MAX_CONFIRMATIONS: usize = 64;
const CONFIRMATION_TTL: Duration = Duration::from_secs(60);

struct Confirmation {
    chat: String,
    message: String,
    emoji: String,
    present: bool,
    recorded_at: Instant,
}

/// Protect a service-acknowledged change until the history projection catches
/// up. Bound to the session by the owner; never persisted to disk.
#[derive(Default)]
pub struct Confirmations { entries: VecDeque<Confirmation> }

impl Confirmations {
    pub fn remember(&mut self, chat: &str, message: &str, emoji: &str, present: bool) {
        self.remember_at(chat, message, emoji, present, Instant::now());
    }

    fn remember_at(&mut self, chat: &str, message: &str, emoji: &str, present: bool, now: Instant) {
        self.entries.retain(|entry| {
            now.saturating_duration_since(entry.recorded_at) < CONFIRMATION_TTL
                && !(entry.chat == chat && entry.message == message && entry.emoji == emoji)
        });
        self.entries.push_back(Confirmation { chat: chat.into(), message: message.into(),
            emoji: emoji.into(), present, recorded_at: now });
        while self.entries.len() > MAX_CONFIRMATIONS { self.entries.pop_front(); }
    }

    pub fn reconcile_page(&mut self, chat: &str, messages: &mut [GraphMessage], user_id: &str, user_name: &str) {
        self.reconcile_at(chat, messages, user_id, user_name, Instant::now());
    }

    fn reconcile_at(&mut self, chat: &str, messages: &mut [GraphMessage], user_id: &str, user_name: &str, now: Instant) {
        self.entries.retain(|entry| now.saturating_duration_since(entry.recorded_at) < CONFIRMATION_TTL);
        self.entries.retain(|entry| {
            if entry.chat != chat { return true; }
            let Some(message) = messages.iter_mut().find(|message| message.id == entry.message) else { return true; };
            if message.is_deleted { return false; }
            let present = message.reactions.iter().any(|reaction| reaction.emoji == entry.emoji && contains_own(reaction, user_id));
            if present == entry.present { return false; }
            set_own(message, &entry.emoji, entry.present, user_id, user_name);
            true
        });
    }

    pub fn clear(&mut self) { self.entries.clear(); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_reaction_projection_preserves_tone_identity_summary_and_other_people() {
        let properties=json!({"emotions":[
            {"key":"1f44a_oncomingfist-tone1","users":[{"mri":"8:orgid:user","displayName":"Pessoa"}]},
            {"key":"1f44a_oncomingfist-tone4","users":[{"mri":"peer","displayName":"Outra pessoa"}]}
        ],"emotionsSummary":[{"key":"1f44a_oncomingfist-tone1","count":8},{"key":"1f44a_oncomingfist-tone4","count":2},{"key":"unknown-not-in-catalog","count":9}]});
        let reactions=from_properties(&properties);
        assert_eq!(reactions.len(),2);
        assert_eq!(reactions[0].count,8);
        assert!(contains_own(&reactions[0],"user"));
        assert_eq!(reactions[1].emoji,"1f44a_oncomingfist-tone4");
        assert_eq!(reactions[1].count,2);
        let encoded=json!({"emotions":properties["emotions"].to_string(),"emotionsSummary":properties["emotionsSummary"].to_string()});
        assert_eq!(from_properties(&encoded),reactions);
        let graph=json!({"reactions":[{"reactionType":"clappinghands","user":{"user":{"id":"user","displayName":"Pessoa"}}}],"emotions":properties["emotions"]});
        let projected=from_properties(&graph);
        assert_eq!(projected.len(),1);
        assert_eq!(projected[0].emoji,"clappinghands");
        assert!(contains_own(&projected[0],"user"));
        let kept = from_properties(&json!({"reactions":[],"emotions":properties["emotions"],"emotionsSummary":properties["emotionsSummary"]}));
        assert_eq!(kept, reactions);
        assert_eq!(token_for_key("LIKE").as_deref(), Some("👍"));
        assert_eq!(token_for_key("like-tone2").as_deref(), Some("👍"));
    }

    #[test]
    fn expanded_picker_emotion_uses_the_observed_catalog_key() {
        let request = request("https://chat.test", "chat", "42", "1f44a_oncomingfist", false, 7).unwrap();
        assert_eq!(request.body, json!({"emotions":{"key":"1f44a_oncomingfist","value":7}}));
        assert!(super::request("https://chat.test", "chat", "42", "invented-reaction", false, 7).is_err());
    }

    #[test]
    fn native_emotion_contract_uses_properties_and_a_numeric_timestamp() {
        let add = request("https://chat.test/", "19:chat@thread.v2", "1720000000000", "😮", false, 1720000000123).unwrap();
        assert_eq!(add.url, "https://chat.test/v1/users/ME/conversations/19%3Achat%40thread.v2/messages/1720000000000/properties?name=emotions");
        assert_eq!(add.body, json!({"emotions":{"key":"surprised","value":1720000000123_i64}}));
        let remove = request("https://chat.test", "19:chat", "42", "❤️", true, 0).unwrap();
        assert_eq!(remove.body, json!({"emotions":{"key":"heart"}}));
        assert!(remove.remove);
        assert!(!remove.url.contains("/reactions"));
        assert!(request("https://chat.test", "chat", "42", "🔥", false, 1).is_err());
        assert!(request("https://chat.test", "chat", "local:pending", "👍", false, 1).is_err());
    }

    fn message() -> GraphMessage { GraphMessage { id: "42".into(), ..Default::default() } }

    #[test]
    fn acknowledged_membership_is_idempotent_and_preserves_other_reactors() {
        let mut message = message();
        set_own(&mut message, "👍", true, "peer", "Same name");
        set_own(&mut message, "👍", true, "user", "Same name");
        set_own(&mut message, "👍", true, "8:orgid:USER", "Same name");
        assert_eq!(message.reactions[0].count, 2);
        set_own(&mut message, "👍", false, "USER", "Same name");
        set_own(&mut message, "👍", false, "USER", "Same name");
        assert_eq!(message.reactions[0].count, 1);
        assert_eq!(message.reactions[0].reactors[0].id, "peer");
    }

    #[test]
    fn stale_history_does_not_erase_ack_and_server_echo_retires_confirmation() {
        let mut confirmations = Confirmations::default();
        confirmations.remember("chat-a", "42", "😮", true);
        let mut page = vec![message()];
        confirmations.reconcile_page("chat-a", &mut page, "user", "You");
        assert_eq!(page[0].reactions[0].emoji, "😮");
        assert_eq!(confirmations.entries.len(), 1);
        confirmations.reconcile_page("chat-a", &mut page, "user", "You");
        assert!(confirmations.entries.is_empty());
    }

    #[test]
    fn confirmations_are_scoped_bounded_and_expire_or_yield_to_deletion() {
        let now = Instant::now();
        let mut confirmations = Confirmations::default();
        confirmations.remember_at("chat-a", "42", "👍", true, now);
        let mut page = vec![message()];
        confirmations.reconcile_at("chat-b", &mut page, "user", "You", now);
        assert!(page[0].reactions.is_empty());
        confirmations.reconcile_at("chat-a", &mut page, "user", "You", now + CONFIRMATION_TTL);
        assert!(page[0].reactions.is_empty());
        assert!(confirmations.entries.is_empty());
        confirmations.remember_at("chat-a", "42", "👍", true, now);
        page[0].is_deleted = true;
        confirmations.reconcile_at("chat-a", &mut page, "user", "You", now);
        assert!(page[0].reactions.is_empty());
        assert!(confirmations.entries.is_empty());
        for id in 0..100 { confirmations.remember("chat-a", &id.to_string(), "👍", true); }
        assert_eq!(confirmations.entries.len(), MAX_CONFIRMATIONS);
        confirmations.clear();
        assert!(confirmations.entries.is_empty());
    }

    #[test]
    fn acknowledged_removal_cannot_be_restored_by_a_stale_history_page() {
        let mut confirmations = Confirmations::default();
        confirmations.remember("chat-a", "42", "👍", false);
        let mut page = vec![message()];
        set_own(&mut page[0], "👍", true, "user", "You");
        set_own(&mut page[0], "👍", true, "peer", "Peer");
        confirmations.reconcile_page("chat-a", &mut page, "user", "You");
        assert_eq!(page[0].reactions[0].count, 1);
        assert_eq!(page[0].reactions[0].reactors[0].id, "peer");
    }
}
