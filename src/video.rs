//! Local camera preview only. No frame is transmitted. CPU conversion fallback
//! feeds an Iced/wgpu texture; hardware probes are documented separately.
use iced::widget::image::Handle;
use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 480;
const FRAME_BYTES: usize = WIDTH as usize * HEIGHT as usize * 4;

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraDevice {
    pub path: String,
    pub label: String,
}
impl std::fmt::Display for CameraDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

pub fn enumerate() -> Result<Vec<CameraDevice>, String> {
    let mut devices = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") else {
        return Ok(devices);
    };
    for entry in entries.flatten().take(64) {
        let name = entry.file_name().to_string_lossy().to_string();
        if !valid_node(&format!("/dev/{name}")) {
            continue;
        }
        // UVC exposes separate metadata nodes. Only list capture index zero.
        if std::fs::read_to_string(entry.path().join("index"))
            .unwrap_or_default()
            .trim()
            != "0"
        {
            continue;
        }
        let label =
            std::fs::read_to_string(entry.path().join("name")).unwrap_or_else(|_| name.clone());
        devices.push(CameraDevice {
            path: format!("/dev/{name}"),
            label: format!(
                "{} · {}",
                label.trim().chars().take(100).collect::<String>(),
                name
            ),
        });
    }
    devices.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(devices)
}

fn valid_node(value: &str) -> bool {
    value.strip_prefix("/dev/video").is_some_and(|number| {
        !number.is_empty() && number.len() <= 4 && number.bytes().all(|b| b.is_ascii_digit())
    })
}

