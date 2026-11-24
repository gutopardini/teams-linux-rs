//! Call signaling module — parse incoming call invitations and manage call lifecycle.
//!
//! This handles signaling only (no media streaming).

#[cfg(feature = "audio")]
pub mod audio;
pub mod call_test;
#[cfg(feature = "video-capture")]
pub mod camera;
#[cfg(feature = "video-capture")]
pub mod codec;
#[cfg(feature = "video-capture")]
pub mod display;
pub mod ice;
pub mod datagram;
pub mod audio_codec;
pub mod audio_playout;
pub mod media_protection;
pub mod dtls;
pub mod audio_control;
pub mod control_session;
pub mod macav;
pub mod media;
pub mod recording;
pub mod rtcp;
pub mod rtp;
pub mod sdp;
pub mod sdp_compress;
pub mod signaling;
pub mod srtp;
pub mod test_tone;
pub mod turn;
pub mod video;

use serde::Deserialize;

/// Links provided in a callInvitation for signaling actions.
#[derive(Debug, Clone, Deserialize)]
pub struct CallLinks {
    pub acceptance: Option<String>,
    pub end: Option<String>,
    #[serde(rename = "mediaAnswer")]
    pub media_answer: Option<String>,
    #[serde(rename = "p2pForkNotification")]
    pub p2p_fork_notification: Option<String>,
}

/// Media content (SDP blob) from the call invitation.
#[derive(Debug, Clone, Deserialize)]
pub struct MediaContent {
    pub blob: Option<String>,
    #[serde(rename = "contentType")]
    pub content_type: Option<String>,
}

/// Participant identity.
#[derive(Debug, Clone, Deserialize)]
pub struct Participant {
    pub id: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(rename = "endpointId")]
    pub endpoint_id: Option<String>,
    #[serde(rename = "languageId")]
    pub language_id: Option<String>,
}

/// Participants block from the invitation.
#[derive(Debug, Clone, Deserialize)]
pub struct Participants {
    pub from: Option<Participant>,
    pub to: Option<Vec<Participant>>,
}

/// Conversation request links.
#[derive(Debug, Clone, Deserialize)]
pub struct ConversationLinks {
    #[serde(rename = "conversationEnd")]
    pub conversation_end: Option<String>,
    #[serde(rename = "conversationUpdate")]
    pub conversation_update: Option<String>,
    #[serde(rename = "localParticipantUpdate")]
    pub local_participant_update: Option<String>,
}

/// Conversation request from the invitation.
#[derive(Debug, Clone, Deserialize)]
pub struct ConversationRequest {
    pub links: Option<ConversationLinks>,
}

/// Debug content with call/endpoint/operation IDs.
#[derive(Debug, Clone, Deserialize)]
pub struct DebugContent {
    #[serde(rename = "callId")]
    pub call_id: Option<String>,
    #[serde(rename = "endpointId")]
    pub endpoint_id: Option<String>,
    #[serde(rename = "operationId")]
    pub operation_id: Option<String>,
}

/// The core call invitation payload from a Trouter push.
#[derive(Debug, Clone, Deserialize)]
pub struct CallInvitation {
    #[serde(rename = "callModalities")]
    pub call_modalities: Option<Vec<String>>,
    pub links: Option<CallLinks>,
    #[serde(rename = "mediaContent")]
    pub media_content: Option<MediaContent>,
}

/// Top-level envelope for a call notification pushed via Trouter.
#[derive(Debug, Clone, Deserialize)]
pub struct CallNotification {
    #[serde(rename = "callInvitation")]
    pub call_invitation: Option<CallInvitation>,
    pub participants: Option<Participants>,
    #[serde(rename = "conversationRequest")]
    pub conversation_request: Option<ConversationRequest>,
    #[serde(rename = "debugContent")]
    pub debug_content: Option<DebugContent>,
}

/// Simple call lifecycle state.
#[derive(Debug, Clone, PartialEq)]
pub enum CallState {
    Idle,
    Ringing,
    Accepting,
    Connected,
    Ended,
}

/// Try to parse a call notification from the Trouter event JSON.
///
/// The Trouter frame body is an HTTP-like request where the body is JSON.
/// We try to extract the JSON and deserialize it.
pub fn parse_call_notification(json_str: &str) -> Option<CallNotification> {
    let value: serde_json::Value = serde_json::from_str(json_str).ok()?;
    parse_call_notification_value(&value, 0)
}

