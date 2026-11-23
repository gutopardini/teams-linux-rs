//! Own native meeting setup, from the original EV/iy payloads and validated SDP.
//! No API here performs login, opens a device, or declares the callee rang
//! merely because the conversation controller accepted an HTTP request.
use crate::{
    call_context::{CallCallbacks, CallContext, CallbackKind},
    call_lifecycle::CallKey,
    call_media::{MediaError, PreparedAudio, RemoteAudio},
    call_session::{
        CallResources, CallSession, CallTransport, ControlError, LeaveOutcome, LocalParticipant,
        ServiceOrigin,
    },
    meeting::JoinLink,
};
use serde_json::{Value, json};
use tokio::sync::watch;

// Original getEndpointCapabilities: ask the resolver to join the existing
// conversation on conflict. This is independent of ending conflicted calls.
const AUTO_JOIN_ON_CONFLICT: u32 = 1 << 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    InvalidRequest,
    StaleTransport,
    InvalidController,
    InteractionRequired,
    InvalidAcceptance,
    Cancelled,
    InvalidState,
    Control(ControlError),
    Media(MediaError),
}

pub struct CallRoute {
    endpoint: String,
    region: Option<String>,
    partition: Option<String>,
    ring: Option<String>,
}
impl std::fmt::Debug for CallRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallRoute([REDACTED])")
    }
}
impl CallRoute {
    /// The caller supplies the existing authenticated routing table. No fixed
    /// cloud/partition and no fallback to the desktop app's identity.
    pub fn from_discovery(table: &Value) -> Result<Self, JoinError> {
        let endpoint = table
            .get("calling_conversationServiceUrl")
            .and_then(Value::as_str)
            .ok_or(JoinError::InvalidRequest)?;
        ServiceOrigin::from_discovery(endpoint).map_err(|_| JoinError::InvalidRequest)?;
        fn field(table: &Value, key: &str) -> Result<Option<String>, JoinError> {
            match table.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(value))
                    if value.len() <= 64 && !value.chars().any(char::is_control) =>
                {
                    Ok((!value.is_empty()).then(|| value.clone()))
                }
                _ => Err(JoinError::InvalidRequest),
            }
        }
        Ok(Self {
            endpoint: endpoint.into(),
            region: field(table, "regionName")?,
            partition: field(table, "partition")?,
            ring: field(table, "ring")?,
        })
    }
    #[allow(dead_code)] // Used by the authenticated ingress adapter, not the login flow.
    pub(crate) fn transport(
        &self,
        token: crate::auth::calling::CallingToken,
    ) -> Result<CallTransport, JoinError> {
        CallTransport::new(
            ServiceOrigin::from_discovery(&self.endpoint).map_err(|_| JoinError::InvalidRequest)?,
            token,
        )
        .map_err(|_| JoinError::InvalidRequest)?
        .with_region(
            self.region.as_deref(),
            self.partition.as_deref(),
            self.ring.as_deref(),
        )
        .map_err(JoinError::Control)
    }
    #[cfg(test)]
    pub(crate) fn fixture(base: &str) -> Self {
        Self {
            endpoint: format!("{base}/epconv"),
            region: None,
            partition: None,
            ring: None,
        }
    }
}

pub struct StartMeeting {
    pub key: CallKey,
    pub identity: LocalParticipant,
    pub invitation: JoinLink,
    pub callbacks: CallCallbacks,
    pub context: CallContext,
    pub muted: bool,
}

pub enum PreparedMedia {
    Native(PreparedAudio),
    #[cfg(feature = "libwebrtc-av")]
    WebRtc(teams_av::engine::Engine),
}
impl From<PreparedAudio> for PreparedMedia {
    fn from(value: PreparedAudio) -> Self { Self::Native(value) }
}
impl PreparedMedia {
    fn offer(&self) -> &str {
        match self {
            Self::Native(value) => value.offer(),
            #[cfg(feature = "libwebrtc-av")]
            Self::WebRtc(value) => value.signaled_offer(),
        }
    }
    fn has_video(&self) -> bool {
        match self {
            Self::Native(value) => value.has_video(),
            #[cfg(feature = "libwebrtc-av")]
            Self::WebRtc(_) => true,
        }
    }
    async fn close(self) {
        match self {
            Self::Native(value) => value.close().await,
            #[cfg(feature = "libwebrtc-av")]
            Self::WebRtc(value) => drop(value),
        }
    }
}

struct MeetingRequest(Value);
impl std::fmt::Debug for MeetingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MeetingRequest([REDACTED])")
    }
}
impl MeetingRequest {
    fn build(start: &StartMeeting, audio: &PreparedMedia) -> Result<Self, JoinError> {
        if !start.callbacks.is_current(&start.key, &start.context) {
            return Err(JoinError::StaleTransport);
        }
        start
            .identity
            .validate()
            .map_err(|_| JoinError::InvalidRequest)?;
        let cb = |kind| start.callbacks.link(kind);
        // Base bits64/1024 are unconditional in the original. GVC1 and
        // hostless4 match this meeting-only path. Do not advertise screen,
        // video, broker, watermark, automatic promotion or compressed pushes.
        let mut payload = json!({
            "conversationRequest": {
                "conversationType":null,"subject":null,"suppressDialout":true,
                "roster":{"type":"Delta","rosterUpdate":cb(CallbackKind::RosterUpdate)},
                "properties":{"allowConversationWithoutHost":true,"enableGroupCallEventMessages":true,
                    "enableGroupCallUpgradeMessage":false,"enableGroupCallMeetupGeneration":false},
                "links":{
                    "conversationEnd":cb(CallbackKind::ConversationEnd),"conversationUpdate":cb(CallbackKind::ConversationUpdate),
                    "localParticipantUpdate":cb(CallbackKind::LocalParticipantUpdate),
                    "addParticipantSuccess":cb(CallbackKind::AddParticipantSuccess),"addParticipantFailure":cb(CallbackKind::AddParticipantFailure),
                    "addModalitySuccess":cb(CallbackKind::AddModalitySuccess),"addModalityFailure":cb(CallbackKind::AddModalityFailure),
                    "confirmUnmute":cb(CallbackKind::ConfirmUnmute),"receiveMessage":cb(CallbackKind::ReceiveMessage)
                }
            },
            "participants":{"from":start.identity,"to":[]},
            "capabilities":null,"endpointCapabilities":1|4|64|1024,"clientEndpointCapabilities":null,
            "endpointMetadata":null,"groupContext":null,"groupChat":null,
            "meetingData":{"meetingCode":"","meetingUrl":start.invitation.browser_url()},
            "callInvitation": {
                "callModalities":["audio"],"replaces":null,"transferor":null,
                "links":{"progress":cb(CallbackKind::Progress),"mediaAnswer":cb(CallbackKind::MediaAnswer),
                    "acceptance":cb(CallbackKind::Acceptance),"redirection":cb(CallbackKind::Redirection),"end":cb(CallbackKind::CallEnd)},
                "mediaContent":{"contentType":"application/sdp","blob":audio.offer()}
            }
        });
        if start.muted {
            payload["endpointState"] =
                json!({"endpointStateSequenceNumber":1,"state":{"isMuted":true}});
        }
        if let Some(thread) = start.invitation.thread_id() {
            let coordinates = start.invitation.meeting_coordinates().map_err(|_| JoinError::InvalidRequest)?;
            let message_id = coordinates.as_ref().map(|c| c.root_message_id.as_str());
            payload["groupChat"] = json!({"threadId":thread,"messageId":message_id});
            if let Some(coordinates) = coordinates {
                // uw is a join from the local participant, not a new dial-out.
                payload["participants"].as_object_mut().unwrap().remove("to");
                // Present in both phases of the successful PWA join and true
                // by default in the original configuration. Do not copy its
                // unrelated video, compression or broker capability flags.
                payload["endpointCapabilities"] = json!(1 | 4 | AUTO_JOIN_ON_CONFLICT | 64 | 1024);
                if let (Some(tenant), Some(organizer)) = (coordinates.tenant_id, coordinates.organizer_id) {
                    payload["meetingInfo"] = json!({"tenantId":tenant,"organizerId":organizer});
                    if let Some(message) = coordinates.reply_chain_message_id {
                        payload["meetingInfo"]["replyChainMessageId"] = json!(message);
                    }
                }
            }
            payload.as_object_mut().unwrap().remove("meetingData");
            payload["callInvitation"]["callModalities"] = json!(["Audio"]);
            if let Some(peer) = start.invitation.callee() {
                // The calling bundle refuses to start without the remote
                // participant. An empty `to` came back as ConversationEnd
                // 409/5704, the conversation-resolution conflict.
                payload["participants"]["to"] = json!([{"id": peer, "participantId": crate::call_context::native_id()}]);
                payload["conversationRequest"]["suppressDialout"] = json!(false);
            }
        }
        if audio.has_video() {
            payload["callInvitation"]["callModalities"]=json!(["Audio","Video"]);
            payload["callInvitation"]["clientContentForMediaController"]=json!({"controlVideoStreaming":cb(CallbackKind::ControlVideoStreaming)});
        }
        #[cfg(feature = "libwebrtc-av")]
        if matches!(audio, PreparedMedia::WebRtc(_)) {
            // The offer already contains camera, receive and sharing lines.
            // The PWA join that connected used this capability word, and the
            // working native client sends the client word beside it. Camera
            // stays off until the mixer accepts a direction update.
            payload["endpointCapabilities"] = json!(73_463);
            payload["clientEndpointCapabilities"] = json!(47_150_826);
            payload["endpointMetadata"] = json!({"holographicCapabilities": 3});
            payload["callInvitation"]["callModalities"] = json!(["Audio"]);
            payload["callInvitation"]["mediaContent"]["contentType"] = json!("application/sdp-ngc-1.0");
            payload["callInvitation"]["mediaContent"]["mediaLegId"] = json!(crate::call_context::native_id());
            payload["callInvitation"]["mediaContent"]["mediaDescriptions"] = json!({"descriptions":[],"requestId":1});
        }
        Ok(Self(payload))
    }

    /// Resolve membership before offering media. This is a different request
    /// from joining: undefined call options in the PWA are absent on the wire.
    fn resolution(&self) -> Value {
        let mut resolve = self.0.clone();
        let root = resolve.as_object_mut().unwrap();
        root.remove("callInvitation");
        root.remove("meetingData");
        root["participants"].as_object_mut().unwrap().remove("to");
        let conversation = root["conversationRequest"].as_object_mut().unwrap();
        conversation.remove("conversationType");
        conversation.remove("suppressDialout");
        let links = conversation["links"].as_object_mut().unwrap();
        for key in ["addModalitySuccess", "addModalityFailure", "confirmUnmute"] {
            links.remove(key);
        }
        resolve
    }

    /// Keep only bounded routing fields returned by the authenticated resolver.
    /// Neither meeting codes nor tenant identities are exposed to diagnostics.
    fn use_resolution(&mut self, body: &Value) -> Result<(), JoinError> {
        if let Some(chat) = body.pointer("/activeModalities/groupChat").filter(|v| !v.is_null()) {
            let thread = chat.get("threadId").and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= 2048 && !s.chars().any(char::is_control))
                .ok_or(JoinError::InvalidController)?;
            if self.0.pointer("/groupChat/threadId").and_then(Value::as_str).is_some_and(|expected| expected != thread) {
                return Err(JoinError::InvalidController);
            }
            let message = chat.get("messageId").unwrap_or(&Value::Null);
            if !message.is_null() && !message.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 128 && s.bytes().all(|c| c.is_ascii_digit())) {
                return Err(JoinError::InvalidController);
            }
            self.0["groupChat"] = json!({"threadId":thread,"messageId":message});
        }
        if let Some(info) = body.get("meetingInfo").filter(|v| !v.is_null()) {
            let mut canonical = serde_json::Map::new();
            for key in ["tenantId", "organizerId", "replyChainMessageId"] {
                if let Some(value) = info.get(key).filter(|v| !v.is_null()) {
                    let text = value.as_str().filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
                        .ok_or(JoinError::InvalidController)?;
                    canonical.insert(key.into(), json!(text));
                }
            }
            if !canonical.contains_key("tenantId") || !canonical.contains_key("organizerId") {
                return Err(JoinError::InvalidController);
            }
            self.0["meetingInfo"] = Value::Object(canonical);
        }
        if let Some(data) = body.get("meetingData").filter(|v| !v.is_null()) {
            let mut canonical = serde_json::Map::new();
            for key in ["meetingCode", "passcode"] {
                if let Some(value) = data.get(key).filter(|v| !v.is_null()) {
                    let text = value.as_str().filter(|s| s.len() <= 128 && !s.chars().any(char::is_control))
                        .ok_or(JoinError::InvalidController)?;
                    canonical.insert(key.into(), json!(text));
                }
            }
            if !canonical.is_empty() { self.0["meetingData"] = Value::Object(canonical); }
        }
        if let Some(subject) = body.get("subject").filter(|v| !v.is_null()) {
            if !subject.as_str().is_some_and(|s| s.len() <= 8192) { return Err(JoinError::InvalidController); }
            self.0["conversationRequest"]["subject"] = subject.clone();
        }
        Ok(())
    }
}

