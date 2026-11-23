//! Media session orchestrator — binds UDP socket, sends/receives SRTP audio.
//!
//! The `MediaSession` ties together RTP encoding, SRTP encryption, and UDP
//! transport into a running audio stream. It applies user mute and speaker
//! volume controls at packet boundaries and records packet statistics.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::time;

use super::{ice, rtp, srtp, video};
use super::datagram::{Datagram,DatagramIo};
use super::audio_codec::{Codec,CodecSet,Encoder,Decoder};

#[cfg(feature = "audio")]
use super::audio;

/// Statistics for received media packets.
#[derive(Debug, Default)]
pub struct MediaStats {
    pub packets_sent: u64,
    /// Microphone frames actually consumed by the RTP sender.
    pub capture_frames: u64,
    /// Sent microphone frames whose RMS exceeded the fixed -50 dBFS activity threshold.
    pub capture_active_frames: u64,
    /// Current local microphone gate state observed by the RTP sender.
    pub microphone_muted: bool,
    pub capture_underruns: u64,
    /// Pre-connection PCM discarded once while the initial microphone gate is closed.
    pub capture_startup_discarded: u64,
    pub send_errors: u64,
    pub send_skipped_frames: u64,
    pub receive_auth_failures: u64,
    pub receive_header_failures: u64,
    pub packets_received: u64,
    pub bytes_received: u64,
    /// All datagrams read from the media socket, regardless of packet type.
    pub inbound_datagrams: u64,
    /// Datagrams discarded because their source differed from the authenticated ICE address.
    pub unexpected_source_datagrams: u64,
    pub stun_datagrams: u64,
    pub dtls_datagrams: u64,
    pub control_datagrams: u64,
    pub control_unprotect_failures: u64,
    pub control_parse_failures: u64,
    pub playout_rejected_packets: u64,
    /// Fixed category codes: local 1=direct, 2=relay; remote candidate 1=host, 2=srflx, 3=relay.
    pub local_transport: u8,
    pub remote_candidate_type: u8,
    /// Fixed codec codes: 1=PCMU, 2=Opus; mask bit 0=PCMU, bit 1=Opus receive support.
    pub send_codec: u8,
    pub receive_codec_mask: u8,
    pub remote_host_candidates: u8,
    pub remote_srflx_candidates: u8,
    pub remote_relay_candidates: u8,
    pub last_seq: u16,
    pub last_timestamp: u32,
    pub rtp_quality: rtp::RtpQualityStats,
    pub playout: super::audio_playout::PlayoutStats,
    pub control: super::audio_control::ControlStats,
}

/// Live user controls shared with the packet send/receive loops.
/// Values take effect on the next 20 ms audio packet.
#[derive(Clone, Debug)]
pub struct AudioControls {
    microphone_muted: Arc<AtomicBool>,
    speaker_volume_percent: Arc<AtomicU8>,
}

impl Default for AudioControls {
    fn default() -> Self {
        Self {
            microphone_muted: Arc::new(AtomicBool::new(false)),
            speaker_volume_percent: Arc::new(AtomicU8::new(100)),
        }
    }
}

impl AudioControls {
    pub fn set_microphone_muted(&self, muted: bool) {
        self.microphone_muted.store(muted, Ordering::Release);
    }

    pub fn microphone_muted(&self) -> bool {
        self.microphone_muted.load(Ordering::Acquire)
    }

    /// Set local receive volume as a percentage. Values above 100 are clamped.
    pub fn set_speaker_volume_percent(&self, percent: u8) {
        self.speaker_volume_percent
            .store(percent.min(100), Ordering::Release);
    }

    pub fn speaker_volume_percent(&self) -> u8 {
        self.speaker_volume_percent.load(Ordering::Acquire)
    }
}

/// External native PCM endpoints. The microphone is mono signed 16-bit at
/// 8000 Hz, 160 samples per packet. The speaker is 160 samples at 8000 Hz for
/// PCMU-only calls and 960 samples at 48000 Hz when Opus is offered. Callers
/// own/cancel their device processes. Supplying these channels never opens
/// CPAL/default devices as a hidden fallback.
pub struct PcmAudioIo {
    pub microphone: std::sync::mpsc::Receiver<Vec<i16>>,
    pub speaker: std::sync::mpsc::SyncSender<Vec<i16>>,
    pub initially_muted: bool,
}

/// A running media session for a single audio stream.
pub struct MediaSession {
    socket: Option<Arc<Datagram>>,
    remote_addr: SocketAddr,
    srtp_ctx: Arc<Mutex<srtp::SrtpContext>>,
    ssrc: u32,
    stats: Arc<Mutex<MediaStats>>,
    audio_controls: AudioControls,
    /// Handle to the send task (for shutdown).
    send_handle: Option<tokio::task::JoinHandle<()>>,
    /// Handle to the recv task (for shutdown).
    recv_handle: Option<tokio::task::JoinHandle<()>>,
    recv_stop: Option<tokio::sync::oneshot::Sender<()>>,
    control_handle: Option<tokio::task::JoinHandle<()>>,
    control: Option<Arc<super::control_session::ControlSession>>,
    /// Audio capture handle (kept alive so the stream stays open).
    #[cfg(feature = "audio")]
    _audio_capture: Option<audio::AudioCapture>,
    /// Audio playback handle (kept alive so the stream stays open).
    #[cfg(feature = "audio")]
    _audio_playback: Option<audio::AudioPlayback>,
}

enum Security {
    Sdes{srtp_local:srtp::SrtpKeyingMaterial,srtp_remote:srtp::SrtpKeyingMaterial},
    Dtls{identity:super::dtls::Identity,remote:super::dtls::Fingerprint,role:super::dtls::Role},
}

impl MediaSession {
    pub fn is_running(&self)->bool {
        self.send_handle.as_ref().is_some_and(|task|!task.is_finished())
            && self.recv_handle.as_ref().is_some_and(|task|!task.is_finished())
            && self.control_handle.as_ref().is_none_or(|task|!task.is_finished())
    }

    /// Create and start a new media session with ICE connectivity checks.
    ///
    /// Binds a UDP socket, runs ICE checks against remote candidates, then starts
    /// the send/recv loops to the verified remote address.
    pub async fn start_with_ice(
        local_port: u16,
        remote_candidates: &[ice::IceCandidate],
        local_creds: &ice::IceCredentials,
        remote_creds: &ice::IceCredentials,
        local_material: &srtp::SrtpKeyingMaterial,
        remote_material: &srtp::SrtpKeyingMaterial,
    ) -> Result<Self> {
        let bind_addr = format!("0.0.0.0:{}", local_port);
        let socket = UdpSocket::bind(&bind_addr)
            .await
            .with_context(|| format!("Failed to bind UDP socket on {}", bind_addr))?;

        Self::start_with_ice_on_socket(
            socket,
            remote_candidates,
            local_creds,
            remote_creds,
            local_material,
            remote_material,
            false,
        )
        .await
    }

    /// Start using the UDP socket already reserved before SDP generation.
    /// Ownership is transferred without closing/rebinding its advertised port.
    /// ICE failure never selects an unverified route or starts audio capture.
    pub async fn start_with_ice_on_socket(
        socket: UdpSocket,
        remote_candidates: &[ice::IceCandidate],
        local_creds: &ice::IceCredentials,
        remote_creds: &ice::IceCredentials,
        local_material: &srtp::SrtpKeyingMaterial,
        remote_material: &srtp::SrtpKeyingMaterial,
        controlling: bool,
    ) -> Result<Self> {
        Self::start_with_ice_and_audio(
            socket,
            remote_candidates,
            local_creds,
            remote_creds,
            local_material,
            remote_material,
            controlling,
            None,
        )
        .await
    }

    pub async fn start_with_ice_and_audio(
        socket: UdpSocket,
        remote_candidates: &[ice::IceCandidate],
        local_creds: &ice::IceCredentials,
        remote_creds: &ice::IceCredentials,
        local_material: &srtp::SrtpKeyingMaterial,
        remote_material: &srtp::SrtpKeyingMaterial,
        controlling: bool,
        external_audio: Option<PcmAudioIo>,
    ) -> Result<Self> {
        Self::start_with_transport(Datagram::Direct(socket),remote_candidates,local_creds,remote_creds,local_material,remote_material,controlling,external_audio).await
    }

