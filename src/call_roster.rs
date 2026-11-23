//! Live meeting roster from `conversation/rosterUpdate`.
//!
//! The Windows calling bundle builds this as `participants` keyed by MRI,
//! each with `details.displayName` and `endpoints`. A full snapshot uses
//! `type: Full`; every other update only changes the people it names.
//! Media flags follow the stream labels already present in the calling SDP
//! (`audio`, `video`, `applicationsharing-video`). Lobby participants may not
//! have endpoints yet, so an explicit waiting marker preserves them. Names and
//! opaque participant MRIs stay in the UI snapshot and are omitted from Debug.

use serde_json::Value;
use std::collections::BTreeMap;

const CAP: usize = 200;

#[derive(Clone, PartialEq, Eq)]
pub struct RosterFace {
    /// Opaque Teams MRI used only to target a moderation command. Never render
    /// or log this value.
    pub participant_id: String,
    pub label: String,
    pub video_on: bool,
    pub screen_on: bool,
    pub muted: Option<bool>,
    pub is_self: bool,
    pub is_waiting: bool,
    pub admission_known: bool,
    pub has_endpoints: bool,
}

impl std::fmt::Debug for RosterFace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RosterFace")
            .field("video_on", &self.video_on)
            .field("screen_on", &self.screen_on)
            .field("muted", &self.muted)
            .field("is_self", &self.is_self)
            .field("is_waiting", &self.is_waiting)
            .field("admission_known", &self.admission_known)
            .field("has_endpoints", &self.has_endpoints)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Person {
    id: String,
    label: String,
    audio_msi: Option<u32>,
    #[cfg(feature = "libwebrtc-av")]
    video_msi: Option<u32>,
    #[cfg(feature = "libwebrtc-av")]
    screen_msi: Option<u32>,
    video_on: bool,
    screen_on: bool,
    muted: Option<bool>,
    is_self: bool,
    is_waiting: Option<bool>,
    has_endpoints: bool,
}

pub struct Roster {
    self_mri: String,
    people: Vec<Person>,
    endpoints: BTreeMap<(String, String), Endpoint>,
}

impl Roster {
    pub fn new(self_mri: String) -> Self {
        Self {
            self_mri,
            people: Vec::new(),
            endpoints: BTreeMap::new(),
        }
    }

    pub fn apply(&mut self, body: &Value) -> bool {
        let Some(root) = roster_root(body) else {
            return false;
        };
        let before = (self.people.clone(), self.endpoints.clone());
        self.apply_endpoints(root);
        apply_root(self, root);
        self.people != before.0 || self.endpoints != before.1
    }

