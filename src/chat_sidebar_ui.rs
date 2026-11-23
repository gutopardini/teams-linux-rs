//! Chat sidebar: presentation and local quick views, retained between edits.
use crate::i18n::tr;
use crate::graph::MessageSearchResult;
use crate::*;
use iced::widget::{column, rule};
use std::collections::{HashMap, HashSet};

pub fn view(state: &TeamsApp, shared_header: bool) -> Element<'static, AppMessage> {
    performance::chat_list_built();
    let search_button = hint(
        button(icon_tone(Icon::Search, if state.chat_list_search.is_some() { Tone::Accent } else { Tone::Secondary }, 18.0))
            .padding(9)
            .on_press(AppMessage::ToggleChatListSearch)
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 6.0)),
        tr("Filter by person or chat name"),
    );
    let header = row![
        text("Chat")
            .size(22)
            .font(semibold())
            .style(ui_text(Tone::Text)),
        space::horizontal(),
        hint(
            button(icon_tone(Icon::More, Tone::Secondary, 18.0))
                .padding(7)
                .on_press(AppMessage::ToggleSidebarMenu)
                .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
            tr("More list options")
        ),
        search_button,
        toolbar_icon(Icon::NewChat),
    ]
    .spacing(4)
    .align_y(Alignment::Center);
    let filter = |label: &'static str, selected, action| {
        button(text(label).size(13))
            .padding([4, 10])
            .on_press(action)
            .style(appearance::ui_filter(selected))
    };
    let filters = row![
        filter(
            tr("Unread"),
            state.chat_list_filter == ChatListFilter::Unread,
            AppMessage::SetChatListFilter(ChatListFilter::Unread)
        ),
        filter(
            "Chats",
            state.chat_list_filter == ChatListFilter::Chats,
            AppMessage::SetChatListFilter(ChatListFilter::Chats)
        ),
        filter(
            tr("Meeting chats"),
            state.chat_list_filter == ChatListFilter::Meetings,
            AppMessage::SetChatListFilter(ChatListFilter::Meetings)
        ),
    ]
    .spacing(5)
    .align_y(Alignment::Center)
    .wrap();

    let query = sidebar_query(state);
    let mut conversations = column![].spacing(CHAT_ROW_GAP);
    if matches!(
        state.chat_list_filter,
        ChatListFilter::Drafts | ChatListFilter::Mentions | ChatListFilter::Hidden
    ) {
        conversations = conversations.push(
            button(
                row![
                    icon_tone(Icon::Back, Tone::Accent, 16.0),
                    text(tr("All chats")).size(13)
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding([8, 12])
            .on_press(AppMessage::SetChatListFilter(ChatListFilter::Recent))
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Accent, 4.0)),
        );
    }
    let grouped = grouped(state);
    if grouped {
        conversations = conversations.push(section_heading(
            state,
            chat_sidebar::Section::QuickViews,
            tr("Quick views"),
        ));
        if !state.chat_organization.quick_collapsed {
            conversations = conversations.push(quick_view(
                state,
                Icon::Mention,
                tr("Mentions"),
                ChatListFilter::Mentions,
            ));
            conversations = conversations.push(quick_view(
                state,
                Icon::Drafts,
                tr("Drafts"),
                ChatListFilter::Drafts,
            ));
        }
        conversations = conversations.push(divider());
        conversations = conversations.push(section_heading(
            state,
            chat_sidebar::Section::Favorites,
            tr("Favorites"),
        ));
        if !state.chat_organization.favorites_collapsed {
            let chats_by_id = chats_by_id(state);
            let mut count = 0;
            for id in &state.chat_organization.favorites {
                let Some(chat) = chats_by_id.get(id.as_str()).copied() else {
                    continue;
                };
                if !chat_matches_filter(chat, state.chat_list_filter) {
                    continue;
                }
                count += 1;
                conversations = conversations.push(chat_row(state, chat));
            }
            if count == 0 && state.chat_list_filter == ChatListFilter::Recent {
                conversations = conversations.push(
                    container(
                        text(tr("Add from a conversation menu"))
                            .size(12)
                            .style(ui_text(Tone::Muted)),
                    )
                    .padding([8, 24]),
                );
            }
        }
        conversations = conversations.push(space::vertical().height(12));
        conversations = conversations.push(section_heading(
            state,
            chat_sidebar::Section::Chats,
            "Chats",
        ));
    } else {
        conversations = conversations.push(
            container(
                text(match state.chat_list_filter {
                    ChatListFilter::Drafts => tr("Drafts"),
                    ChatListFilter::Mentions => tr("Mentions"),
                    ChatListFilter::Hidden => tr("Hidden chats"),
                    _ => tr("Results"),
                })
                .size(14)
                .style(ui_text(Tone::Secondary)),
            )
            .padding([8, 16]),
        );
    }
    let mut visible_count = 0;
    if state.chat_list_filter == ChatListFilter::Mentions {
        conversations = conversations.push(
            container(
                text(tr("In conversations loaded on this device"))
                    .size(12)
                    .style(ui_text(Tone::Muted)),
            )
            .padding([8, 16]),
        );
        let mut hits = Vec::new();
        let mut seen_mentions = HashSet::new();
        let visible_chats: HashSet<&str> = state
            .chats
            .iter()
            .filter(|chat| chat.hidden != Some(true) && title_matches_query(&chat.title, &query))
            .map(|chat| chat.id.as_str())
            .collect();
        for (id, messages) in state
            .conversation_cache
            .histories()
            .map(|(id, history)| (id, history.messages.as_slice()))
            .chain(
                state
                    .selected_chat
                    .as_deref()
                    .map(|id| (id, state.messages.as_slice())),
            )
        {
            if !visible_chats.contains(id) {
                continue;
            }
            for message in messages
                .iter()
                .filter(|m| m.mentions_me && !m.is_deleted && !m.is_mine)
            {
                if seen_mentions.insert((id, message.id.as_str())) {
                    hits.push((id, message));
                }
            }
        }
        hits.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
        for (id, message) in hits.into_iter().take(100) {
            let title = state
                .chats
                .iter()
                .find(|chat| chat.id == id)
                .map(|chat| chat.title.as_str())
                .unwrap_or("Chat");
            let hit = MessageSearchResult {
                message_id: message.id.clone(),
                chat_id: id.to_owned(),
                team_id: None,
                channel_id: None,
                sender: message.author.clone(),
                timestamp: message.timestamp.clone(),
                preview: message.body.clone(),
                subject: None,
            };
            conversations = conversations.push(
                button(
                    column![
                        text(preview_line(&format!("{} · {}", message.author, title), 46))
                            .size(13)
                            .style(ui_text(Tone::Text)),
                        text(preview_line(&message.body, 66))
                            .size(12)
                            .style(ui_text(Tone::Secondary)),
                    ]
                    .spacing(4),
                )
                .padding([10, 16])
                .width(Fill)
                .on_press(AppMessage::OpenGlobalMessageSearchHit(hit))
                .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
            );
            visible_count += 1;
        }
    } else if !grouped || !state.chat_organization.chats_collapsed {
        for chat in &state.chats {
            if !listed_chat(state, chat, &query, grouped) {
                continue;
            }
            visible_count += 1;
            conversations = conversations.push(chat_row(state, chat));
        }
    }
    if visible_count == 0 && (!grouped || !state.chat_organization.chats_collapsed) {
        let status = match state.chat_list_filter {
            _ if !query.is_empty() => tr("No conversations found."),
            ChatListFilter::Drafts => tr("No chat drafts."),
            ChatListFilter::Mentions => tr("No mentions in loaded messages."),
            ChatListFilter::Hidden => tr("No hidden conversations."),
            ChatListFilter::Unread => tr("The service reported no unread conversations."),
            ChatListFilter::Meetings => tr("No meeting chats."),
            _ if state.loading_chats => tr("Loading conversations…"),
            _ => tr("No conversations found."),
        };
        conversations = conversations
            .push(container(text(status).size(12).style(ui_text(Tone::Muted))).padding([12, 16]));
    }
    let header = if shared_header {
        container(header).padding([0, 40]).center_y(appearance::metrics::CHAT_HEADER)
    } else {
        container(header).padding([20, 40])
    };
    let mut top = column![header, crate::divider()].spacing(0);
    if let Some(query) = &state.chat_list_search {
        top = top.push(container(
            text_input(tr("Filter by person or chat name"), query)
                .id("chat-list-search")
                .on_input(AppMessage::ChatListSearchChanged)
                .size(16)
                .padding([8, 12])
                .style(ui_input(false))
                .width(Fill),
        ).padding(iced::Padding { top: 12.0, right: 20.0, left: 40.0, bottom: 0.0 }));
    }
    top = top.push(container(filters).padding([12, 40]));
    if state.sidebar_menu_open {
        top = top.push(
            button(text(tr("Hidden chats")).size(13))
                .padding([8, 12])
                .width(Fill)
                .on_press(AppMessage::SetChatListFilter(ChatListFilter::Hidden))
                .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 4.0)),
        );
    }
    let content = column![
        top,
        rule::horizontal(1).style(|theme| rule::Style {
            color: tone(theme, Tone::Border),
            ..rule::default(theme)
        }),
        container(
            scrollable(conversations)
                .id("chat-list")
                .direction(scrollable::Direction::Vertical(
                    scrollable::Scrollbar::new().width(0).scroller_width(0),
                ))
                .style(ui_scroll)
                .height(Fill)
                .on_scroll(|viewport| AppMessage::ChatListScrolled(
                    viewport.absolute_offset().y,
                    viewport.bounds().height
                ))
        )
        .padding(iced::Padding { left: 20.0, right: 8.0, ..Default::default() }),
    ]
    .spacing(0);
    container(content)
        .width(Fill)
        .height(Fill)
        .padding(iced::Padding {
            bottom: 12.0,
            ..Default::default()
        })
        .style(ui_surface(Tone::Sidebar, false, 0.0))
        .into()
}

