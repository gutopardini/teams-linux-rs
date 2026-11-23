use crate::graph::{ChatSummary, is_private_feed_thread};
use std::collections::HashMap;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeetingAction { Join, Chat }

/// Each native listener owns its notification and is cancelled when the
/// account or the corresponding in-app banner goes away.
pub struct DesktopMeetingNotices {
    active: HashMap<String, Arc<AtomicBool>>,
    sender: mpsc::Sender<(String, Option<MeetingAction>)>,
    receiver: mpsc::Receiver<(String, Option<MeetingAction>)>,
}

impl Default for DesktopMeetingNotices {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self { active: HashMap::new(), sender, receiver }
    }
}

impl DesktopMeetingNotices {
    pub fn show(&mut self, id: &str, subject: &str, can_chat: bool, sound: bool) -> bool {
        // Automatic fixtures must never contact the user's notification service.
        if cfg!(test) { return false; }
        if self.active.len() >= 5 || self.active.contains_key(id) { return false; }
        let cancel = Arc::new(AtomicBool::new(false));
        let (id, subject, stop, sender) = (id.to_owned(), subject.to_owned(), cancel.clone(), self.sender.clone());
        let worker_id = id.clone();
        if std::thread::Builder::new().name("teams-meeting-notice".into()).spawn(move || {
            let result=native_meeting_notice(&worker_id, &subject, can_chat, sound, &stop);
            if result.is_err(){crate::live_trace::record("desktop_meeting_failed",0,1,0);}
            let action=result.ok().flatten();
            let _ = sender.send((worker_id, action));
        }).is_err() { return false; }
        self.active.insert(id, cancel);
        true
    }

    pub fn cancel(&mut self, id: &str) {
        if let Some(stop) = self.active.get(id) { stop.store(true, Ordering::Release); }
    }

    pub fn cancel_all(&mut self) {
        for stop in self.active.values() { stop.store(true, Ordering::Release); }
    }

    pub fn has_active(&self) -> bool { !self.active.is_empty() }

    pub fn drain(&mut self) -> Vec<(String, MeetingAction)> {
        let mut actions = Vec::new();
        while let Ok((id, action)) = self.receiver.try_recv() {
            self.active.remove(&id);
            if let Some(action) = action { actions.push((id, action)); }
        }
        actions
    }
}

impl Drop for DesktopMeetingNotices {
    fn drop(&mut self) { self.cancel_all(); }
}

fn native_meeting_notice(id: &str, subject: &str, can_chat: bool, sound: bool, cancelled: &AtomicBool) -> Result<Option<MeetingAction>, ()> {
    use crate::desktop_notifications::{MessageNotice,Kind};
    let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|_|())?;
    runtime.block_on(async {
        let connection=tokio::time::timeout(Duration::from_secs(5),zbus::Connection::session()).await.map_err(|_|())?.map_err(|_|())?;
        let proxy=zbus::Proxy::new(&connection,"org.freedesktop.Notifications","/org/freedesktop/Notifications","org.freedesktop.Notifications").await.map_err(|_|())?;
        let action=crate::desktop_notifications::meeting_notice(&proxy,&MessageNotice{
            chat_id:id.into(),title:subject.into(),body:"Reunião iniciada".into(),sound,kind:Kind::MeetingStarted{can_chat},
        },cancelled).await?;
        Ok(action.as_deref().and_then(|action|meeting_action(action,can_chat)))
    })
}

fn meeting_action(action: &str, can_chat: bool) -> Option<MeetingAction> {
    match action {
        "join" => Some(MeetingAction::Join),
        "chat" if can_chat => Some(MeetingAction::Chat),
        _ => None,
    }
}

