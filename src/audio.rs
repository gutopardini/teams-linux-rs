use serde_json::Value;
use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const SAMPLE_RATE: u32 = 48_000;
pub const LOCAL_ECHO_DURATION_MS: u32 = 1_000;
const MAX_DIAGNOSTIC_BYTES: usize = 4_096;
const MAX_DEVICE_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const DEVICE_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioDevice {
    pub key: String,
    pub label: String,
    pub is_default: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioDeviceCatalog {
    pub inputs: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
}

#[derive(Clone, Default)]
pub struct AudioCancellation(Arc<AtomicBool>);

impl AudioCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

struct CancelOnDrop {
    cancellation: AudioCancellation,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

/// Runs bounded PipeWire subprocess work outside the UI async executor.
pub async fn run_on_worker<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    let (complete, result) = tokio::sync::oneshot::channel();
    thread::Builder::new()
        .name("teams-linux-audio".into())
        .spawn(move || {
            let _ = complete.send(work());
        })
        .map_err(|_| "Não foi possível iniciar a tarefa de áudio em segundo plano.".to_owned())?;
    result
        .await
        .map_err(|_| "A tarefa de áudio em segundo plano foi interrompida.".to_owned())?
}

/// Runs cancellable PipeWire work on a dedicated thread. The UI owns a clone
/// of the token and cancels the current operation when settings close or the
/// app changes section.
pub async fn run_on_worker_cancellable<T, F>(
    cancellation: AudioCancellation,
    work: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(AudioCancellation) -> Result<T, String> + Send + 'static,
{
    let mut cancel_on_drop = CancelOnDrop {
        cancellation: cancellation.clone(),
        armed: true,
    };
    let (complete, result) = tokio::sync::oneshot::channel();
    thread::Builder::new()
        .name("teams-linux-audio".into())
        .spawn(move || {
            let _ = complete.send(work(cancellation));
        })
        .map_err(|_| "Não foi possível iniciar a tarefa de áudio em segundo plano.".to_owned())?;
    let result = result
        .await
        .map_err(|_| "A tarefa de áudio em segundo plano foi interrompida.".to_owned())?;
    cancel_on_drop.armed = false;
    result
}

pub fn enumerate_devices() -> Result<AudioDeviceCatalog, String> {
    enumerate_devices_with(command_output)
}

fn enumerate_devices_with<F>(run: F) -> Result<AudioDeviceCatalog, String>
where
    F: Fn(&str, &[&str]) -> Result<String, String> + Sync,
{
    let (dump, sources, sinks) = thread::scope(|scope| {
        let dump = scope.spawn(|| run("pw-dump", &[]));
        let sources = scope.spawn(|| run("wpctl", &["list", "audio", "sources"]));
        let sinks = scope.spawn(|| run("wpctl", &["list", "audio", "sinks"]));

        let dump = dump
            .join()
            .map_err(|_| "A leitura do catálogo PipeWire foi interrompida.".to_owned())??;
        let sources = sources
            .join()
            .map_err(|_| "A leitura das entradas de áudio foi interrompida.".to_owned())??;
        let sinks = sinks
            .join()
            .map_err(|_| "A leitura das saídas de áudio foi interrompida.".to_owned())??;
        Ok::<_, String>((dump, sources, sinks))
    })?;
    parse_device_catalog(&dump, &sources, &sinks)
}

fn command_output(program: &str, args: &[&str]) -> Result<String, String> {
    command_output_timeout(program, args, DEVICE_COMMAND_TIMEOUT)
}

pub(crate) fn command_output_timeout(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let output = run_bounded_command(
        program,
        args,
        timeout,
        MAX_DEVICE_CATALOG_BYTES,
    )
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "PipeWire não está disponível neste sistema.".to_owned()
        } else if error.to_string().contains("excedeu o tempo limite") {
            format!("A consulta de dispositivos excedeu o prazo em {program}.")
        } else {
            format!("Não foi possível consultar dispositivos de áudio com {program}.")
        }
    })?;
    if !output.status.success() {
        return Err("Não foi possível consultar os dispositivos de áudio.".into());
    }
    if output.stdout_exceeded_limit {
        return Err(format!(
            "A saída de {program} excedeu o limite seguro para dispositivos de áudio."
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| "A lista de dispositivos de áudio veio em formato inválido.".into())
}

#[derive(Debug)]
struct BoundedCommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stdout_exceeded_limit: bool,
}

/// Runs a small system utility without allowing it to block the UI worker
/// indefinitely or grow memory in proportion to arbitrary stdout/stderr.
fn run_bounded_command(
    program: &str,
    args: &[&str],
    timeout: Duration,
    max_stdout_bytes: usize,
) -> std::io::Result<BoundedCommandOutput> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other("stdout pipe unavailable"));
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other("stderr pipe unavailable"));
    };

    let stdout_reader = spawn_bounded_reader(stdout, max_stdout_bytes);
    let stderr_reader = spawn_bounded_reader(stderr, MAX_DIAGNOSTIC_BYTES);
    let status = wait_for_child(&mut child, timeout, program);
    let stdout = stdout_reader
        .join()
        .map_err(|_| std::io::Error::other("stdout reader stopped"))??;
    let _stderr = stderr_reader
        .join()
        .map_err(|_| std::io::Error::other("stderr reader stopped"))??;
    let status = status.map_err(std::io::Error::other)?;

    Ok(BoundedCommandOutput {
        status,
        stdout: stdout.bytes,
        stdout_exceeded_limit: stdout.exceeded_limit,
    })
}

struct BoundedRead {
    bytes: Vec<u8>,
    exceeded_limit: bool,
}