    pub async fn start_with_transport(
        socket:Datagram,
        remote_candidates:&[ice::IceCandidate],
        local_creds:&ice::IceCredentials,
        remote_creds:&ice::IceCredentials,
        local_material:&srtp::SrtpKeyingMaterial,
        remote_material:&srtp::SrtpKeyingMaterial,
        controlling:bool,
        external_audio:Option<PcmAudioIo>,
    )->Result<Self>{
        Self::start_with_transport_and_codec(socket,remote_candidates,local_creds,remote_creds,local_material,remote_material,controlling,external_audio,Codec::Pcmu).await
    }
    pub async fn start_with_transport_and_codec(
        socket:Datagram,remote_candidates:&[ice::IceCandidate],local_creds:&ice::IceCredentials,remote_creds:&ice::IceCredentials,
        local_material:&srtp::SrtpKeyingMaterial,remote_material:&srtp::SrtpKeyingMaterial,controlling:bool,external_audio:Option<PcmAudioIo>,codec:Codec,
    )->Result<Self>{
        Self::start_with_codecs(socket,remote_candidates,local_creds,remote_creds,local_material,remote_material,controlling,external_audio,CodecSet::single(codec)).await
    }
    pub async fn start_with_codecs(
        socket:Datagram,remote_candidates:&[ice::IceCandidate],local_creds:&ice::IceCredentials,remote_creds:&ice::IceCredentials,
        local_material:&srtp::SrtpKeyingMaterial,remote_material:&srtp::SrtpKeyingMaterial,controlling:bool,external_audio:Option<PcmAudioIo>,codecs:CodecSet,
    )->Result<Self>{
        Self::start_with_security(socket,remote_candidates,local_creds,remote_creds,
            Security::Sdes{srtp_local:local_material.clone(),srtp_remote:remote_material.clone()},controlling,external_audio,codecs,None).await
    }
    pub async fn start_with_dtls(
        socket:Datagram,remote_candidates:&[ice::IceCandidate],local_creds:&ice::IceCredentials,remote_creds:&ice::IceCredentials,
        identity:super::dtls::Identity,fingerprint:super::dtls::Fingerprint,role:super::dtls::Role,
        controlling:bool,external_audio:PcmAudioIo,codecs:CodecSet,
    )->Result<Self>{
        Self::start_with_security(socket,remote_candidates,local_creds,remote_creds,
            Security::Dtls{identity,remote:fingerprint,role},controlling,Some(external_audio),codecs,None).await
    }
    /// Keep the RTP/RTCP sender bound to the source advertised before ICE.
    pub async fn start_with_codecs_for_ssrc(
        socket:Datagram,remote_candidates:&[ice::IceCandidate],local_creds:&ice::IceCredentials,remote_creds:&ice::IceCredentials,
        local_material:&srtp::SrtpKeyingMaterial,remote_material:&srtp::SrtpKeyingMaterial,controlling:bool,external_audio:PcmAudioIo,codecs:CodecSet,ssrc:u32,
    )->Result<Self>{
        Self::start_with_security(socket,remote_candidates,local_creds,remote_creds,
            Security::Sdes{srtp_local:local_material.clone(),srtp_remote:remote_material.clone()},controlling,Some(external_audio),codecs,Some(ssrc)).await
    }
    pub async fn start_with_dtls_for_ssrc(
        socket:Datagram,remote_candidates:&[ice::IceCandidate],local_creds:&ice::IceCredentials,remote_creds:&ice::IceCredentials,
        identity:super::dtls::Identity,fingerprint:super::dtls::Fingerprint,role:super::dtls::Role,
        controlling:bool,external_audio:PcmAudioIo,codecs:CodecSet,ssrc:u32,
    )->Result<Self>{
        Self::start_with_security(socket,remote_candidates,local_creds,remote_creds,
            Security::Dtls{identity,remote:fingerprint,role},controlling,Some(external_audio),codecs,Some(ssrc)).await
    }
    async fn start_with_security(
        socket:Datagram,remote_candidates:&[ice::IceCandidate],local_creds:&ice::IceCredentials,remote_creds:&ice::IceCredentials,
        security:Security,controlling:bool,external_audio:Option<PcmAudioIo>,codecs:CodecSet,source:Option<u32>,
    )->Result<Self>{
        if (codecs.send==Codec::Pcmu&&!codecs.pcmu)||(codecs.send==Codec::Opus&&!codecs.opus)||codecs.max_bitrate.is_some_and(|value|!(6000..=510000).contains(&value)){anyhow::bail!("Invalid negotiated audio codecs");}
        let mut encoder=Encoder::new(codecs.send)?;encoder.limit_bitrate(codecs.max_bitrate)?;
        // Verify required system decoder before ICE/device startup. Each SSRC
        // owns its actual decoder later in AudioPlayout.
        if codecs.opus { let _ = Decoder::new(Codec::Opus)?; }

        let socket=Arc::new(socket);

        // Run ICE connectivity checks
        let agent = ice::IceAgent::new(local_creds.clone(), remote_creds.clone(), controlling);

        let route=match agent.check_connectivity(socket.clone(),remote_candidates).await {
            Ok(route)=>route,
            Err(error)=>{let _=socket.close().await;return Err(error.context("ICE did not authenticate a media route"));}
        };
        let remote_addr=route.remote_addr;
        let remote_candidate_type=match route.candidate_type {
            ice::CandidateType::Host=>1,
            ice::CandidateType::ServerReflexive=>2,
            ice::CandidateType::Relay=>3,
        };
        let local_transport=match &*socket {
            Datagram::Direct(_)=>1,
            Datagram::Relay(_)=>2,
        };
        let protected:Result<_>=async {
            match security {
                Security::Sdes{srtp_local,srtp_remote}=>Ok((srtp::create_context(&srtp_local,&srtp_remote)?,None)),
                Security::Dtls{identity,remote,role}=>{
                    let outcome=identity.handshake(&socket,remote_addr,&agent,remote,role).await?;
                    Ok((srtp::create_context(&outcome.local,&outcome.remote)?,Some(outcome.session)))
                }
            }
        }.await;
        let (srtp_ctx,dtls)=match protected {
            Ok(protection)=>protection,
            Err(error)=>{let _=socket.close().await;return Err(error);}
        };
        let ssrc = source.unwrap_or_else(|| super::video::generate_ssrc().max(1));

        let control = Arc::new(super::control_session::ControlSession::new(&srtp_ctx,ssrc,codecs.send.clock_rate(),
            if codecs.send==Codec::Pcmu{64000}else{codecs.max_bitrate.unwrap_or(16000).min(16000)})?);
        let srtp_ctx = Arc::new(Mutex::new(srtp_ctx));
        let mut initial_stats=MediaStats::default();
        initial_stats.local_transport=local_transport;
        initial_stats.remote_candidate_type=remote_candidate_type;
        initial_stats.send_codec=match codecs.send {Codec::Pcmu=>1,Codec::Opus=>2};
        initial_stats.receive_codec_mask=u8::from(codecs.pcmu)|(u8::from(codecs.opus)<<1);
        for candidate in remote_candidates.iter().filter(|candidate|candidate.transport==ice::Transport::Udp&&candidate.component==1) {
            match candidate.candidate_type {
                ice::CandidateType::Host=>initial_stats.remote_host_candidates=initial_stats.remote_host_candidates.saturating_add(1),
                ice::CandidateType::ServerReflexive=>initial_stats.remote_srflx_candidates=initial_stats.remote_srflx_candidates.saturating_add(1),
                ice::CandidateType::Relay=>initial_stats.remote_relay_candidates=initial_stats.remote_relay_candidates.saturating_add(1),
            }
        }
        let stats = Arc::new(Mutex::new(initial_stats));
        let audio_controls = AudioControls::default();
        audio_controls
            .set_microphone_muted(external_audio.as_ref().is_some_and(|io| io.initially_muted));

        // External endpoints take precedence over optional legacy CPAL I/O.
        #[cfg(feature = "audio")]
        let (audio_capture, default_mic) = if external_audio.is_none() {
            match audio::AudioCapture::start() {
                Some((capture, rx)) => (Some(capture), Some(rx)),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        #[cfg(feature = "audio")]
        let (audio_playback, default_speaker) = if external_audio.is_none() {
            match audio::AudioPlayback::start() {
                Some((playback, tx)) => (Some(playback), Some(tx)),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        #[cfg(not(feature = "audio"))]
        let (default_mic, default_speaker) = (None, None);
        let (mic_rx, spk_tx) = match external_audio {
            Some(io) => (Some(io.microphone), Some(io.speaker)),
            None => (default_mic, default_speaker),
        };

        let send_handle = {
            let socket = socket.clone();
            let srtp_ctx = srtp_ctx.clone();
            let stats = stats.clone();
            let audio_controls = audio_controls.clone();
            tokio::spawn(send_loop(
                socket,
                remote_addr,
                srtp_ctx,
                ssrc,
                stats,
                mic_rx,
                audio_controls.clone(),
                encoder,
                Some(control.clone()),
            ))
        };

        let (recv_stop, recv_stopped) = tokio::sync::oneshot::channel();
        let recv_handle = {
            let socket = socket.clone();
            let srtp_ctx = srtp_ctx.clone();
            let stats = stats.clone();
            let audio_controls = audio_controls.clone();
            tokio::spawn(recv_loop_with_stun(
                socket,
                srtp_ctx,
                stats,
                agent,
                spk_tx,
                audio_controls,
                codecs,
                remote_addr,
                Some(control.clone()),
                dtls,
                Some(recv_stopped),
            ))
        };

        let control_handle=tokio::spawn(control.clone().run(socket.clone(),remote_addr));
        Ok(MediaSession {
            socket: Some(socket),
            remote_addr,
            srtp_ctx,
            ssrc,
            stats,
            audio_controls,
            send_handle: Some(send_handle),
            recv_handle: Some(recv_handle),
            recv_stop: Some(recv_stop),
            control_handle:Some(control_handle),
            control:Some(control),
            #[cfg(feature = "audio")]
            _audio_capture: audio_capture,
            #[cfg(feature = "audio")]
            _audio_playback: audio_playback,
        })
    }

    /// Create and start a new media session (no ICE checks, direct send to remote).
    ///
    /// - `local_port`: UDP port to bind on (0 for auto-assign).
    /// - `remote_addr`: Remote peer's RTP address (from ICE candidate selection).
    /// - `local_material`: Our SRTP keying material (from our SDP answer).
    /// - `remote_material`: Remote SRTP keying material (from their SDP offer).
    pub async fn start(
        local_port: u16,
        remote_addr: SocketAddr,
        local_material: &srtp::SrtpKeyingMaterial,
        remote_material: &srtp::SrtpKeyingMaterial,
    ) -> Result<Self> {
        let bind_addr = format!("0.0.0.0:{}", local_port);
        let socket = UdpSocket::bind(&bind_addr)
            .await
            .with_context(|| format!("Failed to bind UDP socket on {}", bind_addr))?;

        let local_addr = socket.local_addr()?;
        tracing::info!(
            "Media session bound to {}, remote: {}",
            local_addr,
            remote_addr
        );

        let srtp_ctx = srtp::create_context(local_material, remote_material)?;

        // Generate a random-ish SSRC from uuid
        let ssrc = {
            let id = uuid::Uuid::new_v4();
            let bytes = id.as_bytes();
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        };

        let socket = Arc::new(Datagram::Direct(socket));
        let srtp_ctx = Arc::new(Mutex::new(srtp_ctx));
        let stats = Arc::new(Mutex::new(MediaStats::default()));
        let audio_controls = AudioControls::default();

        // Initialize audio devices (optional — graceful fallback to silence).
        #[cfg(feature = "audio")]
        let (audio_capture, mic_rx) = match audio::AudioCapture::start() {
            Some((cap, rx)) => (Some(cap), Some(rx)),
            None => (None, None),
        };
        #[cfg(not(feature = "audio"))]
        let mic_rx: Option<std::sync::mpsc::Receiver<Vec<i16>>> = None;

        #[cfg(feature = "audio")]
        let (audio_playback, spk_tx) = match audio::AudioPlayback::start() {
            Some((pb, tx)) => (Some(pb), Some(tx)),
            None => (None, None),
        };
        #[cfg(not(feature = "audio"))]
        let spk_tx: Option<std::sync::mpsc::SyncSender<Vec<i16>>> = None;

        let send_handle = {
            let socket = socket.clone();
            let srtp_ctx = srtp_ctx.clone();
            let stats = stats.clone();
            let audio_controls = audio_controls.clone();
            tokio::spawn(send_loop(
                socket,
                remote_addr,
                srtp_ctx,
                ssrc,
                stats,
                mic_rx,
                audio_controls.clone(),
                Encoder::new(Codec::Pcmu)?,
                None,
            ))
        };

        let recv_handle = {
            let socket = socket.clone();
            let srtp_ctx = srtp_ctx.clone();
            let stats = stats.clone();
            let audio_controls = audio_controls.clone();
            tokio::spawn(recv_loop(socket, srtp_ctx, stats, spk_tx, audio_controls))
        };

        Ok(MediaSession {
            socket: Some(socket),
            remote_addr,
            srtp_ctx,
            ssrc,
            stats,
            audio_controls,
            send_handle: Some(send_handle),
            recv_handle: Some(recv_handle),
            recv_stop: None,
            control_handle:None,
            control:None,
            #[cfg(feature = "audio")]
            _audio_capture: audio_capture,
            #[cfg(feature = "audio")]
            _audio_playback: audio_playback,
        })
    }

    /// Get the local port this session is bound to.
    pub fn local_port(&self) -> Result<u16> {
        let socket = self
            .socket
            .as_ref()
            .context("Media session has already stopped")?;
        Ok(socket.local_addr()?.port())
    }

    /// Get current media statistics.
    pub async fn stats(&self) -> MediaStats {
        let control = match &self.control {
            Some(c) => Some(c.stats().await),
            None => None,
        };
        let s = self.stats.lock().await;
        MediaStats {
            packets_sent: s.packets_sent,
            capture_frames: s.capture_frames,
            capture_active_frames: s.capture_active_frames,
            microphone_muted: s.microphone_muted,
            capture_underruns: s.capture_underruns,
            capture_startup_discarded: s.capture_startup_discarded,
            send_errors: s.send_errors,
            send_skipped_frames: s.send_skipped_frames,
            receive_auth_failures: s.receive_auth_failures,
            receive_header_failures: s.receive_header_failures,
            packets_received: s.packets_received,
            bytes_received: s.bytes_received,
            inbound_datagrams: s.inbound_datagrams,
            unexpected_source_datagrams: s.unexpected_source_datagrams,
            stun_datagrams: s.stun_datagrams,
            dtls_datagrams: s.dtls_datagrams,
            control_datagrams: s.control_datagrams,
            control_unprotect_failures: s.control_unprotect_failures,
            control_parse_failures: s.control_parse_failures,
            playout_rejected_packets: s.playout_rejected_packets,
            local_transport: s.local_transport,
            remote_candidate_type: s.remote_candidate_type,
            send_codec: s.send_codec,
            receive_codec_mask: s.receive_codec_mask,
            remote_host_candidates: s.remote_host_candidates,
            remote_srflx_candidates: s.remote_srflx_candidates,
            remote_relay_candidates: s.remote_relay_candidates,
            last_seq: s.last_seq,
            last_timestamp: s.last_timestamp,
            rtp_quality: s.rtp_quality,
            playout: s.playout,
            control: control.unwrap_or(s.control),
        }
    }

    /// Return a handle that can mute transmitted microphone frames and adjust
    /// local playback level while this media session is active.
    pub fn audio_controls(&self) -> AudioControls {
        self.audio_controls.clone()
    }

    /// Stop the media session.
    pub async fn stop(&mut self) {
        if let Some(h) = self.send_handle.take() {
            h.abort();
            let _ = h.await;
        }
        if let Some(h) = self.control_handle.take() {
            h.abort();
            let _ = h.await;
        }
        if let (Some(control), Some(socket)) = (self.control.take(), self.socket.as_ref()) {
            let _ = control.send(socket, self.remote_addr, true).await;
            let final_control = control.stats().await;
            self.stats.lock().await.control = final_control;
        }
        let graceful = self.recv_stop.take().is_some_and(|signal| signal.send(()).is_ok());
        if let Some(mut task) = self.recv_handle.take() {
            if !graceful { task.abort(); }
            // The receiver owns DTLS. Let it emit close_notify after SRTCP BYE,
            // but never wait for a remote shutdown acknowledgement.
            if time::timeout(Duration::from_millis(350), &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
        #[cfg(feature = "audio")]
        {
            self._audio_capture = None;
            self._audio_playback = None;
        }
        if let Some(socket) = self.socket.take() {
            let _ = socket.close().await;
        }
        let stats = self.stats.lock().await;
        tracing::info!(
            "Media session stopped. Sent: {}, Received: {} ({} bytes), estimated loss: {}, jitter: {:.2} ms",
            stats.packets_sent,
            stats.packets_received,
            stats.bytes_received,
            stats.rtp_quality.estimated_lost_packets,
            stats.rtp_quality.jitter_millis(),
        );
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        if let Some(h) = self.send_handle.take() {
            h.abort();
        }
        if let Some(h) = self.control_handle.take() {
            h.abort();
        }
        if let Some(h) = self.recv_handle.take() {
            h.abort();
        }
    }
}

/// Send loop: every 20 ms, encode negotiated audio, encrypt with SRTP, send.
async fn send_loop(
    socket: Arc<Datagram>,
    remote_addr: SocketAddr,
    srtp_ctx: Arc<Mutex<srtp::SrtpContext>>,
    ssrc: u32,
    stats: Arc<Mutex<MediaStats>>,
    mic_rx: Option<std::sync::mpsc::Receiver<Vec<i16>>>,
    audio_controls: AudioControls,
    mut encoder: Encoder,
    control: Option<Arc<super::control_session::ControlSession>>,
) {
    let mut seq: u16 = 0;
    let origin = control.as_ref().map(|c|c.origin).unwrap_or_else(time::Instant::now);
    let mut interval = time::interval_at(origin, Duration::from_millis(rtp::PACKET_INTERVAL_MS));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    // Native capture can fill its bounded queue while ICE is connecting.
    // Align once while muted; subsequent ticks always preserve FIFO order.
    let discarded = if audio_controls.microphone_muted() {
        mic_rx.as_ref().map_or(0, discard_initial_capture)
    } else { 0 };
    stats.lock().await.capture_startup_discarded = discarded;
    let has_mic = mic_rx.is_some();
    let mut previous_slot: Option<u128> = None;
    tracing::info!(
        "Media send loop started (SSRC: {:#010x}, mic: {})",
        ssrc,
        if has_mic { "live" } else { "silence" }
    );

    loop {
        let tick = interval.tick().await;

        // One 20 ms frame, in order. Skipping to the newest frame chops speech
        // whenever capture is a frame ahead. The caller bounds the capture queue.
        let samples = mic_rx.as_ref().and_then(|rx| rx.try_recv().ok());
        let has_signal=samples.as_ref().is_some_and(|frame|pcm_frame_has_signal(frame));
        // Use the media schedule, rather than execution-time jitter, for RTP
        // timestamps. The tick includes skipped deadlines after a scheduler stall.
        let slots = tick.duration_since(origin).as_millis() / rtp::PACKET_INTERVAL_MS as u128;
        let timestamp = slots.wrapping_mul(encoder.codec().timestamp_increment() as u128) as u32;
        let muted = audio_controls.microphone_muted();
        let underrun = has_mic && samples.is_none() && !muted;
        let skipped = previous_slot.map(|prev| slots.saturating_sub(prev + 1)).unwrap_or(0) as u64;
        previous_slot = Some(slots);
        {
            let mut s = stats.lock().await;
            s.capture_frames+=u64::from(samples.is_some());
            s.microphone_muted=muted;
            s.capture_underruns += u64::from(underrun);
            s.send_skipped_frames += skipped;
        }
        let Ok(payload) = encoder.encode(samples, muted) else {
            break;
        };
        let rtp_packet = rtp::encode(
            encoder.codec().payload_type(),
            seq,
            timestamp,
            ssrc,
            &payload,
        );

        let srtp_packet = {
            let mut ctx = srtp_ctx.lock().await;
            match srtp::protect(&mut ctx, &rtp_packet) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("SRTP protect failed: {:#}", e);
                    continue;
                }
            }
        };

        match socket.send_to(&srtp_packet, remote_addr).await {
            Ok(_) => {
                let mut s = stats.lock().await;
                s.packets_sent += 1;
                s.capture_active_frames+=u64::from(has_signal&&!muted);
                drop(s);
                if let Some(control)=&control {control.state.lock().await.reports.sent_rtp(payload.len());}
            }
            Err(e) => {
                stats.lock().await.send_errors += 1;
                tracing::warn!("UDP send failed: {:#}", e);
            }
        }

        seq = seq.wrapping_add(1);
    }
}

/// Bounded even for a continuously producing external endpoint. Native queue
/// capacity is eight frames; no draining occurs after this initial boundary.
fn discard_initial_capture(rx: &std::sync::mpsc::Receiver<Vec<i16>>) -> u64 {
    let mut discarded = 0;
    while discarded < 64 && rx.try_recv().is_ok() { discarded += 1; }
    discarded
}

fn pcm_frame_has_signal(samples:&[i16])->bool {
    if samples.is_empty(){return false;}
    let energy=samples.iter().map(|sample|i64::from(*sample)*i64::from(*sample)).sum::<i64>() as u64;
    energy/samples.len() as u64>10_816 // RMS > 104, approximately -50 dBFS.
}

fn scale_speaker_volume(samples: &[i16], volume_percent: u8) -> Vec<i16> {
    let volume_percent = volume_percent.min(100) as i32;
    if volume_percent == 100 {
        return samples.to_vec();
    }
    samples
        .iter()
        .map(|sample| ((*sample as i32 * volume_percent) / 100) as i16)
        .collect()
}

fn rtp_arrival_clock_ticks(origin: Instant) -> u32 {
    (origin.elapsed().as_nanos().saturating_mul(8_000) / 1_000_000_000) as u32
}

/// Statistics for received video packets.
#[derive(Debug, Default)]
pub struct VideoStats {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub bytes_received: u64,
    pub frames_sent: u64,
    pub frames_received: u64,
}

/// A running media session for a video stream.
pub struct VideoMediaSession {
    socket: Option<Arc<UdpSocket>>,
    remote_addr: SocketAddr,
    stats: Arc<Mutex<VideoStats>>,
    send_handle: Option<tokio::task::JoinHandle<()>>,
    recv_handle: Option<tokio::task::JoinHandle<()>>,
}

impl VideoMediaSession {
    /// Create and start a new video media session.
    pub async fn start(
        local_port: u16,
        remote_addr: SocketAddr,
        local_material: &srtp::SrtpKeyingMaterial,
        remote_material: &srtp::SrtpKeyingMaterial,
    ) -> Result<Self> {
        let bind_addr = format!("0.0.0.0:{}", local_port);
        let socket = UdpSocket::bind(&bind_addr)
            .await
            .with_context(|| format!("Failed to bind video UDP socket on {}", bind_addr))?;

        let local_addr = socket.local_addr()?;
        tracing::info!(
            "Video session bound to {}, remote: {}",
            local_addr,
            remote_addr
        );

        let srtp_ctx = srtp::create_context(local_material, remote_material)?;

        let ssrc = {
            let id = uuid::Uuid::new_v4();
            let bytes = id.as_bytes();
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        };

        let socket = Arc::new(socket);
        let srtp_ctx = Arc::new(Mutex::new(srtp_ctx));
        let stats = Arc::new(Mutex::new(VideoStats::default()));

        let send_handle = {
            let socket = socket.clone();
            let srtp_ctx = srtp_ctx.clone();
            let stats = stats.clone();
            tokio::spawn(video_send_loop(socket, remote_addr, srtp_ctx, ssrc, stats))
        };

        let recv_handle = {
            let socket = socket.clone();
            let srtp_ctx = srtp_ctx.clone();
            let stats = stats.clone();
            tokio::spawn(video_recv_loop(socket, srtp_ctx, stats))
        };

        Ok(VideoMediaSession {
            socket: Some(socket),
            remote_addr,
            stats,
            send_handle: Some(send_handle),
            recv_handle: Some(recv_handle),
        })
    }

    /// Get the local port this session is bound to.
    pub fn local_port(&self) -> Result<u16> {
        let socket = self
            .socket
            .as_ref()
            .context("Video media session has already stopped")?;
        Ok(socket.local_addr()?.port())
    }

    /// Get current video statistics.
    pub async fn stats(&self) -> VideoStats {
        let s = self.stats.lock().await;
        VideoStats {
            packets_sent: s.packets_sent,
            packets_received: s.packets_received,
            bytes_received: s.bytes_received,
            frames_sent: s.frames_sent,
            frames_received: s.frames_received,
        }
    }

    /// Stop the video session.
    pub async fn stop(&mut self) {
        if let Some(h) = self.send_handle.take() {
            h.abort();
            let _ = h.await;
        }
        if let Some(h) = self.recv_handle.take() {
            h.abort();
            let _ = h.await;
        }
        self.socket = None;
        let stats = self.stats.lock().await;
        tracing::info!(
            "Video session stopped. Sent: {} pkts/{} frames, Received: {} pkts/{} frames ({} bytes)",
            stats.packets_sent,
            stats.frames_sent,
            stats.packets_received,
            stats.frames_received,
            stats.bytes_received
        );
    }
}

impl Drop for VideoMediaSession {
    fn drop(&mut self) {
        if let Some(h) = self.send_handle.take() {
            h.abort();
        }
        if let Some(h) = self.recv_handle.take() {
            h.abort();
        }
    }
}

/// Video send loop: periodically send black H.264 I-frames.
async fn video_send_loop(
    socket: Arc<UdpSocket>,
    remote_addr: SocketAddr,
    srtp_ctx: Arc<Mutex<srtp::SrtpContext>>,
    ssrc: u32,
    stats: Arc<Mutex<VideoStats>>,
) {
    let mut packetizer = video::VideoPacketizer::new(ssrc);
    let mut interval = time::interval(Duration::from_millis(video::FRAME_INTERVAL_MS));

    tracing::info!(
        "Video send loop started (SSRC: {:#010x}, 15fps black frames)",
        ssrc
    );

    loop {
        interval.tick().await;

        let nal_units = video::generate_black_iframe();
        let rtp_packets = packetizer.packetize_frame(&nal_units);

        for rtp_pkt in &rtp_packets {
            let srtp_packet = {
                let mut ctx = srtp_ctx.lock().await;
                match srtp::protect(&mut ctx, rtp_pkt) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!("Video SRTP protect failed: {:#}", e);
                        continue;
                    }
                }
            };

            match socket.send_to(&srtp_packet, remote_addr).await {
                Ok(_) => {
                    let mut s = stats.lock().await;
                    s.packets_sent += 1;
                }
                Err(e) => {
                    tracing::warn!("Video UDP send failed: {:#}", e);
                }
            }
        }

        let mut s = stats.lock().await;
        s.frames_sent += 1;
    }
}

/// Video receive loop: receive and depacketize H.264 NAL units.
async fn video_recv_loop(
    socket: Arc<UdpSocket>,
    srtp_ctx: Arc<Mutex<srtp::SrtpContext>>,
    stats: Arc<Mutex<VideoStats>>,
) {
    let mut buf = [0u8; 2048];
    let mut depacketizer = video::VideoDepacketizer::new();

    tracing::info!("Video recv loop started");

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, from)) => {
                let data = &buf[..len];

                if len >= 20 && super::ice::is_stun_response(data) {
                    tracing::debug!("Video: STUN response from {}", from);
                    continue;
                }

                let mut ctx = srtp_ctx.lock().await;
                match srtp::unprotect(&mut ctx, data) {
                    Ok(rtp_data) => {
                        if let Ok(pkt) = rtp::decode(&rtp_data) {
                            let mut s = stats.lock().await;
                            s.packets_received += 1;
                            s.bytes_received += rtp_data.len() as u64;

                            let marker = pkt.marker;
                            drop(s); // release lock before depacketize

                            match depacketizer.depacketize(&pkt.payload, marker) {
                                Ok(Some(nal)) => {
                                    let mut s = stats.lock().await;
                                    s.frames_received = depacketizer.frames_received;
                                    if depacketizer.frames_received % 50 == 1 {
                                        tracing::info!(
                                            "Recv video NAL: type={}, size={} bytes (frames: {})",
                                            nal[0] & 0x1F,
                                            nal.len(),
                                            depacketizer.frames_received
                                        );
                                    }
                                }
                                Ok(None) => {} // more fragments needed
                                Err(e) => {
                                    tracing::debug!("Video depacketize error: {:#}", e);
                                }
                            }
                        }
                    }
                    Err(_) => {
                        tracing::trace!(
                            "Could not decrypt video packet from {} ({} bytes)",
                            from,
                            len
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Video UDP recv error: {:#}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Receive loop: receive UDP packets, try to decrypt SRTP, decode and play audio.
async fn recv_loop(
    socket: Arc<Datagram>,
    srtp_ctx: Arc<Mutex<srtp::SrtpContext>>,
    stats: Arc<Mutex<MediaStats>>,
    spk_tx: Option<std::sync::mpsc::SyncSender<Vec<i16>>>,
    audio_controls: AudioControls,
) {
    let mut buf = [0u8; 2048];
    let clock_origin = Instant::now();
    let mut quality_tracker = rtp::RtpQualityTracker::default();

    tracing::info!("Media recv loop started");

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, from)) => {
                let data = &buf[..len];

                // Skip STUN packets
                if len >= 20 && super::ice::is_stun_response(data) {
                    tracing::debug!("Received STUN response from {}", from);
                    continue;
                }

                // Try SRTP decrypt
                let mut ctx = srtp_ctx.lock().await;
                match srtp::unprotect(&mut ctx, data) {
                    Ok(rtp_data) => {
                        if let Ok(pkt) = rtp::decode(&rtp_data) {
                            if pkt.payload_type != rtp::PT_PCMU {
                                continue;
                            }
                            let quality = quality_tracker.observe(
                                pkt.sequence_number,
                                pkt.timestamp,
                                rtp_arrival_clock_ticks(clock_origin),
                            );
                            let mut s = stats.lock().await;
                            s.packets_received += 1;
                            s.bytes_received += rtp_data.len() as u64;
                            s.last_seq = pkt.sequence_number;
                            s.last_timestamp = pkt.timestamp;
                            s.rtp_quality = quality;

                            if s.packets_received % 250 == 1 {
                                tracing::info!(
                                    "Recv audio: seq={}, ts={}, pt={}, payload={} bytes (total: {} pkts)",
                                    pkt.sequence_number,
                                    pkt.timestamp,
                                    pkt.payload_type,
                                    pkt.payload.len(),
                                    s.packets_received
                                );
                            }
                            drop(s);

                            // Decode PCMU and send to speaker.
                            if let Some(ref tx) = spk_tx {
                                let samples: Vec<i16> = pkt
                                    .payload
                                    .iter()
                                    .map(|&b| rtp::ulaw_to_linear(b))
                                    .collect();
                                let samples = scale_speaker_volume(
                                    &samples,
                                    audio_controls.speaker_volume_percent(),
                                );
                                let _ = tx.try_send(samples);
                            }
                        }
                    }
                    Err(_) => {
                        tracing::trace!("Could not decrypt packet from {} ({} bytes)", from, len);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("UDP recv error: {:#}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Receive loop that also responds to incoming STUN binding requests.
///
/// This is used when ICE is active — the peer continues sending STUN checks
/// during the media session and expects responses to keep the path alive.
async fn recv_loop_with_stun(
    socket: Arc<Datagram>,
    srtp_ctx: Arc<Mutex<srtp::SrtpContext>>,
    stats: Arc<Mutex<MediaStats>>,
    ice_agent: ice::IceAgent,
    spk_tx: Option<std::sync::mpsc::SyncSender<Vec<i16>>>,
    audio_controls: AudioControls,
    codecs: CodecSet,
    remote_addr: SocketAddr,
    control: Option<Arc<super::control_session::ControlSession>>,
    mut dtls: Option<super::dtls::Session>,
    mut stop: Option<tokio::sync::oneshot::Receiver<()>>,
) {
    let mut buf = [0u8; 2048];
    let origin = Instant::now();
    let mut secure_rx = {
        let ctx = srtp_ctx.lock().await;
        super::media_protection::RtpReceiver::new(&ctx)
    };
    let mut playout = super::audio_playout::AudioPlayout::new(codecs);
    let mut cadence = time::interval(Duration::from_millis(20));
    cadence.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut speaker_dropped = 0;
    let mut device_silence_frames = 0;
    let silent_samples = if codecs.opus { 960 } else { rtp::SAMPLES_PER_PACKET };
    let mut dtls_clock=time::interval(Duration::from_millis(100));
    dtls_clock.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = async {
                match &mut stop {
                    Some(signal) => { let _ = signal.await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                if let Some(session) = &mut dtls {
                    let _ = session.shutdown(&socket, remote_addr).await;
                }
                break;
            }
            _ = cadence.tick() => {
                let rendered = playout.render(origin.elapsed());
                if let Some(tx) = &spk_tx {
                    // The device clock continues through prefill and DTX. Leaving
                    // pw-play's blocking raw fread without samples starves playback
                    // and makes the next talkspurt resume on an underrun.
                    let frame = match rendered {
                        Some(frame) => scale_speaker_volume(&frame, audio_controls.speaker_volume_percent()),
                        None => { device_silence_frames += 1; vec![0; silent_samples] }
                    };
                    if tx.try_send(frame).is_err() { speaker_dropped += 1; }
                }
                let mut stats = stats.lock().await;
                stats.playout = playout.stats();
                stats.playout.speaker_dropped_frames = speaker_dropped;
                stats.playout.device_silence_frames = device_silence_frames;
                stats.rtp_quality = playout.quality();
            }
            _ = dtls_clock.tick(), if dtls.is_some() => {
                if dtls.as_mut().unwrap().tick(&socket,remote_addr).await.is_err(){break;}
            }
            received = socket.recv_from(&mut buf) => {
                let Ok((length,from)) = received else {break;};
                let data = &buf[..length];
                stats.lock().await.inbound_datagrams+=1;
                if length >= 20 && ice::is_stun_message(data) {
                    stats.lock().await.stun_datagrams+=1;
                    if ice::is_stun_request(data) {
                        ice_agent.handle_stun_request(&*socket,data,from).await;
                    }
                    continue;
                }
                if from != remote_addr { stats.lock().await.unexpected_source_datagrams+=1; continue; }
                if super::dtls::is_dtls(data) {
                    stats.lock().await.dtls_datagrams+=1;
                    if let Some(session)=&mut dtls {
                        if !matches!(session.receive(data,&socket,remote_addr).await, Ok(super::dtls::Received::Progress)){break;}
                    }
                    continue;
                }
                if super::audio_control::is_control(data) {
                    stats.lock().await.control_datagrams+=1;
                    if let Some(control)=&control {
                        let mut state=control.state.lock().await;
                        match state.cipher.unprotect(data) {
                            Err(_) => {
                                state.reports.rejected();
                                drop(state);
                                stats.lock().await.control_unprotect_failures += 1;
                            }
                            Ok(bytes) => match state.reports.receive_control(&bytes,control.origin.elapsed()) {
                                Ok(sources) => for source in sources {secure_rx.retire(source);playout.remove_source(source);},
                                Err(_) => {
                                    state.reports.rejected();
                                    drop(state);
                                    stats.lock().await.control_parse_failures += 1;
                                }
                            }
                        }
                    }
                    continue;
                }
                // Per-source ROC/replay state changes only after authentication.
                let Ok((bytes,index))=secure_rx.unprotect(data) else {
                    stats.lock().await.receive_auth_failures += 1;
                    continue;
                };
                let Ok(packet) = rtp::decode(&bytes) else {
                    stats.lock().await.receive_header_failures += 1;
                    continue;
                };
                if !playout.push(&packet,origin.elapsed()) {stats.lock().await.playout_rejected_packets+=1;continue;}
                if let Some(control)=&control {
                    let clock=if packet.payload_type==111{48000}else{8000};
                    control.state.lock().await.reports.receive_rtp(&packet,index,clock,control.origin.elapsed());
                }
                let mut stats = stats.lock().await;
                stats.packets_received += 1;
                stats.bytes_received += bytes.len() as u64;
                stats.last_seq = packet.sequence_number;
                stats.last_timestamp = packet.timestamp;
                stats.rtp_quality = playout.quality();
                stats.playout = playout.stats();
                stats.playout.speaker_dropped_frames = speaker_dropped;
                stats.playout.device_silence_frames = device_silence_frames;
            }
        }
    }
    // A failed/closed receive association must not continue forwarding speech
    // while its owner observes the completed task and releases the devices.
    audio_controls.set_microphone_muted(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_controls_share_mute_and_clamped_volume_state() {
        let controls = AudioControls::default();
        let session_handle = controls.clone();
        assert!(!controls.microphone_muted());
        assert_eq!(controls.speaker_volume_percent(), 100);

        session_handle.set_microphone_muted(true);
        session_handle.set_speaker_volume_percent(130);
        assert!(controls.microphone_muted());
        assert_eq!(controls.speaker_volume_percent(), 100);

        controls.set_microphone_muted(false);
        controls.set_speaker_volume_percent(0);
        assert!(!session_handle.microphone_muted());
        assert_eq!(session_handle.speaker_volume_percent(), 0);
    }

    #[test]
    fn microphone_mute_replaces_live_pcm_with_exact_silence_payload() {
        let live_samples = Some(vec![12_000, -12_000]);
        let muted = Encoder::new(Codec::Pcmu).unwrap().encode(live_samples,true).unwrap();
        assert_eq!(muted, rtp::silence_payload());

        let live = Encoder::new(Codec::Pcmu).unwrap().encode(Some(vec![12_000, -12_000]),false).unwrap();
        assert_eq!(live.len(), rtp::SAMPLES_PER_PACKET);
        assert_eq!(live[0], rtp::linear_to_ulaw(12_000));
        assert_eq!(live[1], rtp::linear_to_ulaw(-12_000));
        assert!(live[2..].iter().all(|sample| *sample == 0xFF));
    }

    #[test]
    fn speaker_volume_scales_signed_pcm_without_overflow() {
        let samples = [i16::MAX, i16::MIN, 2_000, -2_000];
        assert_eq!(
            scale_speaker_volume(&samples, 50),
            [16_383, -16_384, 1_000, -1_000]
        );
        assert_eq!(scale_speaker_volume(&samples, 0), [0, 0, 0, 0]);
        assert_eq!(scale_speaker_volume(&samples, 130), samples);
    }

    fn test_material() -> srtp::SrtpKeyingMaterial {
        let mut key = [0u8; 16];
        let mut salt = [0u8; 14];
        for i in 0..16 {
            key[i] = (i + 1) as u8;
        }
        for i in 0..14 {
            salt[i] = (i + 17) as u8;
        }
        srtp::SrtpKeyingMaterial {
            master_key: key,
            master_salt: salt,
            tag: 2,
        }
    }

    #[tokio::test]
    async fn test_media_session_binds() {
        // Keep all synthetic media on loopback, including the send loop.
        let sink = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remote = sink.local_addr().unwrap();
        let mat = test_material();

        let mut session = MediaSession::start(0, remote, &mat, &mat).await.unwrap();
        let controls = session.audio_controls();
        controls.set_microphone_muted(true);
        controls.set_speaker_volume_percent(35);
        assert!(session.audio_controls().microphone_muted());
        assert_eq!(session.audio_controls().speaker_volume_percent(), 35);
        let port = session.local_port().unwrap();
        assert!(port > 0);

        // Let it run briefly
        tokio::time::sleep(Duration::from_millis(50)).await;

        let stats = session.stats().await;
        // Should have sent at least 1 packet in 50ms (20ms interval)
        assert!(stats.packets_sent >= 1, "sent: {}", stats.packets_sent);

        session.stop().await;
    }

    #[tokio::test]
    async fn stop_releases_udp_port_while_session_remains_alive() {
        let remote: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let material = test_material();
        let mut session = MediaSession::start(0, remote, &material, &material)
            .await
            .unwrap();
        let port = session.local_port().unwrap();

        session.stop().await;
        session.stop().await;

        assert!(session.local_port().is_err());
        let rebound = UdpSocket::bind(format!("0.0.0.0:{port}")).await.unwrap();
        drop(rebound);
    }

    #[tokio::test]
    async fn video_stop_releases_udp_port_while_session_remains_alive() {
        let remote: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let material = test_material();
        let mut session = VideoMediaSession::start(0, remote, &material, &material)
            .await
            .unwrap();
        let port = session.local_port().unwrap();

        session.stop().await;
        session.stop().await;

        assert!(session.local_port().is_err());
        let rebound = UdpSocket::bind(format!("0.0.0.0:{port}")).await.unwrap();
        drop(rebound);
    }

    #[tokio::test]
    async fn native_receive_authenticates_before_reordering_and_applies_volume_at_playout() {
        let receiving = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = receiving.local_addr().unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let other = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let material = test_material();
        let mut sender = srtp::create_context(&material, &material).unwrap();
        let receiver = Arc::new(Mutex::new(
            srtp::create_context(&material, &material).unwrap(),
        ));
        let stats = Arc::new(Mutex::new(MediaStats::default()));
        let controls = AudioControls::default();
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let agent = ice::IceAgent::new(
            fixture_credentials("left"),
            fixture_credentials("right"),
            true,
        );
        let task = tokio::spawn(recv_loop_with_stun(
            Arc::new(Datagram::Direct(receiving)),
            receiver,
            stats.clone(),
            agent,
            Some(tx),
            controls.clone(),
            CodecSet::single(Codec::Pcmu),
            peer.local_addr().unwrap(),
            None,
            None,
            None,
        ));
        let packets = [3000, 6000, 9000]
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let rtp = rtp::encode(
                    0,
                    index as u16,
                    index as u32 * 160,
                    42,
                    &vec![rtp::linear_to_ulaw(value); 160],
                );
                // Real conference RTP can carry header extensions and padding.
                // Only the authenticated 160 audio bytes reach the decoder.
                let mut extended = rtp[..rtp::RTP_HEADER_SIZE].to_vec();
                extended[0] |= 0x30;
                extended.extend_from_slice(&[0xbe, 0xde, 0, 1, 0x10, 0, 0, 0]);
                extended.extend_from_slice(&rtp[rtp::RTP_HEADER_SIZE..]);
                extended.extend_from_slice(&[0, 0, 0, 4]);
                srtp::protect(&mut sender, &extended).unwrap()
            })
            .collect::<Vec<_>>();
        // A valid cipher from an unselected route must not consume replay state.
        other.send_to(&packets[0], local).await.unwrap();
        let mut invalid = packets[0].clone();
        *invalid.last_mut().unwrap() ^= 1;
        peer.send_to(&invalid, local).await.unwrap();
        for index in [2, 0, 1] {
            peer.send_to(&packets[index], local).await.unwrap();
        }
        controls.set_speaker_volume_percent(50);
        let mut heard = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while heard.len() < 3 {
                if let Ok(frame) = rx.try_recv() {
                    if frame.iter().any(|sample| *sample != 0) { heard.push(frame); }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        for (frame, value) in heard.into_iter().zip([3000, 6000, 9000]) {
            assert_eq!(
                frame,
                vec![rtp::ulaw_to_linear(rtp::linear_to_ulaw(value)) / 2; 160]
            );
        }
        let stats = stats.lock().await;
        assert_eq!(stats.packets_received, 3);
        assert_eq!(stats.receive_auth_failures, 1);
        assert_eq!(stats.receive_header_failures, 0);
        assert!(stats.rtp_quality.reordered_packets >= 1);
        assert_eq!(stats.playout.concealed_samples, 0);
        drop(stats);
        task.abort();
        let _ = task.await;
    }
    #[tokio::test]
    async fn receive_device_clock_continues_through_prefill_dtx_and_resumed_speech() {
        let receiving = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = receiving.local_addr().unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let material = test_material();
        let mut sender = srtp::create_context(&material, &material).unwrap();
        let stats = Arc::new(Mutex::new(MediaStats::default()));
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(recv_loop_with_stun(
            Arc::new(Datagram::Direct(receiving)),
            Arc::new(Mutex::new(srtp::create_context(&material, &material).unwrap())),
            stats.clone(), ice::IceAgent::new(fixture_credentials("left"), fixture_credentials("right"), true),
            Some(tx), AudioControls::default(), CodecSet::single(Codec::Pcmu),
            peer.local_addr().unwrap(), None, None, Some(stopped),
        ));
        async fn next(rx: &std::sync::mpsc::Receiver<Vec<i16>>) -> Vec<i16> {
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if let Ok(frame) = rx.try_recv() { return frame; }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }).await.expect("device clock stopped during silence")
        }
        // No RTP at all: a connected native device still needs a clock.
        for _ in 0..5 { assert_eq!(next(&rx).await, vec![0; 160]); }
        for (sequence, timestamp, value) in [(0, 0, 9000), (1, 6400, -9000)] {
            let packet = rtp::encode(0, sequence, timestamp, 42, &vec![rtp::linear_to_ulaw(value); 160]);
            peer.send_to(&srtp::protect(&mut sender, &packet).unwrap(), local).await.unwrap();
            let expected = rtp::ulaw_to_linear(rtp::linear_to_ulaw(value));
            let mut heard = false;
            for _ in 0..15 {
                if next(&rx).await == vec![expected; 160] { heard = true; break; }
            }
            assert!(heard, "resumed authenticated speech was lost");
            // Pass the bounded PLC window and keep delivering clocked zeros.
            for index in 0..15 {
                let frame = next(&rx).await;
                if index > 4 { assert_eq!(frame, vec![0; 160]); }
            }
        }
        let snapshot = stats.lock().await;
        assert_eq!(snapshot.packets_received, 2);
        assert!(snapshot.playout.device_silence_frames >= 20);
        assert_eq!(snapshot.playout.speaker_dropped_frames, 0);
        assert_eq!(snapshot.playout.decoder_errors, 0);
        assert!(snapshot.playout.concealed_samples <= 2 * 3 * 160);
        drop(snapshot);
        stop.send(()).unwrap();
        task.await.unwrap();
        let _ = rx.try_iter().count();
        assert!(matches!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)));
    }

    #[tokio::test]
    async fn authenticated_control_rejects_forgery_replay_and_other_routes_before_source_exit() {
        let receiving = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = receiving.local_addr().unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let other = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let material = test_material();
        let context = srtp::create_context(&material, &material).unwrap();
        let control = Arc::new(
            super::super::control_session::ControlSession::new(&context, 7, 8000, 64000).unwrap(),
        );
        let stats = Arc::new(Mutex::new(MediaStats::default()));
        let (tx, _rx) = std::sync::mpsc::sync_channel(16);
        let agent = ice::IceAgent::new(
            fixture_credentials("left"),
            fixture_credentials("right"),
            true,
        );
        let task = tokio::spawn(recv_loop_with_stun(
            Arc::new(Datagram::Direct(receiving)),
            Arc::new(Mutex::new(context)),
            stats.clone(),
            agent,
            Some(tx),
            AudioControls::default(),
            CodecSet::single(Codec::Pcmu),
            peer.local_addr().unwrap(),
            Some(control.clone()),
            None,
            None,
        ));
        let sender_context = srtp::create_context(&material, &material).unwrap();
        let mut cipher = super::super::media_protection::ControlCipher::new(&sender_context, 42);
        let mut reports = super::super::audio_control::Reports::new(
            42,
            8000,
            64000,
            "fixture@native.invalid".into(),
        )
        .unwrap();
        let first = reports.plan(Duration::ZERO, 1, false);
        let encrypted = cipher.protect(&first.bytes).unwrap();
        let mut invalid = encrypted.clone();
        *invalid.last_mut().unwrap() ^= 1;
        other.send_to(&encrypted, local).await.unwrap();
        peer.send_to(&invalid, local).await.unwrap();
        peer.send_to(&encrypted, local).await.unwrap();
        peer.send_to(&encrypted, local).await.unwrap();
        // The Microsoft standalone forms and profile extension must traverse
        // the real authenticated UDP receive loop, not only the pure parser.
        for hex in [
            "80c900010000002a",
            "80c800060000002a0000000000000000000000000000000000000000",
            "81ca00030000002a0105746573743000",
            "81cb00010000002a",
            "80c900040000002a0008000c000000000001f400",
        ] {
            let plain: Vec<u8> = (0..hex.len()).step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i+2], 16).unwrap()).collect();
            peer.send_to(&cipher.protect(&plain).unwrap(), local).await.unwrap();
        }
        let mut malformed = vec![0x81, 201, 0, 1];
        malformed.extend(42u32.to_be_bytes());
        peer.send_to(&cipher.protect(&malformed).unwrap(), local).await.unwrap();
        let bye = reports.plan(Duration::ZERO, 2, true);
        peer.send_to(&cipher.protect(&bye.bytes).unwrap(), local)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.stats().await.reports_received < 7 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let seen = control.stats().await;
        assert_eq!(seen.reports_received, 7);
        assert_eq!(seen.rejected_packets, 3);
        let snapshot = stats.lock().await;
        assert_eq!(snapshot.control_unprotect_failures, 2);
        assert_eq!(snapshot.control_parse_failures, 1);
        drop(snapshot);
        assert!(seen.round_trip_ms.is_none());
        assert!(!task.is_finished());
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn bounded_speaker_backpressure_never_blocks_network_or_playout() {
        let receiving = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = receiving.local_addr().unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let material = test_material();
        let mut sender = srtp::create_context(&material, &material).unwrap();
        let stats = Arc::new(Mutex::new(MediaStats::default()));
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let agent = ice::IceAgent::new(
            fixture_credentials("left"),
            fixture_credentials("right"),
            true,
        );
        let task = tokio::spawn(recv_loop_with_stun(
            Arc::new(Datagram::Direct(receiving)),
            Arc::new(Mutex::new(
                srtp::create_context(&material, &material).unwrap(),
            )),
            stats.clone(),
            agent,
            Some(tx),
            AudioControls::default(),
            CodecSet::single(Codec::Pcmu),
            peer.local_addr().unwrap(),
            None,
            None,
            None,
        ));
        for index in 0..6 {
            let bytes = rtp::encode(0, index, index as u32 * 160, 42, &vec![0xff; 160]);
            peer.send_to(&srtp::protect(&mut sender, &bytes).unwrap(), local)
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let s = stats.lock().await;
                if s.playout.speaker_dropped_frames >= 2 {
                    assert_eq!(s.packets_received, 6);
                    break;
                }
                drop(s);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(!task.is_finished());
        task.abort();
        let _ = task.await;
    }
    #[tokio::test]
    async fn muted_start_discards_preconnection_pcm_once_then_preserves_new_speech() {
        let socket = Arc::new(Datagram::Direct(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let material = test_material();
        let context = Arc::new(Mutex::new(srtp::create_context(&material, &material).unwrap()));
        let stats = Arc::new(Mutex::new(MediaStats::default()));
        let controls = AudioControls::default();
        controls.set_microphone_muted(true);
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        for _ in 0..8 { tx.send(vec![9000;160]).unwrap(); }
        let task = tokio::spawn(send_loop(socket, peer.local_addr().unwrap(), context, 42,
            stats.clone(), Some(rx), controls.clone(), Encoder::new(Codec::Pcmu).unwrap(), None));
        let mut receiver = srtp::create_context(&material, &material).unwrap();
        let mut buffer = [0;2048];
        let (len, _) = tokio::time::timeout(Duration::from_secs(1),peer.recv_from(&mut buffer)).await.unwrap().unwrap();
        let first = rtp::decode(&srtp::unprotect(&mut receiver,&buffer[..len]).unwrap()).unwrap();
        assert_eq!(first.payload,vec![rtp::linear_to_ulaw(0);160]);
        assert_eq!(stats.lock().await.capture_startup_discarded,8);
        for value in [1000,5000,7000] {tx.send(vec![value;160]).unwrap();}
        controls.set_microphone_muted(false);
        let mut heard=Vec::new();
        tokio::time::timeout(Duration::from_secs(1),async {
            while heard.len()<3 {
                let (len,_)=peer.recv_from(&mut buffer).await.unwrap();
                let packet=rtp::decode(&srtp::unprotect(&mut receiver,&buffer[..len]).unwrap()).unwrap();
                if packet.payload.iter().any(|&b|b!=rtp::linear_to_ulaw(0)) {heard.push(packet.payload);}
            }
        }).await.unwrap();
        for (payload,value) in heard.iter().zip([1000,5000,7000]) {assert_eq!(*payload,vec![rtp::linear_to_ulaw(value);160]);}
        assert_eq!(stats.lock().await.capture_startup_discarded,8);
        task.abort(); let _=task.await;
    }

    #[tokio::test]
    async fn sender_preserves_capture_order_and_rtp_time_after_a_scheduler_stall() {
        let socket = Arc::new(Datagram::Direct(
            UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        ));
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let material = test_material();
        let context = Arc::new(Mutex::new(
            srtp::create_context(&material, &material).unwrap(),
        ));
        let guard = context.lock().await;
        let (tx, rx) = std::sync::mpsc::sync_channel(3);
        for value in [1000, 5000, 9000] {
            tx.send(vec![value; 160]).unwrap();
        }
        let task = tokio::spawn(send_loop(
            socket,
            peer.local_addr().unwrap(),
            context.clone(),
            42,
            Arc::new(Mutex::new(MediaStats::default())),
            Some(rx),
            AudioControls::default(),
            Encoder::new(Codec::Pcmu).unwrap(),
            None,
        ));
        tokio::time::sleep(Duration::from_millis(85)).await;
        drop(guard);
        let mut decode = srtp::create_context(&material, &material).unwrap();
        let mut packets = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut buffer = [0; 2048];
            while packets.len() < 3 {
                let (len, _) = peer.recv_from(&mut buffer).await.unwrap();
                packets.push(
                    rtp::decode(&srtp::unprotect(&mut decode, &buffer[..len]).unwrap()).unwrap(),
                );
            }
        })
        .await
        .unwrap();
        for (packet, value) in packets.iter().zip([1000, 5000, 9000]) {
            assert_eq!(packet.payload, vec![rtp::linear_to_ulaw(value); 160]);
        }
        assert!(packets[2].timestamp.wrapping_sub(packets[0].timestamp) >= 640);
        assert!(packets[1].timestamp.wrapping_sub(packets[0].timestamp) >= 160);
        assert!(packets[2].timestamp.wrapping_sub(packets[1].timestamp) >= 160);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn sender_uses_distinct_contiguous_timestamps_at_packet_deadlines() {
        let socket = Arc::new(Datagram::Direct(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let material = test_material();
        let context = Arc::new(Mutex::new(srtp::create_context(&material, &material).unwrap()));
        let task = tokio::spawn(send_loop(socket, peer.local_addr().unwrap(), context,
            42, Arc::new(Mutex::new(MediaStats::default())), None,
            AudioControls::default(), Encoder::new(Codec::Pcmu).unwrap(), None));
        let mut decode = srtp::create_context(&material, &material).unwrap();
        let packets = tokio::time::timeout(Duration::from_secs(4), async {
            let mut packets = Vec::new();
            let mut buffer = [0; 2048];
            for _ in 0..75 {
                let (len, _) = peer.recv_from(&mut buffer).await.unwrap();
                packets.push(rtp::decode(&srtp::unprotect(&mut decode, &buffer[..len]).unwrap()).unwrap());
            }
            packets
        }).await.unwrap();
        task.abort();
        let _ = task.await;
        for pair in packets.windows(2) {
            assert_eq!(pair[1].sequence_number.wrapping_sub(pair[0].sequence_number), 1);
            let step = pair[1].timestamp.wrapping_sub(pair[0].timestamp);
            assert!(step >= 160 && step % 160 == 0, "invalid timestamp step {step}");
        }
    }

    fn fixture_credentials(name: &str) -> ice::IceCredentials {
        ice::IceCredentials {
            ufrag: name.into(),
            pwd: format!("fixture-password-{name}-123456"),
        }
    }

    fn fixture_candidate(address: SocketAddr) -> ice::IceCandidate {
        ice::IceCandidate {
            foundation: "fixture".into(),
            component: 1,
            transport: ice::Transport::Udp,
            priority: 100,
            address: address.ip().to_string(),
            port: address.port(),
            candidate_type: ice::CandidateType::Host,
            raddr: None,
            rport: None,
        }
    }

    #[cfg(not(feature = "audio"))]
    #[tokio::test]
    async fn ice_uses_the_reserved_sdp_socket_and_authenticates_checks_after_media_starts() {
        let reserved = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = reserved.local_addr().unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local_creds = fixture_credentials("local");
        let remote_creds = fixture_credentials("remote");
        let offer = super::super::sdp::generate_sdp_offer(
            "127.0.0.1",
            local.port(),
            &local_creds.ufrag,
            &local_creds.pwd,
            &ice::gather_host_candidates(local),
        );
        assert!(offer.sdp.contains(&format!("m=audio {} ", local.port())));
        assert!(
            UdpSocket::bind(local).await.is_err(),
            "SDP generation relinquished its advertised port"
        );
        let local_key = srtp::parse_crypto_line(&offer.crypto_line).unwrap();
        let remote_key = test_material();
        let candidates = [fixture_candidate(peer.local_addr().unwrap())];
        let (session, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(
                MediaSession::start_with_ice_on_socket(
                    reserved,
                    &candidates,
                    &local_creds,
                    &remote_creds,
                    &local_key,
                    &remote_key,
                    true
                ),
                async {
                    let mut buffer = [0u8; 512];
                    let (length, source) = peer.recv_from(&mut buffer).await.unwrap();
                    assert!(ice::verify_message_integrity(
                        &buffer[..length],
                        remote_creds.pwd.as_bytes()
                    ));
                    let response = ice::build_binding_response(
                        &ice::get_transaction_id(&buffer[..length]).unwrap(),
                        source,
                        Some(remote_creds.pwd.as_bytes()),
                    );
                    peer.send_to(&response, source).await.unwrap();
                }
            )
        })
        .await
        .unwrap();
        let mut session = session.unwrap();
        assert_eq!(session.local_port().unwrap(), local.port());
        let invalid = ice::build_ice_binding_request(
            &[0x20; 12],
            "local:remote",
            b"wrong-key",
            100,
            true,
            42,
        );
        let valid = ice::build_ice_binding_request(
            &[0x21; 12],
            "local:remote",
            local_creds.pwd.as_bytes(),
            100,
            true,
            42,
        );
        peer.send_to(&invalid, local).await.unwrap();
        peer.send_to(&valid, local).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut buffer = [0u8; 2048];
            loop {
                let (length, _) = peer.recv_from(&mut buffer).await.unwrap();
                let data = &buffer[..length];
                if !ice::is_stun_response(data) {
                    continue;
                }
                assert_ne!(
                    ice::get_transaction_id(data),
                    Some([0x20; 12]),
                    "active media answered an unauthenticated check"
                );
                if ice::get_transaction_id(data) == Some([0x21; 12]) {
                    assert!(ice::verify_message_integrity(
                        data,
                        local_creds.pwd.as_bytes()
                    ));
                    break;
                }
            }
        })
        .await
        .unwrap();
        session.stop().await;
        let _rebound = UdpSocket::bind(local).await.unwrap();
    }

    #[cfg(not(feature = "audio"))]
    #[tokio::test]
    async fn failed_ice_never_falls_back_to_unverified_rtp_and_releases_the_reserved_socket() {
        let reserved = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = reserved.local_addr().unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let candidates = [fixture_candidate(peer.local_addr().unwrap())];
        let local_creds = fixture_credentials("local");
        let remote_creds = fixture_credentials("remote");
        let material = test_material();
        let start = MediaSession::start_with_ice_on_socket(
            reserved,
            &candidates,
            &local_creds,
            &remote_creds,
            &material,
            &material,
            true,
        );
        tokio::pin!(start);
        let mut attempts = 0;
        let result=tokio::time::timeout(Duration::from_secs(5),async{
            let mut buffer=[0u8;2048];loop{
                tokio::select!{
                    result=&mut start=>break result,
                    received=peer.recv_from(&mut buffer)=>{
                        let (length,source)=received.unwrap();let data=&buffer[..length];
                        assert!(ice::is_stun_request(data),"media was sent without an authenticated route");
                        attempts+=1;
                        let forged=ice::build_binding_response(&ice::get_transaction_id(data).unwrap(),
                            source,Some(b"wrong-key"));
                        peer.send_to(&forged,source).await.unwrap();
                    }
                }
            }
        }).await.unwrap();
        assert!(result.is_err());
        assert!(attempts >= 1);
        let _rebound = UdpSocket::bind(local).await.unwrap();
    }

    #[tokio::test]
    async fn media_stats_report_recovered_rtp_gaps_from_loopback_packets() {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remote = receiver.local_addr().unwrap();
        let local_material = test_material();
        let mut remote_key = test_material();
        remote_key.master_key[0] ^= 0x5a;
        let mut session = MediaSession::start(0, remote, &local_material, &remote_key)
            .await
            .unwrap();
        let destination: SocketAddr = format!("127.0.0.1:{}", session.local_port().unwrap())
            .parse()
            .unwrap();
        let mut sender = srtp::create_context(&remote_key, &local_material).unwrap();
        let payload = rtp::silence_payload();

        for (sequence, timestamp) in [(10, 0), (13, 480)] {
            let packet = rtp::encode(rtp::PT_PCMU, sequence, timestamp, 0x44556677, &payload);
            let protected = srtp::protect(&mut sender, &packet).unwrap();
            receiver.send_to(&protected, destination).await.unwrap();
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if session.stats().await.packets_received >= 2 {
                    break;
                }
                time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(session.stats().await.rtp_quality.estimated_lost_packets, 2);

        for (sequence, timestamp) in [(11, 160), (12, 320)] {
            let packet = rtp::encode(rtp::PT_PCMU, sequence, timestamp, 0x44556677, &payload);
            let protected = srtp::protect(&mut sender, &packet).unwrap();
            receiver.send_to(&protected, destination).await.unwrap();
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if session.stats().await.packets_received >= 4 {
                    break;
                }
                time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let quality = session.stats().await.rtp_quality;
        assert_eq!(quality.estimated_lost_packets, 0);
        assert_eq!(quality.reordered_packets, 2);

        session.stop().await;
    }
}
