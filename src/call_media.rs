//! Native audio offer/answer boundary. Retains the advertised socket through
//! signaling and only hands it to media after validating the selected format.
//! DTLS/SRTP is preferred by the native coordinator; legacy SDES is explicit.
use ost::calling::audio_codec::{self, Codec, CodecSet};
use ost::calling::{dtls, ice, media, sdp, sdp_compress, srtp};
use std::net::{IpAddr, SocketAddr};
use tokio::net::UdpSocket;

pub const MAX_SDP_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaError {
    SocketUnavailable,
    InvalidDescription,
    UnsupportedContentType,
    UnsupportedCodec,
    UnsupportedCrypto,
    UnsupportedDirection,
    RtcpMuxRequired,
    NoCandidate,
    IceFailed,
}

pub struct PreparedAudio {
    socket: UdpSocket,
    offer: String,
    credentials: ice::IceCredentials,
    key: Option<srtp::SrtpKeyingMaterial>,
    dtls: Option<dtls::Identity>,
    setup: dtls::Setup,
    relay: Option<crate::call_turn::Allocation>,
    offered_opus: bool,
    ssrc: u32,
    video: Option<crate::call_video::Prepared>,
}

impl std::fmt::Debug for PreparedAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedAudio([REDACTED])")
    }
}

impl PreparedAudio {
    pub async fn bind(bind: SocketAddr, addresses: &[IpAddr]) -> Result<Self, MediaError> {
        if addresses.is_empty()
            || addresses.len() > 32
            || addresses.iter().any(|ip| {
                ip.is_unspecified()
                    || ip.is_multicast()
                    || ip.is_ipv4() != bind.is_ipv4()
                    || (!bind.ip().is_unspecified() && *ip != bind.ip())
            })
        {
            return Err(MediaError::NoCandidate);
        }
        let socket = UdpSocket::bind(bind)
            .await
            .map_err(|_| MediaError::SocketUnavailable)?;
        let port = socket
            .local_addr()
            .map_err(|_| MediaError::SocketUnavailable)?
            .port();
        let credentials = ice::IceCredentials {
            ufrag: sdp::generate_ice_ufrag(),
            pwd: sdp::generate_ice_pwd(),
        };
        let candidates = addresses
            .iter()
            .enumerate()
            .map(|(index, address)| ice::IceCandidate {
                foundation: (index + 1).to_string(),
                component: 1,
                transport: ice::Transport::Udp,
                priority: 2_130_706_431u32.saturating_sub(index as u32),
                address: address.to_string(),
                port,
                candidate_type: ice::CandidateType::Host,
                raddr: None,
                rport: None,
            })
            .collect::<Vec<_>>();
        // The retained primitive emits an IPv4 origin. Do not advertise IPv6
        // until offer generation and its transport family are implemented.
        if !bind.is_ipv4() {
            return Err(MediaError::NoCandidate);
        }
        let generated = sdp::generate_sdp_offer(
            &addresses[0].to_string(),
            port,
            &credentials.ufrag,
            &credentials.pwd,
            &candidates,
        );
        let key = srtp::parse_crypto_line(&generated.crypto_line)
            .map_err(|_| MediaError::UnsupportedCrypto)?;
        let ssrc = ost::calling::video::generate_ssrc().max(1);
        let mut offer = generated.sdp;
        offer.push_str(&format!("a=x-ssrc-range:{ssrc}-{ssrc}\r\n"));
        crate::live_trace::record("call_audio_source_bound_v1", 0, 0, 0);
        Ok(Self {
            socket,
            offer,
            ssrc,
            credentials,
            key: Some(key),
            dtls: None,
            setup: dtls::Setup::Actpass,
            relay: None,
            offered_opus: false,
            video: None,
        })
    }

    /// Add server-reflexive candidates before signaling, preserving the exact
    /// retained socket, ICE credentials and SRTP key from the original offer.
    pub async fn gather_stun(&mut self, server: &str) -> bool {
        let base = self.offer.lines().find_map(|line| {
            line.strip_prefix("a=candidate:")
                .and_then(|value| value.split_whitespace().nth(4))
        });
        let Some(base) = base.and_then(|value| value.parse::<IpAddr>().ok()) else {
            return false;
        };
        let addresses = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::net::lookup_host(server),
        )
        .await;
        let Ok(Ok(addresses)) = addresses else {
            return false;
        };
        let servers = addresses
            .filter(|address| address.is_ipv4())
            .take(4)
            .collect::<Vec<_>>();
        let Some(candidate) = crate::call_nat::gather(&self.socket, &servers, base).await else {
            return false;
        };
        self.offer.push_str("a=");
        self.offer.push_str(&candidate.to_sdp_line());
        self.offer.push_str("\r\n");
        true
    }

    pub async fn close(self) {
        let Self { socket, relay, video, .. } = self;
        if let Some(video)=video{video.close().await;}
        drop(socket);
        if let Some(relay) = relay {
            relay.release().await;
        }
    }
    pub fn add_video(&mut self, video: crate::call_video::Prepared) {
        self.offer.push_str(&video.section);
        self.video=Some(video);
    }
    pub fn has_video(&self)->bool{self.video.is_some()}
    pub fn take_video(&mut self)->Option<crate::call_video::Prepared>{self.video.take()}

    /// Prefer DTLS in new native meeting offers. SDES remains an explicit
    /// legacy API path; a negotiated DTLS session never falls back to it.
    pub fn enable_dtls(&mut self) -> Result<(), MediaError> {
        if self.dtls.is_some() {
            return Ok(());
        }
        let identity = dtls::Identity::generate().map_err(|_| MediaError::UnsupportedCrypto)?;
        self.offer = self
            .offer
            .lines()
            .filter(|line| !line.starts_with("a=crypto:"))
            .map(|line| {
                if line.starts_with("m=audio ") {
                    {
                        let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
                        format!(
                            "m=audio {} UDP/TLS/RTP/SAVPF {}\r\n",
                            fields[1],
                            fields[3..].join(" ")
                        )
                    }
                } else {
                    format!("{line}\r\n")
                }
            })
            .collect();
        self.offer.push_str(&format!(
            "a=fingerprint:{}\r\na=setup:actpass\r\na=tls-id:{}\r\n",
            identity.fingerprint().sdp(),
            crate::call_context::native_id()
        ));
        self.key = None;
        self.dtls = Some(identity);
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn dtls_answer(&mut self, setup: dtls::Setup) -> Result<(), MediaError> {
        if setup == dtls::Setup::Actpass {
            return Err(MediaError::UnsupportedCrypto);
        }
        self.enable_dtls()?;
        self.offer = self
            .offer
            .replace("a=setup:actpass", &format!("a=setup:{}", setup.sdp()));
        self.setup = setup;
        Ok(())
    }

    pub fn enable_opus(&mut self) -> bool {
        if self.offered_opus {
            return true;
        }
        if !audio_codec::opus_available() {
            return false;
        }
        self.offer = self
            .offer
            .lines()
            .map(|line| {
                if let Some(value) = line.strip_prefix("m=audio ") {
                    let parts = value.split_ascii_whitespace().collect::<Vec<_>>();
                    format!("m=audio {} {} 111 0\r\n", parts[0], parts[1])
                } else {
                    format!("{line}\r\n")
                }
            })
            .collect();
        self.offer.push_str("a=rtpmap:111 opus/48000/2\r\na=fmtp:111 maxplaybackrate=48000;sprop-maxcapturerate=8000;stereo=0;useinbandfec=0;usedtx=0\r\na=maxptime:20\r\n");
        self.offered_opus = true;
        true
    }

    pub fn use_relay(
        &mut self,
        allocation: crate::call_turn::Allocation,
    ) -> Result<(), MediaError> {
        let base = self
            .offer
            .lines()
            .find_map(|line| {
                line.strip_prefix("a=candidate:")
                    .and_then(|value| value.split_whitespace().nth(4))
            })
            .ok_or(MediaError::NoCandidate)?
            .to_owned();
        let relay = allocation.relay_addr();
        let port = allocation
            .base_addr()
            .map_err(|_| MediaError::SocketUnavailable)?
            .port();
        let candidate = ice::IceCandidate {
            foundation: "relay1".into(),
            component: 1,
            transport: ice::Transport::Udp,
            priority: 511,
            address: relay.ip().to_string(),
            port: relay.port(),
            candidate_type: ice::CandidateType::Relay,
            raddr: Some(base),
            rport: Some(port),
        };
        // This path deliberately offers relay only: it never advertises a host
        // route while the media owner is listening exclusively on the relay.
        self.offer = self
            .offer
            .lines()
            .filter(|line| !line.starts_with("a=candidate:"))
            .map(|line| {
                if line.starts_with("c=IN IP4 ") {
                    format!("c=IN IP4 {}\r\n", relay.ip())
                } else if line.starts_with("m=audio ") {
                    format!(
                        "m=audio {} {}\r\n",
                        relay.port(),
                        line.split_ascii_whitespace()
                            .skip(2)
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                } else {
                    format!("{}\r\n", line.trim_end_matches('\r'))
                }
            })
            .collect();
        self.offer.push_str("a=");
        self.offer.push_str(&candidate.to_sdp_line());
        self.offer.push_str("\r\n");
        self.relay = Some(allocation);
        Ok(())
    }

    pub fn offer(&self) -> &str {
        &self.offer
    }
    pub fn local_addr(&self) -> Result<SocketAddr, MediaError> {
        self.socket
            .local_addr()
            .map_err(|_| MediaError::SocketUnavailable)
    }

    /// Explicit I/O is required by the native path. A successful ICE check must
    /// not be presented as a voice call while the engine is sending test silence.
    pub async fn connect(
        self,
        remote: RemoteAudio,
        io: media::PcmAudioIo,
        controlling: bool,
    ) -> Result<media::MediaSession, MediaError> {
        let Some(mut codecs) = remote.codecs.restrict(self.offered_opus) else {
            self.close().await;
            return Err(MediaError::UnsupportedCodec);
        };
        // RFC3264 section7 permits retaining receivers for formats in our
        // initial offer even when absent from the answer. Keep send selection
        // constrained to the answer, but do not discard our advertised receive
        // mapping. PreparedAudio always offers PCMU0; Opus111 is advertised only
        // after the system codec loaded successfully. Never infer PTs from RTP.
        codecs.pcmu = true;
        codecs.opus = self.offered_opus;
        let role = match (&self.dtls, &self.key, &remote.protection) {
            (Some(_), None, Protection::Dtls { setup, .. }) => {
                self.setup.local_role(*setup).map(Some)
            }
            (None, Some(local), Protection::Sdes(key)) if local.tag == key.tag => Ok(None),
            _ => Err(dtls::Error::Profile),
        };
        let role = match role {
            Ok(role) => role,
            Err(_) => {
                self.close().await;
                return Err(MediaError::UnsupportedCrypto);
            }
        };
        let transport = if let Some(relay) = self.relay {
            let peers = remote
                .candidates
                .iter()
                .filter_map(|candidate| {
                    candidate
                        .address
                        .parse::<IpAddr>()
                        .ok()
                        .map(|ip| SocketAddr::new(ip, candidate.port))
                })
                .collect::<Vec<_>>();
            let io = relay
                .connect(&peers)
                .await
                .map_err(|_| MediaError::IceFailed)?;
            ost::calling::datagram::Datagram::Relay(io)
        } else {
            ost::calling::datagram::Datagram::Direct(self.socket)
        };
        let connected = match (self.dtls, self.key, remote.protection) {
            (Some(identity), None, Protection::Dtls { fingerprint, .. }) => {
                let role = role.expect("validated DTLS role");
                media::MediaSession::start_with_dtls_for_ssrc(
                    transport,
                    &remote.candidates,
                    &self.credentials,
                    &remote.credentials,
                    identity,
                    fingerprint,
                    role,
                    controlling,
                    io,
                    codecs,
                    self.ssrc,
                )
                .await
            }
            (None, Some(local), Protection::Sdes(remote_key)) if local.tag == remote_key.tag => {
                media::MediaSession::start_with_codecs_for_ssrc(
                    transport,
                    &remote.candidates,
                    &self.credentials,
                    &remote.credentials,
                    &local,
                    &remote_key,
                    controlling,
                    io,
                    codecs,
                    self.ssrc,
                )
                .await
            }
            _ => {
                let _ = transport.close().await;
                return Err(MediaError::UnsupportedCrypto);
            }
        };
        connected.map_err(|error| {
            match error.downcast_ref::<dtls::Error>() {
                Some(dtls::Error::Certificate | dtls::Error::Profile | dtls::Error::Protocol) => {
                    MediaError::UnsupportedCrypto
                }
                // A lost DTLS flight or failed relay is a connectivity failure,
                // not evidence that the peer selected unsupported protection.
                _ => MediaError::IceFailed,
            }
        })
    }
}

enum Protection {
    Sdes(srtp::SrtpKeyingMaterial),
    Dtls {
        fingerprint: dtls::Fingerprint,
        setup: dtls::Setup,
    },
}
#[derive(Default)]
struct DtlsAttributes {
    fingerprint: Option<dtls::Fingerprint>,
    setup: Option<dtls::Setup>,
}
impl DtlsAttributes {
    fn line(&mut self, line: &str) -> Result<(), MediaError> {
        if let Some(value) = line.strip_prefix("a=fingerprint:") {
            let fingerprint =
                dtls::Fingerprint::parse(value).map_err(|_| MediaError::UnsupportedCrypto)?;
            if self.fingerprint.replace(fingerprint).is_some() {
                return Err(MediaError::UnsupportedCrypto);
            }
        }
        if let Some(value) = line.strip_prefix("a=setup:") {
            let setup = dtls::Setup::parse(value).map_err(|_| MediaError::UnsupportedCrypto)?;
            if self.setup.replace(setup).is_some() {
                return Err(MediaError::UnsupportedCrypto);
            }
        }
        Ok(())
    }
}
/// Private, section-aware agreement retained only in memory.
/// No Debug/Serialize implementation: sections can contain keying material.
/// Equality keeps every line. Renegotiation uses the audio transport binding.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AudioAgreement {
    sections: Vec<Vec<String>>,
}
impl AudioAgreement {
    fn from_description(description: &str) -> Self {
        let mut sections = vec![Vec::new()];
        for line in description.lines().map(|line|line.trim_end_matches('\r')) {
            if line.starts_with("m=") { sections.push(vec![line.to_owned()]); }
            else if !line.is_empty() && !["v=","o=","s=","t="].iter().any(|prefix|line.starts_with(prefix)) {
                sections.last_mut().unwrap().push(line.to_owned());
            }
        }
        for (index, section) in sections.iter_mut().enumerate() {
            if index==0 { section.sort(); } else { section[1..].sort(); }
        }
        Self { sections }
    }

