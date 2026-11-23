use super::*;

fn message(id: &str) -> GraphMessage {
    GraphMessage { id: id.into(), timestamp: id.into(), body: id.into(), ..Default::default() }
}

fn deliver(state: &mut TeamsApp, epoch: u64, sequence: u64, ids: &[&str]) {
    drop(update_inner(state, AppMessage::MessagesLoaded(epoch, sequence, "audit-a".into(),
        Ok((TeamsSession::test_fixture(), MessagePage {
            messages: ids.iter().map(|id| message(id)).collect(), before: None,
        })))));
}

#[test]
fn late_initial_request_cannot_remove_newer_messages_after_reselection() {
    let mut state = TeamsApp::from_client_id(String::new());
    state.demo_mode = false;
    state.session = Some(TeamsSession::test_fixture());
    drop(update_inner(&mut state, AppMessage::SelectConversation("audit-a".into())));
    let first = state.initial_history_sequence;
    drop(update_inner(&mut state, AppMessage::SelectConversation("audit-b".into())));
    drop(update_inner(&mut state, AppMessage::SelectConversation("audit-a".into())));
    let current = state.initial_history_sequence;
    let epoch = state.session_epoch;
    assert!(current > first);
    deliver(&mut state, epoch, current, &["001", "002"]);
    deliver(&mut state, epoch, first, &["001"]);
    assert_eq!(state.messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["001", "002"]);
}

#[test]
fn initial_page_keeps_send_confirmed_while_read_was_in_flight() {
    let mut state = TeamsApp::from_client_id(String::new());
    state.selected_chat = Some("audit-a".into());
    state.messages = vec![message("002")];
    let epoch = state.session_epoch;
    deliver(&mut state, epoch, 0, &["001"]);
    assert_eq!(state.messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["001", "002"]);
}

#[test]
fn stale_initial_success_cannot_restore_session_after_logout() {
    let mut state = TeamsApp::from_client_id(String::new());
    state.selected_chat = Some("audit-a".into());
    state.messages.clear();
    state.session_epoch = 2;
    deliver(&mut state, 1, 0, &["001"]);
    assert!(state.session.is_none());
    assert!(state.messages.is_empty());
}

#[test]
fn stale_initial_error_keeps_current_loading_and_status() {
    let mut state = TeamsApp::from_client_id(String::new());
    state.selected_chat = Some("audit-a".into());
    state.initial_history_sequence = 2;
    state.loading_messages = true;
    state.status = "current".into();
    let epoch = state.session_epoch;
    drop(update_inner(&mut state, AppMessage::MessagesLoaded(epoch, 1, "audit-a".into(), Err("old".into()))));
    assert!(state.loading_messages);
    assert_eq!(state.status, "current");
}

#[test]
fn older_page_for_another_chat_does_not_release_current_request() {
    let mut state = TeamsApp::from_client_id(String::new());
    state.selected_chat = Some("audit-b".into());
    state.loading_history = true;
    let epoch = state.session_epoch;
    drop(update_inner(&mut state, AppMessage::OlderMessagesLoaded(epoch, "audit-a".into(), Err("old".into()))));
    assert!(state.loading_history);
}

#[test]
fn identical_refresh_and_sidebar_scroll_retain_both_widget_trees() {
    use iced::advanced::widget::Tree;
    let mut state = TeamsApp::from_client_id(String::new());
    state.session = Some(TeamsSession::test_fixture());
    state.messages = vec![message("001"), message("002")];
    let chat_id = state.selected_chat.clone().unwrap();
    performance::start_counting();
    let panel = state.message_panel("Test", "", &state.messages, false, 1050.0, false);
    let mut history = Tree::new(panel.as_widget());
    drop(panel);
    let list = state.chat_list(312.into());
    let mut sidebar = Tree::new(list.as_widget());
    drop(list);
    let epoch = state.session_epoch;
    drop(update(&mut state, AppMessage::RefreshedMessages(epoch, chat_id.clone(),
        Ok((TeamsSession::test_fixture(), MessagePage { messages: vec![message("002")], before: None })))));
    drop(update(&mut state, AppMessage::ChatListScrolled(72.0, 700.0)));
    let panel = state.message_panel("Test", "", &state.messages, false, 1050.0, false);
    history.diff(panel.as_widget());
    drop(panel);
    let list = state.chat_list(312.into());
    sidebar.diff(list.as_widget());
    drop(list);
    assert_eq!(performance::history_builds(), 1);
    assert_eq!(performance::chat_list_builds(), 1);
    drop(update(&mut state, AppMessage::RefreshedMessages(epoch, chat_id,
        Ok((TeamsSession::test_fixture(), MessagePage { messages: vec![message("003")], before: None })))));
    let panel = state.message_panel("Test", "", &state.messages, false, 1050.0, false);
    history.diff(panel.as_widget());
    assert_eq!(performance::history_builds(), 2);
    assert_eq!(state.messages.len(), 3);
}

