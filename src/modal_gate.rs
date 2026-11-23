//! Retain the covered widget tree, but suspend its input, layout and rendering
//! while a full-window modal owns the viewport. Closing restores native state.
use iced::advanced::{
    self, Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{self, Tree},
};
use iced::{Element, Length, Rectangle, Size, Vector};

pub fn view<'a, Message: 'a, Theme: 'a, R: renderer::Renderer + 'a>(
    inner: impl Into<Element<'a, Message, Theme, R>>,
    blocked: bool,
) -> Element<'a, Message, Theme, R> {
    Element::new(Gate {
        inner: inner.into(),
        blocked,
        visible: !blocked,
    })
}
/// Render the application chrome in a modal without allowing its controls to
/// change the covered page or steal keyboard focus.
pub fn inert<'a, Message: 'a, Theme: 'a, R: renderer::Renderer + 'a>(
    inner: impl Into<Element<'a, Message, Theme, R>>,
) -> Element<'a, Message, Theme, R> {
    Element::new(Gate {
        inner: inner.into(),
        blocked: true,
        visible: true,
    })
}
struct Gate<'a, Message, Theme, R> {
    inner: Element<'a, Message, Theme, R>,
    blocked: bool,
    visible: bool,
}
impl<Message, Theme, R: renderer::Renderer> Widget<Message, Theme, R>
    for Gate<'_, Message, Theme, R>
{
    fn size(&self) -> Size<Length> {
        self.inner.as_widget().size()
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.inner.as_widget())]
    }
    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[self.inner.as_widget()]);
    }
    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        if !self.visible {
            layout::Node::new(limits.resolve(Length::Fill, Length::Fill, Size::ZERO))
        } else {
            self.inner
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, limits)
        }
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &iced::Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &R,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if !self.blocked {
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
        if self.visible {
            self.inner.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                layout,
                if self.blocked {
                    mouse::Cursor::Unavailable
                } else {
                    cursor
                },
                viewport,
            );
        }
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &R,
        operation: &mut dyn widget::Operation,
    ) {
        if !self.blocked {
            self.inner
                .as_widget_mut()
                .operate(&mut tree.children[0], layout, renderer, operation);
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
        if self.blocked {
            mouse::Interaction::default()
        } else {
            self.inner.as_widget().mouse_interaction(
                &tree.children[0],
                layout,
                cursor,
                viewport,
                renderer,
            )
        }
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &R,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<advanced::overlay::Element<'a, Message, Theme, R>> {
        if self.blocked {
            None
        } else {
            self.inner.as_widget_mut().overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            )
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[derive(Default)]
    struct State {
        value: u32,
    }
    struct Probe(Arc<AtomicUsize>);
    impl Widget<u8, (), ()> for Probe {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fill)
        }
        fn tag(&self) -> widget::tree::Tag {
            widget::tree::Tag::of::<State>()
        }
        fn state(&self) -> widget::tree::State {
            widget::tree::State::new(State { value: 42 })
        }
        fn layout(&mut self, _: &mut Tree, _: &(), limits: &layout::Limits) -> layout::Node {
            self.0.fetch_add(1, Ordering::SeqCst);
            layout::Node::new(limits.max())
        }
        fn update(
            &mut self,
            _: &mut Tree,
            _: &iced::Event,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &(),
            _: &mut dyn Clipboard,
            shell: &mut Shell<'_, u8>,
            _: &Rectangle,
        ) {
            shell.publish(1);
        }
        fn draw(
            &self,
            _: &Tree,
            _: &mut (),
            _: &(),
            _: &renderer::Style,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &Rectangle,
        ) {
        }
    }
    #[test]
    fn covered_base_does_not_receive_input_or_layout_and_retains_state_when_restored() {
        let layouts = Arc::new(AtomicUsize::new(0));
        let make = |blocked| view(Element::new(Probe(layouts.clone())), blocked);
        let mut element = make(false);
        let mut tree = Tree::new(element.as_widget());
        tree.children[0].state.downcast_mut::<State>().value = 73;
        let renderer = ();
        let limits = layout::Limits::new(Size::ZERO, Size::new(100.0, 100.0));
        let _ = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(layouts.load(Ordering::SeqCst), 1);
        let mut covered = make(true);
        tree.diff(covered.as_widget());
        let node = covered
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(layouts.load(Ordering::SeqCst), 1);
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        let mut clipboard = advanced::clipboard::Null;
        covered.as_widget_mut().update(
            &mut tree,
            &iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard,
            &mut shell,
            &Rectangle::with_size(Size::new(100.0, 100.0)),
        );
        assert!(messages.is_empty());
        let mut restored = make(false);
        tree.diff(restored.as_widget());
        assert_eq!(tree.children[0].state.downcast_ref::<State>().value, 73);
        let node = restored
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut shell = Shell::new(&mut messages);
        restored.as_widget_mut().update(
            &mut tree,
            &iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard,
            &mut shell,
            &Rectangle::with_size(Size::new(100.0, 100.0)),
        );
        assert_eq!(messages, vec![1]);
    }
}
