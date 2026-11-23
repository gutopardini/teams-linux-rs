use crate::AppMessage;
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer, widget};
use iced::alignment::Vertical;
use iced::widget::Row;
use iced::{Element, Event, Length, Rectangle, Renderer, Size, Theme, Vector};

pub(super) fn view(
    cells: Row<'static, AppMessage>,
    alignments: Vec<Vertical>,
) -> Element<'static, AppMessage> {
    Element::new(MeasuredRow { inner: cells.into(), alignments })
}

struct MeasuredRow {
    inner: Element<'static, AppMessage>,
    alignments: Vec<Vertical>,
}

impl Widget<AppMessage, Theme, Renderer> for MeasuredRow {
    fn children(&self) -> Vec<widget::Tree> {
        self.inner.as_widget().children()
    }

    fn diff(&self, tree: &mut widget::Tree) {
        self.inner.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.inner.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // Let the row wrap each cell at the width it will actually receive.
        // Then stretch each cell's frame to the tallest cell's measured height.
        let row = self.inner.as_widget_mut().layout(tree, renderer, limits);
        let height = row.size().height.max(16.0).min(limits.max().height);
        let cells = row.children().iter().zip(&self.alignments).map(|(cell, align)| {
            let extra = (height - cell.size().height).max(0.0);
            let offset = match align {
                Vertical::Top => 0.0,
                Vertical::Center => extra / 2.0,
                Vertical::Bottom => extra,
            };
            let content = cell.children()[0].clone().translate(Vector::new(0.0, offset));
            layout::Node::with_children(Size::new(cell.size().width, height), vec![content])
                .move_to(cell.bounds().position())
        }).collect();
        layout::Node::with_children(Size::new(row.size().width, height), cells)
    }

    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.inner.as_widget_mut().operate(tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, AppMessage>,
        viewport: &Rectangle,
    ) {
        self.inner.as_widget_mut().update(tree, event, layout, cursor, renderer, clipboard, shell, viewport);
    }

    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.inner.as_widget().draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.inner.as_widget().mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut widget::Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, AppMessage, Theme, Renderer>> {
        self.inner.as_widget_mut().overlay(tree, layout, renderer, viewport, translation)
    }
}