/// Only a newer message from another identity can alert.
/// Established snapshots suppress startup/repeated polls; mute and the visible
/// conversation suppress the desktop alert while unread reconciliation continues.
pub fn apply_incoming_messages(
    previous: &[ChatSummary],
    latest: &mut [ChatSummary],
    visible_at_bottom: Option<&str>,
) -> Vec<crate::desktop_notifications::MessageNotice> {
    let mut notices = Vec::new();
    let timestamp = |value: Option<&str>| {
        value.and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
    };
    let newest_known = previous.iter().filter_map(|chat| timestamp(chat.last_message_time.as_deref())).max();
    for chat in latest {
        if is_private_feed_thread(&chat.id) { continue; }
        let old = previous.iter().find(|old| old.id == chat.id);
        if chat.hidden.or(old.and_then(|chat| chat.hidden)) == Some(true) { continue; }
        let old_time = old.and_then(|chat| timestamp(chat.last_message_time.as_deref()));
        let new_time = timestamp(chat.last_message_time.as_deref());
        let newer = match (old, new_time) {
            (Some(_), Some(new)) => old_time.is_some_and(|old| new > old),
            // A genuinely new unread thread may alert after an established
            // snapshot, but old directory entries/backfill must not ring.
            (None, Some(new)) => chat.unread == Some(true) && newest_known.is_some_and(|old| new > old),
            _ => false,
        };
        if newer && chat.last_message_is_mine == Some(false) {
            if visible_at_bottom != Some(chat.id.as_str()) { chat.unread = Some(true); }
            if visible_at_bottom != Some(chat.id.as_str()) && chat.muted.or(old.and_then(|chat| chat.muted)) != Some(true) {
                notices.push(crate::desktop_notifications::MessageNotice {
                    sound: true, kind: crate::desktop_notifications::Kind::Message,
                    chat_id: chat.id.clone(),
                    title: if chat.title.trim().is_empty() { "Nova mensagem no Teams".into() } else { chat.title.clone() },
                    body: chat.preview.clone(),
                });
            }
        }
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meeting_notification_actions_require_a_known_action_and_available_chat() {
        assert_eq!(meeting_action("chat", false), None);
        assert_eq!(meeting_action("other", true), None);
        assert_eq!(meeting_action("join", false), Some(MeetingAction::Join));
        assert_eq!(meeting_action("chat", true), Some(MeetingAction::Chat));
    }

    #[test]
    #[ignore = "requires an explicit interactive desktop notification test"]
    fn meeting_notification_service_accepts_a_fictional_notice() {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || native_meeting_notice("teams-linux-fictional-test", "Prévia de reunião", true, false, &worker_stop));
        std::thread::sleep(Duration::from_secs(3));
        stop.store(true, Ordering::Release);
        assert!(worker.join().unwrap().is_ok(), "desktop service did not accept the fictional notice");
    }

    #[test]
    fn desktop_alert_keeps_context_and_new_threads_do_not_replay_old_unread_history() {
        let old = chat("2026-11-09T10:00:00Z", Some(false));
        let mut new = chat("2026-11-09T10:01:00Z", Some(false));
        new.id = "new-chat".into(); new.title = "Equipe".into(); new.preview = "Ana: Olá".into(); new.unread = Some(true);
        let alerts = apply_incoming_messages(&[old.clone()], &mut [new.clone()], None);
        assert_eq!(alerts.len(), 1); assert_eq!(alerts[0].chat_id, "new-chat");
        assert_eq!(alerts[0].title, "Equipe"); assert_eq!(alerts[0].body, "Ana: Olá");
        assert!(apply_incoming_messages(&[], &mut [new.clone()], None).is_empty());
        new.last_message_time = Some("2026-11-08T09:00:00Z".into());
        assert!(apply_incoming_messages(&[old.clone()], &mut [new.clone()], None).is_empty());
        new.id = "48:notifications".into();
        assert!(apply_incoming_messages(&[old], &mut [new], None).is_empty());
    }

    fn chat(time: &str, is_mine: Option<bool>) -> ChatSummary {
        ChatSummary { id: "chat-1".into(), last_message_time: Some(time.into()), last_message_is_mine: is_mine, ..Default::default() }
    }

    #[test]
    fn incoming_updates_mark_unread_and_notify_once_without_a_service_badge() {
        let old = chat("2026-11-03T10:00:00Z", Some(false));
        let new = chat("2026-11-03T10:01:00Z", Some(false));
        let mut latest = vec![new.clone()];
        assert!(!apply_incoming_messages(&[old.clone()], &mut latest, None).is_empty());
        assert_eq!(latest[0].unread, Some(true));
        let mut muted_before = old.clone(); muted_before.muted = Some(true);
        assert!(apply_incoming_messages(&[muted_before], &mut [new.clone()], None).is_empty());
        assert!(apply_incoming_messages(&[], &mut [new.clone()], None).is_empty());
        assert!(apply_incoming_messages(&[new.clone()], &mut [new.clone()], None).is_empty());
        for sender in [None, Some(true)] {
            assert!(apply_incoming_messages(&[old.clone()], &mut [chat("2026-11-03T10:01:00Z", sender)], None).is_empty());
        }
        let mut muted = new.clone(); muted.muted = Some(true);
        let mut latest = vec![muted];
        assert!(apply_incoming_messages(&[old.clone()], &mut latest, None).is_empty());
        assert_eq!(latest[0].unread, Some(true));
        let mut hidden = new.clone(); hidden.hidden = Some(true);
        assert!(apply_incoming_messages(&[old.clone()], &mut [hidden], None).is_empty());
        assert!(apply_incoming_messages(&[old.clone()], &mut [chat("invalid", Some(false))], None).is_empty());
        let mut visible = vec![new];
        assert!(apply_incoming_messages(&[old], &mut visible, Some("chat-1")).is_empty());
        assert_eq!(visible[0].unread, None);
    }
}
