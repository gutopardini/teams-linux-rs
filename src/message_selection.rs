//! Drag selects part of a message. A second click on the same spot selects it all.
use crate::AppMessage;
use iced::advanced::text::{self, Paragraph};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, renderer};
use iced::widget::text::{Alignment, LineHeight, Shaping, Span, Wrapping};
use iced::widget::rich_text;
use iced::{Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Size, Theme};
use std::cell::RefCell;
use std::time::Instant;

#[derive(Clone,Debug,PartialEq,Eq,Hash)]
pub enum Link {
    Web(String),
    Mention { message_id:String, person_id:String },
}
impl Link {
    pub fn action(self)->AppMessage {
        match self {Self::Web(url)=>AppMessage::OpenMessageLink(url),Self::Mention{message_id,person_id}=>AppMessage::OpenMentionCard(message_id,person_id)}
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextPoint {
    pub block: usize,
    pub offset: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageTextSelection {
    pub message_id: String,
    pub anchor: TextPoint,
    pub caret: TextPoint,
    pub dragging: bool,
}

#[derive(Clone, Copy)]
pub struct Interaction<'a> {
    pub message_id: &'a str,
    pub selection: Option<&'a MessageTextSelection>,
}

impl<'a> Interaction<'a> {
    pub fn range_in_block(self, block: usize, text: &str) -> Option<(usize, usize)> {
        let selection = self
            .selection
            .filter(|item| item.message_id == self.message_id)?;
        block_highlight(selection.anchor.clone(), selection.caret.clone(), block, text)
    }

    pub fn is_dragging(self) -> bool {
        self.selection
            .is_some_and(|item| item.dragging && item.message_id == self.message_id)
    }

    pub fn caret_in_block(self, block: usize) -> Option<usize> {
        self.selection
            .filter(|item| item.dragging && item.message_id == self.message_id && item.caret.block == block)
            .map(|item| item.caret.offset)
    }
}

pub fn snap_offset(text: &str, offset: usize) -> usize {
    if offset >= text.len() {
        return text.len();
    }
    let mut index = offset;
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub fn clicks_are_double(
    previous: Option<(&str, f32, f32, Instant)>,
    message_id: &str,
    x: f32,
    y: f32,
    now: Instant,
) -> bool {
    let Some((id, px, py, time)) = previous else {
        return false;
    };
    if id != message_id {
        return false;
    }
    let elapsed = now.saturating_duration_since(time);
    let dx = px - x;
    let dy = py - y;
    elapsed.as_millis() <= 300 && (dx * dx + dy * dy) < 36.0
}

pub fn select_entire(lengths: &[usize]) -> Option<(TextPoint, TextPoint)> {
    let start = lengths.iter().position(|len| *len > 0)?;
    let end = lengths.iter().rposition(|len| *len > 0)?;
    Some((
        TextPoint { block: start, offset: 0 },
        TextPoint {
            block: end,
            offset: lengths[end],
        },
    ))
}

pub fn selected_text(blocks: &[String], anchor: TextPoint, caret: TextPoint) -> String {
    let (start, end) = ordered(anchor, caret);
    if start.block == end.block && start.offset == end.offset {
        return String::new();
    }
    let mut parts = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if index < start.block || index > end.block || block.is_empty() {
            continue;
        }
        let from = if index == start.block {
            snap_offset(block, start.offset)
        } else {
            0
        };
        let to = if index == end.block {
            snap_offset(block, end.offset)
        } else {
            block.len()
        };
        if from < to {
            parts.push(&block[from..to]);
        }
    }
    parts.join("\n")
}

pub fn block_highlight(anchor: TextPoint, caret: TextPoint, block: usize, text: &str) -> Option<(usize, usize)> {
    let (start, end) = ordered(anchor, caret);
    if start.block == end.block && start.offset == end.offset {
        return None;
    }
    if block < start.block || block > end.block {
        return None;
    }
    let from = if block == start.block {
        snap_offset(text, start.offset)
    } else {
        0
    };
    let to = if block == end.block {
        snap_offset(text, end.offset)
    } else {
        text.len()
    };
    (from < to).then_some((from, to))
}

pub fn split_selected(text: &str, range: Option<(usize, usize)>) -> Vec<(std::ops::Range<usize>, bool)> {
    let Some((start, end)) = range else {
        return vec![(0..text.len(), false)];
    };
    let start = snap_offset(text, start).min(text.len());
    let end = snap_offset(text, end.max(start)).min(text.len());
    if start >= end {
        return vec![(0..text.len(), false)];
    }
    let mut pieces = Vec::new();
    if start > 0 {
        pieces.push((0..start, false));
    }
    pieces.push((start..end, true));
    if end < text.len() {
        pieces.push((end..text.len(), false));
    }
    pieces
}

pub fn selection_color() -> Color {
    Color::from_rgba(0.31, 0.47, 0.96, 0.45)
}

fn ordered(anchor: TextPoint, caret: TextPoint) -> (TextPoint, TextPoint) {
    if anchor.block < caret.block || (anchor.block == caret.block && anchor.offset <= caret.offset) {
        (anchor, caret)
    } else {
        (caret, anchor)
    }
}

pub fn selectable<'a>(
    spans: Vec<Span<'a, Link, Font>>,
    visible: String,
    message_id: String,
    block: usize,
    dragging: bool,
    caret: Option<usize>,
    size: f32,
    line_height: f32,
) -> Element<'a, AppMessage> {
    let inner = rich_text(spans.clone())
        .size(size)
        .line_height(line_height)
        .wrapping(Wrapping::WordOrGlyph);
    Element::new(Selectable {
        inner: inner.into(),
        spans,
        visible,
        message_id,
        block,
        dragging,
        caret,
        size,
        line_height,
    })
}

