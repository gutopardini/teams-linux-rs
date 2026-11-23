//! Composer-only paste interception. Text is replayed through Iced's own
//! input so normal cursor position, selection and keyboard behavior survive.
use crate::AppMessage;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use iced::{Element, Event, Length, Rectangle, Size, Theme, Vector};

pub fn composer<'a>(
    inner: iced::widget::TextInput<'a, AppMessage>,
    context: String,
    text: Option<(u64, String, (usize, usize))>,
    value: &str,
) -> Element<'a, AppMessage> {
    Element::new(Observed {
        inner: inner.into(),
        context,
        text,
        image_url: None,
        value: value.into(),
        shortcuts: None,
        rich_editor: false,
        shortcuts_suspended: false,
    })
}
pub fn rich_composer<'a>(
    inner: impl Into<Element<'a, AppMessage>>,
    context: String,
) -> Element<'a, AppMessage> {
    Element::new(Observed {
        inner: inner.into(),
        context,
        text: None,
        image_url: None,
        value: String::new(),
        shortcuts: None,
        rich_editor: true,
        shortcuts_suspended: false,
    })
}
pub fn image<'a>(
    inner: impl Into<Element<'a, AppMessage>>,
    url: String,
) -> Element<'a, AppMessage> {
    Element::new(Observed {
        inner: inner.into(),
        context: url.clone(),
        text: None,
        image_url: Some(url),
        value: String::new(),
        shortcuts: None,
        rich_editor: false,
        shortcuts_suspended: false,
    })
}
struct Observed<'a, R: iced::advanced::text::Renderer = iced::Renderer> {
    inner: Element<'a, AppMessage, Theme, R>,
    context: String,
    text: Option<(u64, String, (usize, usize))>,
    image_url: Option<String>,
    value: String,
    shortcuts: Option<(bool, bool, bool, bool, bool, bool)>,
    rich_editor: bool,
    shortcuts_suspended: bool,
}
pub fn shortcuts<'a>(
    inner: impl Into<Element<'a, AppMessage>>,
    shortcuts_suspended: bool,
    settings_open: bool,
    message_selected: bool,
    message_menu_open: bool,
    image_viewer_open: bool,
    new_conversation_open: bool,
    message_search_open: bool,
) -> Element<'a, AppMessage> {
    Element::new(Observed {
        inner: inner.into(),
        context: "application-shortcuts".into(),
        text: None,
        image_url: None,
        value: String::new(),
        shortcuts: Some((
            settings_open,
            message_selected,
            message_menu_open,
            image_viewer_open,
            new_conversation_open,
            message_search_open,
        )),
        rich_editor: false,
        shortcuts_suspended,
    })
}

fn viewer_wheel_lines(event: &Event) -> Option<f32> {
    let Event::Mouse(mouse::Event::WheelScrolled { delta }) = event else {
        return None;
    };
    let lines = match delta {
        mouse::ScrollDelta::Lines { y, .. } => *y,
        mouse::ScrollDelta::Pixels { y, .. } => *y / 53.0,
    };
    (lines.is_finite() && lines != 0.0).then_some(lines)
}

fn shortcut(
    event: &Event,
    settings_open: bool,
    message_selected: bool,
    message_menu_open: bool,
    text_input_focused: bool,
) -> Option<AppMessage> {
    shortcut_with_viewer(
        event,
        settings_open,
        message_selected,
        message_menu_open,
        false,
        text_input_focused,
    )
}

fn shortcut_with_viewer(
    event: &Event,
    settings_open: bool,
    message_selected: bool,
    message_menu_open: bool,
    image_viewer_open: bool,
    text_input_focused: bool,
) -> Option<AppMessage> {
    shortcut_with_overlays(
        event,
        settings_open,
        message_selected,
        message_menu_open,
        image_viewer_open,
        false,
        text_input_focused,
    )
}

fn shortcut_with_overlays(
    event: &Event,
    settings_open: bool,
    message_selected: bool,
    message_menu_open: bool,
    image_viewer_open: bool,
    new_conversation_open: bool,
    text_input_focused: bool,
) -> Option<AppMessage> {
    shortcut_with_search(
        event,
        settings_open,
        message_selected,
        message_menu_open,
        image_viewer_open,
        new_conversation_open,
        text_input_focused,
        false,
    )
}