fn chat_row(state: &TeamsApp, chat: &ChatSummary) -> Element<'static, AppMessage> {
    let selected = state.selected_chat.as_deref() == Some(chat.id.as_str());
    let draft_preview = draft_preview(state, &chat.id);
    let preview = if state.chat_list_filter == ChatListFilter::Drafts {
        draft_preview.as_deref().unwrap_or(&chat.preview)
    } else {
        chat.preview.as_str()
    };
    let unread = chat.unread == Some(true);
    let activity = chat_time::label(
        chat.last_message_time.as_deref(),
        chrono::Local::now().fixed_offset(),
    );
    let mut heading = row![container(conversation_label(
        preview_line(&chat.title, 160),
        appearance::metrics::CHAT_TITLE,
        if unread {
            semibold()
        } else {
            typography::regular()
        },
        if selected || unread {
            Tone::Text
        } else {
            Tone::Secondary
        }
    )).width(Fill).clip(true)]
    .spacing(6)
    .align_y(Alignment::Center);
    if let Some(activity) = activity {
        heading = heading.push(
            text(activity)
                .size(13)
                .align_x(Alignment::End)
                .style(ui_text(if unread { Tone::Text } else { Tone::Muted })),
        );
    }
    let typing = state
        .typing_indicators
        .has_typing_users(&chat.id, std::time::Instant::now());
    let preview_row: Element<'_, AppMessage> = if typing {
        typing_dots(state.typing_pulse, false)
    } else {
        chat_preview_line(preview, chat.is_group, unread)
    };
    let preview_row: Element<'_, AppMessage> = if unread {
        row![
            container(preview_row).width(Fill),
            container(space::vertical()).width(8).height(8)
                .style(ui_surface(Tone::Accent, false, 4.0)),
        ].spacing(8).align_y(Alignment::Center).width(Fill).into()
    } else {
        preview_row
    };
    let accessibility_hint = if typing {
        let name = state
            .typing_indicators
            .senders(&chat.id, std::time::Instant::now())
            .into_iter()
            .find_map(|sender| {
                sender
                    .display_name
                    .as_deref()
                    .and_then(crate::sender_names::known)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| chat.title.clone());
        crate::i18n::format("{name}\nis typing", &[("name", &name)])
    } else if unread {
        format!("Unread\n{}\n{}", chat.title, preview)
    } else {
        format!("{}\n{}", chat.title, preview)
    };
    let chat_details = column![heading, preview_row].spacing(2).width(Fill);
    button(row![
        sidebar_avatar(state, chat),
        hint(chat_details, accessibility_hint),
    ].spacing(12).align_y(Alignment::Center))
        .width(Fill)
        .height(CHAT_ROW_HEIGHT)
        .padding(iced::Padding { top: 8.0, right: 12.0, bottom: 8.0, left: 20.0 })
        .on_press(AppMessage::SelectConversation(chat.id.clone()))
        .style(ui_button(
            if selected { Tone::Selected } else { Tone::Transparent },
            if selected { Tone::SelectedHover } else { Tone::Hover },
            Tone::Text,
            7.0,
        ))
        .into()
}

