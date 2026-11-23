//! Official expanded composer controls, backed by the existing rich draft model.
use crate::i18n::tr;
use crate::*;
use iced::widget::column;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Menu { FontSize, TextColor, Highlight, Table, More }

pub fn open_link(state: &mut TeamsApp) -> Task<AppMessage> {
    if !state.formatting_open || !(state.drafts_loaded || state.demo_mode) { return Task::none(); }
    let Some(key) = state.draft_key() else { return Task::none(); };
    let (start, end) = state.selected_draft_range().unwrap_or_else(||
        message_body::editor_cursor_range(&state.draft, state.composer_content.cursor()));
    use unicode_segmentation::UnicodeSegmentation;
    state.link_text = state.draft.graphemes(true).skip(start).take(end - start).collect();
    state.link_url = state.draft_formatting.get(&key).and_then(|formatting|
        formatting.links.iter().find(|range| range.start < end && range.end > start))
        .map(|range| range.url.clone()).unwrap_or_default();
    state.link_error.clear();
    state.link_target = Some((key, start, end));
    state.composer_menu = None;
    state.link_editor_open = true;
    iced::widget::operation::focus("draft-link-text")
}

pub fn apply_link(state: &mut TeamsApp) -> Task<AppMessage> {
    if !state.link_editor_open { return Task::none(); }
    let Some((key, start, end)) = state.link_target.clone() else { return Task::none(); };
    if state.draft_key().as_deref() != Some(key.as_str()) {
        return close_link(state);
    }
    let Some(url) = message_body::valid_link(&state.link_url) else {
        state.link_error = tr("Enter a valid http, https, or mailto address.").into();
        return Task::none();
    };
    if state.link_text.trim().is_empty() {
        state.link_error = tr("Enter the text to display.").into();
        return Task::none();
    }
    use unicode_segmentation::UnicodeSegmentation;
    let length = state.draft.graphemes(true).count();
    if start > end || end > length { return close_link(state); }
    let (next, cursor) = crate::insert_composer_text(&state.draft, Some((start, end)), &state.link_text);
    let new_end = start + state.link_text.graphemes(true).count();
    state.composer_content = crate::composer_decorations::Content::with_text(&next);
    state.composer_content.move_to(message_body::editor_cursor_at(&next, cursor));
    let edit = crate::update_inner(state, AppMessage::DraftChanged(next));
    state.draft_formatting.entry(key.clone()).or_default().set_link(start, new_end, &url);
    state.draft_selection = Some((key, start, new_end));
    state.composer_content.move_to(message_body::editor_cursor_for_selection(&state.draft, (start, new_end)));
    state.link_editor_open = false;
    state.link_url.clear();
    state.link_text.clear();
    state.link_error.clear();
    state.link_target = None;
    state.mark_drafts_dirty();
    Task::batch([edit, state.restore_composer_selection((start, new_end))])
}

pub fn close_link(state: &mut TeamsApp) -> Task<AppMessage> {
    state.link_editor_open = false;
    state.link_url.clear();
    state.link_text.clear();
    state.link_error.clear();
    state.link_target = None;
    iced::widget::operation::focus("message-composer-rich")
}