fn shortcut_with_search(
    event: &Event,
    settings_open: bool,
    message_selected: bool,
    message_menu_open: bool,
    image_viewer_open: bool,
    new_conversation_open: bool,
    text_input_focused: bool,
    message_search_open: bool,
) -> Option<AppMessage> {
    use iced::keyboard::{
        Event as KeyboardEvent, Key,
        key::{Code, Named, Physical},
    };
    let Event::Keyboard(KeyboardEvent::KeyPressed {
        key,
        physical_key,
        modifiers,
        repeat,
        ..
    }) = event
    else {
        return None;
    };
    if image_viewer_open {
        if matches!(key, Key::Named(Named::Escape)) {
            return Some(AppMessage::CloseImageViewer);
        }
        if modifiers.control() && modifiers.shift() && !modifiers.alt() {
            return match physical_key {
                Physical::Code(Code::Equal | Code::NumpadAdd) => Some(AppMessage::ZoomImage(1)),
                _ => None,
            };
        }
        if modifiers.control() && !modifiers.alt() && !modifiers.shift() {
            return match physical_key {
                Physical::Code(Code::KeyC) => Some(AppMessage::CopyOriginalImage),
                Physical::Code(Code::KeyS) => Some(AppMessage::SaveOriginalImage),
                Physical::Code(Code::Equal | Code::NumpadAdd) => Some(AppMessage::ZoomImage(1)),
                Physical::Code(Code::Minus | Code::NumpadSubtract) => {
                    Some(AppMessage::ZoomImage(-1))
                }
                Physical::Code(Code::Digit0 | Code::Numpad0) => Some(AppMessage::ResetImageZoom),
                _ => None,
            };
        }
        if !modifiers.control() && !modifiers.alt() && !modifiers.shift() {
            return match physical_key {
                Physical::Code(Code::ArrowLeft) => Some(AppMessage::NavigateImage(-1)),
                Physical::Code(Code::ArrowRight) => Some(AppMessage::NavigateImage(1)),
                _ => None,
            };
        }
        return None;
    }
    if !*repeat && !new_conversation_open && modifiers.control() && !modifiers.alt() {
        match physical_key {
            Physical::Code(Code::Comma) if modifiers.shift()=>return Some(AppMessage::OpenSettings),
            Physical::Code(Code::F1) if !modifiers.shift()=>return Some(AppMessage::AppMenuAction(crate::app_menu::Action::Help)),
            Physical::Code(Code::Period) if !modifiers.shift()=>return Some(AppMessage::AppMenuAction(crate::app_menu::Action::Shortcuts)),
            _=>{}
        }
    }
    if !*repeat && settings_open && !message_menu_open && !new_conversation_open
        && modifiers.control() && modifiers.alt() && !modifiers.shift()
        && matches!(physical_key, Physical::Code(Code::KeyF))
    {
        return Some(AppMessage::FocusSettingsSearch);
    }
    if !*repeat
        && !settings_open
        && !message_menu_open
        && !new_conversation_open
        && modifiers.control()
        && modifiers.alt()
        && !modifiers.shift()
        && matches!(physical_key, Physical::Code(Code::KeyG | Code::KeyE))
    {
        return Some(AppMessage::FocusSearch);
    }
    if !settings_open
        && !message_menu_open
        && !new_conversation_open
        && !text_input_focused
        && modifiers.control()
        && !modifiers.alt()
        && !modifiers.shift()
        && matches!(physical_key, Physical::Code(Code::KeyF))
    {
        return Some(AppMessage::FocusMessageSearch);
    }
    if matches!(key, Key::Named(Named::Escape)) {
        if new_conversation_open {
            return Some(AppMessage::CloseNewConversation);
        }
        if message_menu_open {
            return Some(AppMessage::CloseMessageMenu);
        }
        if settings_open {
            return Some(AppMessage::CloseSettings);
        }
        if message_search_open {
            return Some(AppMessage::ToggleMessageSearch);
        }
    }
    if new_conversation_open {
        return None;
    }
    if message_menu_open
        && !text_input_focused
        && matches!(key, Key::Named(Named::Enter))
        && !modifiers.control()
        && !modifiers.alt()
        && !modifiers.shift()
    {
        return Some(AppMessage::CopySelectedMessage);
    }
    if (message_menu_open || message_selected)
        && !text_input_focused
        && modifiers.control()
        && !modifiers.alt()
        && !modifiers.shift()
        && matches!(physical_key, Physical::Code(Code::KeyC))
    {
        return Some(AppMessage::CopySelectedMessage);
    }
    if text_input_focused {
        return None;
    }
    if message_selected
        && modifiers.shift()
        && !modifiers.control()
        && !modifiers.alt()
        && matches!(physical_key, Physical::Code(Code::F10))
    {
        return Some(AppMessage::OpenSelectedMessageMenu);
    }
    if modifiers.control() && modifiers.alt() && !modifiers.shift() {
        match physical_key {
            Physical::Code(Code::ArrowUp) => return Some(AppMessage::NavigateMessageSelection(-1)),
            Physical::Code(Code::ArrowDown) => {
                return Some(AppMessage::NavigateMessageSelection(1));
            }
            _ => {}
        }
    }
    if !modifiers.control() || modifiers.alt() {
        return None;
    }
    match physical_key {
        Physical::Code(Code::Equal | Code::NumpadAdd) => Some(AppMessage::ZoomStep(10)),
        Physical::Code(Code::Minus | Code::NumpadSubtract) => Some(AppMessage::ZoomStep(-10)),
        Physical::Code(Code::Digit0 | Code::Numpad0) => Some(AppMessage::ResetZoom),
        Physical::Code(Code::Comma) => Some(AppMessage::OpenSettings),
        _ => None,
    }
}

