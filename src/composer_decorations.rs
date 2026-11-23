//! Underline/strike geometry follows the native editor's own shaping engine.
//! Input, IME, selection and clipboard remain owned by Iced's TextEditor.
use crate::{AppMessage, message_body::{self, ComposerFormatting, ComposerHighlighter, ComposerHighlightSettings, InlineMark}};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer, widget::{Operation, Tree}};
use iced::advanced::text::{Editor as _, Highlighter as _};
use iced::advanced::graphics::text::Editor;
use iced::{Element, Event, Font, Length, Padding, Pixels, Rectangle, Size, Theme, Vector};
use iced::widget::{text, text_editor};
use std::{cell::RefCell, ops::Deref};
use unicode_segmentation::UnicodeSegmentation;

/// The optional geometry editor receives exactly the same actions as the input
/// editor, including scrolling. It is allocated only when rich editing is shown.
pub struct Content {
    native: text_editor::Content,
    geometry: RefCell<Option<Geometry>>,
    revision: u64,
}

impl Content {
    pub fn new() -> Self { Self::with_text("") }
    pub fn with_text(value: &str) -> Self {
        Self { native: text_editor::Content::with_text(value), geometry: RefCell::new(None), revision: 0 }
    }
    pub fn perform(&mut self, action: text_editor::Action) {
        if let Some(geometry) = self.geometry.get_mut() { geometry.editor.perform(action.clone()); }
        self.native.perform(action);
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn move_to(&mut self, cursor: iced::advanced::text::editor::Cursor) {
        if let Some(geometry) = self.geometry.get_mut() { geometry.editor.move_to(cursor); }
        self.native.move_to(cursor);
        self.revision = self.revision.wrapping_add(1);
    }
}

impl Deref for Content {
    type Target = text_editor::Content;
    fn deref(&self) -> &Self::Target { &self.native }
}

struct Geometry {
    editor: Editor,
    highlighter: ComposerHighlighter,
    settings: ComposerHighlightSettings,
    bounds: Size,
    font: Font,
    size: f32,
    painted_revision: Option<u64>,
    font_version: Option<iced::advanced::graphics::text::Version>,
    strokes: Vec<Stroke>,
    backgrounds: Vec<Stroke>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Stroke { bounds: Rectangle, color: Ink }

#[derive(Clone, Copy, Debug, PartialEq)]
enum Ink { Text, Link, Color(u32), Background(u32) }

fn add_stroke(output: &mut Vec<Stroke>, bounds: Rectangle, color: Ink) {
    if let Some(last) = output.last_mut().filter(|last| last.color == color
        && (last.bounds.y - bounds.y).abs() < 0.01
        && bounds.x <= last.bounds.x + last.bounds.width + 0.5
        && last.bounds.x <= bounds.x + bounds.width + 0.5)
    {
        let right = (last.bounds.x + last.bounds.width).max(bounds.x + bounds.width);
        last.bounds.x = last.bounds.x.min(bounds.x);
        last.bounds.width = right - last.bounds.x;
    } else { output.push(Stroke { bounds, color }); }
}

fn flags(formatting: &ComposerFormatting, start: usize, length: usize) -> Vec<u8> {
    let mut output = vec![0; length];
    for range in &formatting.marks {
        let bit = match range.mark { InlineMark::Underline => 1, InlineMark::Strike => 2, _ => continue };
        let from = range.start.saturating_sub(start).min(length);
        let to = range.end.saturating_sub(start).min(length);
        for flag in &mut output[from..to.max(from)] { *flag |= bit; }
    }
    output
}

impl Geometry {
    fn new(content: &Content, settings: &ComposerHighlightSettings) -> Self {
        let mut editor = Editor::with_text(&content.text());
        editor.move_to(content.cursor());
        Self { editor, highlighter: ComposerHighlighter::new(settings), settings: settings.clone(),
            bounds: Size::ZERO, font: Font::default(), size: 0.0, painted_revision: None, font_version: None,
            strokes: Vec::new(), backgrounds: Vec::new() }
    }