fn sidebar_avatar(state: &TeamsApp, chat: &ChatSummary) -> Element<'static, AppMessage> {
    let avatar = state.chat_avatar(chat, appearance::metrics::CHAT_AVATAR);
    if chat.is_group {
        return avatar;
    }
    let Some(person_id) = state.chat_person_id(chat) else {
        return avatar;
    };
    let name = chat.title.clone();
    let chat_id = chat.id.clone();
    let photo = state.avatars.photo(&person_id).cloned();
    let presence = state.presence.contact(&person_id, Instant::now()).map(|observed| observed.value);
    let details = state.global_people_results.iter().find(|person| person.id == person_id).cloned();
    let preview = move || {
        let picture: Element<'_, AppMessage> = if let Some(photo) = &photo {
            iced::widget::image(photo.clone()).width(56).height(56)
                .content_fit(iced::ContentFit::Cover).into()
        } else {
            crate::avatar(&name, 56.0)
        };
        let mut profile = column![text(name.clone()).size(17).font(semibold())].spacing(4).width(Fill);
        if let Some(title) = details.as_ref().and_then(|person| person.job_title.as_ref()) {
            profile = profile.push(text(title.clone()).size(13).style(ui_text(Tone::Secondary)));
        }
        if let Some(presence) = &presence {
            profile = profile.push(text(presence.label()).size(13).style(ui_text(Tone::Secondary)));
        }
        let mut card = column![row![picture, profile].spacing(12).align_y(Alignment::Center)].spacing(12);
        if let Some(email) = details.as_ref().map(|person| &person.email).filter(|email| !email.is_empty()) {
            card = card.push(text(email.clone()).size(13).style(ui_text(Tone::Secondary)));
        }
        if let Some(note) = presence.as_ref().and_then(|presence| presence.status_note.as_ref()) {
            card = card.push(text(note.message.clone()).size(13).style(ui_text(Tone::Secondary)));
        }
        card = card.push(button(row![icon_tone(Icon::Chat, Tone::Accent, 16.0), text(tr("Start chat")).size(13)]
            .spacing(8).align_y(Alignment::Center))
            .padding([8, 12])
            .on_press(AppMessage::SelectConversation(chat_id.clone()))
            .style(ui_button(Tone::Hover, Tone::SelectedHover, Tone::Text, 6.0)));
        container(card).width(308).padding(16).style(|theme| container::Style {
            shadow: iced::Shadow {
                color: Color::BLACK.scale_alpha(0.34),
                offset: iced::Vector::new(0.0, 4.0),
                blur_radius: 14.0,
            },
            ..ui_surface(Tone::Surface, true, 8.0)(theme)
        }).into()
    };
    message_actions::view(avatar, preview, None, message_actions::Config {
        context: chat.id.clone(),
        placement: message_actions::Placement::Avatar,
        enabled: !(state.group_details_open || state.new_conversation_open || state.image_viewer.is_some()
            || state.profile_menu_open || state.person_card.is_some() || state.app_menu.surface.is_some()),
        force_visible: state.demo_mode && chat.id == "chat-ana"
            && std::env::args().any(|arg| arg == "--avatar-card-demo"),
        reaction_picker: false,
    })
}

