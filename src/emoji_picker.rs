use crate::i18n::tr;
use crate::*;
use iced::widget::column;

fn category_icon(category: emoji::Category) -> Icon {
    match category {
        emoji::Category::Recent => Icon::EmojiClock,
        emoji::Category::Faces => Icon::EmojiFace,
        emoji::Category::Hands => Icon::EmojiHands,
        emoji::Category::People => Icon::EmojiPeople,
        emoji::Category::Animals => Icon::EmojiAnimals,
        emoji::Category::Food => Icon::EmojiFood,
        emoji::Category::Places => Icon::EmojiPlaces,
        emoji::Category::Objects => Icon::EmojiObjects,
        emoji::Category::Sports => Icon::EmojiSports,
        emoji::Category::Symbols => Icon::EmojiSymbols,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    All,
    Emoji,
    Gifs,
    Stickers,
}

impl Tab {
    const ALL: [Self; 4] = [Self::All, Self::Emoji, Self::Gifs, Self::Stickers];

    fn label(self) -> &'static str {
        match self {
            Self::All => tr("All"),
            Self::Emoji => "Emoji",
            Self::Gifs => "GIFs",
            Self::Stickers => tr("Stickers"),
        }
    }

    fn unavailable(self) -> Option<&'static str> {
        match self {
            Self::All | Self::Emoji => None,
            Self::Gifs => Some(tr("The GIF catalog is currently unavailable.")),
            Self::Stickers => Some(tr("The sticker catalog is currently unavailable.")),
        }
    }
}

pub fn view(state: &TeamsApp) -> Element<'_, AppMessage> {
    let mut tabs = row![].spacing(4);
    for tab in Tab::ALL {
        let active = state.emoji_tab == tab;
        tabs = tabs.push(
            button(
                column![
                    text(tab.label())
                        .size(17)
                        .font(if active { semibold() } else { typography::regular() })
                        .style(ui_text(if active { Tone::Text } else { Tone::Secondary })),
                    container(space::horizontal())
                        .width(Fill)
                        .max_width(72)
                        .height(3)
                        .style(ui_surface(if active { Tone::Accent } else { Tone::Transparent }, false, 1.0)),
                ]
                .spacing(8)
                .width(Fill)
                .align_x(Alignment::Center),
            )
            .width(Fill)
            .padding([8, 2])
            .on_press(AppMessage::EmojiTabSelected(tab))
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
        );
    }
    let mut panel = column![tabs].spacing(12).height(Fill);
    if let Some(status) = state.emoji_tab.unavailable() {
        panel = panel.push(
            container(text(status).size(14).style(ui_text(Tone::Secondary)))
                .center_x(Fill)
                .center_y(Fill),
        );
    } else {
        let search = text_input(tr("Find something fun"), &state.emoji_query)
            .id("composer-emoji-search")
            .size(16)
            .padding([8, 12])
            .on_input(AppMessage::EmojiSearchChanged)
            .style(ui_input(true));
        let searching = !state.emoji_query.trim().is_empty();
        let overview = state.emoji_tab == Tab::All;
        let category = if overview {
            emoji::Category::Recent
        } else {
            state.emoji_category
        };
        let mut entries =
            emoji::visible(category, &state.emoji_query, &state.preferences.recent_emojis);
        let heading = if searching {
            tr("Results")
        } else if overview {
            "Emoji"
        } else if category == emoji::Category::Recent && state.preferences.recent_emojis.is_empty() {
            "Emojis"
        } else {
            category.label()
        };
        let mut header = row![text(heading).size(16).style(ui_text(Tone::Text))]
            .align_y(Alignment::Center);
        if overview && !searching {
            entries.truncate(6);
            header = header.push(space::horizontal()).push(
                button(text(tr("See all  ›")).size(15))
                    .padding([4, 8])
                    .on_press(AppMessage::EmojiTabSelected(Tab::Emoji))
                    .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Secondary, 4.0)),
            );
        }
        let mut content = column![header, emoji_grid(state, &entries)].spacing(12);
        if overview && !searching {
            for (tab, status) in Tab::ALL
                .into_iter()
                .filter_map(|tab| tab.unavailable().map(|status| (tab, status)))
            {
                content = content.push(
                    column![
                        crate::divider(),
                        text(tab.label()).size(16).style(ui_text(Tone::Text)),
                        text(status).size(14).style(ui_text(Tone::Secondary)),
                    ]
                    .spacing(8),
                );
            }
        }
        panel = panel.push(search).push(
            scrollable(content)
                .id("composer-emoji-grid")
                .direction(scroll_axis())
                .style(ui_scroll)
                .height(Fill),
        );
        if !overview {
            panel = panel.push(crate::divider()).push(category_row(category));
        }
    }
    container(panel)
        .padding(12)
        .width(380)
        .height(490)
        .style(crate::message_popup_style)
        .into()
}

