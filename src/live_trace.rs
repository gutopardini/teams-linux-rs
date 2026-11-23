//! Opt-in bounded numeric tracing. No account or message content is accepted.
use std::{io::Write, sync::{OnceLock, atomic::{AtomicU64, Ordering}, mpsc}, time::{Instant, SystemTime, UNIX_EPOCH}};

struct Trace {
    started: Instant,
    sender: mpsc::SyncSender<Sample>,
    pending_input: AtomicU64,
    dropped: AtomicU64,
}
#[derive(serde::Serialize)]
struct Sample {
    at_us: u64,
    phase: &'static str,
    edge: &'static str,
    duration_us: u64,
    messages: usize,
    chats: usize,
    dropped: u64,
    #[serde(skip_serializing_if="Option::is_none")]
    call: Option<crate::call_diagnostics::Diagnostic>,
}
static TRACE: OnceLock<Trace> = OnceLock::new();
const MAX_BYTES: usize = 32 * 1024 * 1024;

pub fn enabled() -> bool { TRACE.get().is_some() }

pub fn enable() -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, DirBuilderExt};
    let root = std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| std::io::Error::other("No state directory"))?
        .join("teams-linux-rs/performance");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&root)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let path = root.join(format!("trace-{stamp}-{}.jsonl", std::process::id()));
    let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    let (sender, receiver) = mpsc::sync_channel::<Sample>(512);
    std::thread::Builder::new().name("teams-perf-writer".into()).spawn(move || {
        let mut file = std::io::BufWriter::new(file);
        // Hash the running inode, not the replaceable release path. This also
        // identifies an older process whose executable is marked deleted.
        let identity = serde_json::json!({
            "phase": "build_identity_v1", "schema_version": 1,
            "pid": std::process::id(), "package_version": env!("CARGO_PKG_VERSION"),
            "executable_sha256": executable_sha256(),
            "source_digest": env!("TEAMS_SOURCE_DIGEST"),
            "build_revision": env!("TEAMS_BUILD_REVISION"),
        });
        let mut header = serde_json::to_vec(&identity).unwrap_or_default();
        header.push(b'\n');
        if file.write_all(&header).and_then(|_| file.flush()).is_err() { return; }
        let mut bytes = header.len();
        while let Ok(sample) = receiver.recv() {
            let Ok(mut line) = serde_json::to_vec(&sample) else { continue; };
            line.push(b'\n');
            if bytes + line.len() > MAX_BYTES { let _ = file.flush(); break; }
            if file.write_all(&line).and_then(|_| file.flush()).is_err() { break; }
            bytes += line.len();
        }
    })?;
    let _ = TRACE.set(Trace { started: Instant::now(), sender, pending_input: AtomicU64::new(0), dropped: AtomicU64::new(0) });
    eprintln!("Performance trace: {}", path.display());
    record("trace_started", 0, 0, 0);
    record("avatar_diagnostics_version", 0, 4, 0);
    record("call_diagnostics_v1",0,0,0);
    record("call_lifecycle_v1",0,0,0);
    Ok(())
}

pub fn record(phase: &'static str, duration_us: u64, messages: usize, chats: usize) {
    record_edge(phase, "finish", duration_us, messages, chats);
}

pub fn start(phase: &'static str, messages: usize, chats: usize) {
    record_edge(phase, "start", 0, messages, chats);
}

fn record_edge(phase: &'static str, edge: &'static str, duration_us: u64, messages: usize, chats: usize) {
    let Some(trace) = TRACE.get() else { return; };
    let sample = Sample { at_us: trace.started.elapsed().as_micros() as u64,
        phase, edge, duration_us, messages, chats, dropped: trace.dropped.load(Ordering::Relaxed),call:None };
    if trace.sender.try_send(sample).is_err() { trace.dropped.fetch_add(1, Ordering::Relaxed); }
}