fn spawn_bounded_reader<R>(
    mut reader: R,
    limit: usize,
) -> thread::JoinHandle<std::io::Result<BoundedRead>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
        let mut chunk = [0_u8; 8 * 1024];
        let mut exceeded_limit = false;
        loop {
            let count = reader.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            let remaining = limit.saturating_sub(bytes.len());
            let retained = count.min(remaining);
            bytes.extend_from_slice(&chunk[..retained]);
            exceeded_limit |= retained < count;
        }
        Ok(BoundedRead {
            bytes,
            exceeded_limit,
        })
    })
}

fn parse_device_catalog(
    dump: &str,
    sources: &str,
    sinks: &str,
) -> Result<AudioDeviceCatalog, String> {
    let document: Value = serde_json::from_str(dump)
        .map_err(|_| "PipeWire retornou uma lista de dispositivos inválida.".to_owned())?;
    let Some(objects) = document.as_array() else {
        return Err("PipeWire retornou uma lista de dispositivos inválida.".into());
    };

    let mut descriptions = std::collections::HashMap::new();
    for object in objects {
        let props = object.pointer("/info/props").and_then(Value::as_object);
        let Some(props) = props else { continue };
        let Some(key) = props.get("node.name").and_then(Value::as_str) else {
            continue;
        };
        let class = props.get("media.class").and_then(Value::as_str);
        if !matches!(class, Some("Audio/Source" | "Audio/Sink")) {
            continue;
        }
        let label = ["node.description", "node.nick"]
            .into_iter()
            .filter_map(|name| props.get(name).and_then(Value::as_str))
            .find(|value| !value.trim().is_empty())
            .unwrap_or(key);
        descriptions.insert(key.to_owned(), label.to_owned());
    }

    Ok(AudioDeviceCatalog {
        inputs: parse_wpctl_devices(sources, "audio/source", &descriptions),
        outputs: parse_wpctl_devices(sinks, "audio/sink", &descriptions),
    })
}

fn parse_wpctl_devices(
    output: &str,
    expected_kind: &str,
    descriptions: &std::collections::HashMap<String, String>,
) -> Vec<AudioDevice> {
    let mut devices = Vec::new();
    for line in output.lines() {
        let mut columns = line.split_whitespace();
        let Some(_id) = columns.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(key) = columns.next() else { continue };
        let Some(kind) = columns.next() else { continue };
        if kind != expected_kind {
            continue;
        }
        let is_default = columns.any(|column| column == "*");
        if devices.iter().any(|device: &AudioDevice| device.key == key) {
            continue;
        }
        devices.push(AudioDevice {
            key: key.to_owned(),
            label: descriptions
                .get(key)
                .cloned()
                .unwrap_or_else(|| key.to_owned()),
            is_default,
        });
    }
    devices.sort_by(|left, right| {
        right
            .is_default
            .cmp(&left.is_default)
            .then_with(|| left.label.to_lowercase().cmp(&right.label.to_lowercase()))
    });
    devices.truncate(64);
    devices
}

struct HeadsetCard {
    id: u32,
    node_suffix: String,
    active_profile: String,
    profiles: Vec<String>,
}

struct CallPlan {
    input: String,
    output: String,
    switch_card: Option<u32>,
    switch_to: Option<String>,
    restore_profile: Option<String>,
}

pub(crate) struct PreparedCallAudio {
    pub input: Option<String>,
    pub output: Option<String>,
    pub restore: Option<HeadsetRestore>,
}

/// Restores the previous Bluetooth profile when the call audio worker exits.
pub(crate) struct HeadsetRestore {
    card: u32,
    profile: String,
}

impl Drop for HeadsetRestore {
    fn drop(&mut self) {
        if safe_profile(&self.profile) {
            let _ = set_card_profile(self.card, &self.profile);
        }
    }
}

/// Resolve explicit endpoints first, otherwise follow WirePlumber's defaults.
/// HFP is required only when the selected input is the Bluetooth microphone.
pub(crate) fn prepare_call_devices(input: Option<&str>, output: Option<&str>) -> Result<PreparedCallAudio, ()> {
    let saved_input = input.unwrap_or("");
    let saved_output = output.unwrap_or("");
    let dump = command_output("pw-dump", &[]).map_err(|_| ())?;
    let sources = command_output("wpctl", &["list", "audio", "sources"]).map_err(|_| ())?;
    let sinks = command_output("wpctl", &["list", "audio", "sinks"]).map_err(|_| ())?;
    let catalog = parse_device_catalog(&dump, &sources, &sinks).map_err(|_| ())?;
    let cards = parse_headset_cards(&dump);
    let plan = plan_call_audio(saved_input, saved_output, &catalog, &cards);
    let mut prepared = PreparedCallAudio {
        input: nonempty(&plan.input),
        output: nonempty(&plan.output),
        restore: None,
    };
    if let (Some(card), Some(profile)) = (plan.switch_card, plan.switch_to.as_deref()) {
        if !set_card_profile(card, profile) { return Err(()); }
        // Own rollback immediately: waiting for new nodes may fail or time out.
        prepared.restore = plan.restore_profile.map(|profile| HeadsetRestore { card, profile });
        let (microphone, speaker) = wait_for_voice_nodes(card, profile).ok_or(())?;
        prepared.input = Some(microphone);
        if cards.iter().find(|c| c.id == card).is_some_and(|c| headset_matches(&plan.output, c)) {
            prepared.output = Some(speaker);
        }
    }
    crate::live_trace::record(
        "call_audio_route",
        u64::from(prepared.restore.is_some()),
        usize::from(prepared.output.as_deref().is_some_and(is_headset_node)),
        usize::from(prepared.input.as_deref().is_some_and(is_headset_node)),
    );
    Ok(prepared)
}

