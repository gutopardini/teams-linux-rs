//! Separate edit buffer: editing never overwrites the unsent chat draft.
use crate::i18n::tr;
use crate::*;
use iced::widget::column;

pub fn view(state: &TeamsApp) -> Element<'_, AppMessage> {
    let Some(id) = state.editing_message_id.clone() else {
        return space::vertical().into();
    };
    let saving = state
        .pending_message_mutation
        .as_ref()
        .is_some_and(|pending| pending.message_id == id);
    let can_save = !saving && !state.editing_message_text.trim().is_empty();
    let save = id.clone();
    let mut editor = text_editor(&state.editing_message_content)
        .id("message-edit")
        .placeholder(tr("Edit message"))
        .padding([8, 12])
        .size(u32::from(state.preferences.message_size))
        .min_height(f32::from(state.preferences.message_size) * 1.5 + 24.0)
        .max_height(240.0)
        .height(iced::Length::Shrink)
        .highlight_with::<message_body::ComposerHighlighter>(
            message_body::ComposerHighlightSettings::new(
                &state.editing_message_text,
                state.editing_message_formatting.clone(),
            ),
            message_body::composer_highlight_format,
        )
        .style(|theme, status| text_editor::Style {
            background: tone(theme, Tone::Surface).into(),
            border: Border::default(),
            ..text_editor::default(theme, status)
        })
        .key_binding(move |key| edit_key(key, &save, can_save, !saving));
    if !saving {
        editor = editor.on_action(AppMessage::MessageEditAction);
    }
    let format = hint(
        button(icon_tone(Icon::Format, Tone::Secondary, 21.0))
            .padding(7)
            .on_press_maybe((!saving).then_some(AppMessage::ToggleEditFormatting))
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
        tr("Formatting"),
    );
    let cancel = hint(
        button(text("×").size(25))
            .width(34)
            .height(34)
            .padding(0)
            .on_press_maybe((!saving).then_some(AppMessage::CancelMessageEdit))
            .style(ui_button(
                Tone::Transparent,
                Tone::Hover,
                Tone::Secondary,
                4.0,
            )),
        tr("Cancel editing (Esc)"),
    );
    let confirm = hint(
        button(icon_tone(Icon::Check, Tone::Secondary, 22.0))
            .padding(7)
            .on_press_maybe(can_save.then_some(AppMessage::SaveMessageEdit(id)))
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
        tr("Confirm edit (Enter)"),
    );
    let mut content = column![].spacing(4);
    if saving {
        content = content.push(
            text(tr("Saving edit…"))
                .size(11)
                .style(ui_text(Tone::Muted)),
        );
    }
    if state.editing_formatting_open {
        let range = message_body::editor_cursor_range(
            &state.editing_message_text,
            state.editing_message_content.cursor(),
        );
        let mut tools = row![].spacing(4);
        for (label, mark) in [
            ("B", message_body::InlineMark::Bold),
            ("I", message_body::InlineMark::Italic),
            ("U", message_body::InlineMark::Underline),
        ] {
            tools = tools.push(
                button(text(label).size(14))
                    .padding([5, 10])
                    .on_press_maybe(
                        (!saving && range.0 < range.1).then_some(AppMessage::ApplyEditMark(mark)),
                    )
                    .style(ui_button(
                        Tone::Transparent,
                        Tone::Hover,
                        Tone::Secondary,
                        4.0,
                    )),
            );
        }
        content = content.push(tools);
    }
    let panel = container(
        content.push(editor).push(
            row![space::horizontal(), format, cancel, confirm]
                .spacing(4)
                .align_y(Alignment::Center),
        ),
    )
    .padding(8)
    .width(Fill)
    .style(|theme| {
        let mut style = ui_surface(Tone::Surface, true, 4.0)(theme);
        style.border.color = tone(theme, Tone::Border);
        style
    });
    container(
        column![
            stack![
                panel,
                container(container(space::horizontal()).height(2).style(ui_surface(
                    Tone::Accent,
                    false,
                    1.0
                )))
                .align_bottom(Fill)
            ],
            container(
                text(tr("Shift+Enter starts a new line."))
                    .size(12)
                    .style(ui_text(Tone::Muted))
            )
            .align_right(Fill)
        ]
        .spacing(5),
    )
    .width(Fill)
    .max_width(1120)
    .into()
}
fn edit_key(
    key: text_editor::KeyPress,
    id: &str,
    save: bool,
    cancel: bool,
) -> Option<text_editor::Binding<AppMessage>> {
    use iced::keyboard::{Key, key::Named};
    if matches!(key.status, text_editor::Status::Focused { .. })
        && key.modifiers.control()
        && !key.modifiers.alt()
        && cancel
    {
        let mark = match key.key.to_latin(key.physical_key) {
            Some('b') => Some(message_body::InlineMark::Bold),
            Some('i') => Some(message_body::InlineMark::Italic),
            Some('u') => Some(message_body::InlineMark::Underline),
            _ => None,
        };
        if let Some(mark) = mark {
            return Some(text_editor::Binding::Custom(AppMessage::ApplyEditMark(
                mark,
            )));
        }
    }
    if matches!(key.status, text_editor::Status::Focused { .. })
        && !key.modifiers.control()
        && !key.modifiers.alt()
    {
        if key.key == Key::Named(Named::Escape) && cancel {
            return Some(text_editor::Binding::Custom(AppMessage::CancelMessageEdit));
        }
        if key.key == Key::Named(Named::Enter) && !key.modifiers.shift() {
            return save
                .then(|| text_editor::Binding::Custom(AppMessage::SaveMessageEdit(id.to_owned())));
        }
    }
    text_editor::Binding::from_key_press(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_uses_a_separate_buffer_and_typing_retains_history_and_sidebar() {
        let mut state = TeamsApp::from_client_id(String::new());
        state.demo_mode = true;
        state.messages = vec![GraphMessage {
            id: "remote-message".into(),
            body: "Teste".into(),
            is_mine: true,
            ..Default::default()
        }];
        state.draft = "Unsent draft must survive".into();
        drop(update(
            &mut state,
            AppMessage::BeginMessageEdit("remote-message".into()),
        ));
        let revisions = (state.chat_render_revision, state.chat_list_revision);
        drop(update(
            &mut state,
            AppMessage::MessageEditAction(text_editor::Action::Edit(text_editor::Edit::Insert(
                'X',
            ))),
        ));
        assert_eq!(
            (state.chat_render_revision, state.chat_list_revision),
            revisions
        );
        assert_eq!(state.draft, "Unsent draft must survive");
        assert_eq!(state.messages[0].body, "Teste");
        drop(update(&mut state, AppMessage::CancelMessageEdit));
        assert_eq!(state.messages[0].body, "Teste");
        assert_eq!(state.draft, "Unsent draft must survive");
        drop(update(
            &mut state,
            AppMessage::BeginMessageEdit("remote-message".into()),
        ));
        drop(update(
            &mut state,
            AppMessage::MessageEditTextChanged("Changed\nsecond line".into()),
        ));
        drop(update(
            &mut state,
            AppMessage::SaveMessageEdit("remote-message".into()),
        ));
        assert_eq!(state.messages[0].body, "Changed\nsecond line");
        assert!(state.editing_message_id.is_none());
        assert_eq!(state.draft, "Unsent draft must survive");
        state.messages[0].is_mine = false;
        drop(update(
            &mut state,
            AppMessage::BeginMessageEdit("remote-message".into()),
        ));
        assert!(state.editing_message_id.is_none());
    }
    #[test]
    fn enter_confirms_shift_enter_inserts_newline_and_escape_cancels() {
        use iced::keyboard::{
            Key, Modifiers,
            key::{Code, Named, Physical},
        };
        let press = |named, modifiers| text_editor::KeyPress {
            key: Key::Named(named),
            modified_key: Key::Named(named),
            physical_key: Physical::Code(Code::Enter),
            modifiers,
            text: None,
            status: text_editor::Status::Focused { is_hovered: false },
        };
        assert!(matches!(
            edit_key(press(Named::Enter, Modifiers::empty()), "m1", true, true),
            Some(text_editor::Binding::Custom(AppMessage::SaveMessageEdit(_)))
        ));
        assert!(matches!(
            edit_key(press(Named::Enter, Modifiers::SHIFT), "m1", true, true),
            Some(text_editor::Binding::Enter)
        ));
        assert!(matches!(
            edit_key(press(Named::Escape, Modifiers::empty()), "m1", true, true),
            Some(text_editor::Binding::Custom(AppMessage::CancelMessageEdit))
        ));
        assert!(edit_key(press(Named::Enter, Modifiers::empty()), "m1", false, false).is_none());
    }
}
