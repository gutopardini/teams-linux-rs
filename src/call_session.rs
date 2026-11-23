//! Native conversation ownership and termination. Not a conference join API.
//!
//! Contracts: original calling-pluginless 26260.1717.5229.7049, `gw`,
//! `processConversationServiceUpdate`, `leaveConversation`, `lw`.
//! A session is created by the future join adapter, never by a user-pasted URL.
//! Credentials, callback links and participant identity are deliberately opaque
//! in Debug. No request in this module performs login or retries a claims grant.

use crate::auth::calling::{CallingToken, CallingTokenType};
use crate::call_context::{CallCallbacks, CallbackKind};
use crate::call_lifecycle::CallKey;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

const MAX_UPDATE_BYTES: usize = 64 * 1024;
const MAX_LINK_BYTES: usize = 16 * 1024;
const LEAVE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionError {
    InvalidService,
    InvalidCredential,
    InvalidIdentity,
    InvalidCall,
    InvalidUpdate,
    StaleCall,
    Ending,
    Transport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all="snake_case")]
pub(crate) enum ControlUpdateError {
    StaleCall, Ending, TooLarge, Format, Sequence,
    ControllerMissing, ControllerOrigin, ControllerPath,
    LeaveMissing, LeaveOrigin, EndpointControl,
}
impl ControlUpdateError {
    fn session_error(self) -> SessionError {
        match self {
            Self::StaleCall => SessionError::StaleCall,
            Self::Ending => SessionError::Ending,
            _ => SessionError::InvalidUpdate,
        }
    }
}

/// Only the origin returned by authenticated service discovery is pinned.
/// Response links are opaque paths on that origin, not guessed route strings.
#[derive(Clone)]
pub struct ServiceOrigin(Url);

impl std::fmt::Debug for ServiceOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServiceOrigin([REDACTED])")
    }
}

impl ServiceOrigin {
    pub fn from_discovery(value: &str) -> Result<Self, SessionError> {
        if value.len() > MAX_LINK_BYTES || value.chars().any(char::is_control) {
            return Err(SessionError::InvalidService);
        }
        let url = Url::parse(value).map_err(|_| SessionError::InvalidService)?;
        let host = url.host_str().ok_or(SessionError::InvalidService)?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port_or_known_default() != Some(443)
            || url.fragment().is_some()
            || url.query().is_some()
            || !["skype.com", "teams.microsoft.com", "teams.live.com"]
                .iter()
                .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        {
            return Err(SessionError::InvalidService);
        }
        Ok(Self(url))
    }

    pub(crate) fn link(&self, value: &str) -> Result<Url, SessionError> {
        if value.len() > MAX_LINK_BYTES || value.chars().any(char::is_control) {
            return Err(SessionError::InvalidUpdate);
        }
        let url = Url::parse(value).map_err(|_| SessionError::InvalidUpdate)?;
        if url.origin() != self.0.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.path() == "/"
        {
            return Err(SessionError::InvalidUpdate);
        }
        Ok(url)
    }

