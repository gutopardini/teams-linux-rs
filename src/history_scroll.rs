//! Reset native scroll state on conversation changes and snap only the first
//! ready layout to the tail. No timer, redraw loop or estimated message height.
use iced::advanced::{
    self, Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{self, Tree},
};
use iced::{Element, Length, Rectangle, Size, Vector};

pub fn view<'a, Message: 'a, Theme: 'a, R: renderer::Renderer + 'a>(
    inner: impl Into<Element<'a, Message, Theme, R>>,
    context: String,
    loading: bool,
    open_at_tail: bool,
    show_overlays: bool,
) -> Element<'a, Message, Theme, R> {
    Element::new(HistoryStart {
        inner: inner.into(),
        context,
        loading,
        open_at_tail,
        show_overlays,
    })
}
struct HistoryStart<'a, Message, Theme, R> {
    inner: Element<'a, Message, Theme, R>,
    context: String,
    loading: bool,
    open_at_tail: bool,
    show_overlays: bool,
}
impl<Message, Theme, R: renderer::Renderer> Widget<Message, Theme, R>
    for HistoryStart<'_, Message, Theme, R>
{
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<State>()
    }
    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(State {
            context: self.context.clone(),
            pending: true,
        })
    }
    fn size(&self) -> Size<Length> {
        self.inner.as_widget().size()
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.inner.as_widget())]
    }
    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        if state.context != self.context {
            *state = State {
                context: self.context.clone(),
                pending: true,
            };
            tree.children = vec![Tree::new(self.inner.as_widget())];
        } else {
            tree.diff_children(&[self.inner.as_widget()]);
        }
    }
    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        let node = self
            .inner
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let state = tree.state.downcast_mut::<State>();
        if state.pending && !self.loading {
            if !self.open_at_tail {
                state.pending = false;
                return node;
            }
            let mut end = widget::operation::scrollable::snap_to::<()>(
                widget::Id::new("message-history"),
                widget::operation::scrollable::RelativeOffset {
                    x: None,
                    y: Some(1.0),
                },
            );
            self.inner.as_widget_mut().operate(
                &mut tree.children[0],
                Layout::new(&node),
                renderer,
                &mut end,
            );
            state.pending = false;
        }
        node
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
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &R,
        operation: &mut dyn widget::Operation,
    ) {
        self.inner
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
    ) -> Option<advanced::overlay::Element<'a, Message, Theme, R>> {
        if !self.show_overlays {
            return None;
        }
        self.inner.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

struct State {
    context: String,
    pending: bool,
}

pub fn message_anchor(id: &str) -> widget::Id {
    widget::Id::from(format!("history-message:{id}"))
}

/// Row top inside the history scrollable. Layout bounds ignore the current
/// scroll translation, so this is the absolute offset that puts that row at
/// the top. `fallback` is used only when the row is not in the tree yet.
pub struct LocateMessage {
    target: widget::Id,
    fallback: Option<f32>,
    content_top: Option<f32>,
    message_top: Option<f32>,
}

impl LocateMessage {
    pub fn new(target: widget::Id, fallback: Option<f32>) -> Self {
        Self {
            target,
            fallback,
            content_top: None,
            message_top: None,
        }
    }

    pub fn observe_container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
        if id == Some(&self.target) && self.message_top.is_none() {
            self.message_top = Some(bounds.y);
        }
    }

    pub fn observe_history(&mut self, content_top: f32) {
        self.content_top = Some(content_top);
    }

    pub fn offset(&self) -> Option<f32> {
        if let (Some(message), Some(content)) = (self.message_top, self.content_top) {
            Some((message - content).max(0.0))
        } else {
            self.fallback.map(|fallback| fallback.max(0.0))
        }
    }
}

pub fn locate_message(target: widget::Id, fallback: Option<f32>) -> LocateMessage {
    LocateMessage::new(target, fallback)
}