    /// Bundle 26288 answers a connected offer when the audio ICE, SRTP and
    /// candidates are unchanged. A longer codec list, a data channel or a
    /// video section does not retarget that binding, so PCMU stays put.
    pub(crate) fn same_session(&self, other: &Self) -> bool {
        self.audio_binding() == other.audio_binding()
    }

    fn audio_binding(&self) -> Vec<String> {
        let mut kept = Vec::new();
        for (index, section) in self.sections.iter().enumerate() {
            let audio = section
                .first()
                .is_some_and(|line| line.starts_with("m=audio "));
            if index != 0 && !audio {
                continue;
            }
            for line in section {
                if let Some(line) = retained_binding_line(line) {
                    kept.push(line);
                }
            }
        }
        kept.sort();
        kept
    }
}

fn retained_binding_line(line: &str) -> Option<String> {
    if let Some(media) = line.strip_prefix("m=audio ") {
        return Some(audio_line_keeping_pcmu(media));
    }
    if line.starts_with("a=ice-ufrag:")
        || line.starts_with("a=ice-pwd:")
        || line.starts_with("a=crypto:")
        || line.starts_with("a=fingerprint:")
        || line.starts_with("a=setup:")
        || line.starts_with("a=candidate:")
        || line.starts_with("a=remote-candidates:")
        || line == "a=rtcp-mux"
        || line.starts_with("a=rtcp:")
        || line.starts_with("a=ptime:")
        || matches!(
            line,
            "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive"
        )
    {
        return Some(line.to_owned());
    }
    None
}

fn audio_line_keeping_pcmu(media: &str) -> String {
    let fields = media.split_ascii_whitespace().collect::<Vec<_>>();
    if fields.len() < 3 {
        return format!("m=audio {media}");
    }
    let payloads = fields[2..]
        .iter()
        .copied()
        .filter(|pt| *pt == "0")
        .collect::<Vec<_>>();
    if payloads.is_empty() {
        return format!("m=audio {media}");
    }
    format!("m=audio {} {} {}", fields[0], fields[1], payloads.join(" "))
}

/// Official `createAnswerAsync(false)` answers every offered section. When the
/// offer is audio only, the description already bound to the socket is repeated.
/// Each later section is rejected with port 0. A shorter answer is what the
/// service rejected with 410/301000 before ending the call. Two audio sections,
/// or a non-audio section before audio, stay on the 488 path.
pub(crate) fn answer_matching_offer(local: &str, offer: &str) -> Option<String> {
    let sections = media_sections(offer);
    let mut kinds = Vec::new();
    for section in &sections {
        kinds.push(media_kind(section)?);
    }
    if kinds.iter().filter(|kind| **kind == "audio").count() != 1 {
        return None;
    }
    if kinds.iter().all(|kind| *kind == "audio") {
        return Some(local.to_owned());
    }
    if kinds.first().copied() != Some("audio") {
        return None;
    }
    if let Some(answer) = answer_with_bound_video(local, &sections) {
        return Some(answer);
    }
    let mut answer = local.to_owned();
    if !answer.ends_with('\n') {
        answer.push_str("\r\n");
    }
    for section in sections.iter().skip(1) {
        answer.push_str(&rejected_media_section(section)?);
    }
    Some(answer)
}