    fn controller(&self, value: &str) -> Result<Self, SessionError> {
        if self.link(value).is_ok() {
            return Ok(self.clone());
        }
        // Authenticated service responses may delegate within the Teams
        // namespace, with region/service labels chosen by the server. Do not
        // guess those labels or expand trust to arbitrary Microsoft/Skype URLs.
        let url = Url::parse(value).map_err(|_| SessionError::InvalidUpdate)?;
        let host = url.host_str().ok_or(SessionError::InvalidUpdate)?;
        if !["ngc.msg.skype.com", "teams.microsoft.com"]
            .iter()
            .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        {
            return Err(SessionError::InvalidUpdate);
        }
        if value.len() > MAX_LINK_BYTES || value.chars().any(char::is_control) {
            return Err(SessionError::InvalidUpdate);
        }
        // Authenticated response links may carry opaque query parameters.
        // Validate the delegated service without treating the link as a new
        // discovery URL. Preserve the original query when links are used.
        let mut service = url;
        service.set_query(None);
        Self::from_discovery(service.as_str()).map_err(|_| SessionError::InvalidUpdate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tiny_http::{Header, Response, Server};

    fn key() -> CallKey {
        CallKey {
            generation: 7,
            id: "fixture-call".into(),
        }
    }

    fn participant() -> LocalParticipant {
        LocalParticipant {
            id: "8:orgid:fixture".into(),
            display_name: "Pessoa fictícia".into(),
            endpoint_id: "fixture-endpoint".into(),
            participant_id: "fixture-participant".into(),
            language_id: "pt-BR".into(),
        }
    }

    fn transport(base: &str) -> CallTransport {
        // Only test code can use HTTP loopback. Production discovery validates
        // HTTPS and Microsoft service suffixes before constructing this type.
        CallTransport::with_headers(
            ServiceOrigin(Url::parse(base).unwrap()),
            credential_headers(CallingTokenType::Skype, "fixture-secret").unwrap(),
        )
        .unwrap()
    }

    fn update(base: &str, sequence: i64, end: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"sequenceNumber":sequence,
            "conversationController":format!("{base}/controller/fixture"),
            "links":{"leave":format!("{base}/{end}")}}))
        .unwrap()
    }

    fn session(base: &str, resources: CallResources) -> CallSession {
        CallSession::new(key(), participant(), transport(base), resources).unwrap()
    }

    #[test]
    fn server_unmute_http_body_is_unwrapped_and_empty_success_is_not_authority() {
        crate::graph::runtime().unwrap().block_on(async {
            let server=Server::http("127.0.0.1:0").unwrap();
            let base=format!("http://{}",server.server_addr());
            let worker=std::thread::spawn(move || {
                for empty in [false,true] {
                    let mut request=server.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
                    assert_eq!(request.url(),"/opaque?ticket=fixture");
                    let mut bytes=String::new();request.as_reader().read_to_string(&mut bytes).unwrap();
                    let body:Value=serde_json::from_str(&bytes).unwrap();
                    assert_eq!(body,json!({"from":participant(),"mediaTypes":["audio"]}));
                    if empty { request.respond(Response::empty(202)).unwrap(); }
                    else { request.respond(Response::from_string(json!({"muteUnmuteResponse":{"8:orgid:fixture":[{"participantId":"fixture-participant","reason":"success","serverMuteVersion":8}]}}).to_string())).unwrap(); }
                }
            });
            let mut call=session(&base,CallResources::default());
            let mut body:Value=serde_json::from_slice(&update(&base,1,"leave")).unwrap();
            body["links"]["unmute"]=json!(format!("{base}/opaque?ticket=fixture"));
            call.apply_conversation_update(&key(),&serde_json::to_vec(&body).unwrap()).unwrap();
            let (transport,url,payload)=call.server_unmute_request(&key()).unwrap();
            let response=transport.post_json(&url,&payload).await.unwrap();
            assert_eq!(response["muteUnmuteResponse"]["8:orgid:fixture"][0]["serverMuteVersion"],8);
            assert!(transport.post_json(&url,&payload).await.unwrap().is_null());
            worker.join().unwrap();
        });
    }

    #[test]
    fn server_unmute_uses_the_validated_opaque_link_and_own_identity() {
        let base="https://fixture.ngc.msg.skype.com";
        let mut call=session(base,CallResources::default());
        assert!(call.server_unmute_request(&key()).is_err());
        let mut body:Value=serde_json::from_slice(&update(base,1,"leave")).unwrap();
        let url=format!("{base}/opaque-action?ticket=fixture");
        body["links"]["unmute"]=json!(url);
        call.apply_conversation_update(&key(),&serde_json::to_vec(&body).unwrap()).unwrap();
        let (_,target,payload)=call.server_unmute_request(&key()).unwrap();
        assert_eq!(target,url);
        assert_eq!(payload,json!({"from":participant(),"mediaTypes":["audio"]}));
        let mut stale=key();stale.generation+=1;
        assert!(matches!(call.server_unmute_request(&stale),Err(SessionError::StaleCall)));
        body["sequenceNumber"]=json!(2);
        body["links"]["unmute"]=json!("https://outside.invalid/unmute");
        assert!(call.apply_conversation_update(&key(),&serde_json::to_vec(&body).unwrap()).is_err());
        assert_eq!(call.server_unmute_request(&key()).unwrap().1,url);
        body["links"]["unmute"]=json!(false);
        assert!(call.apply_conversation_update(&key(),&serde_json::to_vec(&body).unwrap()).is_err());
        call.ending=true;
        assert!(matches!(call.server_unmute_request(&key()),Err(SessionError::Ending)));
    }

    #[test]
    fn detailed_control_rejections_preserve_legacy_errors_and_committed_links() {
        let base="https://fixture.ngc.msg.skype.com";
        let mut call=session(base,CallResources::default());
        call.apply_conversation_update(&key(),&update(base,1,"committed-leave")).unwrap();
        let valid:Value=serde_json::from_slice(&update(base,2,"next-leave")).unwrap();
        for (pointer,replacement,expected) in [
            ("/sequenceNumber",json!("private"),ControlUpdateError::Sequence),
            ("/conversationController",Value::Null,ControlUpdateError::ControllerMissing),
            ("/conversationController",json!("https://outside.invalid/controller"),ControlUpdateError::ControllerOrigin),
            ("/conversationController",json!(base),ControlUpdateError::ControllerPath),
            ("/links/leave",Value::Null,ControlUpdateError::LeaveMissing),
            ("/links/leave",json!("https://else.ngc.msg.skype.com/leave"),ControlUpdateError::LeaveOrigin),
        ] {
            let mut body=valid.clone();*body.pointer_mut(pointer).unwrap()=replacement;
            let bytes=serde_json::to_vec(&body).unwrap();
            assert_eq!(call.apply_conversation_update_detailed(&key(),&bytes),Err(expected));
            assert_eq!(call.apply_conversation_update(&key(),&bytes),Err(SessionError::InvalidUpdate));
            assert_eq!(call.sequence,1);
            assert!(call.leave_link.as_ref().unwrap().path().ends_with("/committed-leave"));
        }
        let mut body=valid;body["links"]["updateEndpointState"]=json!(false);
        assert_eq!(call.apply_conversation_update_detailed(&key(),&serde_json::to_vec(&body).unwrap()),Err(ControlUpdateError::EndpointControl));
        assert_eq!(call.apply_conversation_update_detailed(&key(),b"[]"),Err(ControlUpdateError::Format));
        assert_eq!(call.apply_conversation_update_detailed(&key(),&vec![b' ';MAX_UPDATE_BYTES+1]),Err(ControlUpdateError::TooLarge));
        assert_eq!(call.sequence,1);
    }

    #[test]
    fn initial_negative_sequence_sentinel_is_valid_but_cannot_replace_a_newer_controller() {
        let base="https://fixture.ngc.msg.skype.com";
        for negative in [-1,-2,i64::MIN] {
            let mut call=session(base,CallResources::default());
            assert!(call.apply_conversation_update(&key(),&update(base,negative,"initial-leave")).unwrap());
            assert_eq!(call.sequence,-1);
            assert!(call.apply_conversation_update(&key(),&update(base,3,"new-leave")).unwrap());
            assert!(!call.apply_conversation_update(&key(),&update(base,negative,"old-leave")).unwrap());
            assert_eq!(call.sequence,3);
            assert!(call.leave_link.as_ref().unwrap().path().ends_with("/new-leave"));
            let invalid=serde_json::to_vec(&json!({"sequenceNumber":"-1","conversationController":format!("{base}/controller"),"links":{"leave":format!("{base}/leave")}})).unwrap();
            assert!(call.apply_conversation_update(&key(),&invalid).is_err());
            assert_eq!(call.sequence,3);
        }
    }

    #[test]
    fn authenticated_controller_delegation_is_limited_and_atomic() {
        let front="https://front.ngc.msg.skype.com";
        let transport=CallTransport::with_headers(ServiceOrigin::from_discovery(front).unwrap(),credential_headers(CallingTokenType::Skype,"fixture").unwrap()).unwrap();
        let mut call=CallSession::new(key(),participant(),transport,CallResources::default()).unwrap();
        let regional="https://region.ngc.msg.teams.microsoft.com";
        assert!(call.apply_conversation_update(&key(), &update(regional,1,"leave")).unwrap());
        assert_eq!(call.sequence,1);
        for invalid in ["https://api.asm.skype.com","https://evil.example","https://region.ngc.msg.skype.com.evil.example"] {
            assert!(call.apply_conversation_update(&key(), &update(invalid,2,"leave")).is_err());
            assert_eq!(call.sequence,1);
            assert_eq!(call.leave_link.as_ref().unwrap().host_str(),Some("region.ngc.msg.teams.microsoft.com"));
        }
        let inconsistent=serde_json::to_vec(&json!({"sequenceNumber":2,"conversationController":format!("{front}/controller"),"links":{"leave":format!("{regional}/leave")}})).unwrap();
        assert!(call.apply_conversation_update(&key(),&inconsistent).is_err());
        assert_eq!(call.sequence,1);
    }

    #[test]
    fn authenticated_teams_delegation_preserves_opaque_queries_and_rejects_url_confusion() {
        let front="https://front.ngc.msg.skype.com";
        for regional in ["https://calling.teams.microsoft.com", "https://region.calling.teams.microsoft.com",
            "https://region.ngc.msg.skype.com", "https://region.ngc.msg.teams.microsoft.com",
            "https://regional.service.teams.microsoft.com", "https://teams.microsoft.com"] {
            let mut call=session(front,CallResources::default());
            let mut body:Value=serde_json::from_slice(&update(regional,1,"leave")).unwrap();
            let leave=format!("{regional}/leave?opaque=a%2Fb&value=fixture");
            let control=format!("{regional}/endpoint?opaque=c%2Fd");
            body["conversationController"]=json!(format!("{regional}/conversation?opaque=x%2Fy"));
            body["links"]["leave"]=json!(leave);
            body["links"]["updateEndpointState"]=json!(control);
            assert_eq!(call.apply_conversation_update(&key(),&serde_json::to_vec(&body).unwrap()),Ok(true));
            assert_eq!(call.leave_link.as_ref().unwrap().as_str(),leave);
            assert_eq!(call.mute_request(&key(),true).unwrap().1,control);
            for invalid in ["https://calling.teams.microsoft.com.evil.test", "https://evilteams.microsoft.com",
                "https://graph.microsoft.com", "https://login.microsoftonline.com", "http://region.calling.teams.microsoft.com",
                "https://user@region.calling.teams.microsoft.com", "https://region.calling.teams.microsoft.com:8443",
                "https://region.calling.teams.microsoft.com/#fragment", "https://region.\ncalling.teams.microsoft.com"] {
                let mut bad:Value=serde_json::from_slice(&update(invalid,2,"leave")).unwrap();
                bad["conversationController"]=json!(format!("{invalid}/controller?opaque=fixture"));
                assert!(call.apply_conversation_update(&key(),&serde_json::to_vec(&bad).unwrap()).is_err(),"{invalid}");
                assert_eq!(call.sequence,1);
                assert_eq!(call.leave_link.as_ref().unwrap().as_str(),leave);
            }
            let mut mixed=body.clone();mixed["sequenceNumber"]=json!(2);
            mixed["links"]["leave"]=json!(format!("{front}/leave?opaque=fixture"));
            assert!(call.apply_conversation_update(&key(),&serde_json::to_vec(&mixed).unwrap()).is_err());
            assert_eq!(call.sequence,1);
        }
        // Discovery remains stricter: only authenticated response links can
        // include a query; no broadening of discovery URL validation.
        assert!(ServiceOrigin::from_discovery("https://calling.teams.microsoft.com/?opaque=fixture").is_err());
    }

    #[test]
    fn mute_commands_are_sequenced_bound_to_identity_and_discarded_on_controller_migration() {
        let base="https://amer.ngc.msg.skype.com";
        let mut call=session(base,CallResources::default());
        assert!(!call.mute_supported());
        let mut initial:Value=serde_json::from_slice(&update(base,1,"leave")).unwrap();
        initial["links"]["updateEndpointState"]=json!(format!("{base}/mute"));
        call.apply_conversation_update(&key(),&serde_json::to_vec(&initial).unwrap()).unwrap();
        assert!(call.mute_supported());
        let mut stale=key();stale.generation+=1;
        assert!(matches!(call.mute_request(&stale,false),Err(SessionError::StaleCall)));
        for (sequence,muted) in [(2,false),(3,true)] {
            let (_,url,payload)=call.mute_request(&key(),muted).unwrap();
            assert_eq!(url,format!("{base}/mute"));
            assert_eq!(payload,json!({"from":participant(),"endpointState":{"endpointStateSequenceNumber":sequence,"state":{"isMuted":muted}}}));
        }
        call.apply_conversation_update(&key(),&update(base,2,"leave")).unwrap();
        assert!(call.mute_supported(),"same-origin updates can omit unchanged links");
        call.apply_conversation_update(&key(),&update("https://region.ngc.msg.teams.microsoft.com",3,"leave")).unwrap();
        assert!(!call.mute_supported(),"never send credentials or mute to a stale controller");
    }

    #[test]
    fn discovery_and_links_pin_origin_and_reject_url_confusion() {
        let service =
            ServiceOrigin::from_discovery("https://amer.ngc.msg.skype.com/api/v2/epconv").unwrap();
        assert!(
            service
                .link("https://amer.ngc.msg.skype.com/opaque/leave?k=fixture")
                .is_ok()
        );
        for base in [
            "http://amer.ngc.msg.skype.com/",
            "https://skype.com.evil.test/",
            "https://evilskype.com/",
            "https://user@amer.ngc.msg.skype.com/",
            "https://amer.ngc.msg.skype.com:8443/",
            "https://amer.ngc.msg.skype.com/#x",
            "https://amer.ngc.msg.skype.com/?secret=x",
            "https://amer.\nngc.msg.skype.com/",
        ] {
            assert!(ServiceOrigin::from_discovery(base).is_err(), "{base}");
        }
        for link in [
            "https://other.ngc.msg.skype.com/leave",
            "https://evil.test/leave",
            "https://user@amer.ngc.msg.skype.com/leave",
            "//amer.ngc.msg.skype.com/leave",
            "https://amer.ngc.msg.skype.com/leave#fragment",
            "https://amer.ngc.msg.skype.com/",
        ] {
            assert!(service.link(link).is_err(), "{link}");
        }
    }

    #[test]
    fn tokens_are_sent_in_one_negotiated_header_and_redacted() {
        for kind in [
            CallingTokenType::Aad,
            CallingTokenType::Cae,
            CallingTokenType::Skype,
        ] {
            let headers = credential_headers(kind, "fixture-secret").unwrap();
            let name = if kind == CallingTokenType::Skype {
                "x-skypetoken"
            } else {
                "authorization"
            };
            let value = &headers[name];
            assert!(value.is_sensitive());
            assert!(!format!("{headers:?}").contains("fixture-secret"));
            assert_ne!(
                headers.contains_key("x-skypetoken"),
                headers.contains_key("authorization")
            );
        }
        assert!(credential_headers(CallingTokenType::Skype, "bad\r\nheader").is_err());
        assert!(credential_headers(CallingTokenType::Aad, "").is_err());
    }

    #[test]
    fn updates_are_atomic_ignore_old_sequences_and_never_activate_a_call() {
        let base = "https://amer.ngc.msg.skype.com";
        let mut call = session(base, CallResources::default());
        assert!(
            call.apply_conversation_update(&key(), &update(base, 3, "leave-new"))
                .unwrap()
        );
        assert!(
            !call
                .apply_conversation_update(&key(), &update(base, 2, "leave-old"))
                .unwrap()
        );
        let invalid = update("https://evil.test", 4, "leave");
        assert_eq!(
            call.apply_conversation_update(&key(), &invalid),
            Err(SessionError::InvalidUpdate)
        );
        assert_eq!(call.sequence, 3);
        assert_eq!(call.leave_link.as_ref().unwrap().path(), "/leave-new");
        assert_eq!(
            call.apply_conversation_update(&key(), &vec![b'x'; MAX_UPDATE_BYTES + 1]),
            Err(SessionError::InvalidUpdate)
        );
        let stale = CallKey {
            generation: 6,
            ..key()
        };
        assert_eq!(
            call.apply_conversation_update(&stale, &update(base, 9, "leave")),
            Err(SessionError::StaleCall)
        );
        assert!(!format!("{call:?}").contains("fixture"));
        // The original accepts equal sequence numbers; links can rotate at the
        // same sequence. No phase becomes Active merely because links exist.
        assert!(
            call.apply_conversation_update(&key(), &update(base, 3, "rotated"))
                .unwrap()
        );
        assert_eq!(call.leave_link.as_ref().unwrap().path(), "/rotated");
    }

    async fn media() -> (ost::calling::media::MediaSession, u16, UdpSocket) {
        let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        let crypto = ost::calling::srtp::SrtpKeyingMaterial {
            master_key: [0x23; 16],
            master_salt: [0x45; 14],
            tag: 1,
        };
        let audio = ost::calling::media::MediaSession::start(
            0,
            sink.local_addr().unwrap(),
            &crypto,
            &crypto,
        )
        .await
        .unwrap();
        let port = audio.local_port().unwrap();
        (audio, port, sink)
    }

    #[test]
    fn leave_stops_actual_media_before_request_and_is_idempotent_on_http_failure() {
        crate::graph::runtime().unwrap().block_on(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let (audio, port, _sink) = media().await;
            let crypto = ost::calling::srtp::SrtpKeyingMaterial {
                master_key: [0x23; 16],
                master_salt: [0x45; 14],
                tag: 1,
            };
            let video = ost::calling::media::VideoMediaSession::start(
                0,
                _sink.local_addr().unwrap(),
                &crypto,
                &crypto,
            )
            .await
            .unwrap();
            let video_port = video.local_port().unwrap();
            let pending = crate::video::Cancellation::default();
            let capture = crate::audio::AudioCancellation::new();
            let mut resources = CallResources::default();
            resources.audio = Some(audio);
            resources.video = Some(video);
            resources.pending_camera = Some(pending.clone());
            resources.audio_cancellation = Some(capture.clone());
            let mut call = session(&base, resources);
            call.apply_conversation_update(&key(), &update(&base, 1, "leave?opaque=fixture"))
                .unwrap();
            let requests = Arc::new(AtomicUsize::new(0));
            let observed = requests.clone();
            let worker = std::thread::spawn(move || {
                let mut req = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                observed.fetch_add(1, Ordering::SeqCst);
                assert_eq!(req.method().as_str(), "POST");
                assert_eq!(req.url(), "/leave?opaque=fixture");
                assert_eq!(
                    req.headers()
                        .iter()
                        .find(|h| h.field.equiv("x-skypetoken"))
                        .unwrap()
                        .value
                        .as_str(),
                    "fixture-secret"
                );
                // Verify the actual UDP descriptor is released before the
                // server replies, not just that a boolean changed.
                let _rebound = UdpSocket::bind(("0.0.0.0", port)).unwrap();
                let _rebound_video = UdpSocket::bind(("0.0.0.0", video_port)).unwrap();
                let mut body = String::new();
                req.as_reader().read_to_string(&mut body).unwrap();
                let body: Value = serde_json::from_str(&body).unwrap();
                assert!(body.get("payload").is_none());
                assert_eq!(
                    body.pointer("/participants/from/participantId").unwrap(),
                    "fixture-participant"
                );
                assert!(body.pointer("/participants/scope").is_none());
                assert_eq!(
                    body.pointer("/callTransactionEnd/phrase").unwrap(),
                    "CallEndReasonLocalUserInitiated"
                );
                assert_eq!(body.pointer("/conversationTransactionEnd/code").unwrap(), 0);
                req.respond(Response::from_string("PRIVATE SERVER DETAIL").with_status_code(503))
                    .unwrap();
            });
            assert_eq!(
                call.terminate(&key()).await.unwrap(),
                LeaveOutcome::Rejected(503)
            );
            assert!(pending.is_cancelled());
            assert!(capture.is_cancelled());
            assert!(call.resources.audio.is_none());
            assert!(call.resources.video.is_none());
            assert_eq!(
                call.terminate(&key()).await.unwrap(),
                LeaveOutcome::AlreadyEnded
            );
            assert_eq!(
                call.apply_conversation_update(&key(), &update(&base, 9, "reopen")),
                Err(SessionError::Ending)
            );
            worker.join().unwrap();
            assert_eq!(requests.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn hangup_deletes_the_call_leg_after_leave() {
        crate::graph::runtime().unwrap().block_on(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let mut call = session(&base, CallResources::default());
            call.apply_conversation_update(&key(), &update(&base, 1, "leave"))
                .unwrap();
            call.note_call_leg(format!("{base}/leg"));
            let worker = std::thread::spawn(move || {
                let leave = server.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
                assert_eq!(leave.method().as_str(), "POST");
                assert_eq!(leave.url(), "/leave");
                leave.respond(Response::empty(204)).unwrap();
                let mut leg = server.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
                assert_eq!(leg.method().as_str(), "DELETE");
                assert_eq!(leg.url(), "/leg");
                let mut body = String::new();
                leg.as_reader().read_to_string(&mut body).unwrap();
                let body: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body.pointer("/callEnd/code").unwrap(), 0);
                assert_eq!(
                    body.pointer("/callEnd/phrase").unwrap(),
                    "CallEndReasonLocalUserInitiated"
                );
                leg.respond(Response::empty(204)).unwrap();
            });
            assert_eq!(
                call.terminate(&key()).await.unwrap(),
                LeaveOutcome::Confirmed
            );
            worker.join().unwrap();
        });
    }

    #[test]
    fn redirects_never_forward_credentials_and_success_does_not_require_json() {
        crate::graph::runtime().unwrap().block_on(async {
            for status in [204, 302] {
                let server = Server::http("127.0.0.1:0").unwrap();
                let sink = Server::http("127.0.0.1:0").unwrap();
                let base = format!("http://{}", server.server_addr());
                let target = format!("http://{}/stolen", sink.server_addr());
                let worker = std::thread::spawn(move || {
                    let req = server
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap()
                        .unwrap();
                    req.respond(
                        Response::empty(status)
                            .with_header(Header::from_bytes("Location", target).unwrap()),
                    )
                    .unwrap();
                });
                let mut call = session(&base, CallResources::default());
                call.apply_conversation_update(&key(), &update(&base, 0, "leave"))
                    .unwrap();
                assert_eq!(
                    call.terminate(&key()).await.unwrap(),
                    if status == 204 {
                        LeaveOutcome::Confirmed
                    } else {
                        LeaveOutcome::Rejected(302)
                    }
                );
                worker.join().unwrap();
                assert!(
                    sink.recv_timeout(Duration::from_millis(60))
                        .unwrap()
                        .is_none()
                );
            }
        });
    }

    #[test]
    fn stale_termination_leaves_current_resources_running_and_unjoined_termination_is_local() {
        crate::graph::runtime().unwrap().block_on(async {
            let pending = crate::video::Cancellation::default();
            let mut resources = CallResources::default();
            resources.pending_camera = Some(pending.clone());
            let mut call = session("https://amer.ngc.msg.skype.com", resources);
            assert_eq!(
                call.terminate(&CallKey {
                    generation: 6,
                    ..key()
                })
                .await,
                Err(SessionError::StaleCall)
            );
            assert!(!pending.is_cancelled());
            assert_eq!(
                call.terminate(&key()).await.unwrap(),
                LeaveOutcome::NotJoined
            );
            assert!(pending.is_cancelled());
        });
    }

    #[test]
    fn aborting_leave_future_or_dropping_owner_cannot_keep_media_or_capture_alive() {
        crate::graph::runtime().unwrap().block_on(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let (audio, port, _sink) = media().await;
            let capture = crate::audio::AudioCancellation::new();
            let pending = crate::video::Cancellation::default();
            let mut resources = CallResources::default();
            resources.audio = Some(audio);
            resources.pending_camera = Some(pending.clone());
            resources.audio_cancellation = Some(capture.clone());
            let mut call = session(&base, resources);
            call.apply_conversation_update(&key(), &update(&base, 0, "leave"))
                .unwrap();
            let (signal, received) = iced::futures::channel::oneshot::channel();
            let (release, wait_for_release) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let req = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                signal.send(()).unwrap();
                // Keep response pending until the test has dropped its future.
                wait_for_release
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                drop(req);
            });
            let call_key = key();
            match tokio::time::timeout(
                Duration::from_secs(3),
                iced::futures::future::select(Box::pin(call.terminate(&call_key)), received),
            )
            .await
            .unwrap()
            {
                iced::futures::future::Either::Right((Ok(()), pending_leave)) => {
                    drop(pending_leave)
                }
                _ => panic!("leave must remain pending until server is released"),
            }
            release.send(()).unwrap();
            let _rebound = UdpSocket::bind(("0.0.0.0", port)).unwrap();
            assert!(capture.is_cancelled());
            assert!(pending.is_cancelled());
            assert_eq!(
                call.terminate(&key()).await.unwrap(),
                LeaveOutcome::AlreadyEnded
            );
            worker.join().unwrap();
            let cancel = crate::audio::AudioCancellation::new();
            let mut resources = CallResources::default();
            resources.audio_cancellation = Some(cancel.clone());
            drop(session(&base, resources));
            assert!(cancel.is_cancelled());
        });
    }

    #[test]
    fn pending_outgoing_cancellation_uses_487_and_endpoint_identity() {
        crate::graph::runtime().unwrap().block_on(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let worker = std::thread::spawn(move || {
                let mut req = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                let mut bytes = String::new();
                req.as_reader().read_to_string(&mut bytes).unwrap();
                let body: Value = serde_json::from_str(&bytes).unwrap();
                assert_eq!(body.pointer("/callTransactionEnd/code").unwrap(), 487);
                assert!(body.pointer("/participants/from/participantId").is_none());
                assert_eq!(
                    body.pointer("/participants/from/endpointId").unwrap(),
                    "fixture-endpoint"
                );
                assert!(body.pointer("/participants/scope").is_none());
                req.respond(Response::empty(200)).unwrap();
            });
            let mut call = session(&base, CallResources::default());
            call.apply_conversation_update(&key(), &update(&base, 0, "cancel"))
                .unwrap();
            assert_eq!(
                call.cancel_outgoing(&key()).await.unwrap(),
                LeaveOutcome::Confirmed
            );
            assert_eq!(
                call.terminate(&key()).await.unwrap(),
                LeaveOutcome::AlreadyEnded
            );
            worker.join().unwrap();
        });
    }

    #[test]
    fn matching_remote_end_releases_udp_without_sending_another_leave() {
        crate::graph::runtime().unwrap().block_on(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let (audio, port, _sink) = media().await;
            let mut resources = CallResources::default();
            resources.audio = Some(audio);
            let mut call = session(&base, resources);
            call.apply_conversation_update(&key(), &update(&base, 0, "leave"))
                .unwrap();
            assert_eq!(
                call.release_locally(&CallKey {
                    generation: 6,
                    ..key()
                })
                .await,
                Err(SessionError::StaleCall)
            );
            assert!(UdpSocket::bind(("0.0.0.0", port)).is_err());
            assert!(call.release_locally(&key()).await.unwrap());
            let _rebound = UdpSocket::bind(("0.0.0.0", port)).unwrap();
            assert!(!call.release_locally(&key()).await.unwrap());
            assert_eq!(
                call.terminate(&key()).await.unwrap(),
                LeaveOutcome::AlreadyEnded
            );
            assert!(
                server
                    .recv_timeout(Duration::from_millis(60))
                    .unwrap()
                    .is_none()
            );
        });
    }

    #[test]
    fn failed_constructor_cancels_resources_and_never_accepts_invalid_identity() {
        let cancel = crate::audio::AudioCancellation::new();
        let mut resources = CallResources::default();
        resources.audio_cancellation = Some(cancel.clone());
        let mut identity = participant();
        identity.participant_id = "bad\nidentity".into();
        assert_eq!(
            CallSession::new(
                key(),
                identity,
                transport("https://amer.ngc.msg.skype.com"),
                resources
            )
            .unwrap_err(),
            SessionError::InvalidIdentity
        );
        assert!(cancel.is_cancelled());
        for invalid in [
            CallKey {
                generation: 0,
                ..key()
            },
            CallKey {
                generation: 1,
                id: String::new(),
            },
            CallKey {
                generation: 1,
                id: "bad\nid".into(),
            },
        ] {
            assert_eq!(
                CallSession::new(
                    invalid,
                    participant(),
                    transport("https://amer.ngc.msg.skype.com"),
                    CallResources::default()
                )
                .unwrap_err(),
                SessionError::InvalidCall
            );
        }
    }

    #[test]
    fn timeout_and_connection_refusal_do_not_hold_capture_or_allow_a_second_leave() {
        crate::graph::runtime().unwrap().block_on(async {
            let server = Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr());
            let (release, held) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let req = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                let _ = held.recv_timeout(Duration::from_secs(3));
                drop(req);
            });
            let cancel = crate::audio::AudioCancellation::new();
            let mut resources = CallResources::default();
            resources.audio_cancellation = Some(cancel.clone());
            let mut call = session(&base, resources);
            call.transport.client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_millis(500))
                .build()
                .unwrap();
            call.apply_conversation_update(&key(), &update(&base, 0, "leave"))
                .unwrap();
            let result = call.terminate(&key()).await.unwrap();
            release.send(()).unwrap();
            worker.join().unwrap();
            assert_eq!(result, LeaveOutcome::TimedOut);
            assert!(cancel.is_cancelled());
            assert_eq!(
                call.terminate(&key()).await.unwrap(),
                LeaveOutcome::AlreadyEnded
            );

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            drop(listener);
            let cancel = crate::video::Cancellation::default();
            let mut resources = CallResources::default();
            resources.pending_camera = Some(cancel.clone());
            let mut call = session(&base, resources);
            call.apply_conversation_update(&key(), &update(&base, 0, "leave"))
                .unwrap();
            assert_eq!(
                call.terminate(&key()).await.unwrap(),
                LeaveOutcome::Unavailable
            );
            assert!(cancel.is_cancelled());
        });
    }
}

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalParticipant {
    pub id: String,
    pub display_name: String,
    pub endpoint_id: String,
    pub participant_id: String,
    pub language_id: String,
}