pub(crate) fn call_control(diagnostic: crate::call_diagnostics::ControlDiagnostic) {
    call_sample("call_control_rejected", crate::call_diagnostics::Diagnostic::Control(diagnostic));
}
pub(crate) fn call_acceptance(diagnostic: crate::call_diagnostics::AcceptanceDiagnostic) {
    call_sample("call_acceptance_shape", crate::call_diagnostics::Diagnostic::Acceptance(diagnostic));
}
pub(crate) fn call_remote_end(diagnostic: crate::call_diagnostics::RemoteEndDiagnostic) {
    call_sample("call_remote_end_details_v1", crate::call_diagnostics::Diagnostic::RemoteEnd(diagnostic));
}
pub(crate) fn call_renegotiation(diagnostic: crate::call_diagnostics::RenegotiationDiagnostic) {
    call_sample("call_media_renegotiation_shape_v1", crate::call_diagnostics::Diagnostic::Renegotiation(diagnostic));
}
pub(crate) fn call_media_path(diagnostic: crate::call_diagnostics::MediaPathDiagnostic) {
    call_sample("call_media_path_v1", crate::call_diagnostics::Diagnostic::MediaPath(diagnostic));
}
pub(crate) fn call_invite(diagnostic: crate::call_diagnostics::InviteDiagnostic) {
    call_sample("call_invite_response_shape_v1", crate::call_diagnostics::Diagnostic::Invite(diagnostic));
}
pub(crate) fn call_invite_failure(diagnostic: crate::call_diagnostics::InviteDiagnostic) {
    call_sample("call_invite_failure_shape_v1", crate::call_diagnostics::Diagnostic::Invite(diagnostic));
}
fn call_sample(phase: &'static str, diagnostic: crate::call_diagnostics::Diagnostic) {
    let Some(trace)=TRACE.get() else {return;};
    let sample=Sample {at_us:trace.started.elapsed().as_micros() as u64,
        phase,edge:"finish",duration_us:0,messages:0,chats:0,
        dropped:trace.dropped.load(Ordering::Relaxed),call:Some(diagnostic)};
    if trace.sender.try_send(sample).is_err(){trace.dropped.fetch_add(1,Ordering::Relaxed);}
}

pub fn span<T>(phase: &'static str, action: impl FnOnce() -> T) -> T {
    if !enabled() { return action(); }
    let started = Instant::now();
    start(phase, 0, 0);
    let result = action();
    record(phase, started.elapsed().as_micros() as u64, 0, 0);
    result
}

pub fn input(event: &iced::Event) {
    let phase = match event {
        iced::Event::Mouse(iced::mouse::Event::ButtonPressed(_)) => "mouse_press",
        iced::Event::Mouse(iced::mouse::Event::WheelScrolled { .. }) => "wheel",
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { .. }) => "key_press",
        iced::Event::Window(iced::window::Event::Focused) => "window_focus",
        _ => return,
    };
    if let Some(trace) = TRACE.get() {
        let at = (trace.started.elapsed().as_micros() as u64).max(1);
        let _ = trace.pending_input.compare_exchange(0, at, Ordering::Relaxed, Ordering::Relaxed);
        record(phase, 0, 0, 0);
    }
}

pub fn drawn() {
    if let Some(trace) = TRACE.get() {
        let input = trace.pending_input.swap(0, Ordering::Relaxed);
        if input != 0 {
            record("input_to_cpu_draw", (trace.started.elapsed().as_micros() as u64).saturating_sub(input), 0, 0);
        }
    }
}

pub fn update_phase(message: &crate::AppMessage) -> &'static str {
    use crate::AppMessage::*;
    match message {
        SelectConversation(_) => "update_select_chat", DraftChanged(_) | DraftAction(_) => "update_editor",
        HistoryScrolled(_) => "update_history_scroll", ChatListScrolled(..) => "update_list_scroll",
        SyncTick => "update_sync_tick", RefreshedMessages(..) => "update_refresh",
        MessagesLoaded(..) => "update_initial_page", OpenNewConversation => "update_new_chat",
        AvatarLoaded(..) => "update_avatar", _ => "update_other",
    }
}

/// Only reads the executable inode; neither process environment nor account data.
fn executable_sha256() -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open("/proc/self/exe").ok()?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer).ok()?;
        if count == 0 { break; }
        hash.update(&buffer[..count]);
    }
    Some(format!("{:x}", hash.finalize()))
}
