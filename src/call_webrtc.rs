//! Application-owned device bridge for the optional libwebrtc media engine.
//! Signaling, session identity, and UI remain in the existing call coordinator.
use crate::call_video::Control;
use iced::futures::StreamExt;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use teams_av::{
    engine::Engine,
    pcm::{MicrophoneGate, PcmBridge},
    rtc::{
        prelude::{
            I420Buffer, MediaStreamTrack, PeerConnectionState, VideoBuffer, VideoFormatType,
            VideoFrame, VideoRotation,
        },
        video_source::native::NativeVideoSource,
        video_stream::native::NativeVideoStream,
    },
};
use tokio::{sync::watch, task::JoinHandle};

pub(crate) struct Session {
    engine: Engine,
    gate: MicrophoneGate,
    remote_camera_slot: Option<(String, u32)>,
    screen_slot: Option<(String, u32)>,
    control: Control,
    camera_task: Option<JoinHandle<()>>,
    camera_stats_task: Option<JoinHandle<()>>,
    stop: watch::Sender<bool>,
    volume: watch::Sender<u8>,
    manager: JoinHandle<()>,
    healthy: Arc<AtomicBool>,
}

impl Session {
    pub async fn connect(
        mut engine: Engine,
        answer: &str,
        io: ost::calling::media::PcmAudioIo,
        control: Control,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<Self, ()> {
        if *cancel.borrow() || cancel.has_changed().is_err() {
            return Err(());
        }
        let plan = engine.receive_plan(answer).map_err(|_| ())?;
        let remote_camera_slot = plan.cameras.into_iter().next();
        let screen_slot = plan.screen;
        let screen_mid = engine.screen_mid().to_owned();
        tokio::select! { biased;
            _ = cancel.changed() => return Err(()),
            result = engine.apply_answer(answer) => result.map_err(|_| ())?,
        }
        let deadline = tokio::time::sleep(Duration::from_secs(25));
        tokio::pin!(deadline);
        loop {
            if engine.candidate_overflowed() {
                return Err(());
            }
            match engine.connection_state() {
                PeerConnectionState::Connected => break,
                PeerConnectionState::Failed | PeerConnectionState::Closed => return Err(()),
                _ => {}
            }
            tokio::select! {
                _ = cancel.changed() => return Err(()),
                _ = &mut deadline => return Err(()),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {},
            }
        }
        let gate = engine.microphone_gate();
        let mut bridge = PcmBridge::start(gate.clone(), io.microphone, io.speaker);
        let mut incoming = engine.take_incoming().ok_or(())?;
        let mut candidates = engine.take_candidates();
        let (volume, mut volume_rx) = watch::channel(100u8);
        let (stop, mut stopped) = watch::channel(false);
        let healthy = Arc::new(AtomicBool::new(true));
        let running = healthy.clone();
        let output = control.clone();
        let manager = tokio::spawn(async move {
            let mut videos: Vec<JoinHandle<()>> = Vec::new();
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    _ = volume_rx.changed() => bridge.set_volume(*volume_rx.borrow_and_update()),
                    candidate = async { match &mut candidates { Some(source) => source.recv().await, None => std::future::pending().await } } => if candidate.is_none() { break; },
                    next = incoming.recv() => match next {
                        Some(incoming) => {
                            let presentation = incoming.mid.as_deref() == Some(screen_mid.as_str());
                            match incoming.track {
                            MediaStreamTrack::Audio(track) => {
                                if bridge.add_remote_track(track).is_err() { break; }
                            }
                            MediaStreamTrack::Video(track) => {
                                videos.retain(|job| !job.is_finished());
                                if videos.len() >= 12 { break; }
                                let output = output.clone();
                                videos.push(tokio::spawn(async move {
                                    let mut stream = NativeVideoStream::new(track);
                                    while let Some(frame) = stream.next().await {
                                        if let Some(handle) = video_handle(frame) {
                                            if presentation {
                                                output.publish_screen_frame(handle);
                                            } else {
                                                output.publish_received_frame(handle);
                                            }
                                        }
                                    }
                                }));
                            }
                            }
                        }
                        None => break,
                    },
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if bridge.failed() { break; }
                    }
                }
            }
            for video in &videos {
                video.abort();
            }
            for video in videos {
                let _ = video.await;
            }
            bridge.stop().await;
            running.store(false, Ordering::Release);
        });
        Ok(Self {
            engine,
            gate,
            remote_camera_slot,
            screen_slot,
            control,
            camera_task: None,
            camera_stats_task: None,
            stop,
            volume,
            manager,
            healthy,
        })
    }

    pub fn microphone_gate(&self) -> MicrophoneGate {
        self.gate.clone()
    }
    pub fn camera_mid(&self) -> Option<String> {
        self.engine.camera_mid()
    }
    pub fn camera_publishing(&self) -> bool {
        self.camera_task.is_some()
    }
    pub fn negotiator(&self) -> teams_av::engine::Negotiator {
        self.engine.negotiator()
    }
    pub fn remote_camera_slot(&self) -> Option<(String, u32)> {
        self.remote_camera_slot.clone()
    }
    pub fn screen_slot(&self) -> Option<(String, u32)> {
        self.screen_slot.clone()
    }
    pub fn handle_camera_control(&mut self, body: &serde_json::Value) -> Result<(), ()> {
        self.engine.handle_camera_control(body).map_err(|_| ())
    }
    pub fn set_volume(&self, percent: u8) {
        self.volume.send_replace(percent.min(100));
    }
    pub fn running(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
            && !matches!(
                self.engine.connection_state(),
                PeerConnectionState::Failed | PeerConnectionState::Closed
            )
    }
    pub async fn trace_health(&self) {
        if let Ok(Ok(stats)) =
            tokio::time::timeout(Duration::from_millis(250), self.engine.audio_metrics()).await
        {
            crate::live_trace::record(
                "call_webrtc_audio_rx_v1",
                stats.concealed_samples,
                stats.packets.min(usize::MAX as u64) as usize,
                stats.lost.min(usize::MAX as u64) as usize,
            );
            crate::live_trace::record("call_webrtc_audio_jitter_us_v1", stats.jitter_micros, 0, 0);
        }
    }
    pub fn start_camera(&mut self) -> Result<(), ()> {
        if self.camera_task.is_some() || !self.control.enabled() || !self.control.has_device() {
            return Err(());
        }
        let source = self.engine.start_camera(640, 480).map_err(|_| ())?;
        self.control.network_enabled(true);
        let control = self.control.clone();
        self.camera_task = Some(tokio::spawn(async move {
            send_frames(control, source).await;
        }));
        let control = self.control.clone();
        let sender = self.engine.camera_sender();
        self.camera_stats_task = Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut previous = None;
            loop {
                tick.tick().await;
                let Ok(stats) = sender.get_stats().await else {
                    control.network_sending(false);
                    continue;
                };
                let packets = stats
                    .into_iter()
                    .filter_map(|stat| match stat {
                        teams_av::rtc::stats::RtcStats::OutboundRtp(stream) => {
                            Some(stream.sent.packets_sent)
                        }
                        _ => None,
                    })
                    .sum::<u64>();
                control.network_sending(previous.is_some_and(|old| packets > old));
                previous = Some(packets);
            }
        }));
        Ok(())
    }
    pub fn share_mid(&self) -> Option<String> {
        self.engine.share_mid()
    }
    pub fn sharing(&self) -> bool {
        self.engine.sharing()
    }
    pub fn start_share(&mut self, width: u32, height: u32) -> Result<(), ()> {
        self.engine.start_share(width, height).map_err(|_| ())
    }
    pub fn push_share(&self, buffer: I420Buffer) -> bool {
        self.engine.push_share(buffer)
    }
    pub fn stop_share(&mut self) {
        let _ = self.engine.stop_share();
    }
    pub async fn stop_camera(&mut self) {
        self.control.network_enabled(false);
        let _ = self.engine.stop_camera();
        if let Some(task) = self.camera_task.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.camera_stats_task.take() {
            task.abort();
            let _ = task.await;
        }
    }
    pub async fn stop(mut self) {
        self.stop_camera().await;
        self.stop_share();
        self.stop.send_replace(true);
        let _ = (&mut self.manager).await;
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.control.network_enabled(false);
        if let Some(task) = self.camera_task.take() {
            task.abort();
        }
        if let Some(task) = self.camera_stats_task.take() {
            task.abort();
        }
        self.stop.send_replace(true);
        self.manager.abort();
    }
}

