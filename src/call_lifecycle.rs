//! Call lifecycle reducer kept independent of transport and media.
//!
//! The reducer makes stale responses and duplicate user actions harmless. A
//! transport adapter can translate accepted effects to signaling requests;
//! this module never starts network or audio work itself.

use std::collections::VecDeque;

const MAX_CALL_ID_CHARS: usize = 512;
const MAX_RECENT_INVITES: usize = 128;
const MAX_FAILURE_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPhase {
    Idle,
    IncomingRinging,
    OutgoingDialing,
    Accepting,
    Connecting,
    Active,
    Ending,
    Ended,
    Failed,
}

impl CallPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Ended | Self::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallKey {
    pub generation: u64,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSnapshot {
    pub key: Option<CallKey>,
    pub direction: Option<CallDirection>,
    pub phase: CallPhase,
    pub failure: Option<String>,
}

impl Default for CallSnapshot {
    fn default() -> Self {
        Self {
            key: None,
            direction: None,
            phase: CallPhase::Idle,
            failure: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallEffect {
    PlaceOutgoingCall(CallKey),
    AcceptIncomingCall(CallKey),
    RejectIncomingCall(CallKey),
    EndCall(CallKey),
    ReleaseResources(CallKey),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallTransition {
    pub changed: bool,
    pub effect: Option<CallEffect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallEvent {
    IncomingInvite { id: String },
    OutgoingRequested { id: String },
    Accept { key: CallKey },
    Reject { key: CallKey },
    Cancel { key: CallKey },
    Connecting { key: CallKey },
    Connected { key: CallKey },
    RemoteEnded { key: CallKey },
    EndConfirmed { key: CallKey },
    Failed { key: CallKey, reason: String },
}

#[derive(Debug, Default)]
pub struct CallLifecycle {
    generation: u64,
    snapshot: CallSnapshot,
    recent_incoming_ids: VecDeque<String>,
}

impl CallLifecycle {
    pub fn snapshot(&self) -> &CallSnapshot {
        &self.snapshot
    }

    pub fn apply(&mut self, event: CallEvent) -> CallTransition {
        match event {
            CallEvent::IncomingInvite { id } => {
                if self.begin_call(id, CallDirection::Incoming).is_none() {
                    return CallTransition::default();
                }
                self.snapshot.phase = CallPhase::IncomingRinging;
                CallTransition {
                    changed: true,
                    effect: None,
                }
            }
            CallEvent::OutgoingRequested { id } => {
                let Some(key) = self.begin_call(id, CallDirection::Outgoing) else {
                    return CallTransition::default();
                };
                self.snapshot.phase = CallPhase::OutgoingDialing;
                CallTransition {
                    changed: true,
                    effect: Some(CallEffect::PlaceOutgoingCall(key)),
                }
            }
            CallEvent::Accept { key } => {
                if !self.matches(&key) || self.snapshot.phase != CallPhase::IncomingRinging {
                    return CallTransition::default();
                }
                self.snapshot.phase = CallPhase::Accepting;
                CallTransition {
                    changed: true,
                    effect: Some(CallEffect::AcceptIncomingCall(key)),
                }
            }
            CallEvent::Reject { key } => {
                if !self.matches(&key) || self.snapshot.phase != CallPhase::IncomingRinging {
                    return CallTransition::default();
                }
                self.snapshot.phase = CallPhase::Ending;
                CallTransition {
                    changed: true,
                    effect: Some(CallEffect::RejectIncomingCall(key)),
                }
            }
            CallEvent::Cancel { key } => {
                if !self.matches(&key) {
                    return CallTransition::default();
                }
                let effect = match self.snapshot.phase {
                    CallPhase::IncomingRinging => CallEffect::RejectIncomingCall(key),
                    CallPhase::OutgoingDialing
                    | CallPhase::Accepting
                    | CallPhase::Connecting
                    | CallPhase::Active => CallEffect::EndCall(key),
                    CallPhase::Idle
                    | CallPhase::Ending
                    | CallPhase::Ended
                    | CallPhase::Failed => return CallTransition::default(),
                };
                self.snapshot.phase = CallPhase::Ending;
                CallTransition {
                    changed: true,
                    effect: Some(effect),
                }
            }
            CallEvent::Connecting { key } => {
                if !self.matches(&key)
                    || !matches!(
                        self.snapshot.phase,
                        CallPhase::OutgoingDialing | CallPhase::Accepting
                    )
                {
                    return CallTransition::default();
                }
                self.snapshot.phase = CallPhase::Connecting;
                CallTransition {
                    changed: true,
                    effect: None,
                }
            }
            CallEvent::Connected { key } => {
                if !self.matches(&key)
                    || !matches!(
                        self.snapshot.phase,
                        CallPhase::OutgoingDialing | CallPhase::Accepting | CallPhase::Connecting
                    )
                {
                    return CallTransition::default();
                }
                self.snapshot.phase = CallPhase::Active;
                CallTransition {
                    changed: true,
                    effect: None,
                }
            }
            CallEvent::RemoteEnded { key } => {
                if !self.matches(&key) || self.snapshot.phase.is_terminal() {
                    return CallTransition::default();
                }
                self.snapshot.phase = CallPhase::Ended;
                CallTransition {
                    changed: true,
                    effect: Some(CallEffect::ReleaseResources(key)),
                }
            }
            CallEvent::EndConfirmed { key } => {
                if !self.matches(&key) || self.snapshot.phase != CallPhase::Ending {
                    return CallTransition::default();
                }
                self.snapshot.phase = CallPhase::Ended;
                CallTransition {
                    changed: true,
                    effect: Some(CallEffect::ReleaseResources(key)),
                }
            }
            CallEvent::Failed { key, reason } => {
                if !self.matches(&key) || self.snapshot.phase.is_terminal() {
                    return CallTransition::default();
                }
                self.snapshot.phase = CallPhase::Failed;
                self.snapshot.failure = Some(safe_failure(&reason));
                CallTransition {
                    changed: true,
                    effect: Some(CallEffect::ReleaseResources(key)),
                }
            }
        }
    }

    fn begin_call(&mut self, id: String, direction: CallDirection) -> Option<CallKey> {
        if !matches!(self.snapshot.phase, CallPhase::Idle)
            && !self.snapshot.phase.is_terminal()
        {
            return None;
        }
        if id.chars().take(MAX_CALL_ID_CHARS + 1).count() > MAX_CALL_ID_CHARS {
            return None;
        }
        let id = id.trim();
        if id.is_empty() || id.chars().any(char::is_control) {
            return None;
        }
        if direction == CallDirection::Incoming
            && self.recent_incoming_ids.iter().any(|seen| seen == id)
        {
            return None;
        }
        self.generation = self.generation.checked_add(1)?;
        let key = CallKey {
            generation: self.generation,
            id: id.to_owned(),
        };
        if direction == CallDirection::Incoming {
            self.recent_incoming_ids.push_back(key.id.clone());
            if self.recent_incoming_ids.len() > MAX_RECENT_INVITES {
                self.recent_incoming_ids.pop_front();
            }
        }
        self.snapshot = CallSnapshot {
            key: Some(key.clone()),
            direction: Some(direction),
            phase: CallPhase::Idle,
            failure: None,
        };
        Some(key)
    }

    fn matches(&self, key: &CallKey) -> bool {
        self.snapshot.key.as_ref() == Some(key)
    }
}

fn safe_failure(reason: &str) -> String {
    let cleaned = reason
        .chars()
        .take(MAX_FAILURE_CHARS * 4)
        .filter(|character| !character.is_control())
        .take(MAX_FAILURE_CHARS)
        .collect::<String>();
    if cleaned.is_empty() {
        "A chamada falhou.".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incoming(lifecycle: &mut CallLifecycle, id: &str) -> CallKey {
        assert!(lifecycle
            .apply(CallEvent::IncomingInvite { id: id.into() })
            .changed);
        lifecycle.snapshot().key.clone().unwrap()
    }

    #[test]
    fn incoming_call_moves_through_acceptance_and_connection() {
        let mut lifecycle = CallLifecycle::default();
        let key = incoming(&mut lifecycle, "incoming-1");

        let accepted = lifecycle.apply(CallEvent::Accept { key: key.clone() });
        assert_eq!(accepted.effect, Some(CallEffect::AcceptIncomingCall(key.clone())));
        assert_eq!(lifecycle.snapshot().phase, CallPhase::Accepting);
        assert!(lifecycle.apply(CallEvent::Connecting { key: key.clone() }).changed);
        assert!(lifecycle.apply(CallEvent::Connected { key }).changed);
        assert_eq!(lifecycle.snapshot().phase, CallPhase::Active);
    }

    #[test]
    fn duplicate_accept_and_accept_reject_race_emit_only_one_effect() {
        let mut lifecycle = CallLifecycle::default();
        let key = incoming(&mut lifecycle, "incoming-race");

        assert!(lifecycle
            .apply(CallEvent::Accept { key: key.clone() })
            .changed);
        let duplicate = lifecycle.apply(CallEvent::Accept { key: key.clone() });
        let reject_race = lifecycle.apply(CallEvent::Reject { key });

        assert!(!duplicate.changed);
        assert!(duplicate.effect.is_none());
        assert!(!reject_race.changed);
        assert!(reject_race.effect.is_none());
        assert_eq!(lifecycle.snapshot().phase, CallPhase::Accepting);
    }

    #[test]
    fn canceling_outgoing_suppresses_a_late_connected_response() {
        let mut lifecycle = CallLifecycle::default();
        let started = lifecycle.apply(CallEvent::OutgoingRequested {
            id: "outgoing-1".into(),
        });
        let key = lifecycle.snapshot().key.clone().unwrap();
        assert_eq!(started.effect, Some(CallEffect::PlaceOutgoingCall(key.clone())));

        let canceled = lifecycle.apply(CallEvent::Cancel { key: key.clone() });
        assert_eq!(canceled.effect, Some(CallEffect::EndCall(key.clone())));
        assert_eq!(lifecycle.snapshot().phase, CallPhase::Ending);
        assert!(!lifecycle
            .apply(CallEvent::Connected { key: key.clone() })
            .changed);
        assert_eq!(lifecycle.snapshot().phase, CallPhase::Ending);
        let ended = lifecycle.apply(CallEvent::EndConfirmed { key: key.clone() });
        assert_eq!(ended.effect, Some(CallEffect::ReleaseResources(key)));
        assert_eq!(lifecycle.snapshot().phase, CallPhase::Ended);
    }

    #[test]
    fn stale_response_from_previous_call_cannot_activate_the_next_call() {
        let mut lifecycle = CallLifecycle::default();
        let old = incoming(&mut lifecycle, "incoming-old");
        assert!(lifecycle
            .apply(CallEvent::RemoteEnded { key: old.clone() })
            .changed);
        let current = incoming(&mut lifecycle, "incoming-new");

        assert!(!lifecycle
            .apply(CallEvent::Connected { key: old })
            .changed);
        assert_eq!(lifecycle.snapshot().key.as_ref(), Some(&current));
        assert_eq!(lifecycle.snapshot().phase, CallPhase::IncomingRinging);
    }

    #[test]
    fn duplicate_and_overlapping_invites_do_not_create_phantom_calls() {
        let mut lifecycle = CallLifecycle::default();
        let first = incoming(&mut lifecycle, "incoming-duplicate");
        assert!(!lifecycle
            .apply(CallEvent::IncomingInvite {
                id: "incoming-duplicate".into(),
            })
            .changed);
        assert!(!lifecycle
            .apply(CallEvent::IncomingInvite {
                id: "incoming-overlap".into(),
            })
            .changed);
        assert_eq!(lifecycle.snapshot().key.as_ref(), Some(&first));
        assert_eq!(lifecycle.snapshot().phase, CallPhase::IncomingRinging);

        let ended = lifecycle.apply(CallEvent::RemoteEnded { key: first.clone() });
        assert_eq!(
            ended.effect,
            Some(CallEffect::ReleaseResources(first.clone()))
        );
        assert!(!lifecycle
            .apply(CallEvent::IncomingInvite {
                id: "incoming-duplicate".into(),
            })
            .changed);
        assert_eq!(lifecycle.snapshot().phase, CallPhase::Ended);
    }

    #[test]
    fn incoming_cancel_uses_reject_effect_and_requires_confirmation() {
        let mut lifecycle = CallLifecycle::default();
        let key = incoming(&mut lifecycle, "incoming-cancel");
        let canceled = lifecycle.apply(CallEvent::Cancel { key: key.clone() });

        assert_eq!(
            canceled.effect,
            Some(CallEffect::RejectIncomingCall(key.clone()))
        );
        assert_eq!(lifecycle.snapshot().phase, CallPhase::Ending);
        let ended = lifecycle.apply(CallEvent::EndConfirmed { key: key.clone() });
        assert_eq!(ended.effect, Some(CallEffect::ReleaseResources(key)));
        assert_eq!(lifecycle.snapshot().phase, CallPhase::Ended);
    }

    #[test]
    fn remote_end_emits_resource_release_once() {
        let mut lifecycle = CallLifecycle::default();
        let key = incoming(&mut lifecycle, "incoming-ended");

        let ended = lifecycle.apply(CallEvent::RemoteEnded { key: key.clone() });
        assert!(ended.changed);
        assert_eq!(ended.effect, Some(CallEffect::ReleaseResources(key.clone())));

        let duplicate = lifecycle.apply(CallEvent::RemoteEnded { key: key.clone() });
        let late_confirmation = lifecycle.apply(CallEvent::EndConfirmed { key });
        assert!(!duplicate.changed);
        assert!(duplicate.effect.is_none());
        assert!(!late_confirmation.changed);
        assert!(late_confirmation.effect.is_none());
    }

    #[test]
    fn failure_is_terminal_bounded_and_does_not_accept_old_updates() {
        let mut lifecycle = CallLifecycle::default();
        let key = incoming(&mut lifecycle, "incoming-failed");
        let reason = format!("{}\nsecret-control", "x".repeat(MAX_FAILURE_CHARS + 80));
        let failed = lifecycle
            .apply(CallEvent::Failed {
                key: key.clone(),
                reason,
            });
        assert!(failed.changed);
        assert_eq!(
            failed.effect,
            Some(CallEffect::ReleaseResources(key.clone()))
        );

        assert_eq!(lifecycle.snapshot().phase, CallPhase::Failed);
        let failure = lifecycle.snapshot().failure.as_ref().unwrap();
        assert_eq!(failure.chars().count(), MAX_FAILURE_CHARS);
        assert!(!failure.chars().any(char::is_control));
        assert!(!lifecycle
            .apply(CallEvent::Connected { key })
            .changed);
    }

    #[test]
    fn malformed_call_ids_are_ignored_before_allocating_state() {
        let mut lifecycle = CallLifecycle::default();
        for id in ["", "  ", "bad\nid"] {
            assert!(!lifecycle
                .apply(CallEvent::IncomingInvite { id: id.into() })
                .changed);
        }
        let too_long = "x".repeat(MAX_CALL_ID_CHARS + 1);
        assert!(!lifecycle
            .apply(CallEvent::IncomingInvite { id: too_long })
            .changed);
        assert_eq!(lifecycle.snapshot(), &CallSnapshot::default());
    }
}