impl widget::Operation<f32> for LocateMessage {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn widget::Operation<f32>)) {
        operate(self);
    }

    fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
        self.observe_container(id, bounds);
    }

    fn scrollable(
        &mut self,
        id: Option<&widget::Id>,
        _bounds: Rectangle,
        content_bounds: Rectangle,
        _translation: Vector,
        _state: &mut dyn widget::operation::Scrollable,
    ) {
        if id == Some(&widget::Id::new("message-history")) {
            self.observe_history(content_bounds.y);
        }
    }

    fn finish(&self) -> widget::operation::Outcome<f32> {
        match self.offset() {
            Some(offset) => widget::operation::Outcome::Some(offset),
            None => widget::operation::Outcome::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::advanced::widget::operation::Scrollable;
    use iced::{Point, Theme};
    fn panel(id: &str, loading: bool, content: f32) -> Element<'static, (), Theme, ()> {
        view(
            iced::widget::scrollable(iced::widget::space::vertical().height(content))
                .width(200)
                .height(100)
                .id("message-history"),
            id.into(),
            loading,
            true,
            true,
        )
    }
    #[derive(Default)]
    struct Position(Option<(f32, f32)>);
    impl widget::Operation for Position {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn widget::Operation)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: Rectangle,
            content: Rectangle,
            translation: Vector,
            _: &mut dyn Scrollable,
        ) {
            if id == Some(&widget::Id::new("message-history")) {
                self.0 = Some((translation.y, (content.height - bounds.height).max(0.0)));
            }
        }
    }
    fn position(
        element: &mut Element<'_, (), Theme, ()>,
        tree: &mut Tree,
        node: &layout::Node,
    ) -> (f32, f32) {
        let mut position = Position::default();
        element
            .as_widget_mut()
            .operate(tree, Layout::new(node), &(), &mut position);
        position.0.unwrap()
    }
    fn layout_panel(element: &mut Element<'_, (), Theme, ()>, tree: &mut Tree) -> layout::Node {
        element.as_widget_mut().layout(
            tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 100.0)),
        )
    }
    #[test]
    fn native_scrollbar_opens_at_tail_after_load_and_does_not_fight_manual_wheel() {
        let mut loading = panel("a", true, 20.0);
        let mut tree = Tree::new(loading.as_widget());
        let node = layout_panel(&mut loading, &mut tree);
        assert_eq!(position(&mut loading, &mut tree, &node), (0.0, 0.0));
        let mut ready = panel("a", false, 3000.0);
        tree.diff(ready.as_widget());
        let node = layout_panel(&mut ready, &mut tree);
        assert_eq!(position(&mut ready, &mut tree, &node), (2900.0, 2900.0));
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        let mut clipboard = advanced::clipboard::Null;
        ready.as_widget_mut().update(
            &mut tree,
            &iced::Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 5.0 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(Point::new(50.0, 50.0)),
            &(),
            &mut clipboard,
            &mut shell,
            &Rectangle::with_size(Size::new(200.0, 100.0)),
        );
        let (reading, _) = position(&mut ready, &mut tree, &node);
        assert!(reading < 2900.0);
        let mut grown = panel("a", false, 3500.0);
        tree.diff(grown.as_widget());
        let node = layout_panel(&mut grown, &mut tree);
        assert_eq!(position(&mut grown, &mut tree, &node).0, reading);
        let mut next = panel("b", false, 5000.0);
        tree.diff(next.as_widget());
        let node = layout_panel(&mut next, &mut tree);
        assert_eq!(position(&mut next, &mut tree, &node), (4900.0, 4900.0));
        let mut back = panel("a", false, 3000.0);
        tree.diff(back.as_widget());
        let node = layout_panel(&mut back, &mut tree);
        assert_eq!(position(&mut back, &mut tree, &node), (2900.0, 2900.0));
    }
    #[test]
    fn ready_layout_follows_late_media_geometry_until_the_user_scrolls() {
        let mut initial = panel("a", false, 3000.0);
        let mut tree = Tree::new(initial.as_widget());
        let node = layout_panel(&mut initial, &mut tree);
        assert_eq!(position(&mut initial, &mut tree, &node).0, 2900.0);
        let mut grown = panel("a", false, 8000.0);
        tree.diff(grown.as_widget());
        let node = layout_panel(&mut grown, &mut tree);
        assert_eq!(position(&mut grown, &mut tree, &node), (7900.0, 7900.0));
    }
    #[test]
    fn explicit_search_position_is_not_replaced_by_the_initial_tail_policy() {
        let mut targeted: Element<'static, (), Theme, ()> = view(
            iced::widget::scrollable(iced::widget::space::vertical().height(3000))
                .width(200)
                .height(100)
                .id("message-history"),
            "search-target".into(),
            false,
            false,
            true,
        );
        let mut tree = Tree::new(targeted.as_widget());
        let node = layout_panel(&mut targeted, &mut tree);
        let mut jump = widget::operation::scrollable::scroll_to::<()>(
            widget::Id::new("message-history"),
            widget::operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(700.0),
            },
        );
        targeted
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &(), &mut jump);
        let node = layout_panel(&mut targeted, &mut tree);
        assert_eq!(position(&mut targeted, &mut tree, &node).0, 700.0);
    }

    #[test]
    fn locate_message_uses_the_row_top_inside_the_history() {
        use iced::advanced::widget::operation::Scrollable;
        struct Measure {
            content_top: Option<f32>,
            message_top: Option<f32>,
            target: widget::Id,
        }
        impl widget::Operation for Measure {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn widget::Operation)) {
                visit(self);
            }
            fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
                if id == Some(&self.target) {
                    self.message_top = Some(bounds.y);
                }
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                _bounds: Rectangle,
                content_bounds: Rectangle,
                _translation: Vector,
                _: &mut dyn Scrollable,
            ) {
                if id == Some(&widget::Id::new("message-history")) {
                    self.content_top = Some(content_bounds.y);
                }
            }
        }
        let mut history: Element<'static, (), Theme, ()> = iced::widget::scrollable(
            iced::widget::column![
                iced::widget::container(iced::widget::space::vertical().height(180))
                    .id(message_anchor("a")),
                iced::widget::container(iced::widget::space::vertical().height(220))
                    .id(message_anchor("b")),
            ],
        )
        .width(200)
        .height(100)
        .id("message-history")
        .into();
        let mut tree = Tree::new(history.as_widget());
        let node = layout_panel(&mut history, &mut tree);
        let mut measure = Measure {
            content_top: None,
            message_top: None,
            target: message_anchor("b"),
        };
        history
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &(), &mut measure);
        let mut locate = locate_message(message_anchor("b"), Some(999.0));
        locate.observe_history(measure.content_top.expect("history content"));
        locate.observe_container(
            Some(&message_anchor("b")),
            Rectangle {
                x: 0.0,
                y: measure.message_top.expect("quoted row"),
                width: 200.0,
                height: 220.0,
            },
        );
        assert_eq!(locate.offset(), Some(180.0));
        locate.observe_container(Some(&widget::Id::new("other")), Rectangle::default());
        assert_eq!(locate.offset(), Some(180.0));
    }
}