async fn send_frames(control: Control, source: NativeVideoSource) {
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if let Some(iced::widget::image::Handle::Rgba {
            width,
            height,
            pixels,
            ..
        }) = control.take_network_frame()
        {
            if let Some(buffer) = rgba_to_i420(width, height, pixels.as_ref()) {
                source.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, buffer));
            }
        }
    }
}

pub(crate) struct ScreenPump {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    frames: std::sync::mpsc::Receiver<ScreenFrame>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub(crate) struct ScreenFrame {
    pub width: u32,
    pub height: u32,
    pub buffer: I420Buffer,
}

impl ScreenPump {
    pub(crate) fn start() -> Option<Self> {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (sender, frames) = std::sync::mpsc::sync_channel(1);
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("screen-share".into())
            .spawn(move || portal_capture(flag, sender))
            .ok()?;
        Some(Self { stop, frames, thread: Some(thread) })
    }

    pub(crate) fn latest(&self) -> Option<ScreenFrame> {
        let mut frame = None;
        while let Ok(next) = self.frames.try_recv() {
            frame = Some(next);
        }
        frame
    }

    pub(crate) fn finished(&self) -> bool {
        self.thread.as_ref().is_some_and(|thread| thread.is_finished())
    }
}

impl Drop for ScreenPump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// spa_data_type and spa_video_format values from the system PipeWire headers.
const SPA_DATA_MEM_PTR: u32 = 1;
const SPA_DATA_MEM_FD: u32 = 2;
const SPA_VIDEO_FORMAT_BGRX: u32 = 8;
const SPA_VIDEO_FORMAT_BGRA: u32 = 12;

/// Shared-memory BGRA/BGRx only. A DMA-BUF or an unmapped pointer is not a frame.
fn mapped_bgra<'a>(
    data_type: u32,
    format: u32,
    data: *const u8,
    size: usize,
    stride: u32,
    width: u32,
    height: u32,
) -> Option<&'a [u8]> {
    if data_type != SPA_DATA_MEM_PTR && data_type != SPA_DATA_MEM_FD {
        return None;
    }
    if format != SPA_VIDEO_FORMAT_BGRA && format != SPA_VIDEO_FORMAT_BGRX {
        return None;
    }
    if data.is_null() || width < 2 || height < 2 || width > 7680 || height > 7680 {
        return None;
    }
    let row = stride as usize;
    let need = row.checked_mul(height as usize)?;
    if row < (width as usize).saturating_mul(4) || size < need {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(data, need) })
}