fn plan_call_audio(
    saved_input: &str,
    saved_output: &str,
    catalog: &AudioDeviceCatalog,
    cards: &[HeadsetCard],
) -> CallPlan {
    let mut plan = CallPlan {
        input: call_endpoint(saved_input, &catalog.inputs),
        output: call_endpoint(saved_output, &catalog.outputs),
        switch_card: None,
        switch_to: None,
        restore_profile: None,
    };
    // An A2DP speaker with the built-in/USB microphone does not need HFP.
    // Never switch the first connected card just because a headset exists.
    let Some(card) = cards.iter().find(|card| plan.input.starts_with("bluez_input.")
        && headset_matches(&plan.input, card)) else { return plan; };
    let Some(profile) = voice_profile_name(card) else {
        return plan;
    };
    if card.active_profile != profile {
        plan.switch_card = Some(card.id);
        plan.switch_to = Some(profile.to_owned());
        plan.restore_profile = Some(card.active_profile.clone());
    }
    plan
}

fn call_endpoint(saved: &str, devices: &[AudioDevice]) -> String {
    if !saved.is_empty() { return saved.to_owned(); }
    devices.iter().find(|device| device.is_default)
        .map(|device| device.key.clone()).unwrap_or_default()
}

fn headset_matches(key: &str, card: &HeadsetCard) -> bool {
    let suffix = key.strip_prefix("bluez_input.").or_else(|| key.strip_prefix("bluez_output."));
    !card.node_suffix.is_empty() && suffix.is_some_and(|s| s == card.node_suffix
        || s.strip_prefix(&card.node_suffix).is_some_and(|tail| tail.starts_with('.')))
}

fn is_headset_node(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("bluez_") || key.contains("headset") || key.contains("headphone")
}

fn voice_profile_name(card: &HeadsetCard) -> Option<&str> {
    card.profiles
        .iter()
        .find(|profile| profile.as_str() == "headset-head-unit")
        .or_else(|| {
            card.profiles
                .iter()
                .find(|profile| profile.as_str() == "headset-head-unit-cvsd")
        })
        .map(String::as_str)
}

fn parse_headset_cards(dump: &str) -> Vec<HeadsetCard> {
    let Ok(Value::Array(objects)) = serde_json::from_str::<Value>(dump) else {
        return Vec::new();
    };
    let mut cards = Vec::new();
    for object in &objects {
        if object.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Device") {
            continue;
        }
        let Some(props) = object.pointer("/info/props").and_then(Value::as_object) else {
            continue;
        };
        if props.get("device.api").and_then(Value::as_str) != Some("bluez5") {
            continue;
        }
        if props.get("api.bluez5.connection").and_then(Value::as_str) == Some("disconnected") {
            continue;
        }
        let Some(id) = object
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|id| u32::try_from(id).ok())
            .filter(|id| *id > 0)
        else {
            continue;
        };
        let profiles = object
            .pointer("/info/params/EnumProfile")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("name").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let active_profile = object
            .pointer("/info/params/Profile")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let Some(node_suffix) = props.get("device.name").and_then(Value::as_str)
            .and_then(|name| name.strip_prefix("bluez_card.")) else { continue; };
        cards.push(HeadsetCard {
            id,
            node_suffix: node_suffix.to_owned(),
            active_profile,
            profiles,
        });
    }
    cards
}

fn wait_for_voice_nodes(card: u32, profile: &str) -> Option<(String, String)> {
    for _ in 0..12 {
        thread::sleep(Duration::from_millis(200));
        let Ok(dump) = command_output("pw-dump", &[]) else { continue; };
        if let Some(nodes) = voice_nodes(&dump, card, profile) { return Some(nodes); }
    }
    None
}

fn voice_nodes(dump: &str, card: u32, profile: &str) -> Option<(String, String)> {
    let objects: Vec<Value> = serde_json::from_str(dump).ok()?;
    let active = objects.iter().find(|o| o.get("id").and_then(Value::as_u64) == Some(card as u64)
        && o.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Device"))?;
    if active.pointer("/info/params/Profile/0/name").and_then(Value::as_str) != Some(profile) { return None; }
    let find = |class| {
        let mut nodes = objects.iter().filter_map(|o| {
            if o.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Node") { return None; }
            let props = o.pointer("/info/props")?;
            let id = props.get("device.id")?;
            let id = id.as_u64().or_else(|| id.as_str()?.parse().ok())?;
            if id != card as u64 || props.get("media.class")?.as_str()? != class { return None; }
            nonempty(props.get("node.name")?.as_str()?)
        });
        let first = nodes.next()?;
        if nodes.next().is_some() { return None; }
        Some(first)
    };
    Some((find("Audio/Source")?, find("Audio/Sink")?))
}

fn profile_index(dump: &str, card: u32, profile: &str) -> Option<u32> {
    let objects: Vec<Value> = serde_json::from_str(dump).ok()?;
    let device = objects.iter().find(|o| o.get("id").and_then(Value::as_u64) == Some(card as u64)
        && o.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Device"))?;
    device.pointer("/info/params/EnumProfile")?.as_array()?.iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(profile)
            && p.get("available").and_then(Value::as_str) != Some("no"))?
        .get("index")?.as_u64().and_then(|i| u32::try_from(i).ok())
}

fn set_card_profile(card: u32, profile: &str) -> bool {
    if !safe_profile(profile) { return false; }
    let Ok(dump) = command_output("pw-dump", &[]) else { return false; };
    let Some(index) = profile_index(&dump, card, profile) else { return false; };
    run_bounded_command("wpctl", &["set-profile", &card.to_string(), &index.to_string()], Duration::from_secs(5), 4_096)
        .map(|output| output.status.success()).unwrap_or(false)
}

fn safe_profile(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
}

fn nonempty(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        None
    } else {
        Some(value.to_owned())
    }
}

