//! Bounded continuous PCM bridge. A WebRTC call captures and plays mono 48 kHz.
//! The older audio path captures mono 8 kHz and plays 48 kHz only when Opus is
//! offered. PipeWire
//! does the device resampling. Device access only occurs when an explicit
//! caller starts this owner; tests use synthetic pipes.
use ost::calling::media::PcmAudioIo;
pub use crate::call_aec::AecState;
use crate::call_aec::{AecError, EchoCancellation};
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
const FRAME_SAMPLES: usize = 160;
const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
// Absorb a 100 ms device burst plus scheduling margin without dropping speech.
// Capture stays at 20 ms. Playback asks for 60 ms so a tick with no decoded
// frame can wait for the samples instead of opening a hole in the stream.
const QUEUE_FRAMES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFailure {
    Start,
    CaptureEnded,
    PlaybackEnded,
    CaptureIo,
    PlaybackIo,
    EngineClosed,
    AecUnavailable,
    AecFailed,
}
#[derive(Default)]
struct Status {
    stop: AtomicBool,
    finished: AtomicBool,
    captured: AtomicU64,
    played: AtomicU64,
    capture_dropped: AtomicU64,
    microphone_level: AtomicU8,
    aec_state: AtomicU8,
    failure: std::sync::Mutex<Option<AudioFailure>>,
}
pub struct LiveAudio {
    status: Arc<Status>,
    worker: Option<thread::JoinHandle<()>>,
}
impl LiveAudio {
    /// Call off the UI executor, after explicit prejoin confirmation. A
    /// connected headset replaces the built-in speaker and microphone. No
    /// login or network discovery happens here.
    pub fn start(
        input: Option<&str>,
        output: Option<&str>,
        muted: bool,
        wide_playback: bool,
        wide_capture: bool,
    ) -> Result<(Self, PcmAudioIo), AudioFailure> {
        let prepared = crate::audio::prepare_call_devices(input, output).map_err(|_| AudioFailure::Start)?;
        // The experimental canceller is an 8 kHz graph. A 48 kHz far-end would
        // not match it, so a wide playback call leaves that graph off.
        let experimental_aec = crate::call_aec::experimental_requested(std::env::args()) && !wide_playback && !wide_capture;
        let playback_samples = if wide_playback { 960 } else { FRAME_SAMPLES };
        let capture_samples = if wide_capture { 960 } else { FRAME_SAMPLES };
        let command = |program: &str, target: Option<&str>, rate: &str, latency: &str| -> Result<Command, AudioFailure> {
            let mut cmd = unbuffered_command(program);
            if let Some(target) = target {
                if target.is_empty() || target.len() > 1024 || target.chars().any(char::is_control)
                {
                    return Err(AudioFailure::Start);
                }
                cmd.arg(format!("--target={target}"));
            }
            if experimental_aec {
                cmd.arg("--properties={ node.dont-reconnect = true node.dont-fallback = true }");
            }
            cmd.args(["--raw", "--format=s16"]);
            cmd.arg(format!("--rate={rate}"));
            cmd.arg(format!("--latency={latency}"));
            cmd.args(["--quality=10", "--channels=1", "--media-role=Communication", "-"]);
            Ok(cmd)
        };
        let echo = prepare_echo(experimental_aec, || {
            EchoCancellation::start(prepared.input.as_deref(), prepared.output.as_deref())
        })?;
        let capture_target = echo.as_ref().map(|e| e.input.as_str()).or(prepared.input.as_deref());
        let playback_target = echo.as_ref().map(|e| e.output.as_str()).or(prepared.output.as_deref());
        let capture = command("pw-record", capture_target, if wide_capture { "48000" } else { "8000" }, "20ms")?;
        let playback = command("pw-play", playback_target, if wide_playback { "48000" } else { "8000" }, "60ms")?;
        Self::spawn_owned(capture, playback, muted, Devices {
            capture: None, playback: None, echo, restore: prepared.restore,
        }, playback_samples, capture_samples)
    }
    #[cfg(test)]
    fn spawn(capture: Command, playback: Command, muted: bool, restore: Option<crate::audio::HeadsetRestore>)
        -> Result<(Self, PcmAudioIo), AudioFailure> {
        Self::spawn_owned(capture, playback, muted, Devices { capture: None, playback: None, echo: None, restore }, FRAME_SAMPLES, FRAME_SAMPLES)
    }
    fn spawn_owned(capture: Command, playback: Command, muted: bool, devices: Devices, playback_samples: usize, capture_samples: usize)
        -> Result<(Self, PcmAudioIo), AudioFailure> {
        Self::spawn_with(capture, playback, muted, devices, playback_samples, capture_samples,
            |echo, capture, playback| echo.connect_app(capture, playback),
            |work| thread::Builder::new().name("teams-call-pcm".into()).spawn(work))
    }
    fn spawn_with<C, W>(mut capture: Command, mut playback: Command, muted: bool, mut devices: Devices, playback_samples: usize, capture_samples: usize, connect: C, spawn_worker: W)
        -> Result<(Self, PcmAudioIo), AudioFailure>
    where C: FnOnce(&mut EchoCancellation, u32, u32) -> Result<(), AecError>,
        W: FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<thread::JoinHandle<()>> {
        devices.capture = Some(OwnedChild(capture.stdin(Stdio::null()).stdout(Stdio::piped())
            .stderr(Stdio::null()).spawn().map_err(|_| AudioFailure::Start)?));
        devices.playback = Some(OwnedChild(playback.stdin(Stdio::piped()).stdout(Stdio::null())
            .stderr(Stdio::null()).spawn().map_err(|_| AudioFailure::Start)?));
        if let Some(echo) = &mut devices.echo {
            connect(echo, devices.capture.as_ref().unwrap().0.id(), devices.playback.as_ref().unwrap().0.id())
                .map_err(aec_failure)?;
        }
        let capture = devices.capture.as_mut().unwrap();
        let playback = devices.playback.as_mut().unwrap();
        let mut source = capture.0.stdout.take().ok_or(AudioFailure::Start)?;
        let mut sink = playback.0.stdin.take().ok_or(AudioFailure::Start)?;
        nonblocking(source.as_raw_fd())?;
        nonblocking(sink.as_raw_fd())?;
        let (microphone_tx, microphone) = mpsc::sync_channel(QUEUE_FRAMES);
        let (speaker, speaker_rx) = mpsc::sync_channel::<Vec<i16>>(QUEUE_FRAMES);
        let status = Arc::new(Status::default());
        if devices.echo.is_none() { status.aec_state.store(AecState::Disabled as u8, Ordering::Relaxed); }
        let shared = status.clone();
        let worker = spawn_worker(Box::new(move || {
                let result = (|| -> Result<(), AudioFailure> {
                    let playback_bytes = playback_samples * 2;
                    let capture_bytes = capture_samples * 2;
                    let mut incoming = vec![0u8; capture_bytes];
                    let mut filled = 0;
                    let mut outgoing = vec![0u8; playback_bytes];
                    let mut offset = playback_bytes;
                    // The AEC graph needs a running far-end reference clock
                    // before connect_media can wait for capture readiness.
                    let mut bootstrap = devices.echo.is_some();
                    let mut next_bootstrap = std::time::Instant::now();
                    while !shared.stop.load(Ordering::Acquire) {
                        if let Some(echo) = &mut devices.echo { echo.check_alive().map_err(aec_failure)?; }
                        if devices.capture.as_mut().unwrap()
                            .0
                            .try_wait()
                            .map_err(|_| AudioFailure::CaptureIo)?
                            .is_some()
                        {
                            return Err(AudioFailure::CaptureEnded);
                        }
                        if devices.playback.as_mut().unwrap()
                            .0
                            .try_wait()
                            .map_err(|_| AudioFailure::PlaybackIo)?
                            .is_some()
                        {
                            return Err(AudioFailure::PlaybackEnded);
                        }
                        match source.read(&mut incoming[filled..]) {
                            Ok(0) => return Err(AudioFailure::CaptureEnded),
                            Ok(n) => {
                                filled += n;
                                if filled == capture_bytes {
                                    let frame: Vec<i16> = incoming
                                        .chunks_exact(2)
                                        .map(|b| i16::from_le_bytes([b[0], b[1]]))
                                        .collect();
                                    if devices.echo.is_some() && shared.aec_state.swap(AecState::Active as u8, Ordering::Relaxed) != AecState::Active as u8 {
                                        crate::call_aec::trace(AecState::Active);
                                    }
                                    let level = pcm_level(&frame);
                                    let previous = shared.microphone_level.load(Ordering::Relaxed);
                                    shared.microphone_level.store(level.max(previous.saturating_sub(12)), Ordering::Relaxed);
                                    match microphone_tx.try_send(frame) {
                                        Ok(()) => {
                                            shared.captured.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(mpsc::TrySendError::Full(_)) => {
                                            shared.capture_dropped.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(mpsc::TrySendError::Disconnected(_)) => {
                                            return Err(AudioFailure::EngineClosed);
                                        }
                                    }
                                    filled = 0;
                                }
                            }
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::Interrupted
                                ) => {}
                            Err(_) => return Err(AudioFailure::CaptureIo),
                        }
                        if offset == playback_bytes {
                            match speaker_rx.try_recv() {
                                Ok(frame) => {
                                    bootstrap = false;
                                    if frame.len() == playback_samples {
                                        for (bytes, sample) in
                                            outgoing.chunks_exact_mut(2).zip(frame)
                                        {
                                            bytes.copy_from_slice(&sample.to_le_bytes());
                                        }
                                        offset = 0;
                                    }
                                }
                                Err(mpsc::TryRecvError::Disconnected) => {
                                    return Err(AudioFailure::EngineClosed);
                                }
                                Err(mpsc::TryRecvError::Empty) => {
                                    if bootstrap && std::time::Instant::now() >= next_bootstrap {
                                        outgoing.fill(0);
                                        offset = 0;
                                        next_bootstrap = std::time::Instant::now() + Duration::from_millis(20);
                                    }
                                }
                            }
                        }
                        if offset < playback_bytes {
                            match sink.write(&outgoing[offset..]) {
                                Ok(0) => return Err(AudioFailure::PlaybackEnded),
                                Ok(n) => {
                                    offset += n;
                                    if offset == playback_bytes {
                                        shared.played.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::Interrupted
                                    ) => {}
                                Err(_) => return Err(AudioFailure::PlaybackIo),
                            }
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    Ok(())
                })();
                drop(source);
                drop(sink);
                drop(devices);
                if let Err(error) = result {
                    if matches!(error, AudioFailure::AecFailed | AudioFailure::AecUnavailable) {
                        shared.aec_state.store(AecState::Failed as u8, Ordering::Release);
                        crate::call_aec::trace(AecState::Failed);
                    }
                    *shared.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                }
                crate::live_trace::record("call_pcm_capture", 0,
                    shared.captured.load(Ordering::Relaxed) as usize,
                    shared.capture_dropped.load(Ordering::Relaxed) as usize);
                crate::live_trace::record("call_pcm_playback", 0,
                    shared.played.load(Ordering::Relaxed) as usize, 0);
                shared.finished.store(true, Ordering::Release);
            }))
            .map_err(|_| AudioFailure::Start)?;
        Ok((
            Self {
                status,
                worker: Some(worker),
            },
            PcmAudioIo {
                microphone,
                speaker,
                initially_muted: muted,
            },
        ))
    }
    pub fn is_ready(&self) -> bool {
        self.status.captured.load(Ordering::Acquire) > 0 && !self.is_finished()
    }
    pub fn microphone_level(&self) -> u8 {
        if self.is_finished() { 0 } else { self.status.microphone_level.load(Ordering::Relaxed) }
    }

    #[cfg(test)]
    pub(crate) fn synthetic() -> Result<(Self, PcmAudioIo), AudioFailure> {
        let mut capture = Command::new("python3");
        capture.args(["-u","-c","import os,time,struct\nf=struct.pack('<160h',*([9000]*160))\nwhile True:\n os.write(1,f);time.sleep(.02)"]);
        let mut sink = Command::new("python3");
        sink.args(["-u", "-c", "import os\nwhile os.read(0,320): pass"]);
        Self::spawn(capture, sink, true, None)
    }
    pub fn aec_state(&self) -> AecState {
        match self.status.aec_state.load(Ordering::Acquire) {
            1 => AecState::Active, 2 => AecState::Unavailable, 3 => AecState::Failed, 4 => AecState::Disabled, _ => AecState::Starting,
        }
    }
    pub fn trace_health(&self) {
        crate::call_aec::trace(self.aec_state());
        crate::live_trace::record("call_pcm_capture", 0,
            self.status.captured.load(Ordering::Relaxed) as usize,
            self.status.capture_dropped.load(Ordering::Relaxed) as usize);
        crate::live_trace::record("call_pcm_playback", 0,
            self.status.played.load(Ordering::Relaxed) as usize, 0);
    }

    pub fn request_stop(&self) {
        self.status.stop.store(true, Ordering::Release);
    }

    pub fn failure(&self) -> Option<AudioFailure> {
        *self
            .status
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    pub fn is_finished(&self) -> bool {
        self.status.finished.load(Ordering::Acquire)
    }
    /// Join on a worker, never on the UI thread. Drop also requests shutdown.
    pub fn stop(mut self) {
        self.status.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// Map RMS from -55 dBFS to 0 dBFS onto a compact 0–100 meter.
fn pcm_level(samples: &[i16]) -> u8 {
    if samples.is_empty() { return 0; }
    let energy = samples.iter().map(|&sample| f64::from(sample).powi(2)).sum::<f64>() / samples.len() as f64;
    if energy == 0.0 { return 0; }
    let dbfs = 20.0 * (energy.sqrt() / 32768.0).log10();
    (((dbfs + 55.0) / 55.0).clamp(0.0, 1.0) * 100.0).round() as u8
}
impl Drop for LiveAudio {
    fn drop(&mut self) {
        self.status.stop.store(true, Ordering::Release);
    }
}
fn prepare_echo<T>(enabled: bool, start: impl FnOnce() -> Result<T, AecError>) -> Result<Option<T>, AudioFailure> {
    if enabled { start().map(Some).map_err(aec_failure) }
    else { crate::call_aec::trace(AecState::Disabled); Ok(None) }
}
fn aec_failure(error: AecError) -> AudioFailure {
    crate::call_aec::trace(match error { AecError::Unavailable => AecState::Unavailable, AecError::Failed => AecState::Failed });
    match error { AecError::Unavailable => AudioFailure::AecUnavailable, AecError::Failed => AudioFailure::AecFailed }
}
// One owner makes failure unwinding and thread-spawn errors obey the same
// teardown order: both app streams, AEC module, then Bluetooth profile rollback.
struct Devices {
    capture: Option<OwnedChild>, playback: Option<OwnedChild>,
    echo: Option<EchoCancellation>, restore: Option<crate::audio::HeadsetRestore>,
}
impl Drop for Devices {
    fn drop(&mut self) {
        drop(self.capture.take()); drop(self.playback.take());
        drop(self.echo.take()); drop(self.restore.take());
    }
}
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
// pw-cat raw_record/raw_play use C stdio. In a pipe, stdout's default block
// buffer can hold ~256 ms at 8 kHz, overwhelming the bounded 20 ms bridge.
fn unbuffered_command(program: &str) -> Command {
    let mut command = Command::new("stdbuf");
    command.args(["--input=0", "--output=0", program]);
    command
}

fn nonblocking(fd: std::os::fd::RawFd) -> Result<(), AudioFailure> {
    // Both descriptors are owned pipes; flags do not affect unrelated handles.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(AudioFailure::Start);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normal_calls_never_start_aec_and_experimental_errors_never_fall_back() {
        let disabled = prepare_echo::<()>(false, || panic!("normal call opened experimental graph")).unwrap();
        assert!(disabled.is_none());
        assert_eq!(prepare_echo::<()>(true, || Err(AecError::Failed)), Err(AudioFailure::AecFailed));
        assert_eq!(prepare_echo(true, || Ok(42)).unwrap(), Some(42));
    }
    fn fake_capture() -> Command {
        let mut command = Command::new("python3"); command.args(["-c", "import time;time.sleep(60)"]); command
    }
    fn exists(pid: u32) -> bool { unsafe { libc::kill(pid as i32, 0) == 0 } }
    #[test]
    fn route_failure_and_worker_spawn_failure_release_all_three_processes() {
        for fail_route in [true, false] {
            let echo = EchoCancellation::synthetic(); let backend = echo.process_id();
            let mut children = (0, 0);
            let result = LiveAudio::spawn_with(fake_capture(), fake_capture(), true,
                Devices { capture: None, playback: None, echo: Some(echo), restore: None },
                FRAME_SAMPLES,
                FRAME_SAMPLES,
                |_, capture, playback| { children = (capture,playback); if fail_route { Err(AecError::Failed) } else { Ok(()) } },
                |_| Err(std::io::Error::other("fixture worker spawn failed")));
            assert!(result.is_err());
            for pid in [backend, children.0, children.1] { assert!(!exists(pid), "child {pid} survived"); }
        }
    }
    #[test]
    fn aec_bootstrap_feeds_clocked_silence_before_capture_readiness() {
        let marker = std::env::temp_dir().join(format!("teams-aec-bootstrap-{:016x}", rand::random::<u64>()));
        let mut capture = Command::new("python3");
        capture.args(["-u", "-c", "import os,sys,time,struct\nwhile not os.path.exists(sys.argv[1]): time.sleep(.002)\nf=struct.pack('<160h',*([9000]*160))\nwhile True: os.write(1,f);time.sleep(.02)"]);
        capture.arg(&marker);
        let mut playback = Command::new("python3");
        playback.args(["-u", "-c", "import os,sys\nwhile True:\n f=os.read(0,320)\n if not f: break\n with open(sys.argv[1],'ab') as out: out.write(f)"]);
        playback.arg(&marker);
        let echo = EchoCancellation::synthetic();
        let (owner, io) = LiveAudio::spawn_with(capture, playback, true,
            Devices { capture: None, playback: None, echo: Some(echo), restore: None },
            FRAME_SAMPLES,
            FRAME_SAMPLES,
            |_,_,_| Ok(()), |work| thread::Builder::new().spawn(work)).unwrap();
        assert_eq!(io.microphone.recv_timeout(Duration::from_secs(2)).unwrap(), vec![9000;160]);
        assert_eq!(owner.aec_state(), AecState::Active);
        assert!(owner.is_ready()); assert!(io.initially_muted);
        io.speaker.send(vec![1234;160]).unwrap();
        thread::sleep(Duration::from_millis(100));
        owner.stop();
        let bytes = std::fs::read(&marker).unwrap(); let _ = std::fs::remove_file(&marker);
        assert!(bytes.starts_with(&[0; FRAME_BYTES]));
        let frames = bytes.chunks_exact(FRAME_BYTES).collect::<Vec<_>>();
        assert!(frames.len() <= 10, "bootstrap was not clocked: {}", frames.len());
        let expected = 1234i16.to_le_bytes().repeat(FRAME_SAMPLES);
        assert_eq!(frames.last().unwrap(), &&expected[..]);
    }
    #[test]
    fn meter_tracks_silence_and_increasing_signal() {
        assert_eq!(pcm_level(&[]), 0);
        assert_eq!(pcm_level(&[0; 160]), 0);
        assert_eq!(pcm_level(&[i16::MIN; 160]), 100);
        assert!(pcm_level(&[9000; 160]) > pcm_level(&[900; 160]));
    }
    #[test]
    fn synthetic_continuous_audio_is_bounded_and_stops_without_hardware() {
        let mut capture = Command::new("python3");
        capture.args(["-u","-c","import os,time,struct\nf=struct.pack('<160h',*([9000]*160))\nwhile True:\n os.write(1,f);time.sleep(.02)"]);
        let mut sink = Command::new("python3");
        sink.args(["-u", "-c", "import os\nwhile os.read(0,320): pass"]);
        let (owner, io) = LiveAudio::spawn(capture, sink, true, None).unwrap();
        assert!(io.initially_muted);
        let frame = io.microphone.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(frame, vec![9000; 160]);
        assert!(owner.microphone_level() > 0);
        io.speaker.send(vec![-9000; 160]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while owner.status.played.load(Ordering::Relaxed) == 0
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(owner.status.played.load(Ordering::Relaxed), 1);
        thread::sleep(Duration::from_millis(150));
        assert!(io.microphone.try_iter().count() <= QUEUE_FRAMES);
        assert_eq!(owner.failure(), None);
        let status = owner.status.clone();
        owner.stop();
        assert!(status.finished.load(Ordering::Acquire));
    }
    #[test]
    fn capture_preserves_a_device_quantum_larger_than_three_network_frames() {
        let mut capture = Command::new("python3");
        // One 100 ms quantum, as opposed to a source already paced at 20 ms.
        capture.args(["-u", "-c", "import os,time,struct\nos.write(1,b''.join(struct.pack('<160h',*([v]*160)) for v in range(1,6)))\ntime.sleep(10)"]);
        let mut sink = Command::new("python3");
        sink.args(["-u", "-c", "import os\nwhile os.read(0,320): pass"]);
        let (owner, io) = LiveAudio::spawn(capture, sink, false, None).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while owner.status.captured.load(Ordering::Relaxed)
            + owner.status.capture_dropped.load(Ordering::Relaxed) < 5
        {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        let frames = io.microphone.try_iter().collect::<Vec<_>>();
        assert_eq!(frames, (1..=5).map(|v| vec![v; 160]).collect::<Vec<_>>());
        assert_eq!(owner.status.capture_dropped.load(Ordering::Relaxed), 0);
        owner.stop();
    }
    #[test]
    fn c_stdio_capture_delivers_a_frame_without_waiting_for_a_full_file_buffer() {
        // Same fwrite/stdout boundary as pw-cat raw_record, no hardware access.
        let mut capture = unbuffered_command("python3");
        capture.args(["-c", "import ctypes,time,struct\nc=ctypes.CDLL(None)\nout=ctypes.c_void_p.in_dll(c,'stdout')\nf=struct.pack('<160h',*([9000]*160))\nc.fwrite.argtypes=[ctypes.c_void_p,ctypes.c_size_t,ctypes.c_size_t,ctypes.c_void_p]\nc.fwrite(f,2,160,out)\ntime.sleep(10)"]);
        let mut sink = Command::new("python3");
        sink.args(["-u", "-c", "import os\nwhile os.read(0,320): pass"]);
        let (owner, io) = LiveAudio::spawn(capture, sink, false, None).unwrap();
        let frame = io.microphone.recv_timeout(Duration::from_secs(2));
        owner.stop();
        assert_eq!(frame.unwrap(), vec![9000; 160]);
    }

    #[test]
    fn ended_capture_is_reported_and_releases_worker() {
        let mut capture = Command::new("python3");
        capture.args(["-c", "pass"]);
        let mut sink = Command::new("python3");
        sink.args(["-c", "import os\nwhile os.read(0,320): pass"]);
        let (owner, _io) = LiveAudio::spawn(capture, sink, false, None).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !owner.is_finished() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(owner.is_finished());
        assert_eq!(owner.failure(), Some(AudioFailure::CaptureEnded));
        owner.stop();
    }
}