fn chat_preview_line(preview: &str, group: bool, unread: bool) -> Element<'static, AppMessage> {
    let face = if unread { semibold() } else { typography::regular() };
    let body_tone = if unread { Tone::Text } else { Tone::Muted };
    let Some((sender, body)) = graph::split_preview_sender(preview, group) else {
        return container(conversation_label(
            preview_line(preview, 160),
            appearance::metrics::CHAT_PREVIEW,
            face,
            body_tone,
        ))
        .width(Fill)
        .clip(true)
        .into();
    };
    row![
        text(format!("{sender}:"))
            .size(appearance::metrics::CHAT_PREVIEW)
            .font(face)
            .style(ui_text(if unread { Tone::Text } else { Tone::Secondary })),
        container(conversation_label(
            preview_line(body, 160),
            appearance::metrics::CHAT_PREVIEW,
            face,
            body_tone,
        ))
        .width(Fill)
        .clip(true),
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .into()
}

pub(crate) fn typing_dots(phase: u8, accent: bool) -> Element<'static, AppMessage> {
    let active = usize::from(phase) % 4;
    let mut dots = row![].spacing(4).align_y(Alignment::Center);
    for index in 0..3 {
        let on = active == index;
        dots = dots.push(
            container(space::horizontal())
                .width(6)
                .height(6)
                .style(ui_surface(
                    if on && accent {
                        Tone::Accent
                    } else if on {
                        Tone::Text
                    } else {
                        Tone::Muted
                    },
                    false,
                    3.0,
                )),
        );
    }
    container(dots).height(16).align_y(Alignment::Center).into()
}

