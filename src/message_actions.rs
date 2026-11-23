//! Message actions live in an overlay, so hover and menus never resize history.
use crate::AppMessage;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use iced::{Element, Event, Length, Point, Rectangle, Size, Theme, Vector};

pub struct Config {
    pub context: String,
    pub outgoing: bool,
    pub enabled: bool,
    pub force_visible: bool,
    pub reaction_picker: bool,
}

pub fn view<'a>(
    base: impl Into<Element<'a, AppMessage>>,
    build_toolbar: impl Fn() -> Element<'a, AppMessage> + 'a,
    popup: Option<Element<'a, AppMessage>>,
    config: Config,
) -> Element<'a, AppMessage> {
    Element::new(Actions {
        base: base.into(),
        build_toolbar: Box::new(build_toolbar),
        toolbar: None,
        popup,
        context: config.context,
        outgoing: config.outgoing,
        enabled: config.enabled,
        force_visible: config.force_visible,
        reaction_picker: config.reaction_picker,
    })
}

struct Actions<'a, R: renderer::Renderer = iced::Renderer> {
    base: Element<'a, AppMessage, Theme, R>,
    build_toolbar: Box<dyn Fn() -> Element<'a, AppMessage, Theme, R> + 'a>,
    toolbar: Option<Element<'a, AppMessage, Theme, R>>,
    popup: Option<Element<'a, AppMessage, Theme, R>>,
    context: String,
    outgoing: bool,
    enabled: bool,
    force_visible: bool,
    reaction_picker: bool,
}

#[derive(Default)]
struct State {
    context: String,
    hovered: bool,
    scroll_suppressed: bool,
    visible: bool,
}

fn toolbar_bounds(anchor: Rectangle, size: Size, viewport: Rectangle, outgoing: bool) -> Rectangle {
    let x = if outgoing { (anchor.x + anchor.width) - size.width }
        else { ((anchor.x + anchor.width) - size.width).max(anchor.x) };
    Rectangle {
        x: x.clamp(viewport.x + 4.0, ((viewport.x + viewport.width) - size.width - 4.0).max(viewport.x + 4.0)),
        y: (anchor.y - size.height + 6.0)
            .clamp(viewport.y + 4.0, ((viewport.y + viewport.height) - size.height - 4.0).max(viewport.y + 4.0)),
        width: size.width,
        height: size.height,
    }
}

fn popup_bounds(toolbar: Rectangle, size: Size, viewport: Rectangle, prefer_above: bool) -> Rectangle {
    let below = (toolbar.y + toolbar.height) + 4.0;
    let y = if prefer_above && toolbar.y - size.height - 4.0 >= viewport.y + 4.0 {
        toolbar.y - size.height - 4.0
    } else if below + size.height <= (viewport.y + viewport.height) - 4.0 {
        below
    } else {
        toolbar.y - size.height - 4.0
    };
    Rectangle {
        x: ((toolbar.x + toolbar.width) - size.width)
            .clamp(viewport.x + 4.0, ((viewport.x + viewport.width) - size.width - 4.0).max(viewport.x + 4.0)),
        y: y.clamp(viewport.y + 4.0, ((viewport.y + viewport.height) - size.height - 4.0).max(viewport.y + 4.0)),
        width: size.width,
        height: size.height,
    }
}

fn toolbar_uncovered(layout: Layout<'_>) -> bool {
    let mut children=layout.children();
    let toolbar=children.next().unwrap();
    children.next().is_none_or(|popup| !popup.bounds().intersects(&toolbar.bounds()))
}

fn set_hover(hovered: &mut bool, next: bool, shell: &mut Shell<'_, AppMessage>) {
    if *hovered != next {
        *hovered = next;
        shell.invalidate_layout();
        shell.request_redraw();
    }
}