impl std::fmt::Debug for LocalParticipant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalParticipant([REDACTED])")
    }
}

impl LocalParticipant {
    pub(crate) fn validate(&self) -> Result<(), SessionError> {
        for field in [
            &self.id,
            &self.endpoint_id,
            &self.participant_id,
            &self.language_id,
        ] {
            if field.is_empty() || field.len() > 512 || field.chars().any(char::is_control) {
                return Err(SessionError::InvalidIdentity);
            }
        }
        if self.display_name.len() > 2048 || self.display_name.chars().any(char::is_control) {
            return Err(SessionError::InvalidIdentity);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct CallTransport {
    client: reqwest::Client,
    headers: HeaderMap,
    origin: ServiceOrigin,
    correlation_id: String,
}

impl std::fmt::Debug for CallTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallTransport([REDACTED])")
    }
}

fn credential_headers(kind: CallingTokenType, token: &str) -> Result<HeaderMap, SessionError> {
    if token.is_empty() || token.len() > 64 * 1024 {
        return Err(SessionError::InvalidCredential);
    }
    let (name, value) = match kind {
        CallingTokenType::Skype => ("x-skypetoken", token.to_owned()),
        CallingTokenType::Aad | CallingTokenType::Cae => {
            ("authorization", format!("Bearer {token}"))
        }
    };
    let mut value = HeaderValue::from_str(&value).map_err(|_| SessionError::InvalidCredential)?;
    value.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(name, value);
    headers.insert("x-ms-migration", HeaderValue::from_static("True"));
    Ok(headers)
}

impl CallTransport {
    /// The join adapter must supply the service-negotiated token. This does not
    /// choose a credential, substitute Graph, or renew authentication.
    #[allow(dead_code)] // Consumed when CALL-05 integrates authenticated join.
    pub(crate) fn new(origin: ServiceOrigin, token: CallingToken) -> Result<Self, SessionError> {
        if token.expires_in_secs == 0 {
            return Err(SessionError::InvalidCredential);
        }
        Self::with_headers(
            origin,
            credential_headers(token.token_type, token.expose_to_transport())?,
        )
    }

