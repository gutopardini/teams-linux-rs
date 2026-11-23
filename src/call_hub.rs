//! Immediate, session-bound callback delivery. The Trouter worker feeds this
//! directly; call setup never waits for the UI's chat refresh timer.
use crate::{
    call_context::{CallCallbacks, CallContext, CallbackKind},
    call_lifecycle::CallKey,
};
use ost::event_hub::{TrouterEvent, TrouterEventKind};
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU8, Ordering},
    },
};
use tokio::sync::{Notify, watch};

const MAX_EVENTS: usize = 32;
const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StopReason {
    Running,
    User,
    TransportLost,
    Overflow,
    OwnerReleased,
    RemoteEnded,
}

#[derive(Clone)]
pub struct CallStop(Arc<StopState>);
struct StopState {
    reason: AtomicU8,
    sender: watch::Sender<bool>,
}
impl CallStop {
    fn new() -> (Self, watch::Receiver<bool>) {
        let (sender, receiver) = watch::channel(false);
        (
            Self(Arc::new(StopState {
                reason: AtomicU8::new(0),
                sender,
            })),
            receiver,
        )
    }
    fn fail(&self, reason: StopReason) {
        if self
            .0
            .reason
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = self.0.sender.send(true);
        }
    }
    pub fn cancel(&self) {
        self.fail(StopReason::User);
    }
    pub(crate) fn remote_ended(&self) {
        self.fail(StopReason::RemoteEnded);
    }
    pub(crate) fn overflow(&self) {
        self.fail(StopReason::Overflow);
    }
    pub fn reason(&self) -> StopReason {
        match self.0.reason.load(Ordering::Acquire) {
            0 => StopReason::Running,
            1 => StopReason::User,
            2 => StopReason::TransportLost,
            3 => StopReason::Overflow,
            4 => StopReason::OwnerReleased,
            _ => StopReason::RemoteEnded,
        }
    }
}

pub struct CallDelivery {
    pub kind: CallbackKind,
    pub body: Value,
    /// Only callbacks registered for the currently active camera operation
    /// carry a generation. Initial call answers cannot complete a later toggle.
    pub camera_generation: Option<u64>,
    bytes: usize,
}
impl std::fmt::Debug for CallDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallDelivery")
            .field("kind", &self.kind)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}
impl CallDelivery {
    pub(crate) fn bytes(&self) -> usize { self.bytes }
    fn critical(&self) -> bool {
        matches!(
            self.kind,
            CallbackKind::Acceptance
                | CallbackKind::CallEnd
                | CallbackKind::ConversationEnd
                | CallbackKind::AddParticipantFailure
                | CallbackKind::Redirection
                | CallbackKind::MediaAnswer
                | CallbackKind::MediaRenegotiation
                | CallbackKind::AdmitSuccess
                | CallbackKind::AdmitFailure
        )
    }
}
#[derive(Default)]
struct Queue {
    events: VecDeque<CallDelivery>,
    bytes: usize,
}
#[derive(Default)]
struct Mailbox {
    queue: Mutex<Queue>,
    ready: Notify,
}
struct Subscriber {
    id: u64,
    key: CallKey,
    callbacks: CallCallbacks,
    camera_callbacks: Option<(u64, CallCallbacks)>,
    mailbox: Arc<Mailbox>,
    stop: CallStop,
}
struct State {
    agent_endpoint: String,
    context: Option<CallContext>,
    subscriber: Option<Subscriber>,
    next: u64,
}
impl Default for State {
    fn default() -> Self {
        Self {
            agent_endpoint: crate::call_context::native_id(),
            context: None,
            subscriber: None,
            next: 0,
        }
    }
}
#[derive(Clone, Default)]
pub struct CallHub(Arc<Mutex<State>>);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscribeError {
    NotReady,
    Stale,
    Busy,
}

impl CallHub {
    #[cfg(test)]
    pub(crate) fn queued_events(&self) -> Option<usize> {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.subscriber.as_ref().map(|sub| {
            sub.mailbox.queue.lock().unwrap_or_else(|e| e.into_inner()).events.len()
        })
    }

    pub fn agent_endpoint(&self) -> String {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .agent_endpoint
            .clone()
    }