pub fn grouped(state: &TeamsApp) -> bool {
    state.chat_list_search.as_deref().is_none_or(|query| query.trim().is_empty()) && matches!(
        state.chat_list_filter,
        ChatListFilter::Recent
            | ChatListFilter::Chats
            | ChatListFilter::Meetings
            | ChatListFilter::Unread
    )
}

fn sidebar_query(state: &TeamsApp) -> String {
    state.chat_list_search.as_deref().unwrap_or_default().trim().to_lowercase()
}
pub fn has_draft(state: &TeamsApp, id: &str) -> bool {
    let key = format!("chat:{id}");
    state.drafts.get(&key).is_some_and(|s| !s.trim().is_empty())
        || state
            .image_drafts
            .get(&key)
            .is_some_and(|images| !images.is_empty())
        || state.document_drafts.contains_key(&key)
}
fn draft_preview(state: &TeamsApp, id: &str) -> Option<String> {
    let key = format!("chat:{id}");
    if let Some(value) = state
        .drafts
        .get(&key)
        .filter(|value| !value.trim().is_empty())
    {
        return Some(crate::i18n::format("Draft: {content}", &[("content", &value.trim())]));
    }
    has_draft(state, id).then(|| tr("Draft with an attachment").into())
}
fn section_heading(
    state: &TeamsApp,
    section: chat_sidebar::Section,
    label: &'static str,
) -> Element<'static, AppMessage> {
    let collapsed = state.chat_organization.collapsed(section);
    button(
        row![
            icon_tone(
                if collapsed {
                    Icon::ChevronRight
                } else {
                    Icon::ChevronDown
                },
                Tone::Muted,
                14.0
            ),
            text(label).size(14).style(ui_text(Tone::Secondary))
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .width(Fill)
    .padding([4, 20])
    .on_press_maybe(
        (state.demo_mode || state.chat_organization_loaded)
            .then_some(AppMessage::ToggleSidebarSection(section)),
    )
    .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 0.0))
    .into()
}
fn quick_view(
    state: &TeamsApp,
    icon: Icon,
    label: &'static str,
    filter: ChatListFilter,
) -> Element<'static, AppMessage> {
    button(
        row![
            icon_tone(icon, Tone::Secondary, 18.0),
            text(label).size(14).style(ui_text(Tone::Secondary))
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .width(Fill)
    .height(36)
    .padding(iced::Padding {
        top: 6.0,
        right: 20.0,
        bottom: 6.0,
        left: 40.0,
    })
    .on_press(AppMessage::SetChatListFilter(filter))
    .style(ui_button(
        if state.chat_list_filter == filter {
            Tone::Selected
        } else {
            Tone::Transparent
        },
        Tone::Hover,
        Tone::Text,
        4.0,
    ))
    .into()
}
fn chats_by_id(state: &TeamsApp) -> HashMap<&str, &ChatSummary> {
    let mut chats = HashMap::with_capacity(state.chats.len());
    for chat in &state.chats {
        chats.entry(chat.id.as_str()).or_insert(chat);
    }
    chats
}

fn listed_chat(state: &TeamsApp, chat: &ChatSummary, query: &str, grouped: bool) -> bool {
    chat_matches_filter(chat, state.chat_list_filter)
        && title_matches_query(&chat.title, query)
        && !(grouped && state.chat_organization.is_favorite(&chat.id))
        && !(state.chat_list_filter == ChatListFilter::Drafts && !has_draft(state, &chat.id))
}

/// `query` is already lowercased, matching the previous `to_lowercase` filter.
pub(crate) fn title_matches_query(title: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if title.is_ascii() && query.is_ascii() {
        let needle = query.as_bytes();
        return title.len() >= needle.len()
            && title.as_bytes().windows(needle.len()).any(|window| {
                window
                    .iter()
                    .zip(needle)
                    .all(|(byte, expected)| byte.to_ascii_lowercase() == *expected)
            });
    }
    title.to_lowercase().contains(query)
}

fn divider() -> Element<'static, AppMessage> {
    container(rule::horizontal(1).style(|theme| rule::Style {
        color: tone(theme, Tone::Border),
        ..rule::default(theme)
    }))
    .padding([4, 20])
    .into()
}

