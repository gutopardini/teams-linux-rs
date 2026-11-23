//! Automatic endpoint activity; never writes manual forceavailability.
use crate::{auth::TeamsSession, presence};
use std::{process::Command, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};
const IDLE_AFTER_MS: u64 = 300_000;
const PROBE_INTERVAL: Duration = Duration::from_secs(10);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(45);

pub struct Monitor {
    endpoint: String,
    stopped: Arc<AtomicBool>,
    registered: bool,
    active: Option<bool>,
    last_sent: Option<Instant>,
    last_probe: Option<Instant>,
    in_flight: bool,
    pub error: Option<String>,
    pub confirmation_pending: bool,
}
impl Default for Monitor {
    fn default() -> Self {
        Self { endpoint: presence::automatic_endpoint_id(), stopped: Arc::new(AtomicBool::new(false)), registered: false, active: None,
            last_sent: None, last_probe: None, in_flight: false, error: None, confirmation_pending: false }
    }
}
#[derive(Clone)]
pub struct Report { pub registered: bool, pub active: bool, pub sent: bool }
pub struct Probe { endpoint: String, stopped: Arc<AtomicBool>, registered: bool, active: Option<bool>, heartbeat_due: bool }
impl Monitor {
    pub fn begin(&mut self, now: Instant) -> Option<Probe> {
        if self.stopped.load(Ordering::Relaxed) || self.in_flight || self.last_probe.is_some_and(|last| now.duration_since(last) < if self.error.is_some() { Duration::from_secs(60) } else { PROBE_INTERVAL }) { return None; }
        self.in_flight = true;
        self.last_probe = Some(now);
        Some(Probe { endpoint: self.endpoint.clone(), stopped: self.stopped.clone(), registered: self.registered, active: self.active,
            heartbeat_due: self.last_sent.is_none_or(|last| now.duration_since(last) >= HEARTBEAT_INTERVAL) })
    }
    pub fn stop(&mut self) -> String {
        self.stopped.store(true, Ordering::Relaxed);
        self.endpoint.clone()
    }
    pub fn complete(&mut self, result: Result<Report, String>, now: Instant) -> bool {
        self.in_flight = false;
        match result {
            Ok(report) => {
                self.registered = report.registered;
                self.active = Some(report.active);
                self.error = None;
                if report.sent { self.last_sent = Some(now); self.confirmation_pending = true; }
                report.sent
            }
            Err(error) => { self.registered = false; self.error = Some(error); false }
        }
    }
}

fn gnome(method: &str, destination: &str, path: &str) -> Result<String, String> {
    let output = Command::new("gdbus").args(["call", "--session", "--dest", destination,
        "--object-path", path, "--method", method, "--timeout", "2"])
        .output().map_err(|_| "Monitor de atividade Linux indisponível.".to_owned())?;
    if !output.status.success() { return Err("Não foi possível consultar atividade/bloqueio da sessão Linux.".into()); }
    String::from_utf8(output.stdout).map_err(|_| "Resposta inválida do monitor de atividade.".into())
}
fn observed_activity(idle: &str, locked: &str) -> Result<bool, String> {
    let milliseconds = idle.trim().strip_prefix("(uint64 ").and_then(|s| s.strip_suffix(",)"))
        .and_then(|s| s.parse::<u64>().ok()).ok_or("Resposta inválida de tempo ocioso.".to_owned())?;
    let locked = match locked.trim() { "(true,)" => true, "(false,)" => false,
        _ => return Err("Resposta inválida de bloqueio da sessão.".into()) };
    Ok(!locked && milliseconds < IDLE_AFTER_MS)
}
pub async fn probe(session: TeamsSession, request: Probe) -> Result<Report, String> {
    crate::background::run(move || {
        let idle = gnome("org.gnome.Mutter.IdleMonitor.GetIdletime", "org.gnome.Mutter.IdleMonitor", "/org/gnome/Mutter/IdleMonitor/Core")?;
        let locked = gnome("org.gnome.ScreenSaver.GetActive", "org.gnome.ScreenSaver", "/org/gnome/ScreenSaver")?;
        let active = observed_activity(&idle, &locked)?;
        let sent = !request.registered || request.active != Some(active) || request.heartbeat_due;
        if request.stopped.load(Ordering::Relaxed) { return Err("Presença encerrada.".into()); }
        if sent { presence::report_automatic_activity(session.clone(), &request.endpoint, !request.registered || request.active != Some(active), active)?; }
        if request.stopped.load(Ordering::Relaxed) {
            let _ = presence::delete_automatic_endpoint(session, &request.endpoint);
            return Err("Presença encerrada.".into());
        }
        Ok(Report { registered: true, active, sent })
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn machine_activity_and_lock_are_independent_of_window_focus() {
        assert_eq!(observed_activity("(uint64 900,)", "(false,)"), Ok(true));
        assert_eq!(observed_activity("(uint64 900,)", "(true,)"), Ok(false));
        assert_eq!(observed_activity("(uint64 300000,)", "(false,)"), Ok(false));
        assert!(observed_activity("unknown", "(false,)").is_err());
        assert!(observed_activity("(uint64 900,)", "unknown").is_err());
    }
    #[test]
    fn stopping_invalidates_a_pending_probe_and_prevents_new_ones() {
        let mut monitor = Monitor::default();
        let probe = monitor.begin(Instant::now()).unwrap();
        let endpoint = monitor.stop();
        assert_eq!(endpoint, probe.endpoint);
        assert!(probe.stopped.load(Ordering::Relaxed));
        assert!(monitor.begin(Instant::now() + Duration::from_secs(60)).is_none());
    }
    #[test]
    fn monitor_bounds_inflight_and_refreshes_after_activity_changes() {
        let mut monitor = Monitor::default(); let now = Instant::now();
        assert!(monitor.begin(now).is_some()); assert!(monitor.begin(now).is_none());
        assert!(monitor.complete(Ok(Report { registered: true, active: true, sent: true }), now));
        assert!(monitor.begin(now + Duration::from_secs(5)).is_none());
        let probe = monitor.begin(now + Duration::from_secs(10)).unwrap();
        assert!(probe.registered); assert!(!probe.heartbeat_due); assert_eq!(probe.active, Some(true));
        monitor.complete(Err("fixture".into()), now);
        assert!(!monitor.registered);
        assert!(monitor.error.is_some());
    }
}

pub async fn stop(session: TeamsSession, endpoint: String) -> Result<(), String> {
    crate::background::run(move || presence::delete_automatic_endpoint(session, &endpoint)).await
}