impl<R: renderer::Renderer> Widget<AppMessage, Theme, R> for Actions<'_, R> {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<State>() }
    fn state(&self) -> tree::State {
        tree::State::new(State { context: self.context.clone(), ..State::default() })
    }
    fn children(&self) -> Vec<Tree> {
        let mut children = vec![Tree::new(&self.base), Tree::empty()];
        if let Some(popup) = &self.popup { children.push(Tree::new(popup)); }
        children
    }
    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<State>();
        let reset = state.context != self.context || (!self.enabled && state.visible);
        if reset {
            *state = State { context: self.context.clone(), ..State::default() };
        }
        tree.children[0].diff(&self.base);
        if reset { tree.children[1] = Tree::empty(); }
        if let Some(popup) = &self.popup {
            if let Some(popup_tree) = tree.children.get_mut(2) {
                popup_tree.diff(popup);
            } else {
                tree.children.push(Tree::new(popup));
            }
        } else {
            tree.children.truncate(2);
        }
    }
    fn size(&self) -> Size<Length> { self.base.as_widget().size() }
    fn size_hint(&self) -> Size<Length> { self.base.as_widget().size_hint() }
    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        let base = self.base.as_widget_mut().layout(&mut tree.children[0], renderer, limits);
        layout::Node::with_children(base.size(), vec![base])
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &R, operation: &mut dyn Operation) {
        self.base.as_widget_mut().operate(
            &mut tree.children[0], layout.children().next().unwrap(), renderer, operation,
        );
    }
    fn update(
        &mut self, tree: &mut Tree, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor,
        renderer: &R, clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, AppMessage>, viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        // An unavailable cursor may mean the toolbar above us owns it. The
        // overlay updates hover across both rectangles, including that handoff.
        if matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. })) {
            state.scroll_suppressed = true;
            set_hover(&mut state.hovered, false, shell);
        } else if matches!(event, Event::Mouse(mouse::Event::CursorMoved { .. })) {
            state.scroll_suppressed = false;
            if let Some(position) = cursor.position() {
                set_hover(&mut state.hovered, self.enabled && layout.bounds().contains(position) && viewport.contains(position), shell);
            }
        } else if matches!(event, Event::Keyboard(iced::keyboard::Event::KeyPressed { .. })) {
            state.scroll_suppressed = false;
        }
        if matches!(event, Event::Window(iced::window::Event::Unfocused) | Event::Mouse(mouse::Event::CursorLeft)) {
            set_hover(&mut state.hovered, false, shell);
        }
        self.base.as_widget_mut().update(
            &mut tree.children[0], event, layout.children().next().unwrap(), cursor,
            renderer, clipboard, shell, viewport,
        );
    }
    fn draw(
        &self, tree: &Tree, renderer: &mut R, theme: &Theme, style: &renderer::Style,
        layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle,
    ) {
        self.base.as_widget().draw(
            &tree.children[0], renderer, theme, style, layout.children().next().unwrap(), cursor, viewport,
        );
    }
    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle, renderer: &R) -> mouse::Interaction {
        self.base.as_widget().mouse_interaction(&tree.children[0], layout.children().next().unwrap(), cursor, viewport, renderer)
    }
    fn overlay<'a>(
        &'a mut self, tree: &'a mut Tree, layout: Layout<'a>, renderer: &R,
        viewport: &Rectangle, translation: Vector,
    ) -> Option<overlay::Element<'a, AppMessage, Theme, R>> {
        let state = tree.state.downcast_mut::<State>();
        let (base_tree, rest) = tree.children.split_first_mut()?;
        let (toolbar_tree, popup_trees) = rest.split_first_mut()?;
        let mut layers = Vec::new();
        if let Some(base) = self.base.as_widget_mut().overlay(
            base_tree, layout.children().next().unwrap(), renderer, viewport, translation,
        ) { layers.push(base); }
        let visible = self.enabled && (!state.scroll_suppressed || self.popup.is_some())
            && (state.hovered || self.force_visible || self.popup.is_some())
            && (layout.bounds() + translation).intersects(viewport);
        if state.visible && !visible {
            // A press followed by leaving the hover region must not survive
            // until a later release over a newly displayed toolbar.
            *toolbar_tree = Tree::empty();
            self.toolbar = None;
        }
        let refresh_toolbar = !state.visible || self.toolbar.is_none();
        state.visible = visible;
        if visible {
            let toolbar = self.toolbar.get_or_insert_with(&self.build_toolbar);
            if refresh_toolbar { toolbar_tree.diff(&*toolbar); }
            layers.push(overlay::Element::new(Box::new(ActionsOverlay {
                toolbar,
                toolbar_tree,
                popup: self.popup.as_mut().zip(popup_trees.first_mut()),
                hovered: &mut state.hovered,
                scroll_suppressed: &mut state.scroll_suppressed,
                anchor: layout.bounds() + translation,
                viewport: *viewport,
                outgoing: self.outgoing,
                reaction_picker: self.reaction_picker,
            })));
        }
        (!layers.is_empty()).then(|| overlay::Group::with_children(layers).overlay())
    }
}