pub fn record_local_echo_clip_cancellable(
    target: Option<&str>,
    cancellation: AudioCancellation,
) -> Result<(Vec<u8>, f32), String> {
    let pcm =
        capture_microphone_pcm(target, LOCAL_ECHO_DURATION_MS, Some(&cancellation))?;
    let level = pcm_rms_dbfs(&pcm)?;
    Ok((pcm, level))
}

pub fn play_local_echo_clip_cancellable(
    target: Option<&str>,
    pcm: &[u8],
    cancellation: AudioCancellation,
) -> Result<(), String> {
    validate_local_echo_pcm(pcm)?;
    play_pcm(
        target,
        pcm,
        AudioOperation::EchoPlayback,
        Some(&cancellation),
    )
}

fn validate_local_echo_pcm(pcm: &[u8]) -> Result<(), String> {
    let max_bytes = SAMPLE_RATE as usize * 2 * LOCAL_ECHO_DURATION_MS as usize / 1_000;
    if pcm.is_empty() || pcm.len() % 2 != 0 || pcm.len() > max_bytes {
        Err("O trecho de áudio local não é válido.".into())
    } else {
        Ok(())
    }
}

fn capture_microphone_pcm(
    target: Option<&str>,
    duration_ms: u32,
    cancellation: Option<&AudioCancellation>,
) -> Result<Vec<u8>, String> {
    if cancellation.is_some_and(AudioCancellation::is_cancelled) {
        return Err("O teste de áudio foi cancelado.".into());
    }
    let milliseconds = duration_ms.clamp(1, LOCAL_ECHO_DURATION_MS);
    let samples = SAMPLE_RATE * milliseconds / 1_000;
    let mut command = Command::new("pw-record");
    if let Some(target) = target {
        command.args(["--target", target]);
    }
    let mut child = command
        .args([
            "--raw",
            "--media-category",
            "Capture",
            "--media-role",
            "Communication",
            "--rate",
            "48000",
            "--channels",
            "1",
            "--format",
            "s16",
            "--sample-count",
        ])
        .arg(samples.to_string())
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| audio_command_error("pw-record", error))?;
    let stderr_reader = take_stderr_reader(&mut child);
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = finish_stderr_reader(stderr_reader);
        return Err("Não foi possível ler a captura do microfone.".into());
    };
    let max_bytes = (samples as usize).saturating_mul(2);
    let reader = thread::spawn(move || {
        let mut bytes = Vec::with_capacity(max_bytes);
        stdout
            .take(max_bytes as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let status = wait_for_child_cancellable(
        &mut child,
        Duration::from_millis(u64::from(milliseconds) + 2_500),
        "pw-record",
        cancellation,
    );
    let diagnostic = finish_stderr_reader(stderr_reader);
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            let _ = reader.join();
            return Err(error);
        }
    };
    let pcm = reader
        .join()
        .map_err(|_| "A captura de áudio foi interrompida.".to_owned())?
        .map_err(|_| "Não foi possível ler a captura do microfone.".to_owned())?;
    if !status.success() {
        return Err(pipewire_process_error(AudioOperation::Capture, &diagnostic));
    }
    if pcm.len() < max_bytes {
        return Err("A captura terminou antes de completar o trecho de áudio.".into());
    }
    Ok(pcm)
}

pub fn play_test_tone_cancellable(
    target: Option<&str>,
    cancellation: AudioCancellation,
) -> Result<(), String> {
    play_pcm(
        target,
        &test_tone_pcm(),
        AudioOperation::Playback,
        Some(&cancellation),
    )
}

