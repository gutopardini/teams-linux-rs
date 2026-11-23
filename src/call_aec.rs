//! Per-call, process-owned PipeWire acoustic echo cancellation. No device is
//! opened until LiveAudio::start is called by the explicit join action.
use serde_json::{Value, json};
use std::{os::unix::process::CommandExt, process::{Child, Command, Stdio}, sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc}, thread, time::{Duration, Instant}};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
const PREFIX: &str = "teams-call-aec-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AecState { Starting = 0, Active = 1, Unavailable = 2, Failed = 3, Disabled = 4 }
/// Acoustic validation regressed: only an explicit command-line opt-in may
/// create the experimental graph. No environment or persisted preference.
pub(crate) fn experimental_requested(args: impl IntoIterator<Item = impl AsRef<str>>) -> bool {
    args.into_iter().any(|arg| arg.as_ref() == "--experimental-aec")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AecError { Unavailable, Failed }
impl AecError {
    fn record(self) -> Self {
        trace(match self { Self::Unavailable => AecState::Unavailable, Self::Failed => AecState::Failed });
        self
    }
}
pub(crate) fn trace(state: AecState) {
    crate::live_trace::record("call_aec_state", 0, state as usize, 0);
}

#[derive(Clone, Debug)]
struct Endpoint { id: u64, serial: u64 }
#[derive(Clone, Debug)]
struct Plan { microphone: Endpoint, speaker: Endpoint, names: [String; 4] }

fn number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}
fn props(object: &Value) -> Option<&serde_json::Map<String, Value>> {
    object.pointer("/info/props")?.as_object()
}
fn truth(value: Option<&Value>) -> bool {
    value.is_some_and(|v| v == true || v.as_str() == Some("true"))
}
fn objects(dump: &str) -> Result<Vec<Value>, AecError> {
    serde_json::from_str::<Vec<Value>>(dump).map_err(|_| AecError::Failed)
}
fn physical(objects: &[Value], name: &str, class: &str) -> Result<Endpoint, AecError> {
    if name.is_empty() || name.len() > 1024 || name.chars().any(char::is_control) || name.starts_with(PREFIX) {
        return Err(AecError::Failed);
    }
    let mut matching = objects.iter().filter(|o| o.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Node")
        && props(o).and_then(|p| p.get("node.name")).and_then(Value::as_str) == Some(name));
    let node = matching.next().ok_or(AecError::Failed)?;
    if matching.next().is_some() { return Err(AecError::Failed); }
    let p = props(node).ok_or(AecError::Failed)?;
    // A physical device association and exact class prevent recursive AEC,
    // monitor sources, and virtual defaults. Never follow a new default later.
    if p.get("media.class").and_then(Value::as_str) != Some(class)
        || p.get("device.id").and_then(number).is_none()
        || truth(p.get("node.virtual")) || truth(p.get("stream.monitor"))
        || p.keys().any(|k| k.starts_with("aec.") || k.starts_with("echo.cancel"))
    { return Err(AecError::Failed); }
    Ok(Endpoint { id: node.get("id").and_then(number).ok_or(AecError::Failed)?,
        serial: p.get("object.serial").and_then(number).filter(|n| *n > 0).ok_or(AecError::Failed)? })
}
impl Plan {
    fn new(input: &str, output: &str, dump: &str) -> Result<Self, AecError> {
        let objects = objects(dump)?;
        let tag = format!("{PREFIX}{}-{:016x}", std::process::id(), rand::random::<u64>());
        Ok(Self { microphone: physical(&objects, input, "Audio/Source")?, speaker: physical(&objects, output, "Audio/Sink")?,
            names: ["capture", "source", "sink", "playback"].map(|part| format!("{tag}-{part}")) })
    }
    fn arguments(&self) -> String {
        // JSON is valid SPA JSON. Endpoint names never enter pw-cli's command
        // parser (which also treats '#' in quotes as a comment): target stable
        // numeric serials, with generated names and no shell interpolation.
        json!({
            "library.name": "aec/libspa-aec-webrtc", "audio.rate": 8000,
            "audio.channels": 1, "audio.position": ["MONO"], "monitor.mode": false,
            "aec.args": {"webrtc.noise_suppression": false, "webrtc.gain_control": false,
                "webrtc.high_pass_filter": false, "webrtc.voice_detection": false,
                "webrtc.transient_suppression": false, "webrtc.experimental_agc": false,
                "webrtc.experimental_ns": false, "webrtc.beamforming": false,
                "webrtc.extended_filter": true, "webrtc.delay_agnostic": true},
            "capture.props": {"node.name": self.names[0], "target.object": self.microphone.serial.to_string(),
                "node.dont-reconnect": true, "node.dont-fallback": true},
            "source.props": {"node.name": self.names[1], "media.class": "Audio/Source", "node.virtual": true},
            "sink.props": {"node.name": self.names[2], "media.class": "Audio/Sink", "node.virtual": true},
            "playback.props": {"node.name": self.names[3], "target.object": self.speaker.serial.to_string(),
                "node.dont-reconnect": true, "node.dont-fallback": true}
        }).to_string()
    }
    fn command(&self) -> Command {
        let mut command = Command::new("pw-cli");
        command.args(["-m", "load-module", "libpipewire-module-echo-cancel", &self.arguments()]);
        command
    }
}

