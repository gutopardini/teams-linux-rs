//! Bounded H.264 access units for a native camera track. Opening a camera is
//! explicit; neither discovery nor encoder probing accesses camera hardware.
use std::{
    io::{self, Read, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_BUFFER: usize = 2 * 1024 * 1024;
const QUEUE: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Vaapi,
    OpenH264,
    X264,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeBackend {
    Vaapi,
    Software,
}
#[derive(Debug)]
pub struct AccessUnit {
    pub nals: Vec<Vec<u8>>,
    pub keyframe: bool,
}
pub struct Encoder {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    pub frames: mpsc::Receiver<AccessUnit>,
    pub backend: Backend,
}
pub struct Decoder {
    stop: Arc<AtomicBool>,
    input: mpsc::SyncSender<Vec<u8>>,
    latest: Arc<Mutex<Option<iced::widget::image::Handle>>>,
    pub backend: DecodeBackend,
}
impl Drop for Decoder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
impl Decoder {
    pub fn start() -> Result<Self, &'static str> {
        if let Ok(decoder) = Self::start_with(DecodeBackend::Vaapi) {
            return Ok(decoder);
        }
        Self::start_with(DecodeBackend::Software)
    }
    fn start_with(backend: DecodeBackend) -> Result<Self, &'static str> {
        let mut command = decode_command(backend)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut process = Process(
            command
                .spawn()
                .map_err(|_| "Não foi possível iniciar o decoder de vídeo.")?,
        );
        let mut stdin = process.0.stdin.take().ok_or("Decoder sem entrada.")?;
        let mut stdout = process.0.stdout.take().ok_or("Decoder sem saída.")?;
        for fd in [stdin.as_raw_fd(), stdout.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err("Falha ao preparar decoder.");
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let reading = stop.clone();
        let writing = stop.clone();
        let latest = Arc::new(Mutex::new(None));
        let output = latest.clone();
        let (input, receiver) = mpsc::sync_channel::<Vec<u8>>(2);
        thread::Builder::new()
            .name("teams-h264-decode".into())
            .spawn(move || {
                let writer = thread::spawn(move || {
                    while !writing.load(Ordering::Acquire) {
                        let bytes = match receiver.recv_timeout(Duration::from_millis(20)) {
                            Ok(b) => b,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(_) => break,
                        };
                        let mut offset = 0;
                        let deadline = Instant::now() + Duration::from_secs(2);
                        while offset < bytes.len() && !writing.load(Ordering::Acquire) {
                            match stdin.write(&bytes[offset..]) {
                                Ok(0) => return,
                                Ok(n) => offset += n,
                                Err(e)
                                    if e.kind() == io::ErrorKind::WouldBlock
                                        && Instant::now() < deadline =>
                                {
                                    thread::sleep(Duration::from_millis(2))
                                }
                                Err(_) => return,
                            }
                        }
                    }
                });
                let mut frame = vec![0u8; 640 * 480 * 4];
                let mut offset = 0;
                while !reading.load(Ordering::Acquire) {
                    match stdout.read(&mut frame[offset..]) {
                        Ok(0) => break,
                        Ok(n) => {
                            offset += n;
                            if offset == frame.len() {
                                *output.lock().unwrap_or_else(|e| e.into_inner()) = Some(
                                    iced::widget::image::Handle::from_rgba(640, 480, frame.clone()),
                                );
                                offset = 0;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            if process.0.try_wait().ok().flatten().is_some() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(4));
                        }
                        Err(_) => break,
                    }
                }
                reading.store(true, Ordering::Release);
                drop(process);
                let _ = writer.join();
            })
            .map_err(|_| "Falha no worker de vídeo.")?;
        Ok(Self {
            stop,
            input,
            latest,
            backend,
        })
    }
    pub fn push(&self, unit: &AccessUnit) -> bool {
        let size = unit.nals.iter().map(|n| n.len() + 4).sum::<usize>();
        if size > MAX_BUFFER {
            return false;
        }
        let mut bytes = Vec::with_capacity(size);
        for nal in &unit.nals {
            bytes.extend_from_slice(&[0, 0, 0, 1]);
            bytes.extend_from_slice(nal);
        }
        self.input.try_send(bytes).is_ok()
    }
    pub fn take_frame(&self) -> Option<iced::widget::image::Handle> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
impl Encoder {
    /// Invoke on a device worker, after explicit camera activation.
    pub fn start(
        device: &crate::video::CameraDevice,
        backend: Backend,
    ) -> Result<Self, &'static str> {
        let path = device
            .path
            .strip_prefix("/dev/video")
            .ok_or("Câmera inválida.")?;
        if path.is_empty() || path.len() > 4 || !path.bytes().all(|b| b.is_ascii_digit()) {
            return Err("Câmera inválida.");
        }
        let mut command = base(backend);
        command.args([
            "-f",
            "v4l2",
            "-framerate",
            "30",
            "-video_size",
            "640x480",
            "-i",
            &device.path,
        ]);
        output(&mut command, backend);
        Self::spawn(command, backend)
    }
    /// A listed encoder is insufficient: exercise an actual synthetic frame
    /// on the render device, bounded in time, before selecting GPU encoding.
    pub fn probe() -> Result<Backend, &'static str> {
        for backend in [Backend::Vaapi, Backend::OpenH264, Backend::X264] {
            if backend == Backend::Vaapi && !std::path::Path::new("/dev/dri/renderD128").exists() {
                continue;
            }
            if supported(backend) {
                return Ok(backend);
            }
        }
        Err("Nenhum codificador H.264 funcional foi encontrado.")
    }
    #[cfg(test)]
    pub fn synthetic() -> Result<Self, &'static str> {
        let mut command = base(Backend::OpenH264);
        command.args(["-re", "-f", "lavfi", "-i", "testsrc2=size=640x480:rate=15"]);
        output(&mut command, Backend::OpenH264);
        Self::spawn(command, Backend::OpenH264)
    }
    fn spawn(mut command: Command, backend: Backend) -> Result<Self, &'static str> {
        let mut process = Process(
            command
                .spawn()
                .map_err(|_| "Não foi possível iniciar o codificador de vídeo.")?,
        );
        let mut reader = process
            .0
            .stdout
            .take()
            .ok_or("A câmera não forneceu vídeo.")?;
        let fd = reader.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err("Não foi possível preparar o vídeo.");
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (sender, frames) = mpsc::sync_channel(QUEUE);
        let worker = thread::Builder::new()
            .name("teams-h264-encoder".into())
            .spawn(move || {
                let mut parser = AnnexB::default();
                let mut buffer = [0u8; 16384];
                let mut await_key = true;
                let mut last_bytes = Instant::now();
                while !worker_stop.load(Ordering::Acquire) {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            last_bytes = Instant::now();
                            let Ok(units) = parser.push(&buffer[..count]) else {
                                break;
                            };
                            for unit in units {
                                if await_key && !unit.keyframe {
                                    continue;
                                }
                                match sender.try_send(unit) {
                                    Ok(()) => await_key = false,
                                    Err(mpsc::TrySendError::Full(_)) => await_key = true,
                                    Err(mpsc::TrySendError::Disconnected(_)) => return,
                                }
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if last_bytes.elapsed() > Duration::from_secs(5) {
                                break;
                            }
                            if process.0.try_wait().ok().flatten().is_some() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
                // Process guard kills and reaps on every exit, including failure.
            })
            .map_err(|_| "Não foi possível iniciar o worker de vídeo.")?;
        Ok(Self {
            stop,
            worker: Some(worker),
            frames,
            backend,
        })
    }
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn supported(backend: Backend) -> bool {
    if backend == Backend::Vaapi {
        return matches!(vaapi_state(), VaapiState::Ready(_));
    }
    let mut command = base(backend);
    command.args([
        "-f",
        "lavfi",
        "-i",
        "color=size=640x480:rate=15",
        "-frames:v",
        "2",
    ]);
    output(&mut command, backend);
    command.stdout(Stdio::null());
    let Ok(child) = command.spawn() else {
        return false;
    };
    let mut child = Process(child);
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match child.0.try_wait() {
            Ok(Some(status)) => return status.success(),
            Err(_) => return false,
            _ if Instant::now() >= deadline => return false,
            _ => thread::sleep(Duration::from_millis(10)),
        }
    }
}
fn base(backend: Backend) -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .args(["-nostdin", "-hide_banner", "-loglevel", "error"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if backend == Backend::Vaapi {
        command.args(["-vaapi_device", "/dev/dri/renderD128"]);
        if let VaapiState::Ready(Some(dir)) = vaapi_state() {
            command.env("LIBVA_DRIVERS_PATH", dir);
        }
    }
    command
}
fn output(command: &mut Command, backend: Backend) {
    match backend {
        Backend::Vaapi => {
            command.args([
                "-vf",
                "format=nv12,hwupload",
                "-c:v",
                "h264_vaapi",
                "-profile:v",
                "constrained_baseline",
            ]);
        }
        Backend::OpenH264 => {
            command.args([
                "-vf",
                "format=yuv420p",
                "-c:v",
                "libopenh264",
                "-profile:v",
                "constrained_baseline",
            ]);
        }
        Backend::X264 => {
            command.args([
                "-vf",
                "format=yuv420p",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-profile:v",
                "baseline",
            ]);
        }
    }
    command.args([
        "-an",
        "-b:v",
        "600k",
        "-g",
        "30",
        "-bf",
        "0",
        "-bsf:v",
        "h264_metadata=aud=insert",
        "-f",
        "h264",
        "pipe:1",
    ]);
}

enum VaapiState {
    Ready(Option<PathBuf>),
    Missing,
}
fn vaapi_state() -> &'static VaapiState {
    static STATE: OnceLock<VaapiState> = OnceLock::new();
    STATE.get_or_init(detect_vaapi)
}
fn detect_vaapi() -> VaapiState {
    if !Path::new("/dev/dri/renderD128").exists() {
        return VaapiState::Missing;
    }
    let mut dirs = ihd_dirs();
    dirs.sort_by_key(|dir| {
        std::fs::metadata(dir)
            .and_then(|meta| meta.modified())
            .ok()
    });
    dirs.reverse();
    for dir in &dirs {
        if vaapi_encode_succeeds(Some(dir)) {
            return VaapiState::Ready(Some(dir.clone()));
        }
    }
    if vaapi_encode_succeeds(None) {
        VaapiState::Ready(None)
    } else {
        VaapiState::Missing
    }
}
fn ihd_dirs() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(
        "/var/lib/flatpak/runtime/org.freedesktop.Platform.VAAPI.Intel",
    )];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(
            PathBuf::from(home)
                .join(".local/share/flatpak/runtime/org.freedesktop.Platform.VAAPI.Intel"),
        );
    }
    let mut found = Vec::new();
    for root in roots {
        let mut stack = vec![root];
        let mut seen = 0usize;
        while let Some(dir) = stack.pop() {
            seen += 1;
            if seen > 400 {
                break;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.file_name().and_then(|name| name.to_str()) == Some("iHD_drv_video.so") {
                    if let Some(parent) = path.parent() {
                        found.push(parent.to_path_buf());
                    }
                } else if path.is_dir() {
                    stack.push(path);
                }
            }
        }
    }
    found
}
fn vaapi_encode_succeeds(driver: Option<&Path>) -> bool {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-vaapi_device",
            "/dev/dri/renderD128",
            "-f",
            "lavfi",
            "-i",
            "color=size=640x480:rate=15",
            "-frames:v",
            "2",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = driver {
        command.env("LIBVA_DRIVERS_PATH", dir);
    }
    output(&mut command, Backend::Vaapi);
    let Ok(child) = command.spawn() else {
        return false;
    };
    let mut child = Process(child);
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match child.0.try_wait() {
            Ok(Some(status)) => return status.success(),
            Err(_) => return false,
            _ if Instant::now() >= deadline => return false,
            _ => thread::sleep(Duration::from_millis(10)),
        }
    }
}
fn gpu_ffmpeg() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".cache/teams-linux/ffmpeg-vaapi/bin/ffmpeg");
    path.is_file().then_some(path)
}
fn decode_command(backend: DecodeBackend) -> Result<Command, &'static str> {
    match backend {
        DecodeBackend::Vaapi => {
            let ffmpeg = gpu_ffmpeg().ok_or("decoder de vídeo da GPU indisponível.")?;
            let VaapiState::Ready(driver) = vaapi_state() else {
                return Err("A GPU não expõe H.264.");
            };
            let mut command = Command::new(ffmpeg);
            if let Some(dir) = driver {
                command.env("LIBVA_DRIVERS_PATH", dir);
            }
            command.args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-hwaccel",
                "vaapi",
                "-hwaccel_device",
                "/dev/dri/renderD128",
                "-hwaccel_output_format",
                "vaapi",
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-max_pixels",
                "2073600",
                "-probesize",
                "32768",
                "-analyzeduration",
                "0",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-noautoscale",
                "-an",
                "-vf",
                "hwdownload,format=nv12,scale=640:480:force_original_aspect_ratio=decrease,pad=640:480:(ow-iw)/2:(oh-ih)/2,format=rgba",
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "pipe:1",
            ]);
            Ok(command)
        }
        DecodeBackend::Software => {
            let mut command = Command::new("ffmpeg");
            command.args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-max_pixels",
                "2073600",
                "-probesize",
                "32768",
                "-analyzeduration",
                "0",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-an",
                "-vf",
                "scale=640:480:force_original_aspect_ratio=decrease,pad=640:480:(ow-iw)/2:(oh-ih)/2",
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "pipe:1",
            ]);
            Ok(command)
        }
    }
}