#[test]
fn unchanged_merge_keeps_storage_and_unsorted_input_is_still_normalized() {
    let mut messages = vec![message("001"), message("002")];
    let pointer = messages.as_ptr();
    assert!(!merge_messages(&mut messages, vec![message("002")]));
    assert_eq!(messages.as_ptr(), pointer);
    messages.reverse();
    assert!(merge_messages(&mut messages, vec![]));
    assert_eq!(messages[0].id, "001");
}

#[test]
fn native_history_keeps_every_message_without_estimated_spacers() {
    for count in [0, 40, 50, 100, 1000, 4000] {
        let window = native_history_window(count);
        assert_eq!(window.range, 0..count);
        assert_eq!(window.before_height, 0.0);
        assert_eq!(window.after_height, 0.0);
    }
}

#[test]
fn repeated_geometry_notifications_do_not_rebuild_native_history() {
    use iced::advanced::widget::Tree;
    let mut state = TeamsApp::from_client_id(String::new());
    state.messages = performance::messages(50);
    performance::start_counting();
    let panel = state.message_panel("Test", "", &state.messages, false, 1050.0, false);
    let mut tree = Tree::new(panel.as_widget());
    drop(panel);
    let chat_id = state.selected_chat.clone().unwrap();
    for index in 0..20 {
        drop(update(&mut state, AppMessage::HistoryScrolled(HistoryViewport {
            chat_id: chat_id.clone(), at_bottom: false, offset_y: index as f32 * 80.0,
            content_height: 18000.0 + index as f32, viewport_height: 720.0,
            viewport_width: 1050.0, compact: false,
        })));
        let panel = state.message_panel("Test", "", &state.messages, false, 1050.0, false);
        tree.diff(panel.as_widget());
    }
    assert_eq!(performance::history_builds(), 1);
}

#[test]
fn prefix_windows_match_height_windows_at_boundaries() {
    let heights = (0..1000).map(|i| 36.0 + (i % 100) as f32).collect::<Vec<_>>();
    let mut sum = 0.0;
    let prefix = heights.iter().map(|height| { sum += height; sum }).collect::<Vec<_>>();
    for offset in [None, Some(0.0), Some(300.0), Some(80000.0), Some(f32::NAN)] {
        assert_eq!(virtual_history_window(&heights, offset, 720.0),
            virtual_history_window_from_prefix(&prefix, offset, 720.0));
    }
}

#[test]
fn pending_synchronous_io_does_not_block_deleting_the_draft() {
    use std::{future::Future, task::{Context, Poll}};
    let (release, waiting) = std::sync::mpsc::channel();
    let mut request = Box::pin(background::run(move || {
        waiting.recv_timeout(Duration::from_secs(2)).map_err(|_| "fixture timeout".to_owned())?;
        Ok(())
    }));
    let waker = iced::futures::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    // Reproduce Iced's eager first poll on the UI caller, while I/O is blocked.
    assert!(matches!(request.as_mut().poll(&mut context), Poll::Pending));
    let mut state = TeamsApp::from_client_id(String::new());
    drop(update(&mut state, AppMessage::DraftChanged("Typing".into())));
    drop(update(&mut state, AppMessage::DraftChanged(String::new())));
    assert!(state.draft.is_empty());
    release.send(()).unwrap();
    iced::futures::executor::block_on(request).unwrap();
}