pub fn toolbar(state: &TeamsApp) -> Element<'_, AppMessage> {
    let formatting = state.active_formatting();
    let selection = state.selected_draft_range();
    let key = state.draft_key();
    let active_marks = key.as_ref().and_then(|key| state.draft_active_marks.get(key));
    let mut tools = row![].spacing(4).align_y(Alignment::Center);
    for (icon, mark, label) in [
        (Icon::TextBold, message_body::InlineMark::Bold, tr("Bold (Ctrl+B)")),
        (Icon::TextItalic, message_body::InlineMark::Italic, tr("Italic (Ctrl+I)")),
        (Icon::TextUnderline, message_body::InlineMark::Underline, tr("Underline (Ctrl+U)")),
        (Icon::TextStrike, message_body::InlineMark::Strike, tr("Strikethrough")),
    ] {
        let active = selection.map_or_else(|| active_marks.is_some_and(|marks| marks.contains(&mark)),
            |(start, end)| formatting.has_mark(start, end, mark));
        tools = tools.push(control(icon, label, Some(AppMessage::ApplyDraftMark(mark)), active));
    }
    tools = tools.push(separator());
    for (icon, kind, label) in [
        (Icon::TextBullets, message_body::ListKind::Unordered, tr("Bulleted list")),
        (Icon::TextNumbered, message_body::ListKind::Ordered, tr("Numbered list")),
    ] {
        tools = tools.push(control(icon, label, selection.map(|_| AppMessage::ToggleDraftList(kind)), false));
    }
    tools = tools.push(control(
        Icon::QuoteReply,
        tr("Block quote"),
        selection.map(|_| AppMessage::ToggleDraftQuote),
        selection.is_some_and(|selection| formatting.has_quote(&state.draft, selection)),
    ));
    let table_menu = (state.composer_menu == Some(Menu::Table)).then(|| table_picker());
    tools = tools.push(crate::composer_popover::view(
        control(Icon::Table, tr("Insert table"), Some(AppMessage::ToggleComposerMenu(Menu::Table)), table_menu.is_some()), table_menu));
    tools = tools.push(separator());
    let color = key.as_ref().and_then(|key| state.draft_active_colors.get(key)).copied();
    let color = selection.map_or(Some(color), |(start, end)| formatting.color_for_range(start, end)).flatten();
    let color_menu = (state.composer_menu == Some(Menu::TextColor)).then(|| color_palette(color));
    tools = tools.push(crate::composer_popover::view(
        control(Icon::TextColor, tr("Font color"), Some(AppMessage::ToggleComposerMenu(Menu::TextColor)), color_menu.is_some()), color_menu));
    let highlight = key.as_ref().and_then(|key| state.draft_active_highlights.get(key)).copied();
    let highlight = selection.map_or(Some(highlight), |(start, end)| formatting.highlight_for_range(start, end)).flatten();
    let highlight_menu = (state.composer_menu == Some(Menu::Highlight)).then(|| highlight_palette(highlight));
    tools = tools.push(crate::composer_popover::view(
        control(Icon::TextHighlight, tr("Highlight text"), Some(AppMessage::ToggleComposerMenu(Menu::Highlight)), highlight_menu.is_some()), highlight_menu));
    let size = key.as_ref().and_then(|key| state.draft_active_sizes.get(key)).copied().unwrap_or(100);
    let size = selection.and_then(|(start, end)| formatting.size_for_range(start, end)).unwrap_or(size);
    let font_menu = (state.composer_menu == Some(Menu::FontSize)).then(|| font_sizes(size));
    tools = tools.push(crate::composer_popover::view(
        control(Icon::TextSize, tr("Font size"), Some(AppMessage::ToggleComposerMenu(Menu::FontSize)), font_menu.is_some()), font_menu));
    tools = tools.push(separator());
    let link_menu = state.link_editor_open.then(|| link_dialog(state));
    tools = tools.push(crate::composer_popover::view(
        control(Icon::Link, tr("Insert link"), Some(AppMessage::OpenDraftLink), state.link_editor_open), link_menu));
    tools = tools.push(control(Icon::CodeBlock, tr("Code block"), selection.map(|_| AppMessage::ToggleDraftCodeBlock), false));
    let more_menu = (state.composer_menu == Some(Menu::More)).then(|| more_options(state));
    tools = tools.push(crate::composer_popover::view(
        control(Icon::More, tr("More formatting options"), Some(AppMessage::ToggleComposerMenu(Menu::More)), more_menu.is_some()), more_menu));
    let clear = control(Icon::Delete, tr("Clear draft text"), Some(AppMessage::ClearDraftText), false);
    container(row![container(tools.wrap()).width(Fill), clear].spacing(8).align_y(Alignment::Start))
        .width(Fill).padding([4, 2]).into()
}