#[derive(Default)]
struct AnnexB {
    bytes: Vec<u8>,
    unit: Vec<Vec<u8>>,
    unit_bytes: usize,
}
fn start_code(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..bytes.len().saturating_sub(2)).find_map(|i| {
        if bytes.get(i..i + 4) == Some(&[0, 0, 0, 1]) {
            Some((i, 4))
        } else if bytes.get(i..i + 3) == Some(&[0, 0, 1]) {
            Some((i, 3))
        } else {
            None
        }
    })
}
impl AnnexB {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<AccessUnit>, ()> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_BUFFER {
            return Err(());
        }
        self.bytes.extend_from_slice(bytes);
        let mut units = Vec::new();
        while let Some((first, prefix)) = start_code(&self.bytes, 0) {
            let Some((next, _)) = start_code(&self.bytes, first + prefix) else {
                break;
            };
            let nal = self.bytes[first + prefix..next].to_vec();
            self.bytes.drain(..next);
            if nal.is_empty() {
                continue;
            }
            if nal[0] & 31 == 9 {
                if !self.unit.is_empty() {
                    let nals = std::mem::take(&mut self.unit);
                    let keyframe = nals.iter().any(|n| n[0] & 31 == 5);
                    units.push(AccessUnit { nals, keyframe });
                    self.unit_bytes = 0;
                }
                continue;
            }
            if self.unit.len() >= 128 || self.unit_bytes.saturating_add(nal.len()) > MAX_BUFFER {
                return Err(());
            }
            self.unit_bytes += nal.len();
            self.unit.push(nal);
        }
        Ok(units)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn annex_b_handles_chunk_boundaries_and_bounds_untrusted_units() {
        let mut parser = AnnexB::default();
        let input = [
            0, 0, 0, 1, 9, 0xf0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 3, 0, 0, 0, 1,
            9, 0xf0, 0, 0, 1, 0x41, 4, 0, 0, 1, 9, 0xf0,
        ];
        let mut frames = Vec::new();
        for byte in input {
            frames.extend(parser.push(&[byte]).unwrap());
        }
        assert_eq!(frames.len(), 1);
        assert!(frames[0].keyframe);
        assert_eq!(frames[0].nals.len(), 3);
        assert!(parser.push(&vec![7; MAX_BUFFER + 1]).is_err());
    }
    #[test]
    fn synthetic_cpu_encoder_produces_real_h264_and_stops_without_camera_access() {
        let mut command = base(Backend::OpenH264);
        command.args(["-re", "-f", "lavfi", "-i", "testsrc2=size=640x480:rate=15"]);
        output(&mut command, Backend::OpenH264);
        let encoder = Encoder::spawn(command, Backend::OpenH264).unwrap();
        let unit = encoder.frames.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(unit.keyframe);
        assert!(unit.nals.iter().any(|n| n[0] & 31 == 7));
        encoder.stop();
    }
    #[test]
    fn widescreen_decode_preserves_aspect_instead_of_stretching() {
        let mut command = base(Backend::OpenH264);
        command.args([
            "-re",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:size=640x360:rate=15",
        ]);
        output(&mut command, Backend::OpenH264);
        let encoder = Encoder::spawn(command, Backend::OpenH264).unwrap();
        let decoder = Decoder::start().unwrap();
        let deadline = Instant::now() + Duration::from_secs(6);
        let frame = loop {
            assert!(
                Instant::now() < deadline,
                "decoder produced no widescreen frame"
            );
            if let Ok(unit) = encoder.frames.recv_timeout(Duration::from_millis(20)) {
                decoder.push(&unit);
            }
            if let Some(frame) = decoder.take_frame() {
                break frame;
            }
        };
        let iced::widget::image::Handle::Rgba {
            width,
            height,
            pixels,
            ..
        } = frame
        else {
            panic!("expected RGBA");
        };
        assert_eq!((width, height), (640, 480));
        let pixel = |x: usize, y: usize| &pixels[(y * 640 + x) * 4..(y * 640 + x) * 4 + 3];
        assert!(
            pixel(320, 5).iter().all(|v| *v < 10),
            "top border must be black"
        );
        assert!(
            pixel(320, 475).iter().all(|v| *v < 10),
            "bottom border must be black"
        );
        assert!(
            pixel(320, 240)[0] > 200 && pixel(320, 240)[1] < 20,
            "red center must remain visible"
        );
        encoder.stop();
    }
    #[test]
    fn gpu_h264_encode_and_decode_use_vaapi() {
        assert_eq!(Encoder::probe().unwrap(), Backend::Vaapi);
        let decoder = Decoder::start().unwrap();
        assert_eq!(decoder.backend, DecodeBackend::Vaapi);
    }
}