#[test]
fn restored_startup_waits_for_messages_and_drafts_in_either_order() {
    for drafts_first in [true, false] {
        let mut state = TeamsApp::from_client_id(String::new());
        state.startup_loading = true;
        state.session = Some(TeamsSession::test_fixture());
        state.loading_chats = true;
        state.loading_messages = true;
        state.drafts_loaded = false;
        state.settle_startup();
        assert!(state.startup_loading);
        state.loading_chats = false;
        if drafts_first { state.drafts_loaded = true; } else { state.loading_messages = false; }
        state.settle_startup();
        assert!(state.startup_loading);
        state.drafts_loaded = true;
        state.loading_messages = false;
        state.settle_startup();
        assert!(!state.startup_loading);
    }
}

#[test]
fn missing_session_or_startup_error_cannot_leave_loading_forever() {
    let mut state = TeamsApp::from_client_id(String::new());
    state.startup_loading = true;
    state.restoring_session = true;
    state.drafts_loaded = false;
    state.settle_startup();
    assert!(state.startup_loading);
    state.restoring_session = false;
    state.settle_startup();
    assert!(!state.startup_loading);
    state.startup_loading = true;
    state.session = Some(TeamsSession::test_fixture());
    state.status = "fixture failure".into();
    state.settle_startup();
    assert!(!state.startup_loading);
}

#[test]
fn reading_old_chat_does_not_follow_service_property_order() {
    let recent = ChatSummary { id: "recent".into(), last_message_time: Some("2026-10-28T14:00:00Z".into()), ..Default::default() };
    let old = ChatSummary { id: "old".into(), last_message_time: Some("2026-10-20T14:00:00Z".into()), unread: Some(true), ..Default::default() };
    let previous = vec![recent.clone(), old.clone()];
    let mut read = old.clone(); read.unread = Some(false);
    let result = graph::reconcile_chat_directory(&previous, vec![read, recent], Some("old"));
    assert_eq!(result.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["recent", "old"]);
    assert_eq!(result[1].unread, Some(false));
}

#[test]
fn only_new_message_activity_promotes_old_chat() {
    let recent = ChatSummary { id: "recent".into(), last_message_time: Some("2026-10-28T14:00:00Z".into()), ..Default::default() };
    let old = ChatSummary { id: "old".into(), last_message_time: Some("2026-10-20T14:00:00Z".into()), ..Default::default() };
    let previous = vec![recent.clone(), old.clone()];
    let mut new_activity = old;
    new_activity.last_message_time = Some("2026-10-28T12:00:00-03:00".into());
    let result = graph::reconcile_chat_directory(&previous, vec![recent, new_activity], None);
    assert_eq!(result[0].id, "old");
}

#[test]
fn equal_or_absent_message_times_preserve_previous_order_after_read() {
    for time in [None, Some("2026-10-28T14:00:00Z".to_owned())] {
        let a = ChatSummary { id: "a".into(), last_message_time: time.clone(), ..Default::default() };
        let b = ChatSummary { id: "b".into(), last_message_time: time, ..Default::default() };
        let result = graph::reconcile_chat_directory(&[a.clone(), b.clone()], vec![b, a], Some("b"));
        assert_eq!(result.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }
}

#[test]
fn partial_refresh_keeps_known_last_message_time() {
    let previous = vec![ChatSummary { id: "old".into(), last_message_time: Some("2026-10-20T14:00:00Z".into()), ..Default::default() }];
    let result = graph::reconcile_chat_directory(&previous, vec![ChatSummary { id: "old".into(), ..Default::default() }], None);
    assert_eq!(result[0].last_message_time, previous[0].last_message_time);
}

#[test]
fn automatic_presence_callback_keeps_manual_status_and_ignores_old_epoch() {
    let mut state = TeamsApp::from_client_id(String::new());
    state.presence.set_demo_status(presence::Availability::DoNotDisturb, &state.user_id);
    let epoch = state.session_epoch;
    drop(update_inner(&mut state, AppMessage::AutomaticPresenceFinished(epoch,
        Ok(desktop_presence::Report { registered: true, active: true, sent: true }))));
    assert_eq!(state.presence.selected_own(Instant::now()), Some(presence::Availability::DoNotDisturb));
    drop(update_inner(&mut state, AppMessage::AutomaticPresenceFinished(epoch.wrapping_sub(1), Err("stale".into()))));
    assert!(state.automatic_presence.error.is_none());
}