    pub fn context(&self) -> Option<CallContext> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .context
            .as_ref()
            .filter(|context| context.is_ready())
            .cloned()
    }
    pub fn invalidate(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.context = None;
        if let Some(subscriber) = state.subscriber.take() {
            subscriber.stop.fail(StopReason::TransportLost);
        }
    }
    pub fn subscribe(
        &self,
        key: CallKey,
        callbacks: CallCallbacks,
    ) -> Result<CallSubscription, SubscribeError> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let context = state
            .context
            .as_ref()
            .filter(|context| context.is_ready())
            .ok_or(SubscribeError::NotReady)?;
        if !callbacks.is_current(&key, context) {
            return Err(SubscribeError::Stale);
        }
        if state
            .subscriber
            .as_ref()
            .is_some_and(|sub| sub.stop.reason() == StopReason::Running)
        {
            return Err(SubscribeError::Busy);
        }
        state.next = state.next.checked_add(1).ok_or(SubscribeError::Busy)?;
        let id = state.next;
        let mailbox = Arc::new(Mailbox::default());
        let (stop, receiver) = CallStop::new();
        state.subscriber = Some(Subscriber {
            id,
            key,
            callbacks,
            camera_callbacks: None,
            mailbox: mailbox.clone(),
            stop: stop.clone(),
        });
        Ok(CallSubscription {
            hub: Arc::downgrade(&self.0),
            id,
            mailbox,
            stop,
            receiver,
        })
    }
    /// Register before posting an offer. The ordinary callback builder owns
    /// the route shape and fresh nonces; only the media answer/rejection links
    /// from this set are exposed to the camera operation.
    pub(crate) fn register_camera_callbacks(
        &self,
        key: &CallKey,
        generation: u64,
    ) -> Option<CallCallbacks> {
        if generation == 0 { return None; }
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let context = state.context.as_ref()?.clone();
        let callbacks = context.prepare_callbacks(key.clone())?;
        let subscriber = state.subscriber.as_mut()?;
        if &subscriber.key != key || subscriber.stop.reason() != StopReason::Running
            || !subscriber.callbacks.is_current(key, &context)
        { return None; }
        subscriber.camera_callbacks = Some((generation, callbacks.clone()));
        Some(callbacks)
    }
    pub(crate) fn clear_camera_callbacks(&self, key: &CallKey, generation: u64) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(subscriber) = state.subscriber.as_mut().filter(|sub| &sub.key == key) {
            if subscriber.camera_callbacks.as_ref().is_some_and(|(active, _)| *active == generation) {
                subscriber.camera_callbacks = None;
            }
        }
    }
    pub fn handle_event(&self, event: TrouterEvent) {
        match event.kind {
            TrouterEventKind::TransportDisconnected => self.invalidate(),
            TrouterEventKind::TransportConnected => {
                let context = CallContext::from_connected_event(&event.payload_json);
                let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
                if state.subscriber.as_ref().is_some_and(|sub| {
                    context
                        .as_ref()
                        .is_none_or(|ctx| !sub.callbacks.is_current(&sub.key, ctx))
                }) {
                    if let Some(sub) = state.subscriber.take() {
                        sub.stop.fail(StopReason::TransportLost);
                    }
                }
                state.context = context;
            }
            TrouterEventKind::CallSignal => {
                if event.payload_json.len() > 1024 * 1024 {
                    return;
                }
                let Ok(mut envelope) = serde_json::from_str::<Value>(&event.payload_json) else {
                    return;
                };
                let address = envelope.get("url").and_then(Value::as_str);
                let short_address = envelope.get("shortUrl").and_then(Value::as_str);
                let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
                let (Some(context), Some(sub)) = (&state.context, &state.subscriber) else {
                    return;
                };
                if sub.stop.reason() != StopReason::Running {
                    return;
                }
                let initial = sub.callbacks.delivery_kind_from_addresses(
                    &sub.key, address, short_address, context,
                ).map(|kind| (kind, None));
                let camera = sub.camera_callbacks.as_ref().and_then(|(generation, callbacks)| {
                    callbacks.delivery_kind_from_addresses(&sub.key, address, short_address, context)
                        .filter(|kind| matches!(kind, CallbackKind::MediaAnswer | CallbackKind::MediaRejection))
                        .map(|kind| (kind, Some(*generation)))
                });
                let Some((kind, camera_generation)) = initial.or(camera) else {
                    crate::live_trace::record("call_callback_unmatched",0,0,0);
                    return;
                };
                let Some(body) = envelope.get_mut("body").map(Value::take) else {
                    return;
                };
                let delivery = CallDelivery {
                    kind,
                    body,
                    camera_generation,
                    bytes: event.payload_json.len(),
                };
                let mut queue = sub.mailbox.queue.lock().unwrap_or_else(|e| e.into_inner());
                while queue.events.len() >= MAX_EVENTS || queue.bytes + delivery.bytes > MAX_BYTES {
                    if !delivery.critical() {
                        return;
                    }
                    if let Some(index) = queue.events.iter().position(|event| !event.critical()) {
                        let old = queue.events.remove(index).unwrap();
                        queue.bytes -= old.bytes;
                    } else {
                        sub.stop.fail(StopReason::Overflow);
                        return;
                    }
                }
                crate::live_trace::record("call_callback_delivered",0,kind as usize,0);
                queue.bytes += delivery.bytes;
                queue.events.push_back(delivery);
                drop(queue);
                sub.mailbox.ready.notify_one();
            }
            _ => {}
        }
    }
}