fn control(icon: Icon, label: &'static str, action: Option<AppMessage>, active: bool) -> Element<'static, AppMessage> {
    hint(button(icon_tone(icon, if active { Tone::Accent } else { Tone::Secondary }, 20.0))
        .width(34).height(34).padding(7).on_press_maybe(action)
        .style(ui_button(if active { Tone::Selected } else { Tone::Transparent }, Tone::Hover,
            if active { Tone::Accent } else { Tone::Secondary }, 4.0)), label).into()
}

fn separator() -> Element<'static, AppMessage> {
    container(container(space::vertical()).height(18).width(1).style(ui_surface(Tone::Border, false, 0.0)))
        .padding([0, 3]).into()
}

fn popup<'a>(content: impl Into<Element<'a, AppMessage>>, width: f32) -> Element<'a, AppMessage> {
    container(content).width(width).padding(12).style(crate::message_popup_style).into()
}

fn color_palette(selected: Option<u32>) -> Element<'static, AppMessage> {
    let mut content = column![button(text(tr("Automatic")).size(14)).width(Fill).padding([6, 8])
        .on_press(AppMessage::DraftTextColorSelected(message_body::ComposerTextColor::Default))
        .style(|theme, status| {
            let mut style = ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)(theme, status);
            style.border = Border::default().rounded(4).width(1).color(tone(theme, Tone::SettingsBorder));
            style
        })].spacing(8);
    for colors in message_body::ComposerTextColor::PALETTE.chunks(4) {
        let mut choices = row![].spacing(10);
        for choice in colors {
            let value = choice.value().unwrap();
            let is_selected = selected == Some(value);
            choices = choices.push(hint(button(container(space::horizontal()).width(18).height(18)
                .style(move |_| container::Style { background: Some(rgb(value).into()),
                    border: Border::default().rounded(3), ..Default::default() }))
                .padding(3).on_press(AppMessage::DraftTextColorSelected(*choice))
                .style(ui_button(if is_selected { Tone::Selected } else { Tone::Transparent }, Tone::Hover, Tone::Text, 4.0)), choice.to_string()));
        }
        content = content.push(choices);
    }
    popup(content, 150.0)
}

fn font_sizes(selected: u16) -> Element<'static, AppMessage> {
    let mut content = column![].spacing(2);
    for size in message_body::ComposerFontSize::ALL {
        let label = match size {
            message_body::ComposerFontSize::Small => tr("Small"),
            message_body::ComposerFontSize::Normal => tr("Medium"),
            message_body::ComposerFontSize::Large => tr("Large"),
        };
        content = content.push(button(row![text(label).size(16).width(Fill),
            text(if selected == size.percent() { "✓" } else { "" }).size(16)].spacing(8))
            .width(Fill).padding([6, 8]).on_press(AppMessage::DraftFontSizeSelected(size))
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)));
    }
    popup(content, 180.0)
}

fn more_options(state: &TeamsApp) -> Element<'_, AppMessage> {
    popup(column![
        crate::message_menu_action(Icon::CodeBlock, tr("Inline code"), Some(AppMessage::ApplyDraftMark(message_body::InlineMark::Code)), Tone::Text),
        crate::message_menu_action(Icon::QuoteReply, tr("Block quote"), state.selected_draft_range().map(|_| AppMessage::ToggleDraftQuote), Tone::Text),
        crate::message_menu_action(Icon::Table, tr("Insert table"), Some(AppMessage::ToggleComposerMenu(Menu::Table)), Tone::Text),
        crate::message_menu_action(Icon::Delete, tr("Clear formatting"), state.selected_draft_range().map(|_| AppMessage::ClearDraftFormatting), Tone::Text),
        crate::message_menu_action(Icon::Preview, tr("Preview message"), (!state.draft.is_empty()).then_some(AppMessage::ToggleDraftPreview), Tone::Text),
    ].spacing(4), 250.0)
}

fn table_picker() -> Element<'static, AppMessage> {
    let mut content = column![
        text(tr("Insert table")).size(16).style(ui_text(Tone::Text)),
        text(tr("Choose the initial size")).size(12).style(ui_text(Tone::Muted)),
    ]
    .spacing(8);
    for rows in 2..=4 {
        let mut choices = row![].spacing(6);
        for columns in 2..=5 {
            choices = choices.push(
                button(text(format!("{rows} × {columns}")).size(13))
                    .width(64)
                    .height(32)
                    .padding(4)
                    .on_press(AppMessage::InsertDraftTable { rows, columns })
                    .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
            );
        }
        content = content.push(choices);
    }
    popup(content, 300.0)
}

