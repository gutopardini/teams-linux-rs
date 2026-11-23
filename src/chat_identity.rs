//! Recover direct-chat titles only from names bound to exact person identities.
use crate::*;

/// Shared by titles, message authors, participant cards and reactions.
/// Only identity-bound data enters this index; a displayed header is not proof.
pub fn confirmed_names(state: &TeamsApp) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for chat in state
        .chats
        .iter()
        .filter(|chat| !chat.is_group && chat.peer_name_verified)
    {
        if let Some(id) = chat.person_id.as_deref() {
            sender_names::remember(&mut names, id, &chat.title);
        }
    }
    for (_, history) in state.conversation_cache.histories() {
        remember_authors(&mut names, &history.messages);
    }
    remember_authors(&mut names, &state.channel_messages);
    if state.selected_chat.is_some() && state.chat_messages_loaded_for == state.selected_chat {
        remember_authors(&mut names, &state.messages);
    }
    if let Some(info) = state
        .selected_chat
        .as_deref()
        .and_then(|id| state.avatars.group_info(id))
    {
        for member in info.members.iter().chain(&info.roster_names) {
            sender_names::remember(&mut names, &member.id, &member.name);
        }
    }
    for member in &state.group_members {
        if let Some(id) = member.person_id.as_deref() {
            // The details panel uses this label when the service omits a name.
            // It is not a verified display name and must not replace one.
            if member.display_name != "Participante" {
                sender_names::remember(&mut names, id, &member.display_name);
            }
        }
    }
    sender_names::remember(&mut names, &state.user_id, &state.user_name);
    names
}

fn remember_authors(names: &mut HashMap<String, String>, messages: &[GraphMessage]) {
    for message in messages.iter().filter(|message| !message.is_mine) {
        if let Some(id) = message.author_id.as_deref() {
            sender_names::remember(names, id, &message.author);
        }
    }
}

/// Normalize the directory after identity data arrives. Widgets read this
/// model directly; they do not resolve participants or invent display names.
pub fn repair_titles(state: &mut TeamsApp) -> bool {
    let Some(owner) = avatars::person_id(&state.user_id) else {
        return false;
    };
    let rebound = bind_peers(&mut state.chats, &owner);
    let names = confirmed_names(state);
    state
        .chats
        .iter_mut()
        .filter(|chat| !chat.is_group && !chat.id.starts_with("28:"))
        .fold(rebound, |changed, chat| {
            update_title(chat, &names) || changed
        })
}

fn bind_peers(chats: &mut [ChatSummary], owner: &str) -> bool {
    let mut changed = false;
    for chat in chats.iter_mut().filter(|chat| !chat.is_group) {
        // Encoded 1:1 IDs and roster IDs are two protocol representations of
        // the same identity. Normalize once on ingestion, never in the view.
        let peer = avatars::direct_peer(&chat.id, owner).or_else(|| {
            chat.person_id
                .as_deref()
                .and_then(avatars::person_id)
                .filter(|id| id != owner)
        });
        if peer != chat.person_id {
            chat.person_id = peer;
            chat.peer_name_verified = false;
            changed = true;
        }
    }
    changed
}

fn update_title(chat: &mut ChatSummary, names: &HashMap<String, String>) -> bool {
    let name = chat.person_id.as_ref().and_then(|id| names.get(id));
    let verified = name.is_some();
    let title = name.map(String::as_str).unwrap_or("Conversa direta");
    if chat.title == title && chat.peer_name_verified == verified {
        return false;
    }
    chat.title = title.to_owned();
    chat.peer_name_verified = verified;
    true
}

pub fn load_demo(state: &mut TeamsApp) {
    if !state.demo_mode {
        return;
    }
    state.user_id = "aaaaaaaa-1111-2222-3333-444444444444".into();
    let peer = "11111111-1111-1111-1111-111111111111";
    let chat = &mut state.chats[0];
    chat.id = format!("19:{}_{peer}@unq.gbl.spaces", state.user_id);
    chat.title = state.user_name.clone();
    chat.person_id = None;
    chat.peer_name_verified = false;
    state.selected_chat = Some(chat.id.clone());
    state.chat_messages_loaded_for = state.selected_chat.clone();
    for message in &mut state.messages {
        if message.is_mine {
            message.author_id = Some(state.user_id.clone());
        } else {
            message.author = "Blair Costa".into();
            message.author_id = Some(peer.into());
        }
    }
    repair_titles(state);
}