struct Selectable<'a, R: renderer::Renderer = iced::Renderer> {
    inner: Element<'a, AppMessage, Theme, R>,
    spans: Vec<Span<'a, Link, Font>>,
    visible: String,
    message_id: String,
    block: usize,
    dragging: bool,
    caret: Option<usize>,
    size: f32,
    line_height: f32,
}

struct State<P: Paragraph> {
    hit_test: RefCell<Option<HitTest<P>>>,
    bounds: Size,
    pressed: bool,
}

#[derive(Default)]
struct HitTest<P: Paragraph> {
    spans: Vec<Span<'static, Link, P::Font>>,
    paragraph: P,
}

impl<R: text::Renderer<Font = Font>> Selectable<'_, R> {
    fn cursor_offset(&self, state: &State<R::Paragraph>, local: Point, renderer: &R) -> usize {
        let mut hit_test = state.hit_test.borrow_mut();
        let hit_test = hit_test.get_or_insert_with(HitTest::default);
        sync_paragraph(hit_test, renderer, state.bounds, &self.spans, self.size, self.line_height);
        let offset = hit_test.paragraph.hit_test(local).map(|hit| hit.cursor()).unwrap_or_else(|| {
            if local.y > hit_test.paragraph.min_height() / 2.0 { self.visible.len() } else { 0 }
        });
        snap_offset(&self.visible, offset)
    }
}

impl<R> Widget<AppMessage, Theme, R> for Selectable<'_, R>
where
    R: renderer::Renderer + text::Renderer<Font = Font>,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<R::Paragraph>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<R::Paragraph> {
            hit_test: RefCell::new(None),
            bounds: Size::ZERO,
            pressed: false,
        })
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.inner)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.inner));
    }

    fn size(&self) -> Size<Length> {
        self.inner.as_widget().size()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        tree.state.downcast_mut::<State<R::Paragraph>>().bounds = limits.max();
        self.inner
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &R,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, AppMessage>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<State<R::Paragraph>>();
        let local = cursor.position_in(bounds);
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) if local.is_some() => {
                let offset = self.cursor_offset(state, local.unwrap(), renderer);
                let link = link_at(&self.spans, offset);
                let position = cursor.position().unwrap_or(Point::ORIGIN);
                state.pressed = true;
                shell.publish(AppMessage::MessageTextPressed(
                    self.message_id.clone(),
                    self.block,
                    offset,
                    position.x,
                    position.y,
                    link,
                ));
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. })
                if (self.dragging || state.pressed) && local.is_some() =>
            {
                let offset = self.cursor_offset(state, local.unwrap(), renderer);
                if self.caret != Some(offset) {
                    shell.publish(AppMessage::MessageTextDragged(
                        self.message_id.clone(),
                        self.block,
                        offset,
                    ));
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if self.dragging || state.pressed =>
            {
                state.pressed = false;
                shell.publish(AppMessage::MessageTextReleased);
                shell.capture_event();
            }
            _ => {}
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
        _viewport: &Rectangle,
        renderer: &R,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State<R::Paragraph>>();
        let Some(local) = cursor.position_in(layout.bounds()) else { return mouse::Interaction::None; };
        if self.spans.iter().any(|span| span.link.is_some())
            && link_at(&self.spans, self.cursor_offset(state, local, renderer)).is_some()
        { mouse::Interaction::Pointer } else { mouse::Interaction::Text }
    }
}