pub enum StartResult {
    Waiting(WaitingMeeting),
    Cancelled(Option<LeaveOutcome>),
}
impl std::fmt::Debug for StartResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Waiting(_) => "StartResult::Waiting([REDACTED])",
            Self::Cancelled(_) => "StartResult::Cancelled",
        })
    }
}

struct DirectCall {
    peer: String,
    thread_id: String,
    modalities: Value,
    invited: bool,
}

pub struct WaitingMeeting {
    pub session: CallSession,
    pub(crate) initial_roster: Option<crate::call_roster::Roster>,
    key: CallKey,
    callbacks: CallCallbacks,
    audio: Option<PreparedMedia>,
    muted: bool,
    pub video_control: Option<crate::call_video::Control>,
    pub(crate) video_session: Option<crate::call_video::Session>,
    direct_call: Option<DirectCall>,
    /// SDP already bound to the live socket. A later answer repeats it so the
    /// remote keeps the ICE credentials and SRTP key this session is using.
    local_description: String,
    active_agreement: Option<crate::call_media::AudioAgreement>,
    /// Service URL from the acceptance acknowledgement. The camera offer is
    /// posted here only after the meeting is connected.
    pub(crate) media_renegotiation: Option<String>,
    /// Service URL for `UpdateMediaDescriptions`. The PWA posts the camera
    /// direction change here. The click still uses `media_renegotiation`
    /// until a connected meeting shows this URL was present.
    pub(crate) update_media_descriptions: Option<String>,
    /// Mixer leg deleted on hangup, after leave and before the peer closes.
    pub(crate) call_leg: Option<String>,
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) apply_channel_parameters: Option<String>,
}

pub(crate) fn ack_renegotiation_link(body: &Value) -> Option<&str> {
    body.pointer("/callAcceptanceAcknowledgement/links/mediaRenegotiation")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty() && url.len() <= 4096)
}

/// The acceptance callback carries the same service URL. The 17:50 meeting
/// stored nothing from the acknowledgement response (`call_ack_links`
/// messages=0) and every camera click stopped before an offer.
pub(crate) fn acceptance_renegotiation_link(body: &Value) -> Option<&str> {
    body.pointer("/callAcceptance/links/mediaRenegotiation")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty() && url.len() <= 4096)
}

pub(crate) fn acceptance_update_media_link(body: &Value) -> Option<&str> {
    body.pointer("/callAcceptance/links/updateMediaDescriptions")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty() && url.len() <= 4096)
}

pub(crate) fn ack_update_media_link(body: &Value) -> Option<&str> {
    body.pointer("/callAcceptanceAcknowledgement/links/updateMediaDescriptions")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty() && url.len() <= 4096)
}

/// The acknowledgement response replaces the stored URL only when it carries
/// one. An empty body keeps the URL taken from the acceptance callback.
pub(crate) fn keep_or_replace_link(current: Option<String>, incoming: Option<String>) -> Option<String> {
    incoming.or(current)
}

/// Control URLs that move when the mixer switches legs. A later payload wins,
/// including a `links` object that sits beside the media holder.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MixerLegLinks {
    pub update_media_descriptions: Option<String>,
    pub apply_channel_parameters: Option<String>,
    pub control_video_streaming: Option<String>,
    pub media_renegotiation: Option<String>,
    pub call_leg: Option<String>,
}

const MIXER_LEG_LINK_NAMES: &[&str] = &[
    "updateMediaDescriptions",
    "applyChannelParameters",
    "controlVideoStreaming",
    "mediaRenegotiation",
    "callLeg",
];

pub(crate) fn mixer_leg_links(value: &Value) -> MixerLegLinks {
    let mut links = MixerLegLinks::default();
    collect_mixer_leg_links(value, 0, &mut links);
    links
}

fn collect_mixer_leg_links(value: &Value, depth: usize, out: &mut MixerLegLinks) {
    if depth > 8 {
        return;
    }
    match value {
        Value::Object(map) => {
            if let Some(Value::Object(links)) = map.get("links")
                && links
                    .keys()
                    .any(|name| MIXER_LEG_LINK_NAMES.contains(&name.as_str()))
            {
                for (name, url) in links {
                    let Some(url) = json_link(url) else { continue };
                    match name.as_str() {
                        "updateMediaDescriptions" => out.update_media_descriptions = Some(url),
                        "applyChannelParameters" => out.apply_channel_parameters = Some(url),
                        "controlVideoStreaming" => out.control_video_streaming = Some(url),
                        "mediaRenegotiation" => out.media_renegotiation = Some(url),
                        "callLeg" => out.call_leg = Some(url),
                        _ => {}
                    }
                }
            }
            for child in map.values() {
                collect_mixer_leg_links(child, depth + 1, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_mixer_leg_links(item, depth + 1, out);
            }
        }
        _ => {}
    }
}

fn json_link(value: &Value) -> Option<String> {
    let url = value
        .as_str()
        .or_else(|| value.get("href").and_then(Value::as_str))?;
    if url.is_empty() || url.len() > 4096 {
        None
    } else {
        Some(url.to_owned())
    }
}

/// Outgoing camera offer. The HTTP body is the inner `mediaNegotiation`
/// object: the official sender wraps it in `payload` and then posts that
/// inner object. Audio stays in the offer so the connected session is not
/// replaced by a video-only description.
#[derive(Debug)]
pub(crate) enum CameraOfferReply {
    /// The POST was accepted. The video answer still arrives as `mediaAnswer`.
    Wait,
    /// The server put the answer in the POST response.
    Answer {
        content_type: String,
        blob: String,
        acknowledgement: Option<String>,
    },
    /// The response carried a non-zero failure code. The audio call stays up.
    Rejected,
}

/// The official renegotiation waits for a callback and does not treat the POST
/// body as the SDP answer. This only classifies that body so an inlined answer
/// or a failure code is not thrown away.
pub(crate) fn camera_offer_reply(body: &Value) -> CameraOfferReply {
    if let Some(blob) = body
        .pointer("/mediaAnswer/mediaContent/blob")
        .and_then(Value::as_str)
        .filter(|blob| !blob.is_empty() && blob.len() <= 1024 * 1024)
    {
        let content_type = body
            .pointer("/mediaAnswer/mediaContent/contentType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let acknowledgement = body
            .pointer("/mediaAnswer/links/mediaAcknowledgement")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty() && url.len() <= 4096)
            .map(str::to_owned);
        return CameraOfferReply::Answer {
            content_type,
            blob: blob.to_owned(),
            acknowledgement,
        };
    }
    let code = body
        .get("code")
        .and_then(Value::as_i64)
        .or_else(|| body.pointer("/mediaNegotiationFailure/code").and_then(Value::as_i64));
    if code.is_some_and(|code| code != 0) {
        return CameraOfferReply::Rejected;
    }
    CameraOfferReply::Wait
}

#[cfg(test)]
struct MeetingMedia {
    content_type: &'static str,
    blob: String,
    video_sections: usize,
}

#[cfg(test)]
struct AudioTransport {
    port: String,
    proto: String,
    ufrag: String,
    pwd: String,
    crypto: String,
}

#[cfg(test)]
fn audio_transport(audio: &str) -> Option<AudioTransport> {
    let mut port = None;
    let mut proto = None;
    let mut in_audio = false;
    let mut ufrag = None;
    let mut pwd = None;
    let mut crypto = None;
    for line in audio.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("m=") {
            let mut fields = rest.split_ascii_whitespace();
            let kind = fields.next()?;
            if kind == "audio" && !in_audio {
                let port_text = fields.next()?;
                let proto_text = fields.next()?;
                if !(port_text.bytes().all(|byte| byte.is_ascii_digit())
                    && (1..=5).contains(&port_text.len()))
                {
                    return None;
                }
                if proto_text.len() > 64
                    || proto_text
                        .bytes()
                        .any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'/'))
                {
                    return None;
                }
                port = Some(port_text.to_owned());
                proto = Some(proto_text.to_owned());
                in_audio = true;
                continue;
            }
            if in_audio {
                break;
            }
        }
        if !in_audio {
            continue;
        }
        if let Some(value) = line.strip_prefix("a=ice-ufrag:") {
            if credential(value) {
                ufrag = Some(value.to_owned());
            }
        } else if let Some(value) = line.strip_prefix("a=ice-pwd:") {
            if credential(value) {
                pwd = Some(value.to_owned());
            }
        } else if line.starts_with("a=crypto:") && crypto.is_none() && line.len() <= 512 {
            crypto = Some(line.to_owned());
        }
    }
    Some(AudioTransport {
        port: port?,
        proto: proto?,
        ufrag: ufrag?,
        pwd: pwd?,
        crypto: crypto?,
    })
}

#[cfg(test)]
fn credential(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| byte.is_ascii_graphic() && byte != b':')
}

/// Meeting sections measured with the camera off. Not sent: the compressed
/// form was ended before acceptance with 410/301000 and an SDP phrase.
#[cfg(test)]
fn meeting_placeholder_sections(audio: &str) -> Option<String> {
    let transport = audio_transport(audio)?;
    let mut offer = insert_bundle(audio);
    if !audio.lines().any(|line| line.starts_with("a=mid:")) {
        offer.push_str("\r\na=mid:0");
    }
    let mut video = |mid: u8, direction: &str| {
        offer.push_str(&format!(
            "\r\nm=video {port} {proto} 122\r\na=mid:{mid}\r\na={direction}\r\na=rtpmap:122 H264/90000\r\na=rtcp-mux\r\na=ice-ufrag:{ufrag}\r\na=ice-pwd:{pwd}\r\n{crypto}",
            port = transport.port,
            proto = transport.proto,
            ufrag = transport.ufrag,
            pwd = transport.pwd,
            crypto = transport.crypto,
        ));
    };
    for mid in 1..=6 {
        video(mid, "sendrecv");
    }
    for mid in 7..=10 {
        video(mid, "recvonly");
    }
    video(11, "sendrecv");
    offer.push_str(&format!(
        "\r\nm=x-data {port} {proto} 127\r\na=mid:12\r\na=sendrecv\r\na=rtpmap:127 x-data/90000\r\na=rtcp-mux\r\na=ice-ufrag:{ufrag}\r\na=ice-pwd:{pwd}\r\n{crypto}\r\n",
        port = transport.port,
        proto = transport.proto,
        ufrag = transport.ufrag,
        pwd = transport.pwd,
        crypto = transport.crypto,
    ));
    Some(offer)
}

#[cfg(test)]
fn insert_bundle(audio: &str) -> String {
    const GROUP: &str = "a=group:BUNDLE 0 1 2 3 4 5 6 7 8 9 10 11 12\r\n";
    if audio.lines().any(|line| line.starts_with("a=group:")) {
        return audio.trim_end_matches(['\r', '\n']).to_owned();
    }
    if let Some(index) = audio.find("\r\nm=").or_else(|| audio.find("\nm=")) {
        let split = index + if audio[index..].starts_with("\r\n") { 2 } else { 1 };
        let mut offer = String::new();
        offer.push_str(&audio[..split]);
        offer.push_str(GROUP);
        offer.push_str(audio[split..].trim_end_matches(['\r', '\n']));
        return offer;
    }
    audio.trim_end_matches(['\r', '\n']).to_owned()
}