    pub fn faces(&self) -> Vec<RosterFace> {
        self.people
            .iter()
            .map(|person| RosterFace {
                participant_id: person.id.clone(),
                label: person.label.clone(),
                video_on: person.video_on,
                screen_on: person.screen_on,
                muted: person.muted,
                is_self: person.is_self,
                is_waiting: person.is_waiting == Some(true),
                admission_known: person.is_waiting.is_some(),
                has_endpoints: person.has_endpoints,
            })
            .collect()
    }
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn first_remote_video_source(&self) -> Option<u32> {
        self.people.iter().filter(|person| !person.is_self && person.video_on)
            .find_map(|person| person.video_msi)
    }
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn first_remote_screen_source(&self) -> Option<u32> {
        self.people.iter().filter(|person| !person.is_self && person.screen_on)
            .find_map(|person| person.screen_msi)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Admission { #[default] Unknown, InCall, Lobby }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EndpointAudio {
    pub admission: Admission,
    pub server_muted: Option<bool>,
    pub version: Option<u64>,
}
#[derive(Clone, Default, PartialEq, Eq)]
struct Endpoint {
    participant: String,
    audio: EndpointAudio,
}
impl Roster {
    fn apply_endpoints(&mut self, root: &Value) {
        let Some(people) = root.get("participants").and_then(Value::as_object) else { return; };
        let full = root.get("type").and_then(Value::as_str).is_some_and(|s|s.eq_ignore_ascii_case("full"));
        let previous = self.endpoints.clone();
        if full { self.endpoints.clear(); }
        for (person, value) in people.iter().take(CAP) {
            if leaves(value) {
                self.endpoints.retain(|(id,_),_|id!=person);
                continue;
            }
            let Some(endpoints) = value.get("endpoints").and_then(Value::as_object) else { continue; };
            for (id, value) in endpoints.iter().take(16) {
                let key = (person.clone(), id.clone());
                if value.is_null() { self.endpoints.remove(&key); continue; }
                let mut endpoint = previous.get(&key).cloned().unwrap_or_default();
                if let Some(participant) = value.get("participantId").and_then(Value::as_str) {
                    if !endpoint.participant.is_empty() && endpoint.participant != participant { endpoint = Endpoint::default(); }
                    endpoint.participant = participant.into();
                }
                if value.get("lobby").is_some_and(Value::is_object) {
                    endpoint.audio.admission = Admission::Lobby;
                } else if value.get("call").is_some_and(Value::is_object) {
                    endpoint.audio.admission = Admission::InCall;
                }
                if let Some(call) = value.get("call") {
                    let version = call.get("serverMuteVersion").and_then(Value::as_u64);
                    let muted = call.get("mediaStreams").and_then(Value::as_array).and_then(|streams|streams.iter().find(|stream|
                        matches!(stream.get("type").and_then(Value::as_str),Some("audio"|"audio-teleconferencing"))))
                        .and_then(|stream|stream.get("serverMuted")).and_then(Value::as_bool);
                    if let Some(muted) = muted {
                        let newer = match (endpoint.audio.version, version) {
                            (None, _) => true,
                            (Some(old), Some(new)) => new > old,
                            _ => false,
                        };
                        if newer { endpoint.audio.server_muted=Some(muted); endpoint.audio.version=version; }
                    }
                }
                self.endpoints.insert(key, endpoint);
            }
        }
    }
    pub(crate) fn own_audio(&self, endpoint: &str, participant: &str) -> Option<EndpointAudio> {
        self.endpoints.get(&(self.self_mri.clone(),endpoint.into()))
            .filter(|e|!participant.is_empty() && e.participant==participant).map(|e|e.audio)
    }
    pub(crate) fn confirm_unmute(&mut self, endpoint: &str, participant: &str, body: &Value) -> bool {
        let Some(current) = self.endpoints.get_mut(&(self.self_mri.clone(), endpoint.into())) else { return false; };
        if current.participant != participant || current.audio.admission != Admission::InCall { return false; }
        let Some(info) = body.get("muteUnmuteResponse").and_then(|info|info.get(&self.self_mri)).and_then(Value::as_array) else { return false; };
        let Some(entry) = info.iter().find(|entry|entry.get("participantId").and_then(Value::as_str)==Some(participant)) else { return false; };
        let Some(version) = entry.get("serverMuteVersion").and_then(Value::as_u64) else { return false; };
        if entry.get("reason").and_then(Value::as_str)!=Some("success") || current.audio.version.is_none_or(|old|version<=old) { return false; }
        current.audio.server_muted=Some(false);current.audio.version=Some(version);true
    }
}

fn roster_root(body: &Value) -> Option<&Value> {
    for key in ["roster", "rosterUpdate"] {
        if let Some(root) = body.get(key).filter(|root| root.get("participants").is_some()) {
            return Some(root);
        }
    }
    let participants = body.get("participants")?;
    let invitation = participants.get("from").is_some() || participants.get("to").is_some();
    if invitation || !(participants.is_object() || participants.is_array()) {
        return None;
    }
    Some(body)
}

fn apply_root(roster: &mut Roster, root: &Value) {
    let full = root
        .get("type")
        .and_then(|kind| kind.as_str())
        .is_some_and(|kind| kind.eq_ignore_ascii_case("full"));
    let mut seen = Vec::new();
    let entries: Vec<(String, &Value)> = match root.get("participants") {
        Some(Value::Object(map)) => map.iter().map(|(key, value)| (key.clone(), value)).collect(),
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(|value| str_field(value, &["id", "mri"]).map(|id| (id, value)))
            .collect(),
        _ => Vec::new(),
    };
    for (key, value) in entries {
        let id = str_field(value, &["mri", "id"]).unwrap_or(key);
        if id.trim().is_empty() {
            continue;
        }
        if leaves(value) {
            roster.people.retain(|person| person.id != id);
            continue;
        }
        seen.push(id.clone());
        let parsed = parse_person(&id, value, &roster.self_mri);
        if let Some(current) = roster.people.iter_mut().find(|person| person.id == id) {
            merge(current, parsed, value);
        } else if roster.people.len() < CAP {
            roster.people.push(parsed);
        }
    }
    if full {
        roster.people.retain(|person| seen.contains(&person.id));
    }
}

fn leaves(value: &Value) -> bool {
    if value.is_null() {
        return true;
    }
    let state = str_field(value, &["state", "status"]).unwrap_or_default();
    if ["disconnected", "left", "removed", "terminated", "departed"]
        .iter()
        .any(|marker| state.eq_ignore_ascii_case(marker))
    {
        return true;
    }
    // Empty endpoints can mean lobby or pre-media state. Departure requires
    // an explicit terminal state; a later full roster will drop stale entries.
    false
}

fn parse_person(id: &str, value: &Value, self_mri: &str) -> Person {
    let label = str_field(value, &["displayName", "name"])
        .or_else(|| {
            value
                .get("details")
                .and_then(|details| str_field(details, &["displayName", "name"]))
        })
        .unwrap_or_default();
    let mut person = Person {
        id: id.to_owned(),
        label,
        audio_msi: None,
        #[cfg(feature = "libwebrtc-av")]
        video_msi: None,
        #[cfg(feature = "libwebrtc-av")]
        screen_msi: None,
        video_on: false,
        screen_on: false,
        muted: None,
        is_self: !self_mri.is_empty() && id.eq_ignore_ascii_case(self_mri),
        is_waiting: admission_state(value),
        has_endpoints: has_nonempty_endpoints(value)
            || value.get("mediaStreams").is_some()
            || value.get("call").is_some(),
    };
    let mut endpoints: Vec<&Value> = match value.get("endpoints") {
        Some(Value::Object(map)) => map.values().collect(),
        Some(Value::Array(list)) => list.iter().collect(),
        _ => Vec::new(),
    };
    if value.get("mediaStreams").is_some() || value.get("call").is_some() {
        endpoints.push(value);
    }
    for endpoint in endpoints {
        if endpoint.get("lobby").is_some_and(Value::is_object) {
            person.is_waiting = Some(true);
        } else if endpoint.get("call").is_some_and(Value::is_object)
            && person.is_waiting != Some(true) {
            person.is_waiting = Some(false);
        }
        let streams = endpoint
            .get("call")
            .and_then(|call| call.get("mediaStreams"))
            .or_else(|| endpoint.get("mediaStreams"))
            .and_then(|streams| streams.as_array());
        for stream in streams.into_iter().flatten() {
            let kind = str_field(stream, &["type"])
                .unwrap_or_default()
                .to_ascii_lowercase();
            let Some(msi) = stream.get("sourceId").and_then(as_u32) else {
                continue;
            };
            let direction = str_field(stream, &["direction"])
                .unwrap_or_default()
                .to_ascii_lowercase();
            let server_muted = stream
                .get("serverMuted")
                .and_then(|flag| flag.as_bool())
                .unwrap_or(false);
            let sending = (direction.is_empty() || direction.starts_with("send")) && !server_muted;
            match kind.as_str() {
                "audio" if person.audio_msi.is_none() => person.audio_msi = Some(msi),
                "video" if !person.video_on || sending => {
                    person.video_on = sending;
                    #[cfg(feature = "libwebrtc-av")]
                    { person.video_msi = sending.then_some(msi); }
                },
                "applicationsharing-video" if !person.screen_on || sending => {
                    person.screen_on = sending;
                    #[cfg(feature = "libwebrtc-av")]
                    { person.screen_msi = sending.then_some(msi); }
                }
                _ => {}
            }
        }
        if person.muted.is_none() {
            person.muted = endpoint
                .get("endpointState")
                .and_then(|state| state.get("state"))
                .and_then(|state| state.get("isMuted"))
                .and_then(|flag| flag.as_bool())
                .or_else(|| {
                    endpoint
                        .get("endpointMetadata")
                        .and_then(|metadata| metadata.get("isMicrophoneOn"))
                        .and_then(|flag| flag.as_bool())
                        .map(|on| !on)
                });
        }
    }
    person
}

fn merge(current: &mut Person, incoming: Person, value: &Value) {
    if !incoming.label.is_empty() {
        current.label = incoming.label;
    }
    let has_media = value.get("endpoints").is_some() || value.get("mediaStreams").is_some();
    if has_media {
        current.audio_msi = incoming.audio_msi.or(current.audio_msi);
        current.video_on = incoming.video_on;
        #[cfg(feature = "libwebrtc-av")]
        { current.video_msi = incoming.video_msi; current.screen_msi = incoming.screen_msi; }
        current.screen_on = incoming.screen_on;
        current.has_endpoints = incoming.has_endpoints;
    }
    if incoming.muted.is_some() {
        current.muted = incoming.muted;
    }
    current.is_self = incoming.is_self;
    if incoming.is_waiting.is_some() {
        current.is_waiting = incoming.is_waiting;
    }
}

/// Read only explicit lobby/admission markers. Ordinary roster membership or
/// missing media is not enough to infer that a person is waiting.
fn admission_state(value: &Value) -> Option<bool> {
    let mut explicitly_not_waiting = false;
    for key in ["isWaitingInLobby", "waitingInLobby", "isInLobby", "inLobby", "isWaiting", "waiting"] {
        if let Some(waiting) = bool_field(value, key) {
            if waiting {
                return Some(true);
            }
            explicitly_not_waiting = true;
        }
    }
    for key in ["admissionState", "admissionStatus", "lobbyState", "lobbyStatus", "participantState", "state", "status"] {
        if let Some(candidate) = field(value, key) {
            if let Some(state) = state_value(candidate) {
                if state {
                    return Some(true);
                }
                explicitly_not_waiting = true;
            }
        }
    }
    if let Some(details) = field(value, "details") {
        match admission_state(details) {
            Some(true) => return Some(true),
            Some(false) => explicitly_not_waiting = true,
            None => {}
        }
    }
    explicitly_not_waiting.then_some(false)
}

fn state_value(value: &Value) -> Option<bool> {
    let state = match value {
        Value::String(state) => state,
        Value::Object(_) => {
            let mut explicitly_not_waiting = false;
            for key in ["isWaitingInLobby", "waitingInLobby", "isInLobby", "inLobby", "isWaiting", "waiting"] {
                if let Some(waiting) = bool_field(value, key) {
                    if waiting {
                        return Some(true);
                    }
                    explicitly_not_waiting = true;
                }
            }
            for key in ["admissionState", "admissionStatus", "lobbyState", "lobbyStatus", "participantState", "state", "status", "name", "value"] {
                if let Some(candidate) = field(value, key) {
                    if let Some(state) = state_value(candidate) {
                        if state {
                            return Some(true);
                        }
                        explicitly_not_waiting = true;
                    }
                }
            }
            return explicitly_not_waiting.then_some(false);
        }
        _ => return None,
    };
    let normalized: String = state
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    match normalized.as_str() {
        "waiting" | "waitinginlobby" | "inlobby" | "lobby" | "pendingadmission"
        | "awaitingadmission" | "waitingforadmission" => Some(true),
        "connected" | "inmeeting" | "admitted" | "joined" | "active" | "inconference" => {
            Some(false)
        }
        _ => None,
    }
}

fn has_nonempty_endpoints(value: &Value) -> bool {
    match field(value, "endpoints") {
        Some(Value::Object(map)) => !map.is_empty(),
        Some(Value::Array(list)) => !list.is_empty(),
        _ => false,
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .as_object()?
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value)
}

fn bool_field(value: &Value, key: &str) -> Option<bool> {
    field(value, key).and_then(Value::as_bool)
}

fn str_field(value: &Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    for key in keys {
        if let Some(text) = object
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .and_then(|(_, value)| value.as_str())
        {
            return Some(text.to_owned());
        }
    }
    None
}

fn as_u32(value: &Value) -> Option<u32> {
    match value {
        Value::Number(number) => number.as_u64().and_then(|value| u32::try_from(value).ok()),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "libwebrtc-av")]
    #[test]
    fn remote_video_source_tracks_roster_updates_and_ignores_self() {
        let mut roster = Roster::new("self".into());
        assert!(roster.apply(&serde_json::json!({"participants":{
            "self":{"endpoints":{"one":{"call":{"mediaStreams":[{"type":"video","sourceId":10,"direction":"sendrecv"}]}}}},
            "remote":{"endpoints":{"two":{"call":{"mediaStreams":[{"type":"video","sourceId":42,"direction":"sendonly"}]}}}}
        }})));
        assert_eq!(roster.first_remote_video_source(), Some(42));
        assert!(roster.apply(&serde_json::json!({"participants":{
            "remote":{"endpoints":{"two":{"call":{"mediaStreams":[{"type":"video","sourceId":42,"direction":"recvonly"}]}}}}
        }})));
        assert_eq!(roster.first_remote_video_source(), None);
        assert!(roster.apply(&serde_json::json!({"participants":{
            "remote":{"endpoints":{"two":{"call":{"mediaStreams":[{"type":"applicationsharing-video","sourceId":77,"direction":"sendonly"}]}}}}
        }})));
        assert_eq!(roster.first_remote_screen_source(), Some(77));
    }

    #[test]
    fn full_roster_then_delta_keeps_names_and_drops_who_left() {
        let mut roster = Roster::new("8:orgid:me".into());
        let full = serde_json::json!({
            "participants": {
                "8:orgid:me": {"displayName": "Me", "endpoints": {"e0": {"call": {"mediaStreams": [
                    {"type": "audio", "sourceId": 10, "direction": "sendrecv"},
                    {"type": "video", "sourceId": 11, "direction": "sendrecv"}]}}}},
                "8:orgid:ann": {"displayName": "Ann Baker", "endpoints": {"e1": {
                    "call": {"mediaStreams": [
                        {"type": "audio", "sourceId": "20", "direction": "sendrecv"},
                        {"type": "video", "sourceId": "21", "direction": "sendrecv"},
                        {"type": "applicationsharing-video", "sourceId": 22, "direction": "recvonly"}]},
                    "endpointState": {"state": {"isMuted": true}}}}},
                "8:orgid:bob": {"displayName": "Bob Carter", "endpoints": {"e2": {"call": {"mediaStreams": [
                    {"type": "audio", "sourceId": 30, "direction": "sendrecv"},
                    {"type": "video", "sourceId": 31, "direction": "recvonly"}]}}}}
            },
            "type": "Full"
        });
        assert!(roster.apply(&full));
        let faces = roster.faces();
        assert_eq!(faces.len(), 3);
        let ann = faces.iter().find(|face| face.label == "Ann Baker").unwrap();
        assert!(ann.video_on);
        assert!(!ann.screen_on);
        assert_eq!(ann.muted, Some(true));
        assert!(!ann.is_self);
        let bob = faces.iter().find(|face| face.label == "Bob Carter").unwrap();
        assert!(!bob.video_on);
        assert!(faces.iter().any(|face| face.is_self && face.label == "Me"));

        let delta = serde_json::json!({
            "rosterUpdate": {
                "participants": [
                    {"id": "8:orgid:ann", "endpoints": [{"call": {"mediaStreams": [
                        {"type": "video", "sourceId": 21, "direction": "recvonly"}]}}]},
                    {"id": "8:orgid:bob", "state": "disconnected"},
                    {"id": "8:orgid:cara", "displayName": "Cara Diaz", "endpoints": [{"mediaStreams": [
                        {"type": "video", "sourceId": 41, "direction": "sendonly"}]}]}
                ],
                "type": "Delta"
            }
        });
        assert!(roster.apply(&delta));
        let faces = roster.faces();
        assert!(faces.iter().all(|face| face.label != "Bob Carter"));
        let ann = faces.iter().find(|face| face.label == "Ann Baker").unwrap();
        assert!(!ann.video_on);
        assert_eq!(ann.muted, Some(true));
        let cara = faces.iter().find(|face| face.label == "Cara Diaz").unwrap();
        assert!(cara.video_on);
        assert_eq!(faces.len(), 3);
    }

    #[test]
    fn invitation_participants_are_not_a_roster() {
        let mut roster = Roster::new("8:orgid:me".into());
        let invitation = serde_json::json!({
            "participants": {"from": {"displayName": "Ana"}, "to": [{"displayName": "Bruno"}]}
        });
        assert!(!roster.apply(&invitation));
        assert!(roster.faces().is_empty());
    }

    #[test]
    fn windows_acceptance_roster_uses_details_display_name() {
        let mut roster = Roster::new("8:orgid:me".into());
        let body = serde_json::json!({
            "type": "MultiPartyEndpoint",
            "participants": {
                "8:orgid:ann": {
                    "details": {"id": "8:orgid:ann", "displayName": "Ann Baker"},
                    "endpoints": {"e1": {"call": {}, "contentSharing": {}}}
                }
            }
        });
        assert!(roster.apply(&body));
        let faces = roster.faces();
        assert_eq!(faces.len(), 1);
        assert_eq!(faces[0].label, "Ann Baker");
        assert!(!faces[0].video_on);
    }
    #[test]
    fn structural_lobby_survives_partial_updates_and_transitions_to_call() {
        let mut roster = Roster::new("self".into());
        roster.apply(&serde_json::json!({"type":"Full","participants":{
            "peer":{"endpoints":{"phone":{"lobby":{"mediaStreams":[]}}}}
        }}));
        assert!(roster.faces()[0].is_waiting);
        assert!(roster.faces()[0].admission_known);
        roster.apply(&serde_json::json!({"participants":{"peer":{"details":{"displayName":"Peer"}}}}));
        assert!(roster.faces()[0].is_waiting);
        roster.apply(&serde_json::json!({"participants":{"peer":{"endpoints":{"phone":{"call":{"mediaStreams":[]}}}}}}));
        assert!(!roster.faces()[0].is_waiting);
        roster.apply(&serde_json::json!({"participants":{"peer":{"endpoints":{"phone":{"call":{}},"tablet":{"lobby":{}}}}}}));
        assert!(roster.faces()[0].is_waiting);
    }

    #[test]
    fn endpoint_audio_versions_and_unmute_are_bound_to_the_local_device() {
        let mut roster=Roster::new("self".into());
        let update=|version,muted|serde_json::json!({"participants":{"self":{"endpoints":{
            "desktop":{"participantId":"pc","call":{"serverMuteVersion":version,"mediaStreams":[{"type":"audio","serverMuted":muted}]}},
            "phone":{"participantId":"mobile","call":{"serverMuteVersion":100,"mediaStreams":[{"type":"audio","serverMuted":false}]}}
        }}}});
        roster.apply(&update(7,true));
        assert_eq!(roster.own_audio("desktop","pc").unwrap().server_muted,Some(true));
        assert!(roster.own_audio("desktop","mobile").is_none());
        roster.apply(&update(6,false));
        roster.apply(&update(7,false));
        assert_eq!(roster.own_audio("desktop","pc").unwrap().server_muted,Some(true));
        let response=|participant,version,reason|serde_json::json!({"muteUnmuteResponse":{"self":[{"participantId":participant,"serverMuteVersion":version,"reason":reason}]}});
        assert!(!roster.confirm_unmute("desktop","pc",&response("mobile",8,"success")));
        assert!(!roster.confirm_unmute("desktop","pc",&response("pc",7,"success")));
        assert!(!roster.confirm_unmute("desktop","pc",&response("pc",8,"failure")));
        assert!(!roster.confirm_unmute("desktop","pc",&serde_json::Value::Null));
        assert!(roster.confirm_unmute("desktop","pc",&response("pc",8,"success")));
        roster.apply(&update(7,true));
        assert_eq!(roster.own_audio("desktop","pc").unwrap().server_muted,Some(false));
        roster.apply(&serde_json::json!({"participants":{"self":{"details":{"displayName":"Self"}}}}));
        assert_eq!(roster.own_audio("desktop","pc").unwrap().version,Some(8));
        roster.apply(&serde_json::json!({"type":"Full","participants":{}}));
        assert!(roster.own_audio("desktop","pc").is_none());
        roster.apply(&update(1,true));
        assert_eq!(roster.own_audio("desktop","pc").unwrap().version,Some(1));
        roster.apply(&serde_json::json!({"participants":{"self":{"endpoints":{"desktop":null}}}}));
        assert!(roster.own_audio("desktop","pc").is_none());
    }

}
