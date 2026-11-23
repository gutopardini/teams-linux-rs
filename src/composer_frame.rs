use crate::appearance::{Tone, tone, ui_surface};
use crate::AppMessage;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Id, Operation, Tree, operation::{Focusable, focusable}, tree},
};
use iced::widget::Container;
use iced::{Border, Element, Event, Length, Rectangle, Size, Theme, Vector};

pub const TEXT_INPUT: &str = "message-composer";
pub const RICH_EDITOR: &str = "message-composer-rich";
const FOCUS_STROKE: f32 = 2.0;
const RADIUS: f32 = 16.0;

pub fn editor_height_limit(window_height: f32, attachment_height: f32) -> f32 {
    let preferred = (window_height * 0.5).clamp(64.0, 384.0);
    preferred.min((window_height - 220.0 - attachment_height).max(64.0))
}

pub fn view<'a>(
    inner: Container<'a, AppMessage>,
    window_focused: bool,
    demo_mode: bool,
    rich: bool,
) -> Element<'a, AppMessage> {
    static PREVIEW: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    let preview_blur = if demo_mode {
        *PREVIEW.get_or_init(|| {
            crate::preview_path()?;
            std::env::args().find_map(|arg| match arg.as_str() {
                "--composer-focus-demo" => Some(false),
                "--composer-focus-demo=blurred" => Some(true),
                _ => None,
            })
        })
    } else {
        None
    };
    Element::new(Frame {
        inner: inner.style(ui_surface(Tone::Surface, true, RADIUS)).into(),
        window_focused: window_focused || preview_blur.is_some(),
        editor_id: if rich { RICH_EDITOR } else { TEXT_INPUT },
        preview_blur,
    })
}

struct Frame<'a, R: renderer::Renderer = iced::Renderer> {
    inner: Element<'a, AppMessage, Theme, R>,
    window_focused: bool,
    editor_id: &'static str,
    preview_blur: Option<bool>,
}

#[derive(Default)]
struct ComposerFocus(bool);

#[derive(Default)]
struct State {
    focused: bool,
    preview_ready: bool,
}

impl Operation for ComposerFocus {
    fn focusable(&mut self, id: Option<&Id>, _: Rectangle, state: &mut dyn Focusable) {
        if id.is_some_and(|id| id == &Id::new(TEXT_INPUT) || id == &Id::new(RICH_EDITOR)) {
            self.0 |= state.is_focused();
        }
    }

    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
}

impl<R: renderer::Renderer> Widget<AppMessage, Theme, R> for Frame<'_, R> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        self.inner.as_widget().size()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.inner)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.inner));
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        self.inner.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &R,
        operation: &mut dyn Operation,
    ) {
        self.inner.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
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
        if let Some(blur) = self.preview_blur
            && matches!(event, Event::Window(iced::window::Event::RedrawRequested(_)))
            && !tree.state.downcast_ref::<State>().preview_ready
        {
            self.operate(tree, layout, renderer, &mut focusable::focus(Id::new(self.editor_id)));
            if blur {
                self.operate(tree, layout, renderer, &mut focusable::unfocus());
            }
            tree.state.downcast_mut::<State>().preview_ready = true;
        }
        self.inner.as_widget_mut().update(
            &mut tree.children[0], event, layout, cursor, renderer, clipboard, shell, viewport,
        );
        if matches!(event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            | Event::Touch(iced::touch::Event::FingerPressed { .. })
            | Event::Window(iced::window::Event::RedrawRequested(_))
        ) {
            let mut focus = ComposerFocus::default();
            self.operate(tree, layout, renderer, &mut focus);
            let state = tree.state.downcast_mut::<State>();
            if state.focused != focus.0 {
                state.focused = focus.0;
                shell.request_redraw();
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
        self.inner.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
        if self.window_focused && tree.state.downcast_ref::<State>().focused {
            let bounds = layout.bounds();
            let height = (2.0 * FOCUS_STROKE).min(bounds.height);
            let strip = Rectangle { y: bounds.y + bounds.height - height, height, ..bounds };
            if let Some(clip) = strip.intersection(viewport) {
                renderer.with_layer(clip, |renderer| {
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds,
                            border: Border::default().rounded(RADIUS).width(FOCUS_STROKE)
                                .color(tone(theme, Tone::Accent)),
                            ..Default::default()
                        },
                        iced::Color::TRANSPARENT,
                    );
                });
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &R,
    ) -> mouse::Interaction {
        self.inner.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &R,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, AppMessage, Theme, R>> {
        self.inner.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::{column, text_editor, text_input};
    use iced::window;

    #[test]
    fn composer_indicator_follows_native_focus_in_both_editors() {
        let content = text_editor::Content::<()>::with_text("Rascunho");
        let plain = text_input("", "Rascunho").id(TEXT_INPUT).on_input(AppMessage::DraftChanged);
        let rich = text_editor(&content).id(RICH_EDITOR).on_action(AppMessage::DraftAction);
        let editors: [Element<'_, AppMessage, Theme, ()>; 2] = [plain.into(), rich.into()];
        for editor in editors {
            let other = text_input("Pesquisar", "").id("search").on_input(AppMessage::DraftChanged);
            let mut frame = Frame {
                inner: column![editor, other].into(),
                window_focused: true,
                editor_id: TEXT_INPUT,
                preview_blur: None,
            };
            let mut tree = Tree::new(&frame as &dyn Widget<AppMessage, Theme, ()>);
            let bounds = Rectangle::with_size(Size::new(300.0, 120.0));
            let node = frame.layout(&mut tree, &(), &layout::Limits::new(Size::ZERO, bounds.size()));
            let layout = Layout::new(&node);
            let cursor = mouse::Cursor::Available(layout.children().next().unwrap().bounds().center());
            let redraw = Event::Window(window::Event::RedrawRequested(iced::time::Instant::now()));
            let click = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
            for (event, expected) in [(&redraw, false), (&click, true)] {
                frame.update(
                    &mut tree, event, layout, cursor, &(), &mut iced::advanced::clipboard::Null,
                    &mut Shell::new(&mut Vec::new()), &bounds,
                );
                assert_eq!(tree.state.downcast_ref::<State>().focused, expected);
            }
            frame.operate(&mut tree, layout, &(), &mut focusable::focus(Id::new("search")));
            frame.update(
                &mut tree, &redraw, layout, cursor, &(), &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut Vec::new()), &bounds,
            );
            assert!(!tree.state.downcast_ref::<State>().focused);
        }
    }
}
