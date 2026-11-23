//! Composer menus are overlays: opening them never moves the draft or history.
use crate::AppMessage;
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer, widget::{Operation, Tree}};
use iced::{Element, Event, Length, Point, Rectangle, Size, Theme, Vector};

pub fn view<'a>(base: impl Into<Element<'a, AppMessage>>, popup: Option<Element<'a, AppMessage>>) -> Element<'a, AppMessage> {
    view_with_close(base,popup,false,AppMessage::CloseComposerMenu)
}

pub fn view_above_right<'a>(base: impl Into<Element<'a, AppMessage>>, popup: Option<Element<'a, AppMessage>>) -> Element<'a, AppMessage> {
    Element::new(Popover {
        base: base.into(),
        popup,
        placement: Placement::AboveRight,
        close: AppMessage::CloseComposerMenu,
    })
}

pub fn view_with_close<'a>(base: impl Into<Element<'a,AppMessage>>, popup: Option<Element<'a,AppMessage>>, align_right: bool, close: AppMessage) -> Element<'a,AppMessage> {
    let placement = if align_right { Placement::Right } else { Placement::Center };
    Element::new(Popover {base:base.into(),popup,placement,close})
}

#[derive(Clone, Copy)]
enum Placement {
    Center,
    Right,
    AboveRight,
}

struct Popover<'a> {
    base: Element<'a, AppMessage>,
    popup: Option<Element<'a, AppMessage>>,
    placement: Placement,
    close: AppMessage,
}

const VIEWPORT_MARGIN: f32 = 8.0;
const ANCHOR_GAP: f32 = 6.0;

fn position(anchor: Rectangle, size: Size, viewport: Rectangle, align_right: bool) -> Point {
    let preferred = if align_right { anchor.x + anchor.width - size.width }
        else { anchor.x + anchor.width / 2.0 - size.width / 2.0 };
    let left = viewport.x + VIEWPORT_MARGIN;
    let top = viewport.y + VIEWPORT_MARGIN;
    let x = preferred.clamp(left, (viewport.x + viewport.width - size.width - VIEWPORT_MARGIN).max(left));
    let above = anchor.y - size.height - ANCHOR_GAP;
    let y = if above >= top { above } else { anchor.y + anchor.height + ANCHOR_GAP };
    Point::new(x, y.clamp(top, (viewport.y + viewport.height - size.height - VIEWPORT_MARGIN).max(top)))
}

impl Widget<AppMessage, Theme, iced::Renderer> for Popover<'_> {
    fn size(&self) -> Size<Length> { self.base.as_widget().size() }
    fn children(&self) -> Vec<Tree> {
        let mut children = vec![Tree::new(&self.base)];
        if let Some(popup) = &self.popup { children.push(Tree::new(popup)); }
        children
    }
    fn diff(&self, tree: &mut Tree) {
        let mut children = vec![&self.base];
        if let Some(popup) = &self.popup { children.push(popup); }
        tree.diff_children(&children);
    }
    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        self.base.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor,
        renderer: &iced::Renderer, clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, AppMessage>, viewport: &Rectangle) {
        self.base.as_widget_mut().update(&mut tree.children[0], event, layout, cursor, renderer, clipboard, shell, viewport);
    }
    fn draw(&self, tree: &Tree, renderer: &mut iced::Renderer, theme: &Theme, style: &renderer::Style,
        layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle) {
        self.base.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &iced::Renderer, operation: &mut dyn Operation) {
        self.base.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle, renderer: &iced::Renderer) -> mouse::Interaction {
        self.base.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }
    fn overlay<'a>(&'a mut self, tree: &'a mut Tree, layout: Layout<'a>, renderer: &iced::Renderer,
        viewport: &Rectangle, translation: Vector) -> Option<overlay::Element<'a, AppMessage, Theme, iced::Renderer>> {
        if let Some(popup) = self.popup.as_mut() {
            Some(overlay::Element::new(Box::new(Menu {
                popup, tree: &mut tree.children[1], anchor: layout.bounds() + translation, viewport: *viewport, placement: self.placement, close:self.close.clone(),
            })))
        } else {
            self.base.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
        }
    }
}

struct Menu<'a, 'b> {
    popup: &'a mut Element<'b, AppMessage>,
    tree: &'a mut Tree,
    anchor: Rectangle,
    viewport: Rectangle,
    placement: Placement,
    close: AppMessage,
}