fn portal_capture(
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    sender: std::sync::mpsc::SyncSender<ScreenFrame>,
) {
    #[cfg(target_os = "linux")]
    if linux_portal_capture(stop, sender) {
        crate::live_trace::record("call_screen_capture", 0, 0, 1);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (stop, sender);
    }
}

#[cfg(target_os = "linux")]
fn linux_portal_capture(
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    sender: std::sync::mpsc::SyncSender<ScreenFrame>,
) -> bool {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return true;
    };
    runtime.block_on(async move {
        use ashpd::desktop::screencast::Screencast;
        let Ok(proxy) = Screencast::new().await else { return true };
        let Ok(session) = proxy.create_session().await else { return true };
        let failed = cast_selected_source(&proxy, &session, &stop, sender).await;
        let _ = session.close().await;
        failed
    })
}

#[cfg(target_os = "linux")]
async fn cast_selected_source(
    proxy: &ashpd::desktop::screencast::Screencast<'_>,
    session: &ashpd::desktop::Session<'_, ashpd::desktop::screencast::Screencast<'_>>,
    stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    sender: std::sync::mpsc::SyncSender<ScreenFrame>,
) -> bool {
    use ashpd::desktop::screencast::{CursorMode, SourceType};
    use ashpd::desktop::PersistMode;
    use std::os::fd::IntoRawFd;
    if stop.load(Ordering::SeqCst) {
        return true;
    }
    let Ok(selected) = proxy
        .select_sources(
            session,
            CursorMode::Embedded,
            SourceType::Monitor | SourceType::Window,
            false,
            None,
            PersistMode::DoNot,
        )
        .await
    else {
        return true;
    };
    if selected.response().is_err() || stop.load(Ordering::SeqCst) {
        return true;
    }
    let started = tokio::select! {
        result = proxy.start(session, None) => result,
        _ = portal_stopped(stop) => return true,
    };
    let Ok(started) = started else { return true };
    let Ok(streams) = started.response() else { return true };
    let Some(stream) = streams.streams().first() else { return true };
    let node = stream.pipe_wire_node_id();
    if stop.load(Ordering::SeqCst) {
        return true;
    }
    let Ok(remote) = proxy.open_pipe_wire_remote(session).await else { return true };
    let state = std::sync::Arc::new(ShareCapture {
        sender,
        last: std::sync::Mutex::new(std::time::Instant::now() - Duration::from_millis(66)),
    });
    let raw = std::sync::Arc::as_ptr(&state) as usize;
    let held = state;
    let stop_flag = stop.clone();
    let joined = tokio::task::spawn_blocking(move || {
        let fd = remote.into_raw_fd();
        let rc = teams_av::rtc::cpu_screencast::consume(
            fd,
            node,
            &stop_flag,
            on_cpu_frame,
            raw as *mut std::ffi::c_void,
        );
        drop(held);
        rc
    })
    .await;
    !matches!(joined, Ok(0))
}

