//! OstMac patch: in-process event hub for Trouter push frames.
//!
//! The CLI prints Trouter events to stdout; embedders (Swift via ostmac-core)
//! need them as data. [`publish`] is called from the Trouter frame handler,
//! [`drain`] lets the host poll without sharing a tokio runtime, and
//! [`drain_wait`] lets the host block until events arrive (or a timeout
//! elapses) instead of waking on a fixed timer.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrouterEventKind {
    ChatService,
    Presence,
    CallInvitation,
    /// A delivery to a registered callAgent callback, retaining URL and body.
    CallSignal,
    /// Embedded transport became push-ready after WebSocket and registration.
    TransportConnected,
    /// Embedded transport left its event loop or was cancelled.
    TransportDisconnected,
    /// The server announced a Trouter message-loss gap.
    MessageLoss,
    Unclassified,
}

#[derive(Clone, PartialEq, Eq)]
pub struct TrouterEvent {
    pub kind: TrouterEventKind,
    pub payload_json: String,
}

impl std::fmt::Debug for TrouterEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrouterEvent")
            .field("kind", &self.kind)
            .field("payload_bytes", &self.payload_json.len())
            .finish_non_exhaustive()
    }
}

fn queue() -> &'static (Mutex<VecDeque<TrouterEvent>>, Condvar) {
    static Q: OnceLock<(Mutex<VecDeque<TrouterEvent>>, Condvar)> = OnceLock::new();
    Q.get_or_init(|| (Mutex::new(VecDeque::with_capacity(256)), Condvar::new()))
}

/// Publish one event payload (JSON). Never blocks, drops oldest over 1024.
pub fn publish(event_json: String) {
    publish_routed(TrouterEvent {
        kind: TrouterEventKind::Unclassified,
        payload_json: event_json,
    });
}

/// Publish an event after the Trouter envelope has been classified.
pub fn publish_routed(event: TrouterEvent) {
    let (m, c) = queue();
    let mut q = m.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= 1024 {
        q.pop_front();
    }
    q.push_back(event);
    c.notify_one();
}

/// Drain up to `max` queued events, oldest first.
pub fn drain(max: usize) -> Vec<String> {
    drain_routed(max)
        .into_iter()
        .map(|event| event.payload_json)
        .collect()
}

/// Drain classified events, oldest first.
pub fn drain_routed(max: usize) -> Vec<TrouterEvent> {
    let (m, _) = queue();
    let mut q = m.lock().unwrap_or_else(|e| e.into_inner());
    let n = max.min(q.len());
    q.drain(..n).collect()
}

/// Drain up to `max` queued events, waiting up to `timeout_ms` for the
/// first one when the queue is empty. Returns immediately once any event
/// is present; a timeout yields whatever is queued (possibly empty).
pub fn drain_wait(max: usize, timeout_ms: u64) -> Vec<String> {
    drain_routed_wait(max, timeout_ms)
        .into_iter()
        .map(|event| event.payload_json)
        .collect()
}

/// Drain classified events, waiting up to `timeout_ms` for the first one.
pub fn drain_routed_wait(max: usize, timeout_ms: u64) -> Vec<TrouterEvent> {
    let (m, c) = queue();
    let mut q = m.lock().unwrap_or_else(|e| e.into_inner());
    if q.is_empty() && timeout_ms > 0 {
        q = c
            .wait_timeout(q, Duration::from_millis(timeout_ms))
            .map(|(g, _)| g)
            .unwrap_or_else(|e| e.into_inner().0);
    }
    let n = max.min(q.len());
    q.drain(..n).collect()
}

/// Events still queued (backlog after a drain).
pub fn len() -> usize {
    let (m, _) = queue();
    m.lock().unwrap_or_else(|e| e.into_inner()).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn wait_wakes_on_publish_and_zero_timeout_never_blocks() {
        // Single test: hub is global, parallel hub tests would race.
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        drain(1024);
        let h = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            publish(r#"{"kind":"wake"}"#.to_string());
        });
        let got = drain_wait(64, 5000); // 5s budget, no timing assert
        h.join().unwrap();
        assert_eq!(got.len(), 1);

        publish("a".to_string());
        publish("b".to_string());
        assert_eq!(drain_wait(64, 0).len(), 2);
        assert!(drain_wait(64, 0).is_empty());
        assert_eq!(len(), 0);
    }

    #[test]
    fn classified_and_legacy_drains_preserve_payloads() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        drain(1024);
        publish_routed(TrouterEvent {
            kind: TrouterEventKind::CallInvitation,
            payload_json: r#"{"callInvitation":{}}"#.into(),
        });
        publish(r#"{"resourceType":"NewMessage"}"#.into());

        let classified = drain_routed(1);
        assert_eq!(classified[0].kind, TrouterEventKind::CallInvitation);
        assert_eq!(classified[0].payload_json, r#"{"callInvitation":{}}"#);
        assert_eq!(
            drain(1),
            vec![r#"{"resourceType":"NewMessage"}"#.to_owned()]
        );
    }
}