#[cfg(test)]
fn meeting_media(audio: &str) -> Option<MeetingMedia> {
    let offer = meeting_placeholder_sections(audio)?;
    let video_sections = offer.lines().filter(|line| line.starts_with("m=video ")).count();
    let (content_type, blob) = match ost::calling::sdp_compress::compress_sdp(&offer) {
        Ok(Some(blob)) => ("application/sdp-ngc-1.0", blob),
        Ok(None) => ("application/sdp-ngc-1.0", offer),
        Err(_) => return None,
    };
    Some(MeetingMedia {
        content_type,
        blob,
        video_sections,
    })
}

/// Keeps the audio offer and the new video section on separate lines.
pub(crate) fn camera_offer_sdp(audio: &str, section: &str) -> String {
    let mut offer = audio.trim_end_matches(['\r', '\n']).to_owned();
    offer.push_str("\r\n");
    offer.push_str(section.trim_start_matches(['\r', '\n']));
    if !offer.ends_with('\n') {
        offer.push_str("\r\n");
    }
    offer
}

pub(crate) fn camera_offer_payload(
    sender: &LocalParticipant,
    sdp: &str,
    media_answer: &str,
    rejection: &str,
) -> Value {
    json!({
        "mediaNegotiation": {
            "callModalities": ["Audio", "Video"],
            "sender": sender,
            "links": {"mediaAnswer": media_answer, "rejection": rejection},
            "mediaContent": {"contentType": "application/sdp", "blob": sdp}
        }
    })
}

pub(crate) enum RenegotiationDecision {
    Send(RenegotiationRequest),
    Ignore,
}

pub(crate) struct RenegotiationRequest {
    pub url: String,
    pub payload: Value,
    /// 1 answers with the live local description. 2 rejects an offer this
    /// session cannot apply. The numeric trace never carries the SDP.
    pub trace: usize,
}

impl std::fmt::Debug for RenegotiationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RenegotiationRequest([REDACTED])")
    }
}

async fn stop_requested(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

/// Own this job until its HTTP request finishes. Cancellation releases the
/// offered socket immediately, then compensates a late controller response
/// with one leave. Do not abort the job as a substitute for cancelling it.
pub async fn start_meeting<M: Into<PreparedMedia>>(
    route:CallRoute,transport:CallTransport,start:StartMeeting,audio:M,cancel:watch::Receiver<bool>,
)->Result<StartResult,JoinError>{
    let mut audio=Some(audio.into());
    let result=start_meeting_inner(route,transport,start,&mut audio,cancel).await;
    // A rejected/invalid setup response must release the same prepared lease
    // as a normal cancellation, including its current TURN nonce.
    if let Some(audio)=audio.take(){audio.close().await;}
    result
}
async fn start_meeting_inner(
    route:CallRoute,transport:CallTransport,start:StartMeeting,audio:&mut Option<PreparedMedia>,mut cancel:watch::Receiver<bool>,
)->Result<StartResult,JoinError>{
    if *cancel.borrow() || cancel.has_changed().is_err() {
        return Ok(StartResult::Cancelled(None));
    }
    let mut request=MeetingRequest::build(&start,audio.as_ref().ok_or(JoinError::InvalidState)?)?;
    // Shape-only trace: retain no URL, thread, tenant, organizer or message IDs.
    let coordinate_flags = usize::from(!start.invitation.browser_url().is_empty())
        | (usize::from(request.0.pointer("/groupChat/messageId").is_some_and(|v| v.is_string())) << 1)
        | (usize::from(request.0.get("meetingInfo").is_some()) << 2);
    crate::live_trace::record("call_join_coordinates", 0, coordinate_flags, 0);
    // Successful PWA runtime: resolve epconv without call options or SDP,
    // then join the returned controller. A direct call keeps its dial-out.
    let two_phase = start.invitation.thread_id().is_some() && start.invitation.callee().is_none();
    let public_meeting = two_phase && !start.invitation.browser_url().is_empty();
    crate::live_trace::record("call_join_request_mode", 0, if public_meeting { 4 } else if two_phase { 2 } else if start.invitation.callee().is_some() { 3 } else { 1 }, 0);
    let create = if two_phase { request.resolution() } else { request.0.clone() };
    let (response, cancelled) = {
        let pending = transport.post_json(&route.endpoint, &create);
        tokio::pin!(pending);
        tokio::select! {biased;
            _=stop_requested(&mut cancel)=>{if let Some(audio)=audio.take(){audio.close().await;}(pending.await,true)},
            response=&mut pending=>(response,false),
        }
    };
    let cancelled = cancelled || *cancel.borrow() || cancel.has_changed().is_err();
    let body = match response {
        Ok(body) => body,
        Err(error)=>return if cancelled{Ok(StartResult::Cancelled(None))}else{Err(JoinError::Control(error))},
    };
    let mut initial_roster = crate::call_roster::Roster::new(start.identity.id.clone());
    initial_roster.apply(&body);
    // Keep only the control projection, not the whole participant roster in UI
    // state. The HTTP response itself has a separate1MiB streaming limit.
    let mut control = json!({"conversationController":body.get("conversationController"),
        "links":{"leave":body.pointer("/links/leave"),"updateEndpointState":body.pointer("/links/updateEndpointState"),"unmute":body.pointer("/links/unmute")}});
    if let Some(sequence) = body.get("sequenceNumber") {
        control["sequenceNumber"] = sequence.clone();
    }
    let mut session = CallSession::new(
        start.key.clone(),
        start.identity.clone(),
        transport,
        CallResources::default(),
    )
    .map_err(|_| JoinError::InvalidRequest)?;
    session
        .apply_conversation_update_detailed(
            &start.key,
            &serde_json::to_vec(&control).map_err(|_| JoinError::InvalidController)?,
        )
        .map_err(|error| {
            crate::call_diagnostics::rejected(crate::call_diagnostics::Phase::Create,error,&body,Some(&route.endpoint));
            JoinError::InvalidController
        })?;
    if cancelled {
        if let Some(audio)=audio.take(){audio.close().await;}
        return Ok(StartResult::Cancelled(Some(
            session
                .cancel_outgoing(&start.key)
                .await
                .map_err(|_| JoinError::InvalidState)?,
        )));
    }
    let requires_interaction = ["enableCaptcha", "enforceConsentToJoin"]
        .into_iter()
        .any(|field| {
            body.get("selfParticipant")
                .and_then(|participant| participant.get(field))
                .is_some_and(|value| value != &Value::Bool(false))
        })
        || body
            .pointer("/selfParticipant/preMeetingConsentInfo")
            .is_some_and(|value| !value.is_null());
    if requires_interaction {
        if let Some(audio)=audio.take(){audio.close().await;}
        let _ = session.cancel_outgoing(&start.key).await;
        return Err(JoinError::InteractionRequired);
    }
    if two_phase {
        if *cancel.borrow() || cancel.has_changed().is_err() {
            return Ok(StartResult::Cancelled(Some(session.cancel_outgoing(&start.key).await.map_err(|_|JoinError::InvalidState)?)));
        }
        // apply_conversation_update_detailed has already validated and bound
        // the transport to this controller's delegated HTTPS origin.
        let controller = body["conversationController"].as_str().ok_or(JoinError::InvalidController)?;
        if let Err(error) = request.use_resolution(&body) {
            let _ = session.cancel_outgoing(&start.key).await;
            return Err(error);
        }
        crate::live_trace::record("call_join_controller_start", 0, 0, 0);
        let mut join = request.0.clone();
        // The current PWA sends an empty recipient list after resolution.
        // Group chat calls retain the existing no-recipient representation.
        if public_meeting { join["participants"]["to"] = json!([]); }
        else { join["participants"].as_object_mut().unwrap().remove("to"); }
        let (joined, cancelled) = {
            let pending = session.transport().post_json(controller, &join);
            tokio::pin!(pending);
            tokio::select! { biased;
                _=stop_requested(&mut cancel)=>{
                    if let Some(audio)=audio.take(){audio.close().await;}
                    (pending.await,true)
                },
                result=&mut pending=>(result,false),
            }
        };
        if cancelled || *cancel.borrow() || cancel.has_changed().is_err() {
            return Ok(StartResult::Cancelled(Some(session.cancel_outgoing(&start.key).await.map_err(|_|JoinError::InvalidState)?)));
        }
        match joined {
            Ok(body) => { initial_roster.apply(&body); }
            Err(error) => {
                let _ = session.cancel_outgoing(&start.key).await;
                return Err(JoinError::Control(error));
            }
        }
        crate::live_trace::record("call_join_controller_completed", 0, 0, 0);
    }
    let direct_call = start.invitation.callee().map(|peer| DirectCall {
        peer: peer.to_owned(),
        thread_id: start.invitation.thread_id().unwrap_or_default(),
        modalities: request.0["callInvitation"]["callModalities"].clone(),
        invited: false,
    });
    let local_description = audio
        .as_ref()
        .map(|prepared| prepared.offer().to_owned())
        .unwrap_or_default();
    Ok(StartResult::Waiting(WaitingMeeting {
        session,
        initial_roster: Some(initial_roster),
        key: start.key,
        callbacks: start.callbacks,
        audio:audio.take(),
        video_control:None,
        video_session:None,
        muted: start.muted,
        direct_call,
        local_description,
        active_agreement: None,
        media_renegotiation: None,
        update_media_descriptions: None,
        call_leg: None,
        #[cfg(feature = "libwebrtc-av")]
        apply_channel_parameters: None,
    }))
}

pub(crate) fn answer_modalities(value: Option<&Value>, video: bool) -> Vec<&'static str> {
    if let Some(items) = value.and_then(Value::as_array) {
        if !items.is_empty() {
            let mut out = Vec::new();
            for item in items {
                let Some(name) = item.as_str() else { continue };
                if name.eq_ignore_ascii_case("audio") && !out.contains(&"Audio") {
                    out.push("Audio");
                } else if video && name.eq_ignore_ascii_case("video") && !out.contains(&"Video") {
                    out.push("Video");
                }
            }
            return out;
        }
    }
    let mut out = vec!["Audio"];
    if video {
        out.push("Video");
    }
    out
}

fn dial_operation(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    path.ends_with("/add") || path.ends_with("/addParticipant")
}

fn controller_add(controller: &str) -> Result<String, JoinError> {
    let mut target = url::Url::parse(controller).map_err(|_| JoinError::InvalidController)?;
    let path = target.path().trim_end_matches('/').to_owned();
    if !path.ends_with("/add") {
        target.set_path(&format!("{path}/add"));
    }
    Ok(target.to_string())
}

#[derive(Clone)]
pub struct KeepAlive {
    pub(crate) url: String,
    pub interval: std::time::Duration,
}
impl std::fmt::Debug for KeepAlive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeepAlive([REDACTED])")
    }
}

pub struct AcceptedMedia {
    pub keepalive: Option<KeepAlive>,
    video: Option<crate::call_video::Remote>,
    remote: Option<RemoteAudio>,
    #[cfg(feature = "libwebrtc-av")]
    webrtc_answer: Option<String>,
    acknowledgement: String,
}
impl std::fmt::Debug for AcceptedMedia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AcceptedMedia([REDACTED])")
    }
}

pub(crate) struct RetainedMixerLeg {
    pub call_leg: Option<String>,
    pub stored: usize,
}

impl WaitingMeeting {
    /// Replace the control URLs of the current mixer leg. An invalid or absent
    /// URL leaves the one already stored. `call_leg` is returned so the
    /// keepalive can follow the new leg.
    pub(crate) fn retain_mixer_leg(
        &mut self,
        links: &MixerLegLinks,
        mut accept: impl FnMut(&str) -> bool,
    ) -> RetainedMixerLeg {
        let mut stored = 0usize;
        {
            let mut store = |slot: &mut Option<String>, incoming: &Option<String>| {
                if let Some(url) = incoming.as_deref().filter(|url| accept(url)) {
                    *slot = Some(url.to_owned());
                    stored += 1;
                }
            };
            store(
                &mut self.update_media_descriptions,
                &links.update_media_descriptions,
            );
            store(&mut self.media_renegotiation, &links.media_renegotiation);
        }
        #[cfg(feature = "libwebrtc-av")]
        if let Some(url) = links
            .apply_channel_parameters
            .as_deref()
            .filter(|url| accept(url))
            .or_else(|| {
                links
                    .control_video_streaming
                    .as_deref()
                    .filter(|url| accept(url))
            })
        {
            self.apply_channel_parameters = Some(url.to_owned());
            stored += 1;
        }
        let call_leg = links
            .call_leg
            .as_deref()
            .filter(|url| accept(url))
            .map(str::to_owned);
        if call_leg.is_some() {
            stored += 1;
        }
        RetainedMixerLeg { call_leg, stored }
    }