    fn update(&mut self, settings: &ComposerHighlightSettings, bounds: Size, font: Font, size: f32) {
        let font_version = iced::advanced::graphics::text::font_system().read().expect("font system").version();
        if self.settings != *settings || self.bounds != bounds || self.font != font || self.size != size || self.font_version != Some(font_version) {
            self.painted_revision = None;
        }
        self.font_version = Some(font_version);
        self.highlighter.update(settings);
        self.settings = settings.clone();
        self.bounds = bounds;
        self.font = font;
        self.size = size;
        self.editor.update(bounds, font, Pixels(size), text::LineHeight::default(), text::Wrapping::Word, &mut self.highlighter);
    }

    fn paint(&mut self, revision: u64, theme: &Theme) {
        if self.painted_revision == Some(revision) { return; }
        self.editor.highlight(self.font, &mut self.highlighter,
            |highlight| message_body::composer_highlight_format(highlight, theme));
        self.backgrounds.clear();
        let buffer = self.editor.buffer();
        let mut line = None;
        let mut boundaries = Vec::new();
        for run in buffer.layout_runs() {
            let value = buffer.lines[run.line_i].text();
            let start = self.settings.line_starts.get(run.line_i).copied().unwrap_or(0);
            if line != Some(run.line_i) {
                boundaries.clear();
                boundaries.extend(value.grapheme_indices(true).map(|(byte, _)| byte));
                boundaries.push(value.len());
                line = Some(run.line_i);
            }
            for glyph in run.glyphs {
                let first = boundaries.partition_point(|byte| *byte <= glyph.start).saturating_sub(1);
                if let Some(color) = self.settings.formatting.highlight_at(start + first) {
                    add_stroke(
                        &mut self.backgrounds,
                        Rectangle {
                            x: glyph.x,
                            y: run.line_y - glyph.font_size * 0.9,
                            width: glyph.w,
                            height: glyph.font_size * 1.15,
                        },
                        Ink::Background(color),
                    );
                }
            }
        }
        if !self.settings.formatting.marks.iter().any(|range| matches!(range.mark, InlineMark::Underline | InlineMark::Strike)) {
            self.strokes.clear();
            self.painted_revision = Some(revision);
            return;
        }
        let buffer = self.editor.buffer();
        let mut underlines = Vec::new();
        let mut strikes = Vec::new();
        let mut line = None;
        let mut boundaries = Vec::new();
        let mut marked = Vec::new();
        for run in buffer.layout_runs() {
            let value = buffer.lines[run.line_i].text();
            let start = self.settings.line_starts.get(run.line_i).copied().unwrap_or(0);
            if line != Some(run.line_i) {
                boundaries.clear();
                boundaries.extend(value.grapheme_indices(true).map(|(byte, _)| byte));
                boundaries.push(value.len());
                marked = flags(&self.settings.formatting, start, boundaries.len().saturating_sub(1));
                line = Some(run.line_i);
            }
            for glyph in run.glyphs {
                let first = boundaries.partition_point(|byte| *byte <= glyph.start).saturating_sub(1);
                let end = boundaries.partition_point(|byte| *byte < glyph.end).min(marked.len());
                let bits = marked[first.min(marked.len())..end.max(first.min(marked.len()))]
                    .iter().fold(0, |all, flag| all | flag);
                let color = self.settings.formatting.color_at(start + first).map(Ink::Color).unwrap_or_else(|| {
                    if self.settings.formatting.links.iter().any(|range| range.start <= start + first && start + first < range.end) {
                        Ink::Link
                    } else { Ink::Text }
                });
                let thickness = (glyph.font_size / 16.0).max(1.0);
                if bits & 1 != 0 {
                    add_stroke(&mut underlines, Rectangle { x: glyph.x, y: run.line_y + glyph.font_size * 0.1,
                        width: glyph.w, height: thickness }, color);
                }
                if bits & 2 != 0 {
                    add_stroke(&mut strikes, Rectangle { x: glyph.x, y: run.line_y - glyph.font_size * 0.3,
                        width: glyph.w, height: thickness }, color);
                }
            }
        }
        underlines.extend(strikes);
        self.strokes = underlines;
        self.painted_revision = Some(revision);
    }
}

pub fn view<'a>(inner: impl Into<Element<'a, AppMessage>>, content: &'a Content,
    settings: ComposerHighlightSettings, size: u16, padding: Padding, max_height: f32) -> Element<'a, AppMessage>
{
    Element::new(Decorated { inner: inner.into(), content, settings, size: f32::from(size), padding, max_height })
}

struct Decorated<'a> {
    inner: Element<'a, AppMessage>, content: &'a Content,
    settings: ComposerHighlightSettings, size: f32, padding: Padding, max_height: f32,
}