fn selected_graphemes(value: &str, start: usize, end: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let (start, end) = if start <= end { (start, end) } else { (end, start) };
    value
        .graphemes(true)
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn rich_composer_shortcut(event: &Event) -> Option<AppMessage> {
    use iced::keyboard::{Event as KeyboardEvent, Key, key::Named};

    let Event::Keyboard(KeyboardEvent::KeyPressed {
        key,
        physical_key,
        modifiers,
        repeat: false,
        ..
    }) = event
    else {
        return None;
    };

    if !modifiers.control() || modifiers.alt() {
        return None;
    }

    if matches!(key, Key::Named(Named::Enter)) {
        return (!modifiers.shift()).then_some(AppMessage::SendMessage);
    }

    if modifiers.shift() {
        return None;
    }

    match key.to_latin(*physical_key).map(|character| character.to_ascii_lowercase()) {
        Some('b') => Some(AppMessage::ApplyDraftMark(
            crate::message_body::InlineMark::Bold,
        )),
        Some('i') => Some(AppMessage::ApplyDraftMark(
            crate::message_body::InlineMark::Italic,
        )),
        Some('u') => Some(AppMessage::ApplyDraftMark(
            crate::message_body::InlineMark::Underline,
        )),
        _ => None,
    }
}

#[derive(Default)]
struct State {
    context: String,
    event: Option<Event>,
    injected: u64,
    visible: bool,
    modifiers: iced::keyboard::Modifiers,
    composer_selection: Option<(usize, usize)>,
}

#[derive(Default)]
struct Focus(bool);
impl Operation for Focus {
    fn focusable(
        &mut self,
        _: Option<&iced::advanced::widget::Id>,
        _: Rectangle,
        state: &mut dyn iced::advanced::widget::operation::Focusable,
    ) {
        self.0 |= state.is_focused();
    }
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
}
struct TextClipboard<'a> {
    value: &'a str,
    real: &'a mut dyn Clipboard,
}
impl Clipboard for TextClipboard<'_> {
    fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
        Some(self.value.into())
    }
    fn write(&mut self, kind: iced::advanced::clipboard::Kind, value: String) {
        self.real.write(kind, value);
    }
}
impl<R: iced::advanced::text::Renderer> Widget<AppMessage, Theme, R> for Observed<'_, R> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State {
            context: self.context.clone(),
            ..Default::default()
        })
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.inner.as_widget())]
    }
    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if state.context != self.context {
            *state = State {
                context: self.context.clone(),
                ..Default::default()
            };
        }
        tree.diff_children(std::slice::from_ref(&self.inner));
    }
    fn size(&self) -> Size<Length> {
        self.inner.as_widget().size()
    }
    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        self.inner
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &R,
        operation: &mut dyn Operation,
    ) {
        self.inner
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &R,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, AppMessage>,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        if let Event::Keyboard(
            iced::keyboard::Event::ModifiersChanged(modifiers)
            | iced::keyboard::Event::KeyPressed { modifiers, .. }
            | iced::keyboard::Event::KeyReleased { modifiers, .. },
        ) = event
        {
            state.modifiers = *modifiers;
        }
        if let Some((
            settings_open,
            message_selected,
            message_menu_open,
            image_viewer_open,
            new_conversation_open,
            message_search_open,
        )) = self.shortcuts.filter(|_| !self.shortcuts_suspended)
        {
            if image_viewer_open && let Some(lines) = viewer_wheel_lines(event) {
                shell.publish(AppMessage::ZoomImageWheel(lines));
                shell.capture_event();
                return;
            }
            // Ordinary typing, redraws and pointer moves are not shortcuts.
            // Only traverse the full tree for focus when a key could match.
            if shortcut_with_search(
                event,
                settings_open,
                message_selected,
                message_menu_open,
                image_viewer_open,
                new_conversation_open,
                false,
                message_search_open,
            ).is_some() {
                let mut focus = Focus::default();
                self.inner.as_widget_mut().operate(&mut tree.children[0], layout, renderer, &mut focus);
                if let Some(message) = shortcut_with_search(event, settings_open, message_selected,
                    message_menu_open, image_viewer_open, new_conversation_open, focus.0, message_search_open)
                {
                    shell.publish(message);
                    shell.capture_event();
                    return;
                }
            }
        }
        if let Some(url) = &self.image_url {
            let visible = layout.bounds().intersects(viewport);
            if visible && !state.visible {
                shell.publish(AppMessage::LoadInlineImage(url.clone()));
            } else if !visible && state.visible {
                shell.publish(AppMessage::CancelQueuedInlineImage(url.clone()));
            }
            state.visible = visible;
        }
        if let Some((version, text, selection)) = &self.text {
            if *version > state.injected {
                state.injected = *version;
                if let Some(key) = state.event.take() {
                    let mut original_focus = Focus::default();
                    self.inner.as_widget_mut().operate(
                        &mut tree.children[0],
                        layout,
                        renderer,
                        &mut original_focus,
                    );
                    let input = tree.children[0]
                        .state
                        .downcast_mut::<iced::widget::text_input::State<R::Paragraph>>();
                    input.focus();
                    let (start, end) = selection;
                    input.select_range(*start, *end);
                    let mut clipboard = TextClipboard {
                        value: text,
                        real: clipboard,
                    };
                    if let Event::Keyboard(iced::keyboard::Event::KeyPressed {
                        modifiers, ..
                    }) = &key
                    {
                        // Clipboard transfer is asynchronous. The physical Ctrl
                        // key may already have been released when text arrives.
                        self.inner.as_widget_mut().update(
                            &mut tree.children[0],
                            &Event::Keyboard(iced::keyboard::Event::ModifiersChanged(*modifiers)),
                            layout,
                            cursor,
                            renderer,
                            &mut clipboard,
                            shell,
                            viewport,
                        );
                    }
                    self.inner.as_widget_mut().update(
                        &mut tree.children[0],
                        &key,
                        layout,
                        cursor,
                        renderer,
                        &mut clipboard,
                        shell,
                        viewport,
                    );
                    if let Event::Keyboard(iced::keyboard::Event::KeyPressed {
                        key,
                        modified_key,
                        physical_key,
                        location,
                        modifiers,
                        ..
                    }) = key
                    {
                        self.inner.as_widget_mut().update(
                            &mut tree.children[0],
                            &Event::Keyboard(iced::keyboard::Event::KeyReleased {
                                key,
                                modified_key,
                                physical_key,
                                location,
                                modifiers,
                            }),
                            layout,
                            cursor,
                            renderer,
                            &mut clipboard,
                            shell,
                            viewport,
                        );
                    }
                    self.inner.as_widget_mut().update(
                        &mut tree.children[0],
                        &Event::Keyboard(iced::keyboard::Event::ModifiersChanged(state.modifiers)),
                        layout,
                        cursor,
                        renderer,
                        &mut clipboard,
                        shell,
                        viewport,
                    );
                    if !original_focus.0 {
                        tree.children[0]
                            .state
                            .downcast_mut::<iced::widget::text_input::State<R::Paragraph>>()
                            .unfocus();
                    }
                    shell.publish(AppMessage::TextPasteFinished(*version, false));
                    shell.request_redraw();
                } else {
                    // Zoom or opening settings may have replaced this input's
                    // tree while the clipboard read was in flight.
                    shell.publish(AppMessage::TextPasteFinished(*version, true));
                }
            }
        }
        if self.rich_editor {
            if matches!(event, Event::Keyboard(iced::keyboard::Event::KeyPressed { repeat: false, .. })) {
                let mut focus = Focus::default();
                self.inner.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut focus,
                );
                if focus.0 {
                    if let Some(message) = rich_composer_shortcut(event) {
                        shell.publish(message);
                        shell.capture_event();
                        return;
                    }
                    if let Event::Keyboard(iced::keyboard::Event::KeyPressed {
                        key,
                        physical_key,
                        modifiers,
                        ..
                    }) = event
                        && modifiers.control()
                        && !modifiers.alt()
                        && !modifiers.shift()
                        && key.to_latin(*physical_key) == Some('c')
                    {
                        shell.publish(AppMessage::CopyComposerSelection);
                        shell.capture_event();
                        return;
                    }
                    if let Event::Keyboard(iced::keyboard::Event::KeyPressed {
                        key,
                        physical_key,
                        modifiers,
                        ..
                    }) = event
                        && modifiers.control()
                        && !modifiers.alt()
                        && !modifiers.shift()
                        && key.to_latin(*physical_key) == Some('v')
                    {
                        shell.publish(AppMessage::PasteClipboard(None));
                        shell.capture_event();
                        return;
                    }
                }
            }
        }
        if self.image_url.is_none() && self.shortcuts.is_none() && !self.rich_editor {
            if let Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                repeat: false,
                ..
            }) = event
                && modifiers.control()
                && !modifiers.alt()
                && !modifiers.shift()
                && key.to_latin(*physical_key) == Some('c')
            {
                let mut focus = Focus::default();
                self.inner.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut focus,
                );
                if focus.0 {
                    let input = tree.children[0]
                        .state
                        .downcast_ref::<iced::widget::text_input::State<R::Paragraph>>();
                    let value = iced::widget::text_input::Value::new(&self.value);
                    if let iced::widget::text_input::cursor::State::Selection { start, end } =
                        input.cursor().state(&value)
                    {
                        let selected = selected_graphemes(&self.value, start, end);
                        if !selected.is_empty() {
                            shell.publish(AppMessage::CopyPlainText(selected));
                            shell.capture_event();
                            return;
                        }
                    }
                }
            }
            if let Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key,
                modifiers,
                ..
            }) = event
                && modifiers.shift()
                && !modifiers.control()
                && !modifiers.alt()
                && matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter))
            {
                let mut focus = Focus::default();
                self.inner.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut focus,
                );
                if focus.0 {
                    let input = tree.children[0]
                        .state
                        .downcast_ref::<iced::widget::text_input::State<R::Paragraph>>();
                    let value = iced::widget::text_input::Value::new(&self.value);
                    let range = match input.cursor().state(&value) {
                        iced::widget::text_input::cursor::State::Index(index) => {
                            (index, index)
                        }
                        iced::widget::text_input::cursor::State::Selection { start, end } => (
                            start.min(end),
                            start.max(end),
                        ),
                    };
                    shell.publish(AppMessage::ComposerLineBreak(Some(range)));
                    shell.capture_event();
                    return;
                }
            }
            if let Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                repeat: false,
                ..
            }) = event
            {
                if modifiers.control()
                    && !modifiers.alt()
                    && !modifiers.shift()
                    && key.to_latin(*physical_key) == Some('v')
                {
                    let mut focus = Focus::default();
                    self.inner.as_widget_mut().operate(
                        &mut tree.children[0],
                        layout,
                        renderer,
                        &mut focus,
                    );
                    if focus.0 {
                        state.event = Some(event.clone());
                        let input = tree.children[0]
                            .state
                            .downcast_ref::<iced::widget::text_input::State<R::Paragraph>>();
                        let value = iced::widget::text_input::Value::new(&self.value);
                        let range = match input.cursor().state(&value) {
                            iced::widget::text_input::cursor::State::Index(index) => {
                                (index, index)
                            }
                            iced::widget::text_input::cursor::State::Selection { start, end } => (
                                start.min(end),
                                start.max(end),
                            ),
                        };
                        shell.publish(AppMessage::PasteClipboard(Some(range)));
                        shell.capture_event();
                        return;
                    }
                }
            }
        }
        self.inner.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if self.image_url.is_none() && self.shortcuts.is_none() && !self.rich_editor {
            let input = tree.children[0]
                .state
                .downcast_ref::<iced::widget::text_input::State<R::Paragraph>>();
            let selection = input
                .cursor()
                .selection(&iced::widget::text_input::Value::new(&self.value));
            if selection != state.composer_selection {
                state.composer_selection = selection;
                shell.publish(AppMessage::DraftSelectionChanged(
                    self.context.clone(),
                    selection,
                ));
            }
        }
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut R,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.inner.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &R,
    ) -> mouse::Interaction {
        self.inner.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &R,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, AppMessage, Theme, R>> {
        self.inner.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_enter_uses_the_cursor_after_accented_text_and_emoji() {
        use iced::keyboard::{Event as KeyEvent, Key, Location, Modifiers, key::{Code, Named, Physical}};
        use unicode_segmentation::UnicodeSegmentation;
        let text = "informações 👨‍👩‍👧‍👦 com Kiro";
        let length = text.graphemes(true).count();
        let input: Element<'_, AppMessage, Theme, ()> = iced::widget::text_input("", text)
            .on_input(AppMessage::DraftChanged).into();
        let mut widget = Observed {
            inner: input, context: "chat:test".into(), text: None, image_url: None,
            value: text.into(), shortcuts: None, rich_editor: false, shortcuts_suspended: false,
        };
        let mut tree = Tree::new(&widget as &dyn Widget<AppMessage, Theme, ()>);
        let node = widget.layout(&mut tree, &(), &layout::Limits::new(Size::ZERO, Size::new(600.0, 60.0)));
        let input = tree.children[0].state.downcast_mut::<iced::widget::text_input::State<()>>();
        input.focus();
        input.move_cursor_to_end();
        let key = Event::Keyboard(KeyEvent::KeyPressed {
            key: Key::Named(Named::Enter), modified_key: Key::Named(Named::Enter),
            physical_key: Physical::Code(Code::Enter), location: Location::Standard,
            modifiers: Modifiers::SHIFT, text: None, repeat: false,
        });
        let mut messages = Vec::new();
        widget.update(&mut tree, &key, Layout::new(&node), mouse::Cursor::Unavailable, &(),
            &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut messages),
            &Rectangle::with_size(Size::new(600.0, 60.0)));
        assert!(matches!(messages.as_slice(), [AppMessage::ComposerLineBreak(Some((start, end)))] if *start == length && *end == length));
        messages.clear();
        tree.children[0].state.downcast_mut::<iced::widget::text_input::State<()>>().select_range(2, length - 1);
        widget.update(&mut tree, &key, Layout::new(&node), mouse::Cursor::Unavailable, &(),
            &mut iced::advanced::clipboard::Null, &mut Shell::new(&mut messages),
            &Rectangle::with_size(Size::new(600.0, 60.0)));
        assert!(matches!(messages.as_slice(), [AppMessage::ComposerLineBreak(Some((2, end)))] if *end == length - 1));
    }

    #[test]
    fn image_viewer_shortcuts_cover_navigation_copy_save_zoom_and_escape() {
        use iced::keyboard::{
            Event as KeyEvent, Key, Location, Modifiers,
            key::{Code, Named, Physical},
        };
        let make_event = |code: Code, key: Key, modifiers: Modifiers| {
            Event::Keyboard(KeyEvent::KeyPressed {
                key,
                modified_key: Key::Character(String::new().into()),
                physical_key: Physical::Code(code),
                location: Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            })
        };
        let shortcuts = [
            (
                make_event(
                    Code::ArrowLeft,
                    Key::Named(Named::ArrowLeft),
                    Modifiers::empty(),
                ),
                AppMessage::NavigateImage(-1),
            ),
            (
                make_event(
                    Code::ArrowRight,
                    Key::Named(Named::ArrowRight),
                    Modifiers::empty(),
                ),
                AppMessage::NavigateImage(1),
            ),
            (
                make_event(Code::KeyC, Key::Character("c".into()), Modifiers::CTRL),
                AppMessage::CopyOriginalImage,
            ),
            (
                make_event(Code::KeyS, Key::Character("s".into()), Modifiers::CTRL),
                AppMessage::SaveOriginalImage,
            ),
            (
                make_event(Code::Equal, Key::Character("=".into()), Modifiers::CTRL),
                AppMessage::ZoomImage(1),
            ),
            (
                make_event(
                    Code::Equal,
                    Key::Character("+".into()),
                    Modifiers::CTRL | Modifiers::SHIFT,
                ),
                AppMessage::ZoomImage(1),
            ),
            (
                make_event(Code::Minus, Key::Character("-".into()), Modifiers::CTRL),
                AppMessage::ZoomImage(-1),
            ),
        ];
        for (event, expected) in shortcuts {
            let result = shortcut_with_viewer(&event, false, false, false, true, false);
            let matches = match (result, expected) {
                (Some(AppMessage::NavigateImage(left)), AppMessage::NavigateImage(right)) => {
                    left == right
                }
                (Some(AppMessage::CopyOriginalImage), AppMessage::CopyOriginalImage)
                | (Some(AppMessage::SaveOriginalImage), AppMessage::SaveOriginalImage) => true,
                (Some(AppMessage::ZoomImage(left)), AppMessage::ZoomImage(right)) => left == right,
                _ => false,
            };
            assert!(matches);
        }
        let escape = make_event(Code::Escape, Key::Named(Named::Escape), Modifiers::empty());
        assert!(matches!(
            shortcut_with_viewer(&escape, false, false, false, true, false),
            Some(AppMessage::CloseImageViewer)
        ));
        assert_eq!(
            viewer_wheel_lines(&Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            })),
            Some(1.0)
        );
        assert_eq!(
            viewer_wheel_lines(&Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -106.0 },
            })),
            Some(-2.0)
        );
        assert_eq!(
            viewer_wheel_lines(&Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 1.0, y: 0.0 },
            })),
            None
        );
    }

    #[test]
    fn app_menu_shortcuts_work_in_inputs_and_do_not_replace_clipboard_keys() {
        use iced::keyboard::{Event as KeyEvent,Key,Location,Modifiers,key::{Code,Physical}};
        let event=|code,modifiers|Event::Keyboard(KeyEvent::KeyPressed {key:Key::Character(".".into()),modified_key:Key::Character(".".into()),physical_key:Physical::Code(code),location:Location::Standard,modifiers,text:None,repeat:false});
        fn shortcut(event: &Event)->Option<AppMessage>{shortcut_with_search(event,false,false,false,false,false,true,false)}
        assert!(matches!(shortcut(&event(Code::Comma,Modifiers::CTRL|Modifiers::SHIFT)),Some(AppMessage::OpenSettings)));
        assert!(matches!(shortcut(&event(Code::Period,Modifiers::CTRL)),Some(AppMessage::AppMenuAction(crate::app_menu::Action::Shortcuts))));
        assert!(matches!(shortcut(&event(Code::F1,Modifiers::CTRL)),Some(AppMessage::AppMenuAction(crate::app_menu::Action::Help))));
        assert!(shortcut(&event(Code::KeyC,Modifiers::CTRL)).is_none());
        assert!(shortcut(&event(Code::Period,Modifiers::empty())).is_none());
    }

    #[test]
    fn reading_shortcuts_leave_clipboard_keys_to_the_focused_input() {
        use iced::keyboard::{
            Event as KeyEvent, Key, Location, Modifiers,
            key::{Code, Physical},
        };
        let key = |code, modifiers| {
            Event::Keyboard(KeyEvent::KeyPressed {
                key: Key::Character("+".into()),
                modified_key: Key::Character("+".into()),
                physical_key: Physical::Code(code),
                location: Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            })
        };
        assert!(matches!(
            shortcut(
                &key(Code::Equal, Modifiers::CTRL),
                false,
                false,
                false,
                false
            ),
            Some(AppMessage::ZoomStep(10))
        ));
        assert!(
            shortcut(
                &key(Code::Equal, Modifiers::empty()),
                false,
                false,
                false,
                false
            )
            .is_none()
        );
        assert!(shortcut(&key(Code::KeyV, Modifiers::CTRL), true, false, false, false).is_none());
        assert!(matches!(
            shortcut(
                &key(Code::Comma, Modifiers::CTRL),
                false,
                false,
                false,
                false
            ),
            Some(AppMessage::OpenSettings)
        ));
    }
    #[test]
    fn escape_closes_the_message_menu_before_settings() {
        use iced::keyboard::{
            Event as KeyEvent, Key, Location, Modifiers,
            key::{Code, Physical},
        };
        let escape = Event::Keyboard(KeyEvent::KeyPressed {
            key: Key::Named(iced::keyboard::key::Named::Escape),
            modified_key: Key::Named(iced::keyboard::key::Named::Escape),
            physical_key: Physical::Code(Code::Escape),
            location: Location::Standard,
            modifiers: Modifiers::empty(),
            text: None,
            repeat: false,
        });
        assert!(matches!(
            shortcut(&escape, true, false, true, false),
            Some(AppMessage::CloseMessageMenu)
        ));
        assert!(matches!(
            shortcut(&escape, true, false, false, false),
            Some(AppMessage::CloseSettings)
        ));
        assert!(matches!(
            shortcut_with_overlays(&escape, true, false, false, false, true, true),
            Some(AppMessage::CloseNewConversation)
        ));
        let ordinary_key = Event::Keyboard(KeyEvent::KeyPressed {
            key: Key::Character("x".into()),
            modified_key: Key::Character("x".into()),
            physical_key: Physical::Code(Code::KeyX),
            location: Location::Standard,
            modifiers: Modifiers::CTRL,
            text: None,
            repeat: false,
        });
        assert!(shortcut_with_overlays(
            &ordinary_key,
            false,
            true,
            true,
            false,
            true,
            false
        )
        .is_none());
    }

    #[test]
    fn settings_find_shortcut_works_in_inputs_but_respects_covering_modals() {
        use iced::keyboard::{Event as KeyEvent, Key, Location, Modifiers, key::{Code, Physical}};
        let event = Event::Keyboard(KeyEvent::KeyPressed {
            key: Key::Character("f".into()), modified_key: Key::Character("f".into()),
            physical_key: Physical::Code(Code::KeyF), location: Location::Standard,
            modifiers: Modifiers::CTRL | Modifiers::ALT, text: None, repeat: false,
        });
        assert!(matches!(shortcut_with_search(&event, true, false, false, false, false, true, false), Some(AppMessage::FocusSettingsSearch)));
        assert!(shortcut_with_search(&event, false, false, false, false, false, true, false).is_none());
        assert!(shortcut_with_search(&event, true, false, true, false, false, true, false).is_none());
        assert!(shortcut_with_search(&event, true, false, false, false, true, true, false).is_none());
        assert!(shortcut_with_search(&event, true, false, false, true, false, true, false).is_none());
    }
    #[test]
    fn message_navigation_and_copy_shortcuts_respect_text_input_focus() {
        use iced::keyboard::{
            Event as KeyEvent, Key, Location, Modifiers,
            key::{Code, Named, Physical},
        };
        let key = |physical_key, key, modifiers| {
            Event::Keyboard(KeyEvent::KeyPressed {
                key,
                modified_key: Key::Character(String::new().into()),
                physical_key,
                location: Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            })
        };
        let navigate = key(
            Physical::Code(Code::ArrowUp),
            Key::Named(Named::ArrowUp),
            Modifiers::CTRL | Modifiers::ALT,
        );
        assert!(matches!(
            shortcut(&navigate, false, false, false, false),
            Some(AppMessage::NavigateMessageSelection(-1))
        ));
        assert!(shortcut(&navigate, false, false, false, true).is_none());
        let open = key(
            Physical::Code(Code::F10),
            Key::Character("F10".into()),
            Modifiers::SHIFT,
        );
        assert!(matches!(
            shortcut(&open, false, true, false, false),
            Some(AppMessage::OpenSelectedMessageMenu)
        ));
        assert!(shortcut(&open, false, true, false, true).is_none());
        let enter = key(
            Physical::Code(Code::Enter),
            Key::Named(Named::Enter),
            Modifiers::empty(),
        );
        assert!(matches!(
            shortcut(&enter, false, true, true, false),
            Some(AppMessage::CopySelectedMessage)
        ));
        assert!(shortcut(&enter, false, true, true, true).is_none());
        let copy = key(
            Physical::Code(Code::KeyC),
            Key::Character("c".into()),
            Modifiers::CTRL,
        );
        assert!(matches!(
            shortcut(&copy, false, true, true, false),
            Some(AppMessage::CopySelectedMessage)
        ));
        assert!(matches!(
            shortcut(&copy, false, true, false, false),
            Some(AppMessage::CopySelectedMessage)
        ));
        assert!(shortcut(&copy, false, true, false, true).is_none());
        assert!(shortcut(&copy, false, false, false, false).is_none());
    }

    #[test]
    fn selected_graphemes_keeps_the_marked_span() {
        assert_eq!(selected_graphemes("Olá 👍 mundo", 4, 5), "👍");
        assert_eq!(selected_graphemes("abcdef", 4, 1), "bcd");
    }

    #[test]
    fn ctrl_f_focuses_conversation_search_without_stealing_text_input_or_overlays() {
        use iced::keyboard::{
            Event as KeyEvent, Key, Location, Modifiers,
            key::{Code, Physical},
        };
        let make_event = |modifiers| {
            Event::Keyboard(KeyEvent::KeyPressed {
                key: Key::Character("f".into()),
                modified_key: Key::Character("f".into()),
                physical_key: Physical::Code(Code::KeyF),
                location: Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            })
        };
        let ctrl_f = make_event(Modifiers::CTRL);
        assert!(matches!(
            shortcut_with_overlays(&ctrl_f, false, false, false, false, false, false),
            Some(AppMessage::FocusMessageSearch)
        ));
        assert!(shortcut_with_overlays(&ctrl_f, false, false, false, false, false, true).is_none());
        assert!(shortcut_with_overlays(&ctrl_f, true, false, false, false, false, false).is_none());
        assert!(shortcut_with_overlays(&ctrl_f, false, false, true, false, false, false).is_none());
        assert!(shortcut_with_overlays(&ctrl_f, false, false, false, true, false, false).is_none());
        assert!(shortcut_with_overlays(&ctrl_f, false, false, false, false, true, false).is_none());

        assert!(shortcut_with_overlays(
            &make_event(Modifiers::CTRL | Modifiers::SHIFT),
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .is_none());
        assert!(shortcut_with_overlays(
            &make_event(Modifiers::CTRL | Modifiers::ALT),
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .is_none());
    }

    #[test]
    fn ctrl_alt_g_jumps_to_a_chat_or_channel_even_from_a_text_field() {
        use iced::keyboard::{
            Event as KeyEvent, Key, Location, Modifiers,
            key::{Code, Physical},
        };
        let jump = |modifiers, repeat| {
            Event::Keyboard(KeyEvent::KeyPressed {
                key: Key::Character("g".into()),
                modified_key: Key::Character("g".into()),
                physical_key: Physical::Code(Code::KeyG),
                location: Location::Standard,
                modifiers,
                text: None,
                repeat,
            })
        };
        let chord = jump(Modifiers::CTRL | Modifiers::ALT, false);
        assert!(matches!(
            shortcut_with_overlays(&chord, false, false, false, false, false, true),
            Some(AppMessage::FocusSearch)
        ));
        assert!(matches!(
            shortcut_with_search(&chord, false, true, false, false, false, true, true),
            Some(AppMessage::FocusSearch)
        ));
        assert!(shortcut_with_overlays(&chord, true, false, false, false, false, false).is_none());
        assert!(shortcut_with_overlays(&chord, false, false, true, false, false, false).is_none());
        assert!(shortcut_with_overlays(&chord, false, false, false, true, false, false).is_none());
        assert!(shortcut_with_overlays(&chord, false, false, false, false, true, false).is_none());
        assert!(shortcut_with_overlays(
            &jump(Modifiers::CTRL | Modifiers::ALT | Modifiers::SHIFT, false),
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .is_none());
        assert!(shortcut_with_overlays(
            &jump(Modifiers::CTRL | Modifiers::ALT, true),
            false,
            false,
            false,
            false,
            false,
            true,
        )
        .is_none());
        let official = Event::Keyboard(KeyEvent::KeyPressed {
            key: Key::Character("e".into()),
            modified_key: Key::Character("e".into()),
            physical_key: Physical::Code(Code::KeyE),
            location: Location::Standard,
            modifiers: Modifiers::CTRL | Modifiers::ALT,
            text: None,
            repeat: false,
        });
        assert!(matches!(
            shortcut_with_overlays(&official, false, false, false, false, false, true),
            Some(AppMessage::FocusSearch)
        ));
    }

    #[test]
    fn rich_composer_shortcuts_format_selection_and_send_without_stealing_newline() {
        use iced::keyboard::{
            Event as KeyEvent, Key, Location, Modifiers,
            key::{Code, Named, Physical},
        };

        let character = |letter: &str, code, modifiers| {
            Event::Keyboard(KeyEvent::KeyPressed {
                key: Key::Character(letter.into()),
                modified_key: Key::Character(letter.into()),
                physical_key: Physical::Code(code),
                location: Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            })
        };
        let enter = |modifiers| {
            Event::Keyboard(KeyEvent::KeyPressed {
                key: Key::Named(Named::Enter),
                modified_key: Key::Named(Named::Enter),
                physical_key: Physical::Code(Code::Enter),
                location: Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            })
        };

        assert!(matches!(
            rich_composer_shortcut(&character("b", Code::KeyB, Modifiers::CTRL)),
            Some(AppMessage::ApplyDraftMark(crate::message_body::InlineMark::Bold))
        ));
        assert!(matches!(
            rich_composer_shortcut(&character("i", Code::KeyI, Modifiers::CTRL)),
            Some(AppMessage::ApplyDraftMark(crate::message_body::InlineMark::Italic))
        ));
        assert!(matches!(
            rich_composer_shortcut(&character("u", Code::KeyU, Modifiers::CTRL)),
            Some(AppMessage::ApplyDraftMark(
                crate::message_body::InlineMark::Underline
            ))
        ));
        assert!(matches!(
            rich_composer_shortcut(&enter(Modifiers::CTRL)),
            Some(AppMessage::SendMessage)
        ));
        assert!(rich_composer_shortcut(&enter(Modifiers::CTRL | Modifiers::SHIFT)).is_none());
        assert!(rich_composer_shortcut(
            &character("b", Code::KeyB, Modifiers::CTRL | Modifiers::SHIFT)
        )
        .is_none());
        assert!(rich_composer_shortcut(
            &character("b", Code::KeyB, Modifiers::CTRL | Modifiers::ALT)
        )
        .is_none());
    }

    #[test]
    fn escape_closes_conversation_search_after_higher_priority_overlays() {
        use iced::keyboard::{
            Event as KeyEvent, Key, Location, Modifiers,
            key::{Code, Named, Physical},
        };
        let escape = Event::Keyboard(KeyEvent::KeyPressed {
            key: Key::Named(Named::Escape),
            modified_key: Key::Named(Named::Escape),
            physical_key: Physical::Code(Code::Escape),
            location: Location::Standard,
            modifiers: Modifiers::empty(),
            text: None,
            repeat: false,
        });

        assert!(matches!(
            shortcut_with_search(&escape, false, false, false, false, false, true, true),
            Some(AppMessage::ToggleMessageSearch)
        ));
        assert!(matches!(
            shortcut_with_search(&escape, true, false, false, false, false, true, true),
            Some(AppMessage::CloseSettings)
        ));
        assert!(shortcut_with_search(&escape, false, false, false, false, false, false, false)
            .is_none());
    }

    #[test]
    fn composer_publishes_the_active_grapheme_selection() {
        let text = "A👨‍👩‍👧‍👦e\u{301}Z";
        let input: Element<'_, AppMessage, Theme, ()> = iced::widget::text_input("", text)
            .on_input(AppMessage::DraftChanged)
            .into();
        let mut widget = Observed {
            inner: input,
            context: "chat:test".into(),
            text: None,
            image_url: None,
            value: text.into(),
            shortcuts: None,
            rich_editor: false,
        shortcuts_suspended: false,
        };
        let mut tree = Tree::new(&widget as &dyn Widget<AppMessage, Theme, ()>);
        let node = widget.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(400.0, 60.0)),
        );
        tree.children[0]
            .state
            .downcast_mut::<iced::widget::text_input::State<()>>()
            .select_range(1, 3);
        let event = Event::Mouse(mouse::Event::CursorMoved {
            position: iced::Point::ORIGIN,
        });
        let mut messages = Vec::new();
        widget.update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(Size::new(400.0, 60.0)),
        );
        assert!(matches!(
            messages.as_slice(),
            [AppMessage::DraftSelectionChanged(key, Some((1, 3)))] if key == "chat:test"
        ));
    }

    #[test]
    fn ctrl_v_uses_the_real_input_selection_and_does_not_intercept_other_focus() {
        use iced::keyboard::{
            Event as KeyEvent, Key, Location, Modifiers,
            key::{Code, Physical},
        };
        let text = "A👨‍👩‍👧‍👦e\u{301}Z";
        let input: Element<'_, AppMessage, Theme, ()> = iced::widget::text_input("", text)
            .on_input(AppMessage::DraftChanged)
            .into();
        let mut widget = Observed {
            inner: input,
            context: "chat:test".into(),
            text: None,
            image_url: None,
            value: text.into(),
            shortcuts: None,
            rich_editor: false,
        shortcuts_suspended: false,
        };
        let mut tree = Tree::new(&widget as &dyn Widget<AppMessage, Theme, ()>);
        let node = widget.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(400.0, 60.0)),
        );
        let layout = Layout::new(&node);
        let viewport = Rectangle::with_size(Size::new(400.0, 60.0));
        let key = Event::Keyboard(KeyEvent::KeyPressed {
            key: Key::Character("v".into()),
            modified_key: Key::Character("v".into()),
            physical_key: Physical::Code(Code::KeyV),
            location: Location::Standard,
            modifiers: Modifiers::CTRL,
            text: None,
            repeat: false,
        });
        let mut messages = Vec::new();
        // No focus: the composer must not consume a paste intended for search.
        widget.update(
            &mut tree,
            &key,
            layout,
            mouse::Cursor::Unavailable,
            &(),
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert!(messages.is_empty());
        let input_state = tree.children[0]
            .state
            .downcast_mut::<iced::widget::text_input::State<()>>();
        input_state.focus();
        input_state.select_range(1, 3);
        widget.update(
            &mut tree,
            &key,
            layout,
            mouse::Cursor::Unavailable,
            &(),
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert!(matches!(
            messages.as_slice(),
            [AppMessage::PasteClipboard(Some((1, 3)))]
        ));
        messages.clear();
        widget.update(
            &mut tree,
            &Event::Keyboard(KeyEvent::ModifiersChanged(Modifiers::empty())),
            layout,
            mouse::Cursor::Unavailable,
            &(),
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        tree.children[0]
            .state
            .downcast_mut::<iced::widget::text_input::State<()>>()
            .unfocus();
        widget.text = Some((1, "X".into(), (1, 3)));
        let moved = Event::Mouse(mouse::Event::CursorMoved {
            position: iced::Point::ORIGIN,
        });
        for _ in 0..2 {
            widget.update(
                &mut tree,
                &moved,
                layout,
                mouse::Cursor::Unavailable,
                &(),
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        assert_eq!(
            messages
                .iter()
                .filter(
                    |message| matches!(message, AppMessage::DraftChanged(value) if value == "AXZ")
                )
                .count(),
            1
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, AppMessage::TextPasteFinished(1, false)))
        );
        assert!(
            !tree.children[0]
                .state
                .downcast_ref::<iced::widget::text_input::State<()>>()
                .is_focused()
        );
    }
}