#[cfg(test)]
mod tests {
    use super::*;
    const OWNER: &str = "aaaaaaaa-1111-2222-3333-444444444444";
    const PEER: &str = "bbbbbbbb-1111-2222-3333-444444444444";
    fn state() -> TeamsApp {
        let mut state = TeamsApp::from_client_id(String::new());
        state.demo_mode = true;
        state.user_id = OWNER.into();
        state.user_name = "Alex Silva".into();
        let id = format!("19:{OWNER}_{PEER}@unq.gbl.spaces");
        state.selected_chat = Some(id.clone());
        state.chat_messages_loaded_for = Some(id.clone());
        state.chats = vec![ChatSummary {
            id,
            title: state.user_name.clone(),
            preview: "Você: aprovado".into(),
            last_message_time: Some("2026-10-30T19:29:00Z".into()),
            ..Default::default()
        }];
        state.messages = vec![
            GraphMessage {
                id: "1".into(),
                author: "Blair Costa".into(),
                author_id: Some(format!("8:ORGID:{}", PEER.to_uppercase())),
                ..Default::default()
            },
            GraphMessage {
                id: "2".into(),
                author: state.user_name.clone(),
                author_id: Some(OWNER.into()),
                is_mine: true,
                ..Default::default()
            },
        ];
        state
    }
    #[test]
    fn outgoing_last_message_never_renames_the_peer_to_self() {
        let mut state = state();
        let time = state.chats[0].last_message_time.clone();
        assert!(repair_titles(&mut state));
        let chat = &state.chats[0];
        assert_eq!(chat.title, "Blair Costa");
        assert_eq!(chat.person_id.as_deref(), Some(PEER));
        assert!(chat.peer_name_verified);
        assert_eq!(chat.last_message_time, time);
        assert_eq!(chat.preview, "Você: aprovado");
        assert_eq!(
            state.identity_names().get(PEER).map(String::as_str),
            Some("Blair Costa")
        );
        assert!(!repair_titles(&mut state));
    }
    #[test]
    fn missing_or_stale_author_metadata_uses_a_neutral_title_without_poisoning_names() {
        let mut state = state();
        state.chat_messages_loaded_for = None;
        assert!(repair_titles(&mut state));
        assert_eq!(state.chats[0].title, "Conversa direta");
        state.messages.clear();
        assert!(!state.identity_names().contains_key(PEER));
        state.chat_messages_loaded_for = state.selected_chat.clone();
        state.messages = vec![GraphMessage {
            author: state.user_name.clone(),
            author_id: Some(OWNER.into()),
            is_mine: true,
            ..Default::default()
        }];
        assert!(!repair_titles(&mut state));
        assert_eq!(state.chats[0].title, "Conversa direta");
    }
    #[test]
    fn homonyms_and_self_chats_are_disambiguated_by_id_not_display_name() {
        let mut state = state();
        state.messages[0].author = state.user_name.clone();
        repair_titles(&mut state);
        assert_eq!(state.chats[0].title, state.user_name);
        assert_eq!(state.chats[0].person_id.as_deref(), Some(PEER));
        state.chats.push(ChatSummary {
            id: format!("19:{OWNER}_{OWNER}@unq.gbl.spaces"),
            title: "Wrong fallback".into(),
            ..Default::default()
        });
        repair_titles(&mut state);
        assert_eq!(state.chats[1].title, state.user_name);
        assert_eq!(state.chats[1].person_id.as_deref(), Some(OWNER));
    }
    #[test]
    fn refresh_without_a_peer_name_keeps_an_identity_verified_title() {
        let mut state = state();
        repair_titles(&mut state);
        let mut stale = state.chats[0].clone();
        stale.title = "[Direct message]".into();
        stale.peer_name_verified = false;
        state.chats = graph::reconcile_chat_directory(
            &state.chats,
            vec![stale],
            state.selected_chat.as_deref(),
        );
        state.messages.clear();
        repair_titles(&mut state);
        assert_eq!(state.chats[0].title, "Blair Costa");
        assert!(state.chats[0].peer_name_verified);
    }

    #[test]
    fn loaded_history_and_directory_refresh_share_the_same_identity_pipeline() {
        let mut state = state();
        let messages = std::mem::take(&mut state.messages);
        state.chat_messages_loaded_for = None;
        let chat_id = state.selected_chat.clone().unwrap();
        let epoch = state.session_epoch;
        let sequence = state.initial_history_sequence;
        let session = TeamsSession::test_fixture();
        drop(update(
            &mut state,
            AppMessage::MessagesLoaded(
                epoch,
                sequence,
                chat_id.clone(),
                Ok((
                    session.clone(),
                    MessagePage {
                        messages,
                        before: None,
                    },
                )),
            ),
        ));
        assert_eq!(state.chats[0].title, "Blair Costa");
        assert_eq!(state.user_name, "Alex Silva");
        let mut remote = state.chats[0].clone();
        remote.title = "Alex Silva".into();
        remote.peer_name_verified = false;
        let revision = state.unread_state_revision;
        drop(update(
            &mut state,
            AppMessage::RefreshedChats(
                epoch,
                revision,
                Ok(LoadedChats {
                    session,
                    user_id: OWNER.into(),
                    user_name: "Alex Silva".into(),
                    user_email: String::new(),
                    company_name: String::new(),
                    chats: vec![remote],
                }),
            ),
        ));
        assert_eq!(state.chats[0].title, "Blair Costa");
        let revisions = (state.chat_list_revision, state.chat_render_revision);
        drop(update(
            &mut state,
            AppMessage::DraftChanged("Typing does not resolve names".into()),
        ));
        assert_eq!(
            (state.chat_list_revision, state.chat_render_revision),
            revisions
        );
    }
}
