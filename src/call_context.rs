//! Session-scoped call callback preparation over the existing Trouter channel.
//! Original `yt`: baseMessagingChannelUrl + callAgent/sessionId/causeId + path.
//! This prepares addresses; it does not join a meeting or prove media readiness.
use crate::call_lifecycle::CallKey;
use serde::Deserialize;
use std::time::{Duration, Instant};
use url::Url;

pub(crate) fn native_id() -> String {
    let mut bytes = rand::random::<[u8; 16]>();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[derive(Clone)]
pub struct CallContext {
    root: Url,
    endpoint: String,
    deadline: Instant,
}

impl std::fmt::Debug for CallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallContext([REDACTED])")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connected {
    messaging_root: String,
    transport_endpoint_id: String,
    ttl_seconds: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MetadataError {
    Format,
    Endpoint,
    Expired,
    Address,
    Host,
    Port,
}
impl MetadataError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Format => "metadata_format",
            Self::Endpoint => "metadata_endpoint",
            Self::Expired => "metadata_expired",
            Self::Address => "metadata_address",
            Self::Host => "metadata_host",
            Self::Port => "metadata_port",
        }
    }
}

impl CallContext {
    /// Called only for the session's internal TransportConnected event, never
    /// for a user-pasted meeting URL or an ordinary remote push payload.
    pub(crate) fn from_connected_event(bytes: &str) -> Option<Self> {
        Self::parse_connected_event(bytes).ok()
    }
    pub(crate) fn parse_connected_event(bytes: &str) -> Result<Self, MetadataError> {
        if bytes.len() > 16 * 1024 {
            return Err(MetadataError::Format);
        }
        let wire: Connected = serde_json::from_str(bytes).map_err(|_| MetadataError::Format)?;
        if wire.transport_endpoint_id.is_empty()
            || wire.transport_endpoint_id.len() > 512
            || wire.transport_endpoint_id.chars().any(char::is_control)
        {
            return Err(MetadataError::Endpoint);
        }
        if wire.ttl_seconds == 0 {
            return Err(MetadataError::Expired);
        }
        if wire.messaging_root.len() > 8192 || wire.messaging_root.chars().any(char::is_control) {
            return Err(MetadataError::Address);
        }
        let mut root = Url::parse(&wire.messaging_root).map_err(|_| MetadataError::Address)?;
        let host = root.host_str().ok_or(MetadataError::Address)?;
        if root.scheme() != "https"
            || !root.username().is_empty()
            || root.password().is_some()
            || root.query().is_some()
            || root.fragment().is_some()
        {
            return Err(MetadataError::Address);
        }
        if !["skype.com", "teams.microsoft.com", "teams.live.com"]
            .iter()
            .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        {
            return Err(MetadataError::Host);
        }
        // The pinned original explicitly handles HTTPS SURLs on :3443.
        // Limit this legacy port to the known Trouter families, not every
        // otherwise accepted Microsoft/Skype service or a pasted meeting URL.
        let trouter_host = ["trouter.skype.com", "trouter.teams.microsoft.com"]
            .iter()
            .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")));
        if root.port_or_known_default() != Some(443) && !(root.port() == Some(3443) && trouter_host)
        {
            return Err(MetadataError::Port);
        }
        // Url::join must append under the opaque endpoint, never replace its
        // final segment when the service returns a SURL without a final slash.
        if !root.path().ends_with('/') {
            root.set_path(&format!("{}/", root.path()));
        }
        Ok(Self {
            root,
            endpoint: wire.transport_endpoint_id,
            deadline: Instant::now() + Duration::from_secs(wire.ttl_seconds.min(3600)),
        })
    }

    pub fn is_ready(&self) -> bool {
        Instant::now() < self.deadline
    }