fn parse_call_notification_value(value: &serde_json::Value, depth: usize) -> Option<CallNotification> {
    if depth > 6 {
        return None;
    }
    if let Some(text) = value.as_str() {
        let nested: serde_json::Value = serde_json::from_str(text).ok()?;
        return parse_call_notification_value(&nested, depth + 1);
    }
    let object = value.as_object()?;
    if let Some(body) = object.iter().find(|(key, _)| key.eq_ignore_ascii_case("body")).map(|(_, v)| v) {
        if let Some(notification) = parse_call_notification_value(body, depth + 1) {
            return Some(notification);
        }
    }
    if let Some(note) = object.iter().find(|(key, _)| key.eq_ignore_ascii_case("callNotification")).map(|(_, v)| v) {
        if let Some(notification) = ngc_call_notification(object, note) {
            return Some(notification);
        }
        if let Some(notification) = parse_call_notification_value(note, depth + 1) {
            return Some(notification);
        }
    }
    let invitation = object
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("callInvitation"))
        .map(|(_, value)| value)
        .and_then(|value| parse_invitation(value));
    invitation.as_ref()?;
    let participants_object = object
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("participants"))
        .and_then(|(_, participants)| participants.as_object());
    let from = participants_object
        .and_then(|participants| {
            participants.iter().find(|(key, _)| key.eq_ignore_ascii_case("from")).map(|(_, value)| value)
        })
        .and_then(|from| serde_json::from_value::<Participant>(from.clone()).ok());
    let to = participants_object
        .and_then(|participants| participants.iter().find(|(key, _)| key.eq_ignore_ascii_case("to")).map(|(_, value)| value))
        .and_then(|to| serde_json::from_value::<Vec<Participant>>(to.clone()).ok());
    let debug_content = object
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("debugContent"))
        .and_then(|(_, value)| serde_json::from_value::<DebugContent>(value.clone()).ok());
    let conversation_request = object
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("conversationRequest"))
        .and_then(|(_, value)| serde_json::from_value::<ConversationRequest>(value.clone()).ok());
    Some(CallNotification {
        call_invitation: invitation,
        participants: (from.is_some() || to.is_some()).then_some(Participants { from, to }),
        conversation_request,
        debug_content,
    })
}

/// Live rings put the caller on `callNotification.from`. `to` is one object,
/// and there is no `callInvitation`.
fn ngc_call_notification(
    parent: &serde_json::Map<String, serde_json::Value>,
    note: &serde_json::Value,
) -> Option<CallNotification> {
    let note = if let Some(text) = note.as_str() {
        serde_json::from_str::<serde_json::Value>(text).ok()?
    } else {
        note.clone()
    };
    let note = note.as_object()?;
    let from = note.get("from").and_then(|from| serde_json::from_value::<Participant>(from.clone()).ok())?;
    let to = note.get("to").and_then(|to| {
        if to.is_array() {
            serde_json::from_value::<Vec<Participant>>(to.clone()).ok()
        } else {
            serde_json::from_value::<Participant>(to.clone()).ok().map(|person| vec![person])
        }
    });
    let debug_content = parent
        .get("debugContent")
        .or_else(|| note.get("debugContent"))
        .and_then(|value| serde_json::from_value::<DebugContent>(value.clone()).ok());
    let video = note.get("mediaContent").and_then(|content| content.as_object()).is_some_and(|content| {
        content.get("contentType").and_then(|value| value.as_str()).is_some_and(|kind| kind.to_ascii_lowercase().contains("video"))
            || content.get("blob").and_then(|value| value.as_str()).is_some_and(|blob| blob.to_ascii_lowercase().contains("m=video"))
    });
    Some(CallNotification {
        call_invitation: Some(CallInvitation {
            call_modalities: Some(vec![if video { "video" } else { "audio" }.to_owned()]),
            links: None,
            media_content: None,
        }),
        participants: Some(Participants { from: Some(from), to }),
        conversation_request: None,
        debug_content,
    })
}

/// Read the invitation even when a sibling field has a shape this client does
/// not model. A mismatched `participants.to` used to drop the whole ring.
fn parse_invitation(value: &serde_json::Value) -> Option<CallInvitation> {
    let value = if let Some(text) = value.as_str() {
        serde_json::from_str::<serde_json::Value>(text).ok()?
    } else {
        value.clone()
    };
    let object = value.as_object()?;
    let call_modalities = object.get("callModalities").and_then(|modalities| {
        if let Some(list) = modalities.as_array() {
            Some(list.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
        } else {
            modalities.as_str().map(|one| vec![one.to_owned()])
        }
    });
    let links = object.get("links").and_then(|links| serde_json::from_value(links.clone()).ok());
    let media_content = object.get("mediaContent").and_then(|content| {
        if content.is_object() {
            serde_json::from_value(content.clone()).ok()
        } else {
            content.as_str().map(|blob| MediaContent { blob: Some(blob.to_owned()), content_type: None })
        }
    });
    Some(CallInvitation { call_modalities, links, media_content })
}