pub(crate) struct EchoCancellation {
    process: Child, plan: Plan, pub input: String, pub output: String,
    lifetime: Option<(mpsc::Sender<()>, thread::JoinHandle<()>)>,
    monitor: Option<(Arc<AtomicBool>, thread::JoinHandle<()>)>,
    healthy: Arc<AtomicBool>,
}
impl EchoCancellation {
    pub fn start(input: Option<&str>, output: Option<&str>) -> Result<Self, AecError> {
        trace(AecState::Starting);
        let result = (|| {
            let dump = snapshot(DISCOVERY_TIMEOUT)?;
            let plan = Plan::new(input.ok_or(AecError::Failed)?, output.ok_or(AecError::Failed)?, &dump)?;
            let command = plan.command();
            Self::start_with(plan, command, |_, _, timeout| snapshot(timeout))
        })();
        result.map_err(AecError::record)
    }
    fn start_with<F>(plan: Plan, mut command: Command, mut dump: F) -> Result<Self, AecError>
    where F: FnMut(u32, &Plan, Duration) -> Result<String, AecError> {
        // This subprocess owns the native module. SIGKILL on parent death also
        // closes its PipeWire connections if the application itself crashes.
        let parent = std::process::id();
        unsafe { command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 { return Err(std::io::Error::last_os_error()); }
            if libc::getppid() as u32 != parent { return Err(std::io::Error::other("audio owner exited")); }
            Ok(())
        }); }
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // Linux parent-death signals follow the spawning *thread*. Keep that
        // thread alive for the complete call (a Tokio blocking pool thread may
        // otherwise retire while a healthy call is still running).
        let (started, child) = mpsc::sync_channel(1);
        let (release, lifetime) = mpsc::channel();
        let lifetime = thread::Builder::new().name("teams-aec-owner".into()).spawn(move || {
            if let Err(unsent) = started.send(command.spawn()) {
                if let Ok(mut child) = unsent.0 { let _ = child.kill(); let _ = child.wait(); }
            } else { let _ = lifetime.recv(); }
        }).map_err(|_| AecError::Failed)?;
        let process = match child.recv() {
            Ok(Ok(process)) => process,
            result => {
                drop(release); let _ = lifetime.join();
                return Err(match result {
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => AecError::Unavailable,
                    _ => AecError::Failed,
                });
            }
        };
        let mut owner = Self { process, plan, input: String::new(), output: String::new(),
            lifetime: Some((release, lifetime)), monitor: None, healthy: Arc::new(AtomicBool::new(true)) };
        let deadline = Instant::now() + DISCOVERY_TIMEOUT;
        loop {
            owner.check_alive()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return Err(AecError::Failed); }
            if let Some((input, output)) = owner.routes(&dump(owner.process.id(), &owner.plan, remaining)?, None)? {
                owner.input = input.to_string(); owner.output = output.to_string();
                return Ok(owner);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    #[cfg(test)]
    pub(crate) fn synthetic() -> Self {
        Self::start_with(tests::plan(), tests::sleeper(), |pid, plan, _| Ok(json!(tests::graph(pid,plan,false)).to_string())).unwrap()
    }
    #[cfg(test)]
    pub(crate) fn process_id(&self) -> u32 { self.process.id() }
    pub fn check_alive(&mut self) -> Result<(), AecError> {
        if !self.healthy.load(Ordering::Acquire) || self.process.try_wait().map_err(|_| AecError::Failed)?.is_some() { Err(AecError::Failed) } else { Ok(()) }
    }
    /// Verify the two app edges as well as the two physical edges before the
    /// media worker is exposed. Paused links are valid until frames are sent.
    pub fn connect_app(&mut self, capture: u32, playback: u32) -> Result<(), AecError> {
        self.connect_app_with(capture, playback, snapshot)?;
        let plan = self.plan.clone(); let pid = self.process.id();
        let healthy = self.healthy.clone();
        let stop = Arc::new(AtomicBool::new(false)); let stopping = stop.clone();
        let worker = thread::Builder::new().name("teams-aec-health".into()).spawn(move || {
            let mut missed_inspections = 0;
            while !stopping.load(Ordering::Acquire) {
                for _ in 0..50 {
                    if stopping.load(Ordering::Acquire) { return; }
                    thread::sleep(Duration::from_millis(20));
                }
                let valid = probe_health(&plan, pid, snapshot(Duration::from_millis(750)),
                    (capture, playback), &mut missed_inspections);
                if !valid { healthy.store(false, Ordering::Release); return; }
            }
        }).map_err(|_| AecError::Failed)?;
        self.monitor = Some((stop, worker));
        Ok(())
    }
    fn connect_app_with<F>(&mut self, capture: u32, playback: u32, mut dump: F) -> Result<(), AecError>
    where F: FnMut(Duration) -> Result<String, AecError> {
        let deadline = Instant::now() + DISCOVERY_TIMEOUT;
        loop {
            self.check_alive()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return Err(AecError::Failed); }
            if self.routes(&dump(remaining)?, Some((capture, playback)))?.is_some() { return Ok(()); }
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn routes(&self, dump: &str, app: Option<(u32, u32)>) -> Result<Option<(u64, u64)>, AecError> {
        graph_routes(&self.plan, self.process.id(), dump, app)
    }
}
fn graph_routes(plan: &Plan, pid: u32, dump: &str, app: Option<(u32, u32)>) -> Result<Option<(u64, u64)>, AecError> {
        let objects = objects(dump)?;
        // Serial + global ID must still refer to the selected physical nodes.
        for endpoint in [&plan.microphone, &plan.speaker] {
            if !objects.iter().any(|o| o.get("id").and_then(number) == Some(endpoint.id)
                && props(o).and_then(|p| p.get("object.serial")).and_then(number) == Some(endpoint.serial)) {
                return Err(AecError::Failed);
            }
        }
        let mut nodes = Vec::new();
        for name in &plan.names {
            let found = objects.iter().filter(|o| props(o).and_then(|p| p.get("node.name")).and_then(Value::as_str) == Some(name.as_str())).collect::<Vec<_>>();
            if found.len() > 1 { return Err(AecError::Failed); }
            let Some(node) = found.first() else { return Ok(None); };
            if !owned_by(&objects, node, pid) { return Err(AecError::Failed); }
            nodes.push(*node);
        }
        let ids = nodes.iter().map(|n| n.get("id").and_then(number).ok_or(AecError::Failed)).collect::<Result<Vec<_>, _>>()?;
        if !linked(&objects, plan.microphone.id, ids[0]) || !linked(&objects, ids[3], plan.speaker.id) { return Ok(None); }
        if let Some((capture, playback)) = app {
            let capture = objects.iter().find(|n| owned_by(&objects, n, capture)
                && props(n).and_then(|p| p.get("media.class")).and_then(Value::as_str) == Some("Stream/Input/Audio"));
            let playback = objects.iter().find(|n| owned_by(&objects, n, playback)
                && props(n).and_then(|p| p.get("media.class")).and_then(Value::as_str) == Some("Stream/Output/Audio"));
            let (Some(capture), Some(playback)) = (capture, playback) else { return Ok(None); };
            if !linked(&objects, ids[1], capture["id"].as_u64().ok_or(AecError::Failed)?)
                || !linked(&objects, playback["id"].as_u64().ok_or(AecError::Failed)?, ids[2]) { return Ok(None); }
        }
        let source = props(nodes[1]).and_then(|p| p.get("object.serial")).and_then(number).ok_or(AecError::Failed)?;
        let sink = props(nodes[2]).and_then(|p| p.get("object.serial")).and_then(number).ok_or(AecError::Failed)?;
        if props(nodes[1]).and_then(|p| p.get("media.class")).and_then(Value::as_str) != Some("Audio/Source")
            || props(nodes[2]).and_then(|p| p.get("media.class")).and_then(Value::as_str) != Some("Audio/Sink") { return Err(AecError::Failed); }
        Ok(Some((source, sink)))
    }
// A delayed diagnostic command must not drop an otherwise healthy call.
// Known graph loss fails immediately; unavailable inspection gets three
// consecutive bounded attempts, without ever switching to raw microphone.
fn probe_health(plan: &Plan, pid: u32, dump: Result<String, AecError>, app: (u32,u32), missed: &mut u8) -> bool {
    match dump {
        Ok(dump) => {
            *missed = 0;
            graph_routes(plan, pid, &dump, Some(app)).is_ok_and(|routes| routes.is_some())
        }
        Err(_) => { *missed = missed.saturating_add(1); *missed < 3 }
    }
}
fn owned_by(objects: &[Value], node: &Value, pid: u32) -> bool {
    if node.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Node") { return false; }
    let Some(client) = props(node).and_then(|p| p.get("client.id")).and_then(number) else { return false; };
    objects.iter().any(|c| c.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Client")
        && c.get("id").and_then(number) == Some(client)
        && props(c).and_then(|p| p.get("application.process.id")).and_then(number) == Some(u64::from(pid)))
}
fn linked(objects: &[Value], output: u64, input: u64) -> bool {
    objects.iter().any(|o| o.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Link")
        && o.pointer("/info/output-node-id").and_then(number) == Some(output)
        && o.pointer("/info/input-node-id").and_then(number) == Some(input)
        && matches!(o.pointer("/info/state").and_then(Value::as_str), Some("paused" | "active")))
}
fn snapshot(timeout: Duration) -> Result<String, AecError> {
    crate::audio::command_output_timeout("pw-dump", &[], timeout).map_err(|_| AecError::Unavailable)
}
impl Drop for EchoCancellation {
    fn drop(&mut self) {
        if let Some((stop, _)) = &self.monitor { stop.store(true, Ordering::Release); }
        let _ = self.process.kill(); let _ = self.process.wait();
        if let Some((release, owner)) = self.lifetime.take() { drop(release); let _ = owner.join(); }
        if let Some((_, monitor)) = self.monitor.take() { let _ = monitor.join(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn echo_processing_requires_exact_explicit_opt_in() {
        assert!(!experimental_requested(["teams-linux-rs", "--trace-performance"]));
        assert!(!experimental_requested(["--experimental-aec=false", "prefix--experimental-aec"]));
        assert!(experimental_requested(["teams-linux-rs", "--experimental-aec"]));
    }
    fn node(id: u64, serial: u64, name: &str, class: &str, client: Option<u64>) -> Value {
        let mut node = json!({"id":id,"type":"PipeWire:Interface:Node","info":{"props":{
            "node.name":name,"object.serial":serial,"media.class":class,"device.id":90}}});
        if let Some(client) = client { node["info"]["props"]["client.id"] = json!(client); }
        node
    }
    fn physical_graph() -> Vec<Value> {
        vec![node(10, 101, "mic", "Audio/Source", None), node(20, 201, "speaker", "Audio/Sink", None)]
    }
    pub(super) fn plan() -> Plan { Plan::new("mic", "speaker", &json!(physical_graph()).to_string()).unwrap() }
    fn client(id: u64, pid: u32) -> Value {
        json!({"id":id,"type":"PipeWire:Interface:Client","info":{"props":{"application.process.id":pid.to_string()}}})
    }
    fn link(from: u64, to: u64) -> Value {
        json!({"type":"PipeWire:Interface:Link","info":{"output-node-id":from,"input-node-id":to,"state":"paused"}})
    }
    pub(super) fn graph(pid: u32, plan: &Plan, app: bool) -> Vec<Value> {
        let mut graph = physical_graph();
        graph.push(client(50, pid));
        for (i, name) in plan.names.iter().enumerate() {
            graph.push(node(30+i as u64, 301+i as u64, name,
                ["Stream/Input/Audio", "Audio/Source", "Audio/Sink", "Stream/Output/Audio"][i], Some(50)));
        }
        graph.extend([link(10,30), link(33,20)]);
        if app {
            graph.extend([client(60, 111), client(61, 222),
                node(40, 401, "app-capture", "Stream/Input/Audio", Some(60)),
                node(41, 402, "app-playback", "Stream/Output/Audio", Some(61)), link(31,40), link(41,32)]);
        }
        graph
    }
    pub(super) fn sleeper() -> Command {
        let mut command = Command::new("python3"); command.args(["-c", "import time;time.sleep(60)"]); command
    }
    fn alive(pid: u32) -> bool { unsafe { libc::kill(pid as i32, 0) == 0 } }
    #[test]
    fn native_module_is_owned_and_both_app_directions_are_verified() {
        let mut polls = 0;
        let mut owner = EchoCancellation::start_with(plan(), sleeper(), |pid, plan, _| {
            polls += 1;
            Ok(json!(if polls == 1 { physical_graph() } else { graph(pid, plan, false) }).to_string())
        }).unwrap();
        assert!(polls >= 2);
        assert_eq!(owner.input, "302"); assert_eq!(owner.output, "303");
        let mut full = graph(owner.process.id(), &owner.plan, true);
        assert!(owner.routes(&json!(full).to_string(), Some((111,222))).unwrap().is_some());
        full.pop(); // No far-end reference: physical playback alone is insufficient.
        assert_eq!(owner.routes(&json!(full).to_string(), Some((111,222))).unwrap(), None);
        let mut full = graph(owner.process.id(), &owner.plan, true);
        full.retain(|o| o.pointer("/info/input-node-id").and_then(number) != Some(40));
        assert_eq!(owner.routes(&json!(full).to_string(), Some((111,222))).unwrap(), None);
        let ready = json!(graph(owner.process.id(), &owner.plan, true)).to_string();
        owner.connect_app_with(111,222, |_| Ok(ready.clone())).unwrap();
        let pid = owner.process.id(); assert!(alive(pid)); drop(owner); assert!(!alive(pid));
    }
    #[test]
    fn hostile_endpoint_names_cannot_enter_pw_cli_or_spa_arguments() {
        let hostile = "mic \" } # $(touch /tmp/no) `id` \\";
        let mut fixture = physical_graph(); fixture[0]["info"]["props"]["node.name"] = json!(hostile);
        let plan = Plan::new(hostile,"speaker", &json!(fixture).to_string()).unwrap();
        let args = plan.arguments();
        assert!(!args.contains(hostile)); assert!(!args.contains('#')); assert!(!args.contains("touch"));
        let args: Value = serde_json::from_str(&args).unwrap();
        assert_eq!(args["capture.props"]["target.object"], "101");
        assert_eq!(args["playback.props"]["target.object"], "201");
        assert_eq!(args["aec.args"]["webrtc.noise_suppression"], false);
        assert_eq!(args["aec.args"]["webrtc.gain_control"], false);
        let command = plan.command(); let args = command.get_args().collect::<Vec<_>>();
        assert_eq!(args.len(), 4); assert_eq!(args[0], "-m");
    }
    #[test]
    fn rejects_ambiguous_virtual_monitor_missing_and_control_endpoints() {
        for mutation in ["virtual", "monitor", "duplicate", "device", "serial", "class", "aec"] {
            let mut fixture = physical_graph();
            match mutation {
                "virtual" => fixture[0]["info"]["props"]["node.virtual"] = json!(true),
                "monitor" => fixture[0]["info"]["props"]["stream.monitor"] = json!(true),
                "duplicate" => fixture.push(fixture[0].clone()),
                "device" => { fixture[0]["info"]["props"].as_object_mut().unwrap().remove("device.id"); },
                "serial" => fixture[0]["info"]["props"]["object.serial"] = json!(0),
                "class" => fixture[0]["info"]["props"]["media.class"] = json!("Audio/Sink"),
                _ => fixture[0]["info"]["props"]["aec.method"] = json!("webrtc"),
            }
            assert!(Plan::new("mic", "speaker", &json!(fixture).to_string()).is_err(), "{mutation}");
        }
        for name in ["", "missing", "@DEFAULT_AUDIO_SOURCE@", "teams-call-aec-old-source", "mic\n"] {
            assert!(Plan::new(name, "speaker", &json!(physical_graph()).to_string()).is_err());
        }
    }
    #[test]
    fn rejects_wrong_owner_identity_reused_physical_id_and_failed_links() {
        let owner = EchoCancellation::start_with(plan(), sleeper(), |pid, plan, _| Ok(json!(graph(pid,plan,false)).to_string())).unwrap();
        let wrong = graph(owner.process.id()+1, &owner.plan, false);
        assert!(owner.routes(&json!(wrong).to_string(), None).is_err());
        let mut reused = graph(owner.process.id(), &owner.plan, false);
        reused[0]["info"]["props"]["object.serial"] = json!(999);
        assert!(owner.routes(&json!(reused).to_string(), None).is_err());
        let mut bad_link = graph(owner.process.id(), &owner.plan, false);
        bad_link.last_mut().unwrap()["info"]["state"] = json!("error");
        assert_eq!(owner.routes(&json!(bad_link).to_string(), None).unwrap(), None);
    }
    #[test]
    fn startup_error_and_deadline_drop_the_partial_backend() {
        let mut pid = 0;
        assert!(EchoCancellation::start_with(plan(), sleeper(), |child, _, _| {
            pid = child; Err(AecError::Failed)
        }).is_err()); assert!(!alive(pid));
        let start = Instant::now();
        assert!(EchoCancellation::start_with(plan(), sleeper(), |child, _, _| {
            pid = child; Ok(json!(physical_graph()).to_string())
        }).is_err());
        assert!(start.elapsed() < Duration::from_secs(4)); assert!(!alive(pid));
        assert!(matches!(EchoCancellation::start_with(plan(), Command::new("/nonexistent/teams-aec"), |_,_,_| unreachable!()), Err(AecError::Unavailable)));
    }
    #[test]
    fn health_tolerates_transient_inspection_delay_but_detects_module_loss() {
        let plan = plan(); let pid = 999; let mut missed = 0;
        let valid = json!(graph(pid, &plan, true)).to_string();
        assert!(probe_health(&plan,pid,Err(AecError::Unavailable),(111,222),&mut missed));
        assert!(probe_health(&plan,pid,Err(AecError::Unavailable),(111,222),&mut missed));
        assert!(probe_health(&plan,pid,Ok(valid.clone()),(111,222),&mut missed));
        assert_eq!(missed,0);
        for _ in 0..2 { assert!(probe_health(&plan,pid,Err(AecError::Unavailable),(111,222),&mut missed)); }
        assert!(!probe_health(&plan,pid,Err(AecError::Unavailable),(111,222),&mut missed));
        assert!(!probe_health(&plan,pid,Ok(json!(physical_graph()).to_string()),(111,222),&mut missed));
    }
    #[test]
    fn backend_death_is_detected_and_no_success_is_inferred_from_spawn() {
        let mut owner = EchoCancellation::start_with(plan(), sleeper(), |pid, plan, _| Ok(json!(graph(pid,plan,false)).to_string())).unwrap();
        owner.process.kill().unwrap(); owner.process.wait().unwrap();
        assert_eq!(owner.check_alive(), Err(AecError::Failed));
        let mut command = Command::new("python3"); command.args(["-c","pass"]);
        assert!(EchoCancellation::start_with(plan(), command, |_,_,_| Ok(json!(physical_graph()).to_string())).is_err());
    }
}
