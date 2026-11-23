//! A single libwebrtc peer owns the microphone and negotiated camera slots.
//! The caller owns device capture, signaling, authorization, and UI.
use crate::{
    error::{Error, Result},
    sdp,
};
use libwebrtc::{
    audio_source::native::NativeAudioSource,
    peer_connection_factory::native::PeerConnectionFactoryExt, prelude::*,
    video_source::native::NativeVideoSource,
};
use rand::Rng;
use tokio::sync::mpsc;

const RECEIVE_SLOTS: usize = 9;
const MAX_SDP_BYTES: usize = 256 * 1024;

/// Camera slots after our own line, plus the presentation slot when it has a stream id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceivePlan {
    pub cameras: Vec<(String, u32)>,
    pub screen: Option<(String, u32)>,
}

/// A received track and its negotiated MID. No participant identity is guessed.
pub struct IncomingTrack {
    pub mid: Option<String>,
    pub track: MediaStreamTrack,
}

#[derive(Default)]
pub struct AudioMetrics {
    pub packets: u64,
    pub lost: u64,
    pub concealed_samples: u64,
    pub jitter_micros: u64,
}

/// Closing a partially initialized peer is as important as closing a joined one.
struct OwnedPeer(PeerConnection);
impl Drop for OwnedPeer {
    fn drop(&mut self) {
        self.0.on_track(None);
        self.0.on_ice_candidate(None);
        self.0.close();
    }
}

/// A cloned signaling handle; dropping it does not own or close the call.
#[derive(Clone)]
pub struct Negotiator(PeerConnection);
impl Negotiator {
    #[cfg(feature = "test-support")]
    pub async fn add_candidate(&self, candidate: IceCandidate) -> Result<()> {
        self.0
            .add_ice_candidate(candidate)
            .await
            .map_err(|_| Error::Engine)
    }
    pub async fn answer_offer(&self, teams_offer: &str) -> Result<String> {
        self.answer_keeping(teams_offer, None).await
    }

    /// `keep_share` is our own presentation mid while we are sending it.
    /// Every other share line in the answer stays receive-only.
    pub async fn answer_keeping(&self, teams_offer: &str, keep_share: Option<&str>) -> Result<String> {
        if teams_offer.len() > MAX_SDP_BYTES {
            return Err(Error::Engine);
        }
        let remote = sdp::from_teams_offer(teams_offer)?;
        let offer = SessionDescription::parse(&remote.browser_sdp, SdpType::Offer)
            .map_err(|_| Error::Engine)?;
        self.0
            .set_remote_description(offer)
            .await
            .map_err(|_| Error::Engine)?;
        let answer = self
            .0
            .create_answer(AnswerOptions::default())
            .await
            .map_err(|_| Error::Engine)?;
        let mut browser_answer = answer.to_string();
        // Share slots stay receive-only. Promoting an x-multi-stream line to
        // sendrecv is the 301000 rejection; a local share would name its mid here.
        for mid in remote.plan().screen_share_mids {
            if keep_share == Some(mid.as_str()) {
                continue;
            }
            browser_answer = sdp::stop_sending_on(&browser_answer, &mid);
        }
        let local = SessionDescription::parse(&browser_answer, SdpType::Answer)
            .map_err(|_| Error::Engine)?;
        self.0
            .set_local_description(local)
            .await
            .map_err(|_| Error::Engine)?;
        let mut numbers = || rand::thread_rng().gen_range(1..=u32::MAX - 100);
        Ok(sdp::to_teams_answer(&browser_answer, &remote, &mut numbers)?.sdp)
    }
}