/// Estimate only sidebar row visibility, with a full row of overscan. Section
/// chrome is part of the scroll content, so it must not shift avatar requests.
pub fn avatar_window(state: &TeamsApp) -> Vec<&ChatSummary> {
    let query = sidebar_query(state);
    let grouped = grouped(state);
    let indexed_chats = (grouped && !state.chat_organization.favorites_collapsed).then(|| chats_by_id(state));
    let mut y = if grouped {
        28.0 + if state.chat_organization.quick_collapsed {
            0.0
        } else {
            72.0
        } + 10.0
            + 28.0
    } else {
        32.0
    };
    let mut rows = Vec::new();
    if grouped {
        if !state.chat_organization.favorites_collapsed {
            let mut count = 0;
            for id in &state.chat_organization.favorites {
                if let Some(chat) = indexed_chats
                    .as_ref()
                    .and_then(|chats| chats.get(id.as_str()).copied())
                    .filter(|chat| chat_matches_filter(chat, state.chat_list_filter))
                {
                    rows.push((y, chat));
                    y += CHAT_ROW_STRIDE;
                    count += 1;
                }
            }
            if count == 0 && state.chat_list_filter == ChatListFilter::Recent {
                y += 30.0;
            }
        }
        y += 48.0;
    }
    if state.chat_list_filter != ChatListFilter::Mentions
        && (!grouped || !state.chat_organization.chats_collapsed)
    {
        for chat in &state.chats {
            if !listed_chat(state, chat, &query, grouped) {
                continue;
            }
            rows.push((y, chat));
            y += CHAT_ROW_STRIDE;
        }
    }
    let start = (state.chat_list_offset - CHAT_ROW_STRIDE).max(0.0);
    let end = state.chat_list_offset + state.chat_list_height + CHAT_ROW_STRIDE;
    rows.into_iter()
        .filter(|(y, _)| *y + CHAT_ROW_HEIGHT >= start && *y <= end)
        .map(|(_, chat)| chat)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_time_uses_the_trailing_padding_in_read_and_unread_rows() {
        use iced::advanced::{Layout, layout, widget::{Id, Operation, Tree}};

        struct ActivityBounds(String, Option<iced::Rectangle>);
        impl Operation for ActivityBounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }

            fn text(&mut self, _id: Option<&Id>, bounds: iced::Rectangle, text: &str) {
                if text == self.0 { self.1 = Some(bounds); }
            }
        }

        let state = TeamsApp::from_client_id(String::new());
        let mut chat = ChatSummary {
            id: "fixture-chat".into(),
            title: "Architecture and engineering team chat".into(),
            preview: "Ana: A longer preview exercises the remaining row width".into(),
            last_message_time: Some("2026-11-19T16:28:00Z".into()),
            is_group: true,
            ..Default::default()
        };
        let label = chat_time::label(chat.last_message_time.as_deref(), chrono::Local::now().fixed_offset()).unwrap();
        let renderer = crate::chat_render_tests::headless_renderer();
        for width in [240.0, 426.0] {
            for unread in [false, true] {
                chat.unread = Some(unread);
                let mut row = chat_row(&state, &chat);
                let mut tree = Tree::new(row.as_widget());
                let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, CHAT_ROW_HEIGHT));
                let node = row.as_widget_mut().layout(&mut tree, &renderer, &limits);
                let mut bounds = ActivityBounds(label.clone(), None);
                row.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
                let time = bounds.1.expect("a date in both read and unread rows");
                assert!(time.width >= 28.0);
                assert!((time.x + time.width - (width - 12.0)).abs() < 1.0);
            }
        }
    }

    #[test]
    fn chat_title_search_matches_ascii_without_allocating_and_keeps_accents() {
        assert!(title_matches_query("Ana Costa", "ana"));
        assert!(title_matches_query("ANA", "ana"));
        assert!(!title_matches_query("Ana", "maria"));
        assert!(!title_matches_query("Ana", "anabela"));
        assert!(title_matches_query("João", "joão"));
        assert!(title_matches_query("Ação", "ação"));
        assert!(title_matches_query("Qualquer", ""));
    }
    #[test]
    fn sidebar_sections_filter_real_data_without_reordering_the_directory() {
        let mut state = TeamsApp::from_client_id(String::new());
        state.demo_mode = true;
        state.selected_chat = None;
        state.messages.clear();
        state.chats = vec![
            ChatSummary {
                id: "19:first@thread.v2".into(),
                title: "First".into(),
                ..Default::default()
            },
            ChatSummary {
                id: "19:favorite@thread.v2".into(),
                title: "Favorite".into(),
                ..Default::default()
            },
            ChatSummary {
                id: "19:meeting@thread.v2".into(),
                group_avatar: avatars::GroupHint {
                    meeting: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        state.chat_list_height = 1200.0;
        let ids = state
            .chats
            .iter()
            .map(|chat| chat.id.clone())
            .collect::<Vec<_>>();
        drop(update(
            &mut state,
            AppMessage::SetChatFavorite(ids[1].clone(), true),
        ));
        assert_eq!(
            avatar_window(&state)
                .iter()
                .map(|chat| chat.id.as_str())
                .collect::<Vec<_>>(),
            vec![ids[1].as_str(), ids[0].as_str(), ids[2].as_str()]
        );
        assert_eq!(
            state
                .chats
                .iter()
                .map(|chat| chat.id.clone())
                .collect::<Vec<_>>(),
            ids
        );
        drop(update(
            &mut state,
            AppMessage::ToggleSidebarSection(chat_sidebar::Section::Favorites),
        ));
        assert!(!avatar_window(&state).iter().any(|chat| chat.id == ids[1]));
        drop(update(
            &mut state,
            AppMessage::SetChatListFilter(ChatListFilter::Meetings),
        ));
        assert_eq!(avatar_window(&state).len(), 1);
        assert_eq!(avatar_window(&state)[0].id, ids[2]);
        drop(update(
            &mut state,
            AppMessage::SetChatListFilter(ChatListFilter::Meetings),
        ));
        assert_eq!(state.chat_list_filter, ChatListFilter::Recent);
        state.chats[1].hidden = Some(true);
        state.chat_organization.favorites_collapsed = false;
        assert!(!avatar_window(&state).iter().any(|chat| chat.id == ids[1]));
    }
    #[test]
    fn draft_view_tracks_text_and_attachments_and_stale_account_loads_are_ignored() {
        let mut state = TeamsApp::from_client_id(String::new());
        state.demo_mode = true;
        let id = state.chats[0].id.clone();
        let key = format!("chat:{id}");
        state.drafts.clear();
        state.image_drafts.clear();
        state.document_drafts.clear();
        state.drafts.insert(key.clone(), "  \n ".into());
        assert!(!has_draft(&state, &id));
        state.drafts.insert(key.clone(), "Draft real".into());
        assert!(has_draft(&state, &id));
        drop(update(
            &mut state,
            AppMessage::SetChatListFilter(ChatListFilter::Drafts),
        ));
        assert_eq!(avatar_window(&state).len(), 1);
        state.drafts.remove(&key);
        state.image_drafts.insert(key, vec![images::demo_image()]);
        assert!(has_draft(&state, &id));
        state.chat_organization.favorite(&id, true).unwrap();
        let epoch = state.session_epoch;
        drop(update(
            &mut state,
            AppMessage::SidebarLoaded(
                epoch.wrapping_add(1),
                "another-account".into(),
                Ok(Default::default()),
            ),
        ));
        assert!(state.chat_organization.is_favorite(&id));
    }
}