fn play_pcm(
    target: Option<&str>,
    pcm: &[u8],
    operation: AudioOperation,
    cancellation: Option<&AudioCancellation>,
) -> Result<(), String> {
    if cancellation.is_some_and(AudioCancellation::is_cancelled) {
        return Err("O teste de áudio foi cancelado.".into());
    }
    let mut command = Command::new("pw-play");
    if let Some(target) = target {
        command.args(["--target", target]);
    }
    let mut child = command
        .args([
            "--raw",
            "--media-category",
            "Playback",
            "--media-role",
            "Communication",
            "--rate",
            "48000",
            "--channels",
            "1",
            "--format",
            "s16",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| audio_command_error("pw-play", error))?;
    let stderr_reader = take_stderr_reader(&mut child);

    let Some(stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = finish_stderr_reader(stderr_reader);
        return Err(match operation {
            AudioOperation::EchoPlayback => {
                "Não foi possível iniciar a reprodução do trecho capturado.".into()
            }
            _ => "Não foi possível iniciar o teste do alto-falante.".into(),
        });
    };
    let (status, write_failed) = write_to_child_and_wait(
        &mut child,
        stdin,
        pcm,
        Duration::from_secs(5),
        "pw-play",
        cancellation,
    );
    let diagnostic = finish_stderr_reader(stderr_reader);
    let status = status?;
    if !status.success() {
        return Err(pipewire_process_error(operation, &diagnostic));
    }
    if write_failed {
        return Err(match operation {
            AudioOperation::EchoPlayback => {
                "Não foi possível reproduzir o trecho capturado no PipeWire.".into()
            }
            _ => "Não foi possível enviar o tom de teste ao PipeWire.".into(),
        });
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum AudioOperation {
    Capture,
    Playback,
    EchoPlayback,
}

fn take_stderr_reader(
    child: &mut Child,
) -> Option<thread::JoinHandle<std::io::Result<BoundedRead>>> {
    child
        .stderr
        .take()
        .map(|stderr| spawn_bounded_reader(stderr, MAX_DIAGNOSTIC_BYTES))
}

fn finish_stderr_reader(
    reader: Option<thread::JoinHandle<std::io::Result<BoundedRead>>>,
) -> String {
    reader
        .and_then(|reader| reader.join().ok())
        .and_then(Result::ok)
        .map(|read| String::from_utf8_lossy(&read.bytes).into_owned())
        .unwrap_or_default()
}

fn pipewire_process_error(operation: AudioOperation, diagnostic: &str) -> String {
    let diagnostic = diagnostic.to_ascii_lowercase();
    if ["permission denied", "not authorized", "access denied"]
        .iter()
        .any(|phrase| diagnostic.contains(phrase))
    {
        return match operation {
            AudioOperation::Capture => {
                "O sistema negou acesso ao microfone. Verifique a permissão de áudio do aplicativo."
            }
            AudioOperation::Playback | AudioOperation::EchoPlayback => {
                "O sistema negou acesso à saída de áudio. Verifique as permissões do PipeWire."
            }
        }
        .into();
    }
    if ["no such device", "unknown node", "target not found", "no matching node"]
        .iter()
        .any(|phrase| diagnostic.contains(phrase))
    {
        return "O dispositivo selecionado não está mais disponível. Atualize a lista ou escolha Padrão do sistema.".into();
    }
    if diagnostic.contains("device or resource busy") || diagnostic.contains("device busy") {
        return "O dispositivo de áudio está ocupado. Tente novamente em instantes.".into();
    }
    match operation {
        AudioOperation::Capture => "Não foi possível capturar áudio do microfone selecionado.".into(),
        AudioOperation::Playback => "Não foi possível reproduzir o tom no dispositivo selecionado.".into(),
        AudioOperation::EchoPlayback => {
            "Não foi possível reproduzir o trecho capturado no dispositivo selecionado.".into()
        }
    }
}

fn wait_for_child(
    child: &mut Child,
    timeout: Duration,
    program: &str,
) -> Result<ExitStatus, String> {
    wait_for_child_cancellable(child, timeout, program, None)
}

fn write_to_child_and_wait(
    child: &mut Child,
    mut stdin: std::process::ChildStdin,
    bytes: &[u8],
    timeout: Duration,
    program: &str,
    cancellation: Option<&AudioCancellation>,
) -> (Result<ExitStatus, String>, bool) {
    thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.write_all(bytes));
        let status = wait_for_child_cancellable(child, timeout, program, cancellation);
        let write_failed = writer
            .join()
            .map(|result| result.is_err())
            .unwrap_or(true);
        (status, write_failed)
    })
}

fn wait_for_child_cancellable(
    child: &mut Child,
    timeout: Duration,
    program: &str,
    cancellation: Option<&AudioCancellation>,
) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancellation.is_some_and(AudioCancellation::is_cancelled) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("O teste de áudio foi cancelado.".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Não foi possível concluir {program}."));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("A operação com {program} excedeu o tempo limite."));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn test_tone_pcm() -> Vec<u8> {
    let mut pcm = Vec::with_capacity((SAMPLE_RATE / 4 * 2) as usize);
    for sample in 0..SAMPLE_RATE / 4 {
        let phase = std::f64::consts::TAU * 440.0 * f64::from(sample) / f64::from(SAMPLE_RATE);
        let value = (phase.sin() * f64::from(i16::MAX) * 0.12).round() as i16;
        pcm.extend_from_slice(&value.to_le_bytes());
    }
    pcm
}

fn audio_command_error(program: &str, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!("O utilitário {program} do PipeWire não está instalado.")
    } else {
        format!("Não foi possível iniciar {program} para o teste de áudio.")
    }
}