pub struct Engine {
    factory: PeerConnectionFactory,
    peer: OwnedPeer,
    /// Kept alive so the meeting offer retains its data m-line.
    #[allow(dead_code)]
    data_channel: DataChannel,
    audio_source: NativeAudioSource,
    microphone_gate: crate::pcm::MicrophoneGate,
    camera: RtpTransceiver,
    camera_source: Option<NativeVideoSource>,
    camera_size: Option<(u32, u32)>,
    share: RtpTransceiver,
    share_source: Option<NativeVideoSource>,
    share_size: Option<(u32, u32)>,
    send_filter: crate::send_control::RequestFilter,
    send_limits: Option<Vec<crate::send_control::SendLimit>>,
    offer: sdp::SignaledOffer,
    browser_offer: String,
    screen_mid: String,
    incoming: Option<mpsc::Receiver<IncomingTrack>>,
    candidates: Option<mpsc::Receiver<IceCandidate>>,
    candidate_overflow: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Engine {
    /// Prepare a local peer with no ICE servers. Useful for isolated tests.
    pub async fn prepare() -> Result<Self> {
        Self::prepare_with_ice(Vec::new()).await
    }

    /// A caller may supply validated TURN credentials from the active session.
    /// No OS audio or camera device is opened by preparation.
    pub async fn prepare_with_ice(ice_servers: Vec<IceServer>) -> Result<Self> {
        Self::prepare_with_audio_rate(ice_servers, 48_000).await
    }

    /// Match the explicit PCM capture clock owned by the caller.
    pub async fn prepare_with_audio_rate(
        ice_servers: Vec<IceServer>,
        sample_rate: u32,
    ) -> Result<Self> {
        Self::prepare_layout(ice_servers, sample_rate, RECEIVE_SLOTS).await
    }

    /// `receive_slots` is 9 for a meeting gallery and 0 for a direct call.
    /// A direct call keeps one camera line and one screen line, the same shape
    /// the other native client uses for one person.
    pub async fn prepare_layout(
        ice_servers: Vec<IceServer>,
        sample_rate: u32,
        receive_slots: usize,
    ) -> Result<Self> {
        if !matches!(sample_rate, 8_000 | 48_000) {
            return Err(Error::Engine);
        }
        let factory = PeerConnectionFactory::default();
        let mut config = RtcConfiguration::default();
        if !ice_servers.is_empty() {
            config.ice_transport_type = IceTransportsType::Relay;
        }
        config.ice_servers = ice_servers;
        let peer = OwnedPeer(
            factory
                .create_peer_connection(config)
                .map_err(|_| Error::Engine)?,
        );
        let (candidate_tx, candidates) = mpsc::channel(256);
        let candidate_overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let overflow = candidate_overflow.clone();
        // Install before set_local_description; gathering may start immediately.
        peer.0.on_ice_candidate(Some(Box::new(move |candidate| {
            if candidate_tx.try_send(candidate).is_err() {
                overflow.store(true, std::sync::atomic::Ordering::Release);
            }
        })));
        let (track_tx, incoming) = mpsc::channel(32);
        peer.0.on_track(Some(Box::new(move |event| {
            let _ = track_tx.try_send(IncomingTrack {
                mid: event.transceiver.mid(),
                track: event.track,
            });
        })));

        // Created before the media lines; libwebrtc still places it after them.
        let data_channel = peer
            .0
            .create_data_channel("main-channel", DataChannelInit::default())
            .map_err(|_| Error::Engine)?;

        // Direct 10 ms frames avoid replaying buffered speech across a mute.
        let audio_source = NativeAudioSource::new(AudioSourceOptions::default(), sample_rate, 1, 0);
        let audio_track = factory.create_audio_track("microphone", audio_source.clone());
        let microphone_gate =
            crate::pcm::MicrophoneGate::new(audio_track.clone(), audio_source.clone());
        peer.0
            .add_transceiver(
                MediaStreamTrack::Audio(audio_track.clone()),
                RtpTransceiverInit {
                    direction: RtpTransceiverDirection::SendRecv,
                    stream_ids: vec!["microphone".into()],
                    send_encodings: Vec::new(),
                },
            )
            .map_err(|_| Error::Engine)?;

        let sending = h264(factory.get_rtp_sender_capabilities(MediaType::Video).codecs);
        let receiving = h264(
            factory
                .get_rtp_receiver_capabilities(MediaType::Video)
                .codecs,
        );
        if !sending
            .iter()
            .any(|codec| codec.mime_type.eq_ignore_ascii_case("video/h264"))
            || !receiving
                .iter()
                .any(|codec| codec.mime_type.eq_ignore_ascii_case("video/h264"))
        {
            return Err(Error::Engine);
        }
        let camera = add_video(
            &peer.0,
            RtpTransceiverDirection::SendRecv,
            Some("camera"),
            sending.clone(),
            receive_slots > 0,
        )?;
        for _ in 0..receive_slots {
            add_video(
                &peer.0,
                RtpTransceiverDirection::RecvOnly,
                None,
                receiving.clone(),
                false,
            )?;
        }
        // The presentation line stays recvonly until the user picks a screen.
        let share = add_video(
            &peer.0,
            RtpTransceiverDirection::SendRecv,
            Some("screen"),
            sending,
            false,
        )?;

        let browser = peer
            .0
            .create_offer(OfferOptions {
                offer_to_receive_audio: true,
                offer_to_receive_video: true,
                ..OfferOptions::default()
            })
            .await
            .map_err(|_| Error::Engine)?;
        let browser_offer = browser.to_string();
        peer.0
            .set_local_description(browser)
            .await
            .map_err(|_| Error::Engine)?;
        let screen_mid = (receive_slots + 2).to_string();
        let plan = sdp::OfferPlan {
            screen_share_mids: vec![screen_mid.clone()],
            ..Default::default()
        };
        // The SDP adapter reserves 100 SSRCs for simulcast. Leave headroom so
        // a random value cannot overflow that range.
        let mut numbers = || rand::thread_rng().gen_range(1..=u32::MAX - 100);
        let offer = sdp::to_teams_offer(&browser_offer, &plan, &mut numbers)?;
        Ok(Self {
            factory,
            peer,
            data_channel,
            audio_source,
            microphone_gate,
            camera,
            camera_source: None,
            camera_size: None,
            share,
            share_source: None,
            share_size: None,
            send_filter: Default::default(),
            send_limits: None,
            offer,
            browser_offer,
            screen_mid,
            incoming: Some(incoming),
            candidates: Some(candidates),
            candidate_overflow,
        })
    }

    pub fn signaled_offer(&self) -> &str {
        &self.offer.sdp
    }
    #[cfg(feature = "test-support")]
    pub fn browser_offer_for_fixture(&self) -> &str {
        &self.browser_offer
    }

    pub fn camera_mid(&self) -> Option<String> {
        self.camera.mid()
    }

    pub fn share_mid(&self) -> Option<String> {
        self.share.mid()
    }

    pub fn sharing(&self) -> bool {
        self.share_source.is_some()
    }

    /// Attach a raw screen source. The caller already scaled the frame to an even size.
    pub fn start_share(&mut self, width: u32, height: u32) -> Result<()> {
        if !(2..=1920).contains(&width) || !(2..=1920).contains(&height) || width % 2 != 0 || height % 2 != 0 {
            return Err(Error::Engine);
        }
        if self.share_size == Some((width, height)) && self.share_source.is_some() {
            return Ok(());
        }
        let _ = self.stop_share();
        let source = NativeVideoSource::new(VideoResolution { width, height }, true);
        let track = self.factory.create_video_track("screen", source.clone());
        self.share
            .set_direction(RtpTransceiverDirection::SendRecv)
            .map_err(|_| Error::Engine)?;
        self.share
            .sender()
            .set_track(Some(MediaStreamTrack::Video(track)))
            .map_err(|_| Error::Engine)?;
        self.share_source = Some(source);
        self.share_size = Some((width, height));
        Ok(())
    }

    pub fn push_share(&self, buffer: I420Buffer) -> bool {
        let Some(source) = &self.share_source else {
            return false;
        };
        source.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, buffer))
    }

    pub fn stop_share(&mut self) -> Result<()> {
        let _ = self.share.sender().set_track(None);
        let _ = self.share.set_direction(RtpTransceiverDirection::RecvOnly);
        self.share_source = None;
        self.share_size = None;
        Ok(())
    }
    pub fn camera_sender(&self) -> RtpSender {
        self.camera.sender()
    }
    pub fn negotiator(&self) -> Negotiator {
        Negotiator(self.peer.0.clone())
    }

    /// Mixer-assigned receiving slot, derived from the actual answer.
    pub fn screen_mid(&self) -> &str {
        &self.screen_mid
    }

    /// People slots are the main-video lines after our own camera.
    /// The screen slot is the presentation line, when it carries a stream id.
    pub fn receive_plan(&self, answer: &str) -> Result<ReceivePlan> {
        let streams = sdp::stream_lines(answer, &self.offer.lines)?;
        let slot = |line: &sdp::StreamLine| Some((line.browser_mid.clone(), line.source_stream_id?));
        Ok(ReceivePlan {
            cameras: streams
                .iter()
                .filter(|line| line.role == sdp::LineRole::MainVideo)
                .skip(1)
                .filter_map(slot)
                .collect(),
            screen: streams
                .iter()
                .find(|line| line.role == sdp::LineRole::ScreenShare)
                .and_then(slot),
        })
    }

    pub fn first_remote_camera_slot(&self, answer: &str) -> Result<Option<(String, u32)>> {
        Ok(self.receive_plan(answer)?.cameras.into_iter().next())
    }

    /// Take exactly once so one owner collects the candidates for signaling.
    pub fn take_candidates(&mut self) -> Option<mpsc::Receiver<IceCandidate>> {
        self.candidates.take()
    }

    pub fn candidate_overflowed(&self) -> bool {
        self.candidate_overflow
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// The caller decompresses a bounded Teams answer before applying it.
    pub async fn apply_answer(&self, teams_sdp: &str) -> Result<()> {
        if teams_sdp.len() > MAX_SDP_BYTES {
            return Err(Error::Engine);
        }
        let answer = sdp::to_browser_answer(teams_sdp, &self.offer, &self.browser_offer)?;
        let answer =
            SessionDescription::parse(&answer, SdpType::Answer).map_err(|_| Error::Engine)?;
        self.peer
            .0
            .set_remote_description(answer)
            .await
            .map_err(|_| Error::Engine)
    }

    pub fn audio_source(&self) -> NativeAudioSource {
        self.audio_source.clone()
    }

    pub fn microphone_gate(&self) -> crate::pcm::MicrophoneGate {
        self.microphone_gate.clone()
    }

    pub fn connection_state(&self) -> PeerConnectionState {
        self.peer.0.connection_state()
    }

    /// Aggregate counters only; no SDP, addresses, SSRCs, or identities.
    pub async fn audio_metrics(&self) -> Result<AudioMetrics> {
        let mut result = AudioMetrics::default();
        for stat in self.peer.0.get_stats().await.map_err(|_| Error::Engine)? {
            if let libwebrtc::stats::RtcStats::InboundRtp(stream) = stat {
                if stream.stream.kind != "audio" {
                    continue;
                }
                result.packets = result
                    .packets
                    .saturating_add(stream.received.packets_received);
                result.lost = result
                    .lost
                    .saturating_add(stream.received.packets_lost.max(0) as u64);
                result.concealed_samples = result
                    .concealed_samples
                    .saturating_add(stream.inbound.concealed_samples);
                if stream.received.jitter.is_finite() && stream.received.jitter > 0.0 {
                    result.jitter_micros = result
                        .jitter_micros
                        .max((stream.received.jitter * 1_000_000.0) as u64);
                }
            }
        }
        Ok(result)
    }

    pub fn set_microphone_enabled(&self, enabled: bool) -> bool {
        self.microphone_gate.set_enabled(enabled)
    }

    pub fn start_camera(&mut self, width: u32, height: u32) -> Result<NativeVideoSource> {
        if self.camera_source.is_some()
            || width == 0
            || height == 0
            || width > 4096
            || height > 4096
        {
            return Err(Error::Engine);
        }
        let source = NativeVideoSource::new(VideoResolution { width, height }, false);
        let track = self.factory.create_video_track("camera", source.clone());
        self.camera
            .sender()
            .set_track(Some(MediaStreamTrack::Video(track)))
            .map_err(|_| Error::Engine)?;
        self.camera_source = Some(source.clone());
        self.camera_size = Some((width, height));
        if self.apply_camera_limits().is_err() {
            let _ = self.stop_camera();
            return Err(Error::Engine);
        }
        Ok(source)
    }

    /// Applies only explicit mixer limits; stale sequences cannot reactivate an
    /// older configuration. Requests received before capture are retained.
    pub fn handle_camera_control(&mut self, body: &serde_json::Value) -> Result<()> {
        let Some(request) = crate::send_control::parse_request(body) else {
            return Ok(());
        };
        if let Some(limits) = self.send_filter.admit(&request) {
            self.send_limits = Some(limits);
            if self.camera_source.is_some() {
                self.apply_camera_limits()?;
            }
        }
        Ok(())
    }

    fn apply_camera_limits(&self) -> Result<()> {
        let Some(limits) = &self.send_limits else {
            return Ok(());
        };
        let sender = self.camera.sender();
        let rids: Vec<String> = sender
            .parameters()
            .encodings
            .into_iter()
            .map(|encoding| encoding.rid)
            .collect();
        let plans = crate::send_control::plan_layers(limits, self.camera_size, &rids);
        let apply = |nudge: f64| -> Result<()> {
            let mut parameters = sender.parameters();
            for encoding in &mut parameters.encodings {
                if let Some(plan) = plans.iter().find(|plan| plan.rid == encoding.rid) {
                    encoding.active = plan.active;
                    if let Some(limit) = &plan.limits {
                        encoding.scale_resolution_down_by = Some(
                            (limit.scale_down_by * if plan.keyframe { nudge } else { 1.0 })
                                .clamp(1.0, 16.0),
                        );
                        encoding.max_framerate =
                            limit.max_framerate.map(|fps| fps.clamp(1.0, 60.0));
                        encoding.max_bitrate = limit.max_bitrate.map(|rate| rate.min(50_000_000));
                    }
                }
            }
            sender.set_parameters(parameters).map_err(|_| Error::Engine)
        };
        if plans.iter().any(|plan| plan.keyframe) {
            apply(crate::send_control::KEYFRAME_NUDGE)?;
        }
        apply(1.0)
    }

    /// Detach synchronously. A stale producer can no longer publish its frames.
    pub fn stop_camera(&mut self) -> Result<()> {
        let sender = self.camera.sender();
        // Fail closed even if detaching the sender is rejected.
        if let Some(track) = sender.track() {
            track.set_enabled(false);
        }
        sender.set_track(None).map_err(|_| Error::Engine)?;
        self.camera_source = None;
        self.camera_size = None;
        Ok(())
    }

    pub fn take_incoming(&mut self) -> Option<mpsc::Receiver<IncomingTrack>> {
        self.incoming.take()
    }
}