struct ActionsOverlay<'a, 'b, R: renderer::Renderer> {
    toolbar: &'a mut Element<'b, AppMessage, Theme, R>,
    toolbar_tree: &'a mut Tree,
    popup: Option<(&'a mut Element<'b, AppMessage, Theme, R>, &'a mut Tree)>,
    hovered: &'a mut bool,
    scroll_suppressed: &'a mut bool,
    anchor: Rectangle,
    viewport: Rectangle,
    outgoing: bool,
    reaction_picker: bool,
}

impl<R: renderer::Renderer> overlay::Overlay<AppMessage, Theme, R> for ActionsOverlay<'_, '_, R> {
    fn layout(&mut self, renderer: &R, bounds: Size) -> layout::Node {
        let viewport = self.viewport.intersection(&Rectangle::with_size(bounds)).unwrap_or_default();
        let available = Size::new((viewport.width - 8.0).max(1.0), (viewport.height - 8.0).max(1.0));
        let mut toolbar = self.toolbar.as_widget_mut().layout(
            self.toolbar_tree, renderer, &layout::Limits::new(Size::ZERO, available),
        );
        let toolbar_rect = toolbar_bounds(self.anchor, toolbar.size(), viewport, self.outgoing);
        toolbar.move_to_mut(Point::new(toolbar_rect.x, toolbar_rect.y));
        let mut children = vec![toolbar];
        if let Some((popup, tree)) = &mut self.popup {
            // The expanded picker can escape the history viewport on a small
            // window; it remains an overlay and never resizes the conversation.
            let popup_viewport = if self.reaction_picker {
                Rectangle { x: 0.0, y: 64.0, width: bounds.width, height: (bounds.height - 72.0).max(1.0) }
            } else { viewport };
            let popup_available = popup_viewport.size() - Size::new(8.0,8.0);
            let mut popup = popup.as_widget_mut().layout(
                tree, renderer, &layout::Limits::new(Size::ZERO, Size::new(popup_available.width, popup_available.height.min(if self.reaction_picker { 448.0 } else { 380.0 }))),
            );
            let rect = popup_bounds(toolbar_rect, popup.size(), popup_viewport, self.reaction_picker);
            popup.move_to_mut(Point::new(rect.x, rect.y));
            children.push(popup);
        }
        layout::Node::with_children(bounds, children)
    }
    fn update(&mut self, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &R, clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, AppMessage>) {
        let mut children = layout.children();
        let toolbar = children.next().unwrap();
        let popup_layout = children.next();
        let on_toolbar = toolbar_uncovered(layout) && cursor.is_over(toolbar.bounds());
        let on_popup = popup_layout.is_some_and(|layout| cursor.is_over(layout.bounds()));
        // Never let the hover overlay hide the cursor from the parent scrollable.
        // The popup keeps its own wheel scrolling only when explicitly open.
        if matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. })) && !on_popup {
            *self.scroll_suppressed = true;
            set_hover(self.hovered, false, shell);
            if self.popup.is_some() { shell.publish(AppMessage::CloseMessageMenu); }
            return;
        }
        if matches!(event, Event::Mouse(mouse::Event::CursorMoved { .. })) {
            *self.scroll_suppressed = false;
            if let Some(position) = cursor.position() {
                set_hover(self.hovered, self.anchor.contains(position) || on_toolbar || on_popup, shell);
            }
        }
        if matches!(event, Event::Window(iced::window::Event::Unfocused) | Event::Mouse(mouse::Event::CursorLeft)) {
            set_hover(self.hovered, false, shell);
            if self.popup.is_some() { shell.publish(AppMessage::CloseMessageMenu); }
        }
        if self.popup.is_some() {
            if matches!(event, Event::Keyboard(iced::keyboard::Event::KeyPressed { key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape), .. })) {
                shell.publish(AppMessage::CloseMessageMenu);
                shell.capture_event();
                return;
            }
            if !on_toolbar && !on_popup && matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_))) {
                shell.publish(AppMessage::CloseMessageMenu);
                shell.capture_event();
                return;
            }
            if !on_popup && matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. })) {
                shell.publish(AppMessage::CloseMessageMenu);
            }
        }
        if let Some(((popup, tree), popup_layout)) = self.popup.as_mut().zip(popup_layout) {
            popup.as_widget_mut().update(tree, event, popup_layout, cursor, renderer, clipboard, shell, &layout.bounds());
            if shell.is_event_captured() { return; }
        }
        if toolbar_uncovered(layout) { self.toolbar.as_widget_mut().update(self.toolbar_tree, event, toolbar, cursor, renderer, clipboard, shell, &layout.bounds()); }
        if (on_toolbar || on_popup) && matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_))) {
            shell.capture_event();
        }
    }
    fn draw(&self, renderer: &mut R, theme: &Theme, style: &renderer::Style, layout: Layout<'_>, cursor: mouse::Cursor) {
        let mut children = layout.children();
        let toolbar=children.next().unwrap();
        if toolbar_uncovered(layout) { self.toolbar.as_widget().draw(self.toolbar_tree, renderer, theme, style, toolbar, cursor, &layout.bounds()); }
        if let Some(((popup, tree), popup_layout)) = self.popup.as_ref().zip(children.next()) {
            popup.as_widget().draw(tree, renderer, theme, style, popup_layout, cursor, &layout.bounds());
        }
    }
    fn operate(&mut self, layout: Layout<'_>, renderer: &R, operation: &mut dyn Operation) {
        let mut children = layout.children();
        let toolbar=children.next().unwrap();
        if toolbar_uncovered(layout) { self.toolbar.as_widget_mut().operate(self.toolbar_tree, toolbar, renderer, operation); }
        if let Some(((popup, tree), popup_layout)) = self.popup.as_mut().zip(children.next()) {
            popup.as_widget_mut().operate(tree, popup_layout, renderer, operation);
        }
    }
    fn mouse_interaction(&self, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &R) -> mouse::Interaction {
        if *self.scroll_suppressed { return mouse::Interaction::None; }
        let mut children = layout.children();
        let toolbar = children.next().unwrap();
        if let Some(((popup, tree), popup_layout)) = self.popup.as_ref().zip(children.next()) {
            if cursor.is_over(popup_layout.bounds()) {
                return popup.as_widget().mouse_interaction(tree, popup_layout, cursor, &layout.bounds(), renderer).max(mouse::Interaction::Idle);
            }
        }
        if toolbar_uncovered(layout) && cursor.is_over(toolbar.bounds()) {
            self.toolbar.as_widget().mouse_interaction(self.toolbar_tree, toolbar, cursor, &layout.bounds(), renderer).max(mouse::Interaction::Idle)
        } else { mouse::Interaction::None }
    }
    fn overlay<'a>(&'a mut self, layout: Layout<'a>, renderer: &R) -> Option<overlay::Element<'a, AppMessage, Theme, R>> {
        let mut children = layout.children();
        let mut layers = Vec::new();
        let toolbar=children.next().unwrap();
        if toolbar_uncovered(layout) { if let Some(layer) = self.toolbar.as_widget_mut().overlay(self.toolbar_tree, toolbar, renderer, &layout.bounds(), Vector::ZERO) { layers.push(layer); } }
        if let Some(((popup, tree), popup_layout)) = self.popup.as_mut().zip(children.next()) {
            if let Some(layer) = popup.as_widget_mut().overlay(tree, popup_layout, renderer, &layout.bounds(), Vector::ZERO) { layers.push(layer); }
        }
        (!layers.is_empty()).then(|| overlay::Group::with_children(layers).overlay())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::{Space, button};

    #[test]
    fn expanded_reaction_picker_prefers_above_and_stays_inside_the_window() {
        let viewport=Rectangle {x:0.0,y:64.0,width:1440.0,height:848.0};
        let toolbar=Rectangle {x:1100.0,y:680.0,width:250.0,height:36.0};
        let popup=popup_bounds(toolbar,Size::new(384.0,448.0),viewport,true);
        assert_eq!(popup.y,228.0);
        assert!(popup.y+popup.height < toolbar.y);
        let compact=popup_bounds(toolbar,Size::new(384.0,448.0),Rectangle {width:880.0,height:508.0,..viewport},true);
        assert!(compact.x>=4.0 && compact.x+compact.width<=876.0);
        assert!(compact.y>=68.0 && compact.y+compact.height<=568.0);
    }

    const VIEWPORT: Rectangle = Rectangle { x: 0.0, y: 0.0, width: 500.0, height: 400.0 };

    fn fixture(popup: bool) -> (Actions<'static, ()>, Tree, layout::Node) {
        let mut widget = Actions {
            base: Space::new().width(180).height(50).into(),
            build_toolbar: Box::new(|| button(Space::new().width(80).height(30)).padding(0)
                .on_press(AppMessage::OpenMessageMenu("target".into())).into()),
            toolbar: None,
            popup: popup.then(|| button(Space::new().width(140).height(90)).padding(0)
                .on_press(AppMessage::CopyMessage("target".into())).into()),
            context: "chat:a:target".into(),
            outgoing: false,
            reaction_picker: false,
            enabled: true,
            force_visible: false,
        };
        let mut tree = Tree::new(&widget as &dyn Widget<AppMessage, Theme, ()>);
        let mut node = widget.layout(&mut tree, &(), &layout::Limits::new(Size::ZERO, VIEWPORT.size()));
        node.move_to_mut(Point::new(100.0, 140.0));
        (widget, tree, node)
    }

    fn update_base(widget: &mut Actions<'_, ()>, tree: &mut Tree, node: &layout::Node, position: Point) {
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        widget.update(tree, &Event::Mouse(mouse::Event::CursorMoved { position }), Layout::new(node),
            mouse::Cursor::Available(position), &(), &mut iced::advanced::clipboard::Null, &mut shell, &VIEWPORT);
        assert!(messages.is_empty(), "hover must not publish app actions");
    }

    fn update_overlay(widget: &mut Actions<'_, ()>, tree: &mut Tree, node: &layout::Node,
        position: Point, event: Event) -> (Vec<AppMessage>, bool) {
        let mut layer = widget.overlay(tree, Layout::new(node), &(), &VIEWPORT, Vector::ZERO).unwrap();
        let overlay = layer.as_overlay_mut();
        let layout = overlay.layout(&(), VIEWPORT.size());
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        overlay.update(&event, Layout::new(&layout), mouse::Cursor::Available(position), &(),
            &mut iced::advanced::clipboard::Null, &mut shell);
        let captured = shell.is_event_captured();
        (messages, captured)
    }

    #[test]
    fn a_toolbar_covered_by_the_picker_cannot_receive_invisible_clicks() {
        let (mut widget,mut tree,node)=fixture(true);
        widget.popup=Some(Space::new().width(140).height(90).into());
        widget.diff(&mut tree);
        let mut layer=widget.overlay(&mut tree,Layout::new(&node),&(),&VIEWPORT,Vector::ZERO).unwrap();
        let overlay=layer.as_overlay_mut();
        let layout=overlay.layout(&(),VIEWPORT.size());
        let children=layout.children()[0].children();
        let toolbar=children[0].clone();
        let popup=children[1].clone().move_to(toolbar.bounds().position());
        let overlap=layout::Node::with_children(VIEWPORT.size(),vec![layout::Node::with_children(VIEWPORT.size(),vec![toolbar,popup])]);
        let point=overlap.children()[0].children()[0].bounds().center();
        let mut messages=Vec::new();
        for event in [Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))] {
            let mut shell=Shell::new(&mut messages);
            overlay.update(&event,Layout::new(&overlap),mouse::Cursor::Available(point),&(),&mut iced::advanced::clipboard::Null,&mut shell);
        }
        assert!(messages.is_empty(),"a covered toolbar must not react to clicks through the picker");
    }

    #[test]
    fn wheel_over_toolbar_releases_cursor_and_does_not_reopen_on_redraw() {
        let (mut widget, mut tree, node) = fixture(false);
        update_base(&mut widget, &mut tree, &node, Point::new(140.0, 160.0));
        {
            let mut layer = widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).unwrap();
            let overlay = layer.as_overlay_mut();
            let layout = overlay.layout(&(), VIEWPORT.size());
            let position = layout.children()[0].children()[0].bounds().center();
            assert_ne!(overlay.mouse_interaction(Layout::new(&layout), mouse::Cursor::Available(position), &()), mouse::Interaction::None);
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            overlay.update(&Event::Mouse(mouse::Event::WheelScrolled { delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 } }),
                Layout::new(&layout), mouse::Cursor::Available(position), &(), &mut iced::advanced::clipboard::Null, &mut shell);
            assert!(!shell.is_event_captured());
            assert_eq!(overlay.mouse_interaction(Layout::new(&layout), mouse::Cursor::Available(position), &()), mouse::Interaction::None);
            assert!(messages.is_empty());
        }
        let position = Point::new(140.0, 160.0);
        let mut messages = Vec::new();
        for _ in 0..3 {
            let mut shell = Shell::new(&mut messages);
            widget.update(&mut tree, &Event::Window(iced::window::Event::RedrawRequested(std::time::Instant::now())),
                Layout::new(&node), mouse::Cursor::Available(position), &(), &mut iced::advanced::clipboard::Null, &mut shell, &VIEWPORT);
            assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_none());
        }
        update_base(&mut widget, &mut tree, &node, position);
        assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_some());
    }

    #[test]
    fn stationary_cursor_during_layout_does_not_open_actions() {
        let (mut widget, mut tree, node) = fixture(false);
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        widget.update(&mut tree, &Event::Window(iced::window::Event::RedrawRequested(std::time::Instant::now())),
            Layout::new(&node), mouse::Cursor::Available(Point::new(140.0, 160.0)), &(),
            &mut iced::advanced::clipboard::Null, &mut shell, &VIEWPORT);
        assert!(!tree.state.downcast_ref::<State>().hovered);
        assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_none());
    }

    #[test]
    fn toolbar_is_constructed_only_when_visible() {
        let (mut widget, mut tree, node) = fixture(false);
        let built = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = built.clone();
        widget.build_toolbar = Box::new(move || {
            counter.set(counter.get() + 1);
            Space::new().width(80).height(30).into()
        });
        widget.diff(&mut tree);
        assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_none());
        assert_eq!(built.get(), 0);
        update_base(&mut widget, &mut tree, &node, Point::new(140.0, 160.0));
        for _ in 0..2 {
            assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_some());
        }
        assert_eq!(built.get(), 1);
        update_base(&mut widget, &mut tree, &node, Point::new(480.0, 350.0));
        assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_none());
        assert!(widget.toolbar.is_none());
    }

    #[test]
    fn hover_shows_actions_and_leaving_hides_them_without_reflow() {
        let (mut widget, mut tree, node) = fixture(false);
        assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_none());
        update_base(&mut widget, &mut tree, &node, Point::new(140.0, 160.0));
        assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_some());
        let hovered_size = widget.layout(&mut tree, &(), &layout::Limits::new(Size::ZERO, VIEWPORT.size())).size();
        assert_eq!(node.size(), hovered_size);
        update_overlay(&mut widget, &mut tree, &node, Point::new(480.0, 350.0),
            Event::Mouse(mouse::Event::CursorMoved { position: Point::new(480.0, 350.0) }));
        assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_none());
    }

    #[test]
    fn moving_into_toolbar_preserves_hover_and_button_targets_its_message() {
        let (mut widget, mut tree, node) = fixture(false);
        update_base(&mut widget, &mut tree, &node, Point::new(140.0, 160.0));
        let position = Point::new(240.0, 125.0);
        update_overlay(&mut widget, &mut tree, &node, position, Event::Mouse(mouse::Event::CursorMoved { position }));
        assert!(tree.state.downcast_ref::<State>().hovered);
        let (_, captured) = update_overlay(&mut widget, &mut tree, &node, position,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)));
        assert!(captured);
        let (messages, _) = update_overlay(&mut widget, &mut tree, &node, position,
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)));
        assert!(matches!(messages.as_slice(), [AppMessage::OpenMessageMenu(id)] if id == "target"));
    }

    #[test]
    fn a_press_cancelled_by_leaving_does_not_activate_after_hover_returns() {
        let (mut widget, mut tree, node) = fixture(false);
        update_base(&mut widget, &mut tree, &node, Point::new(140.0, 160.0));
        let position = Point::new(240.0, 125.0);
        update_overlay(&mut widget, &mut tree, &node, position,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)));
        let outside = Point::new(480.0, 350.0);
        update_overlay(&mut widget, &mut tree, &node, outside,
            Event::Mouse(mouse::Event::CursorMoved { position: outside }));
        assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_none());
        update_base(&mut widget, &mut tree, &node, Point::new(140.0, 160.0));
        let (messages, _) = update_overlay(&mut widget, &mut tree, &node, position,
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)));
        assert!(messages.is_empty());
    }

    #[test]
    fn popup_keeps_actions_visible_without_hover_and_does_not_resize_message() {
        let (mut widget, mut tree, node) = fixture(true);
        assert!(!tree.state.downcast_ref::<State>().hovered);
        let mut layer = widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).unwrap();
        let layout = layer.as_overlay_mut().layout(&(), VIEWPORT.size());
        assert_eq!(layout.children()[0].children().len(), 2);
        drop(layer);
        assert_eq!(node.size(), widget.layout(&mut tree, &(), &layout::Limits::new(Size::ZERO, VIEWPORT.size())).size());
    }

    #[test]
    fn popup_click_does_not_fall_through_to_message_and_outside_click_closes() {
        let (mut widget, mut tree, node) = fixture(true);
        let position = Point::new(210.0, 180.0);
        update_overlay(&mut widget, &mut tree, &node, position,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)));
        let (messages, captured) = update_overlay(&mut widget, &mut tree, &node, position,
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)));
        assert!(captured);
        assert!(matches!(messages.as_slice(), [AppMessage::CopyMessage(id)] if id == "target"));
        let (messages, captured) = update_overlay(&mut widget, &mut tree, &node, Point::new(450.0, 300.0),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)));
        assert!(captured);
        assert!(matches!(messages.as_slice(), [AppMessage::CloseMessageMenu]));
    }

    #[test]
    fn escape_and_window_unfocus_close_popup() {
        use iced::keyboard::{Event as KeyEvent, Key, Location, Modifiers, key::{Named, Physical}};
        let (mut widget, mut tree, node) = fixture(true);
        let escape = Event::Keyboard(KeyEvent::KeyPressed {
            key: Key::Named(Named::Escape), modified_key: Key::Named(Named::Escape),
            physical_key: Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified),
            location: Location::Standard, modifiers: Modifiers::empty(), text: None, repeat: false,
        });
        let (messages, captured) = update_overlay(&mut widget, &mut tree, &node, Point::new(240.0, 125.0), escape);
        assert!(captured);
        assert!(matches!(messages.as_slice(), [AppMessage::CloseMessageMenu]));
        let (messages, _) = update_overlay(&mut widget, &mut tree, &node, Point::new(240.0, 125.0),
            Event::Window(iced::window::Event::Unfocused));
        assert!(matches!(messages.as_slice(), [AppMessage::CloseMessageMenu]));
        assert!(!tree.state.downcast_ref::<State>().hovered);
    }

    #[test]
    fn scroll_translation_anchors_visible_messages_and_omits_offscreen_ones() {
        let (mut widget, mut tree, mut node) = fixture(true);
        node.move_to_mut(Point::new(100.0, 900.0));
        let mut layer = widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::new(0.0, -700.0)).unwrap();
        let layout = layer.as_overlay_mut().layout(&(), VIEWPORT.size());
        assert_eq!(layout.children()[0].children()[0].bounds().y, 176.0);
        drop(layer);
        assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_none());
    }

    #[test]
    fn recycled_message_state_does_not_leak_hover_between_conversations() {
        let (mut widget, mut tree, node) = fixture(false);
        update_base(&mut widget, &mut tree, &node, Point::new(140.0, 160.0));
        widget.context = "chat:b:other-message".into();
        widget.diff(&mut tree);
        assert!(!tree.state.downcast_ref::<State>().hovered);
    }

    #[test]
    fn another_menu_or_modal_suppresses_hover_without_replacing_the_base_widget() {
        let (mut widget, mut tree, node) = fixture(false);
        update_base(&mut widget, &mut tree, &node, Point::new(140.0, 160.0));
        widget.enabled = false;
        widget.diff(&mut tree);
        update_base(&mut widget, &mut tree, &node, Point::new(140.0, 160.0));
        assert!(widget.overlay(&mut tree, Layout::new(&node), &(), &VIEWPORT, Vector::ZERO).is_none());
        assert_eq!(tree.children.len(), 2);
        assert_eq!(node.size(), widget.layout(&mut tree, &(), &layout::Limits::new(Size::ZERO, VIEWPORT.size())).size());
    }

    #[test]
    fn floating_controls_stay_inside_viewport_and_popup_flips_above_bottom() {
        let viewport = Rectangle { x: 300.0, y: 80.0, width: 260.0, height: 400.0 };
        for anchor in [Rectangle::new(Point::new(305.0, 85.0), Size::new(40.0, 50.0)),
            Rectangle::new(Point::new(505.0, 440.0), Size::new(50.0, 30.0))] {
            for outgoing in [false, true] {
                let toolbar = toolbar_bounds(anchor, Size::new(228.0, 36.0), viewport, outgoing);
                assert!(toolbar.is_within(&viewport));
                let popup = popup_bounds(toolbar, Size::new(240.0, 190.0), viewport, false);
                assert!(popup.is_within(&viewport));
                if anchor.y > 400.0 { assert!(popup.y < toolbar.y); }
            }
        }
    }
}