fn pcm_rms_dbfs(pcm: &[u8]) -> Result<f32, String> {
    let samples = pcm.chunks_exact(2);
    let count = samples.len();
    if count == 0 {
        return Err("O microfone não retornou amostras de áudio.".into());
    }
    let sum_squares: f64 = samples
        .map(|sample| {
            let value = i16::from_le_bytes([sample[0], sample[1]]) as f64;
            value * value
        })
        .sum();
    let rms = (sum_squares / count as f64).sqrt();
    let dbfs = if rms == 0.0 {
        -90.0
    } else {
        (20.0 * (rms / f64::from(i16::MAX)).log10()).clamp(-90.0, 0.0) as f32
    };
    Ok(dbfs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn device_catalog_maps_pipewire_descriptions_and_default_devices() {
        let dump = json!([
            {
                "id": 41,
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "mic.internal",
                    "node.description": "Microfone interno"
                }}
            },
            {
                "id": 42,
                "info": {"props": {
                    "media.class": "Audio/Sink",
                    "node.name": "speaker.internal",
                    "node.description": "Alto-falante"
                }}
            },
            {
                "id": 43,
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "speaker.monitor",
                    "node.description": "Monitor"
                }}
            }
        ])
        .to_string();
        let sources = "52\tmic.internal\taudio/source\t*\n53\tspeaker.monitor\taudio/source\t\n";
        let sinks = "54\tspeaker.internal\taudio/sink\t*\n";

        let devices = parse_device_catalog(&dump, sources, sinks).unwrap();

        assert_eq!(devices.inputs.len(), 2);
        assert_eq!(devices.inputs[0].key, "mic.internal");
        assert_eq!(devices.inputs[0].label, "Microfone interno");
        assert!(devices.inputs[0].is_default);
        assert!(!devices.inputs[1].is_default);
        assert_eq!(devices.outputs[0].key, "speaker.internal");
        assert_eq!(devices.outputs[0].label, "Alto-falante");
        assert!(devices.outputs[0].is_default);
    }

    #[test]
    fn explicit_builtin_devices_are_not_overridden_by_a_connected_headset() {
        let inputs = vec![AudioDevice {
            key: "alsa_input.HiFi__Mic1__source".into(),
            label: "Microfone".into(),
            is_default: true,
        }];
        let outputs = vec![
            AudioDevice {
                key: "alsa_output.HiFi__Speaker__sink".into(),
                label: "Alto-falante".into(),
                is_default: true,
            },
            AudioDevice {
                key: "bluez_output.headset".into(),
                label: "Fone".into(),
                is_default: false,
            },
        ];
        let catalog = AudioDeviceCatalog { inputs, outputs };
        let cards = vec![HeadsetCard {
            id: 94,
            node_suffix: "headset".into(),
            active_profile: "a2dp-sink".into(),
            profiles: vec![
                "a2dp-sink".into(),
                "headset-head-unit-cvsd".into(),
                "headset-head-unit".into(),
            ],
        }];
        let plan = plan_call_audio(
            "alsa_input.HiFi__Mic1__source",
            "alsa_output.HiFi__Speaker__sink",
            &catalog,
            &cards,
        );
        assert_eq!(plan.output, "alsa_output.HiFi__Speaker__sink");
        assert_eq!(plan.input, "alsa_input.HiFi__Mic1__source");
        assert!(plan.switch_card.is_none());
        assert!(plan.switch_to.is_none());
        assert!(plan.restore_profile.is_none());
        let automatic = plan_call_audio("", "", &catalog, &cards);
        assert_eq!(automatic.output, "alsa_output.HiFi__Speaker__sink");
        assert!(automatic.switch_card.is_none());
    }

    #[test]
    fn explicit_display_output_stays_put_and_an_active_headset_is_not_switched_again() {
        let outputs = vec![AudioDevice {
            key: "alsa_output.HiFi__HDMI1__sink".into(),
            label: "Monitor".into(),
            is_default: false,
        }];
        let catalog = AudioDeviceCatalog {
            inputs: Vec::new(),
            outputs,
        };
        let cards = vec![HeadsetCard {
            id: 7,
            node_suffix: "headset".into(),
            active_profile: "a2dp-sink".into(),
            profiles: vec!["headset-head-unit".into()],
        }];
        let display = plan_call_audio("", "alsa_output.HiFi__HDMI1__sink", &catalog, &cards);
        assert_eq!(display.output, "alsa_output.HiFi__HDMI1__sink");
        assert!(display.switch_card.is_none());

        let headset = AudioDeviceCatalog {
            inputs: vec![AudioDevice {
                key: "bluez_input.headset".into(),
                label: "Fone".into(),
                is_default: true,
            }],
            outputs: vec![AudioDevice {
                key: "bluez_output.headset".into(),
                label: "Fone".into(),
                is_default: true,
            }],
        };
        let active = plan_call_audio(
            "",
            "",
            &headset,
            &[HeadsetCard {
                id: 7,
                node_suffix: "headset".into(),
                active_profile: "headset-head-unit".into(),
                profiles: vec!["headset-head-unit".into()],
            }],
        );
        assert_eq!(active.input, "bluez_input.headset");
        assert_eq!(active.output, "bluez_output.headset");
        assert!(active.switch_card.is_none());
    }

    #[test]
    fn bluetooth_output_follows_system_default_without_stealing_the_microphone() {
        let catalog = AudioDeviceCatalog {
            inputs: vec![AudioDevice {key:"usb-mic".into(),is_default:true,..Default::default()}],
            outputs: vec![AudioDevice {key:"bluez_output.headset.1".into(),is_default:true,..Default::default()}],
        };
        let card = HeadsetCard {id:7,node_suffix:"headset".into(),active_profile:"a2dp-sink".into(),profiles:vec!["headset-head-unit".into()]};
        let automatic = plan_call_audio("", "", &catalog, &[card]);
        assert_eq!(automatic.input, "usb-mic");
        assert_eq!(automatic.output, "bluez_output.headset.1");
        assert!(automatic.switch_card.is_none());
        assert_eq!(call_endpoint("saved-explicit-device", &catalog.outputs), "saved-explicit-device");
    }

    #[test]
    fn bluetooth_microphone_selects_its_own_card_and_keeps_explicit_usb_output() {
        let cards = ["first", "selected"].map(|name| HeadsetCard {
            id:if name=="first" {7} else {8},node_suffix:name.into(),
            active_profile:"a2dp-sink".into(),profiles:vec!["headset-head-unit".into()],
        });
        let plan = plan_call_audio("bluez_input.selected.0", "usb-speaker", &AudioDeviceCatalog::default(), &cards);
        assert_eq!(plan.switch_card, Some(8));
        assert_eq!(plan.output, "usb-speaker");
        assert_eq!(plan.switch_to.as_deref(), Some("headset-head-unit"));
    }

    #[test]
    fn profile_switch_resolves_numeric_index_and_waits_for_both_nodes_on_the_same_card() {
        let mut objects = serde_json::json!([
            {"id":7,"type":"PipeWire:Interface:Device","info":{"params":{
                "Profile":[{"name":"headset-head-unit"}],
                "EnumProfile":[{"name":"headset-head-unit","index":196865,"available":"yes"}]
            }}},
            {"id":80,"type":"PipeWire:Interface:Node","info":{"props":{"device.id":8,"media.class":"Audio/Source","node.name":"wrong-microphone"}}},
            {"id":81,"type":"PipeWire:Interface:Node","info":{"props":{"device.id":"7","media.class":"Audio/Source","node.name":"selected-microphone"}}},
            {"id":82,"type":"PipeWire:Interface:Node","info":{"props":{"device.id":7,"media.class":"Audio/Sink","node.name":"selected-speaker"}}}
        ]);
        let dump = objects.to_string();
        assert_eq!(profile_index(&dump, 7, "headset-head-unit"), Some(196865));
        assert_eq!(profile_index(&dump, 8, "headset-head-unit"), None);
        assert_eq!(voice_nodes(&dump, 7, "headset-head-unit"), Some(("selected-microphone".into(),"selected-speaker".into())));
        assert_eq!(voice_nodes(&dump, 7, "a2dp-sink"), None);
        objects[0]["info"]["params"]["EnumProfile"][0]["available"] = serde_json::json!("no");
        assert_eq!(profile_index(&objects.to_string(), 7, "headset-head-unit"), None);
        objects.as_array_mut().unwrap().pop();
        assert_eq!(voice_nodes(&objects.to_string(), 7, "headset-head-unit"), None);
    }

    #[test]
    fn headset_cards_come_from_the_pipewire_device_profile() {
        let dump = serde_json::json!([
            {
                "id": 94,
                "type": "PipeWire:Interface:Device",
                "info": {
                    "props": {"device.api": "bluez5", "api.bluez5.connection": "connected", "device.name": "bluez_card.headset"},
                    "params": {
                        "EnumProfile": [{"name": "a2dp-sink"}, {"name": "headset-head-unit"}],
                        "Profile": [{"name": "a2dp-sink"}]
                    }
                }
            },
            {
                "id": 3,
                "type": "PipeWire:Interface:Device",
                "info": {
                    "props": {"device.api": "bluez5", "api.bluez5.connection": "disconnected"},
                    "params": {"EnumProfile": [{"name": "headset-head-unit"}], "Profile": [{"name": "off"}]}
                }
            }
        ])
        .to_string();
        let cards = parse_headset_cards(&dump);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id, 94);
        assert_eq!(cards[0].active_profile, "a2dp-sink");
        assert_eq!(voice_profile_name(&cards[0]), Some("headset-head-unit"));

    }

    #[test]
    fn device_enumeration_runs_bounded_queries_concurrently() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded_calls = std::sync::Arc::clone(&calls);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
        let worker_release_rx = std::sync::Arc::clone(&release_rx);
        let worker = thread::spawn(move || {
            enumerate_devices_with(move |program, args| {
                recorded_calls.lock().unwrap().push((
                    program.to_owned(),
                    args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
                ));
                started_tx.send(program.to_owned()).unwrap();
                worker_release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                match (program, args.last().copied()) {
                    ("pw-dump", _) => Ok("[]".into()),
                    ("wpctl", Some("sources" | "sinks")) => Ok(String::new()),
                    _ => Err("query inesperada".into()),
                }
            })
        });
        let mut started_concurrently = 0;
        for _ in 0..3 {
            if started_rx.recv_timeout(Duration::from_secs(1)).is_err() {
                break;
            }
            started_concurrently += 1;
        }
        for _ in 0..3 {
            release_tx.send(()).unwrap();
        }
        let devices = worker.join().unwrap().unwrap();

        assert_eq!(started_concurrently, 3);
        assert!(devices.inputs.is_empty());
        assert!(devices.outputs.is_empty());
        let mut calls = calls.lock().unwrap().clone();
        calls.sort();
        assert_eq!(
            calls,
            vec![
                ("pw-dump".into(), vec![]),
                ("wpctl".into(), vec!["list".into(), "audio".into(), "sinks".into()]),
                (
                    "wpctl".into(),
                    vec!["list".into(), "audio".into(), "sources".into()]
                ),
            ]
        );
    }

    #[test]
    fn audio_subprocess_work_runs_outside_the_calling_thread() {
        let caller = thread::current().id();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let worker = runtime
            .block_on(run_on_worker(|| Ok(thread::current().id())))
            .unwrap();
        assert_ne!(caller, worker);
    }

    #[test]
    fn pcm_meter_handles_silence_and_full_scale_samples() {
        assert_eq!(pcm_rms_dbfs(&[0, 0, 0, 0]).unwrap(), -90.0);
        let full_scale = pcm_rms_dbfs(&[0xff, 0x7f, 0xff, 0x7f]).unwrap();
        assert!(full_scale > -0.01 && full_scale <= 0.0);
    }

    #[test]
    fn malformed_or_empty_pipewire_data_is_rejected_without_io() {
        assert!(parse_device_catalog("{}", "", "").is_err());
        assert!(pcm_rms_dbfs(&[]).is_err());
    }

    #[test]
    fn device_utility_output_is_bounded_and_completed_without_audio_io() {
        let output = run_bounded_command(
            "sh",
            &["-c", "printf 'synthetic catalog'"],
            Duration::from_secs(1),
            64,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"synthetic catalog");
        assert!(!output.stdout_exceeded_limit);

        let oversized = run_bounded_command(
            "sh",
            &["-c", "printf '123456789'"],
            Duration::from_secs(1),
            4,
        )
        .unwrap();
        assert!(oversized.status.success());
        assert!(oversized.stdout_exceeded_limit);
        assert_eq!(oversized.stdout, b"1234");
    }

    #[test]
    fn device_utility_timeout_kills_the_child_and_drains_its_pipes() {
        let result = run_bounded_command("sleep", &["2"], Duration::from_millis(30), 64);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("excedeu o tempo limite"));
    }

    #[test]
    fn audio_cancellation_kills_an_active_child_and_prevents_a_new_playback() {
        let cancellation = AudioCancellation::new();
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            let mut child = Command::new("sleep").arg("5").spawn().unwrap();
            wait_for_child_cancellable(
                &mut child,
                Duration::from_secs(10),
                "sleep",
                Some(&worker_cancellation),
            )
        });
        thread::sleep(Duration::from_millis(40));
        cancellation.cancel();
        assert!(worker
            .join()
            .unwrap()
            .unwrap_err()
            .contains("cancelado"));

        let already_cancelled = AudioCancellation::new();
        already_cancelled.cancel();
        assert!(play_test_tone_cancellable(None, already_cancelled)
            .unwrap_err()
            .contains("cancelado"));
    }

    #[test]
    fn audio_cancellation_unblocks_a_writer_when_playback_child_is_stopped() {
        let mut child = Command::new("sleep")
            .arg("5")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let cancellation = AudioCancellation::new();
        let worker_cancellation = cancellation.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            worker_cancellation.cancel();
        });
        let pcm = vec![0_u8; 1024 * 1024];

        let (status, write_failed) = write_to_child_and_wait(
            &mut child,
            stdin,
            &pcm,
            Duration::from_secs(10),
            "sleep",
            Some(&cancellation),
        );

        canceller.join().unwrap();
        assert!(status.unwrap_err().contains("cancelado"));
        assert!(write_failed);
    }

    #[test]
    fn dropping_audio_worker_future_cancels_its_background_worker() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let cancellation = AudioCancellation::new();
        let worker_cancellation = cancellation.clone();
        let (stopped, stopped_rx) = std::sync::mpsc::channel();

        runtime.block_on(async {
            let worker = tokio::spawn(run_on_worker_cancellable(
                worker_cancellation,
                move |cancellation| {
                    while !cancellation.is_cancelled() {
                        thread::sleep(Duration::from_millis(5));
                    }
                    let _ = stopped.send(());
                    Ok(())
                },
            ));
            tokio::task::yield_now().await;
            worker.abort();
            let _ = worker.await;
        });

        assert!(cancellation.is_cancelled());
        assert!(stopped_rx.recv_timeout(Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn diagnostic_reader_keeps_a_cap_while_draining_excess_bytes() {
        let diagnostic = vec![b'x'; MAX_DIAGNOSTIC_BYTES + 16 * 1024];
        let result = spawn_bounded_reader(std::io::Cursor::new(diagnostic), MAX_DIAGNOSTIC_BYTES)
            .join()
            .unwrap()
            .unwrap();

        assert_eq!(result.bytes.len(), MAX_DIAGNOSTIC_BYTES);
        assert!(result.exceeded_limit);
        assert!(result.bytes.iter().all(|byte| *byte == b'x'));
    }

    #[test]
    fn pipewire_failures_show_safe_permission_device_and_busy_messages() {
        let permission = pipewire_process_error(
            AudioOperation::Capture,
            "Permission denied for node private-headset-name",
        );
        assert!(permission.contains("negou acesso ao microfone"));
        assert!(!permission.contains("private-headset-name"));

        assert!(pipewire_process_error(AudioOperation::Playback, "No such device")
            .contains("Atualize a lista"));
        assert!(pipewire_process_error(AudioOperation::Capture, "Device busy")
            .contains("ocupado"));
        assert!(pipewire_process_error(AudioOperation::Playback, "some other failure")
            .contains("reproduzir o tom"));
    }

    #[test]
    fn speaker_test_tone_is_short_and_bounded() {
        let pcm = test_tone_pcm();
        assert_eq!(pcm.len(), (SAMPLE_RATE / 4 * 2) as usize);
        assert!(pcm
            .chunks_exact(2)
            .map(|sample| i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs())
            .max()
            .is_some_and(|peak| peak <= (i16::MAX as f32 * 0.121) as u16));
    }

    #[test]
    fn local_echo_clip_is_bounded_to_one_second_of_mono_pcm() {
        let max_bytes = SAMPLE_RATE as usize * 2 * LOCAL_ECHO_DURATION_MS as usize / 1_000;
        assert_eq!(LOCAL_ECHO_DURATION_MS, 1_000);
        assert!(validate_local_echo_pcm(&vec![0; max_bytes]).is_ok());
        assert!(validate_local_echo_pcm(&[]).is_err());
        assert!(validate_local_echo_pcm(&[0]).is_err());
        assert!(validate_local_echo_pcm(&vec![0; max_bytes + 2]).is_err());
    }

    #[test]
    fn local_echo_capture_and_playback_are_separate_pipewire_operations() {
        let capture_error = pipewire_process_error(AudioOperation::Capture, "Permission denied");
        let playback_error =
            pipewire_process_error(AudioOperation::EchoPlayback, "Permission denied");
        let generic_playback_error =
            pipewire_process_error(AudioOperation::EchoPlayback, "failure");
        assert!(capture_error.contains("microfone"));
        assert!(playback_error.contains("saída de áudio"));
        assert!(!playback_error.contains("tom"));
        assert!(generic_playback_error.contains("trecho capturado"));
    }
}