fn h264(codecs: Vec<RtpCodecCapability>) -> Vec<RtpCodecCapability> {
    codecs
        .into_iter()
        .filter(|codec| {
            matches!(
                codec.mime_type.to_ascii_lowercase().as_str(),
                "video/h264" | "video/rtx"
            )
        })
        .collect()
}

fn add_video(
    peer: &PeerConnection,
    direction: RtpTransceiverDirection,
    stream: Option<&str>,
    codecs: Vec<RtpCodecCapability>,
    simulcast: bool,
) -> Result<RtpTransceiver> {
    let encodings = if simulcast {
        [("1", 4.0), ("2", 2.0), ("3", 1.0)]
            .into_iter()
            .map(|(rid, scale)| RtpEncodingParameters {
                rid: rid.into(),
                scale_resolution_down_by: Some(scale),
                scalability_mode: Some("L1T1".into()),
                ..Default::default()
            })
            .collect()
    } else {
        Vec::new()
    };
    let transceiver = peer
        .add_transceiver_for_media(
            MediaType::Video,
            RtpTransceiverInit {
                direction,
                stream_ids: stream.into_iter().map(str::to_owned).collect(),
                send_encodings: encodings,
            },
        )
        .map_err(|_| Error::Engine)?;
    transceiver
        .set_codec_preferences(codecs)
        .map_err(|_| Error::Engine)?;
    Ok(transceiver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use libwebrtc::{
        audio_stream::native::NativeAudioStream, video_stream::native::NativeVideoStream,
    };
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::time::{interval, timeout};

    #[tokio::test]
    async fn detached_camera_reports_no_track() {
        let mut engine = Engine::prepare().await.unwrap();
        assert!(engine.camera.sender().track().is_none());
        let source = engine.start_camera(160, 120).unwrap();
        assert!(engine.camera.sender().track().is_some());
        engine.stop_camera().unwrap();
        assert!(engine.camera.sender().track().is_none());
        drop(source);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn engine_sends_audio_and_camera_after_a_translated_answer_and_stops_camera() {
        let mut engine = Engine::prepare().await.unwrap();
        let mut candidates = engine.take_candidates().unwrap();
        let receiver = engine
            .factory
            .create_peer_connection(RtcConfiguration::default())
            .unwrap();
        let (return_candidate, mut from_receiver) = mpsc::unbounded_channel::<IceCandidate>();
        receiver.on_ice_candidate(Some(Box::new(move |candidate| {
            let _ = return_candidate.send(candidate);
        })));
        let (remote_track, mut tracks) = mpsc::unbounded_channel();
        receiver.on_track(Some(Box::new(move |event| {
            let _ = remote_track.send(event.track);
        })));
        let offer = SessionDescription::parse(&engine.browser_offer, SdpType::Offer).unwrap();
        receiver.set_remote_description(offer).await.unwrap();
        let return_audio = NativeAudioSource::new(AudioSourceOptions::default(), 48_000, 1, 0);
        let return_track = engine
            .factory
            .create_audio_track("synthetic-return", return_audio.clone());
        receiver
            .add_track(
                MediaStreamTrack::Audio(return_track),
                &["remote-microphone"],
            )
            .unwrap();
        let answer = receiver
            .create_answer(AnswerOptions::default())
            .await
            .unwrap();
        receiver
            .set_local_description(answer.clone())
            .await
            .unwrap();
        let mut number = || rand::thread_rng().gen_range(1..=u32::MAX - 100);
        let answer = sdp::to_teams_offer(
            &answer.to_string(),
            &sdp::OfferPlan {
                screen_share_mids: vec!["11".into()],
                ..Default::default()
            },
            &mut number,
        )
        .unwrap();
        // This peer only receives video. The mixer-style translation of an
        // offer normally adds placeholder SSRC 1 to receive-only lines; an
        // answer cannot pretend every non-sending line sends that same SSRC.
        let mut synthetic_answer = sdp::parse(&answer.sdp).unwrap();
        for line in &mut synthetic_answer.media {
            if matches!(line.direction(), Some("recvonly" | "inactive")) {
                line.attributes.retain(|attribute| {
                    !matches!(
                        attribute.name.as_str(),
                        "x-ssrc-range" | "ssrc" | "ssrc-group"
                    )
                });
            }
        }
        let synthetic_answer = sdp::write(&synthetic_answer);
        engine.apply_answer(&synthetic_answer).await.unwrap();

        let local = engine.peer.0.clone();
        let remote = receiver.clone();
        let relay = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(candidate) = candidates.recv() => { let _ = remote.add_ice_candidate(candidate).await; }
                    Some(candidate) = from_receiver.recv() => { let _ = local.add_ice_candidate(candidate).await; }
                    else => break,
                }
            }
        });
        let gate = engine.microphone_gate();
        let (mic_sender, mic_receiver) = std::sync::mpsc::sync_channel(8);
        let (speaker_sender, speaker_receiver) = std::sync::mpsc::sync_channel(8);
        let mut audio_bridge =
            crate::pcm::PcmBridge::start(gate.clone(), mic_receiver, speaker_sender);
        gate.set_enabled(true);
        let return_task = tokio::spawn(async move {
            let mut tick = interval(Duration::from_millis(10));
            let mut phase = 0usize;
            loop {
                tick.tick().await;
                let data: Vec<i16> = (0..480)
                    .map(|i| {
                        let t = (phase + i) as f32 / 48_000.0;
                        (8_000.0 * (t * 330.0 * std::f32::consts::TAU).sin()) as i16
                    })
                    .collect();
                phase += 480;
                let frame = AudioFrame {
                    data: data.into(),
                    sample_rate: 48_000,
                    num_channels: 1,
                    samples_per_channel: 480,
                };
                if return_audio.capture_frame(&frame).await.is_err() {
                    break;
                }
            }
        });
        let mut incoming = engine.take_incoming().unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                if let MediaStreamTrack::Audio(track) = incoming.recv().await.unwrap().track {
                    audio_bridge.add_remote_track(track).unwrap();
                    break;
                }
            }
        })
        .await
        .expect("incoming audio track");
        let audio_task = tokio::spawn(async move {
            let mut tick = interval(Duration::from_millis(20));
            let mut phase = 0usize;
            loop {
                tick.tick().await;
                let data = (0..960)
                    .map(|i| {
                        let t = (phase + i) as f32 / 48_000.0;
                        (8_000.0 * (t * 440.0 * std::f32::consts::TAU).sin()) as i16
                    })
                    .collect::<Vec<_>>();
                phase += 960;
                if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) =
                    mic_sender.try_send(data)
                {
                    break;
                }
            }
        });
        engine
            .handle_camera_control(&serde_json::json!({"controlVideoStreaming": {
                "sequenceNumber": 1, "controlInfo": {"sourceId": 1, "fmtParamsList": [
                    {"rid":"1", "max-fs":1200, "max-fps":3000, "max-br":1000},
                    {"rid":"2", "max-fs":1200, "max-fps":3000, "max-br":1000},
                    {"rid":"3", "max-fs":1200, "max-fps":3000, "max-br":1000}
                ]}
            }}))
            .unwrap();
        let video = engine.start_camera(320, 240).unwrap();
        let video_task = tokio::spawn(async move {
            let mut ticker = interval(Duration::from_millis(33));
            let mut tick = 0u8;
            loop {
                ticker.tick().await;
                let mut buffer = I420Buffer::new(320, 240);
                let (y, u, v) = buffer.data_mut();
                y.fill(80 + tick % 80);
                u.fill(128);
                v.fill(128);
                let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
                frame.timestamp_us = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as i64;
                video.capture_frame(&frame);
                tick = tick.wrapping_add(1);
            }
        });
        let mut audio_stream = None;
        let mut video_stream = None;
        timeout(Duration::from_secs(15), async {
            while audio_stream.is_none() || video_stream.is_none() {
                match tracks.recv().await.unwrap() {
                    MediaStreamTrack::Audio(track) => {
                        audio_stream = Some(NativeAudioStream::new(track, 48_000, 1))
                    }
                    MediaStreamTrack::Video(track) if video_stream.is_none() => {
                        video_stream = Some(NativeVideoStream::new(track))
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("the negotiated tracks arrive");
        let mut audio_stream = audio_stream.unwrap();
        let mut video_stream = video_stream.unwrap();
        timeout(Duration::from_secs(20), async {
            let audio = async {
                for _ in 0..100 {
                    let frame = audio_stream.next().await.unwrap();
                    if frame.data.iter().any(|sample| sample.unsigned_abs() > 500) {
                        return;
                    }
                }
                panic!("no audible synthetic samples")
            };
            let video = async {
                for _ in 0..3 {
                    let _ = video_stream.next().await.unwrap();
                }
            };
            tokio::join!(audio, video);
        })
        .await
        .expect("audio and video are decoded");
        timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(frame) = speaker_receiver.try_recv() {
                    if frame.iter().any(|sample| sample.unsigned_abs() > 500) {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("remote audio crosses the native mixer into the speaker channel");
        timeout(Duration::from_secs(3), async {
            loop {
                let audio = engine.audio_metrics().await.unwrap();
                let video = engine.camera_sender().get_stats().await.unwrap().into_iter().any(|stat| {
                    matches!(stat, libwebrtc::stats::RtcStats::OutboundRtp(stream) if stream.sent.packets_sent > 0)
                });
                if audio.packets > 0 && video { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("aggregate audio reception and camera packet counters advance");
        assert!(gate.set_enabled(false));
        tokio::time::sleep(Duration::from_millis(400)).await;
        timeout(Duration::from_secs(2), async {
            for _ in 0..10 {
                let frame = audio_stream.next().await.unwrap();
                assert!(
                    frame.data.iter().all(|sample| sample.unsigned_abs() < 500),
                    "muted audio leaked"
                );
            }
        })
        .await
        .expect("muted stream remains live");
        assert!(gate.set_enabled(true));
        timeout(Duration::from_secs(2), async {
            loop {
                if audio_stream
                    .next()
                    .await
                    .unwrap()
                    .data
                    .iter()
                    .any(|sample| sample.unsigned_abs() > 500)
                {
                    break;
                }
            }
        })
        .await
        .expect("new speech resumes after unmute");
        let reoffer = receiver
            .create_offer(OfferOptions::default())
            .await
            .unwrap();
        // This synthetic peer's add_track sets its preferred direction to
        // sendonly. A mixer that still wants our audio offers sendrecv.
        let mut reoffer = sdp::parse(&reoffer.to_string()).unwrap();
        for line in &mut reoffer.media {
            if line.kind == "audio" {
                for attr in &mut line.attributes {
                    if attr.name == "sendonly" {
                        attr.name = "sendrecv".into();
                    }
                }
            }
        }
        let raw_reoffer = sdp::write(&reoffer);
        receiver
            .set_local_description(SessionDescription::parse(&raw_reoffer, SdpType::Offer).unwrap())
            .await
            .unwrap();
        let mut numbers = || rand::thread_rng().gen_range(1..=u32::MAX - 100);
        let signaled = sdp::to_teams_offer(
            &raw_reoffer,
            &sdp::OfferPlan {
                screen_share_mids: vec!["11".into()],
                ..Default::default()
            },
            &mut numbers,
        )
        .unwrap();
        let mut reoffer = sdp::parse(&signaled.sdp).unwrap();
        for line in &mut reoffer.media {
            if matches!(line.direction(), Some("recvonly" | "inactive")) {
                line.attributes.retain(|attribute| {
                    !matches!(
                        attribute.name.as_str(),
                        "x-ssrc-range" | "ssrc" | "ssrc-group"
                    )
                });
            }
        }
        let teams_answer = engine
            .negotiator()
            .answer_offer(&sdp::write(&reoffer))
            .await
            .expect("renegotiation answer");
        let browser_answer =
            sdp::to_browser_answer(&teams_answer, &signaled, &raw_reoffer).unwrap();
        receiver
            .set_remote_description(
                SessionDescription::parse(&browser_answer, SdpType::Answer).unwrap(),
            )
            .await
            .expect("renegotiation applied by remote");
        if timeout(Duration::from_secs(2), audio_stream.next())
            .await
            .is_err()
        {
            let sender_audio = engine
                .peer
                .0
                .transceivers()
                .into_iter()
                .next()
                .map(|line| (line.direction(), line.current_direction()));
            let receiver_audio = receiver
                .transceivers()
                .into_iter()
                .next()
                .map(|line| (line.direction(), line.current_direction()));
            panic!(
                "renegotiation audio stalled: peers {:?}/{:?}, audio {:?}/{:?}",
                engine.connection_state(),
                receiver.connection_state(),
                sender_audio,
                receiver_audio
            );
        }
        engine.stop_camera().unwrap();
        // Drop any queued frame, then require a quiet interval while the stale
        // producer keeps submitting pictures to its old source.
        timeout(Duration::from_secs(3), async {
            loop {
                if timeout(Duration::from_millis(300), video_stream.next())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
        .await
        .expect("detached camera stops producing remote pictures");
        assert!(
            timeout(Duration::from_secs(2), audio_stream.next())
                .await
                .unwrap()
                .is_some()
        );
        assert!(!audio_bridge.failed());
        audio_task.abort();
        return_task.abort();
        video_task.abort();
        relay.abort();
        audio_bridge.stop().await;
        audio_stream.close();
        video_stream.close();
        receiver.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeated_camera_teardown_releases_threads_and_descriptors() {
        fn counts() -> (usize, usize) {
            (
                std::fs::read_dir("/proc/self/task").unwrap().count(),
                std::fs::read_dir("/proc/self/fd").unwrap().count(),
            )
        }
        async fn cycle() {
            let mut engine = Engine::prepare().await.unwrap();
            let source = engine.start_camera(160, 120).unwrap();
            engine.stop_camera().unwrap();
            drop(source);
            assert!(engine.start_camera(0, 120).is_err());
            drop(engine);
        }
        cycle().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let baseline = counts();
        for _ in 0..5 {
            cycle().await;
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
        let after = counts();
        assert!(
            after.0 <= baseline.0 + 3,
            "threads: {baseline:?} -> {after:?}"
        );
        assert!(
            after.1 <= baseline.1 + 3,
            "descriptors: {baseline:?} -> {after:?}"
        );
    }
}
