//! Native H264 camera track: reserved ICE socket, negotiated SDES/SRTP,
//! bounded authenticated receive and explicit device ownership.
use crate::{
    camera_codec::{AccessUnit, Decoder, Encoder},
    video::CameraDevice,
};
use ost::calling::{
    datagram::{Datagram, DatagramIo},
    ice, media_protection, rtp, sdp, sdp_compress, srtp, video,
};
use std::{
    collections::VecDeque,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::oneshot};

#[derive(Default)]
struct State {
    sent: AtomicU64,
    decoded: AtomicU64,
    authenticated: AtomicU64,
    assembled: AtomicU64,
    auth_rejected: AtomicU64,
    header_rejected: AtomicU64,
    unsupported_payload: AtomicU64,
    decoder_rejected: AtomicU64,
    wanted: AtomicBool,
    sending: AtomicBool,
    available: AtomicBool,
    failed: AtomicBool,
    capture_failed: AtomicBool,
    capture_ready: AtomicBool,
    capture_worker: AtomicBool,
    capture_active: AtomicBool,
    capture_generation: AtomicU64,
    capture_restart: AtomicBool,
    capture_transition: Mutex<()>,
    publishing: AtomicBool,
    encoded: Mutex<EncodedQueue>,
    frame: Mutex<Option<iced::widget::image::Handle>>,
    screen_frame: Mutex<Option<iced::widget::image::Handle>>,
    local_frame: Mutex<Option<iced::widget::image::Handle>>,
    #[cfg(feature = "libwebrtc-av")]
    network_frame: Mutex<Option<iced::widget::image::Handle>>,
    #[cfg(feature = "libwebrtc-av")]
    network_enabled: AtomicBool,
    received_at: Mutex<Option<Instant>>,
    screen_received_at: Mutex<Option<Instant>>,
    device: Mutex<Option<CameraDevice>>,
    #[cfg(test)]
    test_frames: Mutex<Option<std::sync::mpsc::Receiver<AccessUnit>>>,
}
#[derive(Clone, Default)]
pub struct Control(Arc<State>);
impl Control {
    #[cfg(all(test, feature = "libwebrtc-av"))]
    pub(crate) fn fixture_camera_frame(&self, frame: iced::widget::image::Handle) {
        *self.0.device.lock().unwrap() = Some(CameraDevice { path: "/dev/video-fixture".into(), label: "Fixture".into() });
        self.0.wanted.store(true, Ordering::Release);
        *self.0.network_frame.lock().unwrap() = Some(frame);
    }
    pub fn configure(&self, device: CameraDevice, on: bool) {
        {
            let _transition = self.0.capture_transition.lock().unwrap_or_else(|e| e.into_inner());
            *self.0.device.lock().unwrap_or_else(|e| e.into_inner()) = Some(device);
            self.0.capture_generation.fetch_add(1, Ordering::AcqRel);
        }
        self.set_enabled(on);
    }
    pub fn set_enabled(&self, on: bool) {
        {
            let _transition = self.0.capture_transition.lock().unwrap_or_else(|e| e.into_inner());
            if self.0.wanted.swap(on, Ordering::AcqRel) != on {
                self.0.capture_generation.fetch_add(1, Ordering::AcqRel);
            }
            if !on {
                self.0.sending.store(false, Ordering::Release);
                self.0.capture_ready.store(false, Ordering::Release);
                *self.0.local_frame.lock().unwrap_or_else(|e| e.into_inner()) = None;
                #[cfg(feature = "libwebrtc-av")]
                { *self.0.network_frame.lock().unwrap_or_else(|e| e.into_inner()) = None; }
                self.0.encoded.lock().unwrap_or_else(|e| e.into_inner()).clear();
            } else {
                self.0.capture_failed.store(false, Ordering::Release);
            }
        }
        if on {
            self.start_capture_worker();
        }
    }
    fn start_capture_worker(&self) {
        if self.0.capture_worker.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        let owner = Arc::downgrade(&self.0);
        if std::thread::Builder::new().name("teams-camera-source".into())
            .spawn(move || capture_loop(owner)).is_err()
        {
            self.0.capture_worker.store(false, Ordering::Release);
            self.0.capture_failed.store(true, Ordering::Release);
        }
    }
    pub fn capture_ready(&self) -> bool {
        self.0.capture_ready.load(Ordering::Acquire)
    }
    pub fn capture_failed(&self) -> bool {
        self.0.capture_failed.load(Ordering::Acquire)
    }
    pub fn enabled(&self) -> bool {
        self.0.wanted.load(Ordering::Acquire)
    }
    pub fn available(&self) -> bool {
        self.0.available.load(Ordering::Acquire)
    }
    pub fn failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire) || self.capture_failed()
    }
    pub fn sending(&self) -> bool {
        self.0.sending.load(Ordering::Acquire)
    }
    pub fn has_device(&self) -> bool {
        self.0.device.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
    pub fn prepare_send(&self) {
        self.0.failed.store(false, Ordering::Release);
        self.set_enabled(true);
    }
    /// Publication failed; keep the explicitly requested local preview alive.
    /// Only set_enabled(false) owns the user's stop/cancellation intent.
    pub fn fail(&self) {
        self.0.failed.store(true, Ordering::Release);
        self.0.available.store(false, Ordering::Release);
        self.0.sending.store(false, Ordering::Release);
        self.0.publishing.store(false, Ordering::Release);
        self.0.encoded.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    fn begin_publication(&self, enabled: bool) {
        self.0.publishing.store(enabled, Ordering::Release);
        self.0.encoded.lock().unwrap_or_else(|e| e.into_inner()).clear();
        if enabled { self.request_keyframe(); }
    }
    fn request_keyframe(&self) {
        self.0.encoded.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.0.capture_restart.store(true, Ordering::Release);
    }
    fn take_encoded(&self) -> Vec<AccessUnit> {
        let mut queue = self.0.encoded.lock().unwrap_or_else(|e| e.into_inner());
        if self.enabled() && self.0.publishing.load(Ordering::Acquire) {
            queue.frames.drain(..).collect()
        } else { queue.clear(); Vec::new() }
    }
    pub fn take_frame(&self) -> Option<iced::widget::image::Handle> {
        self.0
            .frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
    pub fn take_screen_frame(&self) -> Option<iced::widget::image::Handle> {
        self.0
            .screen_frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
    pub fn take_local_frame(&self) -> Option<iced::widget::image::Handle> {
        let mut frame = self.0.local_frame.lock().unwrap_or_else(|e| e.into_inner());
        if self.enabled() { frame.take() } else { *frame = None; None }
    }
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn network_enabled(&self, enabled: bool) {
        self.0.network_enabled.store(enabled, Ordering::Release);
        self.0.sending.store(false, Ordering::Release);
        self.0.available.store(enabled, Ordering::Release);
        if !enabled {
            *self.0.network_frame.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn take_network_frame(&self) -> Option<iced::widget::image::Handle> {
        if self.enabled() && self.0.network_enabled.load(Ordering::Acquire) {
            self.0.network_frame.lock().unwrap_or_else(|e| e.into_inner()).take()
        } else { None }
    }
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn network_sending(&self, sending: bool) {
        self.0.sending.store(sending && self.enabled() && self.0.network_enabled.load(Ordering::Acquire), Ordering::Release);
    }
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn publish_received_frame(&self, frame: iced::widget::image::Handle) {
        *self.0.frame.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
        *self.0.received_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }
    #[cfg(feature = "libwebrtc-av")]
    pub(crate) fn publish_screen_frame(&self, frame: iced::widget::image::Handle) {
        *self.0.screen_frame.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
        *self.0.screen_received_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }
    pub fn receiving(&self) -> bool {
        self.0
            .received_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|at| at.elapsed() < Duration::from_secs(3))
    }
    pub fn screen_receiving(&self) -> bool {
        self.0
            .screen_received_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|at| at.elapsed() < Duration::from_secs(3))
    }
    fn clear_received(&self) {
        *self.0.received_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.0.frame.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}
/// A bounded queue starts at an IDR and discards a broken prediction chain.
/// Local decoding sees the source directly, independent of this network queue.
#[derive(Default)]
struct EncodedQueue {
    frames: VecDeque<AccessUnit>,
    awaiting_keyframe: bool,
}
impl EncodedQueue {
    fn clear(&mut self) {
        self.frames.clear();
        self.awaiting_keyframe = true;
    }
    fn push(&mut self, unit: AccessUnit) {
        if self.frames.len() >= 3 { self.clear(); }
        if self.awaiting_keyframe && !unit.keyframe { return; }
        self.awaiting_keyframe = false;
        self.frames.push_back(unit);
    }
}
fn capture_current(state: &State, generation: u64) -> bool {
    state.wanted.load(Ordering::Acquire)
        && state.capture_generation.load(Ordering::Acquire) == generation
}
fn capture_failure(state: &State, generation: u64) {
    let _transition = state.capture_transition.lock().unwrap_or_else(|e| e.into_inner());
    if !capture_current(state, generation) { return; }
    state.capture_failed.store(true, Ordering::Release);
    state.capture_ready.store(false, Ordering::Release);
    state.wanted.store(false, Ordering::Release);
    state.capture_generation.fetch_add(1, Ordering::AcqRel);
    state.sending.store(false, Ordering::Release);
    *state.local_frame.lock().unwrap_or_else(|e| e.into_inner()) = None;
    state.encoded.lock().unwrap_or_else(|e| e.into_inner()).clear();
    crate::live_trace::record("call_video_failed", 0, 4, 0);
}
fn show_local(state: &State, generation: u64, frame: iced::widget::image::Handle) {
    let mut slot = state.local_frame.lock().unwrap_or_else(|e| e.into_inner());
    if !capture_current(state, generation) { return; }
    #[cfg(feature = "libwebrtc-av")]
    if state.network_enabled.load(Ordering::Acquire) {
        *state.network_frame.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame.clone());
    }
    *slot = Some(frame);
    state.capture_ready.store(true, Ordering::Release);
}
fn capture_loop(owner: Weak<State>) {
    let mut generation = 0;
    let mut encoder: Option<Encoder> = None;
    let mut decoder: Option<Decoder> = None;
    let mut backend = None;
    let mut live: Option<crate::video::Preview> = None;
    // Weak ownership is intentional: a call's last Control owner can close the
    // source even while no transport exists. No thread owns its own lifetime.
    while let Some(state) = owner.upgrade() {
        if Arc::strong_count(&state) == 1 { break; }
        let next_generation = state.capture_generation.load(Ordering::Acquire);
        let enabled = state.wanted.load(Ordering::Acquire);
        let restart = state.capture_restart.swap(false, Ordering::AcqRel);
        if generation != next_generation || !enabled || restart {
            if let Some(previous) = encoder.take() { previous.stop(); }
            decoder = None;
            if let Some(preview) = live.take() { preview.stop(); }
            state.capture_active.store(false, Ordering::Release);
            generation = next_generation;
        }
        if !enabled {
            drop(state);
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        #[cfg(test)]
        let injected = state.test_frames.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        #[cfg(not(test))]
        let injected = false;
        // The self-view used to decode our own H.264. That stream has no clock,
        // so the picture lagged the camera. Show the newest raw frame instead.
        // Injected fixtures and the legacy publisher still need the codec.
        if !injected && !state.publishing.load(Ordering::Acquire) {
            if let Some(previous) = encoder.take() { previous.stop(); }
            decoder = None;
            if live.is_none() {
                let device = state.device.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let Some(device) = device else {
                    capture_failure(&state, generation);
                    continue;
                };
                if !capture_current(&state, generation) || Arc::strong_count(&state) == 1 { continue; }
                match crate::video::Preview::start_latest(&device, crate::video::Cancellation::default()) {
                    Ok(preview) => {
                        crate::live_trace::record("call_video_preview", 0, 1, 0);
                        live = Some(preview);
                    }
                    Err(_) => { capture_failure(&state, generation); continue; }
                }
            }
            if live.as_ref().is_some_and(|preview| preview.failed()) {
                capture_failure(&state, generation);
                continue;
            }
            if let Some(frame) = live.as_ref().and_then(|preview| preview.take_frame()) {
                show_local(&state, generation, frame);
            }
            state.capture_active.store(true, Ordering::Release);
            drop(state);
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        if let Some(preview) = live.take() { preview.stop(); }
        if encoder.is_none() && !injected {
            let device = state.device.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let Some(device) = device else {
                capture_failure(&state, generation);
                continue;
            };
            let selected = backend.map(Ok).unwrap_or_else(Encoder::probe);
            // Probes run here, never on the UI or call's Tokio runtime. A stop
            // during a probe cannot open the physical device afterwards.
            if !capture_current(&state, generation) || Arc::strong_count(&state) == 1 { continue; }
            match selected.and_then(|selected| Encoder::start(&device, selected)) {
                Ok(created) => {
                    if !capture_current(&state, generation) || Arc::strong_count(&state) == 1 {
                        created.stop();
                        continue;
                    }
                    backend = Some(created.backend);
                    crate::live_trace::record("call_video_encoder", 0, match created.backend {
                        crate::camera_codec::Backend::Vaapi => 0,
                        crate::camera_codec::Backend::OpenH264 => 1,
                        crate::camera_codec::Backend::X264 => 2,
                    }, 0);
                    encoder = Some(created);
                }
                Err(_) => { capture_failure(&state, generation); continue; }
            }
        }
        let mut unit = None;
        let mut ended = false;
        if let Some(source) = &encoder {
            match source.frames.try_recv() {
                Ok(frame) => unit = Some(frame),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => ended = true,
                Err(std::sync::mpsc::TryRecvError::Empty) => {},
            }
        }
        #[cfg(test)]
        if injected {
            let input = state.test_frames.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(input) = &*input {
                match input.try_recv() {
                    Ok(frame) => unit = Some(frame),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => ended = true,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {},
                }
            }
        }
        if ended { capture_failure(&state, generation); continue; }
        state.capture_active.store(true, Ordering::Release);
        if let Some(unit) = unit {
            if decoder.is_none() && unit.keyframe {
                decoder = Decoder::start().ok();
            }
            if capture_current(&state, generation) {
                if let Some(local) = &decoder {
                    if !local.push(&unit) { decoder = None; }
                }
                let mut queue = state.encoded.lock().unwrap_or_else(|e| e.into_inner());
                if capture_current(&state, generation) && state.publishing.load(Ordering::Acquire) {
                    queue.push(unit);
                }
            }
        }
        if let Some(frame) = decoder.as_ref().and_then(|local| local.take_frame()) {
            show_local(&state, generation, frame);
        }
        drop(state);
        std::thread::sleep(Duration::from_millis(5));
    }
    if let Some(encoder) = encoder { encoder.stop(); }
    if let Some(preview) = live.take() { preview.stop(); }
    drop(decoder);
    if let Some(state) = owner.upgrade() {
        state.capture_active.store(false, Ordering::Release);
        state.capture_worker.store(false, Ordering::Release);
    }
}
pub struct Prepared {
    socket: UdpSocket,
    credentials: ice::IceCredentials,
    key: srtp::SrtpKeyingMaterial,
    pub section: String,
    ssrc: u32,
    relay: Option<crate::call_turn::Allocation>,
    direction: OfferDirection,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OfferDirection {
    SendReceive,
    ReceiveOnly,
}

/// Initial video direction from the official `calculateVideoDirection`.
/// Sending on a meeting is `sendrecv`. Not sending is `recvonly`, including a
/// direct call: the offer that failed acceptance was `sendrecv`, and this
/// session cannot add a video section after the first answer.
pub fn initial_remote_camera(camera_on: bool, direct_call: bool) -> OfferDirection {
    if camera_on && !direct_call {
        OfferDirection::SendReceive
    } else {
        OfferDirection::ReceiveOnly
    }
}
pub struct Remote {
    send_allowed: bool,
    recv_allowed: bool,
    ssrc: Option<u32>,
    credentials: ice::IceCredentials,
    key: srtp::SrtpKeyingMaterial,
    candidates: Vec<ice::IceCandidate>,
}
impl Remote {
    pub fn parse(kind: &str, blob: &str) -> Result<Option<Self>, ()> {
        if !matches!(kind, "application/sdp" | "application/sdp-ngc-1.0") {
            return Err(());
        }
        let text = sdp_compress::decompress_sdp_with_limit(blob, crate::call_media::MAX_SDP_BYTES)
            .map_err(|_| ())?;
        if text.lines().count() > 2048
            || text
                .lines()
                .filter(|line| line.starts_with("m=video "))
                .count()
                > 1
            || text
                .lines()
                .any(|line| line.len() > 4096 || line.chars().any(char::is_control))
        {
            return Err(());
        }
        let mut section = String::new();
        let mut active = false;
        for line in text.lines() {
            if line.starts_with("m=") {
                if active {
                    break;
                }
                active = line.starts_with("m=video ");
            }
            if active {
                section.push_str(line);
                section.push_str("\r\n");
            }
        }
        if section.is_empty() {
            return Ok(None);
        }
        let fields = section
            .lines()
            .next()
            .ok_or(())?
            .split_ascii_whitespace()
            .collect::<Vec<_>>();
        let port = fields.get(1).ok_or(())?.parse::<u16>().map_err(|_| ())?;
        if port == 0 {
            return Ok(None);
        }
        if fields.get(2) != Some(&"RTP/SAVP")
            || !fields.iter().skip(3).any(|v| *v == "122")
            || !section
                .lines()
                .any(|l| l.eq_ignore_ascii_case("a=rtpmap:122 X-H264UC/90000"))
            || !section.lines().any(|l| l == "a=rtcp-mux")
        {
            return Err(());
        }
        let session = text
            .lines()
            .take_while(|l| !l.starts_with("m="))
            .collect::<Vec<_>>()
            .join("\r\n");
        let attribute = |prefix: &str| -> Result<String, ()> {
            let mut values = section.lines().filter_map(|l| l.strip_prefix(prefix));
            let value = values
                .next()
                .or_else(|| session.lines().find_map(|l| l.strip_prefix(prefix)))
                .ok_or(())?;
            if values.next().is_some() {
                return Err(());
            }
            Ok(value.to_owned())
        };
        let ufrag = attribute("a=ice-ufrag:")?;
        let pwd = attribute("a=ice-pwd:")?;
        if !(4..=256).contains(&ufrag.len())
            || !(22..=256).contains(&pwd.len())
            || ufrag
                .chars()
                .chain(pwd.chars())
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(());
        }
        let mut keys = Vec::new();
        for line in section.lines().filter(|s| s.starts_with("a=crypto:")) {
            let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
            if fields.len() != 3
                || fields[1] != "AES_CM_128_HMAC_SHA1_80"
                || fields[2].split('|').skip(1).any(|part| part.contains(':'))
            {
                return Err(());
            }
            let key = srtp::parse_crypto_line(line).map_err(|_| ())?;
            if key.tag != 2 || !keys.is_empty() {
                return Err(());
            }
            keys.push(key);
        }
        if keys.len() != 1 {
            return Err(());
        }
        let candidates = ice::parse_candidates_from_sdp_section(&text, "video");
        if candidates.is_empty()
            || candidates.len() > 64
            || candidates.iter().any(|c| {
                c.port == 0
                    || c.address
                        .parse::<IpAddr>()
                        .map_or(true, |ip| ip.is_unspecified() || ip.is_multicast())
            })
        {
            return Err(());
        }
        let mut directions = section.lines().filter(|s| {
            matches!(
                *s,
                "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive"
            )
        });
        let direction = directions.next().unwrap_or("a=sendrecv");
        if directions.next().is_some() {
            return Err(());
        }
        let send_allowed = matches!(direction, "a=sendrecv" | "a=recvonly");
        let recv_allowed = matches!(direction, "a=sendrecv" | "a=sendonly");
        let ssrc = section
            .lines()
            .find_map(|l| l.strip_prefix("a=x-ssrc-range:"))
            .and_then(|v| v.split('-').next()?.parse().ok());
        Ok(Some(Self {
            send_allowed,
            recv_allowed,
            ssrc,
            credentials: ice::IceCredentials { ufrag, pwd },
            key: keys.into_iter().next().unwrap(),
            candidates,
        }))
    }
}
impl Prepared {
    pub async fn bind(
        addresses: &[IpAddr],
        relay: Option<crate::call_turn::Allocation>,
        direction: OfferDirection,
    ) -> Result<Self, ()> {
        let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|_| ())?;
        let port = socket.local_addr().map_err(|_| ())?.port();
        let credentials = ice::IceCredentials {
            ufrag: sdp::generate_ice_ufrag(),
            pwd: sdp::generate_ice_pwd(),
        };
        let local = addresses
            .first()
            .filter(|ip| ip.is_ipv4())
            .ok_or(())?
            .to_string();
        let mut candidates = addresses
            .iter()
            .filter(|ip| ip.is_ipv4())
            .take(32)
            .enumerate()
            .map(|(i, ip)| ice::IceCandidate {
                foundation: (i + 1).to_string(),
                component: 1,
                transport: ice::Transport::Udp,
                priority: 2130706431 - i as u32,
                address: ip.to_string(),
                port,
                candidate_type: ice::CandidateType::Host,
                raddr: None,
                rport: None,
            })
            .collect::<Vec<_>>();
        if let Some(r) = &relay {
            let a = r.relay_addr();
            // Media is received on the allocation, never on the separate
            // retained host socket. Do not advertise a non-listening route.
            candidates.clear();
            candidates.push(ice::IceCandidate {
                foundation: "relay".into(),
                component: 1,
                transport: ice::Transport::Udp,
                priority: 184548351,
                address: a.ip().to_string(),
                port: a.port(),
                candidate_type: ice::CandidateType::Relay,
                raddr: Some(local.clone()),
                rport: Some(r.base_addr().map_err(|_| ())?.port()),
            });
        }
        let ssrc = video::generate_ssrc().min(u32::MAX - 100);
        let endpoint = relay
            .as_ref()
            .map(|r| r.relay_addr())
            .unwrap_or(SocketAddr::new(local.parse().map_err(|_| ())?, port));
        let generated = sdp::generate_av_sdp_offer(&sdp::AvSdpParams {
            local_ip: &local,
            audio_port: port,
            video_port: port,
            audio_ufrag: &credentials.ufrag,
            audio_pwd: &credentials.pwd,
            video_ufrag: &credentials.ufrag,
            video_pwd: &credentials.pwd,
            audio_candidates: &candidates,
            video_candidates: &candidates,
            video_ssrc_base: ssrc,
            audio_ssrc: video::generate_ssrc(),
        });
        let key = srtp::parse_crypto_line(&generated.video_crypto_line).map_err(|_| ())?;
        let offset = generated.sdp.find("m=video ").ok_or(())?;
        let section = generated.sdp[offset..]
            .lines()
            .filter(|l| {
                !l.starts_with("a=rtpmap:121 ")
                    && !l.starts_with("a=rtpmap:123 ")
                    && !l.starts_with("a=fmtp:121 ")
                    && !l.starts_with("a=fmtp:123 ")
                    && !l.starts_with("a=x-caps:121 ")
            })
            .map(|l| {
                if l.starts_with("m=video ") {
                    format!(
                        "m=video {} RTP/SAVP 122\r\nc=IN IP4 {}\r\n",
                        endpoint.port(),
                        endpoint.ip()
                    )
                } else if l == "a=sendrecv" && direction == OfferDirection::ReceiveOnly {
                    "a=recvonly\r\n".to_owned()
                } else {
                    format!("{l}\r\n")
                }
            })
            .collect();
        crate::live_trace::record(
            "call_video_offered",
            0,
            usize::from(relay.is_some()),
            candidates.len(),
        );
        Ok(Self {
            socket,
            credentials,
            key,
            section,
            ssrc,
            relay,
            direction,
        })
    }
    pub async fn close(self) {
        if let Some(relay) = self.relay {
            relay.release().await;
        }
    }
    pub async fn connect(
        self,
        remote: Remote,
        control: Control,
        controlling: bool,
    ) -> Result<Session, ()> {
        let transport = if let Some(relay) = self.relay {
            let peers = remote
                .candidates
                .iter()
                .filter_map(|c| {
                    c.address
                        .parse::<IpAddr>()
                        .ok()
                        .map(|ip| SocketAddr::new(ip, c.port))
                })
                .collect::<Vec<_>>();
            Datagram::Relay(relay.connect(&peers).await.map_err(|_| ())?)
        } else {
            Datagram::Direct(self.socket)
        };
        let socket = Arc::new(transport);
        let agent = ice::IceAgent::new(self.credentials, remote.credentials, controlling);
        let pair = match agent
            .check_connectivity(socket.clone(), &remote.candidates)
            .await
        {
            Ok(p) => p,
            Err(_) => {
                crate::live_trace::record("call_video_failed", 0, 3, 0);
                let _ = socket.close().await;
                return Err(());
            }
        };
        let cipher = srtp::create_context(&self.key, &remote.key).map_err(|_| ())?;
        let (stop, stopped) = oneshot::channel();
        let send_allowed = self.direction == OfferDirection::SendReceive && remote.send_allowed;
        control.0.available.store(
            send_allowed
                && control
                    .0
                    .device
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some(),
            Ordering::Release,
        );
        control.begin_publication(send_allowed);
        crate::live_trace::record("call_video_connected", 0, 0, 0);
        let c = control.clone();
        let worker = tokio::spawn(run(
            socket,
            pair.remote_addr,
            agent,
            cipher,
            self.ssrc,
            remote.ssrc,
            remote.recv_allowed,
            send_allowed,
            c,
            stopped,
        ));
        Ok(Session {
            stop: Some(stop),
            worker: Some(worker),
            control,
        })
    }
}
pub struct Session {
    stop: Option<oneshot::Sender<()>>,
    worker: Option<tokio::task::JoinHandle<()>>,
    control: Control,
}
impl Session {
    pub fn finished(&self) -> bool {
        self.worker.as_ref().is_none_or(|worker| worker.is_finished())
    }
    pub async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(mut w) = self.worker.take() {
            if tokio::time::timeout(Duration::from_secs(3), &mut w)
                .await
                .is_err()
            {
                w.abort();
                let _ = w.await;
            }
        }
        self.control.0.available.store(false, Ordering::Release);
        self.control.0.sending.store(false, Ordering::Release);
        self.control.begin_publication(false);
        self.control.clear_received();
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.control.0.available.store(false, Ordering::Release);
        self.control.0.sending.store(false, Ordering::Release);
        self.control.begin_publication(false);
    }
}
async fn run(
    socket: Arc<Datagram>,
    peer: SocketAddr,
    agent: ice::IceAgent,
    mut cipher: srtp::SrtpContext,
    ssrc: u32,
    remote_ssrc: Option<u32>,
    recv_allowed: bool,
    send_allowed: bool,
    control: Control,
    mut stopped: oneshot::Receiver<()>,
) {
    let mut secure = media_protection::RtpReceiver::new(&cipher);
    let mut feedback = media_protection::ControlCipher::new(&cipher, ssrc);
    let mut last_vsr = Instant::now() - Duration::from_secs(4);
    let mut request_id = 1u16;
    let mut last_pli = Instant::now();
    let mut last_stats = Instant::now();
    let mut packetizer = video::VideoPacketizer::with_config(
        ssrc,
        video::SvcConfig {
            coded_width: 640,
            coded_height: 480,
            display_width: 640,
            display_height: 480,
            bitrate: 600000,
            ..Default::default()
        },
    );
    let mut decoder: Option<Decoder> = None;
    let mut decoder_job = recv_allowed.then(|| tokio::task::spawn_blocking(Decoder::start));
    let mut clock = tokio::time::interval(Duration::from_millis(67));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut buffer = [0u8; 2048];
    let mut assembly = Assembly::default();
    assembly.await_key = true;
    loop {
        tokio::select! {biased;
            _=&mut stopped=>break,
            _=clock.tick()=>{
                if last_stats.elapsed()>=Duration::from_secs(5){
                    crate::live_trace::record("call_video_stats",0,control.0.sent.load(Ordering::Relaxed) as usize,control.0.decoded.load(Ordering::Relaxed) as usize);
                    record_receive_stats(&control);
                    last_stats=Instant::now();
                }
                if recv_allowed && last_vsr.elapsed()>=Duration::from_secs(3){
                    let packet=vsr(ssrc,remote_ssrc.unwrap_or(0),request_id,assembly.await_key);
                    request_id=request_id.wrapping_add(1);last_vsr=Instant::now();
                    if let Ok(packet)=feedback.protect(&packet){let _=socket.send_to(&packet,peer).await;}
                }
                if send_allowed && control.enabled() {
                    for frame in control.take_encoded() {
                        for packet in packetizer.packetize_frame(&frame.nals) {
                            if !control.enabled() || !control.0.publishing.load(Ordering::Acquire) { break; }
                            let Ok(packet) = srtp::protect(&mut cipher, &packet) else { continue; };
                            if socket.send_to(&packet, peer).await.is_err() { control.fail(); break; }
                            control.0.sent.fetch_add(1, Ordering::Relaxed);
                            if control.enabled() { control.0.sending.store(true, Ordering::Release); }
                        }
                    }
                }
                if let Some(d)=&mut decoder {if let Some(frame)=d.take_frame(){*control.0.frame.lock().unwrap_or_else(|e|e.into_inner())=Some(frame);*control.0.received_at.lock().unwrap_or_else(|e|e.into_inner())=Some(Instant::now());control.0.decoded.fetch_add(1,Ordering::Relaxed);}}
            },
            result = async { match &mut decoder_job {
                Some(job) => job.await,
                None => std::future::pending().await,
            }} => {
                decoder_job = None;
                match result {
                    Ok(Ok(created)) => {
                        crate::live_trace::record("call_video_decoder", 0, match created.backend {
                            crate::camera_codec::DecodeBackend::Vaapi => 0,
                            crate::camera_codec::DecodeBackend::Software => 1,
                        }, 0);
                        decoder = Some(created);
                    }
                    _ => crate::live_trace::record("call_video_failed", 0, 5, 0),
                }
            },
            received=socket.recv_from(&mut buffer)=>{
                let Ok((len,from))=received else {control.fail();break;};let data=&buffer[..len];
                if ice::is_stun_message(data){if ice::is_stun_request(data){agent.handle_stun_request(&*socket,data,from).await;}continue;}
                if from!=peer{continue;}
                if data.len()>1 && (192..=223).contains(&data[1]) {
                    if let Ok(packet)=feedback.unprotect(data){
                        if packet.len()>=12&&packet[1]==206&&(packet[0]&31)==1&&last_pli.elapsed()>Duration::from_secs(1){control.request_keyframe();last_pli=Instant::now();}
                    }
                    continue;
                }
                if !recv_allowed{continue;}
                let Ok((bytes,_))=secure.unprotect(data) else {control.0.auth_rejected.fetch_add(1,Ordering::Relaxed);continue;};
                control.0.authenticated.fetch_add(1,Ordering::Relaxed);
                let Ok(mut packet)=rtp::decode(&bytes) else {control.0.header_rejected.fetch_add(1,Ordering::Relaxed);continue;};
                if packet.payload_type!=122{control.0.unsupported_payload.fetch_add(1,Ordering::Relaxed);continue;}
            if packet.csrc_count>0&&bytes.len()>=16{packet.ssrc=u32::from_be_bytes(bytes[12..16].try_into().unwrap());}
                if let Some(unit)=assembly.push(&packet){control.0.assembled.fetch_add(1,Ordering::Relaxed);if let Some(d)=&decoder {if !d.push(&unit){control.0.decoder_rejected.fetch_add(1,Ordering::Relaxed);assembly.await_key=true;}}}
            }
        }
    }
    crate::live_trace::record(
        "call_video_stats",
        0,
        control.0.sent.load(Ordering::Relaxed) as usize,
        control.0.decoded.load(Ordering::Relaxed) as usize,
    );
    record_receive_stats(&control);
    if let Some(job) = decoder_job { job.abort(); }
    drop(decoder);
    control.0.sending.store(false, Ordering::Release);
    control.0.available.store(false, Ordering::Release);
    control.begin_publication(false);
    control.clear_received();
    let _ = socket.close().await;
}
fn record_receive_stats(control: &Control) {
    let state = &control.0;
    crate::live_trace::record(
        "call_video_receive_v1",
        state.authenticated.load(Ordering::Relaxed),
        state.assembled.load(Ordering::Relaxed) as usize,
        state.decoded.load(Ordering::Relaxed) as usize,
    );
    crate::live_trace::record(
        "call_video_receive_rejected_v1",
        state.auth_rejected.load(Ordering::Relaxed),
        state.header_rejected.load(Ordering::Relaxed) as usize,
        state.unsupported_payload.load(Ordering::Relaxed) as usize,
    );
    crate::live_trace::record(
        "call_video_decoder_rejected_v1",
        0,
        state.decoder_rejected.load(Ordering::Relaxed) as usize,
        0,
    );
}
// MS-RTP VSR, one SOURCE_ANY slot, PT122 X-H264UC. The fixed layout is
// referenced to Better Teams ce451cc live::build_vsr; no remote text retained.
fn vsr(sender: u32, media: u32, id: u16, keyframe: bool) -> Vec<u8> {
    let mut b = vec![0x8f, 206, 0, 24];
    b.extend(sender.to_be_bytes());
    b.extend(media.to_be_bytes());
    b.extend(1u16.to_be_bytes());
    b.extend(88u16.to_be_bytes());
    b.extend(0xffff_fffeu32.to_be_bytes());
    b.extend(id.to_be_bytes());
    b.extend([0, 0, 0, if keyframe { 0x80 } else { 0 }, 1, 68, 0, 0, 0, 0]);
    b.extend([122, 1, 0, 3]);
    b.extend(640u16.to_be_bytes());
    b.extend(480u16.to_be_bytes());
    b.extend(100_000u32.to_be_bytes());
    b.extend([0; 4]);
    b.extend(100_000u32.to_be_bytes());
    for i in 0..10 {
        b.extend((if i == 4 { 1u16 } else { 0 }).to_be_bytes());
    }
    b.extend(((1u32 << 2) | (1u32 << 4)).to_be_bytes());
    b.extend(1u16.to_be_bytes());
    b.extend([0; 2]);
    b.extend([0; 16]);
    b.extend((640u32 * 480).to_be_bytes());
    b
}
#[derive(Default)]
struct Assembly {
    source: Option<u32>,
    timestamp: u32,
    sequence: Option<u16>,
    nals: Vec<Vec<u8>>,
    fu: Vec<u8>,
    bytes: usize,
    await_key: bool,
    last: Option<Instant>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}
impl Assembly {
    fn reset(&mut self) {
        self.nals.clear();
        self.fu.clear();
        self.bytes = 0;
        self.await_key = true;
    }
    fn push(&mut self, p: &rtp::RtpPacket) -> Option<AccessUnit> {
        if self.source != Some(p.ssrc)
            || self.timestamp != p.timestamp
            || self
                .sequence
                .is_some_and(|s| s.wrapping_add(1) != p.sequence_number)
            || self
                .last
                .is_some_and(|t| t.elapsed() > Duration::from_secs(1))
        {
            if self.source != Some(p.ssrc) {
                self.sps = None;
                self.pps = None;
            }
            if !self.nals.is_empty()
                || !self.fu.is_empty()
                || self.source != Some(p.ssrc)
                || self
                    .sequence
                    .is_some_and(|s| s.wrapping_add(1) != p.sequence_number)
                || self
                    .last
                    .is_some_and(|t| t.elapsed() > Duration::from_secs(1))
            {
                self.reset();
            }
            self.timestamp = p.timestamp;
            self.source = Some(p.ssrc);
        }
        self.sequence = Some(p.sequence_number);
        self.last = Some(Instant::now());
        self.bytes = self.bytes.saturating_add(p.payload.len());
        if self.bytes > 2 * 1024 * 1024 || self.nals.len() >= 128 {
            self.reset();
            return None;
        }
        let data = &p.payload;
        let kind = data.first().copied()? & 31;
        match kind {
            14 | 30 => {}
            1..=23 => self.nals.push(data.clone()),
            24 => {
                // RFC 6184 STAP-A: the complete aggregate must be valid before
                // retaining any parameter set. Nested aggregation is forbidden.
                let mut offset = 1;
                let mut aggregate = Vec::new();
                while offset < data.len() {
                    if offset + 2 > data.len() {
                        self.reset();
                        return None;
                    }
                    let size = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
                    offset += 2;
                    if size == 0
                        || offset + size > data.len()
                        || aggregate.len() + self.nals.len() >= 128
                    {
                        self.reset();
                        return None;
                    }
                    let nal = &data[offset..offset + size];
                    if !(1..=23).contains(&(nal[0] & 31)) {
                        self.reset();
                        return None;
                    }
                    if nal[0] & 31 != 14 {
                        aggregate.push(nal.to_vec());
                    }
                    offset += size;
                }
                if aggregate.is_empty() {
                    self.reset();
                    return None;
                }
                self.nals.extend(aggregate);
            }
            28 if data.len() > 2 => {
                let header = data[1];
                if header & 0x80 != 0 {
                    self.fu.clear();
                    self.fu.push((data[0] & 0xe0) | (header & 31));
                } else if self.fu.is_empty() {
                    self.reset();
                    return None;
                }
                self.fu.extend_from_slice(&data[2..]);
                if header & 0x40 != 0 {
                    self.nals.push(std::mem::take(&mut self.fu));
                }
            }
            _ => {
                self.reset();
                return None;
            }
        }
        if !p.marker {
            return None;
        }
        if !self.fu.is_empty() {
            self.reset();
            return None;
        }
        self.bytes = 0;
        let mut nals = std::mem::take(&mut self.nals);
        for nal in &nals {
            match nal[0] & 31 {
                7 if nal.len() <= 65536 => self.sps = Some(nal.clone()),
                8 if nal.len() <= 65536 => self.pps = Some(nal.clone()),
                _ => {}
            }
        }
        let keyframe = nals.iter().any(|n| n[0] & 31 == 5);
        if keyframe {
            if !nals.iter().any(|n| n[0] & 31 == 8) {
                if let Some(pps) = &self.pps {
                    nals.insert(0, pps.clone());
                }
            }
            if !nals.iter().any(|n| n[0] & 31 == 7) {
                if let Some(sps) = &self.sps {
                    nals.insert(0, sps.clone());
                }
            }
        }
        if self.await_key && !keyframe {
            return None;
        }
        self.await_key = false;
        Some(AccessUnit { nals, keyframe })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_camera_is_receive_only_unless_a_meeting_is_already_sending() {
        assert_eq!(initial_remote_camera(false, false), OfferDirection::ReceiveOnly);
        assert_eq!(initial_remote_camera(false, true), OfferDirection::ReceiveOnly);
        assert_eq!(initial_remote_camera(true, true), OfferDirection::ReceiveOnly);
        assert_eq!(initial_remote_camera(true, false), OfferDirection::SendReceive);
    }
    #[test]
    fn vsr_has_the_original_source_any_layout_and_cannot_enable_camera() {
        let packet = vsr(1, 2, 7, true);
        assert_eq!(packet.len(), 100);
        assert_eq!(&packet[12..20], &[0, 1, 0, 88, 255, 255, 255, 254]);
        assert_eq!(packet[25], 0x80);
        assert_eq!(packet[26], 1);
        assert_eq!(packet[27], 68);
        let control = Control::default();
        assert!(!control.enabled());
        assert!(!control.sending());
    }
    #[test]
    fn packet_loss_discards_partial_access_units_and_recovers_at_keyframe() {
        let packet = |seq, timestamp, payload: Vec<u8>, marker| rtp::RtpPacket {
            version: 2,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker,
            payload_type: 122,
            sequence_number: seq,
            timestamp,
            ssrc: 1,
            payload,
        };
        let mut assembler = Assembly::default();
        assert!(
            assembler
                .push(&packet(1, 0, vec![0x7c, 0x85, 1, 2], false))
                .is_none()
        );
        assert!(
            assembler
                .push(&packet(3, 0, vec![0x7c, 0x45, 3, 4], true))
                .is_none()
        );
        assert!(assembler.await_key);
        assert!(
            assembler
                .push(&packet(4, 6000, vec![0x41, 1, 2], true))
                .is_none()
        );
        let recovered = assembler
            .push(&packet(5, 12000, vec![0x65, 1, 2], true))
            .unwrap();
        assert!(recovered.keyframe);
        assert!(
            assembler
                .push(&packet(6, 18000, vec![1; 2 * 1024 * 1024 + 1], false))
                .is_none()
        );
        assert!(assembler.fu.is_empty() && assembler.nals.is_empty());
    }
    fn description(p: &Prepared) -> String {
        format!(
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=test\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n{}",
            p.section
        )
    }
    #[test]
    fn aggregate_parameter_sets_are_atomic_and_loss_requires_a_new_keyframe() {
        let packet = |seq, payload: Vec<u8>| rtp::RtpPacket {
            version: 2,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker: true,
            payload_type: 122,
            sequence_number: seq,
            timestamp: seq as u32 * 6000,
            ssrc: 1,
            payload,
        };
        let mut assembly = Assembly::default();
        let unit = assembly
            .push(&packet(
                1,
                vec![24, 0, 2, 0x67, 1, 0, 2, 0x68, 2, 0, 2, 0x65, 3],
            ))
            .unwrap();
        assert_eq!(unit.nals, vec![vec![0x67, 1], vec![0x68, 2], vec![0x65, 3]]);
        assert!(
            assembly.push(&packet(3, vec![0x41, 9])).is_none(),
            "loss between complete frames also breaks prediction"
        );
        let unit = assembly.push(&packet(4, vec![0x65, 4])).unwrap();
        assert_eq!(
            unit.nals.len(),
            3,
            "cached parameter sets precede recovered IDR"
        );
        assert!(
            assembly
                .push(&packet(5, vec![24, 0, 2, 0x67, 5, 0, 9, 0x68]))
                .is_none()
        );
        assert!(assembly.nals.is_empty() && assembly.await_key);
        assert_eq!(
            assembly.sps,
            Some(vec![0x67, 1]),
            "malformed aggregate must not poison SPS cache"
        );
        assert!(assembly.push(&packet(6, vec![24, 0, 1, 24])).is_none());
    }
    #[tokio::test]
    async fn synthetic_h264_crosses_authenticated_ice_srtp_and_decodes_without_camera_access() {
        exercise_video(None).await;
    }
    #[test]
    fn local_camera_preview_appears_before_any_media_session_and_stops_on_request() {
        // Official startStopVideo turns on the local preview before configuring
        // media modalities. This fixture only encodes a generated test pattern.
        let control = Control::default();
        let (sender, receiver) = std::sync::mpsc::sync_channel(3);
        *control.0.test_frames.lock().unwrap() = Some(receiver);
        let encoder = Encoder::synthetic().unwrap();
        control.set_enabled(true);
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut frame = None;
        while Instant::now() < deadline && frame.is_none() {
            if let Ok(unit) = encoder.frames.try_recv() {
                let _ = sender.try_send(unit);
            }
            frame = control.take_local_frame();
            std::thread::sleep(Duration::from_millis(10));
        }
        control.set_enabled(false);
        encoder.stop();
        assert!(frame.is_some(), "local preview must not wait for a remote SDP or ICE");
        assert!(!control.available(), "local preview is not a published transport");
        assert!(!control.sending(), "no network session was created");
        assert!(control.take_local_frame().is_none(), "off clears self-view immediately");
    }
    #[tokio::test]
    async fn synthetic_h264_crosses_turn_and_releases_the_video_allocation() {
        let fixture = crate::call_turn::tests::fixture().await;
        let allocation = crate::call_turn::Allocation::allocate(
            fixture.server,
            crate::call_turn::Credentials::new("fixture-user", "fixture-secret", "rtcmedia")
                .unwrap(),
        )
        .await
        .unwrap();
        exercise_video(Some(allocation)).await;
        assert!(fixture.forwarded.load(Ordering::Relaxed) > 5);
        tokio::time::timeout(Duration::from_secs(2), fixture.released)
            .await
            .unwrap()
            .unwrap();
        fixture.worker.abort();
    }
    async fn exercise_video(relay: Option<crate::call_turn::Allocation>) {
        let addresses = ["127.0.0.1".parse().unwrap()];
        let relayed = relay.is_some();
        let a = Prepared::bind(&addresses, relay, OfferDirection::SendReceive).await.unwrap();
        if relayed {
            assert!(a.section.contains("typ relay"));
            assert!(!a.section.contains("typ host"));
        }
        let b = Prepared::bind(&addresses, None, OfferDirection::ReceiveOnly).await.unwrap();
        assert!(b.section.contains("a=recvonly\r\n"));
        assert!(!b.section.contains("a=sendrecv\r\n"));
        let ar = Remote::parse("application/sdp", &description(&a))
            .unwrap()
            .unwrap();
        let br = Remote::parse("application/sdp", &description(&b))
            .unwrap()
            .unwrap();
        let ca = Control::default();
        let cb = Control::default();
        let (tx, rx) = std::sync::mpsc::sync_channel(3);
        *ca.0.test_frames.lock().unwrap() = Some(rx);
        ca.set_enabled(true);
        let (left, right) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                a.connect(br, ca.clone(), true),
                b.connect(ar, cb.clone(), false)
            )
        })
        .await
        .unwrap();
        let mut left = left.unwrap();
        let mut right = right.unwrap();
        assert!(!cb.available(), "recvonly offer cannot enable local camera transmission");
        cb.set_enabled(true);
        let encoder = crate::camera_codec::Encoder::synthetic().unwrap();
        let received = tokio::time::timeout(Duration::from_secs(12), async {
            let mut remote = None;
            let mut local = false;
            loop {
                if let Ok(frame) = encoder.frames.try_recv() {
                    let _ = tx.try_send(frame);
                }
                if remote.is_none() { remote = cb.take_frame(); }
                local |= ca.take_local_frame().is_some();
                if local && remote.is_some() { break remote.unwrap(); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        ca.set_enabled(false);
        assert!(ca.take_local_frame().is_none(), "camera off must clear self-view immediately");
        tokio::time::sleep(Duration::from_millis(150)).await;
        let sent = ca.0.sent.load(Ordering::Acquire);
        for _ in 0..20 {
            if let Ok(frame) = encoder.frames.try_recv() {
                let _ = tx.try_send(frame);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            ca.0.sent.load(Ordering::Acquire),
            sent,
            "camera off must gate queued video"
        );
        encoder.stop();
        assert!(cb.receiving());
        assert!(cb.0.authenticated.load(Ordering::Acquire) > 0);
        assert!(cb.0.assembled.load(Ordering::Acquire) > 0);
        assert!(cb.0.decoded.load(Ordering::Acquire) > 0);
        assert_eq!(cb.0.auth_rejected.load(Ordering::Acquire), 0);
        assert_eq!(cb.0.header_rejected.load(Ordering::Acquire), 0);
        assert_eq!(cb.0.unsupported_payload.load(Ordering::Acquire), 0);
        assert_eq!(cb.0.sent.load(Ordering::Acquire), 0);
        assert!(!cb.sending());
        left.stop().await;
        right.stop().await;
        assert!(!cb.receiving());
        assert!(
            received.is_ok(),
            "real H264 did not traverse ICE/SRTP/decode"
        );
        assert!(!ca.sending());
        assert!(!cb.available());
    }
    #[tokio::test]
    async fn rejects_video_with_missing_protection_or_unoffered_codec() {
        let p = Prepared::bind(&["127.0.0.1".parse().unwrap()], None, OfferDirection::SendReceive)
            .await
            .unwrap();
        let text = description(&p);
        assert!(
            Remote::parse(
                "application/sdp",
                &text.replace("X-H264UC/90000", "unknown/90000")
            )
            .is_err()
        );
        let no_key = text
            .lines()
            .filter(|s| !s.starts_with("a=crypto:"))
            .collect::<Vec<_>>()
            .join("\r\n");
        assert!(Remote::parse("application/sdp", &no_key).is_err());
        assert!(
            Remote::parse(
                "application/sdp",
                &text.replace("AES_CM_128_HMAC_SHA1_80", "AES_CM_128_HMAC_SHA1_32")
            )
            .is_err()
        );
        let receive_only =
            Remote::parse("application/sdp", &text.replace("a=sendrecv", "a=recvonly"))
                .unwrap()
                .unwrap();
        assert!(receive_only.send_allowed && !receive_only.recv_allowed);
        assert!(Remote::parse("application/sdp", &format!("{text}{}", p.section)).is_err());
        let wildcard = text.replace(" 127.0.0.1 ", " 0.0.0.0 ");
        assert!(Remote::parse("application/sdp", &wildcard).is_err());
        p.close().await;
    }
    #[tokio::test]
    async fn cancelling_pending_video_ice_releases_the_advertised_socket() {
        let addresses = ["127.0.0.1".parse().unwrap()];
        let local = Prepared::bind(&addresses, None, OfferDirection::SendReceive).await.unwrap();
        let bound = local.socket.local_addr().unwrap();
        let peer = Prepared::bind(&addresses, None, OfferDirection::SendReceive).await.unwrap();
        let remote = Remote::parse("application/sdp", &description(&peer))
            .unwrap()
            .unwrap();
        let mut job = Box::pin(local.connect(remote, Control::default(), true));
        tokio::select! {result=&mut job=>panic!("unexpected ICE result {}",result.is_ok()),_=tokio::time::sleep(Duration::from_millis(20))=>{}}
        drop(job);
        let rebound = UdpSocket::bind(bound).await.unwrap();
        drop(rebound);
        peer.close().await;
    }
}