#[derive(Default)]
struct Latest {
    frame: Option<Handle>,
    count: u64,
    last_frame: Option<Instant>,
}
struct Inner {
    cancel: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
    latest: Arc<Mutex<Latest>>,
    started: Instant,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
#[derive(Clone)]
pub struct Preview(Arc<Inner>);
impl Preview {
    pub fn start(device: &CameraDevice, cancellation: Cancellation) -> Result<Self, String> {
        if cancellation.is_cancelled() {
            return Err("Captura cancelada.".into());
        }
        use std::os::unix::fs::FileTypeExt;
        if !valid_node(&device.path)
            || !std::fs::metadata(Path::new(&device.path))
                .is_ok_and(|m| m.file_type().is_char_device())
        {
            return Err("A câmera selecionada não está disponível.".into());
        }
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "v4l2",
            "-framerate",
            "30",
            "-video_size",
            "640x480",
            "-i",
            &device.path,
        ]);
        Self::spawn(cmd, cancellation, true)
    }
    /// Latest camera frame for the in-call self-view. No output frame rate:
    /// pacing the decoded H.264 made motion lag the camera.
    pub fn start_latest(device: &CameraDevice, cancellation: Cancellation) -> Result<Self, String> {
        if cancellation.is_cancelled() {
            return Err("Captura cancelada.".into());
        }
        use std::os::unix::fs::FileTypeExt;
        if !valid_node(&device.path)
            || !std::fs::metadata(Path::new(&device.path))
                .is_ok_and(|meta| meta.file_type().is_char_device())
        {
            return Err("A câmera selecionada não está disponível.".into());
        }
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
            "-use_wallclock_as_timestamps",
            "1",
            "-f",
            "v4l2",
            "-timestamps",
            "abs",
            "-framerate",
            "30",
            "-video_size",
            "640x480",
            "-thread_queue_size",
            "1",
            "-i",
            &device.path,
        ]);
        Self::spawn(cmd, cancellation, false)
    }
    /// Offline fixture source: no device, account or remote transport.
    pub(crate) fn synthetic() -> Result<Self, String> {
        let mut command = Command::new("ffmpeg");
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x480:rate=15",
        ]);
        Self::spawn(command, Cancellation::default(), true)
    }
    fn spawn(mut command: Command, cancellation: Cancellation, paced: bool) -> Result<Self, String> {
        if cancellation.is_cancelled() {
            return Err("Captura cancelada.".into());
        }
        command.args([
            "-an",
            "-vf",
            "format=rgba",
            "-threads",
            "1",
            "-filter_threads",
            "1",
        ]);
        if paced {
            command.args(["-r", "15"]);
        } else {
            command.args(["-fps_mode", "passthrough"]);
        }
        command
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Linux kills capture if the application (or its persistent spawning
        // worker) exits before asynchronous cleanup can run.
        use std::os::unix::process::CommandExt;
        let parent = std::process::id() as libc::pid_t;
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                    || libc::getppid() != parent
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|_| {
            "Não foi possível iniciar a prévia. Verifique se o FFmpeg está instalado.".to_owned()
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Não foi possível ler a câmera.")?;
        let preview = Self(Arc::new(Inner {
            cancel: cancellation.0.clone(),
            ended: Arc::new(AtomicBool::new(false)),
            latest: Arc::new(Mutex::new(Latest::default())),
            started: Instant::now(),
        }));
        let owned = Arc::new(Mutex::new(Some(child)));
        let managed = owned.clone();
        let cancel = preview.0.cancel.clone();
        let ended = preview.0.ended.clone();
        if thread::Builder::new()
            .name("teams-camera-lifecycle".into())
            .spawn(move || {
                let mut child = managed.lock().unwrap().take().unwrap();
                loop {
                    if cancel.load(Ordering::Acquire) {
                        let _ = child.kill();
                        break;
                    }
                    match child.try_wait() {
                        Ok(Some(_)) | Err(_) => break,
                        Ok(None) => {}
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                let _ = child.kill();
                let _ = child.wait();
                ended.store(true, Ordering::Release);
            })
            .is_err()
        {
            if let Some(mut child) = owned.lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err("Não foi possível acompanhar a câmera.".into());
        }
        let latest = preview.0.latest.clone();
        let cancel = preview.0.cancel.clone();
        thread::Builder::new()
            .name("teams-camera-frames".into())
            .spawn(move || read_frames(stdout, latest, cancel))
            .map_err(|_| "Não foi possível receber os quadros da câmera.".to_owned())?;
        Ok(preview)
    }
    pub fn take_frame(&self) -> Option<Handle> {
        let mut latest = self.0.latest.lock().ok()?;
        if self.0.cancel.load(Ordering::Acquire) {
            return None;
        }
        let frame = latest.frame.take()?;
        if let Some(at) = latest.last_frame {
            crate::live_trace::record("video_frame_age", at.elapsed().as_micros() as u64, 0, 0);
        }
        Some(frame)
    }
    /// Explicitly stop all clones of this capture, without waiting on the UI.
    /// The lifecycle worker owns kill/wait. A stale callback or another holder
    /// of Preview must not keep the device running after its owner closes it.
    pub fn stop(&self) {
        self.0.cancel.store(true, Ordering::Release);
        if let Ok(mut latest) = self.0.latest.lock() {
            latest.frame = None;
        }
    }
    pub fn failed(&self) -> bool {
        self.0.ended.load(Ordering::Acquire)
            || (self
                .0
                .latest
                .lock()
                .map(|s| s.last_frame.unwrap_or(self.0.started).elapsed() > Duration::from_secs(6))
                .unwrap_or(true))
    }
}
fn read_frames(mut reader: impl Read, latest: Arc<Mutex<Latest>>, cancel: Arc<AtomicBool>) {
    while !cancel.load(Ordering::Acquire) {
        let mut pixels = vec![0; FRAME_BYTES];
        if reader.read_exact(&mut pixels).is_err() {
            break;
        }
        let frame = Handle::from_rgba(WIDTH, HEIGHT, pixels);
        let Ok(mut state) = latest.lock() else {
            break;
        };
        if cancel.load(Ordering::Acquire) {
            break;
        }
        state.frame = Some(frame); // One latest frame, never an unbounded queue.
        state.count = state.count.saturating_add(1);
        state.last_frame = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_queued_capture_never_spawns_a_process() {
        let cancellation = Cancellation::default();
        cancellation.cancel();
        let result = Preview::spawn(Command::new("must-not-execute-any-process"), cancellation, true);
        assert!(matches!(result,Err(ref error) if error == "Captura cancelada."));
    }

    #[test]
    fn synthetic_capture_releases_process_after_last_owner_drops() {
        // Hermetic media source: never opens a camera, microphone or network.
        if Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            return;
        }
        let preview = Preview::synthetic().unwrap();
        let ended = preview.0.ended.clone();
        let deadline = Instant::now() + Duration::from_secs(3);
        while preview.0.latest.lock().unwrap().count == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(preview.take_frame().is_some());
        drop(preview);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ended.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            ended.load(Ordering::Acquire),
            "capture process was not reaped"
        );
    }

    #[test]
    fn latest_preview_rejects_a_missing_camera_without_spawning() {
        let result = Preview::start_latest(
            &CameraDevice { path: "/dev/video9999".into(), label: "none".into() },
            Cancellation::default(),
        );
        assert!(result.is_err());
    }
    fn frame_queue_replaces_old_frames_and_discards_truncated_tail() {
        let latest = Arc::new(Mutex::new(Latest::default()));
        let bytes = [vec![1; FRAME_BYTES], vec![2; FRAME_BYTES], vec![3; 10]].concat();
        read_frames(
            std::io::Cursor::new(bytes),
            latest.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let mut state = latest.lock().unwrap();
        assert_eq!(state.count, 2);
        let Handle::Rgba { pixels, .. } = state.frame.take().unwrap() else {
            panic!("rgba");
        };
        assert!(pixels.iter().all(|byte| *byte == 2));
        assert!(state.frame.is_none());
    }
    #[test]
    fn stopping_preview_reaps_process_even_while_another_owner_is_retained() {
        let preview = Preview::synthetic().expect("FFmpeg synthetic fixture must be available");
        let retained = preview.clone();
        let ended = preview.0.ended.clone();
        let deadline = Instant::now() + Duration::from_secs(3);
        while preview.0.latest.lock().unwrap().count == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(preview.take_frame().is_some());
        preview.stop();
        drop(preview);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ended.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            ended.load(Ordering::Acquire),
            "retained clone kept capture alive"
        );
        assert!(retained.take_frame().is_none());
        assert!(retained.0.latest.lock().unwrap().frame.is_none());
        retained.stop(); // Repeated shutdown remains harmless.
    }
    #[test]
    fn camera_path_never_becomes_shell_or_arbitrary_file_input() {
        for path in [
            "/etc/passwd",
            "https://example.org",
            "/dev/video0;id",
            "/dev/video/0",
            "/dev/video",
            "/dev/video0/../1",
        ] {
            assert!(!valid_node(path));
        }
        assert!(valid_node("/dev/video12"));
    }
    #[test]
    fn dropping_last_preview_owner_cancels_even_if_worker_holds_flags() {
        let cancel = Arc::new(AtomicBool::new(false));
        let preview = Preview(Arc::new(Inner {
            cancel: cancel.clone(),
            ended: Arc::new(AtomicBool::new(false)),
            latest: Arc::new(Mutex::new(Latest::default())),
            started: Instant::now(),
        }));
        let clone = preview.clone();
        drop(preview);
        assert!(!cancel.load(Ordering::Acquire));
        drop(clone);
        assert!(cancel.load(Ordering::Acquire));
    }
}