fn link_at(spans: &[Span<'_, Link, Font>], offset: usize) -> Option<Link> {
    let mut cursor = 0;
    for span in spans {
        let end = cursor + span.text.len();
        if offset < end {
            return span.link.clone();
        }
        cursor = end;
    }
    None
}

fn sync_paragraph<R>(
    state: &mut HitTest<R::Paragraph>,
    renderer: &R,
    bounds: Size,
    spans: &[Span<'_, Link, Font>],
    size: f32,
    line_height: f32,
) where
    R: text::Renderer<Font = Font>,
{
    let size = Pixels(size);
    let line_height = LineHeight::Relative(line_height);
    let font = renderer.default_font();
    let text_with_spans = || text::Text {
        content: spans,
        bounds,
        size,
        line_height,
        font,
        align_x: Alignment::Default,
        align_y: iced::alignment::Vertical::Top,
        shaping: Shaping::Advanced,
        wrapping: Wrapping::WordOrGlyph,
    };
    if state.spans != spans {
        state.paragraph = R::Paragraph::with_spans(text_with_spans());
        state.spans = spans.iter().cloned().map(Span::to_static).collect();
    } else {
        match state.paragraph.compare(text::Text {
            content: (),
            bounds,
            size,
            line_height,
            font,
            align_x: Alignment::Default,
            align_y: iced::alignment::Vertical::Top,
            shaping: Shaping::Advanced,
            wrapping: Wrapping::WordOrGlyph,
        }) {
            text::Difference::None => {}
            text::Difference::Bounds => state.paragraph.resize(bounds),
            text::Difference::Shape => state.paragraph = R::Paragraph::with_spans(text_with_spans()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_click_resolves_a_link_after_layout_and_wrap_changes() {
        type TextState = State<<iced::Renderer as text::Renderer>::Paragraph>;
        let value = "Open the original message";
        let link = Link::Web("https://example.test".into());
        let spans = vec![Span::new(value).link(link.clone())];
        let mut widget = selectable(spans, value.into(), "msg".into(), 0, false, None, 16.0, 1.35);
        let renderer = crate::chat_render_tests::headless_renderer();
        let mut tree = Tree::new(widget.as_widget());
        let wide = layout::Limits::new(Size::ZERO, Size::new(300.0, 300.0));
        let narrow = layout::Limits::new(Size::ZERO, Size::new(82.0, 300.0));
        let first = widget.as_widget_mut().layout(&mut tree, &renderer, &wide);
        assert!(tree.state.downcast_ref::<TextState>().hit_test.borrow().is_none());
        assert_eq!(widget.as_widget().mouse_interaction(
            &tree, Layout::new(&first), mouse::Cursor::Available(Point::new(4.0, 8.0)),
            &Rectangle::with_size(wide.max()), &renderer,
        ), mouse::Interaction::Pointer);

        let wrapped = widget.as_widget_mut().layout(&mut tree, &renderer, &narrow);
        assert!(wrapped.size().height > first.size().height);
        let cursor = mouse::Cursor::Available(Point::new(4.0, wrapped.size().height - 10.0));
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        widget.as_widget_mut().update(
            &mut tree, &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&wrapped), cursor, &renderer, &mut iced::advanced::clipboard::Null,
            &mut shell, &Rectangle::with_size(narrow.max()),
        );
        assert!(shell.is_event_captured());
        assert!(matches!(messages.as_slice(),
            [AppMessage::MessageTextPressed(id, 0, offset, _, _, Some(found))]
                if id == "msg" && *offset > 0 && *found == link));
    }

    #[test]
    fn drag_keeps_the_marked_span_and_double_click_selects_every_block() {
        let blocks = vec!["um dois".to_owned(), String::new(), "tres quatro".to_owned()];
        let partial = selected_text(
            &blocks,
            TextPoint { block: 0, offset: 3 },
            TextPoint { block: 2, offset: 4 },
        );
        assert_eq!(partial, "dois\ntres");
        let (anchor, caret) = select_entire(&[blocks[0].len(), 0, blocks[2].len()]).unwrap();
        assert_eq!(selected_text(&blocks, anchor, caret), "um dois\ntres quatro");
        assert_eq!(
            block_highlight(
                TextPoint { block: 0, offset: 3 },
                TextPoint { block: 0, offset: 7 },
                0,
                "um dois",
            ),
            Some((3, 7))
        );
        assert!(block_highlight(
            TextPoint { block: 0, offset: 2 },
            TextPoint { block: 0, offset: 2 },
            0,
            "um dois",
        ).is_none());
    }

    #[test]
    fn second_click_is_double_only_on_the_same_message_and_spot() {
        let now = Instant::now();
        let previous = Some(("m", 10.0, 12.0, now));
        assert!(clicks_are_double(previous, "m", 12.0, 13.0, now));
        assert!(!clicks_are_double(previous, "outra", 12.0, 13.0, now));
        assert!(!clicks_are_double(previous, "m", 40.0, 12.0, now));
        assert_eq!(snap_offset("áé", 1), 0);
        assert_eq!(split_selected("abcdef", Some((2, 5))), vec![(0..2, false), (2..5, true), (5..6, false)]);
    }
}