fn media_sections(description: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut current = String::new();
    for line in description.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("m=") {
            if !current.is_empty() {
                sections.push(std::mem::take(&mut current));
            }
        }
        if line.is_empty() {
            continue;
        }
        if line.starts_with("m=") || !current.is_empty() {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.is_empty() {
        sections.push(current);
    }
    sections
}

fn media_kind(section: &str) -> Option<&str> {
    let line = section.lines().next()?;
    let kind = line.strip_prefix("m=")?.split_ascii_whitespace().next()?;
    kind.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        .then_some(kind)
}

fn answer_with_bound_video(local: &str, offer_sections: &[String]) -> Option<String> {
    let local_sections = media_sections(local);
    let videos = local_sections
        .iter()
        .filter(|section| media_kind(section) == Some("video"))
        .count();
    let offered_videos = offer_sections
        .iter()
        .filter(|section| media_kind(section) == Some("video"))
        .count();
    if videos != 1 || offered_videos != 1 {
        return None;
    }
    let audio = local_sections
        .iter()
        .find(|section| media_kind(section) == Some("audio"))?;
    let video = local_sections
        .iter()
        .find(|section| media_kind(section) == Some("video"))?;
    let preamble = local
        .split_once("m=")
        .map(|(head, _)| head.to_owned())
        .unwrap_or_default();
    let mut answer = crlf(&preamble);
    for section in offer_sections {
        let piece = match media_kind(section)? {
            "audio" => crlf(audio),
            "video" => crlf(video),
            _ => rejected_media_section(section)?,
        };
        answer.push_str(&piece);
    }
    Some(answer)
}

fn crlf(section: &str) -> String {
    let mut out = String::new();
    for line in section.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out
}

fn rejected_media_section(section: &str) -> Option<String> {
    let line = section.lines().next()?.trim_end_matches('\r');
    let mut parts = line.split_ascii_whitespace();
    let media = parts.next()?.strip_prefix("m=")?;
    let _port = parts.next()?;
    let proto = parts.next()?;
    if proto.len() > 64 || proto.bytes().any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'/')) {
        return None;
    }
    let format = parts.next().unwrap_or("0");
    if format.len() > 64 || format.bytes().any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')) {
        return None;
    }
    let mut out = format!("m={media} 0 {proto} {format}\r\n");
    if proto.contains("RTP") {
        out.push_str("a=inactive\r\n");
    }
    Some(out)
}
pub struct RemoteAudio {
    credentials: ice::IceCredentials,
    protection: Protection,
    candidates: Vec<ice::IceCandidate>,
    codecs: CodecSet,
    agreement: AudioAgreement,
}

impl std::fmt::Debug for RemoteAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteAudio")
            .field("candidate_count", &self.candidates.len())
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Credentials {
    ufrag: Option<String>,
    pwd: Option<String>,
}

impl Credentials {
    fn line(&mut self, line: &str) -> Result<(), MediaError> {
        let (target, value) = if let Some(value) = line.strip_prefix("a=ice-ufrag:") {
            (&mut self.ufrag, value)
        } else if let Some(value) = line.strip_prefix("a=ice-pwd:") {
            (&mut self.pwd, value)
        } else {
            return Ok(());
        };
        if target.is_some()
            || value.is_empty()
            || value.len() > 256
            || !value.bytes().all(|b| b.is_ascii_graphic() && b != b':')
        {
            return Err(MediaError::InvalidDescription);
        }
        *target = Some(value.into());
        Ok(())
    }
}

// Numeric allowlist only: no SDP, keys, identities or arbitrary strings.
fn trace_audio_formats(description: &str) {
    if !crate::live_trace::enabled() { return; }
    let (flags, ptime)=audio_format_summary(description);
    crate::live_trace::record("call_audio_formats_v1",0,flags,ptime);
}
fn audio_format_summary(description: &str) -> (usize,usize) {
    let mut flags=0usize;let mut ptime=0;let mut audio=false;let mut count=0;
    for line in description.lines() {
        if let Some(media)=line.strip_prefix("m=") {
            let fields:Vec<_>=media.split_ascii_whitespace().collect();
            audio=fields.first()==Some(&"audio");
            if audio { count+=1;
                for pt in fields.iter().skip(3) { match *pt {"0"=>flags|=1,"111"=>flags|=2,_=>{}} }
            }
        }
        if !audio {continue;}
        if let Some(mapping)=line.strip_prefix("a=rtpmap:111 ") {
            flags|=if mapping.eq_ignore_ascii_case("opus/48000/2") {4} else {8};
        }
        if let Some(value)=line.strip_prefix("a=ptime:") {
            ptime=match value {"5"=>5,"10"=>10,"20"=>20,"40"=>40,"60"=>60,"120"=>120,_=>1};
        }
    }
    if count>1 {flags|=16;}
    (flags,ptime)
}