    fn with_headers(origin: ServiceOrigin, headers: HeaderMap) -> Result<Self, SessionError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| SessionError::Transport)?;
        Ok(Self {
            client,
            headers,
            origin,
            correlation_id: crate::call_context::native_id(),
        })
    }

    #[cfg(test)]
    pub(crate) fn fixture(base: &str) -> Self {
        Self::with_headers(
            ServiceOrigin(Url::parse(base).unwrap()),
            credential_headers(CallingTokenType::Skype, "fixture-secret").unwrap(),
        )
        .unwrap()
    }

    pub(crate) fn with_region(
        mut self,
        region: Option<&str>,
        partition: Option<&str>,
        ring: Option<&str>,
    ) -> Result<Self, ControlError> {
        for (name, value) in [
            ("ms-teams-region", region),
            ("ms-teams-partition", partition),
            ("ms-teams-ring", ring),
        ] {
            if let Some(value) = value.filter(|s| !s.is_empty()) {
                if value.len() > 64 {
                    return Err(ControlError::InvalidRequest);
                }
                let value =
                    HeaderValue::from_str(value).map_err(|_| ControlError::InvalidRequest)?;
                self.headers.insert(name, value);
            }
        }
        Ok(self)
    }

    pub(crate) async fn post_json(
        &self,
        target: &str,
        payload: &Value,
    ) -> Result<Value, ControlError> {
        self.send_json(reqwest::Method::POST, target, payload).await
    }

    pub(crate) async fn delete_json(
        &self,
        target: &str,
        payload: &Value,
    ) -> Result<Value, ControlError> {
        self.send_json(reqwest::Method::DELETE, target, payload).await
    }

    async fn send_json(
        &self,
        method: reqwest::Method,
        target: &str,
        payload: &Value,
    ) -> Result<Value, ControlError> {
        let target = self
            .origin
            .link(target)
            .map_err(|_| ControlError::InvalidOrigin)?;
        let bytes = serde_json::to_vec(payload).map_err(|_| ControlError::InvalidRequest)?;
        if bytes.len() > 1024 * 1024 {
            return Err(ControlError::TooLarge);
        }
        let mut response = self
            .client
            .request(method, target)
            .headers(self.headers.clone())
            .header("content-type", "application/json")
            .header("x-microsoft-skype-chain-id", &self.correlation_id)
            .header(
                "x-microsoft-skype-message-id",
                crate::call_context::native_id(),
            )
            .header(
                "x-microsoft-skype-client",
                concat!(
                    "TeamsLinux/0/",
                    env!("CARGO_PKG_VERSION"),
                    "/os=Linux;deviceType=Desktop/"
                ),
            )
            .body(bytes)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    ControlError::TimedOut
                } else {
                    ControlError::Network
                }
            })?;
        if !response.status().is_success() {
            return Err(ControlError::Http(response.status().as_u16()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ControlError::Network)? {
            if chunk.len() > (1024usize * 1024).saturating_sub(bytes.len()) {
                return Err(ControlError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|_| ControlError::InvalidResponse)
    }

    pub(crate) fn validate_response_link(&self, target: &str) -> Result<(), ControlError> {
        self.origin
            .link(target)
            .map(|_| ())
            .map_err(|_| ControlError::InvalidOrigin)
    }

    async fn leave(
        &self,
        link: Url,
        participant: &LocalParticipant,
        reason: EndReason,
    ) -> LeaveOutcome {
        // `gw` returns {payload: ...}; `lw` serializes only data.payload on wire.
        // Scope is omitted: this ends our participation, never the meeting.
        let mut from =
            serde_json::to_value(participant).expect("participant contains only strings");
        if reason == EndReason::CancelOutgoing {
            // Original `_V` identifies a pending call by endpoint, before a
            // participant leg has necessarily been assigned.
            from.as_object_mut().unwrap().remove("participantId");
        }
        let payload = json!({
            "participants": {"from": from},
            "conversationTransactionEnd": {
                "reason": "noError", "code": 0,
                "phrase": "ConversationEndNoModalityConnected"
            },
            "callTransactionEnd": {
                "code": if reason == EndReason::CancelOutgoing { 487 } else { 0 }, "subCode": 0,
                "phrase": "CallEndReasonLocalUserInitiated",
                "resultCategories": ["Success"]
            }
        });
        let message_id = crate::call_context::native_id();
        let request = self
            .client
            .post(link)
            .headers(self.headers.clone())
            .header("x-microsoft-skype-chain-id", &self.correlation_id)
            .header("x-microsoft-skype-message-id", message_id)
            .json(&payload)
            .send();
        match tokio::time::timeout(LEAVE_TIMEOUT, request).await {
            Ok(Ok(response)) if response.status().is_success() => LeaveOutcome::Confirmed,
            Ok(Ok(response)) => LeaveOutcome::Rejected(response.status().as_u16()),
            Ok(Err(error)) if error.is_timeout() => LeaveOutcome::TimedOut,
            Ok(Err(_)) => LeaveOutcome::Unavailable,
            Err(_) => LeaveOutcome::TimedOut,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaveOutcome {
    NotJoined,
    Confirmed,
    Rejected(u16),
    Unavailable,
    TimedOut,
    AlreadyEnded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlError {
    InvalidOrigin,
    InvalidRequest,
    InvalidResponse,
    TooLarge,
    Network,
    TimedOut,
    Http(u16),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EndReason {
    UserLeft,
    CancelOutgoing,
}

/// Own the actual native handles, not a separate boolean that says they stopped.
/// Drop is a cancellation fallback; stop().await also joins the media tasks.
#[derive(Default)]
pub struct CallResources {
    pub live_audio: Option<crate::call_audio::LiveAudio>,
    pub camera: Option<crate::video::Preview>,
    pub pending_camera: Option<crate::video::Cancellation>,
    pub audio_cancellation: Option<crate::audio::AudioCancellation>,
    pub audio: Option<ost::calling::media::MediaSession>,
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) webrtc: Option<crate::call_webrtc::Session>,
    pub video: Option<ost::calling::media::VideoMediaSession>,
}

impl CallResources {
    fn cancel_capture(&mut self) {
        if let Some(audio) = &self.live_audio { audio.request_stop(); }
        if let Some(cancel) = self.pending_camera.take() {
            cancel.cancel();
        }
        if let Some(cancel) = self.audio_cancellation.take() {
            cancel.cancel();
        }
        if let Some(camera) = self.camera.take() {
            camera.stop();
        }
    }

    /// Release the older UDP media before any hangup request. The libwebrtc peer
    /// stays open so leave and the call leg still have a live transport.
    pub async fn stop_legacy_media(&mut self) {
        let audio = self.audio.take();
        let video = self.video.take();
        iced::futures::future::join(
            async move {
                if let Some(mut audio) = audio {
                    audio.stop().await;
                }
            },
            async move {
                if let Some(mut video) = video {
                    video.stop().await;
                }
            },
        )
        .await;
    }

    pub async fn stop(&mut self) {
        self.cancel_capture();
        #[cfg(feature = "libwebrtc-av")]
        if let Some(session) = self.webrtc.take() { session.stop().await; }
        let live_audio = self.live_audio.take();
        // Stop device processes off the UI executor. Cancellation was already
        // requested synchronously, even if the owner future is later dropped.
        if let Some(live_audio) = live_audio {
            let _ = crate::audio::run_on_worker(move || { live_audio.stop(); Ok(()) }).await;
        }
        self.stop_legacy_media().await;
    }
}

impl Drop for CallResources {
    fn drop(&mut self) {
        self.cancel_capture();
    }
}

pub struct CallSession {
    key: CallKey,
    participant: LocalParticipant,
    transport: CallTransport,
    sequence: i64,
    endpoint_state_sequence: u64,
    endpoint_state_link: Option<Url>,
    unmute_link: Option<Url>,
    leave_link: Option<Url>,
    controller: Option<String>,
    update_media_url: Option<String>,
    call_leg_url: Option<String>,
    resources: CallResources,
    ending: bool,
}

impl std::fmt::Debug for CallSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallSession")
            .field("sequence", &self.sequence)
            .field("ending", &self.ending)
            .finish_non_exhaustive()
    }
}

impl CallSession {
    pub(crate) fn transport(&self) -> &CallTransport {
        &self.transport
    }
    pub(crate) fn participant(&self) -> &LocalParticipant {
        &self.participant
    }
    pub(crate) fn controller(&self) -> Option<&str> {
        self.controller.as_deref()
    }

    pub async fn attach_audio(
        &mut self,
        key: &CallKey,
        mut audio: ost::calling::media::MediaSession,
    ) -> Result<(), SessionError> {
        let error = if key != &self.key {
            Some(SessionError::StaleCall)
        } else if self.ending || self.has_attached_media() {
            Some(SessionError::Ending)
        } else {
            None
        };
        if let Some(error) = error {
            audio.stop().await;
            return Err(error);
        }
        self.resources.audio = Some(audio);
        Ok(())
    }

    #[cfg(feature = "libwebrtc-av")]
    pub(crate) async fn attach_webrtc(&mut self, key: &CallKey, session: crate::call_webrtc::Session)
        -> Result<(), SessionError> {
        let error = if key != &self.key { Some(SessionError::StaleCall) }
            else if self.ending || self.resources.audio.is_some() || self.resources.webrtc.is_some() { Some(SessionError::Ending) }
            else { None };
        if let Some(error) = error { session.stop().await; return Err(error); }
        self.resources.webrtc = Some(session);
        Ok(())
    }

    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn webrtc_gate(&self, key: &CallKey) -> Option<teams_av::pcm::MicrophoneGate> {
        if key != &self.key || self.ending { return None; }
        Some(self.resources.webrtc.as_ref()?.microphone_gate())
    }

    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn webrtc_mut(&mut self, key: &CallKey) -> Option<&mut crate::call_webrtc::Session> {
        if key != &self.key || self.ending { return None; }
        self.resources.webrtc.as_mut()
    }
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn webrtc(&self, key: &CallKey) -> Option<&crate::call_webrtc::Session> {
        if key != &self.key || self.ending { return None; }
        self.resources.webrtc.as_ref()
    }

    pub fn media_running(&self)->bool {
        #[cfg(feature = "libwebrtc-av")]
        if self.resources.webrtc.as_ref().is_some_and(|session| session.running()) { return true; }
        self.resources.audio.as_ref().is_some_and(|audio|audio.is_running())
    }
    pub(crate) fn has_attached_media(&self) -> bool {
        #[cfg(feature = "libwebrtc-av")]
        if self.resources.webrtc.is_some() { return true; }
        self.resources.audio.is_some()
    }
    pub fn mute_supported(&self)->bool {self.endpoint_state_link.is_some() && !self.ending}
    pub fn mute_request(&mut self,key:&CallKey,muted:bool)->Result<(CallTransport,String,Value),SessionError>{
        if key!=&self.key {return Err(SessionError::StaleCall);}
        if self.ending {return Err(SessionError::Ending);}
        let link=self.endpoint_state_link.as_ref().ok_or(SessionError::InvalidUpdate)?.to_string();
        self.endpoint_state_sequence=self.endpoint_state_sequence.checked_add(1).ok_or(SessionError::InvalidUpdate)?;
        Ok((self.transport.clone(),link,json!({"from":self.participant,"endpointState":{
            "endpointStateSequenceNumber":self.endpoint_state_sequence,"state":{"isMuted":muted}}})))
    }
    pub(crate) fn server_unmute_request(&self, key: &CallKey) -> Result<(CallTransport,String,Value),SessionError> {
        if key!=&self.key { return Err(SessionError::StaleCall); }
        if self.ending { return Err(SessionError::Ending); }
        let link=self.unmute_link.as_ref().ok_or(SessionError::InvalidUpdate)?;
        Ok((self.transport.clone(),link.to_string(),json!({"from":self.participant,"mediaTypes":["audio"]})))
    }
    pub(crate) fn admit_request(
        &self,
        key: &CallKey,
        participant_id: &str,
        callbacks: &CallCallbacks,
    ) -> Result<(CallTransport, String, Value), SessionError> {
        if key != &self.key {
            return Err(SessionError::StaleCall);
        }
        if self.ending {
            return Err(SessionError::Ending);
        }
        if participant_id.trim().is_empty()
            || participant_id.len() > 512
            || participant_id.chars().any(char::is_control)
            || participant_id.eq_ignore_ascii_case(&self.participant.id)
        {
            return Err(SessionError::InvalidIdentity);
        }
        let controller = self.controller.as_deref().ok_or(SessionError::InvalidUpdate)?;
        let mut target = Url::parse(controller).map_err(|_| SessionError::InvalidUpdate)?;
        let path = target.path().trim_end_matches('/');
        if path.is_empty() || path == "/" {
            return Err(SessionError::InvalidUpdate);
        }
        target.set_path(&format!("{path}/admit"));
        let target = self
            .transport
            .origin
            .link(target.as_str())
            .map_err(|_| SessionError::InvalidUpdate)?
            .to_string();
        let payload = json!({
            "participants": {
                "from": self.participant,
                "to": [{"id": participant_id}]
            },
            "links": {
                "admitSuccess": callbacks.link(CallbackKind::AdmitSuccess),
                "admitFailure": callbacks.link(CallbackKind::AdmitFailure)
            },
            "debugContent": {"causeId": crate::call_context::native_id()}
        });
        Ok((self.transport.clone(), target, payload))
    }
    pub async fn audio_stats(&self, key: &CallKey) -> Option<ost::calling::media::MediaStats> {
        if key != &self.key || self.ending { return None; }
        Some(self.resources.audio.as_ref()?.stats().await)
    }
    pub fn audio_controls(&self, key: &CallKey) -> Option<ost::calling::media::AudioControls> {
        if key != &self.key || self.ending {
            return None;
        }
        self.resources
            .audio
            .as_ref()
            .map(|audio| audio.audio_controls())
    }
    pub fn new(
        key: CallKey,
        participant: LocalParticipant,
        transport: CallTransport,
        resources: CallResources,
    ) -> Result<Self, SessionError> {
        participant.validate()?;
        if key.generation == 0
            || key.id.trim().is_empty()
            || key.id.chars().take(513).count() > 512
            || key.id.chars().any(char::is_control)
        {
            return Err(SessionError::InvalidCall);
        }
        Ok(Self {
            key,
            participant,
            transport,
            resources,
            sequence: -1,
            endpoint_state_sequence: 1,
            endpoint_state_link: None,
            unmute_link: None,
            leave_link: None,
            controller: None,
            update_media_url: None,
            call_leg_url: None,
            ending: false,
        })
    }

    pub(crate) fn note_update_media(&mut self, url: String) {
        if !self.ending && !url.is_empty() {
            self.update_media_url = Some(url);
        }
    }

    pub(crate) fn note_call_leg(&mut self, url: String) {
        if !self.ending && !url.is_empty() {
            self.call_leg_url = Some(url);
        }
    }

    /// A conversation update is not proof of connected media. It only supplies
    /// control links. Apply atomically and ignore older sequence numbers.
    pub fn apply_conversation_update(
        &mut self,
        key: &CallKey,
        bytes: &[u8],
    ) -> Result<bool, SessionError> {
        self.apply_conversation_update_detailed(key, bytes).map_err(ControlUpdateError::session_error)
    }
    pub(crate) fn apply_conversation_update_detailed(
        &mut self, key:&CallKey, bytes:&[u8],
    ) -> Result<bool, ControlUpdateError> {
        if key != &self.key {
            return Err(ControlUpdateError::StaleCall);
        }
        if self.ending {
            return Err(ControlUpdateError::Ending);
        }
        if bytes.len() > MAX_UPDATE_BYTES {
            return Err(ControlUpdateError::TooLarge);
        }
        let body: Value = serde_json::from_slice(bytes).map_err(|_| ControlUpdateError::Format)?;
        if !body.is_object() { return Err(ControlUpdateError::Format); }
        let sequence = match body.get("sequenceNumber") {
            None => -1,
            Some(value) => value
                .as_i64()
                // The original maps negative sequence values to the initial
                // sentinel -1. It is valid on creation, stale after seq >= 0.
                .map(|value| value.max(-1))
                .ok_or(ControlUpdateError::Sequence)?,
        };
        if sequence < self.sequence {
            return Ok(false);
        }
        let controller = body
            .get("conversationController")
            .and_then(Value::as_str)
            .ok_or(ControlUpdateError::ControllerMissing)?;
        let origin = self.transport.origin.controller(controller).map_err(|_|ControlUpdateError::ControllerOrigin)?;
        let controller = origin.link(controller).map_err(|_|ControlUpdateError::ControllerPath)?;
        let leave = body
            .pointer("/links/leave")
            .and_then(Value::as_str)
            .ok_or(ControlUpdateError::LeaveMissing)?;
        let leave = origin.link(leave).map_err(|_|ControlUpdateError::LeaveOrigin)?;
        let endpoint_state_link=match body.pointer("/links/updateEndpointState") {
            None|Some(Value::Null)=>self.endpoint_state_link.as_ref().and_then(|link|origin.link(link.as_str()).ok()),
            Some(Value::String(link))=>Some(origin.link(link).map_err(|_|ControlUpdateError::EndpointControl)?),
            _=>return Err(ControlUpdateError::EndpointControl),
        };
        let unmute_link = match body.pointer("/links/unmute") {
            None | Some(Value::Null) => self.unmute_link.as_ref().and_then(|link|origin.link(link.as_str()).ok()),
            Some(Value::String(link)) => Some(origin.link(link).map_err(|_|ControlUpdateError::EndpointControl)?),
            _ => return Err(ControlUpdateError::EndpointControl),
        };
        self.unmute_link=unmute_link;
        self.endpoint_state_link=endpoint_state_link;
        self.transport.origin = origin;
        self.leave_link = Some(leave);
        self.controller = Some(controller.to_string());
        self.sequence = sequence;
        Ok(true)
    }

    /// Tell the mixer the camera is no longer sending, while the peer is still up.
    async fn retire_publish(&mut self) {
        #[cfg(feature = "libwebrtc-av")]
        {
            let publishing = self
                .resources
                .webrtc
                .as_ref()
                .is_some_and(|session| session.camera_publishing());
            let mid = self
                .resources
                .webrtc
                .as_ref()
                .and_then(|session| session.camera_mid());
            if publishing {
                if let Some(session) = self.resources.webrtc.as_mut() {
                    session.stop_camera().await;
                }
            }
            let Some(url) = self.update_media_url.clone().filter(|_| publishing) else {
                return;
            };
            let Some(mid) = mid else { return };
            let body = json!({"UpdateMediaDescriptions":{"mediaDescriptions":{
                "descriptions":[{"mid": mid, "direction": "recvonly"}],
                "requestId": 1u32
            }}});
            let _ = tokio::time::timeout(LEAVE_TIMEOUT, self.transport.post_json(&url, &body)).await;
            crate::live_trace::record("call_hangup_media_v1", 0, 1, 0);
        }
    }

    /// Drop our mixer leg after leave. A failure still closes the peer afterwards.
    async fn retire_call_leg(&mut self) {
        let Some(url) = self.call_leg_url.take() else { return };
        let body = json!({"callEnd":{"code":0,"subCode":0,"phrase":"CallEndReasonLocalUserInitiated","resultCategories":["Success"]}});
        let _ = tokio::time::timeout(LEAVE_TIMEOUT, self.transport.delete_json(&url, &body)).await;
        crate::live_trace::record("call_hangup_leg_v1", 0, 1, 0);
    }

    /// A wrong/stale key cannot tear down the current call. Once accepted, local
    /// capture stops before network I/O, including if this future is cancelled.
    /// The libwebrtc peer stays up through leave and the call-leg delete.
    /// Repeated calls never send a second leave request.
    pub async fn terminate(&mut self, key: &CallKey) -> Result<LeaveOutcome, SessionError> {
        self.end(key, EndReason::UserLeft).await
    }

    /// Cancellation before connection is a distinct original payload (487),
    /// not a successful established-call hangup (0).
    pub async fn cancel_outgoing(&mut self, key: &CallKey) -> Result<LeaveOutcome, SessionError> {
        self.end(key, EndReason::CancelOutgoing).await
    }

    async fn end(
        &mut self,
        key: &CallKey,
        reason: EndReason,
    ) -> Result<LeaveOutcome, SessionError> {
        if key != &self.key {
            return Err(SessionError::StaleCall);
        }
        if self.ending {
            return Ok(LeaveOutcome::AlreadyEnded);
        }
        self.ending = true;
        let link = self.leave_link.take();
        self.resources.cancel_capture();
        self.resources.stop_legacy_media().await;
        self.retire_publish().await;
        let outcome = match link {
            Some(link) => self.transport.leave(link, &self.participant, reason).await,
            None => LeaveOutcome::NotJoined,
        };
        self.retire_call_leg().await;
        self.resources.stop().await;
        let (kind, status) = match outcome {
            LeaveOutcome::NotJoined => (0, 0),
            LeaveOutcome::Confirmed => (1, 0),
            LeaveOutcome::Rejected(status) => (2, usize::from(status)),
            LeaveOutcome::Unavailable => (3, 0),
            LeaveOutcome::TimedOut => (4, 0),
            LeaveOutcome::AlreadyEnded => (5, 0),
        };
        crate::live_trace::record("call_leave", 0, kind, status);
        Ok(outcome)
    }

    /// Used by a matching remote-end/fatal-error lifecycle effect. Release the
    /// local handles without echoing another leave request back to the service.
    pub async fn release_locally(&mut self, key: &CallKey) -> Result<bool, SessionError> {
        if key != &self.key {
            return Err(SessionError::StaleCall);
        }
        if self.ending {
            return Ok(false);
        }
        self.ending = true;
        self.leave_link = None;
        self.resources.stop().await;
        Ok(true)
    }
}
