//! Fixed-shape call diagnostics. No remote strings, URL paths, IDs, body values
//! or arbitrary property names can be retained in a diagnostic record.
use crate::call_session::ControlUpdateError;
use serde::Serialize;
use serde_json::Value;
use url::Url;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Create,
    Update,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Field {
    Missing,
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}
fn field(value: Option<&Value>) -> Field {
    match value {
        None => Field::Missing,
        Some(Value::Null) => Field::Null,
        Some(Value::Bool(_)) => Field::Boolean,
        Some(Value::Number(_)) => Field::Number,
        Some(Value::String(_)) => Field::String,
        Some(Value::Array(_)) => Field::Array,
        Some(Value::Object(_)) => Field::Object,
    }
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Sequence {
    Missing,
    Null,
    Negative,
    Nonnegative,
    Invalid,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum AddressForm {
    Valid,
    Malformed,
    TooLong,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scheme {
    Https,
    Http,
    Other,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Port {
    HttpsDefault,
    Trouter3443,
    Other,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Family {
    NgcSkype,
    NgcTeams,
    CallingTeams,
    Skype,
    Teams,
    Other,
}
fn family(host: &str) -> Family {
    for (suffix, family) in [
        ("ngc.msg.skype.com", Family::NgcSkype),
        ("ngc.msg.teams.microsoft.com", Family::NgcTeams),
        ("calling.teams.microsoft.com", Family::CallingTeams),
        ("skype.com", Family::Skype),
        ("teams.microsoft.com", Family::Teams),
    ] {
        if host == suffix || host.ends_with(&format!(".{suffix}")) {
            return family;
        }
    }
    Family::Other
}
const MAX_ADDRESS: usize = 16 * 1024;
fn parse(value: Option<&Value>) -> Option<Url> {
    let value = value?.as_str()?;
    (value.len() <= MAX_ADDRESS)
        .then(|| Url::parse(value).ok())
        .flatten()
}
#[derive(Debug, Serialize)]
struct Address {
    form: AddressForm,
    scheme: Scheme,
    port: Port,
    family: Family,
    root_path: bool,
    credentials: bool,
    query: bool,
    fragment: bool,
    controls: bool,
}
fn address(value: Option<&Value>) -> Option<Address> {
    let text = value?.as_str()?;
    let mut shape = Address {
        form: AddressForm::Malformed,
        scheme: Scheme::Other,
        port: Port::Other,
        family: Family::Other,
        root_path: false,
        credentials: false,
        query: false,
        fragment: false,
        controls: false,
    };
    if text.len() > MAX_ADDRESS {
        shape.form = AddressForm::TooLong;
        return Some(shape);
    }
    shape.controls = text.chars().any(char::is_control);
    if let Ok(url) = Url::parse(text) {
        shape.form = AddressForm::Valid;
        shape.scheme = match url.scheme() {
            "https" => Scheme::Https,
            "http" => Scheme::Http,
            _ => Scheme::Other,
        };
        shape.port = match (url.scheme(), url.port_or_known_default()) {
            ("https", Some(443)) => Port::HttpsDefault,
            (_, Some(3443)) => Port::Trouter3443,
            _ => Port::Other,
        };
        shape.family = family(url.host_str().unwrap_or(""));
        shape.root_path = url.path().is_empty() || url.path() == "/";
        shape.credentials = !url.username().is_empty() || url.password().is_some();
        shape.query = url.query().is_some();
        shape.fragment = url.fragment().is_some();
    }
    Some(shape)
}
#[derive(Debug, Serialize)]
pub(crate) struct ControlDiagnostic {
    version: u8,
    phase: Phase,
    reason: ControlUpdateError,
    root: Field,
    controller: Field,
    sequence: Sequence,
    links: Field,
    leave: Field,
    endpoint_control: Field,
    conversation_response: Field,
    response: Field,
    result: Field,
    error: Field,
    controller_address: Option<Address>,
    leave_address: Option<Address>,
    same_controller_leave_origin: Option<bool>,
    same_request_controller_origin: Option<bool>,
}
impl ControlDiagnostic {
    fn capture(
        phase: Phase,
        reason: ControlUpdateError,
        body: &Value,
        request: Option<&str>,
    ) -> Self {
        let controller = body.get("conversationController");
        let leave = body.pointer("/links/leave");
        let sequence = match body.get("sequenceNumber") {
            None => Sequence::Missing,
            Some(Value::Null) => Sequence::Null,
            Some(value) => match value.as_i64() {
                Some(value) if value < 0 => Sequence::Negative,
                Some(_) => Sequence::Nonnegative,
                None => Sequence::Invalid,
            },
        };
        let controller_url = parse(controller);
        let leave_url = parse(leave);
        let request_url = request
            .filter(|value| value.len() <= MAX_ADDRESS)
            .and_then(|value| Url::parse(value).ok());
        Self {
            version: 1,
            phase,
            reason,
            root: field(Some(body)),
            controller: field(controller),
            sequence,
            links: field(body.get("links")),
            leave: field(leave),
            endpoint_control: field(body.pointer("/links/updateEndpointState")),
            conversation_response: field(body.get("conversationResponse")),
            response: field(body.get("response")),
            result: field(body.get("result")),
            error: field(body.get("error")),
            controller_address: address(controller),
            leave_address: address(leave),
            same_controller_leave_origin: controller_url
                .as_ref()
                .zip(leave_url.as_ref())
                .map(|(controller, leave)| controller.origin() == leave.origin()),
            same_request_controller_origin: controller_url
                .as_ref()
                .zip(request_url.as_ref())
                .map(|(controller, request)| controller.origin() == request.origin()),
        }
    }
}
pub(crate) fn rejected(
    phase: Phase,
    reason: ControlUpdateError,
    body: &Value,
    request: Option<&str>,
) {
    if crate::live_trace::enabled() {
        crate::live_trace::call_control(ControlDiagnostic::capture(phase, reason, body, request));
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Diagnostic {
    Control(ControlDiagnostic),
    Acceptance(AcceptanceDiagnostic),
    RemoteEnd(RemoteEndDiagnostic),
    Invite(InviteDiagnostic),
    Renegotiation(RenegotiationDiagnostic),
    MediaPath(MediaPathDiagnostic),
}

/// Fixed-shape aggregate for explaining the native audio path without storing
/// packet contents, addresses, SDP, or microphone samples.
#[derive(Debug, Serialize)]
pub(crate) struct MediaPathDiagnostic {
    version: u8,
    local_transport: &'static str,
    selected_remote_candidate: &'static str,
    remote_host_candidates: u8,
    remote_srflx_candidates: u8,
    remote_relay_candidates: u8,
    send_codec: &'static str,
    receive_pcmu_supported: bool,
    receive_opus_supported: bool,
    packets_sent: u64,
    packets_received: u64,
    inbound_datagrams: u64,
    unexpected_source_datagrams: u64,
    stun_datagrams: u64,
    dtls_datagrams: u64,
    control_datagrams: u64,
    control_unprotect_failures: u64,
    control_parse_failures: u64,
    control_reports_sent: u64,
    control_reports_received: u64,
    control_rejected_packets: u64,
    control_round_trip_ms: Option<u32>,
    remote_cumulative_lost: Option<i32>,
    remote_jitter_ticks: Option<u32>,
    receive_auth_failures: u64,
    receive_header_failures: u64,
    playout_rejected_packets: u64,
    capture_frames: u64,
    capture_active_frames: u64,
    microphone_muted: bool,
    capture_underruns: u64,
    send_errors: u64,
    send_skipped_frames: u64,
    device_silence_frames: u64,
    rendered_frames: u64,
    speaker_dropped_frames: u64,
    jitter_micros: u64,
    estimated_lost_packets: u64,
    remote_fraction_lost: Option<u8>,
}
impl MediaPathDiagnostic {
    fn capture(stats: &ost::calling::media::MediaStats) -> Self {
        let local_transport = match stats.local_transport { 1 => "direct", 2 => "relay", _ => "unknown" };
        let selected_remote_candidate = match stats.remote_candidate_type { 1 => "host", 2 => "srflx", 3 => "relay", _ => "unknown" };
        let send_codec = match stats.send_codec { 1 => "pcmu", 2 => "opus", _ => "unknown" };
        Self {
            version: 1,
            local_transport,
            selected_remote_candidate,
            remote_host_candidates: stats.remote_host_candidates,
            remote_srflx_candidates: stats.remote_srflx_candidates,
            remote_relay_candidates: stats.remote_relay_candidates,
            send_codec,
            receive_pcmu_supported: stats.receive_codec_mask & 1 != 0,
            receive_opus_supported: stats.receive_codec_mask & 2 != 0,
            packets_sent: stats.packets_sent,
            packets_received: stats.packets_received,
            inbound_datagrams: stats.inbound_datagrams,
            unexpected_source_datagrams: stats.unexpected_source_datagrams,
            stun_datagrams: stats.stun_datagrams,
            dtls_datagrams: stats.dtls_datagrams,
            control_datagrams: stats.control_datagrams,
            control_unprotect_failures: stats.control_unprotect_failures,
            control_parse_failures: stats.control_parse_failures,
            control_reports_sent: stats.control.reports_sent,
            control_reports_received: stats.control.reports_received,
            control_rejected_packets: stats.control.rejected_packets,
            control_round_trip_ms: stats.control.round_trip_ms,
            remote_cumulative_lost: stats.control.remote_cumulative_lost,
            remote_jitter_ticks: stats.control.remote_jitter_ticks,
            receive_auth_failures: stats.receive_auth_failures,
            receive_header_failures: stats.receive_header_failures,
            playout_rejected_packets: stats.playout_rejected_packets,
            capture_frames: stats.capture_frames,
            capture_active_frames: stats.capture_active_frames,
            microphone_muted: stats.microphone_muted,
            capture_underruns: stats.capture_underruns,
            send_errors: stats.send_errors,
            send_skipped_frames: stats.send_skipped_frames,
            device_silence_frames: stats.playout.device_silence_frames,
            rendered_frames: stats.playout.rendered_frames,
            speaker_dropped_frames: stats.playout.speaker_dropped_frames,
            jitter_micros: (stats.rtp_quality.jitter_millis() * 1000.0) as u64,
            estimated_lost_packets: stats.rtp_quality.estimated_lost_packets,
            remote_fraction_lost: stats.control.remote_fraction_lost,
        }
    }
}
pub(crate) fn media_path(stats: &ost::calling::media::MediaStats) {
    if crate::live_trace::enabled() {
        crate::live_trace::call_media_path(MediaPathDiagnostic::capture(stats));
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RenegotiationDiagnostic {
    version: u8,
    media_content: Field,
    content_type: Field,
    blob: Field,
    blob_bytes: usize,
    parse_error: Option<crate::call_media::MediaError>,
    answer_link: Field,
    answer_link_valid: bool,
    audio_requested: bool,
}
impl RenegotiationDiagnostic {
    fn capture(body: &Value, parse_error: Option<crate::call_media::MediaError>, answer_link_valid: bool, audio_requested: bool) -> Self {
        Self {
            version: 1,
            media_content: field(body.get("mediaContent")),
            content_type: field(body.pointer("/mediaContent/contentType")),
            blob: field(body.pointer("/mediaContent/blob")),
            blob_bytes: body.pointer("/mediaContent/blob").and_then(Value::as_str).map_or(0, str::len),
            parse_error,
            answer_link: field(body.pointer("/links/mediaAnswer")),
            answer_link_valid,
            audio_requested,
        }
    }
}
pub(crate) fn renegotiation(body: &Value, parse_error: Option<crate::call_media::MediaError>, answer_link_valid: bool, audio_requested: bool) {
    if crate::live_trace::enabled() {
        crate::live_trace::call_renegotiation(RenegotiationDiagnostic::capture(body, parse_error, answer_link_valid, audio_requested));
    }
}

/// Only schema shapes and bounded numeric status codes, never response text or IDs.
#[derive(Debug, Serialize)]
pub(crate) struct InviteDiagnostic {
    version: u8,
    root: Field,
    error: Field,
    result: Field,
    status: Field,
    participants: Field,
    code: Option<u32>,
    sub_code: Option<u32>,
}
impl InviteDiagnostic {
    fn capture(body: &Value) -> Self {
        let number = |value: Option<&Value>| value.and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
        Self {
            version: 1,
            root: field(Some(body)),
            error: field(body.get("error")),
            result: field(body.get("result")),
            status: field(body.get("status")),
            participants: field(body.get("participants")),
            code: number(body.get("code").or_else(|| body.pointer("/error/code"))),
            sub_code: number(body.get("subCode").or_else(|| body.pointer("/error/subCode"))),
        }
    }
}
pub(crate) fn invite_response(body: &Value) {
    if crate::live_trace::enabled() {
        crate::live_trace::call_invite(InviteDiagnostic::capture(body));
    }
}
pub(crate) fn invite_failure(body: &Value) {
    if crate::live_trace::enabled() {
        crate::live_trace::call_invite_failure(InviteDiagnostic::capture(body));
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RemoteEndDiagnostic {
    version: u8,
    before_acceptance: bool,
    code: Option<u32>,
    sub_code: Option<u32>,
    controller_details: bool,
    broadcast_details: bool,
    phrase_terms: Vec<&'static str>,
}
impl RemoteEndDiagnostic {
    fn capture(body: &Value, before_acceptance: bool) -> Self {
        // Original handleCallEnd uses callEnd || body, then controller and
        // broadcast end details in that order. Retain only bounded integers.
        let mut end = body.get("callEnd").filter(|v| v.is_object()).unwrap_or(body);
        let controller_details = end.get("callControllerTransactionEnd").is_some_and(Value::is_object);
        if controller_details { end = &end["callControllerTransactionEnd"]; }
        let broadcast_details = end.get("broadcastOperationFailure").is_some_and(Value::is_object);
        if broadcast_details { end = &end["broadcastOperationFailure"]; }
        let number = |name| end.get(name).and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok());
        // Only fixed technical terms can leave this function. Never retain
        // free-form server phrases, IDs, URLs or additionalData in traces.
        let phrase = end.get("phrase").and_then(Value::as_str)
            .filter(|s| s.len() <= 4096).unwrap_or("").to_ascii_lowercase();
        let phrase_terms = ["sdp", "dtls", "crypto", "codec", "media", "parse", "network", "timeout"]
            .into_iter().filter(|term| phrase.contains(term)).collect();
        Self { version: 1, before_acceptance, code: number("code"), sub_code: number("subCode"), controller_details, broadcast_details, phrase_terms }
    }
}
pub(crate) fn remote_end(body: &Value, before_acceptance: bool) {
    if crate::live_trace::enabled() {
        crate::live_trace::call_remote_end(RemoteEndDiagnostic::capture(body, before_acceptance));
    }
}
#[derive(Debug, Serialize)]
pub(crate) struct AcceptanceDiagnostic {
    version: u8,
    root: Field,
    call_acceptance: Field,
    media_content: Field,
    content_type: Field,
    blob: Field,
    acknowledgement: Field,
    call_leg: Field,
    keepalive: Field,
    keepalive_valid_range: bool,
    acknowledgement_address: Option<Address>,
    call_leg_address: Option<Address>,
}
impl AcceptanceDiagnostic {
    fn capture(body: &Value) -> Self {
        let accepted = body.get("callAcceptance");
        let at = |path| accepted.and_then(|value| value.pointer(path));
        Self {
            version: 1,
            root: field(Some(body)),
            call_acceptance: field(accepted),
            media_content: field(at("/mediaContent")),
            content_type: field(at("/mediaContent/contentType")),
            blob: field(at("/mediaContent/blob")),
            acknowledgement: field(at("/links/acknowledgement")),
            call_leg: field(at("/links/callLeg")),
            keepalive: field(at("/callKeepAliveInterval")),
            keepalive_valid_range: at("/callKeepAliveInterval")
                .and_then(Value::as_f64)
                .is_some_and(|value| value.is_finite() && (1.0..=3600.0).contains(&value)),
            acknowledgement_address: address(at("/links/acknowledgement")),
            call_leg_address: address(at("/links/callLeg")),
        }
    }
}
/// Records the shape before validation; this is not an acceptance success event.
pub(crate) fn acceptance(body: &Value) {
    if crate::live_trace::enabled() {
        crate::live_trace::call_acceptance(AcceptanceDiagnostic::capture(body));
    }
}

/// Lifecycle records accept only local enums, never remote values.
pub(crate) fn phase(phase: crate::native_call::Phase) {
    use crate::native_call::Phase;
    let name = match phase {
        Phase::Preparing => "call_preparing",
        Phase::Joining => "call_joining",
        Phase::Waiting => "call_waiting_acceptance",
        Phase::Connecting => "call_connecting_media",
        Phase::Connected => "call_connected",
        Phase::Ending => "call_ending",
        Phase::Ended => "call_ended",
        Phase::Failed => "call_failed",
    };
    crate::live_trace::record(name, 0, 0, 0);
}
pub(crate) fn fault(fault: crate::native_call::Fault) {
    use crate::native_call::Fault;
    let name = match fault {
        Fault::Invitation => "call_fault_invitation",
        Fault::Session => "call_fault_session",
        Fault::Route => "call_fault_route",
        Fault::Identity => "call_fault_identity",
        Fault::Transport => "call_fault_transport",
        Fault::Busy => "call_fault_busy",
        Fault::Audio => "call_fault_audio",
        Fault::AecUnavailable => "call_fault_aec_unavailable",
        Fault::AecFailed => "call_fault_aec_failed",
        Fault::Media => "call_fault_media",
        Fault::Protocol => "call_fault_protocol",
        Fault::Negotiation(_) => "call_fault_negotiation",
        Fault::Http(_) => "call_fault_http",
        Fault::Codec => "call_fault_codec",
        Fault::Protection => "call_fault_protection",
        Fault::Timeout => "call_fault_timeout",
        Fault::Unsupported => "call_fault_unsupported",
        Fault::Consent => "call_fault_consent",
        Fault::Worker => "call_fault_worker",
        Fault::RemoteBeforeAcceptance => "call_fault_remote_before_acceptance",
    };
    let code = match fault {
        Fault::Http(code) => usize::from(code),
        _ => 0,
    };
    crate::live_trace::record(name, 0, code, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renegotiation_reports_the_parse_boundary_without_media_or_private_values() {
        let body = serde_json::json!({"mediaContent":{"contentType":"private-type","blob":"private-SDP-key"},"links":{"mediaAnswer":"https://private.teams.microsoft.com/private?private"},"callModalities":["private"]});
        let text = serde_json::to_string(&Diagnostic::Renegotiation(RenegotiationDiagnostic::capture(&body, Some(crate::call_media::MediaError::UnsupportedCodec), true, false))).unwrap();
        assert!(!text.contains("private"));
        let report: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["parse_error"], "unsupported_codec");
        assert_eq!(report["blob_bytes"], 15);
        assert_eq!(report["answer_link_valid"], true);
        assert_eq!(report["audio_requested"], false);
    }
    #[test]
    fn invite_response_diagnostics_never_record_private_values() {
        let body = serde_json::json!({"error":{"code":403,"subCode":12,"message":"private-token"},
            "status":"private-status", "participants":[{"id":"private-user"}], "url":"private-url"});
        let report = serde_json::to_string(&InviteDiagnostic::capture(&body)).unwrap();
        assert!(!report.contains("private"));
        let parsed: Value = serde_json::from_str(&report).unwrap();
        assert_eq!(parsed["code"], 403);
        assert_eq!(parsed["sub_code"], 12);
        assert_eq!(parsed["participants"], "array");
        let report = InviteDiagnostic::capture(&serde_json::json!({"code":"secret","subCode":-1}));
        assert!(report.code.is_none() && report.sub_code.is_none());
    }
    #[test]
    fn remote_end_retains_only_numeric_codes_and_original_override_order() {
        let body = serde_json::json!({"callEnd":{"code":200,"phrase":"private-secret",
            "callControllerTransactionEnd":{"code":403,"subCode":12345,"phrase":"private-secret",
                "broadcastOperationFailure":{"code":488,"subCode":67890,"additionalData":"private-secret"}}}});
        let report = serde_json::to_string(&RemoteEndDiagnostic::capture(&body, true)).unwrap();
        assert!(!report.contains("private"));
        let parsed: Value = serde_json::from_str(&report).unwrap();
        assert_eq!(parsed["code"], 488);
        assert_eq!(parsed["sub_code"], 67890);
        let hint = RemoteEndDiagnostic::capture(&serde_json::json!({"code":410,"phrase":"private-secret SDP parse private-user"}),true);
        assert_eq!(hint.phrase_terms, vec!["sdp", "parse"]);
        assert!(!serde_json::to_string(&hint).unwrap().contains("private"));
        for bad in [Value::Null, serde_json::json!({"code":"private-secret","subCode":-1})] {
            let report = RemoteEndDiagnostic::capture(&bad, true);
            assert_eq!(report.code, None);
            assert_eq!(report.sub_code, None);
        }
    }
    #[test]
    fn acceptance_shape_never_retains_media_or_remote_values() {
        let body = serde_json::json!({"callAcceptance":{
            "mediaContent":{"contentType":"private-type","blob":"private-SDP-password"},
            "links":{"acknowledgement":"https://private.ngc.msg.skype.com/private?secret=private",
                     "callLeg":"https://private.ngc.msg.skype.com/private"},
            "callKeepAliveInterval":30,"private-roster":"private-user"}});
        let report = Diagnostic::Acceptance(AcceptanceDiagnostic::capture(&body));
        let text = serde_json::to_string(&report).unwrap();
        assert!(!text.contains("private"));
        assert!(!text.contains("password"));
        assert!(text.len() < 2048);
        let wire: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(wire["blob"], "string");
        assert_eq!(wire["keepalive_valid_range"], true);
        assert_eq!(wire["acknowledgement_address"]["query"], true);
        let missing = serde_json::to_value(AcceptanceDiagnostic::capture(&Value::Null)).unwrap();
        assert_eq!(missing["call_acceptance"], "missing");
        assert_eq!(missing["keepalive_valid_range"], false);
    }
    #[test]
    fn serialized_report_keeps_types_and_causes_without_private_values_or_keys() {
        let body = serde_json::json!({"conversationController":"https://user:password@private.ngc.msg.skype.com/secret-controller?token=private#hidden",
            "sequenceNumber":-1,"links":{"leave":"https://private.ngc.msg.skype.com/private-leave","updateEndpointState":false},
            "roster":{"private-person-id":"private-message"},"private-key":"private-value","error":{"message":"private-server-body"}});
        let report = ControlDiagnostic::capture(
            Phase::Create,
            ControlUpdateError::EndpointControl,
            &body,
            Some("https://private.ngc.msg.skype.com/private-request"),
        );
        let text = serde_json::to_string(&report).unwrap();
        for secret in [
            "private",
            "password",
            "secret-controller",
            "token",
            "hidden",
            "roster",
        ] {
            assert!(!text.contains(secret), "diagnostic leaked {secret}");
        }
        let wire: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(wire["controller"], "string");
        assert_eq!(wire["sequence"], "negative");
        assert_eq!(wire["controller_address"]["family"], "ngc_skype");
        assert_eq!(wire["controller_address"]["credentials"], true);
        assert_eq!(wire["same_controller_leave_origin"], true);
        assert_eq!(wire["same_request_controller_origin"], true);
        assert!(text.len() < 2048);
    }
    #[test]
    fn malformed_oversized_and_wrapped_responses_are_bounded_and_do_not_echo_content() {
        for body in [
            Value::Null,
            serde_json::json!({"conversationResponse":{"conversationController":"private"}}),
            serde_json::json!({"conversationController":"private".repeat(16384),"sequenceNumber":"private"}),
        ] {
            let report = ControlDiagnostic::capture(
                Phase::Update,
                ControlUpdateError::ControllerMissing,
                &body,
                None,
            );
            let text = serde_json::to_string(&report).unwrap();
            assert!(!text.contains("private"));
            assert!(text.len() < 2048);
        }
    }
}