    pub(crate) fn remember_camera_offer(&mut self, offer: String) {
        self.local_description = offer;
    }
    pub(crate) fn current_offer(&self) -> &str {
        &self.local_description
    }
    /// The original `handleMediaNegotiationOffer` stores `links.mediaAnswer`
    /// and raises the offer only while the call is connected. Bundle 26288
    /// processes the offer and posts `createAnswerAsync(false)`. The audio
    /// section stays the one already bound to the socket. Each extra section
    /// is answered with port 0, because a shorter SDP is rejected and the
    /// service ends the call. An offer this session cannot apply is rejected
    /// with 488/3102 when the rejection link stays on the controller origin.
    /// An empty callback is ignored so a hollow event cannot end the call.
    pub(crate) fn renegotiation_answer(&self, body: &Value) -> RenegotiationDecision {
        let negotiation = body.get("mediaNegotiation").unwrap_or(body);
        let answer_url = negotiation
            .pointer("/links/mediaAnswer")
            .and_then(Value::as_str);
        let rejection_url = negotiation.pointer("/links/rejection").and_then(Value::as_str);
        let content_type = negotiation
            .pointer("/mediaContent/contentType")
            .and_then(Value::as_str)
            .unwrap_or("");
        let blob = negotiation
            .pointer("/mediaContent/blob")
            .and_then(Value::as_str)
            .unwrap_or("");
        let remote = if blob.len() <= crate::call_media::MAX_SDP_BYTES {
            RemoteAudio::parse(content_type, blob)
        } else { Err(MediaError::InvalidDescription) };
        let offer_sdp = ost::calling::sdp_compress::decompress_sdp_with_limit(blob, crate::call_media::MAX_SDP_BYTES).ok();
        let shaped = offer_sdp.as_deref().and_then(|offer| crate::call_media::answer_matching_offer(&self.local_description, offer));
        let remote_ok = remote.as_ref().is_ok_and(|remote|
            self.active_agreement.as_ref().is_some_and(|active| active.same_session(&remote.agreement())));
        let video = self
            .local_description
            .lines()
            .any(|line| line.starts_with("m=video "));
        let modalities = answer_modalities(negotiation.get("callModalities"), video);
        crate::call_diagnostics::renegotiation(negotiation, remote.err(),
            answer_url.is_some_and(|url| self.session.transport().validate_response_link(url).is_ok()),
            modalities.contains(&"Audio"));
        let can_answer = remote_ok
            && shaped.is_some()
            && modalities.iter().any(|name| *name == "Audio");
        if can_answer {
            if let Some(url) = answer_url
                .filter(|url| self.session.transport().validate_response_link(url).is_ok())
            {
                let mut media_answer = json!({
                    "callModalities": modalities,
                    "sender": self.session.participant(),
                    "links": {"mediaAcknowledgement": self.callbacks.link(CallbackKind::MediaAcknowledgement)},
                    "mediaContent": {"contentType": "application/sdp", "blob": shaped.unwrap_or_default()}
                });
                if modalities.iter().any(|name| *name == "Video") {
                    media_answer["clientContentForMediaController"] = json!({
                        "controlVideoStreaming": self.callbacks.link(CallbackKind::ControlVideoStreaming)
                    });
                }
                return RenegotiationDecision::Send(RenegotiationRequest {
                    url: url.to_owned(),
                    payload: json!({"mediaAnswer": media_answer}),
                    trace: 1,
                });
            }
        }
        let offered_something = negotiation.get("links").is_some() || negotiation.get("mediaContent").is_some();
        if offered_something {
            if let Some(url) = rejection_url
                .filter(|url| self.session.transport().validate_response_link(url).is_ok())
            {
                return RenegotiationDecision::Send(RenegotiationRequest {
                    url: url.to_owned(),
                    payload: json!({"mediaNegotiationFailure": {
                        "sender": self.session.participant(),
                        "code": 488,
                        "subCode": 3102,
                        "phrase": "CallEndReasonMediaAnswerError"
                    }}),
                    trace: 2,
                });
            }
        }
        RenegotiationDecision::Ignore
    }

    pub(crate) fn awaits_direct_ring(&self) -> bool {
        self.direct_call.as_ref().is_some_and(|call| !call.invited)
    }