    pub fn prepare_callbacks(&self, key: CallKey) -> Option<CallCallbacks> {
        if !self.is_ready()
            || key.generation == 0
            || key.id.trim().is_empty()
            || key.id.chars().take(513).count() > 512
            || key.id.chars().any(char::is_control)
        {
            return None;
        }
        // Our own per-call UUID, independent of any remote supplied call ID.
        let session = native_id();
        let links = CallbackKind::ALL
            .into_iter()
            .map(|kind| {
                let nonce = rand::random::<u32>();
                self.root
                    .join(&format!("callAgent/{session}/{nonce:08x}{}", kind.path()))
            })
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        Some(CallCallbacks {
            key,
            links,
            transport: self.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum CallbackKind {
    Progress,
    MediaAnswer,
    Acceptance,
    Redirection,
    CallEnd,
    ConversationEnd,
    ConversationUpdate,
    LocalParticipantUpdate,
    AddParticipantSuccess,
    AddParticipantFailure,
    ReceiveMessage,
    RosterUpdate,
    AddModalitySuccess,
    AddModalityFailure,
    ConfirmUnmute,
    MediaRenegotiation,
    Transfer,
    Replacement,
    BalanceUpdate,
    RetargetCompletion,
    ControlVideoStreaming,
    UpdateMediaDescriptions,
    MediaAcknowledgement,
    AdmitSuccess,
    AdmitFailure,
    MediaRejection,
}

impl CallbackKind {
    const ALL: [Self; 26] = [
        Self::Progress,
        Self::MediaAnswer,
        Self::Acceptance,
        Self::Redirection,
        Self::CallEnd,
        Self::ConversationEnd,
        Self::ConversationUpdate,
        Self::LocalParticipantUpdate,
        Self::AddParticipantSuccess,
        Self::AddParticipantFailure,
        Self::ReceiveMessage,
        Self::RosterUpdate,
        Self::AddModalitySuccess,
        Self::AddModalityFailure,
        Self::ConfirmUnmute,
        Self::MediaRenegotiation,
        Self::Transfer,
        Self::Replacement,
        Self::BalanceUpdate,
        Self::RetargetCompletion,
        Self::ControlVideoStreaming,
        Self::UpdateMediaDescriptions,
        Self::MediaAcknowledgement,
            Self::AdmitSuccess,
            Self::AdmitFailure,
            Self::MediaRejection,
        ];
    fn path(self) -> &'static str {
        match self {
            Self::Progress => "/call/progress/",
            Self::MediaAnswer => "/call/mediaAnswer/",
            Self::Acceptance => "/call/acceptance/",
            Self::Redirection => "/call/redirection/",
            Self::CallEnd => "/call/end/",
            Self::ConversationEnd => "/conversation/conversationEnd/",
            Self::ConversationUpdate => "/conversation/conversationUpdate/",
            Self::LocalParticipantUpdate => "/conversation/localParticipantUpdate/",
            Self::AddParticipantSuccess => "/conversation/addParticipantSuccess/",
            Self::AddParticipantFailure => "/conversation/addParticipantFailure/",
            Self::ReceiveMessage => "/conversation/receiveMessage/",
            Self::RosterUpdate => "/conversation/rosterUpdate/",
            Self::AddModalitySuccess => "/conversation/addModalitySuccess/",
            Self::AddModalityFailure => "/conversation/addModalityFailure/",
            Self::ConfirmUnmute => "/conversation/confirmUnmute/",
            Self::MediaRenegotiation => "/call/mediaRenegotiation/",
            Self::Transfer => "/call/transfer/",
            Self::Replacement => "/call/replacement/",
            Self::BalanceUpdate => "/call/balanceUpdate/",
            Self::RetargetCompletion => "/call/retargetCompletion/",
            Self::ControlVideoStreaming => "/call/controlVideoStreaming/",
            Self::UpdateMediaDescriptions => "/call/updateMediaDescriptions",
            Self::MediaAcknowledgement => "/call/mediaAcknowledgement/",
            // The PWA capture observed these link names in the /admit body.
            // The per-call path shape follows the other conversation callbacks.
            Self::AdmitSuccess => "/conversation/admitSuccess/",
            Self::AdmitFailure => "/conversation/admitFailure/",
            Self::MediaRejection => "/call/mediaRejection/",
        }
    }
}

#[derive(Clone)]
pub struct CallCallbacks {
    key: CallKey,
    links: Vec<Url>,
    transport: CallContext,
}

impl std::fmt::Debug for CallCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallCallbacks")
            .field("count", &self.links.len())
            .finish_non_exhaustive()
    }
}

impl CallCallbacks {
    pub fn link(&self, kind: CallbackKind) -> &str {
        self.links[kind as usize].as_str()
    }
    pub fn is_current(&self, key: &CallKey, context: &CallContext) -> bool {
        key == &self.key
            && self.transport.is_ready()
            && context.is_ready()
            && self.transport.root == context.root
            && self.transport.endpoint == context.endpoint
    }
    pub fn accepts(
        &self,
        key: &CallKey,
        kind: CallbackKind,
        address: &str,
        context: &CallContext,
    ) -> bool {
        self.delivery_kind(key, address, context) == Some(kind)
    }

    pub(crate) fn delivery_kind(
        &self,
        key: &CallKey,
        address: &str,
        context: &CallContext,
    ) -> Option<CallbackKind> {
        self.delivery_kind_from_addresses(key, Some(address), None, context)
    }