fn highlight_palette(selected: Option<u32>) -> Element<'static, AppMessage> {
    let mut content = column![button(text(tr("No highlight")).size(14)).width(Fill).padding([6, 8])
        .on_press(AppMessage::ClearDraftHighlight)
        .style(|theme, status| {
            let mut style = ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)(theme, status);
            style.border = Border::default().rounded(4).width(1).color(tone(theme, Tone::SettingsBorder));
            style
        })].spacing(8);
    for colors in message_body::ComposerHighlightColor::PALETTE.chunks(4) {
        let mut choices = row![].spacing(10);
        for choice in colors {
            let value = choice.value();
            let is_selected = selected == Some(value);
            choices = choices.push(hint(button(container(space::horizontal()).width(18).height(18)
                .style(move |_| container::Style { background: Some(rgb(value).into()),
                    border: Border::default().rounded(3), ..Default::default() }))
                .padding(3).on_press(AppMessage::DraftHighlightSelected(*choice))
                .style(ui_button(if is_selected { Tone::Selected } else { Tone::Transparent }, Tone::Hover, Tone::Text, 4.0)), choice.to_string()));
        }
        content = content.push(choices);
    }
    popup(content, 160.0)
}

fn link_dialog(state: &TeamsApp) -> Element<'_, AppMessage> {
    let valid = !state.link_text.trim().is_empty() && message_body::valid_link(&state.link_url).is_some();
    let mut content = column![
        text(tr("Insert link")).size(18).font(semibold()).style(ui_text(Tone::Text)),
        text(tr("Text to display")).size(16).style(ui_text(Tone::Text)),
        text_input(tr("Text to display"), &state.link_text).id("draft-link-text")
            .on_input(AppMessage::DraftLinkTextChanged).size(16).padding([8, 12]).style(ui_input(true)),
        text(tr("Address")).size(16).style(ui_text(Tone::Text)),
        text_input(tr("Link to a file or page"), &state.link_url).id("draft-link-url")
            .on_input(AppMessage::DraftLinkChanged).on_submit(AppMessage::ApplyDraftLink).size(16).padding([8, 12]).style(ui_input(true)),
    ].spacing(16);
    if !state.link_error.is_empty() { content = content.push(text(&state.link_error).size(13).style(ui_text(Tone::Danger))); }
    content = content.push(row![space::horizontal(),
        button(text(tr("Cancel")).size(16)).padding([8, 16]).on_press(AppMessage::CloseDraftLink)
            .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 4.0)),
        button(text(tr("Insert")).size(16)).padding([8, 16]).on_press_maybe(valid.then_some(AppMessage::ApplyDraftLink))
            .style(ui_button(Tone::Primary, Tone::SelectedHover, Tone::OnPrimary, 4.0)),
    ].spacing(12).align_y(Alignment::Center));
    popup(container(content).padding(8), 364.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(draft: &str) -> TeamsApp {
        let mut state = TeamsApp::from_client_id(String::new());
        state.formatting_open = true;
        let _ = update(&mut state, AppMessage::DraftChanged(draft.into()));
        state.composer_content = crate::composer_decorations::Content::with_text(draft);
        state
    }

    #[test]
    fn link_dialog_replaces_selected_graphemes_and_keeps_both_sides_of_the_draft() {
        let mut state = editor("Olá 👩‍💻, mundo");
        let key = state.draft_key().unwrap();
        state.draft_selection = Some((key.clone(), 4, 5));
        let _ = update(&mut state, AppMessage::OpenDraftLink);
        assert_eq!(state.link_text, "👩‍💻");
        assert!(state.link_editor_open);
        let _ = update(&mut state, AppMessage::DraftLinkTextChanged("equipe 🚀".into()));
        let _ = update(&mut state, AppMessage::DraftLinkChanged("https://example.invalid/a?x=1&y=2".into()));
        let _ = update(&mut state, AppMessage::ApplyDraftLink);
        assert_eq!(state.draft, "Olá equipe 🚀, mundo");
        assert_eq!(state.composer_content.text(), state.draft);
        let link = &state.draft_formatting[&key].links[0];
        assert_eq!((link.start, link.end), (4, 12));
        assert_eq!(link.url, "https://example.invalid/a?x=1&y=2");
        assert!(!state.link_editor_open);
        assert!(!state.draft_preview_open);
    }

    #[test]
    fn link_dialog_inserts_into_an_empty_draft_and_rejects_unsafe_addresses_before_editing() {
        let mut state = editor("");
        let _ = update(&mut state, AppMessage::OpenDraftLink);
        let _ = update(&mut state, AppMessage::DraftLinkTextChanged("Portal".into()));
        let _ = update(&mut state, AppMessage::DraftLinkChanged("javascript:alert(1)".into()));
        let _ = update(&mut state, AppMessage::ApplyDraftLink);
        assert!(state.draft.is_empty());
        assert!(!state.link_error.is_empty());
        let _ = update(&mut state, AppMessage::DraftLinkChanged("https://example.invalid/portal".into()));
        let _ = update(&mut state, AppMessage::ApplyDraftLink);
        assert_eq!(state.draft, "Portal");
        assert_eq!(state.active_formatting().links.len(), 1);
    }

    #[test]
    fn cancelling_or_switching_conversations_cannot_apply_a_link_to_another_draft() {
        let mut state = editor("Meu rascunho");
        let _ = update(&mut state, AppMessage::OpenDraftLink);
        let _ = update(&mut state, AppMessage::DraftLinkTextChanged("Não inserir".into()));
        let _ = update(&mut state, AppMessage::CloseDraftLink);
        assert_eq!(state.draft, "Meu rascunho");
        assert!(state.active_formatting().links.is_empty());
        let _ = update(&mut state, AppMessage::OpenDraftLink);
        let _ = update(&mut state, AppMessage::DraftLinkTextChanged("Não inserir".into()));
        let _ = update(&mut state, AppMessage::DraftLinkChanged("https://example.invalid".into()));
        state.selected_chat = Some("different-fixture-chat".into());
        let _ = update(&mut state, AppMessage::ApplyDraftLink);
        assert_eq!(state.draft, "Meu rascunho");
        assert!(!state.link_editor_open);
    }

    #[test]
    fn hidden_link_dialog_cannot_apply_a_delayed_submit_after_opening_settings() {
        let mut state = editor("Preservar texto");
        let _ = update(&mut state, AppMessage::OpenDraftLink);
        let _ = update(&mut state, AppMessage::DraftLinkTextChanged("Link antigo".into()));
        let _ = update(&mut state, AppMessage::DraftLinkChanged("https://example.invalid".into()));
        let _ = update(&mut state, AppMessage::OpenSettings);
        let _ = update(&mut state, AppMessage::ApplyDraftLink);
        assert_eq!(state.draft, "Preservar texto");
        assert!(state.active_formatting().links.is_empty());
    }

    #[test]
    fn menus_preserve_editor_selection_without_rebuilding_the_history() {
        let mut state = editor("Meu rascunho");
        let key = state.draft_key().unwrap();
        state.draft_selection = Some((key.clone(), 0, 3));
        let history = state.chat_render_revision;
        for menu in [Menu::TextColor, Menu::FontSize, Menu::More] {
            let _ = update(&mut state, AppMessage::ToggleComposerMenu(menu));
            assert_eq!(state.composer_menu, Some(menu));
            assert_eq!(state.selected_draft_range(), Some((0, 3)));
            let _ = update(&mut state, AppMessage::CloseComposerMenu);
        }
        assert_eq!(state.chat_render_revision, history);
        let _ = update(&mut state, AppMessage::ToggleComposerMenu(Menu::TextColor));
        let _ = update(&mut state, AppMessage::DraftTextColorSelected(message_body::ComposerTextColor::Blue));
        assert_eq!(state.composer_menu, None);
        assert_eq!(state.draft_formatting[&key].color_for_range(0, 3), Some(Some(0x2454a1)));
        assert!(!state.draft_preview_open);
    }
}