impl RemoteAudio {
    pub(crate) fn agreement(&self) -> AudioAgreement { self.agreement.clone() }
    pub fn parse(content_type: &str, blob: &str) -> Result<Self, MediaError> {
        if !matches!(content_type, "application/sdp" | "application/sdp-ngc-1.0") {
            return Err(MediaError::UnsupportedContentType);
        }
        let description = sdp_compress::decompress_sdp_with_limit(blob, MAX_SDP_BYTES)
            .map_err(|_| MediaError::InvalidDescription)?;
        trace_audio_formats(&description);
        let mut session = Credentials::default();
        let mut audio = Credentials::default();
        let mut media_section = false;
        let mut in_audio = false;
        let mut seen_audio = false;
        let mut mux = false;
        let mut crypto = None;
        let mut dtls_media = false;
        let mut session_dtls = DtlsAttributes::default();
        let mut audio_dtls = DtlsAttributes::default();
        let mut audio_lines = String::new();
        let mut direction = None;
        let mut session_direction = None;
        let mut mapping = false;
        let mut opus_mapping = false;
        let mut seen_payload_111 = false;
        let mut payloads = Vec::new();
        let mut max_bitrate = None;
        for (index, line) in description.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if index > 2048 || line.len() > 4096 || line.chars().any(|ch| ch.is_control()) {
                return Err(MediaError::InvalidDescription);
            }
            if let Some(value) = line.strip_prefix("m=") {
                media_section = true;
                in_audio = false;
                let fields = value.split_ascii_whitespace().collect::<Vec<_>>();
                if fields.len() < 4 {
                    return Err(MediaError::InvalidDescription);
                }
                let port = fields[1]
                    .parse::<u16>()
                    .map_err(|_| MediaError::InvalidDescription)?;
                if fields[0] == "audio" {
                    if seen_audio || port == 0 {
                        return Err(MediaError::InvalidDescription);
                    }
                    payloads = fields[3..]
                        .iter()
                        .map(|value| {
                            value
                                .parse::<u8>()
                                .map_err(|_| MediaError::InvalidDescription)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    dtls_media = matches!(fields[2], "UDP/TLS/RTP/SAVP" | "UDP/TLS/RTP/SAVPF");
                    // A Teams renegotiation lists the whole codec catalogue on
                    // one audio line and then a data channel. Neither changes
                    // the PCMU binding. The cap stays finite.
                    if payloads.len() > 32
                        || !matches!(
                            fields[2],
                            "RTP/SAVP" | "RTP/SAVPF" | "UDP/TLS/RTP/SAVP" | "UDP/TLS/RTP/SAVPF"
                        )
                    {
                        return Err(MediaError::UnsupportedCodec);
                    }
                    seen_audio = true;
                    in_audio = true;
                }
            }
            if !media_section {
                session.line(line)?;
                session_dtls.line(line)?;
            } else if in_audio {
                audio_dtls.line(line)?;
            }
            if let Some(value) = line.strip_prefix("a=") {
                if matches!(value, "sendrecv" | "sendonly" | "recvonly" | "inactive") {
                    let slot = if in_audio {
                        Some(&mut direction)
                    } else if !media_section {
                        Some(&mut session_direction)
                    } else {
                        None
                    };
                    if let Some(slot) = slot {
                        if slot.replace(value).is_some() {
                            return Err(MediaError::InvalidDescription);
                        }
                    }
                }
            }
            if !in_audio {
                continue;
            }
            audio.line(line)?;
            audio_lines.push_str(line);
            audio_lines.push_str("\r\n");
            if line == "a=rtcp-mux" {
                mux = true;
            }
            if let Some(value) = line.strip_prefix("a=maxptime:") {
                if value
                    .parse::<u32>()
                    .ok()
                    .filter(|v| *v >= 20 && *v <= 120)
                    .is_none()
                {
                    return Err(MediaError::UnsupportedCodec);
                }
            }
            if let Some(value) = line.strip_prefix("a=ptime:") {
                if value != "20" {
                    return Err(MediaError::UnsupportedCodec);
                }
            }
            if let Some(value) = line.strip_prefix("a=rtpmap:0 ") {
                if mapping
                    || !(value.eq_ignore_ascii_case("PCMU/8000")
                        || value.eq_ignore_ascii_case("PCMU/8000/1"))
                {
                    return Err(MediaError::UnsupportedCodec);
                }
                mapping = true;
            }
            if let Some(value) = line.strip_prefix("a=rtpmap:111 ") {
                let value = value.trim();
                if seen_payload_111 {
                    return Err(MediaError::UnsupportedCodec);
                }
                seen_payload_111 = true;
                if value.is_empty() {
                    return Err(MediaError::UnsupportedCodec);
                }
                // Payload 111 is not reserved for Opus. The official client
                // keeps PCMU when the offer also names RED, comfort noise or
                // telephone-event, and does not reject the whole session.
                if value.eq_ignore_ascii_case("opus/48000/2")
                    || value.eq_ignore_ascii_case("opus/48000")
                {
                    opus_mapping = true;
                }
            }
            if let Some(value) = line.strip_prefix("a=fmtp:111 ") {
                for parameter in value.split(';') {
                    if let Some((name, value)) = parameter.split_once('=') {
                        if name.trim().eq_ignore_ascii_case("maxaveragebitrate") {
                            if max_bitrate.is_some() {
                                return Err(MediaError::InvalidDescription);
                            }
                            max_bitrate = Some(
                                value
                                    .trim()
                                    .parse::<u32>()
                                    .ok()
                                    .filter(|v| (6000..=510000).contains(v))
                                    .ok_or(MediaError::InvalidDescription)?,
                            );
                        }
                    }
                }
            }
            if line.starts_with("a=crypto:") {
                if crypto.is_some() {
                    return Err(MediaError::UnsupportedCrypto);
                }
                let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
                if fields.len() != 3
                    || fields[1] != "AES_CM_128_HMAC_SHA1_80"
                    || fields[2].split('|').skip(1).any(|part| part.contains(':'))
                {
                    return Err(MediaError::UnsupportedCrypto);
                }
                crypto =
                    Some(srtp::parse_crypto_line(line).map_err(|_| MediaError::UnsupportedCrypto)?);
            }
        }
        if !seen_audio {
            return Err(MediaError::InvalidDescription);
        }
        if direction.or(session_direction).unwrap_or("sendrecv") != "sendrecv" {
            return Err(MediaError::UnsupportedDirection);
        }
        if !mux {
            return Err(MediaError::RtcpMuxRequired);
        }
        let pcmu = payloads.contains(&0);
        let opus = payloads.contains(&111) && opus_mapping && audio_codec::opus_available();
        let send = payloads
            .iter()
            .find_map(|pt| match pt {
                111 if opus => Some(Codec::Opus),
                0 if pcmu => Some(Codec::Pcmu),
                _ => None,
            })
            .ok_or(MediaError::UnsupportedCodec)?;
        let codecs = CodecSet {
            send,
            pcmu,
            opus,
            max_bitrate,
        };
        let credentials = ice::IceCredentials {
            ufrag: audio
                .ufrag
                .or(session.ufrag)
                .ok_or(MediaError::InvalidDescription)?,
            pwd: audio
                .pwd
                .or(session.pwd)
                .ok_or(MediaError::InvalidDescription)?,
        };
        let fingerprint = audio_dtls.fingerprint.or(session_dtls.fingerprint);
        let setup = audio_dtls.setup.or(session_dtls.setup);
        let protection = if dtls_media {
            if crypto.is_some() {
                return Err(MediaError::UnsupportedCrypto);
            }
            Protection::Dtls {
                fingerprint: fingerprint.ok_or(MediaError::UnsupportedCrypto)?,
                setup: setup.ok_or(MediaError::UnsupportedCrypto)?,
            }
        } else {
            if fingerprint.is_some() || setup.is_some() {
                return Err(MediaError::UnsupportedCrypto);
            }
            Protection::Sdes(crypto.ok_or(MediaError::UnsupportedCrypto)?)
        };
        let candidates = ice::parse_candidates_from_sdp_section(&audio_lines, "audio")
            .into_iter()
            .filter(|candidate| {
                candidate.transport == ice::Transport::Udp
                    && candidate.component == 1
                    && candidate.port != 0
            })
            .filter(|candidate| {
                candidate
                    .address
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| !ip.is_multicast() && !ip.is_unspecified())
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() || candidates.len() > 64 {
            return Err(MediaError::NoCandidate);
        }
        Ok(Self {
            credentials,
            protection,
            candidates,
            codecs,
            agreement: AudioAgreement::from_description(&description),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;
    fn io() -> (
        media::PcmAudioIo,
        mpsc::SyncSender<Vec<i16>>,
        mpsc::Receiver<Vec<i16>>,
    ) {
        let (mic_tx, mic_rx) = mpsc::sync_channel(4);
        let (speaker_tx, speaker_rx) = mpsc::sync_channel(4);
        (
            media::PcmAudioIo {
                microphone: mic_rx,
                speaker: speaker_tx,
                initially_muted: false,
            },
            mic_tx,
            speaker_rx,
        )
    }
    #[tokio::test]
    async fn sender_uses_the_source_advertised_in_the_audio_offer() {
        use ost::calling::datagram::DatagramIo;
        let local = prepared().await;
        let source = local.ssrc;
        assert!(local.offer().contains(&format!("a=x-ssrc-range:{source}-{source}\r\n")));
        let peer = prepared().await;
        let remote = RemoteAudio::parse("application/sdp", peer.offer()).unwrap();
        let agent = ice::IceAgent::new(peer.credentials.clone(), local.credentials.clone(), false);
        let worker = tokio::spawn(async move {
            let socket = ost::calling::datagram::Datagram::Direct(peer.socket);
            let mut bytes = [0u8; 2048];
            loop {
                let (n, from) = socket.recv_from(&mut bytes).await.unwrap();
                if ice::is_stun_request(&bytes[..n]) {
                    agent.handle_stun_request(&socket, &bytes[..n], from).await;
                } else if n >= 12 && bytes[0] >> 6 == 2
                    && !ost::calling::audio_control::is_control(&bytes[..n]) {
                    return u32::from_be_bytes(bytes[8..12].try_into().unwrap());
                }
            }
        });
        let (io, _microphone, _speaker) = io();
        let mut session = local.connect(remote, io, true).await.unwrap();
        let received = tokio::time::timeout(Duration::from_secs(3), worker).await.unwrap().unwrap();
        session.stop().await;
        assert_eq!(received, source, "wire RTP must use the negotiated source");
    }
    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        crate::graph::runtime().unwrap().block_on(future)
    }
    async fn prepared() -> PreparedAudio {
        PreparedAudio::bind(
            "127.0.0.1:0".parse().unwrap(),
            &["127.0.0.1".parse().unwrap()],
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn opus_offer_answer_checks_mapping_bitrate_and_local_capabilities() {
        if !audio_codec::opus_available() {
            return;
        }
        let mut remote = prepared().await;
        assert!(remote.enable_opus());
        assert!(remote.offer().contains("maxplaybackrate=48000"));
        assert!(remote.offer().contains("sprop-maxcapturerate=8000"));
        assert!(!remote.offer().contains("maxplaybackrate=8000"));
        let offer = remote
            .offer()
            .replace("a=fmtp:111 ", "a=fmtp:111 MAXAVERAGEBITRATE = 6000;");
        let answer = RemoteAudio::parse("application/sdp", &offer).unwrap();
        assert_eq!(answer.codecs.send, Codec::Opus);
        assert_eq!(answer.codecs.max_bitrate, Some(6000));
        assert_eq!(answer.codecs.restrict(false).unwrap().send, Codec::Pcmu);
        assert!(RemoteAudio::parse("application/sdp", &offer.replace("= 6000", "= 5999")).is_err());
        // An unusable extra codec must not invalidate the still-valid PCMU
        // binding. It must never be selected, including in an Opus-only answer.
        let invalid_opus = offer.replace("opus/48000/2", "opus/8000/1");
        let fallback = RemoteAudio::parse("application/sdp", &invalid_opus).unwrap();
        assert_eq!(fallback.codecs.send, Codec::Pcmu);
        assert!(!fallback.codecs.opus);
        assert!(RemoteAudio::parse("application/sdp",
            &invalid_opus.replace("RTP/SAVP 111 0", "RTP/SAVP 111")).is_err());
        assert!(
            RemoteAudio::parse(
                "application/sdp",
                &offer.replace("a=maxptime:20", "a=maxptime:10")
            )
            .is_err()
        );
        let only_opus = offer.replace("RTP/SAVP 111 0", "RTP/SAVP 111");
        let answer = RemoteAudio::parse("application/sdp", &only_opus).unwrap();
        let local = prepared().await;
        let port = local.local_addr().unwrap();
        assert!(matches!(
            local.connect(answer, io().0, true).await,
            Err(MediaError::UnsupportedCodec)
        ));
        let _rebound = UdpSocket::bind(port).await.unwrap();
    }

    #[tokio::test]
    async fn stun_candidate_keeps_socket_credentials_crypto_and_native_sdp_parseable() {
        let mut prepared = PreparedAudio::bind(
            "127.0.0.1:0".parse().unwrap(),
            &["127.0.0.1".parse().unwrap()],
        )
        .await
        .unwrap();
        let local = prepared.local_addr().unwrap();
        let original = prepared.offer().to_owned();
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buffer = [0; 256];
            let (size, source) = server.recv_from(&mut buffer).await.unwrap();
            assert_eq!(source, local);
            assert_eq!(size, 20);
            let transaction = ice::get_transaction_id(&buffer[..size]).unwrap();
            let response = ice::build_binding_response(
                &transaction,
                "203.0.113.88:45000".parse().unwrap(),
                None,
            );
            server.send_to(&response, source).await.unwrap();
        });
        assert!(prepared.gather_stun(&address.to_string()).await);
        task.await.unwrap();
        assert_eq!(prepared.local_addr().unwrap(), local);
        assert!(prepared.offer().starts_with(&original));
        assert!(prepared.offer().contains("a=candidate:srflx1 1 UDP"));
        assert!(RemoteAudio::parse("application/sdp", prepared.offer()).is_ok());
        assert!(UdpSocket::bind(local).await.is_err());
        drop(prepared);
        let _rebound = UdpSocket::bind(local).await.unwrap();
    }

    #[tokio::test]
    async fn native_pcm_and_authenticated_ice_cross_an_owned_turn_relay_bidirectionally() {
        use std::sync::atomic::Ordering;
        let fixture = crate::call_turn::tests::fixture().await;
        let allocation = crate::call_turn::Allocation::allocate(
            fixture.server,
            crate::call_turn::Credentials::new("fixture-user", "fixture-secret", "rtcmedia")
                .unwrap(),
        )
        .await
        .unwrap();
        exercise_relay(allocation).await;
        assert!(fixture.forwarded.load(Ordering::SeqCst) > 0);
        tokio::time::timeout(Duration::from_secs(1), fixture.released)
            .await
            .unwrap()
            .unwrap();
        fixture.worker.await.unwrap();
    }
    #[tokio::test]
    async fn opus_ice_srtp_and_relay_exchange_real_encoded_audio() {
        if !audio_codec::opus_available() {
            return;
        }
        let fixture = crate::call_turn::tests::fixture().await;
        let allocation = crate::call_turn::Allocation::allocate(
            fixture.server,
            crate::call_turn::Credentials::new("fixture-user", "fixture-secret", "rtcmedia")
                .unwrap(),
        )
        .await
        .unwrap();
        exercise_relay_with_codec(allocation, true).await;
        tokio::time::timeout(Duration::from_secs(1), fixture.released)
            .await
            .unwrap()
            .unwrap();
        fixture.worker.await.unwrap();
    }

    #[tokio::test]
    async fn native_audio_crosses_tcp_and_tls_relays_with_authenticated_ice_and_srtp() {
        for tls in [false, true] {
            for opus in [false, true] {
                if opus && !audio_codec::opus_available() {
                    continue;
                }
                let fixture = crate::call_turn::tests::fixture().await;
                let (addr, bridge) = crate::call_turn_io::tests::bridge(fixture.server, tls).await;
                let io = if tls {
                    crate::call_turn_io::Connection::tls_with_roots(
                        addr,
                        "relay.fixture.invalid",
                        crate::call_turn_io::tests::fixture_roots(),
                    )
                    .await
                    .unwrap()
                } else {
                    crate::call_turn_io::Connection::tcp(addr).await.unwrap()
                };
                let allocation = crate::call_turn::Allocation::allocate_on(
                    io,
                    addr,
                    crate::call_turn::Credentials::new(
                        "fixture-user",
                        "fixture-secret",
                        "rtcmedia",
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
                exercise_relay_with_codec(allocation, opus).await;
                tokio::time::timeout(Duration::from_secs(1), fixture.released)
                    .await
                    .unwrap()
                    .unwrap();
                fixture.worker.await.unwrap();
                tokio::time::timeout(Duration::from_secs(1), bridge)
                    .await
                    .unwrap()
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn native_control_reports_rtt_and_source_exit_cross_udp_tcp_and_tls_relays() {
        for transport in 0..3 {
            let fixture = crate::call_turn::tests::fixture().await;
            let (endpoint, bridge) = if transport == 0 {
                (fixture.server, None)
            } else {
                let (endpoint, task) =
                    crate::call_turn_io::tests::bridge(fixture.server, transport == 2).await;
                (endpoint, Some(task))
            };
            let credentials =
                crate::call_turn::Credentials::new("fixture-user", "fixture-secret", "rtcmedia")
                    .unwrap();
            let allocation = if transport == 0 {
                crate::call_turn::Allocation::allocate(endpoint, credentials)
                    .await
                    .unwrap()
            } else {
                let connection = if transport == 2 {
                    crate::call_turn_io::Connection::tls_with_roots(
                        endpoint,
                        "relay.fixture.invalid",
                        crate::call_turn_io::tests::fixture_roots(),
                    )
                    .await
                    .unwrap()
                } else {
                    crate::call_turn_io::Connection::tcp(endpoint)
                        .await
                        .unwrap()
                };
                crate::call_turn::Allocation::allocate_on(connection, endpoint, credentials)
                    .await
                    .unwrap()
            };
            let mut left = prepared().await;
            let right = prepared().await;
            left.use_relay(allocation).unwrap();
            let left_offer = RemoteAudio::parse("application/sdp", left.offer()).unwrap();
            let right_offer = RemoteAudio::parse("application/sdp", right.offer()).unwrap();
            let (left_io, _left_mic, _left_speaker) = io();
            let (right_io, _right_mic, _right_speaker) = io();
            let (left, right) = tokio::join!(
                left.connect(right_offer, left_io, true),
                right.connect(left_offer, right_io, false)
            );
            let mut left = left.unwrap();
            let mut right = right.unwrap();
            tokio::time::timeout(Duration::from_secs(12), async {
                loop {
                    let a = left.stats().await;
                    let b = right.stats().await;
                    if a.control.reports_received > 0
                        && b.control.reports_received > 0
                        && a.control.round_trip_ms.is_some()
                        && b.control.round_trip_ms.is_some()
                    {
                        assert!(a.packets_received > 0 && b.packets_received > 0);
                        assert_eq!(a.control.remote_fraction_lost, Some(0));
                        assert_eq!(b.control.remote_fraction_lost, Some(0));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            right.stop().await;
            assert_eq!(right.stats().await.control.bye_packets_sent, 1);
            tokio::time::timeout(Duration::from_secs(1), async {
                while left.stats().await.control.bye_sources_received == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            assert!(
                left.is_running(),
                "a source BYE must not end the meeting controller"
            );
            left.stop().await;
            assert_eq!(left.stats().await.control.bye_packets_sent, 1);
            tokio::time::timeout(Duration::from_secs(1), fixture.released)
                .await
                .unwrap()
                .unwrap();
            fixture.worker.await.unwrap();
            if let Some(bridge) = bridge {
                tokio::time::timeout(Duration::from_secs(1), bridge)
                    .await
                    .unwrap()
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn dtls_srtp_audio_and_control_cross_native_udp_tcp_tls_relays() {
        for transport in 0..3 {
            for opus in [false, true] {
                if opus && !audio_codec::opus_available() {
                    continue;
                }
                let fixture = crate::call_turn::tests::fixture().await;
                let (endpoint, bridge) = if transport == 0 {
                    (fixture.server, None)
                } else {
                    let (endpoint, task) =
                        crate::call_turn_io::tests::bridge(fixture.server, transport == 2).await;
                    (endpoint, Some(task))
                };
                let credentials = crate::call_turn::Credentials::new(
                    "fixture-user",
                    "fixture-secret",
                    "rtcmedia",
                )
                .unwrap();
                let allocation = if transport == 0 {
                    crate::call_turn::Allocation::allocate(endpoint, credentials)
                        .await
                        .unwrap()
                } else {
                    let connection = if transport == 2 {
                        crate::call_turn_io::Connection::tls_with_roots(
                            endpoint,
                            "relay.fixture.invalid",
                            crate::call_turn_io::tests::fixture_roots(),
                        )
                        .await
                        .unwrap()
                    } else {
                        crate::call_turn_io::Connection::tcp(endpoint)
                            .await
                            .unwrap()
                    };
                    crate::call_turn::Allocation::allocate_on(connection, endpoint, credentials)
                        .await
                        .unwrap()
                };
                exercise_relay_with_security(allocation, opus, None, true).await;
                tokio::time::timeout(Duration::from_secs(1), fixture.released)
                    .await
                    .unwrap()
                    .unwrap();
                fixture.worker.await.unwrap();
                if let Some(task) = bridge {
                    tokio::time::timeout(Duration::from_secs(1), task)
                        .await
                        .unwrap()
                        .unwrap();
                }
            }
        }
    }
    #[tokio::test]
    async fn cancelling_after_dtls_clienthello_sends_no_audio_and_releases_reservation() {
        use ost::calling::datagram::DatagramIo;
        let mut local = prepared().await;
        local.enable_dtls().unwrap();
        let address = local.local_addr().unwrap();
        let mut peer = prepared().await;
        peer.dtls_answer(dtls::Setup::Passive).unwrap();
        let remote = RemoteAudio::parse("application/sdp", peer.offer()).unwrap();
        let agent = ice::IceAgent::new(peer.credentials.clone(), local.credentials.clone(), false);
        let (hello, hello_rx) = tokio::sync::oneshot::channel();
        let peer_socket = ost::calling::datagram::Datagram::Direct(peer.socket);
        let worker = tokio::spawn(async move {
            let mut hello = Some(hello);
            let mut input = [0; 2048];
            loop {
                let (n, from) = peer_socket.recv_from(&mut input).await.unwrap();
                if ice::is_stun_request(&input[..n]) {
                    agent
                        .handle_stun_request(&peer_socket, &input[..n], from)
                        .await;
                } else {
                    assert!(
                        dtls::is_dtls(&input[..n]),
                        "audio was sent before pinned DTLS completed"
                    );
                    if let Some(hello) = hello.take() {
                        let _ = hello.send(());
                    }
                }
            }
        });
        let (io, microphone, _speaker) = io();
        let job = tokio::spawn(local.connect(remote, io, true));
        tokio::time::timeout(Duration::from_secs(2), hello_rx)
            .await
            .unwrap()
            .unwrap();
        job.abort();
        match job.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("DTLS cancellation completed unexpectedly"),
        };
        assert!(matches!(
            microphone.try_send(vec![0; 160]),
            Err(std::sync::mpsc::TrySendError::Disconnected(_))
        ));
        let _rebound = UdpSocket::bind(address).await.unwrap();
        worker.abort();
        let _ = worker.await;
    }

    #[tokio::test]
    async fn dtls_sdp_scope_roles_and_no_downgrade_are_explicit() {
        let mut left = prepared().await;
        left.enable_dtls().unwrap();
        assert!(!left.offer().contains("a=crypto:"));
        assert!(left.offer().contains("UDP/TLS/RTP/SAVPF"));
        let fp = left
            .offer()
            .lines()
            .find(|l| l.starts_with("a=fingerprint:"))
            .unwrap()
            .to_owned();
        for changed in [
            left.offer().replace(&fp, ""),
            left.offer().replace("a=setup:actpass", "a=setup:holdconn"),
            left.offer().replace("sha-256", "sha-1"),
            format!("{}{}\r\n", left.offer(), fp),
        ] {
            assert!(matches!(
                RemoteAudio::parse("application/sdp", &changed),
                Err(MediaError::UnsupportedCrypto)
            ));
        }
        let mut session = left
            .offer()
            .replace(&format!("{fp}\r\n"), "")
            .replace("a=setup:actpass\r\n", "");
        let boundary = session.find("m=audio").unwrap();
        session.insert_str(boundary, &format!("{fp}\r\na=setup:actpass\r\n"));
        assert!(RemoteAudio::parse("application/sdp", &session).is_ok());
        let plain = prepared().await;
        let downgrade = RemoteAudio::parse("application/sdp", plain.offer()).unwrap();
        let address = left.local_addr().unwrap();
        let (io, _, _) = io();
        assert!(matches!(
            left.connect(downgrade, io, true).await,
            Err(MediaError::UnsupportedCrypto)
        ));
        let _rebound = UdpSocket::bind(address).await.unwrap();
    }

    async fn exercise_relay(allocation: crate::call_turn::Allocation) {
        exercise_relay_with_codec(allocation, false).await;
    }
    async fn exercise_relay_with_codec(allocation: crate::call_turn::Allocation, opus: bool) {
        exercise_relay_with_delay(allocation, opus, None).await;
    }
    async fn exercise_relay_with_delay(
        allocation: crate::call_turn::Allocation,
        opus: bool,
        expire_before_close: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) {
        exercise_relay_with_security(allocation, opus, expire_before_close, false).await;
    }

    async fn exercise_relay_with_security(
        allocation: crate::call_turn::Allocation,
        opus: bool,
        expire_before_close: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        dtls: bool,
    ) {
        let relay_base = allocation.base_addr().unwrap();
        let reliable = allocation.reliable();
        let mut left = prepared().await;
        let mut right = prepared().await;
        if opus {
            assert!(left.enable_opus());
            assert!(right.enable_opus());
        }
        left.use_relay(allocation).unwrap();
        assert!(left.offer().contains("typ relay"));
        assert!(!left.offer().contains("typ host"));
        if dtls {
            left.enable_dtls().unwrap();
            right.dtls_answer(dtls::Setup::Passive).unwrap();
            assert!(!left.offer().contains("a=crypto:"));
            assert!(left.offer().contains("a=fingerprint:sha-256"));
        }
        let left_description = RemoteAudio::parse("application/sdp", left.offer()).unwrap();
        let right_description = RemoteAudio::parse("application/sdp", right.offer()).unwrap();
        let (left_io, left_mic, left_speaker) = io();
        let (right_io, right_mic, right_speaker) = io();
        left_mic.send(vec![9000; 160]).unwrap();
        right_mic.send(vec![-9000; 160]).unwrap();
        let (left, right) = tokio::time::timeout(Duration::from_secs(4), async {
            tokio::join!(
                left.connect(right_description, left_io, true),
                right.connect(left_description, right_io, false)
            )
        })
        .await
        .unwrap();
        let mut left = left.unwrap();
        let mut right = right.unwrap();
        let mut left_heard = false;
        let mut right_heard = false;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !left_heard || !right_heard {
                let wave = (0..160)
                    .map(|index| {
                        ((2.0 * std::f32::consts::PI * 440.0 * index as f32 / 8000.0).sin()
                            * 9000.0) as i16
                    })
                    .collect::<Vec<_>>();
                let _ = left_mic.try_send(wave.clone());
                let _ = right_mic.try_send(wave);

                if let Ok(frame) = left_speaker.try_recv() {
                    left_heard |= frame.iter().any(|v| *v < -5000);
                }
                if let Ok(frame) = right_speaker.try_recv() {
                    right_heard |= frame.iter().any(|v| *v > 5000);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        if dtls {
            tokio::time::timeout(Duration::from_secs(6), async {
                while left.stats().await.control.reports_received == 0
                    || right.stats().await.control.reports_received == 0
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        if opus {
            tokio::time::timeout(Duration::from_secs(1), async {
                while left.stats().await.packets_received < 3 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let stats = left.stats().await;
            assert_eq!(stats.rtp_quality.clock_rate, 48000);
            assert_eq!(stats.last_timestamp % 960, 0);
        }
        if let Some(data_allowed) = &expire_before_close {
            right.stop().await;
            data_allowed.store(false, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(3500)).await;
        }
        left.stop().await;
        if dtls && expire_before_close.is_none() {
            // Stop must send an authenticated close over the selected route.
            // The peer's sole receiver exits, so it cannot keep playing media
            // after that association has been closed.
            tokio::time::timeout(Duration::from_secs(1), async {
                while right.is_running() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }).await.unwrap();
            assert!(right.audio_controls().microphone_muted());
        }
        if expire_before_close.is_none() {
            right.stop().await;
        }
        if !reliable {
            let _rebound = UdpSocket::bind(relay_base).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires an isolated coturn binary in TEAMS_LINUX_COTURN; loopback only"]
    async fn native_audio_interoperates_with_local_coturn() {
        use base64::Engine;
        use std::process::{Child, Command, Stdio};
        struct ChildOwner {
            child: Child,
            directory: std::path::PathBuf,
        }
        impl Drop for ChildOwner {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                let _ = std::fs::remove_dir_all(&self.directory);
            }
        }
        let binary = std::env::var("TEAMS_LINUX_COTURN")
            .expect("set TEAMS_LINUX_COTURN to an installed or extracted coturn executable");
        let reservation = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = reservation.local_addr().unwrap();
        drop(reservation);
        let tls_reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tls_address = tls_reservation.local_addr().unwrap();
        drop(tls_reservation);
        let directory =
            std::env::temp_dir().join(format!("teams-coturn-{}", crate::call_context::native_id()));
        std::fs::create_dir(&directory).unwrap();
        let pidfile = directory.join("server.pid");
        let cert = directory.join("cert.pem");
        let key = directory.join("key.pem");
        for (path, label, der) in [
            (
                &cert,
                "CERTIFICATE",
                include_bytes!("../tests/fixtures/turn/cert.der").as_slice(),
            ),
            (
                &key,
                "PRIVATE KEY",
                include_bytes!("../tests/fixtures/turn/key.der").as_slice(),
            ),
        ] {
            let encoded = base64::engine::general_purpose::STANDARD.encode(der);
            std::fs::write(
                path,
                format!("-----BEGIN {label}-----\n{encoded}\n-----END {label}-----\n"),
            )
            .unwrap();
        }
        let child = Command::new(binary)
            .args([
                "-n",
                "--lt-cred-mech",
                "--stale-nonce=1",
                "--realm=rtcmedia",
                "--user=fixture-user:fixture-secret",
                "--listening-ip=127.0.0.1",
                "--relay-ip=127.0.0.1",
                "--listening-port",
                &address.port().to_string(),
                "--relay-threads=1",
                "--min-port=62000",
                "--max-port=62020",
                "--no-cli",
                "--cli-password=fixture-only",
                "--allow-loopback-peers",
                "--tls-listening-port",
                &tls_address.port().to_string(),
                "--cert",
                cert.to_str().unwrap(),
                "--pkey",
                key.to_str().unwrap(),
                "--log-file=stdout",
                "--simple-log",
                "--pidfile",
            ])
            .arg(&pidfile)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut child = ChildOwner { child, directory };
        let allocated = crate::call_turn::Allocation::allocate(
            address,
            crate::call_turn::Credentials::new("fixture-user", "fixture-secret", "rtcmedia")
                .unwrap(),
        )
        .await;
        assert!(
            child.child.try_wait().unwrap().is_none(),
            "coturn exited before the test"
        );
        exercise_relay(allocated.unwrap()).await;
        if audio_codec::opus_available() {
            let allocation = crate::call_turn::Allocation::allocate(
                address,
                crate::call_turn::Credentials::new("fixture-user", "fixture-secret", "rtcmedia")
                    .unwrap(),
            )
            .await
            .unwrap();
            exercise_relay_with_codec(allocation, true).await;
        }
        let dtls_allocation = crate::call_turn::Allocation::allocate(
            address,
            crate::call_turn::Credentials::new("fixture-user", "fixture-secret", "rtcmedia")
                .unwrap(),
        )
        .await
        .unwrap();
        exercise_relay_with_security(dtls_allocation, audio_codec::opus_available(), None, true)
            .await;
        for tls in [false, true] {
            for opus in [false, true] {
                if opus && !audio_codec::opus_available() {
                    continue;
                }
                let io = if tls {
                    crate::call_turn_io::Connection::tls_with_roots(
                        tls_address,
                        "relay.fixture.invalid",
                        crate::call_turn_io::tests::fixture_roots(),
                    )
                    .await
                    .unwrap()
                } else {
                    crate::call_turn_io::Connection::tcp(address).await.unwrap()
                };
                let endpoint = if tls { tls_address } else { address };
                let allocation = crate::call_turn::Allocation::allocate_on(
                    io,
                    endpoint,
                    crate::call_turn::Credentials::new(
                        "fixture-user",
                        "fixture-secret",
                        "rtcmedia",
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
                exercise_relay_with_security(allocation, opus, None, true).await;
            }
        }
        // Observe only the wire error code, never credentials or packet bodies.
        // A loopback forwarder keeps one UDP source port for the allocation.
        let forwarder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_endpoint = forwarder.local_addr().unwrap();
        let challenged = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = challenged.clone();
        let data_allowed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let forwarding = data_allowed.clone();
        let deleted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let confirmed_delete = deleted.clone();
        let status = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let trace = status.clone();
        let forward = tokio::spawn(async move {
            let mut client = None;
            let mut deletions = Vec::new();
            let mut buffer = [0; 2048];
            loop {
                let (size, source) = forwarder.recv_from(&mut buffer).await.unwrap();
                let kind = u16::from_be_bytes([buffer[0], buffer[1]]);
                if kind != 0x16 && kind != 0x17 {
                    let mut status = trace.lock().unwrap();
                    if status.len() < 32 {
                        status.push((
                            source == address,
                            kind,
                            crate::call_turn::tests::is_stale_response(&buffer[..size]),
                        ));
                    }
                }
                if source == address {
                    if kind == 0x104 && size >= 20 && deletions.contains(&buffer[8..20].to_vec()) {
                        confirmed_delete.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    if crate::call_turn::tests::is_stale_response(&buffer[..size]) {
                        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    if let Some(client) = client {
                        forwarder.send_to(&buffer[..size], client).await.unwrap();
                    }
                } else if client.is_none() || client == Some(source) {
                    client = Some(source);
                    if crate::call_turn::tests::is_delete_request(&buffer[..size]) {
                        deletions.push(buffer[8..20].to_vec());
                    }
                    if kind == 0x16 && !forwarding.load(std::sync::atomic::Ordering::SeqCst) {
                        continue;
                    }
                    forwarder.send_to(&buffer[..size], address).await.unwrap();
                }
            }
        });
        let allocation = crate::call_turn::Allocation::allocate(
            relay_endpoint,
            crate::call_turn::Credentials::new("fixture-user", "fixture-secret", "rtcmedia")
                .unwrap(),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(3500)).await;
        exercise_relay_with_delay(allocation, false, Some(data_allowed)).await;
        assert!(
            challenged.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "coturn did not send both expired-nonce challenges: {:?}",
            status.lock().unwrap()
        );
        assert!(
            deleted.load(std::sync::atomic::Ordering::SeqCst),
            "coturn did not confirm release of the allocation"
        );
        forward.abort();
        let _ = forward.await;
        drop(child);
    }

    #[test]
    fn native_audio_exchanges_actual_pcm_over_authenticated_srtp_and_releases_both_advertised_ports()
     {
        run(async {
            let left = prepared().await;
            let right = prepared().await;
            let left_addr = left.local_addr().unwrap();
            let right_addr = right.local_addr().unwrap();
            assert!(UdpSocket::bind(left_addr).await.is_err());
            assert!(UdpSocket::bind(right_addr).await.is_err());
            let left_description = RemoteAudio::parse("application/sdp", left.offer()).unwrap();
            let right_description = RemoteAudio::parse("application/sdp", right.offer()).unwrap();
            let (left_io, left_mic, left_speaker) = io();
            let (right_io, right_mic, right_speaker) = io();
            let (left, right) = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(
                    left.connect(right_description, left_io, true),
                    right.connect(left_description, right_io, false)
                )
            })
            .await
            .unwrap();
            let mut left = left.unwrap();
            let mut right = right.unwrap();
            assert_eq!(left.local_port().unwrap(), left_addr.port());
            assert_eq!(right.local_port().unwrap(), right_addr.port());
            async fn nonzero(receiver: &mpsc::Receiver<Vec<i16>>) -> Vec<i16> {
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if let Ok(samples) = receiver.try_recv() {
                            if samples.iter().any(|s| s.abs() > 500) {
                                break samples;
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap()
            }
            // A real device drains clocked silence throughout setup. Start the
            // test speech after both peers connect, draining both outputs together.
            let _ = left_speaker.try_iter().count();
            let _ = right_speaker.try_iter().count();
            left_mic.send(vec![9000; 160]).unwrap();
            right_mic.send(vec![-9000; 160]).unwrap();
            let (l, r) = tokio::join!(nonzero(&left_speaker), nonzero(&right_speaker));
            assert_eq!(l.len(), 160);
            assert_eq!(r.len(), 160);
            assert!(l.iter().all(|s| (*s + 9000).abs() < 512));
            assert!(r.iter().all(|s| (*s - 9000).abs() < 512));
            left.stop().await;
            right.stop().await;
            let _l = UdpSocket::bind(left_addr).await.unwrap();
            let _r = UdpSocket::bind(right_addr).await.unwrap();
        });
    }

    #[test]
    fn answer_is_scoped_to_audio_and_rejects_unimplemented_formats_before_media() {
        run(async {
            let local = prepared().await;
            let offer = local.offer();
            for (from, to, error) in [
                (
                    "RTP/SAVP 0",
                    "UDP/TLS/RTP/SAVPF 111",
                    MediaError::UnsupportedCodec,
                ),
                ("a=rtcp-mux\r\n", "", MediaError::RtcpMuxRequired),
                ("a=sendrecv", "a=inactive", MediaError::UnsupportedDirection),
                ("PCMU/8000", "opus/48000/2", MediaError::UnsupportedCodec),
                (
                    "AES_CM_128_HMAC_SHA1_80",
                    "AES_CM_128_HMAC_SHA1_32",
                    MediaError::UnsupportedCrypto,
                ),
                ("a=ptime:20", "a=ptime:60", MediaError::UnsupportedCodec),
            ] {
                assert_eq!(
                    RemoteAudio::parse("application/sdp", &offer.replace(from, to)).unwrap_err(),
                    error
                );
            }
            let mixed = format!(
                "{offer}m=video 0 RTP/SAVP 122\r\na=ice-pwd:wrong-video-generation\r\na=ice-ufrag:video\r\n"
            );
            let remote = RemoteAudio::parse("application/sdp", &mixed).unwrap();
            assert_eq!(remote.credentials.pwd, local.credentials.pwd);
            assert!(!format!("{local:?} {remote:?}").contains(&local.credentials.pwd));
            assert!(!format!("{local:?} {remote:?}").contains("inline:"));
        });
    }

    #[test]
    fn compressed_description_has_a_hard_expansion_limit_and_duplicate_credentials_fail() {
        run(async {
            let local = prepared().await;
            let long = format!("{}{}", local.offer(), "a=x-fixture:padding\r\n".repeat(100));
            let compressed = sdp_compress::compress_sdp(&long).unwrap().unwrap();
            assert!(RemoteAudio::parse("application/sdp-ngc-1.0", &compressed).is_ok());
            assert!(sdp_compress::decompress_sdp_with_limit(&compressed, 256).is_err());
            assert!(sdp_compress::decompress_sdp_with_limit("", 256).is_err());
            assert!(
                sdp_compress::decompress_sdp_with_limit(
                    &compressed[..compressed.len() / 2],
                    MAX_SDP_BYTES
                )
                .is_err()
            );
            let duplicate = format!("{}a=ice-pwd:duplicate\r\n", local.offer());
            assert_eq!(
                RemoteAudio::parse("application/sdp", &duplicate).unwrap_err(),
                MediaError::InvalidDescription
            );
            assert_eq!(
                RemoteAudio::parse("application/json", local.offer()).unwrap_err(),
                MediaError::UnsupportedContentType
            );
        });
    }

    #[test]
    fn unsupported_answer_drops_reservation_without_sending_or_opening_hardware() {
        run(async {
            let local = prepared().await;
            let address = local.local_addr().unwrap();
            assert!(
                RemoteAudio::parse("application/sdp", "v=0\r\nm=audio 0 RTP/SAVP 0\r\n").is_err()
            );
            drop(local);
            let _rebound = UdpSocket::bind(address).await.unwrap();
        });
    }
    #[test]
    fn agreement_ignores_session_version_and_attribute_order_but_keeps_media_changes() {
        let base="v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=fixture\r\nt=0 0\r\nm=audio 9 RTP/SAVP 0\r\na=ice-pwd:secret\r\na=crypto:fixture\r\na=rtcp-mux\r\n";
        let reordered="v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=fixture\r\nt=0 0\r\nm=audio 9 RTP/SAVP 0\r\na=rtcp-mux\r\na=crypto:fixture\r\na=ice-pwd:secret\r\n";
        let agreement=AudioAgreement::from_description(base);
        assert!(agreement==AudioAgreement::from_description(reordered));
        for changed in [base.replace("secret","other"),base.replace("crypto:fixture","crypto:other"),base.replace("SAVP 0","SAVP 111"),format!("{base}m=video 10 RTP/SAVP 100\r\n"),format!("{base}a=recvonly\r\n")] {
            assert!(agreement!=AudioAgreement::from_description(&changed));
        }
    }

    #[test]
    fn renegotiation_answer_rejects_extra_sections_and_keeps_the_bound_audio() {
        let local = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=fixture\r\nt=0 0\r\nm=audio 9 RTP/SAVP 0\r\na=crypto:fixture\r\n";
        let audio_only = format!("{local}a=rtpmap:111 SILK/16000\r\n");
        assert_eq!(answer_matching_offer(local, &audio_only).as_deref(), Some(local));
        let offer = format!("{audio_only}m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nm=video 9 RTP/SAVP 122\r\na=ice-pwd:other\r\n");
        let answer = answer_matching_offer(local, &offer).unwrap();
        assert!(answer.starts_with(local));
        assert!(answer.contains("m=application 0 UDP/DTLS/SCTP webrtc-datachannel\r\n"));
        assert!(answer.contains("m=video 0 RTP/SAVP 122\r\na=inactive\r\n"));
        let with_data = format!("{audio_only}m=x-data 9 UDP/DTLS/SCTP 127\r\n");
        let data_answer = answer_matching_offer(local, &with_data).unwrap();
        assert!(data_answer.contains("m=x-data 0 UDP/DTLS/SCTP 127\r\n"));
        assert!(!answer.contains("SILK/16000"));
        assert!(!answer.contains("ice-pwd:other"));
        assert!(answer_matching_offer(local, &format!("{offer}m=audio 9 RTP/SAVP 0\r\n")).is_none());
        assert!(answer_matching_offer(local, "m=video 9 RTP/SAVP 122\r\nm=audio 9 RTP/SAVP 0\r\n").is_none());
        let bound = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=fixture\r\nt=0 0\r\nm=audio 9 RTP/SAVP 0\r\na=crypto:audio\r\nm=video 9 RTP/SAVP 122\r\na=recvonly\r\na=crypto:video\r\n";
        let with_video = "v=0\r\nm=audio 9 RTP/SAVP 0\r\na=rtpmap:111 SILK/16000\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nm=video 9 RTP/SAVP 122\r\na=ice-pwd:other\r\n";
        let kept = answer_matching_offer(bound, with_video).unwrap();
        assert!(kept.contains("a=crypto:video"));
        assert!(kept.contains("a=recvonly"));
        assert!(!kept.contains("m=video 0"));
        assert!(!kept.contains("ice-pwd:other"));
        assert!(!kept.contains("SILK"));
        assert!(kept.contains("m=application 0 UDP/DTLS/SCTP webrtc-datachannel\r\n"));
        let audio_at = kept.find("m=audio").unwrap();
        let video_at = kept.find("m=video 9").unwrap();
        let app_at = kept.find("m=application 0").unwrap();
        assert!(audio_at < app_at && app_at < video_at);
    }

    #[test]
    fn codec_summary_is_numeric_and_distinguishes_offer_payloads_mapping_and_duration() {
        let description="m=audio 9 RTP/SAVP 0 111\r\na=rtpmap:111 opus/48000/2\r\na=ptime:40\r\na=crypto:private-fixture\r\nm=video 10 RTP/SAVP 111\r\na=rtpmap:111 private-video\r\n";
        assert_eq!(audio_format_summary(description),(7,40));
        assert_eq!(audio_format_summary("m=audio 9 RTP/SAVP 0\r\n"),(1,0));
        assert_eq!(audio_format_summary("m=audio 9 RTP/SAVP 111\r\na=rtpmap:111 opus/48000\r\na=ptime:private\r\n"),(10,1));
    }

    #[test]
    fn red_on_payload_111_keeps_pcmu_and_optional_opus_channels_stay_opus() {
        run(async {
            let local = prepared().await;
            let red = format!(
                "{}a=rtpmap:111 red/48000/2\r\na=fmtp:111 111/111\r\n",
                local.offer().replace("RTP/SAVP 0", "RTP/SAVP 0 111")
            );
            let remote = RemoteAudio::parse("application/sdp", &red).unwrap();
            assert_eq!(remote.codecs.send, Codec::Pcmu);
            assert!(!remote.codecs.opus);
            let only_red = local.offer().replace("RTP/SAVP 0", "RTP/SAVP 111").replace(
                "a=rtpmap:0 PCMU/8000",
                "a=rtpmap:111 red/48000/2",
            );
            assert_eq!(
                RemoteAudio::parse("application/sdp", &only_red).unwrap_err(),
                MediaError::UnsupportedCodec
            );
            let base = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=fixture\r\nt=0 0\r\nm=audio 9 RTP/SAVP 0\r\na=ice-pwd:secret\r\na=crypto:fixture\r\na=rtcp-mux\r\n";
            let active = AudioAgreement::from_description(base);
            let with_red = format!("{base}a=rtpmap:111 red/48000/2\r\n").replace("SAVP 0", "SAVP 0 111");
            assert!(active.same_session(&AudioAgreement::from_description(&with_red)));
            assert!(active != AudioAgreement::from_description(&with_red));
            let other_key = with_red.replace("crypto:fixture", "crypto:other");
            assert!(!active.same_session(&AudioAgreement::from_description(&other_key)));
            let event = format!(
                "{}a=rtpmap:111 telephone-event/8000\r\na=fmtp:111 0-15\r\n",
                local.offer().replace("RTP/SAVP 0", "RTP/SAVP 0 111")
            );
            let remote_event = RemoteAudio::parse("application/sdp", &event).unwrap();
            assert_eq!(remote_event.codecs.send, Codec::Pcmu);
            assert!(!remote_event.codecs.opus);
            let with_event = format!("{base}a=rtpmap:111 telephone-event/8000\r\n").replace("SAVP 0", "SAVP 0 111");
            assert!(active.same_session(&AudioAgreement::from_description(&with_event)));
            let replaced = format!("{base}a=rtpmap:111 telephone-event/8000\r\n").replace("SAVP 0", "SAVP 111");
            assert!(!active.same_session(&AudioAgreement::from_description(&replaced)));
            let opus = format!("{base}a=rtpmap:111 opus/48000/2\r\n");
            let opus_without_channels = format!("{base}a=rtpmap:111 opus/48000\r\n");
            assert!(AudioAgreement::from_description(&opus)
                .same_session(&AudioAgreement::from_description(&opus_without_channels)));
            let catalogue = format!(
                "{}\r\na=mid:0\r\na=rtcp-fb:111 nack\r\na=ssrc:1 cname:fixture\r\na=rtpmap:111 SILK/16000\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nm=video 9 RTP/SAVP 122\r\n",
                base.replace(
                    "SAVP 0",
                    "SAVP 0 111 104 114 9 103 8 116 115 106 113 118 119 97 101 120 121"
                )
            );
            assert!(active.same_session(&AudioAgreement::from_description(&catalogue)));
            assert!(active != AudioAgreement::from_description(&catalogue));
            let moved = catalogue.replace("a=ice-pwd:secret", "a=ice-pwd:other");
            assert!(!active.same_session(&AudioAgreement::from_description(&moved)));
            let with_candidate = format!(
                "{base}a=candidate:1 1 UDP 2130706431 192.0.2.10 9 typ host\r\n"
            );
            assert!(!active.same_session(&AudioAgreement::from_description(&with_candidate)));
            let payloads = (0..33)
                .map(|pt| pt.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            let too_many = local
                .offer()
                .replace("RTP/SAVP 0", &format!("RTP/SAVP {payloads}"));
            assert_eq!(
                RemoteAudio::parse("application/sdp", &too_many).unwrap_err(),
                MediaError::UnsupportedCodec
            );
            let listed = format!(
                "{}\r\na=rtpmap:111 SILK/16000\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n",
                local.offer().replace(
                    "RTP/SAVP 0",
                    "RTP/SAVP 0 111 104 114 9 103 8 116 115 106 113 118 119 97 101 120 121"
                )
            );
            let parsed = RemoteAudio::parse("application/sdp", &listed).unwrap();
            assert_eq!(parsed.codecs.send, Codec::Pcmu);
            assert!(!parsed.codecs.opus);
        });
        if audio_codec::opus_available() {
            run(async {
                let mut local = prepared().await;
                assert!(local.enable_opus());
                let offered = local.offer().replace("opus/48000/2", "opus/48000");
                let remote = RemoteAudio::parse("application/sdp", &offered).unwrap();
                assert_eq!(remote.codecs.send, Codec::Opus);
            });
        }
    }

    #[tokio::test]
    async fn pcmu_answer_preserves_advertised_opus_receiver_and_pcmu_sender() {
        assert!(audio_codec::opus_available(), "asymmetric interop test requires libopus.so.0");
        let mut desktop=prepared().await;assert!(desktop.enable_opus());
        let mut phone=prepared().await;assert!(phone.enable_opus());
        // Answer only selects PCMU for desktop transmission. The phone uses
        // Opus111 from the desktop offer for its transmission.
        let phone_answer=RemoteAudio::parse("application/sdp",&phone.offer().replace(" 111 0", " 0")).unwrap();
        assert!(!phone_answer.codecs.opus);
        let desktop_offer=RemoteAudio::parse("application/sdp",desktop.offer()).unwrap();
        assert_eq!(desktop_offer.codecs.send,Codec::Opus);
        let (desktop_io,desktop_mic,desktop_speaker)=io();
        let (phone_io,phone_mic,phone_speaker)=io();
        let (desktop,phone)=tokio::time::timeout(Duration::from_secs(4),async {
            tokio::join!(desktop.connect(phone_answer,desktop_io,true),phone.connect(desktop_offer,phone_io,false))
        }).await.unwrap();
        let mut desktop=desktop.unwrap();let mut phone=phone.unwrap();
        let mut desktop_heard=false;let mut phone_heard=false;
        tokio::time::timeout(Duration::from_secs(3),async {
            while !desktop_heard || !phone_heard {
                let wave:Vec<i16>=(0..160).map(|i|((i as f32*440.0*std::f32::consts::TAU/8000.0).sin()*9000.0) as i16).collect();
                let _=desktop_mic.try_send(wave.clone());let _=phone_mic.try_send(wave);
                if let Ok(frame)=desktop_speaker.try_recv() {desktop_heard|=frame.iter().any(|v|v.abs()>1000);}
                if let Ok(frame)=phone_speaker.try_recv() {phone_heard|=frame.iter().any(|v|v.abs()>1000);}
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.unwrap();
        let stats=desktop.stats().await;
        assert_eq!(stats.send_codec,1);
        assert_eq!(stats.receive_codec_mask,3);
        assert_eq!(phone.stats().await.send_codec,2);
        assert!(stats.packets_received>0);
        assert_eq!(stats.playout.unsupported_payload_packets,0);
        desktop.stop().await;phone.stop().await;
    }

}