    /// Dial the peer once. An update link is used only when that update is
    /// already in hand and names an add operation on the authorized origin.
    /// Otherwise /add keeps the current controller query. The create response's
    /// addParticipant link is never read here.
    pub(crate) async fn ring(
        &mut self,
        update: Option<&Value>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<bool, JoinError> {
        let Some(call) = self.direct_call.as_ref() else { return Ok(false); };
        if call.invited { return Ok(false); }
        if *cancel.borrow() || cancel.has_changed().is_err() { return Ok(true); }
        let from_update = update
            .map(|body| body.get("conversationUpdate").unwrap_or(body))
            .and_then(|body| body.pointer("/links/addParticipant"))
            .and_then(Value::as_str)
            .filter(|link| self.session.transport().validate_response_link(link).is_ok())
            .filter(|link| url::Url::parse(link).is_ok_and(|url| dial_operation(url.path())));
        let (target, target_class) = match from_update {
            Some(link) => {
                let class = url::Url::parse(link)
                    .ok()
                    .filter(|url| url.path().trim_end_matches('/').ends_with("/addParticipant"))
                    .map_or(2, |_| 1);
                (link.to_owned(), class)
            }
            None => (controller_add(self.session.controller().ok_or(JoinError::InvalidController)?)?, 0),
        };
        crate::live_trace::record("call_invite_target_v1", 0, target_class, 0);
        let (peer, thread_id, modalities) = {
            let call = self.direct_call.as_ref().unwrap();
            (call.peer.clone(), call.thread_id.clone(), call.modalities.clone())
        };
        let from = self.session.participant().clone();
        let payload = json!({
            "disableUnmute": false,
            "participants": {"from": from, "to": [{"id": peer, "participantId": crate::call_context::native_id()}]},
            "participantInvitationData": {"callModalities": modalities, "callDirection": "Outgoing"},
            "callInvitation": {"callModalities": modalities, "replaces": null, "transferor": null},
            "replacementDetails": null,
            "groupContext": null,
            "groupChat": {"threadId": thread_id, "messageId": null},
            "links": {
                "addParticipantSuccess": self.callbacks.link(CallbackKind::AddParticipantSuccess),
                "addParticipantFailure": self.callbacks.link(CallbackKind::AddParticipantFailure)
            }
        });
        crate::live_trace::record("call_invite_start", 0, 0, 0);
        let (result, cancelled) = {
            let pending = self.session.transport().post_json(&target, &payload);
            tokio::pin!(pending);
            tokio::select! { biased;
                _ = stop_requested(&mut cancel) => (pending.await, true),
                result = &mut pending => (result, false),
            }
        };
        if cancelled || *cancel.borrow() || cancel.has_changed().is_err() {
            if result.is_ok() {
                if let Some(call) = &mut self.direct_call { call.invited = true; }
            }
            return Ok(true);
        }
        match result {
            Ok(body) => crate::call_diagnostics::invite_response(&body),
            Err(error) => return Err(JoinError::Control(error)),
        }
        if let Some(call) = &mut self.direct_call { call.invited = true; }
        crate::live_trace::record("call_invite_completed", 0, 0, 0);
        Ok(false)
    }

    pub fn acceptance(
        &mut self,
        current: &CallContext,
        address: &str,
        bytes: &[u8],
    ) -> Result<AcceptedMedia, JoinError> {
        if !self
            .callbacks
            .accepts(&self.key, CallbackKind::Acceptance, address, current)
        {
            return Err(JoinError::StaleTransport);
        }
        if bytes.len() > 1024 * 1024 {
            return Err(JoinError::InvalidAcceptance);
        }
        let body: Value =
            serde_json::from_slice(bytes).map_err(|_| JoinError::InvalidAcceptance)?;
        crate::call_diagnostics::acceptance(&body);
        let accepted = body
            .get("callAcceptance")
            .ok_or(JoinError::InvalidAcceptance)?;
        let field = |path| {
            accepted
                .pointer(path)
                .and_then(Value::as_str)
                .ok_or(JoinError::InvalidAcceptance)
        };
        let acknowledgement = field("/links/acknowledgement")?;
        self.session
            .transport()
            .validate_response_link(acknowledgement)
            .map_err(JoinError::Control)?;
        #[cfg(feature = "libwebrtc-av")]
        let is_webrtc = matches!(self.audio.as_ref(), Some(PreparedMedia::WebRtc(_)));
        #[cfg(not(feature = "libwebrtc-av"))]
        let is_webrtc = false;
        let content_type = field("/mediaContent/contentType")?;
        let blob = field("/mediaContent/blob")?;
        #[cfg(feature = "libwebrtc-av")]
        let webrtc_answer = if is_webrtc {
            if !matches!(content_type, "application/sdp" | "application/sdp-ngc-1.0") {
                return Err(JoinError::Media(MediaError::UnsupportedContentType));
            }
            let text = ost::calling::sdp_compress::decompress_sdp_with_limit(blob, crate::call_media::MAX_SDP_BYTES)
                .map_err(|_| JoinError::Media(MediaError::InvalidDescription))?;
            teams_av::sdp::parse(&text).map_err(|_| JoinError::Media(MediaError::InvalidDescription))?;
            Some(text)
        } else { None };
        let remote = if is_webrtc { None } else {
            Some(RemoteAudio::parse(content_type, blob).map_err(JoinError::Media)?)
        };
        let keepalive = match accepted.get("callKeepAliveInterval") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let seconds = value
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 1.0 && *v <= 3600.0)
                    .ok_or(JoinError::InvalidAcceptance)?;
                let url = field("/links/callLeg")?;
                self.session
                    .transport()
                    .validate_response_link(url)
                    .map_err(JoinError::Control)?;
                self.call_leg = Some(url.to_owned());
                Some(KeepAlive {
                    url: url.into(),
                    interval: std::time::Duration::from_secs_f64(seconds * 0.9),
                })
            }
        };
        let video=if !is_webrtc && self.audio.as_ref().is_some_and(|a|a.has_video()) {
            crate::call_video::Remote::parse(field("/mediaContent/contentType")?,field("/mediaContent/blob")?).unwrap_or_else(|_|{crate::live_trace::record("call_video_failed",0,2,0);if let Some(c)=&self.video_control{c.fail();}None})
        } else {None};
        let offered = acceptance_renegotiation_link(&body);
        let stored = offered
            .filter(|url| self.session.transport().validate_response_link(url).is_ok())
            .map(str::to_owned);
        // messages: 1 stored, 2 present but rejected, 0 absent.
        crate::live_trace::record(
            "call_acceptance_renegotiation_link",
            0,
            if stored.is_some() { 1 } else if offered.is_some() { 2 } else { 0 },
            0,
        );
        if stored.is_some() {
            self.media_renegotiation = stored;
        }
        let update_offered = acceptance_update_media_link(&body);
        let update_stored = update_offered
            .filter(|url| self.session.transport().validate_response_link(url).is_ok())
            .map(str::to_owned);
        crate::live_trace::record(
            "call_acceptance_update_media_link",
            0,
            if update_stored.is_some() { 1 } else if update_offered.is_some() { 2 } else { 0 },
            0,
        );
        if update_stored.is_some() {
            self.update_media_descriptions = update_stored;
        }
        #[cfg(feature = "libwebrtc-av")]
        if let Some(url) = accepted.pointer("/links/applyChannelParameters")
            .or_else(|| accepted.pointer("/links/controlVideoStreaming")).and_then(Value::as_str)
            .filter(|url| self.session.transport().validate_response_link(url).is_ok()) {
            self.apply_channel_parameters = Some(url.to_owned());
        }
        Ok(AcceptedMedia {
            video,
            keepalive,
            remote,
            #[cfg(feature = "libwebrtc-av")]
            webrtc_answer,
            acknowledgement: acknowledgement.into(),
        })
    }

    /// Tell the call controller the SDP answer arrived. This must not wait for
    /// a headset profile switch: the controller drops the conversation when the
    /// acknowledgement is late, and the next create then conflicts (409/5704).
    pub async fn acknowledge(
        &mut self,
        accepted: &AcceptedMedia,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<(), JoinError> {
        let stopped = *cancel.borrow() || cancel.has_changed().is_err();
        if stopped {
            self.cancel().await?;
            return Err(JoinError::Cancelled);
        }
        if self.audio.is_none() {
            return Err(JoinError::InvalidState);
        }
        crate::live_trace::record("call_ack_post", 0, 0, 0);
        let cb = |kind| self.callbacks.link(kind);
        let payload = json!({"callAcceptanceAcknowledgement":{"links":{
            "mediaRenegotiation":cb(CallbackKind::MediaRenegotiation),"transfer":cb(CallbackKind::Transfer),
            "replacement":cb(CallbackKind::Replacement),"balanceUpdate":cb(CallbackKind::BalanceUpdate),
            "retargetCompletion":cb(CallbackKind::RetargetCompletion),"controlVideoStreaming":cb(CallbackKind::ControlVideoStreaming),
            "updateMediaDescriptions":cb(CallbackKind::UpdateMediaDescriptions)}}});
        let ack = {
            let pending = self
                .session
                .transport()
                .post_json(&accepted.acknowledgement, &payload);
            tokio::pin!(pending);
            tokio::select! { biased; _=stop_requested(cancel)=>None, result=&mut pending=>Some(result) }
        };
        match ack {
            Some(Ok(body)) => {
                // An empty acknowledgement must not erase the URL already
                // copied from callAcceptance.links. The 17:57 meeting stored
                // that URL and this response then cleared it, so the camera
                // click had nowhere to post.
                let incoming = ack_renegotiation_link(&body)
                    .filter(|url| self.session.transport().validate_response_link(url).is_ok())
                    .map(str::to_owned);
                self.media_renegotiation = keep_or_replace_link(self.media_renegotiation.take(), incoming);
                crate::live_trace::record(
                    "call_ack_links",
                    0,
                    usize::from(self.media_renegotiation.is_some()),
                    0,
                );
                let update_incoming = ack_update_media_link(&body)
                    .filter(|url| self.session.transport().validate_response_link(url).is_ok())
                    .map(str::to_owned);
                self.update_media_descriptions = keep_or_replace_link(self.update_media_descriptions.take(), update_incoming);
                #[cfg(feature = "libwebrtc-av")]
                if let Some(url) = body.pointer("/callAcceptanceAcknowledgement/links/applyChannelParameters")
                    .or_else(|| body.pointer("/callAcceptanceAcknowledgement/links/controlVideoStreaming"))
                    .and_then(Value::as_str)
                    .filter(|url| self.session.transport().validate_response_link(url).is_ok()) {
                    self.apply_channel_parameters = Some(url.to_owned());
                }
                crate::live_trace::record(
                    "call_ack_update_media_link",
                    0,
                    usize::from(self.update_media_descriptions.is_some()),
                    0,
                );
                crate::live_trace::record("call_ack_ok", 0, 0, 0);
                Ok(())
            }
            Some(Err(error)) => {
                self.cancel().await?;
                Err(JoinError::Control(error))
            }
            None => {
                self.cancel().await?;
                Err(JoinError::Cancelled)
            }
        }
    }

    pub async fn connect_media(
        &mut self,
        accepted: AcceptedMedia,
        mut io: ost::calling::media::PcmAudioIo,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<(), JoinError> {
        let stopped = *cancel.borrow() || cancel.has_changed().is_err();
        if stopped {
            self.cancel().await?;
            return Err(JoinError::Cancelled);
        }
        io.initially_muted = self.muted;
        let audio = self.audio.take().ok_or(JoinError::InvalidState)?;
        #[cfg(feature = "libwebrtc-av")]
        let mut audio = match audio {
            PreparedMedia::Native(audio) => audio,
            PreparedMedia::WebRtc(engine) => {
                let answer = accepted.webrtc_answer.ok_or(JoinError::InvalidAcceptance)?;
                let control = self.video_control.clone().ok_or(JoinError::InvalidState)?;
                match crate::call_webrtc::Session::connect(engine, &answer, io, control, cancel).await {
                    Ok(session) if !*cancel.borrow() && !cancel.has_changed().is_err() => {
                        self.session.attach_webrtc(&self.key, session).await.map_err(|_| JoinError::InvalidState)?;
                        return Ok(());
                    }
                    Ok(session) => { session.stop().await; self.cancel().await?; return Err(JoinError::Cancelled); }
                    Err(()) if *cancel.borrow() || cancel.has_changed().is_err() => {
                        self.cancel().await?; return Err(JoinError::Cancelled);
                    }
                    Err(()) => { self.cancel().await?; return Err(JoinError::Media(MediaError::IceFailed)); }
                }
            }
        };
        #[cfg(not(feature = "libwebrtc-av"))]
        let PreparedMedia::Native(mut audio) = audio;
        let video=audio.take_video();
        let remote = accepted.remote.ok_or(JoinError::InvalidAcceptance)?;
        let agreement = remote.agreement();
        let connected = {
            let pending = audio.connect(remote, io, true);
            tokio::pin!(pending);
            tokio::select! {biased;_=stop_requested(cancel)=>None,result=&mut pending=>Some(result)}
        };
        let stopped = *cancel.borrow() || cancel.has_changed().is_err();
        match connected {
            Some(Ok(audio)) if !stopped => {
                self.session
                    .attach_audio(&self.key, audio)
                    .await
                    .map_err(|_| JoinError::InvalidState)?;
                self.active_agreement = Some(agreement);
                if let Some(controls) = self.session.audio_controls(&self.key) {
                    controls.set_microphone_muted(self.muted);
                }
                if let Some(prepared)=video {
                    match (accepted.video,self.video_control.clone()) {
                        (Some(remote),Some(control))=>{
                            let pending=prepared.connect(remote,control.clone(),true);tokio::pin!(pending);
                            let result=tokio::select!{biased;_=stop_requested(cancel)=>None,result=&mut pending=>Some(result)};
                            match result {
                                Some(Ok(session))=>self.video_session=Some(session),
                                Some(Err(_))=>control.fail(),
                                None=>{control.set_enabled(false);self.cancel().await?;return Err(JoinError::Cancelled);}
                            }
                        },
                        (_,control)=>{prepared.close().await;if let Some(c)=control{c.fail();}}
                    }
                }
                Ok(())
            }
            Some(Ok(mut audio)) => {
                audio.stop().await;
                self.cancel().await?;
                Err(JoinError::Cancelled)
            }
            Some(Err(error)) => {
                self.cancel().await?;
                Err(JoinError::Media(error))
            }
            None => {
                self.cancel().await?;
                Err(JoinError::Cancelled)
            }
        }
    }

    pub async fn acknowledge_and_connect(
        &mut self,
        accepted: AcceptedMedia,
        io: ost::calling::media::PcmAudioIo,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<(), JoinError> {
        self.acknowledge(&accepted, cancel).await?;
        self.connect_media(accepted, io, cancel).await
    }

    pub async fn stop_video(&mut self) {
        if let Some(control)=&self.video_control {control.set_enabled(false);}
        if let Some(mut session)=self.video_session.take(){session.stop().await;}
    }
    pub async fn cancel(&mut self) -> Result<LeaveOutcome, JoinError> {
        self.stop_video().await;
        if let Some(audio)=self.audio.take(){audio.close().await;}
        if let Some(url) = self.call_leg.clone() {
            self.session.note_call_leg(url);
        }
        if let Some(url) = self.update_media_descriptions.clone() {
            self.session.note_update_media(url);
        }
        if self.session.has_attached_media() {
            self.session
                .terminate(&self.key)
                .await
                .map_err(|_| JoinError::InvalidState)
        } else {
            self.session
                .cancel_outgoing(&self.key)
                .await
                .map_err(|_| JoinError::InvalidState)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};
    use tiny_http::{Header, Response, Server};

    #[test]
    fn camera_offer_uses_the_connected_renegotiation_shape() {
        let sender = LocalParticipant {
            id: "8:orgid:aaaaaaaa-1111-2222-3333-444444444444".into(),
            display_name: "Fixture".into(),
            endpoint_id: "endpoint".into(),
            participant_id: "participant".into(),
            language_id: "pt-BR".into(),
        };
        let body = camera_offer_payload(&sender, "v=0\r\n", "https://fixture.example/answer", "https://fixture.example/reject");
        assert!(body.get("payload").is_none());
        assert_eq!(body.pointer("/mediaNegotiation/callModalities").unwrap(), &json!(["Audio", "Video"]));
        assert_eq!(body.pointer("/mediaNegotiation/mediaContent/blob").unwrap(), "v=0\r\n");
        assert!(body.pointer("/mediaNegotiation/links/mediaAnswer").unwrap().is_string());
        assert!(body.pointer("/mediaNegotiation/links/rejection").unwrap().is_string());
        let ack = json!({"callAcceptanceAcknowledgement":{"links":{"mediaRenegotiation":"https://fixture.example/renegotiate"}}});
        assert_eq!(ack_renegotiation_link(&ack), Some("https://fixture.example/renegotiate"));
        assert_eq!(ack_renegotiation_link(&json!({})), None);
        let accepted = json!({"callAcceptance":{"links":{"mediaRenegotiation":"https://fixture.example/from-acceptance"}}});
        assert_eq!(acceptance_renegotiation_link(&accepted), Some("https://fixture.example/from-acceptance"));
        assert_eq!(acceptance_renegotiation_link(&ack), None);
        let accepted_update = json!({"callAcceptance":{"links":{"updateMediaDescriptions":"https://fixture.example/update"}}});
        assert_eq!(acceptance_update_media_link(&accepted_update), Some("https://fixture.example/update"));
        assert_eq!(acceptance_update_media_link(&accepted), None);
        let ack_update = json!({"callAcceptanceAcknowledgement":{"links":{"updateMediaDescriptions":"https://fixture.example/update-ack"}}});
        assert_eq!(ack_update_media_link(&ack_update), Some("https://fixture.example/update-ack"));
        assert_eq!(ack_update_media_link(&json!({})), None);
        assert_eq!(keep_or_replace_link(Some("https://fixture.example/update".into()), None).as_deref(), Some("https://fixture.example/update"));
        let acknowledgement = mixer_leg_links(&json!({
            "mediaAcknowledgement": {"links": {
                "updateMediaDescriptions": "https://fixture.example/leg2/update",
                "applyChannelParameters": "https://fixture.example/leg2/apply",
                "callLeg": "https://fixture.example/leg2"
            }}
        }));
        assert_eq!(acknowledgement.update_media_descriptions.as_deref(), Some("https://fixture.example/leg2/update"));
        assert_eq!(acknowledgement.apply_channel_parameters.as_deref(), Some("https://fixture.example/leg2/apply"));
        assert_eq!(acknowledgement.call_leg.as_deref(), Some("https://fixture.example/leg2"));
        let beside = mixer_leg_links(&json!({
            "mediaRenegotiation": {"links": {"mediaAnswer": "https://fixture.example/answer"}},
            "links": {
                "updateMediaDescriptions": {"href": "https://fixture.example/new/update"},
                "callLeg": "https://fixture.example/new/leg"
            }
        }));
        assert_eq!(beside.update_media_descriptions.as_deref(), Some("https://fixture.example/new/update"));
        assert_eq!(beside.call_leg.as_deref(), Some("https://fixture.example/new/leg"));
        assert_eq!(beside.media_renegotiation, None);
        let nested = mixer_leg_links(&json!({
            "links": {"callLeg": "https://fixture.example/old"},
            "mediaAcknowledgement": {"links": {"callLeg": "https://fixture.example/new"}}
        }));
        assert_eq!(nested.call_leg.as_deref(), Some("https://fixture.example/new"));
        assert_eq!(mixer_leg_links(&json!({})), MixerLegLinks::default());
        assert_eq!(mixer_leg_links(&json!({"links": {"callLeg": ""}})).call_leg, None);
        assert!(ack_renegotiation_link(&json!({})).is_none());
        let joined = camera_offer_sdp("v=0\r\nm=audio 1 RTP/SAVP 0", "m=video 2 RTP/SAVP 122\r\n");
        assert!(joined.contains("RTP/SAVP 0\r\nm=video 2 RTP/SAVP 122\r\n"), "{joined:?}");
        assert!(matches!(camera_offer_reply(&Value::Null), CameraOfferReply::Wait));
        assert!(matches!(camera_offer_reply(&json!({"code":0})), CameraOfferReply::Wait));
        assert!(matches!(camera_offer_reply(&json!({"code":488})), CameraOfferReply::Rejected));
        match camera_offer_reply(&json!({"mediaAnswer":{"mediaContent":{"contentType":"application/sdp","blob":"v=0\r\n"}}})) {
            CameraOfferReply::Answer { content_type, blob, acknowledgement } => {
                assert_eq!(content_type, "application/sdp");
                assert_eq!(blob, "v=0\r\n");
                assert!(acknowledgement.is_none());
            }
            other => panic!("expected the inlined answer, got {other:?}"),
        }
        assert_eq!(keep_or_replace_link(Some("from-acceptance".into()), None).as_deref(), Some("from-acceptance"));
        assert_eq!(keep_or_replace_link(Some("from-acceptance".into()), Some("from-ack".into())).as_deref(), Some("from-ack"));
        assert_eq!(keep_or_replace_link(None, None), None);
    }

    fn context() -> CallContext {
        CallContext::from_connected_event(
            &json!({"messagingRoot":"https://fixture.trouter.skype.com/v4/opaque/",
            "transportEndpointId":"fixture-endpoint","ttlSeconds":3600})
            .to_string(),
        )
        .unwrap()
    }
    fn parameters() -> StartMeeting {
        let key = CallKey {
            generation: 1,
            id: "fixture-call".into(),
        };
        let context = context();
        StartMeeting {
            callbacks: context.prepare_callbacks(key.clone()).unwrap(),
            context,
            key,
            identity: LocalParticipant {
                id: "8:orgid:fixture-person".into(),
                display_name: "Pessoa fictícia".into(),
                endpoint_id: "fixture-endpoint".into(),
                participant_id: "fixture-participant".into(),
                language_id: "pt-BR".into(),
            },
            invitation: JoinLink::parse("https://teams.microsoft.com/meet/fixture?p=fake-passcode")
                .unwrap(),
            muted: false,
        }
    }
    async fn audio() -> PreparedAudio {
        PreparedAudio::bind(
            "127.0.0.1:0".parse().unwrap(),
            &["127.0.0.1".parse().unwrap()],
        )
        .await
        .unwrap()
    }
    fn io() -> (
        ost::calling::media::PcmAudioIo,
        mpsc::SyncSender<Vec<i16>>,
        mpsc::Receiver<Vec<i16>>,
    ) {
        let (mic_tx, mic_rx) = mpsc::sync_channel(4);
        let (speaker_tx, speaker_rx) = mpsc::sync_channel(4);
        (
            ost::calling::media::PcmAudioIo {
                microphone: mic_rx,
                speaker: speaker_tx,
                initially_muted: false,
            },
            mic_tx,
            speaker_rx,
        )
    }
    fn response(base: &str) -> Value {
        json!({"sequenceNumber":1,"conversationController":format!("{base}/controller"),"links":{"leave":format!("{base}/leave")}})
    }
    fn body(request: &mut tiny_http::Request) -> Value {
        let mut text = String::new();
        request.as_reader().read_to_string(&mut text).unwrap();
        serde_json::from_str(&text).unwrap()
    }
    fn recv(server: &Server) -> tiny_http::Request {
        server
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .expect("expected signaling request")
    }
    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        crate::graph::runtime().unwrap().block_on(future)
    }

    #[test]
    fn meeting_placeholders_reuse_the_audio_port_without_candidates() {
        let audio = "v=0\r\nm=audio 40000 RTP/SAVP 0\r\na=sendrecv\r\na=ice-ufrag:fixtureufrag\r\na=ice-pwd:fixturepwd\r\na=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:fixture|2^31\r\na=candidate:x 1 UDP 1 127.0.0.1 40000 typ host\r\n";
        let offer = meeting_placeholder_sections(audio).unwrap();
        assert!(offer.contains("a=group:BUNDLE 0 1 2 3 4 5 6 7 8 9 10 11 12\r\nm=audio "));
        assert!(offer.contains("a=mid:0\r\n"));
        assert!(offer.contains("m=video 40000 RTP/SAVP 122\r\na=mid:1\r\na=sendrecv\r\n"));
        assert!(offer.contains("m=video 40000 RTP/SAVP 122\r\na=mid:7\r\na=recvonly\r\n"));
        assert!(offer.contains("m=video 40000 RTP/SAVP 122\r\na=mid:11\r\na=sendrecv\r\n"));
        assert!(offer.contains("m=x-data 40000 RTP/SAVP 127\r\na=mid:12\r\na=sendrecv\r\n"));
        assert!(offer.contains("a=ice-ufrag:fixtureufrag\r\n"));
        assert_eq!(offer.lines().filter(|line| line.starts_with("m=video ")).count(), 11);
        let video = offer.split_once("m=video ").unwrap().1;
        assert!(!video.contains("a=candidate:"));
        assert!(!video.contains("c=IN "));
        let media = meeting_media(audio).unwrap();
        assert_eq!(media.content_type, "application/sdp-ngc-1.0");
        assert_eq!(media.video_sections, 11);
        assert!(meeting_placeholder_sections("v=0\r\n").is_none());
    }

    #[cfg(feature = "libwebrtc-av")]
    #[test]
    fn webrtc_meeting_offers_audio_and_camera_without_claiming_camera_is_on() {
        run(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let worker_base = base.clone();
            let worker = std::thread::spawn(move || {
                let mut create = recv(&server);
                let body = body(&mut create);
                let invitation = &body["callInvitation"];
                assert_eq!(invitation["callModalities"], json!(["Audio"]));
                assert_eq!(body["endpointCapabilities"], 73463);
                assert_eq!(body["clientEndpointCapabilities"], 47150826);
                assert_eq!(body["endpointMetadata"]["holographicCapabilities"], 3);
                assert!(invitation["clientContentForMediaController"]["controlVideoStreaming"].is_string());
                assert_eq!(invitation["mediaContent"]["contentType"], "application/sdp-ngc-1.0");
                assert_eq!(invitation["mediaContent"]["mediaDescriptions"]["descriptions"], json!([]));
                assert_eq!(invitation["mediaContent"]["mediaDescriptions"]["requestId"], 1);
                let offer = invitation["mediaContent"]["blob"].as_str().unwrap();
                let parsed = teams_av::sdp::parse(offer).unwrap();
                assert!(parsed.media.iter().any(|line| line.kind == "audio"));
                assert!(parsed.media.iter().any(|line| line.kind == "video" && line.value("label") == Some("main-video")));
                let data = parsed.media.last().unwrap();
                assert_eq!(data.kind, "x-data");
                assert_eq!(data.mid(), Some("12"));
                assert_eq!(data.value("x-data-protocol"), Some("sctp"));
                let camera = parsed.media.iter().find(|line| line.value("label") == Some("main-video")).unwrap();
                let range = camera.value("x-ssrc-range").unwrap();
                let (first, last) = range.split_once('-').unwrap();
                assert_eq!(last.parse::<u32>().unwrap() - first.parse::<u32>().unwrap(), 99);
                assert!(parsed.session.attributes.iter().any(|attr| attr.name == "group"));
                create.respond(Response::from_string(response(&worker_base).to_string())).unwrap();
                recv(&server).respond(Response::empty(204)).unwrap();
            });
            let engine = teams_av::engine::Engine::prepare().await.unwrap();
            let (_stop, cancel) = watch::channel(false);
            let result = start_meeting(CallRoute::fixture(&base), CallTransport::fixture(&base),
                parameters(), PreparedMedia::WebRtc(engine), cancel).await.unwrap();
            let StartResult::Waiting(mut waiting) = result else { panic!("expected waiting") };
            waiting.cancel().await.unwrap();
            worker.join().unwrap();
        });
    }

    #[test]
    fn meeting_resolution_precedes_media_and_seeds_the_existing_roster() {
        run(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let worker_base = base.clone();
            let worker = std::thread::spawn(move || {
                let mut first = recv(&server);
                assert_eq!(first.url(), "/epconv");
                let create = body(&mut first);
                assert_eq!(create["groupChat"]["threadId"], "19:fixture@thread.v2");
                assert_eq!(create["groupChat"]["messageId"], "0");
                assert_eq!(create["meetingInfo"], json!({"tenantId":"11111111-1111-4111-8111-111111111111","organizerId":"22222222-2222-4222-8222-222222222222"}));
                assert!(create.get("meetingData").is_none());
                assert!(create.get("callInvitation").is_none());
                assert!(create["conversationRequest"].get("suppressDialout").is_none());
                assert!(create["conversationRequest"].get("conversationType").is_none());
                assert!(create["participants"].get("to").is_none());
                assert_eq!(create["endpointCapabilities"], 1 | 4 | 32 | 64 | 1024);
                let mut resolved = response(&worker_base);
                resolved["conversationController"] = json!(format!("{worker_base}/controller?route=fixture"));
                resolved["activeModalities"] = json!({"groupChat":create["groupChat"]});
                resolved["meetingInfo"] = create["meetingInfo"].clone();
                resolved["meetingData"] = json!({"meetingCode":"1234567890123456","passcode":"fixture8"});
                resolved["subject"] = json!("Reunião fictícia");
                resolved["roster"] = json!({"type":"Delta","participants":{
                    "8:orgid:other-fixture":{"details":{"displayName":"Pessoa já presente"}}
                }});
                first.respond(Response::from_string(resolved.to_string())).unwrap();
                let mut second = recv(&server);
                assert_eq!(second.url(), "/controller?route=fixture");
                let join = body(&mut second);
                assert_eq!(join["participants"]["from"], create["participants"]["from"]);
                assert_eq!(join["endpointCapabilities"], create["endpointCapabilities"]);
                assert_eq!(join["participants"]["to"], json!([]));
                assert_eq!(join["meetingInfo"], resolved["meetingInfo"]);
                assert_eq!(join["meetingData"], resolved["meetingData"]);
                assert_eq!(join["groupChat"], resolved["activeModalities"]["groupChat"]);
                assert_eq!(join["conversationRequest"]["subject"], resolved["subject"]);
                assert_eq!(join["conversationRequest"]["suppressDialout"], true);
                assert_eq!(join["callInvitation"]["callModalities"], json!(["Audio"]));
                let blob = join["callInvitation"]["mediaContent"]["blob"].as_str().unwrap();
                assert!(blob.starts_with("v=0"));
                assert!(!blob.contains("m=video "));
                second.respond(Response::empty(204)).unwrap();
                let leave = recv(&server);
                assert_eq!(leave.url(), "/leave");
                leave.respond(Response::empty(204)).unwrap();
            });
            let mut start = parameters();
            let context = json!({"Tid":"11111111-1111-4111-8111-111111111111","Oid":"22222222-2222-4222-8222-222222222222"}).to_string();
            start.invitation = JoinLink::parse(&format!("https://teams.microsoft.com/l/meetup-join/19%3Afixture%40thread.v2/0?context={}",urlencoding::encode(&context))).unwrap();
            let (_stop, cancel) = watch::channel(false);
            let result = start_meeting(CallRoute::fixture(&base), CallTransport::fixture(&base), start, audio().await, cancel).await.unwrap();
            let StartResult::Waiting(mut waiting) = result else { panic!("expected callback wait"); };
            let faces = waiting.initial_roster.as_ref().unwrap().faces();
            assert_eq!(faces.len(), 1);
            assert_eq!(faces[0].label, "Pessoa já presente");
            waiting.cancel().await.unwrap();
            worker.join().unwrap();
        });
    }

    #[test]
    fn meeting_resolution_cannot_switch_the_requested_thread() {
        run(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let worker_base = base.clone();
            let worker = std::thread::spawn(move || {
                let first = recv(&server);
                let mut resolved = response(&worker_base);
                resolved["activeModalities"] = json!({"groupChat":{"threadId":"19:wrong@thread.v2","messageId":"0"}});
                first.respond(Response::from_string(resolved.to_string())).unwrap();
                let request = recv(&server);
                assert_eq!(request.url(), "/leave");
                request.respond(Response::empty(204)).unwrap();
            });
            let mut start = parameters();
            start.invitation = JoinLink::parse("https://teams.microsoft.com/l/meetup-join/19%3Afixture%40thread.v2/0").unwrap();
            let (_stop, cancel) = watch::channel(false);
            let result = start_meeting(CallRoute::fixture(&base), CallTransport::fixture(&base), start, audio().await, cancel).await;
            assert!(matches!(result, Err(JoinError::InvalidController)));
            worker.join().unwrap();
        });
    }

    #[test]
    fn rejected_second_phase_releases_the_created_conversation() {
        run(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let worker_base = base.clone();
            let worker = std::thread::spawn(move || {
                recv(&server).respond(Response::from_string(response(&worker_base).to_string())).unwrap();
                let request = recv(&server);
                assert_eq!(request.url(), "/controller");
                request.respond(Response::empty(403)).unwrap();
                let request = recv(&server);
                assert_eq!(request.url(), "/leave");
                request.respond(Response::empty(204)).unwrap();
            });
            let mut start = parameters();
            start.invitation = JoinLink::chat("19:fixture@thread.v2", None).unwrap();
            let (_stop, cancel) = watch::channel(false);
            let result = start_meeting(CallRoute::fixture(&base), CallTransport::fixture(&base), start, audio().await, cancel).await;
            assert!(matches!(result, Err(JoinError::Control(ControlError::Http(403)))));
            worker.join().unwrap();
        });
    }

    #[test]
    fn direct_audio_call_invites_only_the_validated_peer_and_preserves_controller_query() {
        run(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}",server.server_addr());
            let target = "8:orgid:aaaaaaaa-1111-2222-3333-444444444444";
            let worker_base = base.clone();
            let worker = std::thread::spawn(move || {
                let mut req = recv(&server);
                assert_eq!(req.url(),"/epconv");
                let create = body(&mut req);
                assert_eq!(create["endpointCapabilities"].as_u64().unwrap() & 32, 0);
                assert_eq!(create["conversationRequest"]["suppressDialout"],false);
                assert_eq!(create["participants"]["to"].as_array().unwrap().len(),1);
                assert_eq!(create["participants"]["to"][0]["id"],target);
                assert!(create["participants"]["to"][0]["participantId"].as_str().is_some_and(|id| !id.is_empty()));
                assert_eq!(create["callInvitation"]["callModalities"],json!(["Audio"]));
                assert!(create.get("meetingData").is_none());
                let mut reply = response(&worker_base);
                reply["conversationController"] = json!(format!("{worker_base}/controller?route=fixture"));
                reply["links"]["addParticipant"] = json!(format!("{worker_base}/different-operation"));
                req.respond(Response::from_string(reply.to_string())).unwrap();
                let mut req = recv(&server);
                assert_eq!(req.url(),"/controller/add?route=fixture");
                let invite = body(&mut req);
                assert_eq!(invite["participants"]["to"].as_array().unwrap().len(),1);
                assert_eq!(invite["participants"]["to"][0]["id"],target);
                assert_eq!(invite["participantInvitationData"]["callModalities"],json!(["Audio"]));
                assert_eq!(invite["participantInvitationData"]["callDirection"],"Outgoing");
                assert!(invite["callInvitation"].get("links").is_none());
                req.respond(Response::empty(204)).unwrap();
                let req = recv(&server);
                assert_eq!(req.url(),"/leave");
                req.respond(Response::empty(204)).unwrap();
            });
            let mut start = parameters();
            start.invitation = JoinLink::chat("19:fixture@unq.gbl.spaces",Some(target)).unwrap();
            let (_stop,cancel) = watch::channel(false);
            let result = start_meeting(CallRoute::fixture(&base),CallTransport::fixture(&base),start,audio().await,cancel).await.unwrap();
            let StartResult::Waiting(mut waiting) = result else {panic!("expected waiting");};
            let (_stop, ring_cancel) = watch::channel(false);
            let stale = json!({"links":{"addParticipant": format!("{base}/different-operation")}});
            assert!(!waiting.ring(Some(&stale), ring_cancel).await.unwrap());
            waiting.cancel().await.unwrap();
            worker.join().unwrap();
        });
    }

    #[test]
    fn direct_ring_posts_the_update_add_participant_link() {
        run(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let worker_base = base.clone();
            let worker = std::thread::spawn(move || {
                let req = recv(&server);
                req.respond(Response::from_string(response(&worker_base).to_string())).unwrap();
                let req = recv(&server);
                assert_eq!(req.url(), "/controller/addParticipant?from=update");
                req.respond(Response::empty(204)).unwrap();
                let req = recv(&server);
                assert_eq!(req.url(), "/leave");
                req.respond(Response::empty(204)).unwrap();
            });
            let mut start = parameters();
            start.invitation = JoinLink::chat("19:fixture@unq.gbl.spaces", Some("aaaaaaaa-1111-2222-3333-444444444444")).unwrap();
            let (_stop, cancel) = watch::channel(false);
            let StartResult::Waiting(mut waiting) = start_meeting(CallRoute::fixture(&base), CallTransport::fixture(&base), start, audio().await, cancel).await.unwrap() else { panic!("expected waiting"); };
            let (_stop, ring_cancel) = watch::channel(false);
            let update = json!({"links":{"addParticipant": format!("{base}/controller/addParticipant?from=update")}});
            assert!(!waiting.ring(Some(&update), ring_cancel).await.unwrap());
            waiting.cancel().await.unwrap();
            worker.join().unwrap();
        });
    }

    #[test]
    fn invalid_controller_response_releases_prepared_relay_with_a_stale_nonce(){
        run(async{
            let relay=crate::call_turn::tests::fixture_stale_delete().await;
            let allocation=crate::call_turn::Allocation::allocate(relay.server,crate::call_turn::Credentials::new("fixture-user","fixture-secret","rtcmedia").unwrap()).await.unwrap();let relay_base=allocation.base_addr().unwrap();
            let mut offered=audio().await;let local=offered.local_addr().unwrap();offered.use_relay(allocation).unwrap();
            let server=Server::http("127.0.0.1:0").unwrap();let base=format!("http://{}",server.server_addr());
            let worker=std::thread::spawn(move||{let req=recv(&server);req.respond(Response::from_string(json!({"conversationController":"https://evil.example/controller","links":{"leave":"https://evil.example/leave"}}).to_string())).unwrap();});
            let(_stop,cancel)=watch::channel(false);
            let result=start_meeting(CallRoute::fixture(&base),CallTransport::fixture(&base),parameters(),offered,cancel).await;
            assert!(matches!(result,Err(JoinError::InvalidController)));
            tokio::time::timeout(Duration::from_secs(1),relay.released).await.unwrap().unwrap();relay.worker.await.unwrap();worker.join().unwrap();assert_eq!(relay.challenges.load(std::sync::atomic::Ordering::SeqCst),1);
            let _host=tokio::net::UdpSocket::bind(local).await.unwrap();let _relay=tokio::net::UdpSocket::bind(relay_base).await.unwrap();
        });
    }

    #[test]
    fn controller_ack_and_pcm_media_form_one_owned_native_session_until_hangup() {
        run(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let params = parameters();
            let acceptance_address = params.callbacks.link(CallbackKind::Acceptance).to_owned();
            let context = params.context.clone();
            let left = audio().await;
            let left_addr = left.local_addr().unwrap();
            let left_offer = left.offer().to_owned();
            let expected_offer = left_offer.clone();
            let reply = response(&base);
            let worker = std::thread::spawn(move || {
                let mut create = recv(&server);
                assert_eq!(create.url(), "/epconv");
                let payload = body(&mut create);
                assert!(payload.get("payload").is_none());
                assert_eq!(
                    payload
                        .pointer("/participants/to")
                        .unwrap()
                        .as_array()
                        .unwrap()
                        .len(),
                    0
                );
                assert_eq!(
                    payload
                        .pointer("/conversationRequest/suppressDialout")
                        .unwrap(),
                    true
                );
                assert_eq!(
                    payload.pointer("/callInvitation/callModalities").unwrap(),
                    &json!(["audio"])
                );
                assert_eq!(
                    payload
                        .pointer("/callInvitation/mediaContent/blob")
                        .unwrap(),
                    &expected_offer
                );
                assert_eq!(
                    payload.pointer("/meetingData/meetingUrl").unwrap(),
                    "https://teams.microsoft.com/meet/fixture?p=fake-passcode"
                );
                assert_eq!(payload["endpointCapabilities"], 1093);
                assert!(payload.get("client_id").is_none());
                let chain = create
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("x-microsoft-skype-chain-id"))
                    .unwrap()
                    .value
                    .to_string();
                assert_eq!(chain.len(), 36);
                create
                    .respond(Response::from_string(reply.to_string()))
                    .unwrap();
                let mut ack = recv(&server);
                assert_eq!(ack.url(), "/ack");
                let payload = body(&mut ack);
                assert_eq!(
                    payload
                        .pointer("/callAcceptanceAcknowledgement/links")
                        .unwrap()
                        .as_object()
                        .unwrap()
                        .len(),
                    7
                );
                assert_eq!(
                    ack.headers()
                        .iter()
                        .find(|h| h.field.equiv("x-microsoft-skype-chain-id"))
                        .unwrap()
                        .value
                        .as_str(),
                    chain
                );
                ack.respond(Response::empty(204)).unwrap();
                let mut end = recv(&server);
                assert_eq!(end.url(), "/leave");
                assert_eq!(
                    body(&mut end).pointer("/callTransactionEnd/code").unwrap(),
                    0
                );
                let _rebound = std::net::UdpSocket::bind(left_addr)
                    .expect("media port held when leave reached server");
                end.respond(Response::empty(200)).unwrap();
            });
            let (cancel_tx, mut cancel_rx) = watch::channel(false);
            let StartResult::Waiting(mut waiting) = start_meeting(
                CallRoute::fixture(&base),
                CallTransport::fixture(&base),
                params,
                left,
                cancel_rx.clone(),
            )
            .await
            .unwrap() else {
                panic!("unexpected cancellation")
            };
            assert!(
                waiting.session.audio_controls(&waiting.key).is_none(),
                "HTTP response alone is not a connected call"
            );
            let right = audio().await;
            let right_offer = right.offer().to_owned();
            let acceptance =
                json!({"callAcceptance":{"links":{"acknowledgement":format!("{base}/ack")},
                "mediaContent":{"contentType":"application/sdp","blob":right_offer}}})
                .to_string();
            for interval in [json!(0),json!(-1),json!(0.01),json!(3601),json!("1")] {
                let invalid=json!({"callAcceptance":{"links":{"acknowledgement":format!("{base}/ack"),"callLeg":format!("{base}/leg")},"callKeepAliveInterval":interval,"mediaContent":{"contentType":"application/sdp","blob":right_offer}}});
                assert!(waiting.acceptance(&context,&acceptance_address,&serde_json::to_vec(&invalid).unwrap()).is_err());
            }
            let mut accepted_control=json!({"callAcceptance":{"links":{"acknowledgement":format!("{base}/ack"),"callLeg":format!("{base}/leg")},"callKeepAliveInterval":10,"mediaContent":{"contentType":"application/sdp","blob":right_offer}}});
            let checked=waiting.acceptance(&context,&acceptance_address,&serde_json::to_vec(&accepted_control).unwrap()).unwrap();
            assert_eq!(checked.keepalive.unwrap().interval,Duration::from_secs(9));
            accepted_control["callAcceptance"]["links"]["callLeg"]=json!("https://evil.example/leg");
            assert!(waiting.acceptance(&context,&acceptance_address,&serde_json::to_vec(&accepted_control).unwrap()).is_err());
            assert_eq!(
                waiting
                    .acceptance(
                        &context,
                        &acceptance_address.replace("/call/acceptance/", "/call/end/"),
                        acceptance.as_bytes()
                    )
                    .unwrap_err(),
                JoinError::StaleTransport
            );
            let accepted = waiting
                .acceptance(&context, &acceptance_address, acceptance.as_bytes())
                .unwrap();
            let (left_io, left_mic, _left_speaker) = io();
            let (right_io, _right_mic, right_speaker) = io();
            left_mic.send(vec![9000; 160]).unwrap();
            let (connected, right) = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(
                    waiting.acknowledge_and_connect(accepted, left_io, &mut cancel_rx),
                    right.connect(
                        RemoteAudio::parse("application/sdp", &left_offer).unwrap(),
                        right_io,
                        false
                    )
                )
            })
            .await
            .unwrap();
            connected.unwrap();
            let mut right = right.unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if right_speaker
                        .try_iter()
                        .any(|samples| samples.iter().any(|sample| *sample > 500))
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            assert!(waiting.session.audio_controls(&waiting.key).is_some());
            assert_eq!(waiting.cancel().await.unwrap(), LeaveOutcome::Confirmed);
            right.stop().await;
            worker.join().unwrap();
            drop(cancel_tx);
        });
    }

    #[test]
    fn cancel_while_create_is_pending_releases_socket_before_late_response_then_leaves_once() {
        run(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let prepared = audio().await;
            let address = prepared.local_addr().unwrap();
            let reply = response(&base);
            let (seen, received) = iced::futures::channel::oneshot::channel();
            let (release, held) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let req = recv(&server);
                seen.send(()).unwrap();
                held.recv_timeout(Duration::from_secs(3)).unwrap();
                req.respond(Response::from_string(reply.to_string()))
                    .unwrap();
                let mut leave = recv(&server);
                assert_eq!(leave.url(), "/leave");
                assert_eq!(
                    body(&mut leave)
                        .pointer("/callTransactionEnd/code")
                        .unwrap(),
                    487
                );
                leave.respond(Response::empty(200)).unwrap();
                assert!(
                    server
                        .recv_timeout(Duration::from_millis(50))
                        .unwrap()
                        .is_none()
                );
            });
            let (cancel, rx) = watch::channel(false);
            let job = tokio::spawn(start_meeting(
                CallRoute::fixture(&base),
                CallTransport::fixture(&base),
                parameters(),
                prepared,
                rx,
            ));
            tokio::time::timeout(Duration::from_secs(2), received)
                .await
                .unwrap()
                .unwrap();
            cancel.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Ok(socket) = tokio::net::UdpSocket::bind(address).await {
                        drop(socket);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("cancelled create retained its offered socket");
            release.send(()).unwrap();
            assert!(matches!(
                job.await.unwrap().unwrap(),
                StartResult::Cancelled(Some(LeaveOutcome::Confirmed))
            ));
            worker.join().unwrap();
        });
    }

    #[test]
    fn consent_or_captcha_cannot_be_silently_skipped_after_controller_creation() {
        run(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let mut reply = response(&base);
            reply["selfParticipant"] = json!({"enforceConsentToJoin":true});
            let worker = std::thread::spawn(move || {
                recv(&server)
                    .respond(Response::from_string(reply.to_string()))
                    .unwrap();
                let mut leave = recv(&server);
                assert_eq!(
                    body(&mut leave)
                        .pointer("/callTransactionEnd/code")
                        .unwrap(),
                    487
                );
                leave.respond(Response::empty(200)).unwrap();
            });
            let (_cancel, rx) = watch::channel(false);
            assert!(matches!(
                start_meeting(
                    CallRoute::fixture(&base),
                    CallTransport::fixture(&base),
                    parameters(),
                    audio().await,
                    rx
                )
                .await,
                Err(JoinError::InteractionRequired)
            ));
            worker.join().unwrap();
        });
    }

    #[test]
    fn pre_cancel_and_redirect_never_start_media_or_forward_credentials() {
        run(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let (_cancel, rx) = watch::channel(true);
            assert!(matches!(
                start_meeting(
                    CallRoute::fixture(&base),
                    CallTransport::fixture(&base),
                    parameters(),
                    audio().await,
                    rx
                )
                .await
                .unwrap(),
                StartResult::Cancelled(None)
            ));
            assert!(
                server
                    .recv_timeout(Duration::from_millis(50))
                    .unwrap()
                    .is_none()
            );
            let sink = Server::http("127.0.0.1:0").unwrap();
            let target = format!("http://{}/stolen", sink.server_addr());
            let worker = std::thread::spawn(move || {
                recv(&server)
                    .respond(
                        Response::empty(302)
                            .with_header(Header::from_bytes("Location", target).unwrap()),
                    )
                    .unwrap();
            });
            let (_cancel, rx) = watch::channel(false);
            assert!(matches!(
                start_meeting(
                    CallRoute::fixture(&base),
                    CallTransport::fixture(&base),
                    parameters(),
                    audio().await,
                    rx
                )
                .await,
                Err(JoinError::Control(ControlError::Http(302)))
            ));
            worker.join().unwrap();
            assert!(
                sink.recv_timeout(Duration::from_millis(50))
                    .unwrap()
                    .is_none()
            );
        });
    }

    #[test]
    fn routing_uses_discovery_and_does_not_invent_cloud_region_or_partition() {
        let route=CallRoute::from_discovery(&json!({"calling_conversationServiceUrl":"https://fixture.ngc.msg.skype.com/api/v2/epconv/"})).unwrap();
        assert!(route.region.is_none());
        assert!(route.partition.is_none());
        assert!(route.ring.is_none());
        assert!(
            CallRoute::from_discovery(
                &json!({"calling_conversationServiceUrl":"https://evil.test/epconv"})
            )
            .is_err()
        );
        assert!(CallRoute::from_discovery(&json!({"calling_conversationServiceUrl":"https://fixture.ngc.msg.skype.com/epconv","partition":"bad\r\nvalue"})).is_err());
        assert!(!format!("{route:?}").contains("fixture"));
    }

    #[test]
    fn connected_renegotiation_answers_with_the_live_local_description() {
        run(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let worker_base = base.clone();
            let worker = std::thread::spawn(move || {
                let create = recv(&server);
                assert_eq!(create.url(), "/epconv");
                create
                    .respond(Response::from_string(response(&worker_base).to_string()))
                    .unwrap();
                let mut answer = recv(&server);
                assert_eq!(answer.url(), "/media-answer");
                let posted = body(&mut answer);
                assert_eq!(posted["mediaAnswer"]["callModalities"], json!(["Audio"]));
                assert_eq!(posted["mediaAnswer"]["sender"]["id"], "8:orgid:fixture-person");
                assert!(posted["mediaAnswer"]["links"]["mediaAcknowledgement"]
                    .as_str()
                    .unwrap()
                    .contains("/call/mediaAcknowledgement/"));
                assert!(posted["mediaAnswer"]["mediaContent"]["blob"]
                    .as_str()
                    .unwrap()
                    .starts_with("v=0"));
                assert!(posted.get("debugContent").is_none());
                assert!(posted["mediaAnswer"].get("clientContentForMediaController").is_none());
                answer.respond(Response::empty(204)).unwrap();
                let leave = recv(&server);
                assert_eq!(leave.url(), "/leave");
                leave.respond(Response::empty(204)).unwrap();
            });
            let (_stop, cancel) = watch::channel(false);
            let result = start_meeting(
                CallRoute::fixture(&base),
                CallTransport::fixture(&base),
                parameters(),
                audio().await,
                cancel,
            )
            .await
            .unwrap();
            let StartResult::Waiting(mut waiting) = result else {
                panic!("expected wait");
            };
            let remote = audio().await.offer().to_owned();
            // Model the agreement installed by a successful connect_media.
            waiting.active_agreement=Some(RemoteAudio::parse("application/sdp",&remote).unwrap().agreement());
            let offer = json!({
                "mediaNegotiation": {
                    "callModalities": ["Audio", "Video", "ScreenSharer"],
                    "links": {
                        "mediaAnswer": format!("{base}/media-answer"),
                        "rejection": format!("{base}/reject")
                    },
                    "mediaContent": {"contentType": "application/sdp", "blob": remote}
                }
            });
            let RenegotiationDecision::Send(request) = waiting.renegotiation_answer(&offer) else {
                panic!("expected answer");
            };
            assert_eq!(request.trace, 1);
            let mut changed=offer.clone();
            changed["mediaNegotiation"]["mediaContent"]["blob"]=json!(audio().await.offer());
            let RenegotiationDecision::Send(rejection)=waiting.renegotiation_answer(&changed) else {panic!("expected rejection");};
            assert_eq!(rejection.trace,2);
            assert!(!format!("{request:?}").contains("v=0"));
            assert_eq!(
                request.payload["mediaAnswer"]["mediaContent"]["blob"].as_str(),
                Some(waiting.local_description.as_str())
            );
            waiting
                .session
                .transport()
                .post_json(&request.url, &request.payload)
                .await
                .unwrap();
            let screen = json!({
                "mediaNegotiation": {
                    "callModalities": ["ScreenSharer"],
                    "links": {
                        "mediaAnswer": "https://evil.test/answer",
                        "rejection": format!("{base}/reject")
                    },
                    "mediaContent": {"contentType": "application/sdp", "blob": remote}
                }
            });
            let RenegotiationDecision::Send(rejection) = waiting.renegotiation_answer(&screen) else {
                panic!("expected rejection");
            };
            assert_eq!(rejection.trace, 2);
            assert!(rejection.url.ends_with("/reject"));
            assert_eq!(rejection.payload["mediaNegotiationFailure"]["code"], 488);
            assert_eq!(rejection.payload["mediaNegotiationFailure"]["subCode"], 3102);
            assert!(matches!(
                waiting.renegotiation_answer(&json!({"mediaNegotiation": {}})),
                RenegotiationDecision::Ignore
            ));
            let red = format!(
                "{}a=rtpmap:111 red/48000/2\r\n",
                remote.replace("RTP/SAVP 0", "RTP/SAVP 0 111")
            );
            let mut red_offer = offer.clone();
            red_offer["mediaNegotiation"]["mediaContent"]["blob"] = json!(red);
            let RenegotiationDecision::Send(red_answer) = waiting.renegotiation_answer(&red_offer) else {
                panic!("expected the RED extension to keep the current answer");
            };
            assert_eq!(red_answer.trace, 1);
            assert_eq!(
                red_answer.payload["mediaAnswer"]["mediaContent"]["blob"].as_str(),
                Some(waiting.local_description.as_str())
            );
            let event = format!(
                "{}a=rtpmap:111 telephone-event/8000\r\n",
                remote.replace("RTP/SAVP 0", "RTP/SAVP 0 111")
            );
            let mut event_offer = offer.clone();
            event_offer["mediaNegotiation"]["mediaContent"]["blob"] = json!(event);
            let RenegotiationDecision::Send(event_answer) = waiting.renegotiation_answer(&event_offer) else {
                panic!("expected an extra telephone-event payload to keep the current answer");
            };
            assert_eq!(event_answer.trace, 1);
            assert_eq!(
                event_answer.payload["mediaAnswer"]["mediaContent"]["blob"].as_str(),
                Some(waiting.local_description.as_str())
            );
            let catalogue = format!(
                "{}\r\na=rtpmap:111 SILK/16000\r\na=mid:audio\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nm=video 9 RTP/SAVP 122\r\n",
                remote.replace(
                    "RTP/SAVP 0",
                    "RTP/SAVP 0 111 104 114 9 103 8 116 115 106 113 118 119 97 101 120 121"
                )
            );
            let mut catalogue_offer = offer.clone();
            catalogue_offer["mediaNegotiation"]["mediaContent"]["blob"] = json!(catalogue);
            let RenegotiationDecision::Send(catalogue_answer) = waiting.renegotiation_answer(&catalogue_offer) else {
                panic!("expected the long codec catalogue to keep the current answer");
            };
            assert_eq!(catalogue_answer.trace, 1);
            let blob = catalogue_answer.payload["mediaAnswer"]["mediaContent"]["blob"]
                .as_str()
                .unwrap();
            assert!(blob.starts_with(waiting.local_description.trim_end_matches(['\r', '\n'])));
            assert!(blob.contains("m=application 0 UDP/DTLS/SCTP webrtc-datachannel\r\n"));
            assert!(blob.contains("m=video 0 RTP/SAVP 122\r\na=inactive\r\n"));
            assert!(!blob.contains("SILK/16000"));
            assert!(!blob.contains("m=application 9"));
            assert!(!blob.contains("m=video 9"));
            waiting.cancel().await.unwrap();
            worker.join().unwrap();
        });
    }
}