    /// Resolve the full and/or Trouter `shortUrl` representations against the
    /// current session. When both are supplied they must identify one exact
    /// callback from this attempt.
    pub(crate) fn delivery_kind_from_addresses(
        &self,
        key: &CallKey,
        address: Option<&str>,
        short_address: Option<&str>,
        context: &CallContext,
    ) -> Option<CallbackKind> {
        if address.is_none() && short_address.is_none() { return None; }
        let resolve = |address: &str| {
            if address.len() > 16 * 1024 || address.chars().any(char::is_control) { return None; }
            let candidate = self.transport.root.join(address).ok()?;
            CallbackKind::ALL
                .into_iter()
                .find(|kind| self.link(*kind) == candidate.as_str())
        };
        if !self.is_current(key, context) {
            return None;
        }
        match (address.and_then(resolve), short_address.and_then(resolve)) {
            (Some(full), Some(short)) if full == short => Some(full),
            (Some(_), Some(_)) => None,
            (Some(kind), None) if short_address.is_none() => Some(kind),
            (None, Some(kind)) if address.is_none() => Some(kind),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key() -> CallKey {
        CallKey {
            generation: 1,
            id: "fixture-call".into(),
        }
    }
    fn payload(root: &str, endpoint: &str) -> String {
        serde_json::json!({"messagingRoot":root,"transportEndpointId":endpoint,"ttlSeconds":7200})
            .to_string()
    }
    fn context() -> CallContext {
        CallContext::from_connected_event(&payload(
            "https://fixture.trouter.skype.com/v4/opaque/",
            "fixture-endpoint",
        ))
        .unwrap()
    }
    #[test]
    fn callback_addresses_are_stable_per_call_unique_and_bound_to_transport_and_generation() {
        let context = context();
        let callbacks = context.prepare_callbacks(key()).unwrap();
        let second = context.prepare_callbacks(key()).unwrap();
        let unique = CallbackKind::ALL
            .into_iter()
            .map(|kind| callbacks.link(kind))
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), 26);
        for kind in CallbackKind::ALL {
            let link = callbacks.link(kind);
            let url = Url::parse(link).unwrap();
            assert_eq!(url.host_str(), context.root.host_str());
            assert!(url.path().starts_with("/v4/opaque/callAgent/"));
            assert!(url.path().ends_with(kind.path()));
            assert_ne!(link, second.link(kind));
            assert!(callbacks.accepts(&key(), kind, link, &context));
            assert!(!callbacks.accepts(
                &CallKey {
                    generation: 2,
                    ..key()
                },
                kind,
                link,
                &context
            ));
        }
        let replacement = CallContext::from_connected_event(&payload(
            context.root.as_str(),
            "replacement-endpoint",
        ))
        .unwrap();
        assert!(!callbacks.is_current(&key(), &replacement));
        assert!(!format!("{callbacks:?}").contains("opaque"));
        assert!(!format!("{context:?}").contains("fixture"));
        assert!(context.deadline.duration_since(Instant::now()) <= Duration::from_secs(3600));
    }
    #[test]
    fn invalid_or_expired_connected_metadata_never_produces_native_call_readiness() {
        for root in [
            "http://fixture.trouter.skype.com/root/",
            "https://skype.com.evil.test/root/",
            "https://user@fixture.trouter.skype.com/root/",
            "https://fixture.trouter.skype.com/root/?secret=fixture",
            "https://fixture.trouter.skype.com:8443/root/",
            "https://images.skype.com:3443/root/",
        ] {
            assert!(CallContext::from_connected_event(&payload(root, "fixture")).is_none());
        }
        assert!(
            CallContext::from_connected_event(&payload(
                "https://fixture.trouter.skype.com/root/",
                ""
            ))
            .is_none()
        );
        assert!(CallContext::from_connected_event("{}").is_none());
        let mut expired = context();
        expired.deadline = Instant::now();
        assert!(!expired.is_ready());
        assert!(expired.prepare_callbacks(key()).is_none());
    }

    #[test]
    fn original_trouter_port_and_endpoint_without_final_slash_keep_callback_binding() {
        for root in [
            "https://fixture.trouter.skype.com:3443/v4/opaque%2Fendpoint",
            "https://fixture.trouter.teams.microsoft.com:3443/v4/opaque%2Fendpoint/",
            "https://fixture.trouter.skype.com/v4/opaque%2Fendpoint",
        ] {
            let context = CallContext::parse_connected_event(&payload(root, "fixture")).unwrap();
            let callbacks = context.prepare_callbacks(key()).unwrap();
            let original = Url::parse(root).unwrap();
            let link = callbacks.link(CallbackKind::Acceptance);
            let callback = Url::parse(link).unwrap();
            assert_eq!(callback.origin(), original.origin());
            assert!(
                callback
                    .path()
                    .starts_with("/v4/opaque%2Fendpoint/callAgent/")
            );
            assert!(callbacks.accepts(&key(), CallbackKind::Acceptance, link, &context));
            let mut other_port = callback.clone();
            other_port.set_port(Some(8443)).unwrap();
            assert!(!callbacks.accepts(
                &key(),
                CallbackKind::Acceptance,
                other_port.as_str(),
                &context
            ));
            assert!(!format!("{context:?}").contains("opaque"));
        }
        assert_eq!(
            CallContext::parse_connected_event(&payload(
                "https://fixture.trouter.skype.com:8443/root",
                "fixture"
            ))
            .unwrap_err(),
            MetadataError::Port
        );
        assert_eq!(
            CallContext::parse_connected_event(&payload(
                "https://skype.com.evil.test/root",
                "fixture"
            ))
            .unwrap_err(),
            MetadataError::Host
        );
        assert_eq!(
            CallContext::parse_connected_event("{}").unwrap_err(),
            MetadataError::Format
        );
    }
}