impl overlay::Overlay<AppMessage, Theme, iced::Renderer> for Menu<'_, '_> {
    fn layout(&mut self, renderer: &iced::Renderer, bounds: Size) -> layout::Node {
        let mut height = bounds.height - 2.0 * VIEWPORT_MARGIN;
        if matches!(self.placement, Placement::AboveRight) {
            height = height.min(self.anchor.y - self.viewport.y - VIEWPORT_MARGIN - ANCHOR_GAP);
        }
        let available = Size::new(
            (bounds.width - 2.0 * VIEWPORT_MARGIN).max(1.0),
            height.max(1.0),
        );
        let mut node = self.popup.as_widget_mut().layout(self.tree, renderer, &layout::Limits::new(Size::ZERO, available));
        let mut at=position(self.anchor,node.size(),self.viewport,!matches!(self.placement, Placement::Center));
        if matches!(self.close,AppMessage::CloseMentions){at.x=(self.anchor.x+16.).clamp(self.viewport.x+8.,(self.viewport.x+self.viewport.width-node.size().width-8.).max(self.viewport.x+8.));}
        node.move_to_mut(at);
        layout::Node::with_children(bounds, vec![node])
    }
    fn update(&mut self, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, AppMessage>) {
        let content = layout.children().next().unwrap();
        if matches!(self.close,AppMessage::CloseMentions) {
            if let Event::Keyboard(iced::keyboard::Event::KeyPressed{key,modifiers,..})=event {
                if modifiers.is_empty() {
                    use iced::keyboard::{Key,key::Named};
                    let action=match key {Key::Named(Named::ArrowUp)=>Some(AppMessage::MentionNavigate(-1)),Key::Named(Named::ArrowDown)=>Some(AppMessage::MentionNavigate(1)),Key::Named(Named::Enter|Named::Tab)=>Some(AppMessage::MentionAccept),_=>None};
                    if let Some(action)=action {shell.publish(action);shell.capture_event();return;}
                }
            }
        }
        let outside_press = !cursor.is_over(content.bounds()) && matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_)));
        let escape = matches!(event, Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape), .. }));
        if outside_press || escape {
            shell.publish(self.close.clone());
            shell.capture_event();
            return;
        }
        self.popup.as_widget_mut().update(self.tree, event, content, cursor, renderer, clipboard, shell, &self.viewport);
        if cursor.is_over(content.bounds()) && matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_) | mouse::Event::WheelScrolled { .. })) {
            shell.capture_event();
        }
    }
    fn draw(&self, renderer: &mut iced::Renderer, theme: &Theme, style: &renderer::Style, layout: Layout<'_>, cursor: mouse::Cursor) {
        self.popup.as_widget().draw(self.tree, renderer, theme, style, layout.children().next().unwrap(), cursor, &self.viewport);
    }
    fn operate(&mut self, layout: Layout<'_>, renderer: &iced::Renderer, operation: &mut dyn Operation) {
        self.popup.as_widget_mut().operate(self.tree, layout.children().next().unwrap(), renderer, operation);
    }
    fn mouse_interaction(&self, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &iced::Renderer) -> mouse::Interaction {
        self.popup.as_widget().mouse_interaction(self.tree, layout.children().next().unwrap(), cursor, &self.viewport, renderer)
    }
    fn overlay<'a>(&'a mut self, layout: Layout<'a>, renderer: &iced::Renderer) -> Option<overlay::Element<'a, AppMessage, Theme, iced::Renderer>> {
        self.popup.as_widget_mut().overlay(self.tree, layout.children().next().unwrap(), renderer, &self.viewport, Vector::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menus_prefer_above_the_composer_and_stay_inside_small_windows() {
        let viewport = Rectangle::with_size(Size::new(880.0, 580.0));
        let size = Size::new(300.0, 230.0);
        let at_bottom = position(Rectangle { x: 810.0, y: 480.0, width: 32.0, height: 32.0 }, size, viewport, false);
        assert_eq!(at_bottom, Point::new(572.0, 244.0));
        let at_top = position(Rectangle { x: 0.0, y: 0.0, width: 32.0, height: 32.0 }, size, viewport, false);
        assert_eq!(at_top, Point::new(8.0, 38.0));
        let anchor = Rectangle { x: 400.0, y: 480.0, width: 32.0, height: 32.0 };
        assert_eq!(position(anchor, size, viewport, false).x, 266.0);
        assert_eq!(position(anchor, size, viewport, true).x, 132.0);
    }
}