#[cfg(target_os = "linux")]
async fn portal_stopped(stop: &std::sync::atomic::AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

#[cfg(target_os = "linux")]
struct ShareCapture {
    sender: std::sync::mpsc::SyncSender<ScreenFrame>,
    last: std::sync::Mutex<std::time::Instant>,
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn on_cpu_frame(
    user: *mut std::ffi::c_void,
    frame: *const teams_av::rtc::cpu_screencast::CpuFrame,
) {
    if user.is_null() || frame.is_null() {
        return;
    }
    let state = unsafe { &*(user as *const ShareCapture) };
    let frame = unsafe { &*frame };
    let Some(pixels) = mapped_bgra(
        frame.data_type,
        frame.format,
        frame.data,
        frame.size,
        frame.stride,
        frame.width,
        frame.height,
    ) else {
        return;
    };
    let mut last = state.last.lock().unwrap_or_else(|error| error.into_inner());
    if last.elapsed() < Duration::from_millis(66) {
        return;
    }
    let Some(buffer) = bgra_to_i420(pixels, frame.width, frame.height, frame.stride) else {
        return;
    };
    *last = std::time::Instant::now();
    let (width, height) = (buffer.width(), buffer.height());
    let _ = state.sender.try_send(ScreenFrame { width, height, buffer });
}

fn bgra_to_i420(pixels: &[u8], width: u32, height: u32, stride: u32) -> Option<I420Buffer> {
    let (out_w, out_h) = fit_even(width, height)?;
    if stride < width.saturating_mul(4) || pixels.len() < (stride as usize).checked_mul(height as usize)? {
        return None;
    }
    let mut buffer = I420Buffer::new(out_w, out_h);
    let (sy, su, sv) = buffer.strides();
    let (y_plane, u_plane, v_plane) = buffer.data_mut();
    for row in (0..out_h as usize).step_by(2) {
        for col in (0..out_w as usize).step_by(2) {
            let mut u = 0i32;
            let mut v = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let src_row = (row + dy) * height as usize / out_h as usize;
                    let src_col = (col + dx) * width as usize / out_w as usize;
                    let i = src_row * stride as usize + src_col * 4;
                    let (b, g, r) = (i32::from(pixels[i]), i32::from(pixels[i + 1]), i32::from(pixels[i + 2]));
                    y_plane[(row + dy) * sy as usize + col + dx] =
                        (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
                    u += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                    v += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                }
            }
            u_plane[(row / 2) * su as usize + col / 2] = (u / 4).clamp(0, 255) as u8;
            v_plane[(row / 2) * sv as usize + col / 2] = (v / 4).clamp(0, 255) as u8;
        }
    }
    Some(buffer)
}

fn fit_even(width: u32, height: u32) -> Option<(u32, u32)> {
    if width < 2 || height < 2 || width > 7680 || height > 7680 {
        return None;
    }
    let long = width.max(height);
    let (width, height) = if long <= 1920 {
        (width & !1, height & !1)
    } else {
        let scale = 1920.0 / long as f64;
        ((((width as f64) * scale) as u32).max(2) & !1, (((height as f64) * scale) as u32).max(2) & !1)
    };
    (width >= 2 && height >= 2).then_some((width, height))
}

fn rgba_to_i420(width: u32, height: u32, pixels: &[u8]) -> Option<I420Buffer> {
    if width == 0
        || height == 0
        || width > 4096
        || height > 4096
        || width % 2 != 0
        || height % 2 != 0
        || pixels.len()
            != (width as usize)
                .checked_mul(height as usize)?
                .checked_mul(4)?
    {
        return None;
    }
    let mut buffer = I420Buffer::new(width, height);
    let (sy, su, sv) = buffer.strides();
    let (y_plane, u_plane, v_plane) = buffer.data_mut();
    for row in (0..height as usize).step_by(2) {
        for col in (0..width as usize).step_by(2) {
            let mut u = 0i32;
            let mut v = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = ((row + dy) * width as usize + col + dx) * 4;
                    let (r, g, b) = (
                        i32::from(pixels[i]),
                        i32::from(pixels[i + 1]),
                        i32::from(pixels[i + 2]),
                    );
                    y_plane[(row + dy) * sy as usize + col + dx] =
                        (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
                    u += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                    v += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                }
            }
            u_plane[(row / 2) * su as usize + col / 2] = (u / 4).clamp(0, 255) as u8;
            v_plane[(row / 2) * sv as usize + col / 2] = (v / 4).clamp(0, 255) as u8;
        }
    }
    Some(buffer)
}
fn video_handle(
    frame: teams_av::rtc::prelude::BoxVideoFrame,
) -> Option<iced::widget::image::Handle> {
    let buffer = frame.buffer;
    let (width, height) = (buffer.width(), buffer.height());
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return None;
    }
    let stride = width.checked_mul(4)?;
    let mut pixels = vec![0u8; (stride as usize).checked_mul(height as usize)?];
    buffer.to_argb(
        VideoFormatType::ABGR,
        &mut pixels,
        stride,
        width as i32,
        height as i32,
    );
    Some(iced::widget::image::Handle::from_rgba(
        width, height, pixels,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn camera_conversion_rejects_bad_sizes_and_keeps_color_planes_bounded() {
        assert!(rgba_to_i420(3, 2, &[0; 24]).is_none());
        assert!(rgba_to_i420(2, 2, &[0; 15]).is_none());
        assert_eq!(fit_even(1920, 1080), Some((1920, 1080)));
        assert_eq!(fit_even(3840, 2160), Some((1920, 1080)));
        let bgra = [0u8, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255];
        let pixels = mapped_bgra(SPA_DATA_MEM_FD, SPA_VIDEO_FORMAT_BGRA, bgra.as_ptr(), bgra.len(), 8, 2, 2)
            .expect("memfd BGRA");
        assert_eq!(pixels, &bgra);
        assert!(mapped_bgra(3, SPA_VIDEO_FORMAT_BGRA, bgra.as_ptr(), bgra.len(), 8, 2, 2).is_none());
        assert!(mapped_bgra(SPA_DATA_MEM_PTR, SPA_VIDEO_FORMAT_BGRA, std::ptr::null(), 16, 8, 2, 2).is_none());
        assert!(mapped_bgra(SPA_DATA_MEM_PTR, 11, bgra.as_ptr(), bgra.len(), 8, 2, 2).is_none());
        assert!(mapped_bgra(SPA_DATA_MEM_FD, SPA_VIDEO_FORMAT_BGRX, bgra.as_ptr(), 8, 8, 2, 2).is_none());
        let bgra = [0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255];
        let screen = bgra_to_i420(&bgra, 2, 2, 8).unwrap();
        assert_eq!((screen.width(), screen.height()), (2, 2));
        let rgba = [
            255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
        ];
        let frame = rgba_to_i420(2, 2, &rgba).unwrap();
        let (y, u, v) = frame.data();
        assert!(y[0] > 60 && y[0] < 100);
        assert!(u[0] < 128 && v[0] > 128);
        let buffer: teams_av::rtc::prelude::BoxVideoBuffer = Box::new(frame);
        let result = video_handle(VideoFrame::new(VideoRotation::VideoRotation0, buffer)).unwrap();
        let iced::widget::image::Handle::Rgba { pixels, .. } = result else {
            panic!("RGBA expected")
        };
        assert!(pixels[0] > 200 && pixels[2] < 30 && pixels[3] == 255);
    }
}

#[cfg(all(test, feature = "libwebrtc-test-support"))]
mod integration_tests {
    use super::*;
    use teams_av::{
        rtc::{
            audio_stream::native::NativeAudioStream,
            peer_connection_factory::native::PeerConnectionFactoryExt, prelude::*,
            video_stream::native::NativeVideoStream,
        },
        sdp,
    };
    use tokio::{sync::mpsc, time::timeout};

    #[test]
    fn native_application_bridge_sends_pcm_and_preview_frames_through_webrtc() {
        crate::graph::runtime().unwrap().block_on(async {
            let mut engine = Engine::prepare_with_audio_rate(Vec::new(), 8_000).await.unwrap();
            let mut local_candidates = engine.take_candidates().unwrap();
            let negotiator = engine.negotiator();
            let factory = PeerConnectionFactory::default();
            let remote = factory.create_peer_connection(RtcConfiguration::default()).unwrap();
            let (candidate_tx, mut remote_candidates) = mpsc::unbounded_channel();
            remote.on_ice_candidate(Some(Box::new(move |candidate| { let _ = candidate_tx.send(candidate); })));
            let (track_tx, mut tracks) = mpsc::unbounded_channel();
            remote.on_track(Some(Box::new(move |event| { let _ = track_tx.send(event.track); })));
            let offer = SessionDescription::parse(engine.browser_offer_for_fixture(), SdpType::Offer).unwrap();
            remote.set_remote_description(offer).await.unwrap();
            let answer = remote.create_answer(AnswerOptions::default()).await.unwrap();
            remote.set_local_description(answer.clone()).await.unwrap();
            let mut number = || rand::random::<u32>() % (u32::MAX - 100);
            let answer = sdp::to_teams_offer(&answer.to_string(), &sdp::OfferPlan {
                screen_share_mids: vec!["11".into()], ..Default::default()
            }, &mut number).unwrap();
            let mut answer = sdp::parse(&answer.sdp).unwrap();
            for line in &mut answer.media {
                if matches!(line.direction(), Some("recvonly" | "inactive")) {
                    line.attributes.retain(|attribute| !matches!(attribute.name.as_str(), "x-ssrc-range" | "ssrc" | "ssrc-group"));
                }
            }
            let answer = sdp::write(&answer);
            let other = remote.clone();
            let relay = tokio::spawn(async move {
                loop { tokio::select! {
                    Some(candidate) = local_candidates.recv() => { let _ = other.add_ice_candidate(candidate).await; }
                    Some(candidate) = remote_candidates.recv() => { let _ = negotiator.add_candidate(candidate).await; }
                    else => break,
                }}
            });
            let (mic_tx, mic_rx) = std::sync::mpsc::sync_channel(8);
            let (speaker_tx, _speaker_rx) = std::sync::mpsc::sync_channel(8);
            let io = ost::calling::media::PcmAudioIo { microphone: mic_rx, speaker: speaker_tx, initially_muted: true };
            let control = Control::default();
            let sample = iced::widget::image::Handle::from_rgba(640, 480, vec![180u8; 640*480*4]);
            control.fixture_camera_frame(sample.clone());
            let (_cancel_tx, mut cancel) = watch::channel(false);
            let mut session = Session::connect(engine, &answer, io, control.clone(), &mut cancel).await.unwrap();
            let gate = session.microphone_gate();
            assert!(gate.set_enabled(true));
            session.start_camera().unwrap();
            let source = tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                let mut phase = 0usize;
                loop {
                    tick.tick().await;
                    let samples = (0..160).map(|n| {
                        let t = (phase + n) as f32 / 8_000.0;
                        (7_000.0 * (t * 440.0 * std::f32::consts::TAU).sin()) as i16
                    }).collect();
                    phase += 160;
                    if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) = mic_tx.try_send(samples) { break; }
                }
            });
            let frames_control = control.clone();
            let frames = tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(33));
                loop { tick.tick().await; frames_control.fixture_camera_frame(sample.clone()); }
            });
            let mut audio = None;
            let mut video = None;
            timeout(Duration::from_secs(10), async {
                while audio.is_none() || video.is_none() {
                    match tracks.recv().await.unwrap() {
                        MediaStreamTrack::Audio(track) => audio = Some(NativeAudioStream::new(track, 48_000, 1)),
                        MediaStreamTrack::Video(track) if video.is_none() => video = Some(NativeVideoStream::new(track)),
                        _ => {},
                    }
                }
            }).await.expect("audio and camera tracks");
            let mut audio = audio.unwrap();
            let mut video = video.unwrap();
            timeout(Duration::from_secs(10), async {
                let sound = async {
                    loop {
                        if audio.next().await.unwrap().data.iter().any(|sample| sample.unsigned_abs() > 500) { break; }
                    }
                };
                let picture = async {
                    for _ in 0..60 {
                        let frame = video.next().await.unwrap();
                        let decoded = frame.buffer.to_i420();
                        let luma = decoded.data().0;
                        if luma.iter().map(|pixel| u64::from(*pixel)).sum::<u64>() > (luma.len() as u64) * 80 { return; }
                    }
                    panic!("only black frames were received")
                };
                tokio::join!(sound, picture);
            }).await.expect("PCM and preview frames traverse the app bridge");
            session.stop_camera().await;
            timeout(Duration::from_secs(3), async {
                loop {
                    if timeout(Duration::from_millis(300), video.next()).await.is_err() { break; }
                }
            }).await.expect("camera stops while the source still produces frames");
            assert!(timeout(Duration::from_secs(2), audio.next()).await.unwrap().is_some());
            session.start_camera().unwrap();
            timeout(Duration::from_secs(5), async {
                loop {
                    let frame = video.next().await.unwrap();
                    let decoded = frame.buffer.to_i420();
                    let luma = decoded.data().0;
                    if luma.iter().map(|pixel| u64::from(*pixel)).sum::<u64>() > (luma.len() as u64) * 80 { break; }
                }
            }).await.expect("camera resumes after being turned back on");
            frames.abort(); source.abort();
            session.stop_camera().await;
            session.stop().await;
            remote.close(); relay.abort();
        });
    }
}