fn emoji_grid(state: &TeamsApp, entries: &[&emoji::Entry]) -> Element<'static, AppMessage> {
    let mut grid = column![].spacing(4);
    for entries in entries.chunks(6) {
        let mut line = row![].spacing(8);
        for entry in entries {
            let value = state.preferences.skin_tone.apply(entry.value);
            line = line.push(hint(
                button(
                    text(value)
                        .size(30)
                        .font(typography::EMOJI)
                        .shaping(iced::widget::text::Shaping::Advanced),
                )
                .width(48)
                .height(48)
                .padding(4)
                .on_press(AppMessage::InsertComposerEmoji(entry.value.into()))
                .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
                entry.label(),
            ));
        }
        grid = grid.push(line);
    }
    if entries.is_empty() {
        grid = grid.push(
            text(tr("No emoji found."))
                .size(14)
                .style(ui_text(Tone::Secondary)),
        );
    }
    grid.into()
}

fn category_row(selected: emoji::Category) -> Element<'static, AppMessage> {
    let mut categories = row![].spacing(2).align_y(Alignment::Center);
    for category in emoji::Category::ALL {
        let active = selected == category;
        let tone = if active { Tone::Accent } else { Tone::Secondary };
        let background = if active { Tone::Selected } else { Tone::Transparent };
        categories = categories.push(hint(
            button(icon_tone(category_icon(category), tone, 20.0))
                .width(32)
                .height(36)
                .padding(6)
                .on_press(AppMessage::EmojiCategorySelected(category))
                .style(ui_button(background, Tone::Hover, Tone::Text, 4.0)),
            category.label(),
        ));
    }
    categories.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn searching_and_choosing_an_emoji_preserve_the_captured_draft_range_and_history() {
        let mut state = TeamsApp::from_client_id(String::new());
        let _ = update(&mut state, AppMessage::DraftChanged("Olá mundo".into()));
        let key = state.draft_key().unwrap();
        state.draft_selection = Some((key, 4, 9));
        let history = state.chat_render_revision;
        let _ = update(&mut state, AppMessage::ToggleComposerEmoji);
        assert!(state.composer_emoji_open);
        assert_eq!(state.emoji_tab, Tab::All);
        let _ = update(&mut state, AppMessage::EmojiSearchChanged("programadora".into()));
        assert_eq!(state.emoji_query, "programadora");
        for tab in [Tab::Gifs, Tab::Stickers, Tab::Emoji] {
            let _ = update(&mut state, AppMessage::EmojiTabSelected(tab));
            assert_eq!(state.emoji_tab, tab);
            assert_eq!(state.emoji_query, "programadora");
        }
        let _ = update(&mut state, AppMessage::EmojiCategorySelected(emoji::Category::People));
        assert!(state.emoji_query.is_empty());
        let _ = update(&mut state, AppMessage::InsertComposerEmoji("👩‍💻".into()));
        assert_eq!(state.draft, "Olá 👩‍💻");
        assert_eq!(state.composer_content.text(), "Olá 👩‍💻");
        assert_eq!(state.preferences.recent_emojis, ["👩‍💻"]);
        assert!(!state.composer_emoji_open);
        assert!(!state.preferences_saving);
        assert_eq!(state.chat_render_revision, history);
        let _ = update(&mut state, AppMessage::ToggleComposerEmoji);
        assert_eq!(state.emoji_tab, Tab::All);
    }
}