impl Widget<AppMessage, Theme, iced::Renderer> for Decorated<'_> {
    fn children(&self) -> Vec<Tree> { vec![Tree::new(&self.inner)] }
    fn diff(&self, tree: &mut Tree) { tree.diff_children(std::slice::from_ref(&self.inner)); }
    fn size(&self) -> Size<Length> { self.inner.as_widget().size() }
    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        use iced::advanced::text::Renderer as _;
        let node = self.inner.as_widget_mut().layout(&mut tree.children[0], renderer, limits);
        let mut geometry = self.content.geometry.borrow_mut();
        let geometry = geometry.get_or_insert_with(|| Geometry::new(self.content, &self.settings));
        geometry.update(&self.settings, Size::new((node.size().width - self.padding.x()).max(0.0),
            (limits.max().height.min(self.max_height) - self.padding.y()).max(0.0)), renderer.default_font(), self.size);
        node
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, AppMessage>, viewport: &Rectangle) {
        self.inner.as_widget_mut().update(&mut tree.children[0], event, layout, cursor, renderer, clipboard, shell, viewport);
    }
    fn draw(&self, tree: &Tree, renderer: &mut iced::Renderer, theme: &Theme, style: &renderer::Style,
        layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle) {
        use iced::advanced::Renderer as _;
        if let Some(geometry) = self.content.geometry.borrow_mut().as_mut() {
            geometry.paint(self.content.revision, theme);
        }
        // Keep native editor primitives at the local origin. Rendering the
        // same editor at an absolute Y offset loses glyphs on the tested Iced
        // wgpu path; a renderer translation preserves glyphs, caret and clip.
        // Input/layout/IME operations keep their normal window coordinates.
        let Some(clip) = layout.bounds().shrink(self.padding).intersection(viewport) else { return; };
        let origin = layout.bounds().position() + Vector::new(self.padding.left, self.padding.top);
        if let Some(geometry) = self.content.geometry.borrow().as_ref() {
            for background in &geometry.backgrounds {
                let translated = background.bounds + Vector::new(origin.x, origin.y);
                if let Some(bounds) = translated.intersection(&clip) {
                    let Ink::Background(value) = background.color else { continue; };
                    renderer.fill_quad(renderer::Quad { bounds, ..Default::default() }, crate::appearance::rgb(value));
                }
            }
        }
        let position = layout.position();
        let translation = Vector::new(position.x, position.y);
        let node = layout::Node::new(layout.bounds().size());
        renderer.with_translation(translation, |renderer| {
            self.inner.as_widget().draw(&tree.children[0], renderer, theme, style, Layout::new(&node),
                cursor, &(*viewport - translation));
        });
        if let Some(geometry) = self.content.geometry.borrow_mut().as_mut() {
            for stroke in &geometry.strokes {
                let translated = stroke.bounds + Vector::new(origin.x, origin.y);
                if let Some(bounds) = translated.intersection(&clip) {
                    let color = match stroke.color {
                        Ink::Color(value) => crate::appearance::rgb(value),
                        Ink::Text => crate::appearance::tone(theme, crate::appearance::Tone::Text),
                        Ink::Link => crate::appearance::tone(theme, crate::appearance::Tone::Accent),
                        Ink::Background(value) => crate::appearance::rgb(value),
                    };
                    renderer.fill_quad(renderer::Quad { bounds, ..Default::default() }, color);
                }
            }
        }
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &iced::Renderer, operation: &mut dyn Operation) {
        self.inner.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle, renderer: &iced::Renderer) -> mouse::Interaction {
        self.inner.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }
    fn overlay<'a>(&'a mut self, tree: &'a mut Tree, layout: Layout<'a>, renderer: &iced::Renderer,
        viewport: &Rectangle, translation: Vector) -> Option<overlay::Element<'a, AppMessage, Theme, iced::Renderer>> {
        self.inner.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shaped(value: &str, formatting: ComposerFormatting, width: f32, height: f32) -> Content {
        static FONTS: std::sync::Once = std::sync::Once::new();
        FONTS.call_once(|| {
            iced::advanced::graphics::text::font_system().write().unwrap()
                .load_font(std::borrow::Cow::Borrowed(crate::typography::REGULAR_BYTES));
        });
        let content = Content::with_text(value);
        let settings = ComposerHighlightSettings::new(value, formatting);
        let mut geometry = Geometry::new(&content, &settings);
        geometry.update(&settings, Size::new(width, height), crate::typography::regular(), 16.0);
        geometry.paint(content.revision, &Theme::Dark);
        *content.geometry.borrow_mut() = Some(geometry);
        content
    }

    #[test]
    fn native_replacement_and_cursor_are_shared_after_a_zwj_selection() {
        let mut formatting = ComposerFormatting::default();
        formatting.toggle_mark(1, 2, InlineMark::Underline);
        let mut content = shaped("A👩‍💻B", formatting, 300.0, 120.0);
        content.move_to(message_body::editor_cursor_for_selection("A👩‍💻B", (1, 2)));
        content.perform(text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new("🌍".into()))));
        assert_eq!(content.text(), "A🌍B");
        let geometry = content.geometry.borrow();
        let editor = &geometry.as_ref().unwrap().editor;
        assert_eq!(editor.line(0).unwrap().text, "A🌍B");
        assert_eq!(editor.cursor(), content.cursor());
    }

    #[test]
    fn wrapping_and_scrolling_keep_decoration_geometry_inside_the_visible_lines() {
        let value = "Texto longo de teste ".repeat(40);
        let mut formatting = ComposerFormatting::default();
        formatting.toggle_mark(0, value.graphemes(true).count(), InlineMark::Underline);
        let mut content = shaped(&value, formatting, 120.0, 80.0);
        let before = content.geometry.borrow().as_ref().unwrap().strokes.clone();
        assert!(before.len() >= 3);
        assert!(before.iter().all(|stroke| stroke.bounds.width > 0.0 && stroke.bounds.width <= 121.0));
        content.perform(text_editor::Action::Scroll { lines: 4 });
        let mut geometry = content.geometry.borrow_mut();
        let geometry = geometry.as_mut().unwrap();
        geometry.paint(content.revision, &Theme::Dark);
        assert!(geometry.editor.buffer().scroll().line > 0 || geometry.editor.buffer().scroll().vertical > 0.0);
        assert!(!geometry.strokes.is_empty());
        assert!(geometry.strokes.iter().all(|stroke| stroke.bounds.y < 100.0));
    }
    #[test]
    fn grapheme_marks_remain_half_open_and_do_not_bleed_into_the_next_line() {
        let mut formatting = ComposerFormatting::default();
        formatting.toggle_mark(1, 3, InlineMark::Underline);
        formatting.toggle_mark(2, 4, InlineMark::Strike);
        assert_eq!(flags(&formatting, 0, 5), [0, 1, 3, 2, 0]);
        assert_eq!(flags(&formatting, 3, 3), [2, 0, 0]);
    }

    #[test]
    fn highlight_geometry_tracks_the_same_shaped_glyphs_as_underlines() {
        let mut formatting = ComposerFormatting::default();
        formatting.set_highlight(0, 5, Some(0xfdd472));
        let content = shaped("Atenção", formatting, 300.0, 80.0);
        let geometry = content.geometry.borrow();
        let geometry = geometry.as_ref().unwrap();
        assert!(!geometry.backgrounds.is_empty());
        assert!(geometry.backgrounds.iter().all(|stroke| {
            stroke.bounds.width > 0.0 && stroke.bounds.height > 0.0
        }));
    }
    #[test]
    fn strokes_join_visual_neighbors_but_never_cross_a_gap_or_a_line() {
        let mut output = Vec::new();
        let rect = |x, y| Rectangle { x, y, width: 10.0, height: 1.0 };
        add_stroke(&mut output, rect(20.0, 12.0), Ink::Text);
        add_stroke(&mut output, rect(10.0, 12.0), Ink::Text);
        assert_eq!(output[0].bounds.width, 20.0);
        add_stroke(&mut output, rect(40.0, 12.0), Ink::Text);
        add_stroke(&mut output, rect(50.0, 30.0), Ink::Text);
        assert_eq!(output.len(), 3);
    }
}