pub struct CallSubscription {
    hub: Weak<Mutex<State>>,
    id: u64,
    mailbox: Arc<Mailbox>,
    pub stop: CallStop,
    receiver: watch::Receiver<bool>,
}
impl CallSubscription {
    pub fn cancellation(&self) -> watch::Receiver<bool> {
        self.receiver.clone()
    }
    pub fn try_next(&mut self) -> Option<CallDelivery> {
        if self.stop.reason() != StopReason::Running {
            return None;
        }
        let mut queue = self.mailbox.queue.lock().unwrap_or_else(|e| e.into_inner());
        let event = queue.events.pop_front()?;
        queue.bytes -= event.bytes;
        Some(event)
    }
    pub async fn next(&mut self) -> Option<CallDelivery> {
        loop {
            if self.stop.reason() != StopReason::Running {
                return None;
            }
            let ready = self.mailbox.ready.notified();
            let event = {
                let mut queue = self.mailbox.queue.lock().unwrap_or_else(|e| e.into_inner());
                let event = queue.events.pop_front();
                if let Some(event) = &event {
                    queue.bytes -= event.bytes;
                }
                event
            };
            if event.is_some() {
                return event;
            }
            tokio::select! {_=ready=>{},_=self.receiver.changed()=>{}}
        }
    }
}
impl Drop for CallSubscription {
    fn drop(&mut self) {
        self.stop.fail(StopReason::OwnerReleased);
        if let Some(hub) = self.hub.upgrade() {
            let mut state = hub.lock().unwrap_or_else(|e| e.into_inner());
            if state
                .subscriber
                .as_ref()
                .is_some_and(|sub| sub.id == self.id)
            {
                state.subscriber = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (CallHub, CallKey, CallCallbacks) {
        let hub = CallHub::default();
        hub.handle_event(TrouterEvent{kind:TrouterEventKind::TransportConnected,payload_json:serde_json::json!({"messagingRoot":"https://fixture.trouter.skype.com/opaque/","transportEndpointId":"fixture-endpoint","ttlSeconds":300}).to_string()});
        let key = CallKey {
            generation: 1,
            id: "fixture-call".into(),
        };
        let callbacks = hub
            .context()
            .unwrap()
            .prepare_callbacks(key.clone())
            .unwrap();
        (hub, key, callbacks)
    }
    fn deliver(hub: &CallHub, callbacks: &CallCallbacks, kind: CallbackKind) {
        // Exercise the same HTTP-over-WebSocket decoder used by the worker.
        let frame=serde_json::json!({"method":"POST","url":callbacks.link(kind),"body":{"private":"fixture-secret"}}).to_string();
        hub.handle_event(ost::trouter::data_frame_routed_event(&frame).unwrap());
    }

    #[test]
    fn camera_operation_callbacks_reject_previous_attempt_and_wrong_kinds() {
        let (hub, key, original) = fixture();
        let mut subscription = hub.subscribe(key.clone(), original.clone()).unwrap();
        let first = hub.register_camera_callbacks(&key, 1).unwrap();
        deliver(&hub, &first, CallbackKind::MediaAnswer);
        let early = subscription.try_next().expect("answer is routable before HTTP finishes");
        assert_eq!(early.kind, CallbackKind::MediaAnswer);
        assert_eq!(early.camera_generation, Some(1));

        let second = hub.register_camera_callbacks(&key, 2).unwrap();
        deliver(&hub, &first, CallbackKind::MediaAnswer);
        deliver(&hub, &second, CallbackKind::CallEnd);
        assert!(subscription.try_next().is_none(), "camera callbacks must not accept a previous operation or another kind");
        hub.clear_camera_callbacks(&key, 1);
        deliver(&hub, &second, CallbackKind::MediaRejection);
        assert_eq!(subscription.try_next().unwrap().camera_generation, Some(2));
        hub.clear_camera_callbacks(&key, 2);
        deliver(&hub, &second, CallbackKind::MediaAnswer);
        assert!(subscription.try_next().is_none(), "off invalidates the camera callbacks");

        deliver(&hub, &original, CallbackKind::CallEnd);
        let ending = subscription.try_next().unwrap();
        assert_eq!(ending.kind, CallbackKind::CallEnd);
        assert_eq!(ending.camera_generation, None);
    }
    #[tokio::test]
    async fn real_frame_routing_is_bound_to_nonce_and_disconnect_cancels_immediately() {
        let (hub, key, callbacks) = fixture();
        let mut subscription = hub.subscribe(key.clone(), callbacks.clone()).unwrap();
        assert!(matches!(
            hub.subscribe(key, callbacks.clone()),
            Err(SubscribeError::Busy)
        ));
        let frame=serde_json::json!({"url":callbacks.link(CallbackKind::Acceptance).replace("fixture.trouter.skype.com","evil.example"),"body":{}}).to_string();
        hub.handle_event(ost::trouter::data_frame_routed_event(&frame).unwrap());
        assert!(subscription.mailbox.queue.lock().unwrap().events.is_empty());
        deliver(&hub, &callbacks, CallbackKind::Acceptance);
        let event = subscription.next().await.unwrap();
        assert_eq!(event.kind, CallbackKind::Acceptance);
        assert_eq!(event.body["private"], "fixture-secret");
        assert!(!format!("{event:?}").contains("fixture-secret"));
        let mut cancellation = subscription.cancellation();
        hub.invalidate();
        cancellation.changed().await.unwrap();
        assert!(*cancellation.borrow());
        assert_eq!(subscription.stop.reason(), StopReason::TransportLost);
        assert!(subscription.next().await.is_none());
    }
    #[tokio::test]
    async fn short_url_and_equivalent_dual_url_reach_only_the_current_callback() {
        let (hub, key, callbacks) = fixture();
        let mut subscription = hub.subscribe(key, callbacks.clone()).unwrap();
        let full = callbacks.link(CallbackKind::Acceptance).to_owned();
        let short = full.split_once("/callAgent/").unwrap().1;
        let frame = serde_json::json!({"id":7,"method":"POST","shortUrl":format!("callAgent/{short}"),"body":{"callAcceptance":{}}}).to_string();
        hub.handle_event(ost::trouter::data_frame_routed_event(&frame).unwrap());
        assert_eq!(subscription.next().await.unwrap().kind, CallbackKind::Acceptance);

        let conflicting = serde_json::json!({"id":8,"method":"POST","url":callbacks.link(CallbackKind::CallEnd),"shortUrl":format!("callAgent/{short}"),"body":{"callAcceptance":{}}}).to_string();
        hub.handle_event(ost::trouter::data_frame_routed_event(&conflicting).unwrap());
        assert!(subscription.mailbox.queue.lock().unwrap().events.is_empty());

        let equivalent = serde_json::json!({"id":9,"method":"POST","url":full,"shortUrl":format!("callAgent/{short}"),"body":{"callAcceptance":{}}}).to_string();
        hub.handle_event(ost::trouter::data_frame_routed_event(&equivalent).unwrap());
        assert_eq!(subscription.next().await.unwrap().kind, CallbackKind::Acceptance);
    }
    #[tokio::test]
    async fn critical_events_evict_progress_and_overflow_stops_instead_of_losing_control() {
        let (hub, key, callbacks) = fixture();
        let mut sub = hub.subscribe(key, callbacks.clone()).unwrap();
        for _ in 0..MAX_EVENTS {
            deliver(&hub, &callbacks, CallbackKind::Progress);
        }
        deliver(&hub, &callbacks, CallbackKind::CallEnd);
        for _ in 0..MAX_EVENTS - 1 {
            assert_eq!(sub.next().await.unwrap().kind, CallbackKind::Progress);
        }
        assert_eq!(sub.next().await.unwrap().kind, CallbackKind::CallEnd);
        for _ in 0..MAX_EVENTS {
            deliver(&hub, &callbacks, CallbackKind::Progress);
        }
        deliver(&hub, &callbacks, CallbackKind::AddParticipantFailure);
        for _ in 0..MAX_EVENTS - 1 {
            assert_eq!(sub.next().await.unwrap().kind, CallbackKind::Progress);
        }
        assert_eq!(sub.next().await.unwrap().kind, CallbackKind::AddParticipantFailure);
        for _ in 0..MAX_EVENTS {
            deliver(&hub, &callbacks, CallbackKind::Progress);
        }
        deliver(&hub, &callbacks, CallbackKind::MediaRenegotiation);
        for _ in 0..MAX_EVENTS - 1 {
            assert_eq!(sub.next().await.unwrap().kind, CallbackKind::Progress);
        }
        assert_eq!(sub.next().await.unwrap().kind, CallbackKind::MediaRenegotiation);
        for _ in 0..=MAX_EVENTS {
            deliver(&hub, &callbacks, CallbackKind::Acceptance);
        }
        assert_eq!(sub.stop.reason(), StopReason::Overflow);
        assert!(sub.next().await.is_none());
    }
    #[test]
    fn dropping_stopped_owner_does_not_unregister_replacement() {
        let (hub, key, callbacks) = fixture();
        let old = hub.subscribe(key.clone(), callbacks.clone()).unwrap();
        old.stop.cancel();
        let replacement = hub.subscribe(key, callbacks.clone()).unwrap();
        drop(old);
        deliver(&hub, &callbacks, CallbackKind::Acceptance);
        assert_eq!(replacement.mailbox.queue.lock().unwrap().events.len(), 1);
        assert_eq!(replacement.stop.reason(), StopReason::Running);
    }
}
