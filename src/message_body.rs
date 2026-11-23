//! A bounded HTML-to-native text model. No WebView, scripts or remote styles.
use crate::{
    AppMessage,
    appearance::{Tone, quiet_frame, quote_mark, tone, ui_surface, ui_text},
    images::{InlineImage, MediaCache},
    paste,
    preferences::Preferences,
    typography,
};
use iced::widget::text::{Span, Wrapping};
use iced::widget::{button, column, container, image, rich_text, row, space, stack, text, tooltip};
use iced::{Background, Border, Element, Fill, Font, Theme, font};
use scraper::{ElementRef, Html};
use serde::{Deserialize, Serialize};
use std::ops::Range;

mod table_row;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    pub blocks: Vec<Block>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardPayload {
    pub text: String,
    pub html: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposerPaste {
    pub text: String,
    pub formatting: ComposerFormatting,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Block {
    pub runs: Vec<Run>,
    pub media: Option<InlineImage>,
    pub card: Option<CardFallback>,
    pub quote: u8,
    /// Identifies one source quote, including all of its paragraphs.
    pub quote_group: usize,
    pub indent: u8,
    pub marker: Option<String>,
    pub ordered_list_style: Option<OrderedListStyle>,
    pub ordered_list_value: Option<i64>,
    pub heading: u8,
    pub pre: bool,
    pub table: Option<Table>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OrderedListStyle {
    #[default]
    Decimal,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

impl OrderedListStyle {
    fn html_type(self) -> Option<&'static str> {
        match self {
            Self::Decimal => None,
            Self::LowerAlpha => Some("a"),
            Self::UpperAlpha => Some("A"),
            Self::LowerRoman => Some("i"),
            Self::UpperRoman => Some("I"),
        }
    }

    fn marker(self, value: i64) -> String {
        let label = match self {
            Self::Decimal => value.to_string(),
            Self::LowerAlpha => {
                alphabetic_list_value(value, false).unwrap_or_else(|| value.to_string())
            }
            Self::UpperAlpha => {
                alphabetic_list_value(value, true).unwrap_or_else(|| value.to_string())
            }
            Self::LowerRoman => roman_list_value(value, false).unwrap_or_else(|| value.to_string()),
            Self::UpperRoman => roman_list_value(value, true).unwrap_or_else(|| value.to_string()),
        };
        format!("{label}.")
    }
}

fn alphabetic_list_value(value: i64, uppercase: bool) -> Option<String> {
    if value <= 0 {
        return None;
    }
    let base = if uppercase { b'A' } else { b'a' };
    let mut number = value as u64;
    let mut label = Vec::new();
    while number > 0 {
        number -= 1;
        label.push((base + (number % 26) as u8) as char);
        number /= 26;
    }
    label.reverse();
    Some(label.into_iter().collect())
}

fn roman_list_value(value: i64, uppercase: bool) -> Option<String> {
    if !(1..=3_999).contains(&value) {
        return None;
    }
    let mut number = value as usize;
    let mut label = String::new();
    for (amount, digits) in [
        (1_000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ] {
        while number >= amount {
            number -= amount;
            label.push_str(digits);
        }
    }
    Some(if uppercase {
        label
    } else {
        label.to_ascii_lowercase()
    })
}

fn ordered_list_style(element: ElementRef<'_>) -> OrderedListStyle {
    if let Some(css) = element.value().attr("style") {
        for declaration in css.split(';') {
            let Some((key, value)) = declaration.split_once(':') else {
                continue;
            };
            if key.trim().eq_ignore_ascii_case("list-style-type") {
                match value.trim().to_ascii_lowercase().as_str() {
                    "lower-alpha" | "lower-latin" => return OrderedListStyle::LowerAlpha,
                    "upper-alpha" | "upper-latin" => return OrderedListStyle::UpperAlpha,
                    "lower-roman" => return OrderedListStyle::LowerRoman,
                    "upper-roman" => return OrderedListStyle::UpperRoman,
                    "decimal" => return OrderedListStyle::Decimal,
                    _ => {}
                }
            }
        }
    }

    match element.value().attr("type").map(str::trim) {
        Some("a") => OrderedListStyle::LowerAlpha,
        Some("A") => OrderedListStyle::UpperAlpha,
        Some("i") => OrderedListStyle::LowerRoman,
        Some("I") => OrderedListStyle::UpperRoman,
        _ => OrderedListStyle::Decimal,
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table {
    pub rows: Vec<TableRow>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableRow {
    pub cells: Vec<TableCell>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableCell {
    pub blocks: Vec<Block>,
    pub header: bool,
    pub alignment: CellAlignment,
    pub vertical_alignment: CellVerticalAlignment,
    /// Number of logical table columns occupied by this cell.
    pub column_span: u8,
    /// Percentage width hint retained from HTML width attributes or styles.
    /// Values use hundredths of one percent (2500 = 25%).
    pub width: Option<u32>,
    pub color: Option<u32>,
    pub background: Option<u32>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CellAlignment {
    #[default]
    Start,
    Center,
    End,
    Justify,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CellVerticalAlignment {
    #[default]
    Top,
    Center,
    Bottom,
}
impl CellAlignment {
    fn iced(self) -> iced::Alignment {
        match self {
            Self::Center => iced::Alignment::Center,
            Self::End => iced::Alignment::End,
            Self::Start | Self::Justify => iced::Alignment::Start,
        }
    }

    fn css(self) -> &'static str {
        match self {
            Self::Start => "left",
            Self::Center => "center",
            Self::End => "right",
            Self::Justify => "justify",
        }
    }
}
impl CellVerticalAlignment {
    fn iced(self) -> iced::alignment::Vertical {
        match self {
            Self::Top => iced::alignment::Vertical::Top,
            Self::Center => iced::alignment::Vertical::Center,
            Self::Bottom => iced::alignment::Vertical::Bottom,
        }
    }

    fn css(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Center => "middle",
            Self::Bottom => "bottom",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardFallback {
    pub kind: String,
    pub title: Option<String>,
    pub lines: Vec<String>,
    pub line_styles: Vec<CardLineStyle>,
    pub fact_sets: Vec<CardFactSet>,
    /// Horizontal groups. Each range indexes `lines` and belongs to one column.
    pub column_rows: Vec<CardColumnRow>,
    /// First safe HTTPS open action. Other labels stay in `actions`.
    pub action: Option<CardAction>,
    /// Every action label, in payload order. `url` is empty when it cannot be opened.
    pub actions: Vec<CardAction>,
    /// Icon that sits beside the line at `line`.
    pub marks: Vec<CardMark>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardColumnRow {
    pub columns: Vec<Range<usize>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardMark {
    pub line: usize,
    pub kind: CardMarkKind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardMarkKind {
    Brand,
    Issue,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardFactSet {
    /// Insert this fact group before the line at this index.
    pub before_line: usize,
    pub facts: Vec<CardFact>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardFact {
    pub title: String,
    pub value: String,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CardLineStyle {
    pub emphasis: CardLineEmphasis,
    pub subtle: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum CardLineEmphasis {
    #[default]
    Body,
    Strong,
    Heading,
}

impl CardLineStyle {
    fn merge(self, other: Self) -> Self {
        Self {
            emphasis: self.emphasis.max(other.emphasis),
            subtle: self.subtle && other.subtle,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardAction {
    pub label: String,
    pub url: String,
    pub positive: bool,
    pub icon: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub style: Style,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
    pub mention: bool,
    pub mention_info: Option<crate::mentions::Mention>,
    pub link: Option<String>,
    pub color: Option<u32>,
    pub background: Option<u32>,
    pub size: u16,
}

/// Formatting applied to a grapheme range in the native message composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InlineMark {
    Bold,
    Italic,
    Underline,
    Strike,
    Code,
}

impl InlineMark {
    pub const ALL: [Self; 5] = [
        Self::Bold,
        Self::Italic,
        Self::Underline,
        Self::Strike,
        Self::Code,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatRange {
    pub start: usize,
    pub end: usize,
    pub mark: InlineMark,
}

/// Percentage of the normal chat text size applied to a grapheme range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeRange {
    pub start: usize,
    pub end: usize,
    pub percent: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorRange {
    pub start: usize,
    pub end: usize,
    pub color: u32,
}

/// A pale background applied to a grapheme range in the composer.
///
/// Text colour and highlight colour are deliberately kept as separate ranges:
/// this mirrors the Teams/Rusty Teams model and lets a highlight coexist with
/// an arbitrary foreground colour.
pub type HighlightRange = ColorRange;

/// Inclusive first line, exclusive last line of a block quotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteRange {
    pub start: usize,
    pub end: usize,
}

/// A table represented by consecutive tab-separated editor lines.
/// `start`/`end` are inclusive/exclusive line indexes; `columns` keeps empty
/// cells visible when a freshly inserted table has not been filled yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRange {
    pub start: usize,
    pub end: usize,
    pub columns: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerFontSize { Small, Normal, Large }

impl ComposerFontSize {
    pub const ALL: [Self; 3] = [Self::Small, Self::Normal, Self::Large];
    pub fn percent(self) -> u16 {
        match self { Self::Small => 90, Self::Normal => 100, Self::Large => 125 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerTextColor { Default, Red, Orange, Yellow, Lime, Green, Teal, Blue, Pink }

impl ComposerTextColor {
    /// Measured from the official 2026-11-10 font color popup. Existing draft
    /// ranges store raw RGB values, so older colors still roundtrip unchanged.
    pub const PALETTE: [Self; 8] = [Self::Red, Self::Orange, Self::Yellow, Self::Lime,
        Self::Green, Self::Teal, Self::Blue, Self::Pink];
    pub fn value(self) -> Option<u32> {
        match self {
            Self::Default => None,
            Self::Red => Some(0xb4444d), Self::Orange => Some(0xcc593a),
            Self::Yellow => Some(0xfdbe33), Self::Lime => Some(0xbcc94d),
            Self::Green => Some(0x2f9962), Self::Teal => Some(0x3a787a),
            Self::Blue => Some(0x2454a1), Self::Pink => Some(0xa33b79),
        }
    }
}

impl std::fmt::Display for ComposerTextColor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Default => "Automático", Self::Red => "Vermelho", Self::Orange => "Laranja",
            Self::Yellow => "Amarelo", Self::Lime => "Verde-limão", Self::Green => "Verde",
            Self::Teal => "Verde-azulado", Self::Blue => "Azul", Self::Pink => "Rosa",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerHighlightColor {
    Yellow,
    Rose,
    Orange,
    Lime,
    Green,
    Teal,
    Blue,
    Pink,
}

impl ComposerHighlightColor {
    /// The light swatches used by the Rusty Teams highlight popup.
    pub const PALETTE: [Self; 8] = [
        Self::Rose,
        Self::Orange,
        Self::Yellow,
        Self::Lime,
        Self::Green,
        Self::Teal,
        Self::Blue,
        Self::Pink,
    ];

    pub fn value(self) -> u32 {
        match self {
            Self::Rose => 0xdf9299,
            Self::Orange => 0xf4a593,
            Self::Yellow => 0xfdd472,
            Self::Lime => 0xe5f18f,
            Self::Green => 0x82cda8,
            Self::Teal => 0x9dd9db,
            Self::Blue => 0xc7d4e8,
            Self::Pink => 0xebd3e1,
        }
    }
}

impl std::fmt::Display for ComposerHighlightColor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Rose => "Rosa suave",
            Self::Orange => "Pêssego",
            Self::Yellow => "Amarelo suave",
            Self::Lime => "Lima suave",
            Self::Green => "Verde suave",
            Self::Teal => "Turquesa suave",
            Self::Blue => "Azul suave",
            Self::Pink => "Lilás suave",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRange {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListKind {
    Unordered,
    Ordered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListRange {
    /// Inclusive first line, exclusive last line.
    pub start: usize,
    pub end: usize,
    pub kind: ListKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeBlockRange {
    /// Inclusive first line, exclusive last line.
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComposerFormatting {
    pub mentions: Vec<crate::mention_composer::Range>,
    pub marks: Vec<FormatRange>,
    pub sizes: Vec<SizeRange>,
    pub colors: Vec<ColorRange>,
    pub highlights: Vec<HighlightRange>,
    pub links: Vec<LinkRange>,
    pub lists: Vec<ListRange>,
    pub code_blocks: Vec<CodeBlockRange>,
    pub quote_blocks: Vec<QuoteRange>,
    pub tables: Vec<TableRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerHighlightSettings {
    pub formatting: ComposerFormatting,
    pub line_starts: Vec<usize>,
}

impl ComposerHighlightSettings {
    pub fn new(text: &str, formatting: ComposerFormatting) -> Self {
        use unicode_segmentation::UnicodeSegmentation;

        let mut line_starts = Vec::new();
        let mut start = 0;
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                start += 1;
            }
            line_starts.push(start);
            start += line.graphemes(true).count();
        }
        Self {
            formatting,
            line_starts,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ComposerHighlight {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: bool,
    pub color: Option<u32>,
    pub emoji: bool,
}

#[derive(Debug, Clone)]
pub struct ComposerHighlighter {
    settings: ComposerHighlightSettings,
    line: usize,
}

impl iced::advanced::text::Highlighter for ComposerHighlighter {
    type Settings = ComposerHighlightSettings;
    type Highlight = ComposerHighlight;
    type Iterator<'a> = std::vec::IntoIter<(Range<usize>, ComposerHighlight)>;

    fn new(settings: &Self::Settings) -> Self {
        Self {
            settings: settings.clone(),
            line: 0,
        }
    }

    fn update(&mut self, settings: &Self::Settings) {
        if self.settings != *settings {
            self.settings = settings.clone();
            self.line = 0;
        }
    }

    fn change_line(&mut self, line: usize) {
        self.line = line;
    }

    fn highlight_line(&mut self, line: &str) -> Self::Iterator<'_> {
        use unicode_segmentation::UnicodeSegmentation;

        let start = self
            .settings
            .line_starts
            .get(self.line)
            .copied()
            .unwrap_or(0);
        self.line += 1;
        let mut output = Vec::new();
        let mut active: Option<(usize, usize, ComposerHighlight)> = None;
        let mut grapheme_index = 0;
        for (byte, grapheme) in line.grapheme_indices(true) {
            let index = start + grapheme_index;
            grapheme_index += 1;
            let highlight = ComposerHighlight {
                bold: self.settings.formatting.marks.iter().any(|range| {
                    range.mark == InlineMark::Bold && range.start <= index && index < range.end
                }),
                italic: self.settings.formatting.marks.iter().any(|range| {
                    range.mark == InlineMark::Italic && range.start <= index && index < range.end
                }),
                code: self
                    .settings
                    .formatting
                    .code_blocks
                    .iter()
                    .any(|range| range.start <= self.line - 1 && self.line - 1 < range.end)
                    || self.settings.formatting.marks.iter().any(|range| {
                        range.mark == InlineMark::Code && range.start <= index && index < range.end
                    }),
                link: self.settings.formatting.mentions.iter().any(|r|r.start<=index&&index<r.end) || self
                    .settings
                    .formatting
                    .links
                    .iter()
                    .any(|range| range.start <= index && index < range.end),
                color: self.settings.formatting.color_at(index),
                emoji: typography::is_color_emoji(grapheme),
            };
            let end = byte + grapheme.len();
            // Cover plain runs too, so newly created editor buffers use the
            // bundled font even when the draft has no formatting marks.
            match active.take() {
                Some((begin, last, style)) if style == highlight && last == byte =>
                {
                    active = Some((begin, end, style));
                }
                Some((begin, last, style)) => {
                    output.push((begin..last, style));
                    active = Some((byte, end, highlight));
                }
                None => {
                    active = Some((byte, end, highlight));
                }
            }
        }
        if let Some((begin, end, style)) = active {
            output.push((begin..end, style));
        }
        output.into_iter()
    }

    fn current_line(&self) -> usize {
        self.line
    }
}

pub fn composer_highlight_format(
    highlight: &ComposerHighlight,
    theme: &Theme,
) -> iced::advanced::text::highlighter::Format<Font> {
    let font = if highlight.emoji {
        typography::EMOJI
    } else if highlight.code {
        Font::MONOSPACE
    } else {
        Font {
            family: if highlight.italic { typography::ITALIC_FALLBACK.family } else { typography::regular().family },
            weight: if highlight.bold {
                font::Weight::Semibold
            } else {
                font::Weight::Normal
            },
            style: if highlight.italic {
                font::Style::Italic
            } else {
                font::Style::Normal
            },
            ..Font::DEFAULT
        }
    };
    let color = highlight.color.map_or_else(
        || crate::appearance::tone(theme, if highlight.link {
            crate::appearance::Tone::Accent
        } else {
            crate::appearance::Tone::Text
        }),
        crate::appearance::rgb,
    );
    iced::advanced::text::highlighter::Format {
        color: Some(color),
        font: Some(font),
    }
}

pub fn editor_cursor_for_selection(
    text: &str,
    selection: (usize, usize),
) -> iced::advanced::text::editor::Cursor {
    iced::advanced::text::editor::Cursor {
        position: editor_position(text, selection.1),
        selection: Some(editor_position(text, selection.0)),
    }
}

pub fn editor_cursor_at(text: &str, index: usize) -> iced::advanced::text::editor::Cursor {
    iced::advanced::text::editor::Cursor {
        position: editor_position(text, index),
        selection: None,
    }
}

pub fn editor_selection(
    text: &str,
    cursor: iced::advanced::text::editor::Cursor,
) -> Option<(usize, usize)> {
    let anchor = cursor.selection?;
    let active = editor_position_to_grapheme(text, cursor.position);
    let anchor = editor_position_to_grapheme(text, anchor);
    (active != anchor).then_some((active.min(anchor), active.max(anchor)))
}

pub fn editor_cursor_range(
    text: &str,
    cursor: iced::advanced::text::editor::Cursor,
) -> (usize, usize) {
    editor_selection(text, cursor).unwrap_or_else(|| {
        let offset = editor_position_to_grapheme(text, cursor.position);
        (offset, offset)
    })
}

fn editor_position(text: &str, target: usize) -> iced::advanced::text::editor::Position {
    use unicode_segmentation::UnicodeSegmentation;

    let mut start = 0;
    let lines = text.split('\n').collect::<Vec<_>>();
    for (line_index, line) in lines.iter().enumerate() {
        let graphemes = line.graphemes(true).collect::<Vec<_>>();
        let end = start + graphemes.len();
        if target <= end || line_index + 1 == lines.len() {
            let local = target.saturating_sub(start).min(graphemes.len());
            let column = graphemes
                .iter()
                .take(local)
                .map(|grapheme| grapheme.len())
                .sum();
            return iced::advanced::text::editor::Position {
                line: line_index,
                column,
            };
        }
        start = end + 1;
    }
    iced::advanced::text::editor::Position {
        line: lines.len().saturating_sub(1),
        column: 0,
    }
}

fn editor_position_to_grapheme(
    text: &str,
    position: iced::advanced::text::editor::Position,
) -> usize {
    use unicode_segmentation::UnicodeSegmentation;

    let mut offset = 0;
    let mut lines = text.split('\n');
    for _ in 0..position.line {
        if let Some(line) = lines.next() {
            offset += line.graphemes(true).count() + 1;
        } else {
            return text.graphemes(true).count();
        }
    }
    let Some(line) = lines.next() else {
        return text.graphemes(true).count();
    };
    let mut column = position.column.min(line.len());
    while !line.is_char_boundary(column) {
        column -= 1;
    }
    offset + line[..column].graphemes(true).count()
}

impl ComposerFormatting {
    pub fn is_empty(&self) -> bool {
        self.mentions.is_empty()
            && self.marks.is_empty()
            && self.sizes.is_empty()
            && self.colors.is_empty()
            && self.highlights.is_empty()
            && self.links.is_empty()
            && self.lists.is_empty()
            && self.code_blocks.is_empty()
            && self.quote_blocks.is_empty()
            && self.tables.is_empty()
    }

    pub fn merge_inline_paste(&mut self, pasted: &Self, grapheme_offset: usize) {
        self.marks
            .extend(pasted.marks.iter().map(|range| FormatRange {
                start: range.start.saturating_add(grapheme_offset),
                end: range.end.saturating_add(grapheme_offset),
                mark: range.mark,
            }));
        self.sizes
            .extend(pasted.sizes.iter().map(|range| SizeRange {
                start: range.start.saturating_add(grapheme_offset),
                end: range.end.saturating_add(grapheme_offset),
                percent: range.percent,
            }));
        self.colors
            .extend(pasted.colors.iter().map(|range| ColorRange {
                start: range.start.saturating_add(grapheme_offset),
                end: range.end.saturating_add(grapheme_offset),
                color: range.color,
            }));
        self.highlights
            .extend(pasted.highlights.iter().map(|range| HighlightRange {
                start: range.start.saturating_add(grapheme_offset),
                end: range.end.saturating_add(grapheme_offset),
                color: range.color,
            }));
        self.links
            .extend(pasted.links.iter().map(|range| LinkRange {
                start: range.start.saturating_add(grapheme_offset),
                end: range.end.saturating_add(grapheme_offset),
                url: range.url.clone(),
            }));
        normalize_marks(&mut self.marks);
        normalize_sizes(&mut self.sizes);
        normalize_colors(&mut self.colors);
        normalize_colors(&mut self.highlights);
        normalize_links(&mut self.links);
    }

    pub fn has_mark(&self, start: usize, end: usize, mark: InlineMark) -> bool {
        start < end
            && self
                .marks
                .iter()
                .any(|range| range.mark == mark && range.start <= start && range.end >= end)
    }

    pub fn has_mark_at(&self, cursor: usize, mark: InlineMark) -> bool {
        self.marks.iter().any(|range| {
            range.mark == mark
                && (range.start <= cursor && cursor < range.end
                    || cursor > 0 && range.end == cursor)
        })
    }

    pub fn size_at(&self, cursor: usize) -> u16 {
        self.sizes
            .iter()
            .find(|range| {
                range.start <= cursor && cursor < range.end
                    || cursor > 0 && range.end == cursor
            })
            .map_or(100, |range| range.percent)
    }

    pub fn size_for_range(&self, start: usize, end: usize) -> Option<u16> {
        if start > end {
            return None;
        }
        if start == end {
            return Some(self.size_at(start));
        }
        let size = self.size_at(start);
        (start..end)
            .all(|index| self.size_at(index) == size)
            .then_some(size)
    }

    pub fn set_size(&mut self, start: usize, end: usize, percent: u16) -> bool {
        if start >= end {
            return false;
        }
        let percent = percent.clamp(75, 200);
        let mut next = Vec::with_capacity(self.sizes.len() + usize::from(percent != 100));
        for range in self.sizes.drain(..) {
            if range.end <= start || range.start >= end {
                next.push(range);
                continue;
            }
            if range.start < start {
                next.push(SizeRange {
                    start: range.start,
                    end: start,
                    percent: range.percent,
                });
            }
            if range.end > end {
                next.push(SizeRange {
                    start: end,
                    end: range.end,
                    percent: range.percent,
                });
            }
        }
        if percent != 100 {
            next.push(SizeRange {
                start,
                end,
                percent,
            });
        }
        normalize_sizes(&mut next);
        self.sizes = next;
        true
    }

    pub fn color_at(&self, cursor: usize) -> Option<u32> {
        self.colors
            .iter()
            .find(|range| {
                range.start <= cursor && cursor < range.end
                    || cursor > 0 && range.end == cursor
            })
            .map(|range| range.color)
    }

    pub fn color_for_range(&self, start: usize, end: usize) -> Option<Option<u32>> {
        if start > end {
            return None;
        }
        if start == end {
            return Some(self.color_at(start));
        }
        let color = self.color_at(start);
        (start..end)
            .all(|index| self.color_at(index) == color)
            .then_some(color)
    }

    pub fn set_color(&mut self, start: usize, end: usize, color: Option<u32>) -> bool {
        if start >= end {
            return false;
        }
        let mut next = Vec::with_capacity(self.colors.len() + usize::from(color.is_some()));
        for range in self.colors.drain(..) {
            if range.end <= start || range.start >= end {
                next.push(range);
                continue;
            }
            if range.start < start {
                next.push(ColorRange {
                    start: range.start,
                    end: start,
                    color: range.color,
                });
            }
            if range.end > end {
                next.push(ColorRange {
                    start: end,
                    end: range.end,
                    color: range.color,
                });
            }
        }
        if let Some(color) = color {
            next.push(ColorRange { start, end, color });
        }
        normalize_colors(&mut next);
        self.colors = next;
        true
    }

    pub fn highlight_at(&self, cursor: usize) -> Option<u32> {
        self.highlights
            .iter()
            .find(|range| {
                range.start <= cursor && cursor < range.end
                    || cursor > 0 && range.end == cursor
            })
            .map(|range| range.color)
    }

    pub fn highlight_for_range(&self, start: usize, end: usize) -> Option<Option<u32>> {
        if start > end {
            return None;
        }
        if start == end {
            return Some(self.highlight_at(start));
        }
        let color = self.highlight_at(start);
        (start..end)
            .all(|index| self.highlight_at(index) == color)
            .then_some(color)
    }

    pub fn set_highlight(&mut self, start: usize, end: usize, color: Option<u32>) -> bool {
        if start >= end {
            return false;
        }
        let mut next = Vec::with_capacity(self.highlights.len() + usize::from(color.is_some()));
        for range in self.highlights.drain(..) {
            if range.end <= start || range.start >= end {
                next.push(range);
                continue;
            }
            if range.start < start {
                next.push(HighlightRange {
                    start: range.start,
                    end: start,
                    color: range.color,
                });
            }
            if range.end > end {
                next.push(HighlightRange {
                    start: end,
                    end: range.end,
                    color: range.color,
                });
            }
        }
        if let Some(color) = color {
            next.push(HighlightRange { start, end, color });
        }
        normalize_colors(&mut next);
        self.highlights = next;
        true
    }

    pub fn toggle_mark(&mut self, start: usize, end: usize, mark: InlineMark) -> bool {
        if start >= end {
            return false;
        }
        let remove = self.has_mark(start, end, mark);
        let mut next = Vec::with_capacity(self.marks.len() + usize::from(!remove));
        for range in self.marks.drain(..) {
            if range.mark != mark || range.end <= start || range.start >= end {
                next.push(range);
                continue;
            }
            if range.start < start {
                next.push(FormatRange {
                    start: range.start,
                    end: start,
                    mark,
                });
            }
            if range.end > end {
                next.push(FormatRange {
                    start: end,
                    end: range.end,
                    mark,
                });
            }
        }
        if !remove {
            next.push(FormatRange { start, end, mark });
        }
        normalize_marks(&mut next);
        self.marks = next;
        true
    }

    pub fn set_link(&mut self, start: usize, end: usize, url: &str) -> bool {
        let Some(url) = valid_link(url) else {
            return false;
        };
        if start >= end {
            return false;
        }
        let mut next = Vec::with_capacity(self.links.len() + 1);
        for range in self.links.drain(..) {
            if range.end <= start || range.start >= end {
                next.push(range);
                continue;
            }
            if range.start < start {
                next.push(LinkRange {
                    start: range.start,
                    end: start,
                    url: range.url.clone(),
                });
            }
            if range.end > end {
                next.push(LinkRange {
                    start: end,
                    end: range.end,
                    url: range.url,
                });
            }
        }
        next.push(LinkRange { start, end, url });
        normalize_links(&mut next);
        self.links = next;
        true
    }

    pub fn toggle_list(&mut self, text: &str, selection: (usize, usize), kind: ListKind) -> bool {
        let (start, end) = selected_lines(text, selection);
        if start >= end {
            return false;
        }
        let remove = self
            .lists
            .iter()
            .any(|range| range.start <= start && range.end >= end && range.kind == kind);
        let mut next = Vec::with_capacity(self.lists.len() + usize::from(!remove));
        for range in self.lists.drain(..) {
            if range.end <= start || range.start >= end {
                next.push(range);
                continue;
            }
            if range.start < start {
                next.push(ListRange {
                    start: range.start,
                    end: start,
                    kind: range.kind,
                });
            }
            if range.end > end {
                next.push(ListRange {
                    start: end,
                    end: range.end,
                    kind: range.kind,
                });
            }
        }
        if !remove {
            next.push(ListRange { start, end, kind });
            self.code_blocks = self
                .code_blocks
                .drain(..)
                .flat_map(|range| {
                    split_line_range(range.start, range.end, start, end)
                        .into_iter()
                        .map(|(start, end)| CodeBlockRange { start, end })
                })
                .collect();
            normalize_code_blocks(&mut self.code_blocks);
        }
        normalize_lists(&mut next);
        self.lists = next;
        true
    }

    pub fn toggle_code_block(&mut self, text: &str, selection: (usize, usize)) -> bool {
        let (start, end) = selected_lines(text, selection);
        if start >= end {
            return false;
        }
        let remove = self
            .code_blocks
            .iter()
            .any(|range| range.start <= start && range.end >= end);
        let mut next = Vec::with_capacity(self.code_blocks.len() + usize::from(!remove));
        for range in self.code_blocks.drain(..) {
            if range.end <= start || range.start >= end {
                next.push(range);
                continue;
            }
            next.extend(
                split_line_range(range.start, range.end, start, end)
                    .into_iter()
                    .map(|(start, end)| CodeBlockRange { start, end }),
            );
        }
        if !remove {
            next.push(CodeBlockRange { start, end });
            self.lists = self
                .lists
                .drain(..)
                .flat_map(|range| {
                    split_line_range(range.start, range.end, start, end)
                        .into_iter()
                        .map(move |(start, end)| ListRange {
                            start,
                            end,
                            kind: range.kind,
                        })
                })
                .collect();
            normalize_lists(&mut self.lists);
        }
        normalize_code_blocks(&mut next);
        self.code_blocks = next;
        true
    }

    /// Toggle a block quotation for every line touched by the selection.
    /// Quoting is line-oriented, just like lists and code blocks in Rusty
    /// Teams. Applying it also removes list/code markers from those lines so
    /// the exported HTML has one unambiguous block structure.
    pub fn toggle_quote(&mut self, text: &str, selection: (usize, usize)) -> bool {
        let (start, end) = selected_lines(text, selection);
        if start >= end {
            return false;
        }
        let remove = self
            .quote_blocks
            .iter()
            .any(|range| range.start <= start && range.end >= end);
        let mut next = Vec::with_capacity(self.quote_blocks.len() + usize::from(!remove));
        for range in self.quote_blocks.drain(..) {
            if range.end <= start || range.start >= end {
                next.push(range);
                continue;
            }
            if range.start < start {
                next.push(QuoteRange {
                    start: range.start,
                    end: start,
                });
            }
            if range.end > end {
                next.push(QuoteRange {
                    start: end,
                    end: range.end,
                });
            }
        }
        if !remove {
            next.push(QuoteRange { start, end });
            self.lists = self
                .lists
                .drain(..)
                .flat_map(|range| {
                    split_line_range(range.start, range.end, start, end)
                        .into_iter()
                        .map(move |(start, end)| ListRange {
                            start,
                            end,
                            kind: range.kind,
                        })
                })
                .collect();
            self.code_blocks = self
                .code_blocks
                .drain(..)
                .flat_map(|range| {
                    split_line_range(range.start, range.end, start, end)
                        .into_iter()
                        .map(|(start, end)| CodeBlockRange { start, end })
                })
                .collect();
            normalize_lists(&mut self.lists);
            normalize_code_blocks(&mut self.code_blocks);
        }
        normalize_quote_blocks(&mut next);
        self.quote_blocks = next;
        true
    }

    pub fn has_quote(&self, text: &str, selection: (usize, usize)) -> bool {
        let (start, end) = selected_lines(text, selection);
        start < end
            && self
                .quote_blocks
                .iter()
                .any(|range| range.start <= start && range.end >= end)
    }

    pub fn insert_table(&mut self, start: usize, rows: usize, columns: usize) -> bool {
        if rows == 0 || columns == 0 || rows > 20 || columns > 12 {
            return false;
        }
        self.tables.push(TableRange {
            start,
            end: start.saturating_add(rows),
            columns: columns.clamp(1, u8::MAX as usize) as u8,
        });
        normalize_table_ranges(&mut self.tables);
        true
    }

    /// Remove all inline and line formatting from the selected text.
    pub fn clear_range(&mut self, text: &str, selection: (usize, usize)) -> bool {
        let (start, end) = selected_lines(text, selection);
        if selection.0 >= selection.1 {
            return false;
        }
        let (inline_start, inline_end) = selection;
        self.marks = split_format_ranges(&self.marks, inline_start, inline_end);
        self.sizes = split_format_ranges(&self.sizes, inline_start, inline_end);
        self.colors = split_format_ranges(&self.colors, inline_start, inline_end);
        self.highlights = split_format_ranges(&self.highlights, inline_start, inline_end);
        self.links = split_format_ranges(&self.links, inline_start, inline_end);
        self.lists = split_line_ranges(&self.lists, start, end);
        self.code_blocks = split_line_ranges(&self.code_blocks, start, end);
        self.quote_blocks = split_line_ranges(&self.quote_blocks, start, end);
        self.tables = split_line_ranges(&self.tables, start, end);
        normalize_marks(&mut self.marks);
        normalize_sizes(&mut self.sizes);
        normalize_colors(&mut self.colors);
        normalize_colors(&mut self.highlights);
        normalize_links(&mut self.links);
        normalize_lists(&mut self.lists);
        normalize_code_blocks(&mut self.code_blocks);
        normalize_quote_blocks(&mut self.quote_blocks);
        normalize_table_ranges(&mut self.tables);
        true
    }

    /// Keep styles on unchanged text when the user edits the plain-text input.
    /// Grapheme ranges inside replaced text are dropped; unchanged prefixes and
    /// suffixes keep their styles and shift with the text.
    pub fn adjust_after_edit(&mut self, old: &str, new: &str) {
        use unicode_segmentation::UnicodeSegmentation;

        crate::mention_composer::adjust(&mut self.mentions,old,new);
        let old_graphemes = old.graphemes(true).collect::<Vec<_>>();
        let new_graphemes = new.graphemes(true).collect::<Vec<_>>();
        let (prefix, old_end, new_end) = common_edit(&old_graphemes, &new_graphemes);
        self.marks = self
            .marks
            .drain(..)
            .flat_map(|range| {
                remap_range(range.start, range.end, prefix, old_end, new_end)
                    .into_iter()
                    .map(move |(start, end)| FormatRange {
                        start,
                        end,
                        mark: range.mark,
                    })
            })
            .collect();
        self.sizes = self
            .sizes
            .drain(..)
            .flat_map(|range| {
                remap_range(range.start, range.end, prefix, old_end, new_end)
                    .into_iter()
                    .map(move |(start, end)| SizeRange {
                        start,
                        end,
                        percent: range.percent,
                    })
            })
            .collect();
        self.colors = self
            .colors
            .drain(..)
            .flat_map(|range| {
                remap_range(range.start, range.end, prefix, old_end, new_end)
                    .into_iter()
                    .map(move |(start, end)| ColorRange {
                        start,
                        end,
                        color: range.color,
                    })
            })
            .collect();
        self.highlights = self
            .highlights
            .drain(..)
            .flat_map(|range| {
                remap_range(range.start, range.end, prefix, old_end, new_end)
                    .into_iter()
                    .map(move |(start, end)| HighlightRange {
                        start,
                        end,
                        color: range.color,
                    })
            })
            .collect();
        self.links = self
            .links
            .drain(..)
            .flat_map(|range| {
                remap_range(range.start, range.end, prefix, old_end, new_end)
                    .into_iter()
                    .map(move |(start, end)| LinkRange {
                        start,
                        end,
                        url: range.url.clone(),
                    })
            })
            .collect();
        normalize_marks(&mut self.marks);
        normalize_sizes(&mut self.sizes);
        normalize_colors(&mut self.colors);
        normalize_colors(&mut self.highlights);
        normalize_links(&mut self.links);

        let old_lines = old.split('\n').collect::<Vec<_>>();
        let new_lines = new.split('\n').collect::<Vec<_>>();
        let old_line_refs = old_lines.iter().map(|line| *line).collect::<Vec<_>>();
        let new_line_refs = new_lines.iter().map(|line| *line).collect::<Vec<_>>();
        let (prefix, old_end, new_end) = common_edit(&old_line_refs, &new_line_refs);
        self.lists = self
            .lists
            .drain(..)
            .flat_map(|range| {
                remap_range(range.start, range.end, prefix, old_end, new_end)
                    .into_iter()
                    .map(move |(start, end)| ListRange {
                        start,
                        end,
                        kind: range.kind,
                    })
            })
            .collect();
        normalize_lists(&mut self.lists);
        self.code_blocks = self
            .code_blocks
            .drain(..)
            .flat_map(|range| {
                remap_covered_range(range.start, range.end, prefix, old_end, new_end)
                    .into_iter()
                    .map(|(start, end)| CodeBlockRange { start, end })
            })
            .collect();
        normalize_code_blocks(&mut self.code_blocks);
        self.quote_blocks = self
            .quote_blocks
            .drain(..)
            .flat_map(|range| {
                remap_covered_range(range.start, range.end, prefix, old_end, new_end)
                    .into_iter()
                    .map(|(start, end)| QuoteRange { start, end })
            })
            .collect();
        normalize_quote_blocks(&mut self.quote_blocks);
        self.tables = self
            .tables
            .drain(..)
            .flat_map(|range| {
                remap_covered_range(range.start, range.end, prefix, old_end, new_end)
                    .into_iter()
                    .map(move |(start, end)| TableRange {
                        start,
                        end,
                        columns: range.columns,
                    })
            })
            .collect();
        normalize_table_ranges(&mut self.tables);
    }

    /// Preserve active inline styles on text typed after a collapsed cursor or
    /// over a selected range. Existing ranges still follow the ordinary edit
    /// mapper, so changing unrelated text does not inherit formatting.
    pub fn adjust_after_edit_with_active_marks(
        &mut self,
        old: &str,
        new: &str,
        active_marks: &[InlineMark],
    ) {
        self.adjust_after_edit_with_active_formats(old, new, active_marks, 100, None);
    }

    pub fn adjust_after_edit_with_active_formats(
        &mut self,
        old: &str,
        new: &str,
        active_marks: &[InlineMark],
        active_size: u16,
        active_color: Option<u32>,
    ) {
        self.adjust_after_edit_with_active_formats_and_highlight(
            old,
            new,
            active_marks,
            active_size,
            active_color,
            None,
        );
    }

    pub fn adjust_after_edit_with_active_formats_and_highlight(
        &mut self,
        old: &str,
        new: &str,
        active_marks: &[InlineMark],
        active_size: u16,
        active_color: Option<u32>,
        active_highlight: Option<u32>,
    ) {
        use unicode_segmentation::UnicodeSegmentation;

        let old_graphemes = old.graphemes(true).collect::<Vec<_>>();
        let new_graphemes = new.graphemes(true).collect::<Vec<_>>();
        let (inserted_start, _, inserted_end) = common_edit(&old_graphemes, &new_graphemes);
        self.adjust_after_edit(old, new);
        if inserted_start >= inserted_end {
            return;
        }
        for mark in active_marks {
            self.marks.push(FormatRange {
                start: inserted_start,
                end: inserted_end,
                mark: *mark,
            });
        }
        normalize_marks(&mut self.marks);
        if active_size != 100 {
            self.sizes.push(SizeRange {
                start: inserted_start,
                end: inserted_end,
                percent: active_size.clamp(75, 200),
            });
            normalize_sizes(&mut self.sizes);
        }
        if let Some(color) = active_color {
            self.colors.push(ColorRange {
                start: inserted_start,
                end: inserted_end,
                color,
            });
            normalize_colors(&mut self.colors);
        }
        if let Some(color) = active_highlight {
            self.highlights.push(HighlightRange {
                start: inserted_start,
                end: inserted_end,
                color,
            });
            normalize_colors(&mut self.highlights);
        }
    }
}

fn common_edit<T: PartialEq>(old: &[T], new: &[T]) -> (usize, usize, usize) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    (prefix, old.len() - suffix, new.len() - suffix)
}

fn remap_range(
    start: usize,
    end: usize,
    prefix: usize,
    old_end: usize,
    new_end: usize,
) -> Vec<(usize, usize)> {
    if start >= end {
        return Vec::new();
    }
    if end <= prefix {
        return vec![(start, end)];
    }
    let delta = new_end as isize - old_end as isize;
    if start >= old_end {
        let shift = |value: usize| (value as isize + delta).max(0) as usize;
        return vec![(shift(start), shift(end))];
    }
    let mut mapped = Vec::with_capacity(2);
    if start < prefix {
        mapped.push((start, prefix));
    }
    if end > old_end {
        let shift = |value: usize| (value as isize + delta).max(0) as usize;
        let start = shift(old_end.max(start));
        let end = shift(end);
        if start < end {
            mapped.push((start, end));
        }
    }
    mapped
}

fn remap_covered_range(
    start: usize,
    end: usize,
    prefix: usize,
    old_end: usize,
    new_end: usize,
) -> Vec<(usize, usize)> {
    if start <= prefix && end >= old_end && end > prefix {
        let delta = new_end as isize - old_end as isize;
        return vec![(start, (end as isize + delta).max(0) as usize)];
    }
    remap_range(start, end, prefix, old_end, new_end)
}

fn split_line_range(
    range_start: usize,
    range_end: usize,
    selection_start: usize,
    selection_end: usize,
) -> Vec<(usize, usize)> {
    let mut result = Vec::with_capacity(2);
    if range_start < selection_start {
        result.push((range_start, selection_start.min(range_end)));
    }
    if range_end > selection_end {
        result.push((selection_end.max(range_start), range_end));
    }
    result.retain(|(start, end)| start < end);
    result
}

trait ComposerRangeBounds: Clone {
    fn bounds(&self) -> (usize, usize);
    fn with_bounds(&self, start: usize, end: usize) -> Self;
}

impl ComposerRangeBounds for FormatRange {
    fn bounds(&self) -> (usize, usize) { (self.start, self.end) }
    fn with_bounds(&self, start: usize, end: usize) -> Self { Self { start, end, mark: self.mark } }
}
impl ComposerRangeBounds for SizeRange {
    fn bounds(&self) -> (usize, usize) { (self.start, self.end) }
    fn with_bounds(&self, start: usize, end: usize) -> Self { Self { start, end, percent: self.percent } }
}
impl ComposerRangeBounds for ColorRange {
    fn bounds(&self) -> (usize, usize) { (self.start, self.end) }
    fn with_bounds(&self, start: usize, end: usize) -> Self { Self { start, end, color: self.color } }
}
impl ComposerRangeBounds for LinkRange {
    fn bounds(&self) -> (usize, usize) { (self.start, self.end) }
    fn with_bounds(&self, start: usize, end: usize) -> Self { Self { start, end, url: self.url.clone() } }
}
impl ComposerRangeBounds for ListRange {
    fn bounds(&self) -> (usize, usize) { (self.start, self.end) }
    fn with_bounds(&self, start: usize, end: usize) -> Self { Self { start, end, kind: self.kind } }
}
impl ComposerRangeBounds for CodeBlockRange {
    fn bounds(&self) -> (usize, usize) { (self.start, self.end) }
    fn with_bounds(&self, start: usize, end: usize) -> Self { Self { start, end } }
}
impl ComposerRangeBounds for QuoteRange {
    fn bounds(&self) -> (usize, usize) { (self.start, self.end) }
    fn with_bounds(&self, start: usize, end: usize) -> Self { Self { start, end } }
}
impl ComposerRangeBounds for TableRange {
    fn bounds(&self) -> (usize, usize) { (self.start, self.end) }
    fn with_bounds(&self, start: usize, end: usize) -> Self {
        Self { start, end, columns: self.columns }
    }
}

fn split_format_ranges<T: ComposerRangeBounds>(
    ranges: &[T],
    selection_start: usize,
    selection_end: usize,
) -> Vec<T> {
    let mut next = Vec::with_capacity(ranges.len());
    for range in ranges {
        let (start, end) = range.bounds();
        if end <= selection_start || start >= selection_end {
            next.push(range.clone());
            continue;
        }
        if start < selection_start {
            next.push(range.with_bounds(start, selection_start));
        }
        if end > selection_end {
            next.push(range.with_bounds(selection_end, end));
        }
    }
    next
}

fn split_line_ranges<T: ComposerRangeBounds>(
    ranges: &[T],
    selection_start: usize,
    selection_end: usize,
) -> Vec<T> {
    let mut next = Vec::with_capacity(ranges.len());
    for range in ranges {
        let (start, end) = range.bounds();
        for (start, end) in split_line_range(start, end, selection_start, selection_end) {
            next.push(range.with_bounds(start, end));
        }
    }
    next
}

fn normalize_marks(ranges: &mut Vec<FormatRange>) {
    ranges.sort_by_key(|range| (range.mark, range.start, range.end));
    let mut merged: Vec<FormatRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if range.start >= range.end {
            continue;
        }
        if let Some(last) = merged
            .last_mut()
            .filter(|last| last.mark == range.mark && range.start <= last.end)
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

fn normalize_sizes(ranges: &mut Vec<SizeRange>) {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<SizeRange> = Vec::with_capacity(ranges.len());
    for mut range in ranges.drain(..) {
        range.percent = range.percent.clamp(75, 200);
        if range.start >= range.end || range.percent == 100 {
            continue;
        }
        if let Some(last) = merged.last_mut() {
            if range.start < last.end {
                range.start = last.end;
            }
            if range.start >= range.end {
                continue;
            }
            if last.percent == range.percent && range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    *ranges = merged;
}

fn normalize_colors(ranges: &mut Vec<ColorRange>) {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<ColorRange> = Vec::with_capacity(ranges.len());
    for mut range in ranges.drain(..) {
        if range.start >= range.end {
            continue;
        }
        if let Some(last) = merged.last_mut() {
            if range.start < last.end {
                range.start = last.end;
            }
            if range.start >= range.end {
                continue;
            }
            if last.color == range.color && range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    *ranges = merged;
}

fn normalize_links(ranges: &mut Vec<LinkRange>) {
    ranges.sort_by(|a, b| (&a.url, a.start, a.end).cmp(&(&b.url, b.start, b.end)));
    let mut merged: Vec<LinkRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if range.start >= range.end {
            continue;
        }
        if let Some(last) = merged
            .last_mut()
            .filter(|last| last.url == range.url && range.start <= last.end)
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

fn normalize_lists(ranges: &mut Vec<ListRange>) {
    ranges.sort_by_key(|range| (range.kind, range.start, range.end));
    let mut merged: Vec<ListRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if range.start >= range.end {
            continue;
        }
        if let Some(last) = merged
            .last_mut()
            .filter(|last| last.kind == range.kind && range.start <= last.end)
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

fn normalize_code_blocks(ranges: &mut Vec<CodeBlockRange>) {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<CodeBlockRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if range.start >= range.end {
            continue;
        }
        if let Some(last) = merged.last_mut().filter(|last| range.start <= last.end) {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

fn normalize_quote_blocks(ranges: &mut Vec<QuoteRange>) {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<QuoteRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if range.start >= range.end {
            continue;
        }
        if let Some(last) = merged.last_mut().filter(|last| range.start <= last.end) {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

fn normalize_table_ranges(ranges: &mut Vec<TableRange>) {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut normalized: Vec<TableRange> = Vec::with_capacity(ranges.len());
    for mut range in ranges.drain(..) {
        range.columns = range.columns.clamp(1, 12);
        if range.start >= range.end {
            continue;
        }
        if let Some(last) = normalized.last_mut() {
            if range.start < last.end {
                continue;
            }
        }
        normalized.push(range);
    }
    *ranges = normalized;
}

fn selected_lines(text: &str, selection: (usize, usize)) -> (usize, usize) {
    use unicode_segmentation::UnicodeSegmentation;

    let graphemes = text.graphemes(true).collect::<Vec<_>>();
    let (start, end) = selection;
    if start >= end || start >= graphemes.len() {
        return (0, 0);
    }
    let line_at = |index: usize| {
        graphemes
            .iter()
            .take(index.min(graphemes.len()))
            .filter(|grapheme| **grapheme == "\n")
            .count()
    };
    (line_at(start), line_at(end - 1) + 1)
}
impl Default for Style {
    fn default() -> Self {
        Self {
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            code: false,
            mention: false,
            mention_info: None,
            link: None,
            color: None,
            background: None,
            size: 100,
        }
    }
}

pub fn valid_link(value: &str) -> Option<String> {
    if value.chars().any(char::is_control) {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    let allowed = match url.scheme() {
        "https" | "http" => {
            url.host_str().is_some() && url.username().is_empty() && url.password().is_none()
        }
        "mailto" => !url.path().is_empty(),
        _ => false,
    };
    allowed.then(|| value.to_owned())
}

fn css_color(value: &str) -> Option<u32> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        return match hex.len() {
            6 => u32::from_str_radix(hex, 16).ok(),
            3 => {
                u32::from_str_radix(&hex.chars().flat_map(|c| [c, c]).collect::<String>(), 16).ok()
            }
            _ => None,
        };
    }
    if let Some(rgb) = value.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let rgb = rgb
            .split(',')
            .map(|part| part.trim().parse::<u8>())
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        if rgb.len() == 3 {
            return Some((u32::from(rgb[0]) << 16) | (u32::from(rgb[1]) << 8) | u32::from(rgb[2]));
        }
    }
    match value {
        "red" => Some(0xc4314b),
        "blue" => Some(0x005a9e),
        "green" => Some(0x107c10),
        "yellow" => Some(0xfff100),
        "black" => Some(0),
        "white" => Some(0xffffff),
        _ => None,
    }
}
fn font_size(value: &str) -> Option<u16> {
    let units = [
        ("px", 6.25),
        ("pt", 100.0 / 12.0),
        ("%", 1.0),
        ("em", 100.0),
    ];
    units.into_iter().find_map(|(unit, multiplier)| {
        value
            .strip_suffix(unit)
            .and_then(|n| n.trim().parse::<f32>().ok())
            .filter(|n| n.is_finite() && *n > 0.0)
            .map(|n| (n * multiplier).round().clamp(75.0, 200.0) as u16)
    })
}
fn styled(element: ElementRef<'_>, parent: &Style) -> Style {
    let mut style = parent.clone();
    let tag = element.value().name();
    match tag {
        "b" | "strong" | "th" | "author" => style.bold = true,
        "i" | "em" => style.italic = true,
        "u" => style.underline = true,
        "s" | "strike" | "del" => style.strike = true,
        "code" | "pre" => style.code = true,
        "at" => {
            style.mention = true;
        }
        "a" => style.link = element.value().attr("href").and_then(valid_link),
        _ => {}
    }
    if element
        .value()
        .attr("itemtype")
        .is_some_and(|v| v.ends_with("/Mention"))
    {
        style.mention = true;
    }
    if tag=="at" || element.value().attr("itemtype").is_some_and(|value|value.ends_with("/Mention")) {
        style.mention_info=Some(crate::mentions::from_element(element));
    }
    if let Some(value) = element.value().attr("color") {
        style.color = css_color(value);
    }
    if let Some(css) = element.value().attr("style") {
        for declaration in css.split(';') {
            let Some((key, value)) = declaration.split_once(':') else {
                continue;
            };
            let value = value.trim().to_ascii_lowercase();
            match key.trim().to_ascii_lowercase().as_str() {
                "font-weight" => {
                    style.bold = value == "bold" || value.parse::<u16>().is_ok_and(|v| v >= 600)
                }
                "font-style" => style.italic = value == "italic" || value == "oblique",
                "text-decoration" | "text-decoration-line" => {
                    style.underline = value.contains("underline");
                    style.strike = value.contains("line-through");
                }
                "color" => style.color = css_color(&value),
                "background-color" => style.background = css_color(&value),
                "font-size" => {
                    if let Some(size) = font_size(&value) {
                        style.size = size;
                    }
                }
                _ => {}
            }
        }
    }
    style
}

fn card_text(value: &str) -> Option<String> {
    let value = value.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>().join("\n");
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.chars().take(2_048).collect())
    }
}

fn card_kind(value: &str) -> Option<&'static str> {
    let value = value.to_ascii_lowercase();
    if value.contains("adaptivecard") || value.contains("application/vnd.microsoft.card.adaptive") {
        Some("Cartão adaptável")
    } else if value.contains("messagecard")
        || value.contains("o365connector")
        || value.contains("application/vnd.microsoft.card")
    {
        Some("Cartão de mensagem")
    } else {
        None
    }
}

fn parse_card_json(text: &str) -> Option<serde_json::Value> {
    let text = text
        .trim()
        .strip_prefix("<![CDATA[")
        .unwrap_or(text.trim())
        .strip_suffix("]]>")
        .unwrap_or(text.trim())
        .trim();
    let mut value: serde_json::Value = serde_json::from_str(text).ok()?;
    for _ in 0..3 {
        let Some(inner) = value.get("content").and_then(serde_json::Value::as_str) else {
            break;
        };
        let Some(nested) = serde_json::from_str(inner).ok() else {
            break;
        };
        value = nested;
    }
    value.is_object().then_some(value)
}

fn card_from_json(value: &serde_json::Value, kind: &str) -> CardFallback {
    let title = card_string(value, &["title", "subject", "summary"]).and_then(card_text);
    let mut lines = Vec::new();
    let mut line_styles = Vec::new();
    let mut fact_sets = Vec::new();
    let mut column_rows = Vec::new();
    let mut action = None;
    let mut actions = Vec::new();
    let mut marks = Vec::new();
    collect_card_content(
        value,
        &mut lines,
        &mut line_styles,
        &mut fact_sets,
        &mut column_rows,
        &mut action,
        &mut actions,
        &mut marks,
        0,
        false,
    );
    if lines.is_empty() && fact_sets.is_empty() && title.is_none() && action.is_none() && actions.is_empty()
    {
        push_card_line(
            &mut lines,
            &mut line_styles,
            "A prévia deste conteúdo não está disponível.",
            CardLineStyle::default(),
        );
    }
    CardFallback {
        kind: kind.to_owned(),
        title,
        lines,
        line_styles,
        fact_sets,
        column_rows,
        action,
        actions,
        marks,
    }
}

fn parse_raw_card(text: &str) -> Option<CardFallback> {
    let kind = card_kind(text)?;
    let payload = parse_card_json(text)?;
    Some(card_from_json(&payload, kind))
}

fn card_string<'a>(value: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
        .filter(|value| !value.trim().is_empty())
}

fn push_card_line(
    lines: &mut Vec<String>,
    line_styles: &mut Vec<CardLineStyle>,
    value: &str,
    style: CardLineStyle,
) {
    const MAX_CARD_LINES: usize = 24;
    let Some(value) = card_text(value) else {
        return;
    };
    if let Some(index) = lines.iter().position(|line| line == &value) {
        if let Some(existing) = line_styles.get_mut(index) {
            *existing = existing.merge(style);
        }
    } else if lines.len() < MAX_CARD_LINES {
        lines.push(value);
        line_styles.push(style);
    }
}

fn card_line_style(value: &serde_json::Value) -> CardLineStyle {
    let emphasis = match card_string(value, &["size"])
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("large" | "extralarge") => CardLineEmphasis::Heading,
        _ if card_string(value, &["weight"])
            .is_some_and(|weight| weight.eq_ignore_ascii_case("bolder")) =>
        {
            CardLineEmphasis::Strong
        }
        _ => CardLineEmphasis::Body,
    };
    CardLineStyle {
        emphasis,
        subtle: value
            .get("isSubtle")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    }
}

fn mark_kind(hint: &str) -> CardMarkKind {
    let hint = hint.to_ascii_lowercase();
    if hint.contains("bug")
        || hint.contains("issue")
        || hint.contains("story")
        || hint.contains("epic")
        || hint.contains("task")
    {
        CardMarkKind::Issue
    } else if hint.contains("jira") {
        CardMarkKind::Brand
    } else {
        CardMarkKind::Issue
    }
}

fn column_mark_hint(column: &serde_json::Value) -> Option<CardMarkKind> {
    column
        .get("items")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .find_map(|item| {
            if !card_string(item, &["type"]).is_some_and(|kind| kind.eq_ignore_ascii_case("Image"))
            {
                return None;
            }
            let hint = format!(
                "{} {}",
                card_string(item, &["altText", "alt"]).unwrap_or(""),
                card_string(item, &["url"]).unwrap_or("")
            );
            Some(mark_kind(&hint))
        })
}

fn collect_column_set(
    object: &serde_json::Map<String, serde_json::Value>,
    lines: &mut Vec<String>,
    line_styles: &mut Vec<CardLineStyle>,
    fact_sets: &mut Vec<CardFactSet>,
    column_rows: &mut Vec<CardColumnRow>,
    action: &mut Option<CardAction>,
    actions: &mut Vec<CardAction>,
    marks: &mut Vec<CardMark>,
    depth: usize,
) {
    let Some(columns) = object.get("columns").and_then(serde_json::Value::as_array) else {
        return;
    };
    let mut ranges = Vec::new();
    let mut pending_mark = None;
    for column in columns.iter().take(8) {
        let own_mark = column_mark_hint(column);
        let start = lines.len();
        if let Some(items) = column.get("items") {
            collect_card_content(
                items,
                lines,
                line_styles,
                fact_sets,
                column_rows,
                action,
                actions,
                marks,
                depth + 1,
                false,
            );
        }
        if lines.len() > start {
            if let Some(mut kind) = own_mark.or(pending_mark.take())
                && marks.len() < 12
            {
                if card_line_visible(&lines[start]).eq_ignore_ascii_case("jira") {
                    kind = CardMarkKind::Brand;
                }
                marks.push(CardMark { line: start, kind });
            }
            ranges.push(start..lines.len());
        } else if pending_mark.is_none() {
            pending_mark = own_mark;
        }
    }
    if ranges.len() >= 2 && column_rows.len() < 12 {
        column_rows.push(CardColumnRow { columns: ranges });
    }
}

fn collect_card_content(
    value: &serde_json::Value,
    lines: &mut Vec<String>,
    line_styles: &mut Vec<CardLineStyle>,
    fact_sets: &mut Vec<CardFactSet>,
    column_rows: &mut Vec<CardColumnRow>,
    action: &mut Option<CardAction>,
    actions: &mut Vec<CardAction>,
    marks: &mut Vec<CardMark>,
    depth: usize,
    in_action_list: bool,
) {
    if depth > 16 || lines.len() >= 24 {
        return;
    }
    match value {
        serde_json::Value::Array(items) => {
            for item in items.iter().take(64) {
                collect_card_content(
                    item,
                    lines,
                    line_styles,
                    fact_sets,
                    column_rows,
                    action,
                    actions,
                    marks,
                    depth + 1,
                    in_action_list,
                );
            }
        }
        serde_json::Value::Object(object) => {
            if card_string(value, &["type"])
                .is_some_and(|kind| kind.eq_ignore_ascii_case("ColumnSet"))
            {
                collect_column_set(
                    object,
                    lines,
                    line_styles,
                    fact_sets,
                    column_rows,
                    action,
                    actions,
                    marks,
                    depth,
                );
                return;
            }
            if card_string(value, &["type"])
                .is_some_and(|kind| kind.eq_ignore_ascii_case("RichTextBlock"))
            {
                let text = object
                    .get("inlines")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|inline| card_string(inline, &["text"]))
                    .collect::<String>();
                push_card_line(lines, line_styles, &text, card_line_style(value));
                return;
            }
            let style = card_line_style(value);
            let is_fact_set = card_string(value, &["type"])
                .is_some_and(|kind| kind.eq_ignore_ascii_case("FactSet"));
            if is_fact_set && fact_sets.len() < 24 {
                let facts = object
                    .get("facts")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(24)
                    .filter_map(|fact| {
                        Some(CardFact {
                            title: card_string(fact, &["title"]).and_then(card_text)?,
                            value: card_string(fact, &["value"]).and_then(card_text)?,
                        })
                    })
                    .collect::<Vec<_>>();
                if !facts.is_empty() {
                    fact_sets.push(CardFactSet {
                        before_line: lines.len(),
                        facts,
                    });
                }
            }
            if !in_action_list {
                if let (Some(name), Some(value)) = (
                    card_string(value, &["name", "title"]),
                    card_string(value, &["value"]),
                ) {
                    push_card_line(
                        lines,
                        line_styles,
                        &format!("{name}: {value}"),
                        CardLineStyle::default(),
                    );
                }
                for key in ["activityTitle", "activitySubtitle", "text", "description"] {
                    if let Some(value) = card_string(value, &[key]) {
                        let line_style = if style.emphasis == CardLineEmphasis::Body
                            && matches!(key, "activityTitle" | "activitySubtitle")
                        {
                            CardLineStyle {
                                emphasis: CardLineEmphasis::Strong,
                                ..style
                            }
                        } else {
                            style
                        };
                        push_card_line(lines, line_styles, value, line_style);
                    }
                }
            }
            if in_action_list && actions.len() < 8 {
                if let Some(card_action) = parse_card_action(value) {
                    if action.is_none() {
                        *action = Some(card_action.clone());
                    }
                    actions.push(card_action);
                } else if let Some(label) =
                    card_string(value, &["title", "name", "text"]).and_then(card_text)
                {
                    let (positive, icon) = action_flags(value);
                    actions.push(CardAction {
                        label,
                        url: String::new(),
                        positive,
                        icon,
                    });
                }
            }
            for key in [
                "sections",
                "body",
                "items",
                "columns",
                "facts",
                "actions",
                "potentialAction",
            ] {
                if is_fact_set && key == "facts" {
                    continue;
                }
                if let Some(child) = object.get(key) {
                    let child_is_action_list = matches!(key, "actions" | "potentialAction");
                    collect_card_content(
                        child,
                        lines,
                        line_styles,
                        fact_sets,
                        column_rows,
                        action,
                        actions,
                        marks,
                        depth + 1,
                        child_is_action_list,
                    );
                }
            }
        }
        _ => {}
    }
}

fn parse_card_action(value: &serde_json::Value) -> Option<CardAction> {
    let action_type = card_string(value, &["type", "@type"])?;
    let adaptive_open_url = action_type.eq_ignore_ascii_case("Action.OpenUrl");
    let message_open_uri = action_type.eq_ignore_ascii_case("OpenUri");
    if !adaptive_open_url && !message_open_uri {
        return None;
    }

    let raw_url = value
        .get("url")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            if !message_open_uri {
                return None;
            }
            value
                .get("targets")
                .and_then(serde_json::Value::as_array)?
                .iter()
                .find(|target| {
                    target
                        .get("os")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|os| os.eq_ignore_ascii_case("default"))
                })?
                .get("uri")
                .and_then(serde_json::Value::as_str)
        })?;
    let url = valid_link(raw_url)?;
    if !url.starts_with("https://") {
        return None;
    }
    let label = card_string(value, &["title", "name", "text"])
        .and_then(card_text)
        .unwrap_or_else(|| "Abrir cartão".into());
    let (positive, icon) = action_flags(value);
    Some(CardAction {
        label,
        url,
        positive,
        icon,
    })
}

fn action_flags(value: &serde_json::Value) -> (bool, bool) {
    let positive = card_string(value, &["style"])
        .is_some_and(|style| style.eq_ignore_ascii_case("positive"));
    let icon = card_string(value, &["iconUrl"]).is_some();
    (positive, icon)
}

fn card_markdown_runs(text: &str) -> Vec<Run> {
    let mut runs = Vec::new();
    append_card_markdown(text, &Style::default(), &mut runs, 0);
    linkify(&mut runs);
    runs.retain(|run| !run.text.is_empty());
    runs
}

fn card_line_plain(line: &str) -> String {
    let runs = card_markdown_runs(line);
    if runs.is_empty() {
        return line.to_owned();
    }
    let mut plain = String::new();
    for run in &runs {
        plain.push_str(&run.text);
        if let Some(url) = run.style.link.as_deref()
            && run.text != url
        {
            plain.push_str(" (");
            plain.push_str(url);
            plain.push(')');
        }
    }
    plain
}

fn append_card_markdown(text: &str, style: &Style, runs: &mut Vec<Run>, depth: usize) {
    if depth > 8 {
        push_merged_run(runs, text, style);
        return;
    }
    let mut rest = text;
    while !rest.is_empty() {
        if let Some((label, url, consumed)) = take_card_markdown_link(rest) {
            push_merged_run(
                runs,
                &label,
                &Style {
                    link: Some(url),
                    ..style.clone()
                },
            );
            rest = &rest[consumed..];
            continue;
        }
        if let Some((inner, consumed, bold, italic)) = take_card_emphasis(rest) {
            let mut nested = style.clone();
            nested.bold |= bold;
            nested.italic |= italic;
            append_card_markdown(inner, &nested, runs, depth + 1);
            rest = &rest[consumed..];
            continue;
        }
        let split = rest
            .char_indices()
            .find_map(|(index, character)| {
                (index > 0 && matches!(character, '[' | '*' | '_')).then_some(index)
            })
            .unwrap_or(rest.len());
        if split == 0 {
            let character = rest.chars().next().expect("rest is not empty");
            let width = character.len_utf8();
            push_merged_run(runs, &rest[..width], style);
            rest = &rest[width..];
            continue;
        }
        push_merged_run(runs, &rest[..split], style);
        rest = &rest[split..];
    }
}

fn push_merged_run(runs: &mut Vec<Run>, text: &str, style: &Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = runs.last_mut().filter(|run| &run.style == style) {
        last.text.push_str(text);
    } else {
        runs.push(Run {
            text: text.to_owned(),
            style: style.clone(),
        });
    }
}

fn take_card_markdown_link(text: &str) -> Option<(String, String, usize)> {
    if !text.starts_with('[') {
        return None;
    }
    let mut depth = 0usize;
    let mut close = None;
    for (index, character) in text.char_indices() {
        match character {
            '[' => depth += 1,
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let rest = text.get(close + 1..)?;
    if !rest.starts_with('(') {
        return None;
    }
    let url_and_tail = rest.get(1..)?;
    let url_len = url_and_tail.find(')')?;
    let url = valid_link(url_and_tail.get(..url_len)?)?;
    if !url.starts_with("https://") {
        return None;
    }
    let label = text.get(1..close)?.trim();
    if label.is_empty() {
        return None;
    }
    Some((label.to_owned(), url, close + url_len + 3))
}

fn take_card_emphasis(text: &str) -> Option<(&str, usize, bool, bool)> {
    let (marker, bold, italic) = if text.starts_with("**") || text.starts_with("__") {
        (&text[..2], true, false)
    } else if text.starts_with('*') {
        ("*", false, true)
    } else {
        return None;
    };
    let body = text.get(marker.len()..)?;
    let end = body.find(marker)?;
    if end == 0 {
        return None;
    }
    let inner = body.get(..end)?;
    if inner.contains('\n') {
        return None;
    }
    Some((inner, marker.len() + end + marker.len(), bold, italic))
}

fn safe_element_text(element: ElementRef<'_>) -> String {
    fn collect(node: ElementRef<'_>, output: &mut String, depth: usize) {
        if depth > 64 {
            return;
        }
        for child in node.children() {
            if let Some(value) = child.value().as_text() {
                output.push_str(value);
                output.push(' ');
            } else if let Some(element) = ElementRef::wrap(child)
                && !matches!(
                    element.value().name(),
                    "script" | "style" | "iframe" | "object" | "svg" | "template"
                )
            {
                collect(element, output, depth + 1);
            }
        }
    }
    let mut output = String::new();
    collect(element, &mut output, 0);
    output
}

fn parse_card_attachment(element: ElementRef<'_>) -> Option<CardFallback> {
    let image_selector = scraper::Selector::parse("img[src]").expect("static image selector");
    if element.select(&image_selector).any(|image| crate::images::parse_inline_image_attributes(image.value()).is_some()) {
        return None;
    }

    let attributes = element
        .value()
        .attrs()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    let text = safe_element_text(element);
    let marker = card_kind(&attributes).or_else(|| card_kind(&text));
    if marker.is_none() && !text.trim().is_empty() {
        return None;
    }

    let payload = parse_card_json(&text);
    let kind = marker.unwrap_or("Anexo sem prévia");
    let mut card = payload
        .as_ref()
        .map(|payload| card_from_json(payload, kind))
        .unwrap_or_else(|| {
            let lines: Vec<_> = card_text(&text)
                .filter(|text| marker.is_some() && !text.starts_with('{'))
                .into_iter()
                .collect();
            CardFallback {
                kind: kind.to_owned(),
                title: None,
                line_styles: vec![CardLineStyle::default(); lines.len()],
                lines,
                fact_sets: Vec::new(),
                column_rows: Vec::new(),
                action: None,
                actions: Vec::new(),
                marks: Vec::new(),
            }
        });

    if card.action.is_none() {
        let link_selector = scraper::Selector::parse("a[href]").expect("static link selector");
        card.action = element.select(&link_selector).find_map(|link| {
            let url = link.value().attr("href").and_then(valid_link)?;
            if !url.starts_with("https://") {
                return None;
            }
            let label = card_text(&link.text().collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| "Abrir cartão".into());
            Some(CardAction {
                label,
                url,
                positive: false,
                icon: false,
            })
        });
    }
    if card.actions.is_empty()
        && let Some(action) = card.action.clone()
    {
        card.actions.push(action);
    }

    if card.lines.is_empty() && card.title.is_none() && card.action.is_none() && card.actions.is_empty()
    {
        push_card_line(
            &mut card.lines,
            &mut card.line_styles,
            "A prévia deste conteúdo não está disponível.",
            CardLineStyle::default(),
        );
    }
    Some(card)
}

fn table_cell_alignment(element: ElementRef<'_>) -> (CellAlignment, CellVerticalAlignment) {
    let mut horizontal = element
        .value()
        .attr("align")
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut vertical = element
        .value()
        .attr("valign")
        .unwrap_or("")
        .to_ascii_lowercase();
    if let Some(style) = element.value().attr("style") {
        for declaration in style.split(';') {
            let Some((key, value)) = declaration.split_once(':') else {
                continue;
            };
            match key.trim().to_ascii_lowercase().as_str() {
                "text-align" => horizontal = value.trim().to_ascii_lowercase(),
                "vertical-align" => vertical = value.trim().to_ascii_lowercase(),
                _ => {}
            }
        }
    }
    let horizontal = match horizontal.as_str() {
        "center" => CellAlignment::Center,
        "right" | "end" => CellAlignment::End,
        "justify" => CellAlignment::Justify,
        _ => CellAlignment::Start,
    };
    let vertical = match vertical.as_str() {
        "middle" | "center" => CellVerticalAlignment::Center,
        "bottom" => CellVerticalAlignment::Bottom,
        _ => CellVerticalAlignment::Top,
    };
    (horizontal, vertical)
}

fn percentage_width(value: &str) -> Option<u32> {
    let percentage = value.trim().strip_suffix('%')?.trim().parse::<f32>().ok()?;
    if !percentage.is_finite() || percentage <= 0.0 || percentage > 1_000.0 {
        return None;
    }
    Some((percentage * 100.0).round() as u32)
}

fn table_cell_width(element: ElementRef<'_>) -> Option<u32> {
    let mut width = element.value().attr("width").and_then(percentage_width);
    if let Some(style) = element.value().attr("style") {
        for declaration in style.split(';') {
            let Some((key, value)) = declaration.split_once(':') else {
                continue;
            };
            if key.trim().eq_ignore_ascii_case("width") {
                // A CSS width declaration overrides the legacy width attribute.
                // Unsupported units fall back to automatic table layout.
                width = percentage_width(value);
            }
        }
    }
    width
}

fn table_cell_column_span(element: ElementRef<'_>) -> u8 {
    const MAX_TABLE_COLUMNS: usize = 12;
    element
        .value()
        .attr("colspan")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|span| span.clamp(1, MAX_TABLE_COLUMNS) as u8)
        .unwrap_or(1)
}

fn table_column_count(table: &Table) -> usize {
    table
        .rows
        .iter()
        .map(|row| {
            row.cells
                .iter()
                .map(|cell| usize::from(cell.column_span.max(1)))
                .sum::<usize>()
        })
        .max()
        .unwrap_or_default()
        .min(12)
}

fn table_row_belongs_to_table(row: ElementRef<'_>, table: ElementRef<'_>) -> bool {
    let table_id = table.id();
    let mut ancestor = row.parent();
    while let Some(node) = ancestor {
        if node.id() == table_id {
            return true;
        }
        if ElementRef::wrap(node).is_some_and(|element| element.value().name() == "table") {
            return false;
        }
        ancestor = node.parent();
    }
    false
}

fn table_block_text_width(block: &Block) -> usize {
    if let Some(table) = &block.table {
        return table
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| {
                        cell.blocks
                            .iter()
                            .map(table_block_text_width)
                            .max()
                            .unwrap_or(1)
                    })
                    .sum()
            })
            .max()
            .unwrap_or(1);
    }
    if let Some(card) = &block.card {
        return card
            .title
            .iter()
            .chain(card.lines.iter())
            .chain(card.actions.iter().map(|action| &action.label))
            .map(|line| line.lines().map(str::chars).map(Iterator::count).max().unwrap_or(1))
            .max()
            .unwrap_or(1);
    }
    if block.media.is_some() {
        return 12;
    }
    block
        .runs
        .iter()
        .flat_map(|run| run.text.lines())
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(1)
}

fn table_auto_column_widths(table: &Table, columns: usize) -> Vec<u32> {
    const MIN_COLUMN_CHARS: usize = 18;
    const MAX_COLUMN_CHARS: usize = 80;
    let mut widths = vec![0usize; columns];
    for row in &table.rows {
        let mut column = 0usize;
        for cell in &row.cells {
            if column >= columns {
                break;
            }
            let span = usize::from(cell.column_span.max(1)).min(columns - column);
            let content_width = cell
                .blocks
                .iter()
                .map(table_block_text_width)
                .max()
                .unwrap_or(1)
                .max(1)
                .div_ceil(span);
            for offset in 0..span {
                widths[column + offset] = widths[column + offset].max(content_width);
            }
            column += span;
        }
    }
    widths
        .into_iter()
        .map(|width| width.clamp(MIN_COLUMN_CHARS, MAX_COLUMN_CHARS) as u32)
        .collect()
}

fn resolved_table_column_widths(table: &Table, columns: usize) -> Vec<u32> {
    let mut widths = vec![None; columns];
    for row in &table.rows {
        let mut column = 0usize;
        for cell in &row.cells {
            if column >= columns {
                break;
            }
            let span = usize::from(cell.column_span.max(1)).min(columns - column);
            if let Some(width) = cell.width {
                let base_width = width / span as u32;
                let remainder = width % span as u32;
                for offset in 0..span {
                    if widths[column + offset].is_none() {
                        widths[column + offset] =
                            Some(base_width + u32::from((offset as u32) < remainder));
                    }
                }
            }
            column += span;
        }
    }

    let specified_sum = widths
        .iter()
        .flatten()
        .map(|width| u64::from(*width))
        .sum::<u64>();
    let specified_count = widths.iter().filter(|width| width.is_some()).count();
    let missing_count = columns.saturating_sub(specified_count);
    let inferred = if missing_count == 0 {
        1
    } else if specified_sum < 10_000 {
        ((10_000 - specified_sum) / missing_count as u64).max(1) as u32
    } else if specified_count > 0 {
        (specified_sum / specified_count as u64).max(1) as u32
    } else {
        1
    };

    if specified_count == 0 {
        return table_auto_column_widths(table, columns);
    }
    widths
        .into_iter()
        .map(|width| width.unwrap_or(inferred).max(1))
        .collect()
}

fn table_column_fill_portions(widths: &[u32]) -> Vec<u16> {
    let max_width = widths.iter().copied().max().unwrap_or(1).max(1) as f64;
    widths
        .iter()
        .map(|width| ((f64::from(*width) / max_width * 1_000.0).round() as u16).max(1))
        .collect()
}

#[derive(Default)]
struct Builder<'a> {
    files: &'a [ost::api::ChatFileRef],
    inline_urls: Vec<String>,
    document: Document,
    current: Block,
    nodes: usize,
    inline_images: usize,
    exhausted: bool,
}
impl Builder<'_> {
    fn flush(&mut self) {
        if !self.current.pre {
            while self
                .current
                .runs
                .last()
                .is_some_and(|r| r.text.trim().is_empty())
            {
                self.current.runs.pop();
            }
            if let Some(last) = self.current.runs.last_mut() {
                last.text = last.text.trim_end_matches(char::is_whitespace).to_owned();
            }
        }
        if !self.current.runs.is_empty() {
            let runs = std::mem::take(&mut self.current.runs);
            self.document.blocks.push(Block {
                runs,
                ..self.current.clone()
            });
            self.current.marker = None;
        }
    }
    fn push(&mut self, value: &str, style: &Style) {
        let value = if self.current.pre {
            value.to_owned()
        } else {
            // HTML collapses whitespace, while explicit <br> remains a newline.
            let mut result = String::new();
            let mut whitespace = self
                .current
                .runs
                .last()
                .and_then(|run| run.text.chars().last())
                .is_none_or(char::is_whitespace);
            for ch in value.chars() {
                if ch.is_ascii_whitespace() {
                    if !whitespace {
                        result.push(' ');
                    }
                    whitespace = true;
                } else {
                    result.push(ch);
                    whitespace = false;
                }
            }
            result
        };
        if value.is_empty() {
            return;
        }
        if let Some(run) = self
            .current
            .runs
            .last_mut()
            .filter(|run| run.style == *style)
        {
            run.text.push_str(&value);
        } else {
            self.current.runs.push(Run {
                text: value,
                style: style.clone(),
            });
        }
    }
    fn children(&mut self, element: ElementRef<'_>, style: &Style, depth: usize) {
        for child in element.children() {
            self.nodes += 1;
            if depth > 64 || self.nodes > 30_000 {
                self.exhausted = true;
                return;
            }
            if let Some(text) = child.value().as_text() {
                self.push(text, style);
            } else if let Some(element) = ElementRef::wrap(child) {
                self.element(element, style, depth + 1);
            }
        }
    }
    fn element(&mut self, element: ElementRef<'_>, inherited: &Style, depth: usize) {
        let name = element.value().name();
        if matches!(
            name,
            "script"
                | "style"
                | "iframe"
                | "object"
                | "svg"
                | "head"
                | "template"
                | "card"
                | "uriobject"
                | "legacyquote"
        ) {
            return;
        }
        let style = styled(element, inherited);
        match name {
            "br" => {
                self.current.runs.push(Run {
                    text: "\n".into(),
                    style,
                });
            }
            "img" => {
                if let Some(media) = crate::images::parse_inline_image_attributes(element.value()) {
                    self.flush();
                    if self.inline_images < crate::images::MAX_IMAGES {
                        self.inline_images += 1;
                        let marker = self.current.marker.take();
                        self.document.blocks.push(Block {
                            media: Some(media), marker, quote: self.current.quote,
                            quote_group: self.current.quote_group, indent: self.current.indent,
                            ..Default::default()
                        });
                    } else if let Some(alt) = element.value().attr("alt") {
                        self.push(alt, &style);
                    }
                } else if let Some(alt) = element.value().attr("alt") {
                    self.push(alt, &style);
                }
            }
            "attachment" => {
                let mut matching = self.files.iter().filter(|file| element.value().attr("id").is_some_and(|id| !id.is_empty() && file.attachment_id.as_deref() == Some(id)));
                if let Some(file) = matching.next().filter(|_| matching.next().is_none()) {
                    self.flush();
                    let images = scraper::Selector::parse("img[src]").expect("static image selector");
                    if element.select(&images).any(|image| crate::images::parse_inline_image_attributes(image.value()).is_some()) {
                        self.children(element, &style, depth);
                    } else if self.inline_images < crate::images::MAX_IMAGES
                        && let Some(picture) = crate::images::file_preview(file)
                    {
                        if self.inline_urls.iter().any(|url| *url == picture.url) { return; }
                        self.inline_images += 1;
                        self.document.blocks.push(Block {
                            media: Some(picture), quote: self.current.quote,
                            quote_group: self.current.quote_group, indent: self.current.indent,
                            ..Default::default()
                        });
                    }
                } else if let Some(card) = parse_card_attachment(element) {
                    self.flush();
                    let marker = self.current.marker.take();
                    self.document.blocks.push(Block {
                        card: Some(card),
                        marker,
                        quote: self.current.quote,
                        quote_group: self.current.quote_group,
                        indent: self.current.indent,
                        ..Default::default()
                    });
                } else {
                    self.children(element, &style, depth);
                }
            }
            _ if matches!(name, "div" | "section")
                && element.value().attrs().any(|(key, value)| {
                    matches!(
                        key.to_ascii_lowercase().as_str(),
                        "contenttype" | "data-contenttype"
                    ) && card_kind(value).is_some()
                }) =>
            {
                if let Some(card) = parse_card_attachment(element) {
                    self.flush();
                    let marker = self.current.marker.take();
                    self.document.blocks.push(Block {
                        card: Some(card),
                        marker,
                        quote: self.current.quote,
                        quote_group: self.current.quote_group,
                        indent: self.current.indent,
                        ..Default::default()
                    });
                } else {
                    self.children(element, &style, depth);
                }
            }
            "table" => self.table(element, &style, depth),
            "ul" | "ol" => {
                self.flush();
                let old = self.current.clone();
                self.current.indent = old.indent.saturating_add(1);
                let ordered_style = (name == "ol").then(|| ordered_list_style(element));
                let mut number = element
                    .value()
                    .attr("start")
                    .and_then(|value| value.parse::<i64>().ok())
                    .unwrap_or(1);
                for child in element.child_elements() {
                    self.flush();
                    self.current.ordered_list_style = ordered_style;
                    self.current.marker = Some(if let Some(style) = ordered_style {
                        number = child
                            .value()
                            .attr("value")
                            .and_then(|value| value.parse::<i64>().ok())
                            .unwrap_or(number);
                        self.current.ordered_list_value = Some(number);
                        let marker = style.marker(number);
                        number = number.saturating_add(1);
                        marker
                    } else {
                        self.current.ordered_list_value = None;
                        "•".into()
                    });
                    self.children(child, &styled(child, &style), depth + 1);
                    self.flush();
                }
                self.current = old;
            }
            "blockquote" | "quote" | "pre" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                let old = self.current.clone();
                if matches!(name, "blockquote" | "quote") {
                    if self.current.quote == 0 {
                        self.current.quote_group = self.document.blocks.len() + 1;
                    }
                    self.current.quote = self.current.quote.saturating_add(1);
                }
                if name == "pre" {
                    self.current.pre = true;
                }
                if name.starts_with('h') {
                    self.current.heading = name[1..].parse().unwrap_or(0);
                }
                self.children(element, &style, depth);
                self.flush();
                self.current = old;
            }
            "p" | "div" | "section" | "tr" => {
                self.flush();
                let start = self.document.blocks.len();
                self.children(element, &style, depth);
                self.flush();
                // The editor serializes an intentional empty line as an empty
                // paragraph. Teams also uses an empty block with a single br.
                if matches!(name, "p" | "div") && self.document.blocks.len() == start
                    && text_paragraph(&self.current)
                    && element.children().all(|child| {
                        child.value().as_text().is_some_and(|text| text.trim().is_empty())
                            || ElementRef::wrap(child).is_some_and(|element| element.value().name() == "br")
                    })
                    && (name == "p" || element.child_elements().next().is_some())
                {
                    self.document.blocks.push(self.current.clone());
                }
            }
            "td" | "th" => {
                if !self.current.runs.is_empty() {
                    self.push(" | ", &style);
                }
                self.children(element, &style, depth);
            }
            "hr" => {
                self.flush();
            }
            _ => self.children(element, &style, depth),
        }
    }

    fn table(&mut self, element: ElementRef<'_>, style: &Style, depth: usize) {
        const MAX_ROWS: usize = 200;
        const MAX_COLUMNS: usize = 12;
        const MAX_CELLS: usize = 1024;
        self.flush();
        let rows_selector = scraper::Selector::parse("tr").expect("static table row selector");
        let mut table = Table::default();
        let mut cells_seen = 0usize;
        for row in element
            .select(&rows_selector)
            .filter(|row| table_row_belongs_to_table(*row, element))
        {
            if table.rows.len() >= MAX_ROWS {
                self.exhausted = true;
                return;
            }
            self.nodes = self.nodes.saturating_add(1);
            if self.nodes > 30_000 || depth > 64 {
                self.exhausted = true;
                return;
            }
            let mut table_row = TableRow::default();
            let mut logical_columns = 0usize;
            for cell in row
                .child_elements()
                .filter(|cell| matches!(cell.value().name(), "td" | "th"))
            {
                let column_span = table_cell_column_span(cell);
                if table_row.cells.len() >= MAX_COLUMNS
                    || logical_columns.saturating_add(usize::from(column_span)) > MAX_COLUMNS
                {
                    self.exhausted = true;
                    return;
                }
                logical_columns += usize::from(column_span);
                cells_seen = cells_seen.saturating_add(1);
                if cells_seen > MAX_CELLS {
                    self.exhausted = true;
                    return;
                }
                self.nodes = self.nodes.saturating_add(1);
                let mut cell_builder = Builder { files: self.files, inline_urls: self.inline_urls.clone(), ..Default::default() };
                let cell_style = styled(cell, style);
                cell_builder.children(cell, &cell_style, depth + 2);
                cell_builder.flush();
                self.nodes = self.nodes.saturating_add(cell_builder.nodes);
                if self.nodes > 30_000 || cell_builder.exhausted {
                    self.exhausted = true;
                    return;
                }
                for block in &mut cell_builder.document.blocks {
                    linkify_nested(block);
                }
                let (alignment, vertical_alignment) = table_cell_alignment(cell);
                table_row.cells.push(TableCell {
                    blocks: cell_builder.document.blocks,
                    header: cell.value().name() == "th",
                    alignment,
                    vertical_alignment,
                    column_span,
                    width: table_cell_width(cell),
                    color: cell_style.color,
                    background: cell_style
                        .background
                        .or_else(|| cell.value().attr("bgcolor").and_then(css_color)),
                });
            }
            if !table_row.cells.is_empty() {
                table.rows.push(table_row);
            }
        }
        if !table.rows.is_empty() {
            self.document.blocks.push(Block {
                table: Some(table),
                quote: self.current.quote,
                quote_group: self.current.quote_group,
                indent: self.current.indent,
                ..Default::default()
            });
        }
    }
}

fn linkify_nested(block: &mut Block) {
    linkify(&mut block.runs);
    if let Some(table) = &mut block.table {
        for row in &mut table.rows {
            for cell in &mut row.cells {
                for block in &mut cell.blocks {
                    linkify_nested(block);
                }
            }
        }
    }
}

fn block_contains_inline_image(block: &Block) -> bool {
    block.media.is_some()
        || block.table.as_ref().is_some_and(|table| {
            table.rows.iter().any(|row| {
                row.cells
                    .iter()
                    .any(|cell| cell.blocks.iter().any(block_contains_inline_image))
            })
        })
}

fn limit_inline_images(document: &mut Document, remaining: &mut usize) {
    document.blocks.retain_mut(|block| {
        if block.media.is_some() {
            if *remaining == 0 {
                return false;
            }
            *remaining -= 1;
        }
        if let Some(table) = &mut block.table {
            for row in &mut table.rows {
                for cell in &mut row.cells {
                    let mut nested = Document {
                        blocks: std::mem::take(&mut cell.blocks),
                    };
                    limit_inline_images(&mut nested, remaining);
                    cell.blocks = nested.blocks;
                }
            }
        }
        true
    });
}

fn inline_image_html(media: &InlineImage) -> String {
    if !crate::images::valid_media_url(&media.url) {
        return "[Imagem]".into();
    }
    let dimensions = if media.width > 0 && media.height > 0 {
        format!(" width=\"{}\" height=\"{}\"", media.width, media.height)
    } else {
        String::new()
    };
    format!(
        "<img src=\"{}\" itemtype=\"http://schema.skype.com/AMSImage\"{} alt=\"Imagem\" />",
        escape_html(&media.url),
        dimensions
    )
}

#[derive(Default)]
struct ComposerPasteBuilder {
    text: String,
    marks: Vec<(usize, usize, InlineMark)>,
    links: Vec<(usize, usize, String)>,
    sizes: Vec<(usize, usize, u16)>,
    colors: Vec<(usize, usize, u32)>,
    highlights: Vec<(usize, usize, u32)>,
}

#[derive(Clone, Copy)]
enum PasteEndpoint {
    MarkStart(usize),
    MarkEnd(usize),
    LinkStart(usize),
    LinkEnd(usize),
    SizeStart(usize),
    SizeEnd(usize),
    ColorStart(usize),
    ColorEnd(usize),
    HighlightStart(usize),
    HighlightEnd(usize),
}

impl ComposerPasteBuilder {
    fn append_runs(&mut self, runs: &[Run]) {
        for run in runs {
            if run.text.is_empty() {
                continue;
            }
            let start = self.text.len();
            self.text.push_str(&run.text);
            let end = self.text.len();
            for (enabled, mark) in [
                (run.style.bold, InlineMark::Bold),
                (run.style.italic, InlineMark::Italic),
                (run.style.underline, InlineMark::Underline),
                (run.style.strike, InlineMark::Strike),
                (run.style.code, InlineMark::Code),
            ] {
                if enabled {
                    self.marks.push((start, end, mark));
                }
            }
            if let Some(url) = run.style.link.as_deref().and_then(valid_link) {
                self.links.push((start, end, url));
            }
            if run.style.size != 100 {
                self.sizes.push((start, end, run.style.size));
            }
            if let Some(color) = run.style.color {
                self.colors.push((start, end, color));
            }
            if let Some(color) = run.style.background {
                self.highlights.push((start, end, color));
            }
        }
    }

    fn append_table(&mut self, table: &Table) {
        for (row_index, row) in table.rows.iter().enumerate() {
            if row_index > 0 {
                self.text.push('\n');
            }
            let mut column = 0usize;
            for cell in &row.cells {
                if column > 0 {
                    self.text.push('\t');
                }
                for (block_index, block) in cell.blocks.iter().enumerate() {
                    if block_index > 0 {
                        self.text.push(' ');
                    }
                    self.append_runs(&block.runs);
                    if block.media.is_some() {
                        self.text.push_str("[Imagem]");
                    }
                }
                let span = usize::from(cell.column_span.max(1));
                for _ in 1..span {
                    self.text.push('\t');
                }
                column = column.saturating_add(span);
            }
        }
    }

    fn finish(self) -> ComposerPaste {
        use unicode_segmentation::UnicodeSegmentation;

        let mut endpoints = Vec::with_capacity(
            (self.marks.len()
                + self.links.len()
                + self.sizes.len()
                + self.colors.len()
                + self.highlights.len())
                * 2,
        );
        for (index, (start, end, _)) in self.marks.iter().enumerate() {
            endpoints.push((*start, PasteEndpoint::MarkStart(index)));
            endpoints.push((*end, PasteEndpoint::MarkEnd(index)));
        }
        for (index, (start, end, _)) in self.links.iter().enumerate() {
            endpoints.push((*start, PasteEndpoint::LinkStart(index)));
            endpoints.push((*end, PasteEndpoint::LinkEnd(index)));
        }
        for (index, (start, end, _)) in self.sizes.iter().enumerate() {
            endpoints.push((*start, PasteEndpoint::SizeStart(index)));
            endpoints.push((*end, PasteEndpoint::SizeEnd(index)));
        }
        for (index, (start, end, _)) in self.colors.iter().enumerate() {
            endpoints.push((*start, PasteEndpoint::ColorStart(index)));
            endpoints.push((*end, PasteEndpoint::ColorEnd(index)));
        }
        for (index, (start, end, _)) in self.highlights.iter().enumerate() {
            endpoints.push((*start, PasteEndpoint::HighlightStart(index)));
            endpoints.push((*end, PasteEndpoint::HighlightEnd(index)));
        }
        endpoints.sort_unstable_by_key(|(offset, _)| *offset);
        let mut mark_ranges = vec![(0, 0); self.marks.len()];
        let mut link_ranges = vec![(0, 0); self.links.len()];
        let mut size_ranges = vec![(0, 0); self.sizes.len()];
        let mut color_ranges = vec![(0, 0); self.colors.len()];
        let mut highlight_ranges = vec![(0, 0); self.highlights.len()];
        let mut endpoint_index = 0;
        let mut grapheme_index = 0;
        for (byte_start, grapheme) in self.text.grapheme_indices(true) {
            let byte_end = byte_start + grapheme.len();
            while endpoint_index < endpoints.len() && endpoints[endpoint_index].0 <= byte_start {
                let endpoint = endpoints[endpoint_index].1;
                set_paste_endpoint(
                    endpoint,
                    grapheme_index,
                    &mut mark_ranges,
                    &mut link_ranges,
                    &mut size_ranges,
                    &mut color_ranges,
                    &mut highlight_ranges,
                );
                endpoint_index += 1;
            }
            while endpoint_index < endpoints.len() && endpoints[endpoint_index].0 < byte_end {
                let endpoint = endpoints[endpoint_index].1;
                let boundary = match endpoint {
                    PasteEndpoint::MarkStart(_)
                    | PasteEndpoint::LinkStart(_)
                    | PasteEndpoint::SizeStart(_)
                    | PasteEndpoint::ColorStart(_)
                    | PasteEndpoint::HighlightStart(_) => grapheme_index,
                    PasteEndpoint::MarkEnd(_)
                    | PasteEndpoint::LinkEnd(_)
                    | PasteEndpoint::SizeEnd(_)
                    | PasteEndpoint::ColorEnd(_)
                    | PasteEndpoint::HighlightEnd(_) => grapheme_index + 1,
                };
                set_paste_endpoint(
                    endpoint,
                    boundary,
                    &mut mark_ranges,
                    &mut link_ranges,
                    &mut size_ranges,
                    &mut color_ranges,
                    &mut highlight_ranges,
                );
                endpoint_index += 1;
            }
            grapheme_index += 1;
        }
        while endpoint_index < endpoints.len() {
            let endpoint = endpoints[endpoint_index].1;
            set_paste_endpoint(
                endpoint,
                grapheme_index,
                &mut mark_ranges,
                &mut link_ranges,
                &mut size_ranges,
                &mut color_ranges,
                &mut highlight_ranges,
            );
            endpoint_index += 1;
        }

        let mut formatting = ComposerFormatting::default();
        for ((_, _, mark), (start, end)) in self.marks.into_iter().zip(mark_ranges) {
            if start < end {
                formatting.marks.push(FormatRange { start, end, mark });
            }
        }
        for ((_, _, url), (start, end)) in self.links.into_iter().zip(link_ranges) {
            if start < end && valid_link(&url).is_some() {
                formatting.links.push(LinkRange { start, end, url });
            }
        }
        for ((_, _, percent), (start, end)) in self.sizes.into_iter().zip(size_ranges) {
            if start < end {
                formatting.sizes.push(SizeRange {
                    start,
                    end,
                    percent,
                });
            }
        }
        for ((_, _, color), (start, end)) in self.colors.into_iter().zip(color_ranges) {
            if start < end {
                formatting.colors.push(ColorRange { start, end, color });
            }
        }
        for ((_, _, color), (start, end)) in self.highlights.into_iter().zip(highlight_ranges) {
            if start < end {
                formatting.highlights.push(HighlightRange { start, end, color });
            }
        }
        normalize_marks(&mut formatting.marks);
        normalize_links(&mut formatting.links);
        normalize_sizes(&mut formatting.sizes);
        normalize_colors(&mut formatting.colors);
        normalize_colors(&mut formatting.highlights);
        ComposerPaste {
            text: self.text,
            formatting,
        }
    }
}

fn set_paste_endpoint(
    endpoint: PasteEndpoint,
    grapheme_index: usize,
    marks: &mut [(usize, usize)],
    links: &mut [(usize, usize)],
    sizes: &mut [(usize, usize)],
    colors: &mut [(usize, usize)],
    highlights: &mut [(usize, usize)],
) {
    match endpoint {
        PasteEndpoint::MarkStart(index) => marks[index].0 = grapheme_index,
        PasteEndpoint::MarkEnd(index) => marks[index].1 = grapheme_index,
        PasteEndpoint::LinkStart(index) => links[index].0 = grapheme_index,
        PasteEndpoint::LinkEnd(index) => links[index].1 = grapheme_index,
        PasteEndpoint::SizeStart(index) => sizes[index].0 = grapheme_index,
        PasteEndpoint::SizeEnd(index) => sizes[index].1 = grapheme_index,
        PasteEndpoint::ColorStart(index) => colors[index].0 = grapheme_index,
        PasteEndpoint::ColorEnd(index) => colors[index].1 = grapheme_index,
        PasteEndpoint::HighlightStart(index) => highlights[index].0 = grapheme_index,
        PasteEndpoint::HighlightEnd(index) => highlights[index].1 = grapheme_index,
    }
}

impl Document {
    pub fn from_clipboard_html(html: &str) -> Option<ComposerPaste> {
        let html = crate::images::normalize_clipboard_html(html.to_owned())?;
        let document = Self::from_html(&html)?;
        Some(document.to_composer_paste())
    }

    pub fn to_composer_paste(&self) -> ComposerPaste {
        let mut builder = ComposerPasteBuilder::default();
        for (block_index, block) in self.blocks.iter().enumerate() {
            if block_index > 0 {
                builder.text.push('\n');
            }
            if block.quote > 0 {
                for _ in 0..block.quote.min(4) {
                    builder.text.push_str("> ");
                }
            }
            if let Some(marker) = &block.marker {
                builder.text.push_str(marker);
                builder.text.push(' ');
            }
            let content_start = builder.text.len();
            if let Some(table) = &block.table {
                builder.append_table(table);
            } else if let Some(card) = &block.card {
                if let Some(title) = &card.title {
                    builder.text.push_str(title);
                } else {
                    builder.text.push_str(&card.kind);
                }
                for index in 0..=card.lines.len() {
                    for set in card.fact_sets.iter().filter(|set| set.before_line == index) {
                        for fact in &set.facts {
                            if !builder.text.ends_with('\n') {
                                builder.text.push('\n');
                            }
                            builder
                                .text
                                .push_str(&format!("{}: {}", fact.title, fact.value));
                        }
                    }
                    if let Some(line) = card.lines.get(index) {
                        if !builder.text.ends_with('\n') {
                            builder.text.push('\n');
                        }
                        builder.text.push_str(&card_line_plain(line));
                    }
                }
                for action in &card.actions {
                    if !builder.text.ends_with('\n') {
                        builder.text.push('\n');
                    }
                    builder.text.push_str(&action.label);
                    if valid_link(&action.url).is_some() {
                        builder.text.push_str(" (");
                        let start = builder.text.len();
                        builder.text.push_str(&action.url);
                        let end = builder.text.len();
                        builder.links.push((start, end, action.url.clone()));
                        builder.text.push(')');
                    }
                }
            } else if block.media.is_some() {
                builder.text.push_str("[Imagem]");
            } else {
                builder.append_runs(&block.runs);
            }
            if block.pre || block.heading > 0 {
                let end = builder.text.len();
                if content_start < end {
                    let mark = if block.pre {
                        InlineMark::Code
                    } else {
                        InlineMark::Bold
                    };
                    builder.marks.push((content_start, end, mark));
                }
            }
        }
        builder.finish()
    }

    pub fn from_html(html: &str) -> Option<Self> {
        Self::from_html_with_files(html, &[])
    }

    pub fn from_html_with_files(html: &str, files: &[ost::api::ChatFileRef]) -> Option<Self> {
        if html.len() > 2 * 1024 * 1024 {
            return None;
        }
        if let Some(card) = parse_raw_card(html) {
            return Some(Self {
                blocks: vec![Block {
                    card: Some(card),
                    ..Default::default()
                }],
            });
        }
        // Close the actual anchor before HTML5 adoption-agency repair can clone
        // it into later paragraphs. Keep the escaped token visible as received.
        let repaired;
        let html = if html.contains("<a ") && html.contains("&lt;/a&gt;") {
            repaired = html.replace("&lt;/a&gt;", "&lt;/a&gt;</a>");
            repaired.as_str()
        } else { html };
        let html = Html::parse_fragment(html);
        if files.is_empty() && html.root_element().child_elements().next().is_none() {
            return Some(Self::plain(&html.root_element().text().collect::<String>()));
        }
        let images = scraper::Selector::parse("img[src]").expect("static image selector");
        let inline_urls = html.select(&images).filter_map(|image| crate::images::parse_inline_image_attributes(image.value()))
            .take(crate::images::MAX_IMAGES).map(|image| image.url).collect();
        let mut builder = Builder { files, inline_urls, ..Default::default() };
        builder.children(html.root_element(), &Style::default(), 0);
        builder.flush();
        if builder.exhausted {
            return None;
        }
        for block in &mut builder.document.blocks {
            linkify_nested(block);
        }
        let mut image_budget = crate::images::MAX_IMAGES;
        limit_inline_images(&mut builder.document, &mut image_budget);
        // Native files can be carried only in properties.files, with no HTML
        // attachment marker. Their metadata order follows the caption.
        let mut represented = builder.document.inline_images();
        for file in files.iter().take(256) {
            if image_budget == 0 { break; }
            let Some(id) = file.attachment_id.as_deref().filter(|id| !id.is_empty()) else { continue; };
            if files.iter().filter(|other| other.attachment_id.as_deref() == Some(id)).take(2).count() != 1 { continue; }
            let Some(image) = crate::images::file_preview(file) else { continue; };
            if represented.iter().any(|have| have.url == image.url) { continue; }
            represented.push(image.clone());
            builder.document.blocks.push(Block { media: Some(image), ..Default::default() });
            image_budget -= 1;
        }
        builder.document.blocks.iter().any(|block| !blank_line(block)).then_some(builder.document)
    }
    pub fn plain(value: &str) -> Self {
        let mut runs = vec![Run {
            text: value.into(),
            style: Style::default(),
        }];
        linkify(&mut runs);
        Self {
            blocks: vec![Block {
                runs,
                ..Default::default()
            }],
        }
    }

    /// Clone the native document and highlight query matches in text runs.
    /// Original run styles and links are retained; table cells are traversed
    /// recursively. Non-text attachment/card labels remain unchanged.
    pub fn with_search_highlights(&self, query: &str) -> Self {
        let terms = search_terms(query);
        if terms.is_empty() {
            return self.clone();
        }
        let mut highlighted = self.clone();
        for block in &mut highlighted.blocks {
            highlight_block(block, &terms);
        }
        highlighted
    }

    pub fn clipboard_payload(&self) -> ClipboardPayload {
        let mut text_blocks = Vec::with_capacity(self.blocks.len());
        let mut html_blocks = Vec::with_capacity(self.blocks.len());
        for block in &self.blocks {
            if let Some(media) = &block.media {
                text_blocks.push("[Imagem]".into());
                html_blocks.push(format!("<p>{}</p>", inline_image_html(media)));
                continue;
            }
            if let Some(card) = &block.card {
                text_blocks.push(card_plain(card));
                html_blocks.push(card_html(card));
                continue;
            }
            if let Some(table) = &block.table {
                text_blocks.push(table_plain(table));
                html_blocks.push(table_html(table));
                continue;
            }
            let mut plain = block
                .marker
                .as_ref()
                .map(|marker| format!("{marker} "))
                .unwrap_or_default();
            let mut rich = block
                .marker
                .as_ref()
                .map(|marker| format!("{} ", escape_html(marker)))
                .unwrap_or_default();
            for run in &block.runs {
                plain.push_str(&plain_run(run));
                rich.push_str(&html_run(run, block.pre));
            }
            if block.quote > 0 {
                plain = plain
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            let margin = usize::from(block.indent).saturating_mul(24);
            let body = if block.pre {
                format!("<pre style=\"margin:0 0 8px 0; margin-left:{margin}px\">{rich}</pre>")
            } else if (1..=6).contains(&block.heading) {
                format!(
                    "<h{} style=\"margin:0 0 8px 0; margin-left:{margin}px\">{rich}</h{}>",
                    block.heading, block.heading
                )
            } else {
                format!("<p style=\"margin:0 0 8px 0; margin-left:{margin}px\">{rich}</p>")
            };
            html_blocks.push(if block.quote > 0 {
                format!("<blockquote>{body}</blockquote>")
            } else {
                body
            });
            text_blocks.push(plain);
        }
        ClipboardPayload {
            text: text_blocks.join("\n"),
            html: format!("<html><body>{}</body></html>", html_blocks.join("")),
        }
    }

    pub fn standalone_text_card_width(&self) -> Option<f32> {
        match self.blocks.as_slice() {
            [block] if block.quote == 0 => block.card.as_ref().and_then(text_card_width),
            _ => None,
        }
    }

    pub fn combines_text_and_card(&self) -> bool {
        self.blocks.iter().any(|block| block.quote == 0 && block.card.is_some())
            && self.blocks.iter().any(|block| {
                block.quote == 0 && block.runs.iter().any(|run| !run.text.trim().is_empty())
            })
    }

    pub fn inline_images(&self) -> Vec<InlineImage> {
        fn collect(block: &Block, images: &mut Vec<InlineImage>) {
            if let Some(image) = &block.media
                && images.len() < crate::images::MAX_IMAGES
                && !images.iter().any(|have| have.url == image.url)
            { images.push(image.clone()); }
            if let Some(table) = &block.table {
                for cell in table.rows.iter().flat_map(|row| &row.cells) {
                    for block in &cell.blocks { collect(block, images); }
                }
            }
        }
        let mut images = Vec::new();
        for block in &self.blocks { collect(block, &mut images); }
        images
    }

    pub fn contains_inline_images(&self) -> bool {
        self.blocks.iter().any(block_contains_inline_image)
    }

    pub fn estimated_heap_bytes(&self) -> usize {
        fn blocks_bytes(blocks: &[Block]) -> usize {
            std::mem::size_of_val(blocks) + blocks.iter().map(|block| {
                block.runs.iter().map(|run| std::mem::size_of::<Run>() + run.text.capacity()
                    + run.style.link.as_ref().map_or(0, String::capacity)).sum::<usize>()
                + block.media.as_ref().map_or(0, |image| image.url.capacity() + image.id.capacity())
                + block.table.as_ref().map_or(0, |table| table.rows.iter().flat_map(|row| &row.cells)
                    .map(|cell| blocks_bytes(&cell.blocks)).sum::<usize>())
                + block.card.as_ref().map_or(0, |card| card.kind.capacity() + card.title.as_ref().map_or(0, String::capacity)
                    + card.lines.iter().map(String::capacity).sum::<usize>())
            }).sum::<usize>()
        }
        blocks_bytes(&self.blocks)
    }

    /// Estimate wrapped text rows and inline image height without constructing
    /// clipboard strings or Iced widgets. Used only to keep the history window
    /// bounded; the visible rows still use normal Iced layout.
    pub fn estimated_layout(&self, columns: usize, image_width: f32) -> (usize, f32) {
        self.blocks.iter().fold((0, 0.0), |total, block| {
            let (lines, images) = estimated_block_layout(block, columns.max(1), image_width);
            (total.0.saturating_add(lines), total.1 + images)
        })
    }

    pub fn estimated_plain_lines(text: &str, columns: usize) -> usize {
        estimated_text_lines(std::iter::once(text), columns.max(1))
    }

    pub fn from_editor(value: &str, formatting: &ComposerFormatting) -> Self {
        use unicode_segmentation::UnicodeSegmentation;

        let graphemes = value.graphemes(true).collect::<Vec<_>>();
        let mut blocks = Vec::new();
        let mut line_start = 0;
        let mut line_index = 0;
        let mut quote_group: usize = 0;
        let mut previous_quote = false;
        loop {
            let line_end = graphemes[line_start..]
                .iter()
                .position(|grapheme| *grapheme == "\n")
                .map(|offset| line_start + offset)
                .unwrap_or(graphemes.len());
            let pre = formatting
                .code_blocks
                .iter()
                .any(|range| range.start <= line_index && line_index < range.end);
            let mut runs: Vec<Run> = Vec::new();
            for index in line_start..line_end {
                let mut style = Style {
                    code: pre,
                    size: formatting.size_at(index),
                    color: formatting.color_at(index),
                    background: formatting.highlight_at(index),
                    ..Style::default()
                };
                for range in formatting
                    .marks
                    .iter()
                    .filter(|range| range.start <= index && index < range.end)
                {
                    match range.mark {
                        InlineMark::Bold => style.bold = true,
                        InlineMark::Italic => style.italic = true,
                        InlineMark::Underline => style.underline = true,
                        InlineMark::Strike => style.strike = true,
                        InlineMark::Code => style.code = true,
                    }
                }
                if let Some((ordinal,range))=formatting.mentions.iter().enumerate().find(|(_,range)|range.start<=index&&index<range.end&&range.end<=graphemes.len()&&graphemes[range.start..range.end].concat()==range.entry.name) {
                    let target=if range.entry.everyone {crate::mentions::Target::Everyone} else {
                        crate::avatars::person_id(&range.entry.id).map(crate::mentions::Target::Person).unwrap_or_default()
                    };
                    if target!=crate::mentions::Target::Unknown {
                        style.mention=true;
                        style.mention_info=Some(crate::mentions::Mention{item_id:Some(ordinal.to_string()),target,own:false});
                    }
                }
                style.link = formatting
                    .links
                    .iter()
                    .find(|range| range.start <= index && index < range.end)
                    .and_then(|range| valid_link(&range.url));
                if let Some(last) = runs.last_mut().filter(|last| last.style == style) {
                    last.text.push_str(graphemes[index]);
                } else {
                    runs.push(Run {
                        text: graphemes[index].to_owned(),
                        style,
                    });
                }
            }
            let marker = (!pre)
                .then(|| {
                    formatting
                        .lists
                        .iter()
                        .find(|range| range.start <= line_index && line_index < range.end)
                        .map(|range| match range.kind {
                            ListKind::Unordered => "•".to_owned(),
                            ListKind::Ordered => format!("{}.", line_index - range.start + 1),
                        })
                })
                .flatten();
            let quoted = formatting
                .quote_blocks
                .iter()
                .any(|range| range.start <= line_index && line_index < range.end);
            if quoted && !previous_quote {
                quote_group = quote_group.saturating_add(1);
            }
            blocks.push(Block {
                runs,
                marker,
                pre,
                quote: u8::from(quoted),
                quote_group: quoted.then_some(quote_group).unwrap_or_default(),
                ..Default::default()
            });
            previous_quote = quoted;
            line_index += 1;
            if line_end == graphemes.len() {
                break;
            }
            line_start = line_end + 1;
        }
        let blocks = editor_tables(blocks, &formatting.tables);
        Self { blocks }
    }

    /// Serialize the editor's bounded native document to semantic HTML.
    /// Text and destinations are escaped and links are scheme-checked.
    pub fn to_html(&self) -> String {
        let mut html = String::new();
        let mut index = 0;
        while index < self.blocks.len() {
            let block = &self.blocks[index];
            if let Some(card) = &block.card {
                html.push_str(&card_html(card));
                index += 1;
                continue;
            }
            if let Some(table) = &block.table {
                html.push_str(&table_html(table));
                index += 1;
                continue;
            }
            if block.pre {
                let mut end = index + 1;
                while end < self.blocks.len() && self.blocks[end].pre {
                    end += 1;
                }
                html.push_str("<pre>");
                for (offset, code_line) in self.blocks[index..end].iter().enumerate() {
                    if offset > 0 {
                        html.push('\n');
                    }
                    html.push_str(&editor_runs(code_line));
                }
                html.push_str("</pre>");
                index = end;
                continue;
            }
            if block.quote > 0 {
                let end = quote_group_end(&self.blocks, index);
                html.push_str("<blockquote>");
                for quoted in &self.blocks[index..end] {
                    html.push_str(&editor_paragraph_body(quoted));
                }
                html.push_str("</blockquote>");
                index = end;
                continue;
            }
            let Some(marker) = block.marker.as_deref() else {
                html.push_str(&editor_paragraph(block));
                index += 1;
                continue;
            };
            let list_style = block
                .ordered_list_style
                .or_else(|| decimal_list_value(marker).map(|_| OrderedListStyle::Decimal));
            let mut end = index + 1;
            while end < self.blocks.len() {
                let next_block = &self.blocks[end];
                if next_block.marker.is_none() {
                    break;
                }
                let next_style = next_block.ordered_list_style.or_else(|| {
                    next_block
                        .marker
                        .as_deref()
                        .and_then(decimal_list_value)
                        .map(|_| OrderedListStyle::Decimal)
                });
                if next_style != list_style {
                    break;
                }
                end += 1;
            }
            let ordered = list_style.is_some();
            let start = ordered.then(|| {
                block
                    .ordered_list_value
                    .or_else(|| decimal_list_value(marker))
                    .unwrap_or(1)
            });
            if let Some(style) = list_style {
                html.push_str("<ol");
                if let Some(list_type) = style.html_type() {
                    html.push_str(&format!(" type=\"{list_type}\""));
                }
                if let Some(start) = start {
                    html.push_str(&format!(" start=\"{start}\""));
                }
                html.push('>');
            } else {
                html.push_str("<ul>");
            }
            let mut expected_value = start.unwrap_or(1);
            for item in &self.blocks[index..end] {
                let item_value = item
                    .ordered_list_value
                    .or_else(|| item.marker.as_deref().and_then(decimal_list_value))
                    .unwrap_or(expected_value);
                if ordered && item_value != expected_value {
                    html.push_str(&format!("<li value=\"{item_value}\">"));
                } else {
                    html.push_str("<li>");
                }
                html.push_str(&editor_runs(item));
                html.push_str("</li>");
                expected_value = item_value.saturating_add(1);
            }
            html.push_str(if ordered { "</ol>" } else { "</ul>" });
            index = end;
        }
        html
    }
}

fn editor_tables(mut blocks: Vec<Block>, tables: &[TableRange]) -> Vec<Block> {
    if tables.is_empty() {
        return blocks;
    }
    let mut output = Vec::with_capacity(blocks.len());
    let mut index = 0;
    while index < blocks.len() {
        let Some(range) = tables.iter().find(|range| range.start == index) else {
            output.push(std::mem::take(&mut blocks[index]));
            index += 1;
            continue;
        };
        let end = range.end.min(blocks.len());
        let rows = blocks[index..end]
            .iter()
            .map(|block| editor_table_row(block, usize::from(range.columns)))
            .collect();
        output.push(Block {
            table: Some(Table { rows }),
            ..Default::default()
        });
        index = end;
    }
    output
}

fn editor_table_row(block: &Block, columns: usize) -> TableRow {
    use unicode_segmentation::UnicodeSegmentation;

    let mut cell_runs: Vec<Vec<Run>> = vec![Vec::new()];
    for run in &block.runs {
        let mut fragment_start = 0;
        for (byte, grapheme) in run.text.grapheme_indices(true) {
            if grapheme != "\t" {
                continue;
            }
            if fragment_start < byte {
                append_table_run(&mut cell_runs, run, fragment_start, byte);
            }
            cell_runs.push(Vec::new());
            fragment_start = byte + grapheme.len();
        }
        if fragment_start < run.text.len() {
            append_table_run(&mut cell_runs, run, fragment_start, run.text.len());
        }
    }
    while cell_runs.len() < columns.max(1) {
        cell_runs.push(Vec::new());
    }
    TableRow {
        cells: cell_runs
            .into_iter()
            .take(columns.max(1))
            .map(|runs| TableCell {
                blocks: vec![Block {
                    runs,
                    ..Default::default()
                }],
                column_span: 1,
                ..Default::default()
            })
            .collect(),
    }
}

fn append_table_run(cells: &mut [Vec<Run>], run: &Run, start: usize, end: usize) {
    let Some(cell) = cells.last_mut() else {
        return;
    };
    let text = &run.text[start..end];
    if let Some(last) = cell.last_mut().filter(|last| last.style == run.style) {
        last.text.push_str(text);
    } else {
        cell.push(Run {
            text: text.to_owned(),
            style: run.style.clone(),
        });
    }
}

fn decimal_list_value(marker: &str) -> Option<i64> {
    marker
        .strip_suffix('.')
        .and_then(|value| value.parse::<i64>().ok())
}

fn estimated_block_layout(block: &Block, columns: usize, image_width: f32) -> (usize, f32) {
    if let Some(media) = &block.media {
        return (0, estimated_image_height(media, image_width));
    }
    if let Some(card) = &block.card {
        let lines = card
            .title
            .iter()
            .chain(card.lines.iter())
            .chain(card.actions.iter().map(|action| &action.label))
            .map(|line| estimated_text_lines(std::iter::once(line.as_str()), columns))
            .sum();
        return (lines, 0.0);
    }
    if let Some(table) = &block.table {
        let mut rows = 0usize;
        let mut images = 0.0;
        for row in &table.rows {
            let mut row_lines = 0usize;
            for cell in &row.cells {
                let (lines, cell_images) =
                    cell.blocks.iter().fold((0usize, 0.0f32), |total, child| {
                        let (lines, images) = estimated_block_layout(child, columns, image_width);
                        (total.0.saturating_add(lines), total.1 + images)
                    });
                row_lines = row_lines.max(lines);
                images += cell_images;
            }
            rows = rows.saturating_add(row_lines.max(1));
        }
        return (rows, images);
    }
    (
        estimated_text_lines(block.runs.iter().map(|run| run.text.as_str()), columns),
        0.0,
    )
}

fn estimated_image_height(media: &InlineImage, max_width: f32) -> f32 {
    crate::images::display_size(
        media,
        0,
        0,
        max_width.min(crate::images::INLINE_MAX_WIDTH),
        crate::images::INLINE_MAX_HEIGHT,
    )
    .1
}

fn estimated_text_lines<'a>(texts: impl IntoIterator<Item = &'a str>, columns: usize) -> usize {
    use unicode_segmentation::UnicodeSegmentation;

    let columns = columns.max(1);
    let mut lines = 0usize;
    let mut line_width = 0usize;
    let mut ended_with_newline = false;
    for text in texts {
        for grapheme in text.graphemes(true) {
            if grapheme == "\n" {
                lines = lines.saturating_add(line_width.div_ceil(columns).max(1));
                line_width = 0;
                ended_with_newline = true;
            } else if grapheme != "\r" {
                let wide = grapheme.chars().next().is_some_and(|character| {
                    matches!(
                        character as u32,
                        0x1100..=0x11ff
                            | 0x2e80..=0xa4cf
                            | 0xac00..=0xd7af
                            | 0xf900..=0xfaff
                            | 0xfe30..=0xfe6f
                            | 0xff01..=0xff60
                            | 0x1f300..=0x1faff
                    )
                });
                line_width = line_width.saturating_add(if wide { 2 } else { 1 });
                ended_with_newline = false;
            }
        }
    }
    if line_width > 0 {
        lines = lines.saturating_add(line_width.div_ceil(columns));
    } else if ended_with_newline {
        lines = lines.saturating_add(1);
    }
    lines
}

fn editor_runs(block: &Block) -> String {
    block
        .runs
        .iter()
        .map(|run| html_run(run, block.pre))
        .collect()
}

fn editor_paragraph(block: &Block) -> String {
    let body = editor_paragraph_body(block);
    if block.quote > 0 {
        format!("<blockquote>{body}</blockquote>")
    } else {
        body
    }
}

fn editor_paragraph_body(block: &Block) -> String {
    let content = editor_runs(block);
    if block.pre {
        format!("<pre>{content}</pre>")
    } else if (1..=6).contains(&block.heading) {
        format!("<h{0}>{1}</h{0}>", block.heading, content)
    } else {
        format!("<p>{content}</p>")
    }
}

fn table_plain(table: &Table) -> String {
    table
        .rows
        .iter()
        .map(|row| {
            let mut line = String::new();
            let mut column = 0usize;
            for cell in &row.cells {
                if column > 0 {
                    line.push('\t');
                }
                line.push_str(
                    &cell
                        .blocks
                        .iter()
                        .map(plain_block)
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                let span = usize::from(cell.column_span.max(1));
                for _ in 1..span {
                    line.push('\t');
                }
                column = column.saturating_add(span);
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn card_plain(card: &CardFallback) -> String {
    let mut lines = vec![card.kind.clone()];
    if let Some(title) = &card.title {
        lines.push(title.clone());
    }
    for index in 0..=card.lines.len() {
        for set in card.fact_sets.iter().filter(|set| set.before_line == index) {
            lines.extend(
                set.facts
                    .iter()
                    .map(|fact| format!("{}: {}", fact.title, fact.value)),
            );
        }
        if let Some(line) = card.lines.get(index) {
            lines.push(card_line_plain(line));
        }
    }
    for action in &card.actions {
        if valid_link(&action.url).is_some() {
            lines.push(format!("{}: {}", action.label, action.url));
        } else {
            lines.push(action.label.clone());
        }
    }
    lines.join("\n")
}

fn card_html(card: &CardFallback) -> String {
    let mut html = format!(
        "<section><p><strong>{}</strong></p>",
        escape_html(&card.kind)
    );
    if let Some(title) = &card.title {
        html.push_str(&format!("<h3>{}</h3>", escape_html(title)));
    }
    for index in 0..=card.lines.len() {
        for set in card.fact_sets.iter().filter(|set| set.before_line == index) {
            html.push_str("<table><tbody>");
            for fact in &set.facts {
                html.push_str(&format!(
                    "<tr><th scope=\"row\">{}</th><td>{}</td></tr>",
                    escape_html(&fact.title),
                    escape_html(&fact.value)
                ));
            }
            html.push_str("</tbody></table>");
        }
        let Some(line) = card.lines.get(index) else {
            continue;
        };
        let escaped = card_markdown_runs(line)
            .iter()
            .map(|run| html_run(run, false))
            .collect::<String>();
        let escaped = if escaped.is_empty() {
            escape_html(line)
        } else {
            escaped
        };
        let style = card.line_styles.get(index).copied().unwrap_or_default();
        let line_html = match (style.emphasis, style.subtle) {
            (CardLineEmphasis::Body, false) => format!("<p>{escaped}</p>"),
            (CardLineEmphasis::Body, true) => format!("<p><small>{escaped}</small></p>"),
            (CardLineEmphasis::Strong, false) => {
                format!("<p><strong>{escaped}</strong></p>")
            }
            (CardLineEmphasis::Strong, true) => {
                format!("<p><strong><small>{escaped}</small></strong></p>")
            }
            (CardLineEmphasis::Heading, false) => format!("<h4>{escaped}</h4>"),
            (CardLineEmphasis::Heading, true) => {
                format!("<h4><small>{escaped}</small></h4>")
            }
        };
        html.push_str(&line_html);
    }
    for action in &card.actions {
        if valid_link(&action.url).is_some() {
            html.push_str(&format!(
                "<p><a href=\"{}\">{}</a></p>",
                escape_html(&action.url),
                escape_html(&action.label)
            ));
        } else {
            html.push_str(&format!("<p>{}</p>", escape_html(&action.label)));
        }
    }
    html.push_str("</section>");
    html
}

fn plain_block(block: &Block) -> String {
    if block.media.is_some() {
        return "[Imagem]".into();
    }
    if let Some(card) = &block.card {
        return card_plain(card);
    }
    if let Some(table) = &block.table {
        return table_plain(table);
    }
    let mut value = block
        .marker
        .as_ref()
        .map(|marker| format!("{marker} "))
        .unwrap_or_default();
    for run in &block.runs {
        value.push_str(&plain_run(run));
    }
    if block.quote > 0 {
        value = value
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    value
}

fn format_percentage_width(width: u32) -> String {
    if width % 100 == 0 {
        format!("{}%", width / 100)
    } else {
        format!("{}.{:02}%", width / 100, width % 100)
    }
}

fn table_html(table: &Table) -> String {
    let mut html = String::from("<table><tbody>");
    for row in &table.rows {
        html.push_str("<tr>");
        for cell in &row.cells {
            let (tag, close) = if cell.header {
                ("th", "</th>")
            } else {
                ("td", "</td>")
            };
            let mut styles = Vec::new();
            if cell.alignment != CellAlignment::Start {
                styles.push(format!("text-align:{}", cell.alignment.css()));
            }
            if cell.vertical_alignment != CellVerticalAlignment::Top {
                styles.push(format!("vertical-align:{}", cell.vertical_alignment.css()));
            }
            if let Some(width) = cell.width {
                styles.push(format!("width:{}", format_percentage_width(width)));
            }
            if let Some(color) = cell.color {
                styles.push(format!("color:#{color:06x}"));
            }
            if let Some(background) = cell.background {
                styles.push(format!("background-color:#{background:06x}"));
            }
            html.push('<');
            html.push_str(tag);
            if cell.column_span > 1 {
                html.push_str(&format!(" colspan=\"{}\"", cell.column_span));
            }
            if !styles.is_empty() {
                html.push_str(" style=\"");
                html.push_str(&styles.join(";"));
                html.push('"');
            }
            html.push('>');
            for block in &cell.blocks {
                html.push_str(&html_block(block));
            }
            html.push_str(close);
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table>");
    html
}

fn html_block(block: &Block) -> String {
    if let Some(media) = &block.media {
        return format!("<p>{}</p>", inline_image_html(media));
    }
    if let Some(card) = &block.card {
        return card_html(card);
    }
    if let Some(table) = &block.table {
        return table_html(table);
    }
    let mut rich = block
        .marker
        .as_ref()
        .map(|marker| format!("{} ", escape_html(marker)))
        .unwrap_or_default();
    for run in &block.runs {
        rich.push_str(&html_run(run, block.pre));
    }
    let body = if block.pre {
        format!("<pre>{rich}</pre>")
    } else if (1..=6).contains(&block.heading) {
        format!("<h{0}>{1}</h{0}>", block.heading, rich)
    } else {
        format!("<p>{rich}</p>")
    };
    if block.quote > 0 {
        format!("<blockquote>{body}</blockquote>")
    } else {
        body
    }
}

pub fn clipboard_payload(document: Option<&Document>, fallback: &str) -> ClipboardPayload {
    let plain;
    let document = if let Some(document) = document {
        document
    } else {
        plain = Document::plain(fallback);
        &plain
    };
    document.clipboard_payload()
}

fn plain_run(run: &Run) -> String {
    match run.style.link.as_deref() {
        Some(link) if run.text.trim() != link => format!("{} ({link})", run.text),
        _ => run.text.clone(),
    }
}

/// Parse case-insensitive terms shared by local search and rich-text marking.
pub(crate) fn search_terms(query: &str) -> Vec<String> {
    let lowercase = query.to_lowercase();
    lowercase
        .split_whitespace()
        .map(|term| term.trim_matches(|character: char| !character.is_alphanumeric()))
        .filter(|term| !term.is_empty())
        .map(str::to_owned)
        .collect()
}

fn highlight_block(block: &mut Block, terms: &[String]) {
    block.runs = block
        .runs
        .iter()
        .flat_map(|run| highlight_run(run, terms))
        .collect();
    if let Some(table) = &mut block.table {
        for row in &mut table.rows {
            for cell in &mut row.cells {
                for nested in &mut cell.blocks {
                    highlight_block(nested, terms);
                }
            }
        }
    }
}

fn highlight_run(run: &Run, terms: &[String]) -> Vec<Run> {
    use unicode_segmentation::UnicodeSegmentation;

    let mut lowered = String::new();
    let mut graphemes = Vec::new();
    for (original_start, grapheme) in run.text.grapheme_indices(true) {
        let lowered_start = lowered.len();
        lowered.extend(grapheme.chars().flat_map(char::to_lowercase));
        graphemes.push((
            lowered_start,
            lowered.len(),
            original_start,
            original_start + grapheme.len(),
        ));
    }
    if graphemes.is_empty() {
        return vec![run.clone()];
    }

    let mut ranges = Vec::new();
    for query in terms {
        let mut search_from = 0;
        while search_from < lowered.len() {
            let Some(relative) = lowered[search_from..].find(query) else {
                break;
            };
            let lowered_start = search_from + relative;
            let lowered_end = lowered_start + query.len();
            let Some(first) = graphemes
                .iter()
                .position(|(start, end, _, _)| *start <= lowered_start && lowered_start < *end)
            else {
                break;
            };
            let Some(last) = graphemes
                .iter()
                .position(|(start, end, _, _)| *start < lowered_end && lowered_end <= *end)
            else {
                break;
            };
            ranges.push((graphemes[first].2, graphemes[last].3));
            search_from = lowered_end.max(lowered_start + 1);
        }
    }

    if ranges.is_empty() {
        return vec![run.clone()];
    }
    ranges.sort_unstable_by_key(|(start, _)| *start);
    let mut merged_ranges: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some((_, previous_end)) = merged_ranges.last_mut()
            && start <= *previous_end
        {
            *previous_end = (*previous_end).max(end);
        } else {
            merged_ranges.push((start, end));
        }
    }
    let mut fragments = Vec::with_capacity(merged_ranges.len() * 2 + 1);
    let mut cursor = 0;
    for (start, end) in merged_ranges {
        if cursor < start {
            fragments.push(run_fragment(run, cursor, start, false));
        }
        fragments.push(run_fragment(run, start, end, true));
        cursor = end;
    }
    if cursor < run.text.len() {
        fragments.push(run_fragment(run, cursor, run.text.len(), false));
    }
    fragments
}

fn run_fragment(run: &Run, start: usize, end: usize, highlighted: bool) -> Run {
    let mut fragment = run.clone();
    fragment.text = run.text[start..end].to_owned();
    if highlighted {
        fragment.style.background = Some(0xffe082);
        fragment.style.color = Some(0x242424);
    }
    fragment
}

fn escape_html(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&#39;".to_owned(),
            _ => character.to_string(),
        })
        .collect()
}

fn html_run(run: &Run, preformatted: bool) -> String {
    let mut content = escape_html(&run.text);
    if !preformatted {
        content = content.replace('\n', "<br>");
    }
    let style = &run.style;
    if style.code {
        content = format!("<code>{content}</code>");
    }
    if style.strike {
        content = format!("<s>{content}</s>");
    }
    if style.underline {
        content = format!("<u>{content}</u>");
    }
    if style.italic {
        content = format!("<em>{content}</em>");
    }
    if style.bold {
        content = format!("<strong>{content}</strong>");
    }
    let mut declarations = Vec::new();
    if let Some(color) = style.color {
        declarations.push(format!("color:#{color:06x}"));
    }
    if let Some(background) = style.background {
        declarations.push(format!("background-color:#{background:06x}"));
    }
    if style.size != 100 {
        declarations.push(format!("font-size:{}%", style.size));
    }
    if style.mention {
        declarations.push("font-weight:600".to_owned());
    }
    if !declarations.is_empty() {
        content = format!(
            "<span style=\"{}\">{content}</span>",
            declarations.join(";")
        );
    }
    if let Some(link) = style.link.as_deref().and_then(valid_link) {
        content = format!("<a href=\"{}\">{content}</a>", escape_html(&link));
    }
    if let Some(mention)=&style.mention_info {
        if mention.target!=crate::mentions::Target::Unknown {
            if let Some(id)=mention.item_id.as_deref() {
                content=format!("<span itemtype=\"http://schema.skype.com/Mention\" itemid=\"{}\">{content}</span>",escape_html(id));
            }
        }
    }
    content
}
fn linkify(runs: &mut Vec<Run>) {
    let mut result = Vec::new();
    for run in runs.drain(..) {
        if run.style.link.is_some() || run.style.code || run.style.mention {
            result.push(run);
            continue;
        }
        for word in run.text.split_inclusive(char::is_whitespace) {
            let token = word
                .trim_end_matches(char::is_whitespace)
                .trim_end_matches(['.', ',', ';', '!', '?', ')', ']']);
            let link = if token.starts_with("https://") || token.starts_with("http://") {
                valid_link(token)
            } else {
                None
            };
            if let Some(link) = link {
                result.push(Run {
                    text: token.into(),
                    style: Style {
                        link: Some(link),
                        ..run.style.clone()
                    },
                });
                if word.len() > token.len() {
                    result.push(Run {
                        text: word[token.len()..].into(),
                        style: run.style.clone(),
                    });
                }
            } else if let Some(last) = result.last_mut().filter(|last| last.style == run.style) {
                last.text.push_str(word);
            } else {
                result.push(Run {
                    text: word.into(),
                    style: run.style.clone(),
                });
            }
        }
    }
    *runs = result;
}

fn luminance(color: iced::Color) -> f32 {
    let channel = |v: f32| {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}
fn contrast(a: iced::Color, b: iced::Color) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}
fn readable_color(
    color: Option<u32>,
    background: iced::Color,
    fallback: iced::Color,
) -> iced::Color {
    if let Some(color) = color.map(crate::appearance::rgb)
        && contrast(color, background) >= 4.5
    {
        return color;
    }
    if contrast(fallback, background) >= 4.5 {
        return fallback;
    }
    let black = crate::appearance::rgb(0x000000);
    let white = crate::appearance::rgb(0xffffff);
    if contrast(black, background) >= contrast(white, background) {
        black
    } else {
        white
    }
}

/// Readable message measure, capped on wide panes while preserving small-window gutters.
pub fn bubble_width(pane: f32, mine: bool, compact: bool) -> f32 {
    let available =
        (pane - if compact { 24.0 } else { 48.0 } - if mine { 0.0 } else { 42.0 }).max(100.0);
    if compact {
        available
    } else {
        available.min(crate::appearance::metrics::MESSAGE_BUBBLE_WIDTH)
    }
}

fn text_runs(block: &Block) -> &[Run] {
    if block.media.is_some() || block.card.is_some() || block.table.is_some() {
        &[]
    } else {
        &block.runs
    }
}

pub fn visible_block_texts(document: &Document) -> Vec<String> {
    document
        .blocks
        .iter()
        .map(|block| text_runs(block).iter().map(|run| run.text.as_str()).collect())
        .collect()
}

pub fn view_with_media(
    document: Option<&Document>,
    fallback: &str,
    prefs: &Preferences,
    theme: &Theme,
    mine: bool,
    media: &MediaCache,
    max_image_width: f32,
    interaction: Option<&crate::message_selection::Interaction<'_>>,
) -> Element<'static, AppMessage> {
    let plain;
    let document = if let Some(document) = document {
        document
    } else {
        plain = Document::plain(fallback);
        &plain
    };
    let visible_text = document.blocks.iter()
        .flat_map(|block| text_runs(block))
        .map(|run| run.text.as_str())
        .collect::<String>();
    let standalone_emoji = {
        use unicode_segmentation::UnicodeSegmentation;
        let trimmed = visible_text.trim();
        let mut graphemes = trimmed.graphemes(true);
        graphemes.next().is_some_and(typography::is_color_emoji) && graphemes.next().is_none()
    };
    let mut emoji_preferences;
    let prefs = if standalone_emoji {
        emoji_preferences = prefs.clone();
        emoji_preferences.message_size = (u32::from(prefs.message_size) * 3 / 2)
            .clamp(24, 48) as u16;
        &emoji_preferences
    } else {
        prefs
    };
    let interaction = interaction.copied().map(|mut interaction| {
        if interaction.is_dragging() {
            let mut indices = document.blocks.iter().enumerate()
                .filter(|(_, block)| text_runs(block).iter().any(|run| !run.text.is_empty()))
                .map(|(index, _)| index);
            interaction.text_edges = indices.next().map(|first| {
                (first, indices.next_back().unwrap_or(first))
            });
        }
        interaction
    });
    let mut content = column![];
    let mut index = 0;
    while index < document.blocks.len() {
        let block = &document.blocks[index];
        if index > 0 {
            let gap = message_block_gap(&document.blocks[index - 1], block, mine);
            if gap > 0.0 {
                content = content.push(space::vertical().height(gap));
            }
        }
        if blank_line(block) {
            content = content.push(space::vertical().height(f32::from(prefs.message_size) * prefs.line_height()));
            index += 1;
        } else if block.quote > 0 {
            let end = quote_group_end(&document.blocks, index);
            content = content.push(render_quote(
                &document.blocks[index..end],
                prefs,
                theme,
                mine,
                media,
                max_image_width,
                interaction.as_ref(),
                index,
            ));
            index = end;
        } else {
            content = content.push(render_block(
                block,
                prefs,
                theme,
                mine,
                false,
                media,
                max_image_width,
                None,
                None,
                interaction.as_ref(),
                index,
            ));
            index += 1;
        }
    }
    content.into()
}

impl Document {
    pub fn resolve_mentions(&mut self,metadata:Option<&serde_json::Value>,owner:&str){
        let entries=crate::mentions::metadata(metadata);
        fn walk(block:&mut Block,entries:&std::collections::HashMap<String,crate::mentions::Target>,owner:&str,quoted:bool){
            let quoted=quoted || block.quote>0;
            for run in &mut block.runs {if let Some(mention)=&mut run.style.mention_info{crate::mentions::resolve(mention,entries,owner,quoted);}}
            if let Some(table)=&mut block.table{for row in &mut table.rows{for cell in &mut row.cells{for block in &mut cell.blocks{walk(block,entries,owner,quoted);}}}}
        }
        for block in &mut self.blocks{walk(block,&entries,owner,false);}
    }
    pub fn mention_badge(&self)->crate::mentions::Badge{
        fn walk(block:&Block,output:&mut crate::mentions::Badge){
            if block.quote>0{return;}
            for run in &block.runs{if let Some(mention)=&run.style.mention_info{
                if mention.own{*output=crate::mentions::Badge::Person;}
                else if mention.target==crate::mentions::Target::Everyone && *output==crate::mentions::Badge::None{*output=crate::mentions::Badge::Everyone;}
            }}
            if let Some(table)=&block.table{for row in &table.rows{for cell in &row.cells{for block in &cell.blocks{walk(block,output);}}}}
        }
        let mut result=crate::mentions::Badge::None;for block in &self.blocks{walk(block,&mut result);}result
    }
    pub fn mentioned_person(&self,id:&str)->Option<String>{
        fn find(block:&Block,id:&str)->Option<String>{
            let mut found=false;let mut name=String::new();
            for run in &block.runs{
                let matches=run.style.mention_info.as_ref().is_some_and(|mention|matches!(&mention.target,crate::mentions::Target::Person(person) if person==id));
                if matches{found=true;name.push_str(&run.text);}else if found{break;}
            }
            if found{return Some(name.trim().chars().take(256).collect());}
            block.table.as_ref().and_then(|table|table.rows.iter().flat_map(|row|&row.cells).flat_map(|cell|&cell.blocks).find_map(|block|find(block,id)))
        }
        self.blocks.iter().find_map(|block|find(block,id))
    }
}

fn is_list_item(block: &Block) -> bool {
    block.marker.is_some() || block.runs.first().is_some_and(|run| {
        run.text.trim_start().starts_with("• ")
    })
}

fn text_paragraph(block: &Block) -> bool {
    block.quote == 0
        && !block.pre
        && block.heading == 0
        && block.marker.is_none()
        && block.media.is_none()
        && block.card.is_none()
        && block.table.is_none()
}

fn blank_line(block: &Block) -> bool {
    text_paragraph(block) && block.runs.is_empty()
}

fn plain_single_line(block: &Block) -> bool {
    text_paragraph(block)
        && block.runs.iter().all(|run| !run.text.contains('\n'))
        && block.runs.iter().any(|run| !run.text.trim().is_empty())
}

/// A typed list keeps one line rhythm: "1) DEMO-1", "12. item", or a short label ending in ":".
fn stacked_line(block: &Block) -> bool {
    if !plain_single_line(block) {
        return false;
    }
    let text: String = block.runs.iter().map(|run| run.text.as_str()).collect();
    let text = text.trim();
    if text.len() <= 80 && text.ends_with(':') {
        return true;
    }
    if text.starts_with("- ") || text.starts_with("* ") {
        return true;
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    (1..=3).contains(&digits)
        && text.as_bytes().get(digits).is_some_and(|mark| *mark == b')' || *mark == b'.')
}

fn contact_line(block: &Block) -> bool {
    !block.runs.is_empty() && block.quote == 0 && !block.pre && block.marker.is_none()
        && block.runs.iter().all(|run| run.text.trim().is_empty()
            || run.style.link.as_deref().is_some_and(|url| url.starts_with("mailto:")))
}

fn message_block_gap(previous: &Block, current: &Block, mine: bool) -> f32 {
    if previous.quote > 0 && current.quote > 0 {
        24.0
    } else if previous.quote > 0 {
        6.0
    } else if blank_line(previous) || blank_line(current)
        || mine && text_paragraph(previous) && text_paragraph(current)
    {
        0.0
    } else if contact_line(previous) && contact_line(current) {
        0.0
    } else if is_list_item(previous) && is_list_item(current) {
        0.0
    } else if is_list_item(current) {
        18.0
    } else if stacked_line(previous) && stacked_line(current) {
        0.0
    } else if plain_single_line(previous) && plain_single_line(current)
        && previous.runs.iter().rev().find(|run| {
            !run.text.chars().all(|ch| ch.is_whitespace() || matches!(ch, ',' | ';'))
        }).is_some_and(|run| run.style.mention)
    {
        0.0
    } else {
        12.0
    }
}

pub fn view_plain_with_search_highlights(
    value: &str,
    query: &str,
    size: f32,
    prefs: &Preferences,
    theme: &Theme,
    mine: bool,
) -> Element<'static, AppMessage> {
    let document = Document::plain(value).with_search_highlights(query);
    let Some(block) = document.blocks.first() else {
        return text(String::new()).into();
    };
    render_runs(
        &block.runs,
        size,
        prefs,
        theme,
        mine,
        false,
        false,
        None,
        None,
        None,
        0,
    )
}

/// Adjacent quotations are distinct even when there is no paragraph between them.
fn quote_group_end(blocks: &[Block], start: usize) -> usize {
    let group = blocks[start].quote_group;
    start + blocks[start..].iter()
        .take_while(|block| block.quote > 0 && block.quote_group == group)
        .count()
}

fn render_quote(
    blocks: &[Block],
    prefs: &Preferences,
    theme: &Theme,
    mine: bool,
    media: &MediaCache,
    max_image_width: f32,
    interaction: Option<&crate::message_selection::Interaction<'_>>,
    start_index: usize,
) -> Element<'static, AppMessage> {
    let mut quoted = column![].spacing(3);
    for (index, block) in blocks.iter().enumerate() {
        let is_author = index == 0 && blocks.len() > 1
            && !block.runs.is_empty()
            && block.runs.iter().all(|run| run.text.trim().is_empty() || run.style.bold);
        let paragraph = if is_author {
            // Author/date are source content: never manufacture missing metadata.
            author_line(
                block,
                f32::from(prefs.message_size) * 0.82,
                theme,
                interaction,
                start_index + index,
            )
        } else {
            render_block(
                block,
                prefs,
                theme,
                mine,
                false,
                media,
                max_image_width.min(520.0),
                None,
                None,
                interaction,
                start_index + index,
            )
        };
        quoted = quoted.push(paragraph);
    }
    container(stack![
        container(quoted).padding(iced::Padding { left: 13.0, ..Default::default() }),
        container(space::vertical()).width(3).height(Fill).style(quote_mark(0.0)),
    ])
        .padding([6, 8])
        .max_width(560)
        .style(quiet_frame(Tone::Surface, 4.0))
        .into()
}

fn render_block(
    block: &Block,
    prefs: &Preferences,
    theme: &Theme,
    mine: bool,
    force_bold: bool,
    media: &MediaCache,
    max_image_width: f32,
    alignment: Option<CellAlignment>,
    surface: Option<iced::Color>,
    interaction: Option<&crate::message_selection::Interaction<'_>>,
    block_index: usize,
) -> Element<'static, AppMessage> {
    let size = block_font_size(block, prefs);
    let mut rendered = if let Some(inline_image) = &block.media {
        inline_image_view(
            inline_image,
            media,
            max_image_width.min(crate::images::INLINE_MAX_WIDTH),
        )
    } else if let Some(card) = &block.card {
        render_card(card, prefs, theme, mine)
    } else if let Some(table) = &block.table {
        render_table(table, prefs, theme, mine, media, max_image_width)
    } else {
        let text = render_runs(
            &block.runs,
            size,
            prefs,
            theme,
            mine,
            block.heading > 0 || force_bold,
            block.pre,
            surface,
            alignment,
            interaction,
            block_index,
        );
        if block.pre {
            container(text)
                .padding(12)
                .style(ui_surface(Tone::Surface, true, 4.0))
                .into()
        } else {
            text
        }
    };
    if let Some(marker) = &block.marker {
        rendered = row![text_marker(marker, size), rendered].spacing(8).into();
    }
    if block.indent > 1 {
        rendered = container(rendered)
            .padding(iced::Padding {
                left: f32::from(block.indent - 1) * 16.0,
                ..Default::default()
            })
            .into();
    }
    rendered
}

fn block_font_size(block: &Block, prefs: &Preferences) -> f32 {
    f32::from(prefs.message_size)
        * match block.heading {
            1 => 1.6,
            2 => 1.35,
            3 => 1.2,
            _ => 1.0,
        }
}

const TABLE_CELL_PADDING: [f32; 2] = [4.0, 8.0];
const TABLE_BLOCK_SPACING: f32 = 2.0;

fn short_cell_line_width(block: &Block, bold: bool, prefs: &Preferences) -> Option<f32> {
    use iced::advanced::text::{self as layout_text, Paragraph as _};
    use iced::{Pixels, Size};

    const SHORT_LINE_CHARS: usize = 18;
    if block.pre || block.quote > 0 || block.marker.is_some() || block.indent > 1 {
        return None;
    }
    let characters = block.runs.iter().map(|run| run.text.chars().count()).sum::<usize>();
    if characters == 0 || characters > SHORT_LINE_CHARS
        || block.runs.iter().any(|run| run.text.contains('\n'))
    {
        return None;
    }

    let size = block_font_size(block, prefs);
    let spans: Vec<Span<'_, (), Font>> = block.runs.iter().flat_map(|run| {
        let font = message_run_font(&run.style, bold || block.heading > 0);
        let run_size = size * f32::from(run.style.size) / 100.0;
        typography::emoji_segments(&run.text).map(move |(segment, emoji)| {
            Span::new(segment)
                .font(if emoji { typography::EMOJI } else { font })
                .size(run_size)
        })
    }).collect();
    let paragraph = iced::advanced::graphics::text::Paragraph::with_spans(layout_text::Text {
        content: &spans,
        bounds: Size::INFINITE,
        size: Pixels(size),
        line_height: layout_text::LineHeight::Relative(prefs.line_height()),
        font: typography::regular(),
        align_x: layout_text::Alignment::Default,
        align_y: iced::alignment::Vertical::Top,
        shaping: layout_text::Shaping::Advanced,
        wrapping: Wrapping::None,
    });
    Some(paragraph.min_width())
}

fn table_layout_portions(
    table: &Table,
    columns: usize,
    width: f32,
    prefs: &Preferences,
) -> Vec<u16> {
    let preferred = resolved_table_column_widths(table, columns);
    if table.rows.iter().flat_map(|row| &row.cells).any(|cell| cell.width.is_some()) {
        return table_column_fill_portions(&preferred);
    }
    let padding = 2.0 * TABLE_CELL_PADDING[1] + 2.0;
    let mut minimums = vec![padding; columns];
    for row in &table.rows {
        let mut column = 0;
        for cell in &row.cells {
            let span = usize::from(cell.column_span.max(1));
            if span == 1 && let Some(minimum) = minimums.get_mut(column) {
                for block in &cell.blocks {
                    if let Some(line_width) = short_cell_line_width(block, cell.header, prefs) {
                        *minimum = minimum.max(line_width + padding);
                    }
                }
            }
            column += span;
        }
    }
    // Reserve enough space for compact labels and amounts, then divide the
    // space left over according to the preferred widths of the prose columns.
    let available = (width - minimums.iter().sum::<f32>()).max(0.0);
    let total_weight = preferred.iter().sum::<u32>().max(1) as f32;
    let flex: Vec<_> = preferred.iter().zip(&minimums)
        .map(|(&weight, &minimum)| (width * weight as f32 / total_weight - minimum).max(0.0))
        .collect();
    let total_flex = flex.iter().sum::<f32>().max(1.0);
    let widths: Vec<_> = minimums.into_iter().zip(flex)
        .map(|(minimum, flex)| ((minimum + available * flex / total_flex) * 100.0).round() as u32)
        .collect();
    table_column_fill_portions(&widths)
}

fn render_table(
    table: &Table,
    prefs: &Preferences,
    theme: &Theme,
    mine: bool,
    media: &MediaCache,
    content_width: f32,
) -> Element<'static, AppMessage> {
    let columns = table_column_count(table);
    let column_portions = table_layout_portions(table, columns, content_width, prefs);
    let total_portions = column_portions
        .iter()
        .map(|width| u32::from(*width))
        .sum::<u32>()
        .max(1);
    let mut rendered_rows = column![];
    for row_data in &table.rows {
        let mut cells = row![].width(Fill);
        let mut vertical_alignments = Vec::with_capacity(columns);
        let mut column = 0usize;
        for cell in &row_data.cells {
            if column >= columns {
                break;
            }
            let span = usize::from(cell.column_span.max(1)).min(columns - column);
            let column_portion = column_portions[column..column + span]
                .iter()
                .map(|portion| u32::from(*portion))
                .sum::<u32>()
                .min(u32::from(u16::MAX)) as u16;
            let inner_width = (content_width * f32::from(column_portion) / total_portions as f32
                - TABLE_CELL_PADDING[1] * 2.0).max(0.0);
            let mut cell_blocks = column![]
                .spacing(TABLE_BLOCK_SPACING)
                .width(Fill)
                .align_x(cell.alignment.iced());
            let cell_tone = if cell.header {
                Tone::Hover
            } else {
                Tone::Surface
            };
            let cell_background = cell
                .background
                .map(crate::appearance::rgb)
                .unwrap_or_else(|| tone(theme, cell_tone));
            for block in &cell.blocks {
                cell_blocks = cell_blocks.push(if block.quote > 0 {
                    render_quote(
                        std::slice::from_ref(block),
                        prefs,
                        theme,
                        mine,
                        media,
                        inner_width,
                        None,
                        0,
                    )
                } else { render_block(
                    block,
                    prefs,
                    theme,
                    mine,
                    cell.header,
                    media,
                    inner_width,
                    Some(cell.alignment),
                    Some(cell_background),
                    None,
                    0,
                ) });
            }
            let rendered_cell = container(cell_blocks)
                .width(iced::Length::FillPortion(column_portion))
                .padding(TABLE_CELL_PADDING)
                .clip(true)
                .style(move |theme| container::Style {
                    background: Some(Background::Color(cell_background)),
                    border: Border::default()
                        .color(tone(theme, Tone::Border))
                        .width(0.5),
                    ..Default::default()
                });
            cells = cells.push(rendered_cell);
            vertical_alignments.push(cell.vertical_alignment.iced());
            column += span;
        }
        while column < columns {
            let portion = column_portions.get(column).copied().unwrap_or(1);
            cells = cells.push(
                container(space::Space::new())
                    .width(iced::Length::FillPortion(portion))
                    .padding(TABLE_CELL_PADDING)
                    .style(|theme| container::Style {
                        background: Some(Background::Color(tone(theme, Tone::Surface))),
                        border: Border::default()
                            .color(tone(theme, Tone::Border))
                            .width(0.5),
                        ..Default::default()
                    }),
            );
            vertical_alignments.push(iced::alignment::Vertical::Top);
            column += 1;
        }
        rendered_rows = rendered_rows.push(table_row::view(cells, vertical_alignments));
    }
    container(rendered_rows)
        .width(content_width)
        .style(ui_surface(
            if mine { Tone::Outgoing } else { Tone::Incoming },
            true,
            4.0,
        ))
        .into()
}

fn card_line_visible(line: &str) -> String {
    card_markdown_runs(line)
        .iter()
        .map(|run| run.text.as_str())
        .collect()
}

fn card_line_is_compact(line: &str) -> bool {
    let runs = card_markdown_runs(line);
    let chars: usize = runs.iter().map(|run| run.text.chars().count()).sum();
    chars <= 28 && runs.iter().all(|run| run.style.link.is_none())
}

fn priority_label(line: &str) -> Option<String> {
    let visible = card_line_visible(line);
    let trimmed = visible
        .trim()
        .trim_start_matches(['⌄', '↓', '▾', '▼'])
        .trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "low" | "lowest" | "medium" | "high" | "highest" => Some(trimmed.to_owned()),
        _ => None,
    }
}

fn is_status_pill(line: &str) -> bool {
    let visible = card_line_visible(line);
    let trimmed = visible.trim();
    !trimmed.is_empty()
        && trimmed.chars().count() <= 32
        && trimmed
            .chars()
            .all(|character| character.is_uppercase() || character.is_whitespace() || character == '-')
}

fn card_mark_view(kind: CardMarkKind) -> Element<'static, AppMessage> {
    let (label, color) = match kind {
        CardMarkKind::Brand => ("J", 0x2684ff),
        CardMarkKind::Issue => ("", 0xe2483d),
    };
    container(
        text(label)
            .size(11)
            .font(typography::semibold())
            .style(ui_text(Tone::OnPrimary)),
    )
    .padding(if label.is_empty() { [7, 7] } else { [2, 5] })
    .style(move |_theme| container::Style {
        background: Some(Background::Color(crate::appearance::rgb(color))),
        border: Border::default().rounded(4.0),
        ..Default::default()
    })
    .into()
}

fn column_row_starting_at(card: &CardFallback, index: usize) -> Option<&CardColumnRow> {
    card.column_rows
        .iter()
        .filter(|row| {
            row.columns
                .first()
                .is_some_and(|column| column.start == index)
        })
        .max_by_key(|row| {
            row.columns
                .last()
                .map(|column| column.end)
                .unwrap_or(index)
                .saturating_sub(index)
        })
}

fn card_column_is_separator(card: &CardFallback, range: &Range<usize>) -> bool {
    !range.is_empty()
        && range.clone().all(|index| {
            card.lines
                .get(index)
                .is_some_and(|line| matches!(line.trim(), "|" | "│" | "¦"))
        })
}

fn card_line_runs(line: &str, style: CardLineStyle, muted: Option<u32>) -> Vec<Run> {
    let mut runs = card_markdown_runs(line);
    for run in &mut runs {
        if style.emphasis != CardLineEmphasis::Body {
            run.style.bold = true;
        }
        if run.style.color.is_none() {
            run.style.color = muted;
        }
    }
    if runs.is_empty() {
        runs.push(Run {
            text: line.to_owned(),
            style: Style {
                bold: style.emphasis != CardLineEmphasis::Body,
                color: muted,
                ..Style::default()
            },
        });
    }
    runs
}

fn card_has_facts_at(card: &CardFallback, index: usize) -> bool {
    card.fact_sets
        .iter()
        .any(|set| set.before_line == index && !set.facts.is_empty())
}

fn render_card_facts(
    card: &CardFallback,
    index: usize,
    base_size: f32,
) -> Element<'static, AppMessage> {
    let mut facts = column![].spacing(6);
    for set in card.fact_sets.iter().filter(|set| set.before_line == index) {
        for fact in &set.facts {
            let fact_row = row![
                text(fact.title.clone())
                    .size(base_size * 0.92)
                    .font(typography::semibold())
                    .style(ui_text(Tone::Secondary))
                    .width(iced::Length::FillPortion(35)),
                text(fact.value.clone())
                    .size(base_size)
                    .style(ui_text(Tone::Text))
                    .width(iced::Length::FillPortion(65)),
            ]
            .spacing(12)
            .align_y(iced::Alignment::Center);
            facts = facts.push(
                container(fact_row)
                    .width(Fill)
                    .padding([6, 8])
                    .style(ui_surface(Tone::Surface, true, 4.0)),
            );
        }
    }
    facts.into()
}

fn render_card_line(
    card: &CardFallback,
    index: usize,
    prefs: &Preferences,
    theme: &Theme,
    mine: bool,
    base_size: f32,
) -> Element<'static, AppMessage> {
    let line = card.lines.get(index).map(String::as_str).unwrap_or("");
    let style = card.line_styles.get(index).copied().unwrap_or_default();
    let muted = style.subtle.then(|| {
        let color = tone(theme, Tone::Muted);
        ((color.r * 255.0).round() as u32) << 16
            | ((color.g * 255.0).round() as u32) << 8
            | (color.b * 255.0).round() as u32
    });
    let size = if style.emphasis == CardLineEmphasis::Heading {
        base_size * 1.16
    } else {
        base_size
    };
    let mut runs = card_line_runs(line, style, muted);
    if let Some(label) = priority_label(line) {
        runs = vec![
            Run {
                text: "⌄ ".into(),
                style: Style::default(),
            },
            Run {
                text: label,
                style: Style {
                    bold: true,
                    ..Style::default()
                },
            },
        ];
    }
    let rendered = render_runs(&runs, size, prefs, theme, mine, false, false, None, None, None, 0);
    let rendered = if is_status_pill(line) {
        container(rendered)
            .padding([2, 8])
            .style(ui_surface(Tone::Hover, false, 10.0))
            .into()
    } else {
        rendered
    };
    let Some(mark) = card.marks.iter().find(|mark| mark.line == index) else {
        return rendered;
    };
    row![card_mark_view(mark.kind), rendered]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()
}

fn text_card_width(card: &CardFallback) -> Option<f32> {
    (card.kind == "Cartão adaptável" && card.column_rows.is_empty() && card.fact_sets.is_empty()
        && !card.marks.iter().any(|mark| mark.kind == CardMarkKind::Brand)).then_some(540.0)
}

fn render_card(
    card: &CardFallback,
    prefs: &Preferences,
    theme: &Theme,
    mine: bool,
) -> Element<'static, AppMessage> {
    let adaptive = card.kind == "Cartão adaptável";
    let mut content = column![].spacing(10);
    if !adaptive {
        content = content.push(
            text(card.kind.clone())
                .size(12)
                .font(typography::semibold())
                .style(ui_text(Tone::Secondary)),
        );
    }
    if let Some(title) = &card.title {
        content = content.push(
            text(title.clone())
                .size(f32::from(prefs.message_size) * 1.08)
                .font(typography::semibold())
                .style(ui_text(Tone::Text)),
        );
    }
    let base_size = f32::from(prefs.message_size);
    let has_brand = card.marks.iter().any(|mark| mark.kind == CardMarkKind::Brand)
        || card
            .lines
            .iter()
            .any(|line| card_line_visible(line).eq_ignore_ascii_case("jira"));
    if !has_brand && card.lines.iter().any(|line| line.contains("atlassian.net")) {
        content = content.push(
            row![
                card_mark_view(CardMarkKind::Brand),
                text("Jira")
                    .size(base_size)
                    .font(typography::semibold())
                    .style(ui_text(Tone::Text))
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
    }
    let mut index = 0;
    while index <= card.lines.len() {
        if let Some(column_row) = column_row_starting_at(card, index) {
            if card_has_facts_at(card, index) {
                content = content.push(render_card_facts(card, index, base_size));
            }
            let packed = column_row.columns.iter().all(|range| {
                card_column_is_separator(card, range)
                    || range
                        .clone()
                        .all(|line_index| {
                            card.lines
                                .get(line_index)
                                .is_some_and(|line| card_line_is_compact(line))
                        })
            });
            let mut columns = row![]
                .spacing(if packed { 6 } else { 8 })
                .align_y(iced::Alignment::Center);
            for range in &column_row.columns {
                if card_column_is_separator(card, range) {
                    continue;
                }
                let mut column = column![].spacing(4);
                for line_index in range.clone() {
                    if line_index != index && card_has_facts_at(card, line_index) {
                        column = column.push(render_card_facts(card, line_index, base_size));
                    }
                    column = column.push(render_card_line(
                        card, line_index, prefs, theme, mine, base_size,
                    ));
                }
                let compact = range.clone().all(|line_index| {
                    card.lines
                        .get(line_index)
                        .is_some_and(|line| card_line_is_compact(line))
                });
                let width = if packed || compact {
                    iced::Length::Shrink
                } else {
                    iced::Length::Fill
                };
                columns = columns.push(container(column).width(width));
            }
            content = content.push(columns);
            index = column_row
                .columns
                .last()
                .map(|column| column.end)
                .unwrap_or(index + 1);
            continue;
        }
        if card_has_facts_at(card, index) {
            content = content.push(render_card_facts(card, index, base_size));
        }
        if index < card.lines.len() {
            content = content.push(render_card_line(
                card, index, prefs, theme, mine, base_size,
            ));
        }
        index += 1;
    }
    if !card.actions.is_empty() {
        let actions = card.actions.iter().take(8).collect::<Vec<_>>();
        let primary = actions
            .iter()
            .position(|action| action.positive)
            .unwrap_or(0);
        let split = actions.len() >= 3;
        let (left, right) = if split {
            (&actions[..actions.len() - 1], Some(actions[actions.len() - 1]))
        } else {
            (actions.as_slice(), None)
        };
        let mut bar = row![].spacing(8).align_y(iced::Alignment::Center);
        for (index, action) in left.iter().enumerate() {
            bar = bar.push(card_action_chip(action, index == primary));
        }
        if let Some(action) = right {
            bar = bar.push(space::horizontal());
            bar = bar.push(card_action_chip(action, false));
        }
        content = content.push(bar.width(Fill));
    }
    container(content)
        .width(Fill)
        .max_width(text_card_width(card).unwrap_or(f32::INFINITY))
        .padding(12)
        .style(move |theme: &Theme| {
            let role = if mine { Tone::Outgoing } else { Tone::Card };
            container::Style {
                background: Some(Background::Color(tone(theme, role))),
                border: Border::default().rounded(8.0).color(tone(theme, Tone::Border)).width(1.0),
                ..Default::default()
            }
        })
        .into()
}

fn card_action_chip(action: &CardAction, primary: bool) -> Element<'static, AppMessage> {
    let mut label = action.label.clone();
    if (action.icon || label.eq_ignore_ascii_case("notify")) && !label.contains('🔔') {
        label = format!("🔔 {label}");
    }
    let label = if primary {
        text(label).size(12).style(ui_text(Tone::OnPrimary))
    } else {
        text(label).size(12)
    };
    if valid_link(&action.url).is_some() {
        button(label)
            .padding([6, 12])
            .on_press(AppMessage::OpenMessageLink(action.url.clone()))
            .style(crate::appearance::ui_button(
                if primary { Tone::Primary } else { Tone::Hover },
                if primary {
                    Tone::Accent
                } else {
                    Tone::SelectedHover
                },
                if primary { Tone::OnPrimary } else { Tone::Text },
                6.0,
            ))
            .into()
    } else if primary {
        container(label)
            .padding([6, 12])
            .style(ui_surface(Tone::Primary, false, 6.0))
            .into()
    } else {
        container(label)
            .padding([6, 12])
            .style(ui_surface(Tone::Hover, true, 6.0))
            .into()
    }
}

pub fn inline_image_view(
    inline: &InlineImage,
    media: &MediaCache,
    max_width: f32,
) -> Element<'static, AppMessage> {
    let cached = media.get(&inline.url);
    let (width, height) = crate::images::display_size(
        inline,
        cached.map_or(0, |loaded| loaded.width),
        cached.map_or(0, |loaded| loaded.height),
        max_width,
        crate::images::INLINE_MAX_HEIGHT,
    );
    let preview: Element<'static, AppMessage> = if let Some(loaded) = cached {
        let open = button(
            image(loaded.handle.clone())
                .width(width)
                .height(height)
                .content_fit(iced::ContentFit::Contain),
        )
        .padding(0)
        .on_press(AppMessage::OpenInlineImage(inline.url.clone()))
        .style(crate::appearance::ui_button(
            Tone::Transparent,
            Tone::Transparent,
            Tone::Text,
            6.0,
        ));
        tooltip(
            open,
            text("Abrir imagem original").size(12),
            tooltip::Position::Bottom,
        )
        .delay(std::time::Duration::from_millis(400))
        .into()
    } else {
        let loading = media.loading(&inline.url);
        container(
            button(text(if loading {
                "Carregando imagem…"
            } else {
                "Carregar imagem"
            }))
            .padding(12)
            .on_press_maybe((!loading).then_some(AppMessage::RetryInlineImage(inline.url.clone())))
            .style(crate::appearance::ui_button(
                Tone::Transparent,
                Tone::Hover,
                Tone::Secondary,
                6.0,
            )),
        )
        .width(width)
        .height(height)
        .center_x(width)
        .center_y(height)
        .into()
    };
    paste::image(preview, inline.url.clone())
}

fn author_line(
    block: &Block,
    size: f32,
    theme: &Theme,
    interaction: Option<&crate::message_selection::Interaction<'_>>,
    block_index: usize,
) -> Element<'static, AppMessage> {
    let visible: String = block.runs.iter().map(|run| run.text.as_str()).collect();
    let color = tone(theme, Tone::Secondary);
    let Some(interaction) = interaction else {
        return text(visible).size(size).style(ui_text(Tone::Secondary)).into();
    };
    let range = interaction.range_in_block(block_index, &visible);
    let mut spans = Vec::new();
    for (piece, selected) in crate::message_selection::split_selected(&visible, range) {
        if piece.is_empty() {
            continue;
        }
        let mut span = Span::new(visible[piece].to_owned()).size(size).color(color);
        if selected {
            span = span.background(crate::message_selection::selection_color());
        }
        spans.push(span);
    }
    crate::message_selection::selectable(
        spans,
        visible,
        interaction.message_id.to_owned(),
        block_index,
        interaction.drag_in_block(block_index),
        size,
        size * 1.35,
    )
}

fn message_run_color(style: &Style, theme: &Theme, background: iced::Color, fallback: iced::Color) -> iced::Color {
    if style.mention {
        tone(theme, if style.mention_info.as_ref().is_some_and(crate::mentions::Mention::attention) { Tone::Mention } else { Tone::Accent })
    } else if style.link.is_some() {
        let accent = tone(theme, Tone::Accent);
        if style.background.is_some() { readable_color(None, background, accent) } else { accent }
    } else {
        readable_color(style.color, background, fallback)
    }
}

fn message_run_font(style: &Style, force_bold: bool) -> Font {
    Font {
        weight: if style.bold || force_bold
            || style.mention_info.as_ref().is_some_and(crate::mentions::Mention::attention)
        {
            font::Weight::Semibold
        } else {
            font::Weight::Normal
        },
        style: if style.italic { font::Style::Italic } else { font::Style::Normal },
        ..if style.code {
            Font::MONOSPACE
        } else if style.italic {
            typography::ITALIC_FALLBACK
        } else {
            typography::regular()
        }
    }
}

fn render_runs(
    runs: &[Run],
    size: f32,
    prefs: &Preferences,
    theme: &Theme,
    mine: bool,
    force_bold: bool,
    preformatted: bool,
    surface_override: Option<iced::Color>,
    alignment: Option<CellAlignment>,
    interaction: Option<&crate::message_selection::Interaction<'_>>,
    block_index: usize,
) -> Element<'static, AppMessage> {
    let surface = surface_override
        .unwrap_or_else(|| tone(theme, if mine { Tone::Outgoing } else { Tone::Incoming }));
    let visible: String = runs.iter().map(|run| run.text.as_str()).collect();
    let selected = interaction.and_then(|interaction| interaction.range_in_block(block_index, &visible));
    let mut spans = Vec::with_capacity(runs.len());
    let mut byte_cursor = 0;
    for run in runs {
        let style = &run.style;
        let font = message_run_font(style, force_bold);
        let highlight = style.background.map(crate::appearance::rgb);
        let bg = highlight.unwrap_or(surface);
        let fallback_color = if highlight.is_some() {
            if luminance(bg) > 0.4 {
                crate::appearance::rgb(0x242424)
            } else {
                crate::appearance::rgb(0xffffff)
            }
        } else {
            tone(
                theme,
                if style.link.is_some() || style.mention {
                    Tone::Accent
                } else {
                    Tone::Text
                },
            )
        };
        let local = selected.and_then(|(start, end)| {
            let from = start.max(byte_cursor);
            let to = end.min(byte_cursor + run.text.len());
            (from < to).then_some((from - byte_cursor, to - byte_cursor))
        });
        for (piece, is_selected) in crate::message_selection::split_selected(&run.text, local) {
            if piece.is_empty() {
                continue;
            }
            for (segment, emoji) in typography::emoji_segments(&run.text[piece]) {
                let mut span = Span::new(segment.to_owned())
                    .font(if emoji { typography::EMOJI } else { font })
                    .size(size * f32::from(style.size) / 100.0)
                    .underline(style.underline || (style.link.is_some() && !style.mention))
                    .strikethrough(style.strike)
                    .color(message_run_color(style, theme, bg, fallback_color));
                if is_selected {
                    span = span.background(crate::message_selection::selection_color());
                } else if style.code && !preformatted {
                    span = span.background(tone(theme, Tone::Hover));
                }
                if let Some(highlight) = highlight.filter(|_| !is_selected) {
                    span = span.background(highlight);
                }
                if style.mention {
                    if let Some((crate::mentions::Mention{target:crate::mentions::Target::Person(person_id),..}, interaction))=style.mention_info.as_ref().zip(interaction) {
                        span=span.link(crate::message_selection::Link::Mention {message_id:interaction.message_id.into(),person_id:person_id.clone()});
                    }
                } else if let Some(link) = &style.link {
                    span = span.link(crate::message_selection::Link::Web(link.clone()));
                }
                spans.push(span);
            }
        }
        byte_cursor += run.text.len();
    }
    if let Some(interaction) = interaction.filter(|_| !visible.is_empty()) {
        return crate::message_selection::selectable(
            spans,
            visible,
            interaction.message_id.to_owned(),
            block_index,
            interaction.drag_in_block(block_index),
            size,
            prefs.line_height(),
        );
    }
    let mut rendered = rich_text(spans)
        .size(size)
        .line_height(prefs.line_height())
        .wrapping(Wrapping::WordOrGlyph)
        .style(ui_text(Tone::Text))
        .on_link_click(crate::message_selection::Link::action);
    if let Some(alignment) = alignment {
        rendered = rendered.width(Fill).align_x(alignment.iced());
    }
    rendered.into()
}
fn text_marker(marker: &str, size: f32) -> Element<'static, AppMessage> {
    text(marker.to_owned())
        .size(size)
        .style(ui_text(Tone::Text))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn all_text(doc: &Document) -> String {
        doc.blocks
            .iter()
            .flat_map(|b| b.runs.iter())
            .map(|r| r.text.as_str())
            .collect()
    }

    #[test]
    fn sent_lines_stay_contiguous_and_keep_only_explicit_blanks() {
        let text = "Review notes\nPlease check the link.\n• Task: EXAMPLE-1\n\nThanks!";
        let document = Document::from_editor(text, &ComposerFormatting::default());
        let received = Document::from_html(&document.to_html()).unwrap();
        for document in [&document, &received] {
            assert_eq!(visible_block_texts(document).join("\n"), text);
            assert!(blank_line(&document.blocks[3]));
            assert_eq!(document.blocks.iter().filter(|block| blank_line(block)).count(), 1);
            for pair in document.blocks.windows(2) {
                assert_eq!(message_block_gap(&pair[0], &pair[1], true), 0.0);
            }
        }
        assert_eq!(message_block_gap(&received.blocks[0], &received.blocks[1], false), 12.0);
        let div = Document::from_html("<div>Start</div><div><br></div><div>End</div>").unwrap();
        assert_eq!(visible_block_texts(&div).join("\n"), "Start\n\nEnd");
    }

    #[test]
    fn only_a_card_with_message_text_uses_the_full_bubble() {
        let html = include_str!("../tests/fixtures/message-with-card.html");
        let document = Document::from_html(html).unwrap();
        assert!(document.combines_text_and_card());

        let (text, card) = html.split_once("<attachment").unwrap();
        let card = Document::from_html(&format!("<attachment{card}")).unwrap();
        assert!(card.standalone_text_card_width().is_some());
        assert!(!card.combines_text_and_card());
        assert!(!Document::from_html(text).unwrap().combines_text_and_card());
    }

    #[test]
    fn workflow_reply_mentions_followed_by_a_line_do_not_add_a_paragraph_gap() {
        let raw = include_str!("../tests/fixtures/workflow-reply.html");
        let (_, body) = crate::quoted_reply::split_all(raw).unwrap();
        let document = Document::from_html(&body).unwrap();
        let [greeting, follow_up] = document.blocks.as_slice() else {
            panic!("expected the greeting and the follow-up");
        };
        assert_eq!(greeting.runs.iter().filter(|run| run.style.mention).count(), 3);
        assert_eq!(plain_block(follow_up), "Poderiam conferir o alerta de teste, por favor?");
        assert_eq!(message_block_gap(greeting, follow_up, false), 0.0);

        let explicit_break = body.replace("</p>\n<p>Poderiam", "<br>Poderiam");
        let document = Document::from_html(&explicit_break).unwrap();
        assert_eq!(document.blocks.len(), 1);
        assert!(plain_block(&document.blocks[0]).contains(",\nPoderiam"));
    }
    #[test]
    fn received_file_slots_require_unique_identity_and_keep_existing_images_once() {
        let url = "https://api.asm.skype.com/v1/objects/0-file/views/imgo";
        let file = ost::api::ChatFileRef {
            preview: Some(ost::api::ChatFilePreview { url:url.into(), width:500, height:300 }),
            attachment_id:Some("file".into()), name:"capture.png".into(), file_type:Some("png".into()),
            object_url:"https://example.sharepoint.com/capture.png".into(), share_url:None, sender:None, time:None,
        };
        let html = format!("<p>Antes</p><img src='{url}'><attachment id='file'></attachment><p>Depois</p>");
        let doc = Document::from_html_with_files(&html, std::slice::from_ref(&file)).unwrap();
        assert_eq!(doc.blocks.iter().filter(|block|block.media.is_some()).count(),1);
        assert_eq!(doc.blocks.last().unwrap().runs[0].text,"Depois");
        let table = Document::from_html_with_files("<table><tr><td><attachment id='file'></attachment></td></tr></table>", std::slice::from_ref(&file)).unwrap();
        assert_eq!(table.inline_images().len(),1);
        let mut unidentified = file.clone(); unidentified.attachment_id=None;
        let doc = Document::from_html_with_files("<attachment></attachment>",&[unidentified]).unwrap();
        assert!(doc.blocks[0].card.is_some());
        let doc = Document::from_html_with_files("<attachment id='file'></attachment>",&[file.clone(),file]).unwrap();
        assert!(doc.blocks[0].card.is_some());
    }

    #[test]
    fn received_links_keep_semantic_color_and_contact_lines_stay_contiguous() {
        let doc = Document::from_html("<div style='color:white'><p><a href='mailto:ana@example.test'>ana@example.test</a></p><p><a href='mailto:bia@example.test'><span style='color:white'>bia@example.test</span></a></p><p>Podem conferir?</p></div>").unwrap();
        let theme = Theme::Dark;
        let style = &doc.blocks[0].runs[0].style;
        assert_eq!(message_run_color(style, &theme, tone(&theme, Tone::Incoming), tone(&theme, Tone::Accent)), tone(&theme, Tone::Accent), "received links must not inherit white body text");
        assert_eq!(message_block_gap(&doc.blocks[0], &doc.blocks[1], false), 0.0, "consecutive email links form one contact block");
        assert_eq!(message_block_gap(&doc.blocks[1], &doc.blocks[2], false), 12.0);
        let plain = Style { color: Some(0xe8825e), ..Style::default() };
        assert_eq!(message_run_color(&plain, &theme, tone(&theme, Tone::Incoming), tone(&theme, Tone::Text)), crate::appearance::rgb(0xe8825e));
    }

    #[test]
    fn mentions_keep_parent_weight_instead_of_forcing_bold() {
        let document = Document::from_html("<p><at>Ana</at> <strong><at>Bruno</at></strong></p>").unwrap();
        let runs = &document.blocks[0].runs;
        assert!(runs.iter().any(|run| run.text == "Ana" && run.style.mention && !run.style.bold));
        assert!(runs.iter().any(|run| run.text == "Bruno" && run.style.mention && run.style.bold));
    }

    #[test]
    fn escaped_anchor_end_does_not_link_following_workflow_paragraphs() {
        let document = Document::from_html("<a href='https://example.test/monitor'><p>Monitor&lt;/a&gt;</p><p><b>Ação esperada</b></p><p>Conferir o alerta</p></a><p><a href='https://example.test/monitor'>Workflow</a></p>").unwrap();
        let fixture = Document::from_html(include_str!("../tests/fixtures/workflow-alert.html")).unwrap();
        let action = fixture.blocks.iter().flat_map(|b| b.runs.iter()).find(|r| r.text.contains("Ação esperada")).unwrap();
        assert!(action.style.link.is_none());
        let find = |value: &str| document.blocks.iter().flat_map(|b| b.runs.iter()).find(|r| r.text.contains(value)).unwrap();
        assert!(find("Monitor").style.link.is_some());
        assert!(find("Ação esperada").style.link.is_none());
        assert!(find("Conferir").style.link.is_none());
        assert!(find("Workflow").style.link.is_some());
        assert!(all_text(&document).contains("</a>"));
    }

    #[test]
    fn quote_paragraphs_share_a_card_without_merging_adjacent_quotations() {
        let document = Document::from_html("<blockquote><p><strong>Ana</strong></p><p>Primeira citação</p></blockquote><blockquote><p><strong>Bruno</strong></p><p>Segunda citação</p></blockquote><p>Resposta</p>").unwrap();
        let fixture = Document::from_html(include_str!("../tests/fixtures/quoted-replies.html")).unwrap();
        assert!(all_text(&fixture).contains("Obrigado!"));
        assert_eq!(document.blocks.len(), 5);
        assert_eq!(quote_group_end(&document.blocks, 0), 2);
        assert_eq!(quote_group_end(&document.blocks, 2), 4);
        assert_ne!(document.blocks[0].quote_group, document.blocks[2].quote_group);
        assert_eq!(document.blocks[4].quote, 0);
        assert!(all_text(&document).contains("Primeira citação"));
        assert!(all_text(&document).contains("Segunda citação"));
    }

    #[test]
    fn message_spacing_distinguishes_quotes_paragraphs_and_list_items() {
        let doc = Document::from_html(concat!(
            "<blockquote><p>Primeira citação</p></blockquote>",
            "<blockquote><p>Segunda citação</p></blockquote>",
            "<p>Olá</p><p>Início</p><ul><li>Um</li><li>Dois</li></ul><p>Fim</p>"
        )).unwrap();
        assert_eq!(message_block_gap(&doc.blocks[0], &doc.blocks[1], false), 24.0);
        assert_eq!(message_block_gap(&doc.blocks[1], &doc.blocks[2], false), 6.0);
        assert_eq!(message_block_gap(&doc.blocks[2], &doc.blocks[3], false), 12.0);
        assert_eq!(message_block_gap(&doc.blocks[3], &doc.blocks[4], false), 18.0);
        assert_eq!(message_block_gap(&doc.blocks[4], &doc.blocks[5], false), 0.0);
        assert_eq!(message_block_gap(&doc.blocks[5], &doc.blocks[6], false), 12.0);
    }

    #[test]
    fn typed_ticket_lines_stay_on_one_line_rhythm() {
        let doc = Document::from_html(concat!(
            "<p>Bugs:</p>",
            "<p>Alta prioridade:</p>",
            "<p>1) DEMO-1186</p>",
            "<p>2) DEMO-1081</p>",
            "<p>5) DEMO-1962 e DEMO-1157.</p>",
            "<p>Menos prioridade:</p>",
            "<p>9) DEMO-1209 e DEMO-1208</p>",
            "<p>11) DEMO-595</p>",
        ))
        .unwrap();
        assert_eq!(doc.blocks.len(), 8);
        for pair in doc.blocks.windows(2) {
            assert_eq!(message_block_gap(&pair[0], &pair[1], false), 0.0);
        }
        let prose = Document::from_html("<p>Olá</p><p>Início do assunto.</p>").unwrap();
        assert_eq!(message_block_gap(&prose.blocks[0], &prose.blocks[1], false), 12.0);
    }

    #[test]
    fn real_html_blocks_keep_nested_styles_links_entities_and_list_markers() {
        let doc =
            Document::from_html(include_str!("../tests/fixtures/message-formatting.html")).unwrap();
        assert_eq!(doc.blocks[0].heading, 3);
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| b.marker.as_deref() == Some("•"))
                .count(),
            2
        );
        let runs: Vec<_> = doc.blocks.iter().flat_map(|b| &b.runs).collect();
        assert!(runs.iter().any(|r| r.style.bold && r.text == "revisão"));
        assert!(
            runs.iter()
                .any(|r| r.style.italic && r.text == "um único lugar")
        );
        assert!(runs.iter().any(|r| r.style.link.as_deref()
            == Some("https://example.com/revisao")
            && r.text == "resumo da revisão"));
        assert!(runs.iter().any(|r| r.style.background == Some(0xfff100)));
        assert!(
            runs.iter()
                .any(|r| r.style.strike && r.text == "Layout antigo")
        );
        assert!(!all_text(&doc).contains("<strong>"));
        let doc = Document::from_html("<p> A &amp; <b>B <i>C</i></b><br>D&nbsp;E </p><p>Fim</p>")
            .unwrap();
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(all_text(&doc), "A & B C\nD\u{a0}EFim");
        assert!(
            doc.blocks[0]
                .runs
                .iter()
                .any(|r| r.style.bold && r.style.italic && r.text == "C")
        );
    }
    #[test]
    fn clipboard_keeps_inline_formatting_links_and_plain_text_fallback() {
        let doc = Document::from_html("<p><strong>Review</strong> <a href='https://example.com/review?a=1&amp;b=2'>summary</a></p>").unwrap();
        let payload = doc.clipboard_payload();
        assert_eq!(
            payload.text,
            "Review summary (https://example.com/review?a=1&b=2)"
        );
        assert!(payload.html.contains("<strong>Review</strong>"));
        assert!(
            payload
                .html
                .contains("<a href=\"https://example.com/review?a=1&amp;b=2\">summary</a>")
        );
    }

    #[test]
    fn clipboard_html_becomes_safe_editor_text_with_inline_styles_and_links() {
        let paste = Document::from_clipboard_html(
            "<p>Hello <strong>bold</strong> and <a href='https://example.com/review'>review</a></p><p>second line</p>",
        )
        .unwrap();
        assert_eq!(paste.text, "Hello bold and review\nsecond line");
        assert!(paste.formatting.has_mark(6, 10, InlineMark::Bold));
        assert!(paste.formatting.links.iter().any(|range| {
            range.start == 15 && range.end == 21 && range.url == "https://example.com/review"
        }));
    }

    #[test]
    fn clipboard_html_drops_unsafe_link_destinations_and_script_content() {
        let paste = Document::from_clipboard_html(
            "<p><a href='javascript:alert(1)'>unsafe label</a><script>steal()</script></p>",
        )
        .unwrap();
        assert_eq!(paste.text, "unsafe label");
        assert!(paste.formatting.links.is_empty());
        assert!(!paste.text.contains("steal"));
    }

    #[test]
    fn clipboard_html_does_not_split_a_combining_grapheme_at_a_style_boundary() {
        let paste = Document::from_clipboard_html("<p>A<strong>e</strong>\u{301}Z</p>").unwrap();
        assert_eq!(paste.text, "Ae\u{301}Z");
        assert!(paste.formatting.has_mark(1, 2, InlineMark::Bold));
    }
    #[test]
    fn clipboard_escapes_text_and_preserves_quotes_lists_and_code() {
        let doc = Document::from_html("<blockquote><p><code>&lt;script&gt;</code></p></blockquote><ul><li>safe &amp; sound</li></ul>").unwrap();
        let payload = doc.clipboard_payload();
        assert_eq!(payload.text, "> <script>\n• safe & sound");
        assert!(payload.html.contains("<blockquote><p"));
        assert!(payload.html.contains("<code>&lt;script&gt;</code>"));
        assert!(payload.html.contains("• safe &amp; sound"));
        assert!(!payload.html.contains("<script>"));
    }
    #[test]
    fn composer_ranges_use_graphemes_and_serialize_semantic_html() {
        let text = "A👨‍👩‍👧‍👦e\u{301}Z";
        let mut formatting = ComposerFormatting::default();
        assert!(formatting.toggle_mark(1, 3, InlineMark::Bold));
        assert!(formatting.set_link(1, 3, "https://example.com/review?a=1&b=2"));
        let document = Document::from_editor(text, &formatting);
        assert_eq!(document.blocks[0].runs.len(), 3);
        assert!(document.blocks[0].runs[1].style.bold);
        assert_eq!(document.blocks[0].runs[1].text, "👨‍👩‍👧‍👦e\u{301}");
        assert_eq!(
            document.to_html(),
            "<p>A<a href=\"https://example.com/review?a=1&amp;b=2\"><strong>👨‍👩‍👧‍👦e\u{301}</strong></a>Z</p>"
        );
        assert!(!formatting.set_link(0, 1, "javascript:alert(1)"));
    }

    #[test]
    fn composer_active_marks_format_new_text_until_the_user_turns_them_off() {
        let mut formatting = ComposerFormatting::default();
        formatting.adjust_after_edit_with_active_marks("", "Olá", &[InlineMark::Bold]);
        assert!(formatting.has_mark(0, 3, InlineMark::Bold));

        formatting.adjust_after_edit_with_active_marks(
            "Olá",
            "Olá!",
            &[InlineMark::Bold],
        );
        assert!(formatting.has_mark(0, 4, InlineMark::Bold));

        formatting.adjust_after_edit_with_active_marks("Olá!", "Olá! ok", &[]);
        assert!(formatting.has_mark(0, 4, InlineMark::Bold));
        assert!(!formatting.has_mark(0, 7, InlineMark::Bold));
        assert!(formatting.has_mark_at(4, InlineMark::Bold));
        assert!(!formatting.has_mark_at(6, InlineMark::Bold));
    }

    #[test]
    fn composer_font_size_applies_to_selection_and_can_stay_active_while_typing() {
        let mut formatting = ComposerFormatting::default();
        assert!(formatting.set_size(1, 3, 150));
        assert_eq!(formatting.size_for_range(1, 3), Some(150));
        assert_eq!(formatting.size_for_range(0, 3), None);

        let document = Document::from_editor("A👋B", &formatting);
        assert_eq!(document.blocks[0].runs[1].style.size, 150);
        assert!(document.to_html().contains("font-size:150%"));

        formatting.set_size(1, 3, 100);
        assert_eq!(formatting.size_for_range(0, 3), Some(100));
        assert!(formatting.sizes.is_empty());

        formatting.adjust_after_edit_with_active_formats("", "Oi", &[], 125, None);
        formatting.adjust_after_edit_with_active_formats("Oi", "Oi!", &[], 125, None);
        assert_eq!(formatting.size_for_range(0, 3), Some(125));
        formatting.adjust_after_edit_with_active_formats("Oi!", "Oi! normal", &[], 100, None);
        assert_eq!(formatting.size_for_range(0, 3), Some(125));
        assert_eq!(formatting.size_at(4), 100);
    }

    #[test]
    fn composer_text_color_applies_to_selection_and_new_text() {
        let mut formatting = ComposerFormatting::default();
        assert!(formatting.set_color(1, 3, Some(0xc50f1f)));
        assert_eq!(formatting.color_for_range(1, 3), Some(Some(0xc50f1f)));
        assert_eq!(formatting.color_for_range(0, 3), None);

        let document = Document::from_editor("A👋B", &formatting);
        assert_eq!(document.blocks[0].runs[1].style.color, Some(0xc50f1f));
        assert!(document.to_html().contains("color:#c50f1f"));

        formatting.set_color(1, 3, None);
        assert!(formatting.colors.is_empty());
        formatting.adjust_after_edit_with_active_formats("", "Oi", &[], 100, Some(0x005a9e));
        formatting.adjust_after_edit_with_active_formats("Oi", "Oi!", &[], 100, Some(0x005a9e));
        assert_eq!(formatting.color_for_range(0, 3), Some(Some(0x005a9e)));
        formatting.adjust_after_edit_with_active_formats("Oi!", "Oi! ok", &[], 100, None);
        assert_eq!(formatting.color_for_range(0, 3), Some(Some(0x005a9e)));
        assert_eq!(formatting.color_at(4), None);
    }

    #[test]
    fn composer_highlight_and_quote_roundtrip_to_editor_html() {
        let text = "Atenção\nsegundo parágrafo";
        let mut formatting = ComposerFormatting::default();
        assert!(formatting.set_highlight(0, 7, Some(0xfdd472)));
        assert_eq!(formatting.highlight_for_range(0, 7), Some(Some(0xfdd472)));
        assert!(formatting.toggle_quote(
            text,
            (
                0,
                unicode_segmentation::UnicodeSegmentation::graphemes(text, true).count(),
            ),
        ));

        let document = Document::from_editor(text, &formatting);
        assert_eq!(document.blocks[0].quote, 1);
        assert_eq!(document.blocks[1].quote, 1);
        assert_eq!(document.blocks[0].runs[0].style.background, Some(0xfdd472));
        assert_eq!(
            document.to_html(),
            "<blockquote><p><span style=\"background-color:#fdd472\">Atenção</span></p><p>segundo parágrafo</p></blockquote>"
        );

        assert!(formatting.toggle_quote(text, (0, 7)));
        assert_eq!(formatting.quote_blocks, vec![QuoteRange { start: 1, end: 2 }]);
    }

    #[test]
    fn composer_highlight_stays_active_for_new_text_and_pastes_back() {
        let mut formatting = ComposerFormatting::default();
        formatting.adjust_after_edit_with_active_formats_and_highlight(
            "",
            "Oi",
            &[],
            100,
            None,
            Some(0xe5f18f),
        );
        assert_eq!(formatting.highlight_for_range(0, 2), Some(Some(0xe5f18f)));
        let document = Document::from_editor("Oi", &formatting);
        let paste = document.to_composer_paste();
        assert_eq!(paste.text, "Oi");
        assert_eq!(paste.formatting.highlights[0].color, 0xe5f18f);
    }

    #[test]
    fn composer_table_ranges_roundtrip_tabbed_editor_lines_to_html() {
        let text = "Etapa\tResponsável\nRevisão\tAna";
        let mut formatting = ComposerFormatting::default();
        assert!(formatting.insert_table(0, 2, 2));
        let document = Document::from_editor(text, &formatting);
        let table = document.blocks[0].table.as_ref().expect("editor table");
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0].cells.len(), 2);
        assert_eq!(table.rows[1].cells[1].blocks[0].runs[0].text, "Ana");
        assert_eq!(
            document.to_html(),
            "<table><tbody><tr><td><p>Etapa</p></td><td><p>Responsável</p></td></tr><tr><td><p>Revisão</p></td><td><p>Ana</p></td></tr></tbody></table>"
        );
    }

    #[test]
    fn rich_composer_highlighter_uses_byte_spans_for_grapheme_ranges() {
        use unicode_segmentation::UnicodeSegmentation;

        let text = "A👨‍👩‍👧‍👦e\u{301}Z";
        let mut formatting = ComposerFormatting::default();
        formatting.toggle_mark(1, 3, InlineMark::Bold);
        let settings = ComposerHighlightSettings::new(text, formatting);
        let mut highlighter =
            <ComposerHighlighter as iced::advanced::text::Highlighter>::new(&settings);
        let runs = iced::advanced::text::Highlighter::highlight_line(&mut highlighter, text)
            .collect::<Vec<_>>();
        let bold_start = text.graphemes(true).next().unwrap().len();
        let bold_end = text.len() - "Z".len();
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0], (0..bold_start, ComposerHighlight::default()));
        assert!(runs[1].1.bold && runs[1].1.emoji);
        assert!(runs[2].1.bold && !runs[2].1.emoji);
        assert_eq!(runs[1].0.start, bold_start);
        assert_eq!(runs[2].0.end, bold_end);
        assert_eq!(runs[3], (bold_end..text.len(), ComposerHighlight::default()));

        let selection = (1, 3);
        let cursor = editor_cursor_for_selection(text, selection);
        assert_eq!(editor_selection(text, cursor), Some(selection));
    }

    #[test]
    fn rich_composer_highlighter_keeps_plain_text_visible() {
        use iced::advanced::text::Highlighter;
        let text = "Teste 👋\nOutra linha";
        let settings = ComposerHighlightSettings::new(text, ComposerFormatting::default());
        let mut highlighter = ComposerHighlighter::new(&settings);
        for line in text.lines() {
            let runs = highlighter.highlight_line(line).collect::<Vec<_>>();
            if line.contains('👋') {
                assert_eq!(runs.len(), 2);
                assert!(!runs[0].1.emoji);
                assert!(runs[1].1.emoji);
                assert_eq!(
                    composer_highlight_format(&runs[0].1, &Theme::Dark).font,
                    Some(typography::regular())
                );
                assert_eq!(
                    composer_highlight_format(&runs[1].1, &Theme::Dark).font,
                    Some(typography::EMOJI)
                );
            } else {
                assert_eq!(runs, vec![(0..line.len(), ComposerHighlight::default())]);
                let format = composer_highlight_format(&runs[0].1, &Theme::Dark);
                assert_eq!(format.font, Some(typography::regular()));
                assert_eq!(format.color, Some(crate::appearance::tone(&Theme::Dark, crate::appearance::Tone::Text)));
            }
        }
    }

    #[test]
    fn composer_lists_and_inline_styles_are_escaped_and_reversible() {
        let text = "<first>\nsecond\nthird";
        let mut formatting = ComposerFormatting::default();
        formatting.toggle_mark(0, 7, InlineMark::Code);
        formatting.toggle_list(text, (0, 14), ListKind::Ordered);
        let document = Document::from_editor(text, &formatting);
        assert_eq!(
            document.to_html(),
            "<ol start=\"1\"><li><code>&lt;first&gt;</code></li><li>second</li></ol><p>third</p>"
        );
        assert!(formatting.toggle_mark(0, 7, InlineMark::Code));
        assert!(!formatting.has_mark(0, 7, InlineMark::Code));
        assert!(formatting.toggle_list(text, (0, 14), ListKind::Ordered));
        assert!(formatting.lists.is_empty());
    }

    #[test]
    fn composer_preview_document_keeps_underline_strike_and_list_markers() {
        let text = "revisar\npublicar";
        let mut formatting = ComposerFormatting::default();
        assert!(formatting.toggle_mark(0, 7, InlineMark::Underline));
        assert!(formatting.toggle_mark(8, 16, InlineMark::Strike));
        assert!(formatting.toggle_list(text, (0, 16), ListKind::Unordered));

        let preview = Document::from_editor(text, &formatting);
        assert_eq!(preview.blocks[0].marker.as_deref(), Some("•"));
        assert!(preview.blocks[0].runs[0].style.underline);
        assert_eq!(preview.blocks[0].runs[0].text, "revisar");
        assert_eq!(preview.blocks[1].marker.as_deref(), Some("•"));
        assert!(preview.blocks[1].runs[0].style.strike);
        assert_eq!(preview.blocks[1].runs[0].text, "publicar");
        assert_eq!(
            preview.to_html(),
            "<ul><li><u>revisar</u></li><li><s>publicar</s></li></ul>"
        );
    }

    #[test]
    fn code_block_selection_serializes_as_pre_and_stays_active_when_a_line_is_inserted() {
        let text = "fn main() {\n  run();\n}";
        let mut formatting = ComposerFormatting::default();
        assert!(formatting.toggle_code_block(
            text,
            (
                0,
                unicode_segmentation::UnicodeSegmentation::graphemes(text, true).count(),
            ),
        ));
        let document = Document::from_editor(text, &formatting);
        assert_eq!(
            document.to_html(),
            "<pre><code>fn main() {</code>\n<code>  run();</code>\n<code>}</code></pre>"
        );
        formatting.adjust_after_edit(text, "fn main() {\n  run();\n  more();\n}");
        assert_eq!(
            formatting.code_blocks,
            vec![CodeBlockRange { start: 0, end: 4 }]
        );
        let edited = Document::from_editor("fn main() {\n  run();\n  more();\n}", &formatting);
        assert!(edited.blocks.iter().all(|block| block.pre));
        assert_eq!(
            Document::from_html(&document.to_html()).unwrap().blocks[0].runs[0].text,
            "fn main() {\n  run();\n}"
        );
    }

    #[test]
    fn edits_keep_unchanged_prefix_and_suffix_styles_and_shift_links() {
        let mut formatting = ComposerFormatting::default();
        formatting.toggle_mark(0, 9, InlineMark::Italic);
        formatting.set_link(3, 6, "https://example.com");
        formatting.adjust_after_edit("abcXYZdef", "abc++def");
        assert_eq!(
            formatting.marks,
            vec![
                FormatRange {
                    start: 0,
                    end: 3,
                    mark: InlineMark::Italic,
                },
                FormatRange {
                    start: 5,
                    end: 8,
                    mark: InlineMark::Italic,
                },
            ]
        );
        assert!(formatting.links.is_empty());
        let document = Document::from_editor("abc++def", &formatting);
        assert_eq!(document.to_html(), "<p><em>abc</em>++<em>def</em></p>");
    }

    #[test]
    fn edits_preserve_list_membership_on_unchanged_lines() {
        let mut formatting = ComposerFormatting::default();
        formatting.toggle_list("one\ntwo\nthree", (0, 13), ListKind::Unordered);
        formatting.adjust_after_edit("one\ntwo\nthree", "one\nnew\ntwo\nthree");
        assert_eq!(
            formatting.lists,
            vec![
                ListRange {
                    start: 0,
                    end: 1,
                    kind: ListKind::Unordered,
                },
                ListRange {
                    start: 2,
                    end: 4,
                    kind: ListKind::Unordered,
                }
            ]
        );
    }
    #[test]
    fn plain_message_payload_linkifies_and_keeps_the_link_destination() {
        let payload = clipboard_payload(None, "Confira https://example.com/notes.");
        assert_eq!(payload.text, "Confira https://example.com/notes.");
        assert!(payload.html.contains("href=\"https://example.com/notes\""));
    }

    #[test]
    fn search_highlights_keep_run_text_links_and_inline_styles() {
        let document = Document::plain("Revise o build antes do deploy.");
        let highlighted = document.with_search_highlights("BUILD");
        let block = &highlighted.blocks[0];
        assert_eq!(
            block
                .runs
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>(),
            "Revise o build antes do deploy."
        );
        let match_run = block
            .runs
            .iter()
            .find(|run| run.style.background.is_some())
            .unwrap();
        assert_eq!(match_run.text, "build");
        assert_eq!(match_run.style.color, Some(0x242424));

        let linked = Document::plain("Leia https://example.test/ReleaseNotes agora.");
        let linked = linked.with_search_highlights("releasenotes");
        let link_match = linked.blocks[0]
            .runs
            .iter()
            .find(|run| run.style.background.is_some())
            .unwrap();
        assert_eq!(link_match.text, "ReleaseNotes");
        assert_eq!(
            link_match.style.link.as_deref(),
            Some("https://example.test/ReleaseNotes")
        );
    }

    #[test]
    fn search_highlights_each_term_in_any_query_order() {
        let document = Document::plain("Deploy aprovado antes da revisão final.");
        let highlighted = document.with_search_highlights("REVISÃO, deploy");
        let block = &highlighted.blocks[0];
        let text = block
            .runs
            .iter()
            .filter(|run| run.style.background.is_some())
            .map(|run| run.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(text, ["Deploy", "revisão"]);
        assert_eq!(
            block
                .runs
                .iter()
                .map(|run| run.text.as_str())
                .collect::<String>(),
            "Deploy aprovado antes da revisão final."
        );
    }

    #[test]
    fn search_highlights_can_be_cleared_without_changing_the_source_document() {
        let document = Document::plain("Equipe de revisão");
        let highlighted = document.with_search_highlights("equipe");
        let cleared = document.with_search_highlights("  ");
        assert!(
            highlighted.blocks[0]
                .runs
                .iter()
                .any(|run| run.style.background.is_some())
        );
        assert!(
            cleared.blocks[0]
                .runs
                .iter()
                .all(|run| run.style.background.is_none())
        );
        assert_eq!(document.blocks[0].runs[0].text, "Equipe de revisão");
    }
    #[test]
    fn quotes_nested_numbered_lists_and_preformatted_code_remain_separate() {
        let doc = Document::from_html("<blockquote><p>Mensagem citada</p></blockquote><ol start='3'><li><p>Primeiro</p><ul><li>Filho</li></ul></li><li>Segundo</li></ol><pre><code>fn main() {\n    println!(&quot;ok&quot;);\n}</code></pre>").unwrap();
        assert_eq!(doc.blocks[0].quote, 1);
        assert_eq!(doc.blocks[1].marker.as_deref(), Some("3."));
        assert_eq!(doc.blocks[1].ordered_list_value, Some(3));
        assert_eq!(
            doc.blocks[1].ordered_list_style,
            Some(OrderedListStyle::Decimal)
        );
        assert_eq!(doc.blocks[2].indent, 2);
        assert_eq!(doc.blocks[3].marker.as_deref(), Some("4."));
        assert_eq!(doc.blocks[3].ordered_list_value, Some(4));
        let code = doc.blocks.last().unwrap();
        assert!(code.pre && code.runs[0].style.code);
        assert_eq!(code.runs[0].text, "fn main() {\n    println!(\"ok\");\n}");
    }

    #[test]
    fn ordered_list_type_start_and_item_value_survive_parse_and_copy() {
        let document = Document::from_html(
            r#"<ol type="A" start="3"><li>Third</li><li value="6">Sixth</li><li>Seventh</li></ol>"#,
        )
        .unwrap();

        assert_eq!(
            document
                .blocks
                .iter()
                .map(|block| block.marker.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["C.", "F.", "G."]
        );
        assert!(
            document
                .blocks
                .iter()
                .all(|block| { block.ordered_list_style == Some(OrderedListStyle::UpperAlpha) })
        );
        assert_eq!(
            document
                .blocks
                .iter()
                .map(|block| block.ordered_list_value)
                .collect::<Vec<_>>(),
            [Some(3), Some(6), Some(7)]
        );
        assert_eq!(
            document.to_html(),
            "<ol type=\"A\" start=\"3\"><li>Third</li><li value=\"6\">Sixth</li><li>Seventh</li></ol>"
        );
    }

    #[test]
    fn ordered_list_css_style_formats_roman_markers_and_clipboard_type() {
        let document = Document::from_html(
            r#"<ol style="list-style-type: lower-roman" start="4"><li>Fourth</li><li>Fifth</li></ol>"#,
        )
        .unwrap();

        assert_eq!(document.blocks[0].marker.as_deref(), Some("iv."));
        assert_eq!(document.blocks[1].marker.as_deref(), Some("v."));
        assert_eq!(
            document.to_html(),
            "<ol type=\"i\" start=\"4\"><li>Fourth</li><li>Fifth</li></ol>"
        );
    }

    #[test]
    fn alphabetic_ordered_lists_roll_over_after_z() {
        let document = Document::from_html(
            r#"<ol type="A" start="26"><li>Twenty-six</li><li>Twenty-seven</li></ol>"#,
        )
        .unwrap();

        assert_eq!(document.blocks[0].marker.as_deref(), Some("Z."));
        assert_eq!(document.blocks[1].marker.as_deref(), Some("AA."));
        assert_eq!(
            document.to_html(),
            "<ol type=\"A\" start=\"26\"><li>Twenty-six</li><li>Twenty-seven</li></ol>"
        );
    }

    #[test]
    fn inline_images_keep_their_position_dimensions_and_clipboard_order() {
        let document =
            Document::from_html(include_str!("../tests/fixtures/message-inline-images.html"))
                .unwrap();

        assert!(document.contains_inline_images());
        assert_eq!(document.blocks.len(), 6);
        assert_eq!(document.blocks[0].runs[0].text, "Antes");
        assert_eq!(document.blocks[1].media.as_ref().unwrap().id, "0-first");
        assert_eq!(
            (
                document.blocks[1].media.as_ref().unwrap().width,
                document.blocks[1].media.as_ref().unwrap().height,
            ),
            (1920, 1080)
        );
        assert_eq!(document.blocks[2].runs[0].text, "depois.");
        assert_eq!(document.blocks[4].media.as_ref().unwrap().id, "0-second");
        assert_eq!(document.blocks[5].runs[0].text, "rodapé.");

        let clipboard = document.clipboard_payload();
        assert!(clipboard.text.find("Antes").unwrap() < clipboard.text.find("[Imagem]").unwrap());
        assert!(clipboard.text.find("[Imagem]").unwrap() < clipboard.text.find("depois.").unwrap());
        assert!(clipboard.html.find("Antes").unwrap() < clipboard.html.find("0-first").unwrap());
        assert!(clipboard.html.find("0-first").unwrap() < clipboard.html.find("depois.").unwrap());
    }

    #[test]
    fn inline_image_budget_is_shared_with_nested_table_cells() {
        let cell = "<td><img src='https://us-api.asm.skype.com/v1/objects/0-image/views/imgo' itemtype='http://schema.skype.com/AMSImage' width='4' height='3'/></td>";
        let html = format!("<table><tr>{}</tr></table>", cell.repeat(12));
        let document = Document::from_html(&html).unwrap();
        let count = document.blocks[0]
            .table
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .flat_map(|row| &row.cells)
            .flat_map(|cell| &cell.blocks)
            .filter(|block| block.media.is_some())
            .count();
        assert_eq!(count, crate::images::MAX_IMAGES);
    }

    #[test]
    fn tables_keep_rows_cells_headers_rich_text_and_clipboard_structure() {
        let doc =
            Document::from_html(include_str!("../tests/fixtures/message-table.html")).unwrap();
        assert_eq!(doc.blocks.len(), 1);
        let table = doc.blocks[0].table.as_ref().unwrap();
        assert_eq!(table.rows.len(), 3);
        assert_eq!(table.rows[0].cells.len(), 3);
        assert!(table.rows[0].cells.iter().all(|cell| cell.header));
        assert!(table.rows[1].cells[0].blocks[0].runs[0].style.bold);
        assert_eq!(
            table.rows[1].cells[2].blocks[0].runs[0]
                .style
                .link
                .as_deref(),
            Some("https://example.com/status")
        );

        let copied = doc.clipboard_payload();
        assert_eq!(
            copied.text,
            "Etapa\tResponsável\tStatus\nRevisão\tAna\tAguardando (https://example.com/status)\nPublicação\tRui\tNão iniciada"
        );
        assert!(copied.html.contains("<table><tbody>"));
        assert!(
            copied
                .html
                .contains("<th><p><strong>Etapa</strong></p></th>")
        );
        assert!(copied.html.contains("href=\"https://example.com/status\""));
    }

    #[test]
    fn tables_preserve_cell_alignment_vertical_alignment_and_colors() {
        let doc = Document::from_html(
            "<table><tr><td align='center' valign='middle' bgcolor='#ffeecc' style='color:#123456'><p>Central</p></td><td style='text-align:right;vertical-align:bottom;background-color:rgb(240,224,208);color:rgb(18,52,86)'><p>Direita</p></td></tr></table>",
        )
        .unwrap();
        let cells = &doc.blocks[0].table.as_ref().unwrap().rows[0].cells;
        assert_eq!(cells[0].alignment, CellAlignment::Center);
        assert_eq!(cells[0].vertical_alignment, CellVerticalAlignment::Center);
        assert_eq!(cells[0].background, Some(0xffeecc));
        assert_eq!(cells[0].color, Some(0x123456));
        assert_eq!(cells[0].blocks[0].runs[0].style.color, Some(0x123456));
        assert_eq!(cells[1].alignment, CellAlignment::End);
        assert_eq!(cells[1].vertical_alignment, CellVerticalAlignment::Bottom);
        assert_eq!(cells[1].background, Some(0xf0e0d0));
        assert_eq!(cells[1].color, Some(0x123456));

        let clipboard = doc.clipboard_payload();
        assert!(clipboard.html.contains(
            "text-align:center;vertical-align:middle;color:#123456;background-color:#ffeecc"
        ));
        assert!(clipboard.html.contains(
            "text-align:right;vertical-align:bottom;color:#123456;background-color:#f0e0d0"
        ));
    }

    #[test]
    fn tables_preserve_percentage_column_widths_in_layout_and_clipboard() {
        let doc = Document::from_html(
            "<table><tr><th width='20%' style='width:25.5%'>Etapa</th><th width='74.5%'>Status</th></tr><tr><td>Revisão</td><td>Em andamento</td></tr></table>",
        )
        .unwrap();
        let table = doc.blocks[0].table.as_ref().unwrap();
        assert_eq!(table.rows[0].cells[0].width, Some(2_550));
        assert_eq!(table.rows[0].cells[1].width, Some(7_450));
        assert_eq!(resolved_table_column_widths(table, 2), vec![2_550, 7_450]);
        assert_eq!(
            table_column_fill_portions(&resolved_table_column_widths(table, 2)),
            vec![342, 1_000]
        );
        let copied = doc.clipboard_payload();
        assert!(copied.html.contains("width:25.50%"));
        assert!(copied.html.contains("width:74.50%"));
    }

    #[test]
    fn tables_auto_size_text_columns_for_wide_status_summaries() {
        let document = Document::from_html(
            "<table><tr><th>Situação</th><th>Faturas</th><th>Valor (R$)</th><th>Cartão</th><th>Boleto</th><th>Próximo passo</th></tr><tr><td>Cartão não encontrado na Pagar.me, em retentativa</td><td>6</td><td>R$ 726.65</td><td>6</td><td>0</td><td>Régua do Gateway2 tenta de novo a partir de 21/11 às 08:00, mas o cartão não existe mais na Pagar.me</td></tr></table>",
        )
        .unwrap();
        let table = document.blocks[0].table.as_ref().unwrap();
        let widths = resolved_table_column_widths(table, 6);
        assert!(widths[5] > widths[0]);
        assert!(widths[0] > widths[1]);
        assert_eq!(widths[1], widths[3]);
        assert_eq!(widths[3], widths[4]);
    }

    #[test]
    #[ignore = "requires a local GPU adapter; run explicitly for chat rendering"]
    fn tables_fit_rendered_text_and_values_inside_their_rows() {
        use crate::chat_render_tests::{capture, headless_renderer, save_snapshot};
        use iced::advanced::{Layout, layout::Limits, widget::Tree};
        use iced::Size;

        let mut renderer = headless_renderer();
        let doc = Document::from_html(include_str!("../tests/fixtures/summary-table-layout.html"))
            .unwrap();
        let table = doc.blocks[0].table.as_ref().unwrap();
        let media = MediaCache::default();
        let theme = crate::appearance::teams_theme(crate::Appearance::Dark);
        let prefs = Preferences { roomy_lines: true, ..Default::default() };
        for width in [670.0, 1012.0, 330.0] {
            let limits = Limits::new(Size::ZERO, Size::new(width, f32::INFINITY));
            let mut widget = render_table(table, &prefs, &theme, true, &media, 1012.0);
            let mut tree = Tree::new(widget.as_widget());
            let node = widget.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let rows = Layout::new(&node).children().next().unwrap();
            assert_eq!(rows.children().count(), table.rows.len());
            for (row_index, (row, source)) in rows.children().zip(&table.rows).enumerate() {
                for (cell_index, (cell, source)) in row.children().zip(&source.cells).enumerate() {
                    assert!((cell.bounds().height - row.bounds().height).abs() < 0.1);
                    let content = cell.children().next().unwrap();
                    let mut natural = iced::widget::column![].spacing(2).width(Fill);
                    for block in &source.blocks {
                        natural = natural.push(render_block(
                            block, &prefs, &theme, true, source.header, &media,
                            content.bounds().width, Some(source.alignment), None, None, 0,
                        ));
                    }
                    let mut natural: Element<'static, AppMessage> = natural.into();
                    let mut natural_tree = Tree::new(natural.as_widget());
                    let intrinsic = natural.as_widget_mut().layout(
                        &mut natural_tree, &renderer,
                        &Limits::new(Size::ZERO, Size::new(content.bounds().width, f32::INFINITY)),
                    );
                    let needed = intrinsic.size().height;
                    let space = cell.bounds().y + cell.bounds().height - content.bounds().y
                        - TABLE_CELL_PADDING[0];
                    assert!(needed <= space + 0.1,
                        "{width}px table row {row_index} cell {cell_index}: text needs {needed}px in {space}px");
                    if width == 1012.0 && row_index == 4 && cell_index == 2 {
                        assert!(needed <= 16.0 * prefs.line_height() + 0.1,
                            "the total fits on one line at desktop width, found {needed}px");
                    }
                }
            }
            if width == 1012.0 {
                let zoom = 1.25;
                let size = Size::new((node.size().width * zoom).ceil() as u32, (node.size().height * zoom).ceil() as u32);
                let rgba = capture(&mut renderer, &widget, &tree, &node, &theme, size, zoom);
                save_snapshot("tabela-125.png", size, &rgba);
            }
        }
    }

    #[test]
    fn colspan_is_preserved_in_layout_and_clipboard_representations() {
        let document = Document::from_html(
            "<table><tr><th colspan='2' width='100%'>Resumo</th></tr><tr><td>A</td><td>B</td></tr></table>",
        )
        .unwrap();
        let table = document.blocks[0].table.as_ref().unwrap();

        assert_eq!(table.rows[0].cells[0].column_span, 2);
        assert_eq!(table_column_count(table), 2);
        assert_eq!(resolved_table_column_widths(table, 2), vec![5_000, 5_000]);
        assert_eq!(
            table_column_fill_portions(&resolved_table_column_widths(table, 2)),
            vec![1_000, 1_000]
        );
        assert!(document.to_composer_paste().text.starts_with("Resumo\t\n"));

        let copied = document.clipboard_payload();
        assert_eq!(copied.text, "Resumo\t\nA\tB");
        assert!(
            copied
                .html
                .contains("<th colspan=\"2\" style=\"width:100%\">")
        );
    }

    #[test]
    fn colspan_is_bounded_and_invalid_values_fall_back_to_one_column() {
        let bounded =
            Document::from_html("<table><tr><td colspan='999'>wide</td></tr></table>").unwrap();
        let bounded_table = bounded.blocks[0].table.as_ref().unwrap();
        assert_eq!(bounded_table.rows[0].cells[0].column_span, 12);
        assert_eq!(table_column_count(bounded_table), 12);

        let invalid =
            Document::from_html("<table><tr><td colspan='not-a-number'>ordinary</td></tr></table>")
                .unwrap();
        assert_eq!(
            invalid.blocks[0].table.as_ref().unwrap().rows[0].cells[0].column_span,
            1
        );
    }

    #[test]
    fn nested_tables_keep_their_rows_inside_their_own_table() {
        let document = Document::from_html(
            "<table><tr><td>Outer<table><tr><td>Inner A</td><td>Inner B</td></tr></table></td><td>Outer B</td></tr></table>",
        )
        .unwrap();
        let outer = document.blocks[0].table.as_ref().unwrap();

        assert_eq!(outer.rows.len(), 1);
        assert_eq!(outer.rows[0].cells.len(), 2);
        let inner = outer.rows[0].cells[0]
            .blocks
            .iter()
            .find_map(|block| block.table.as_ref())
            .expect("nested table remains in its parent cell");
        assert_eq!(inner.rows.len(), 1);
        assert_eq!(inner.rows[0].cells.len(), 2);

        let copied = document.clipboard_payload();
        assert_eq!(copied.text, "Outer\nInner A\tInner B\tOuter B");
        assert_eq!(copied.html.matches("<table><tbody>").count(), 2);
    }

    #[test]
    fn tables_infer_unset_percentage_columns_and_ignore_unsupported_width_units() {
        let partial =
            Document::from_html("<table><tr><td style='width:25%'>A</td><td>B</td></tr></table>")
                .unwrap();
        assert_eq!(
            resolved_table_column_widths(partial.blocks[0].table.as_ref().unwrap(), 2),
            vec![2_500, 7_500]
        );

        let pixel_width =
            Document::from_html("<table><tr><td style='width:120px'>A</td><td>B</td></tr></table>")
                .unwrap();
        assert_eq!(
            table_column_fill_portions(&resolved_table_column_widths(
                pixel_width.blocks[0].table.as_ref().unwrap(),
                2
            )),
            vec![1_000, 1_000]
        );
    }

    #[test]
    fn adaptive_cards_render_as_bounded_native_content_and_keep_safe_actions() {
        let doc = Document::from_html(
            r#"<attachment contenttype="application/vnd.microsoft.card.adaptive">{"type":"AdaptiveCard","title":"Deploy","body":[{"type":"TextBlock","text":"Deploy aprovado"},{"type":"FactSet","facts":[{"title":"Ambiente","value":"HML"}]}],"actions":[{"type":"Action.OpenUrl","title":"Abrir deploy","url":"https://example.com/build?a=1&b=2"}]}</attachment>"#,
        )
        .unwrap();
        let card = doc.blocks[0].card.as_ref().unwrap();
        assert_eq!(card.kind, "Cartão adaptável");
        assert_eq!(card.title.as_deref(), Some("Deploy"));
        assert!(card.lines.iter().any(|line| line == "Deploy aprovado"));
        assert_eq!(card.fact_sets.len(), 1);
        assert_eq!(card.fact_sets[0].before_line, 1);
        assert_eq!(card.fact_sets[0].facts[0].title, "Ambiente");
        assert_eq!(card.fact_sets[0].facts[0].value, "HML");
        let action = card.action.as_ref().unwrap();
        assert_eq!(action.label, "Abrir deploy");
        assert_eq!(action.url, "https://example.com/build?a=1&b=2");

        let clipboard = doc.clipboard_payload();
        assert!(clipboard.text.contains("Cartão adaptável\nDeploy"));
        assert!(clipboard.text.contains("Ambiente: HML"));
        assert!(clipboard.html.contains("<section>"));
        assert!(clipboard.html.contains(
            "<table><tbody><tr><th scope=\"row\">Ambiente</th><td>HML</td></tr></tbody></table>"
        ));
        let composer = doc.to_composer_paste().text;
        let body = composer.find("Deploy aprovado").unwrap();
        let fact = composer.find("Ambiente: HML").unwrap();
        let action = composer.find("Abrir deploy").unwrap();
        assert!(body < fact && fact < action);
        assert!(
            clipboard
                .html
                .contains("href=\"https://example.com/build?a=1&amp;b=2\"")
        );
        assert!(!clipboard.html.contains("<script"));
    }

    #[test]
    fn workflow_card_preserves_line_breaks_and_link_text_from_attachment() {
        let doc = Document::from_html(r#"<attachment contenttype="application/vnd.microsoft.card.adaptive">{"type":"AdaptiveCard","body":[{"type":"TextBlock","text":"Resumo\n\n• Primeiro\n• Segundo\nhttps://example.com/case"}]}</attachment>"#).unwrap();
        let card = doc.blocks.iter().find_map(|block| block.card.as_ref()).unwrap();
        assert_eq!(card.lines[0], "Resumo\n\n• Primeiro\n• Segundo\nhttps://example.com/case");
        let mut runs = vec![Run { text: card.lines[0].clone(), style: Style::default() }];
        linkify(&mut runs);
        assert!(runs.iter().any(|run| run.style.link.as_deref() == Some("https://example.com/case")));
    }

    #[test]
    fn adaptive_fact_sets_keep_position_and_escape_clipboard_html() {
        let document = Document::from_html(
            r#"<attachment contenttype="application/vnd.microsoft.card.adaptive">{"type":"AdaptiveCard","body":[{"type":"TextBlock","text":"Antes"},{"type":"FactSet","facts":[{"title":"\u003cb\u003eAmbiente\u003c/b\u003e","value":"HML & PRD"},{"title":"Owner","value":"Ana"}]},{"type":"TextBlock","text":"Depois"}]}</attachment>"#,
        )
        .unwrap();
        let card = document.blocks[0].card.as_ref().unwrap();

        assert_eq!(card.lines, ["Antes", "Depois"]);
        assert_eq!(card.fact_sets.len(), 1);
        assert_eq!(card.fact_sets[0].before_line, 1);
        assert_eq!(card.fact_sets[0].facts.len(), 2);

        let copied = document.clipboard_payload();
        let before = copied.text.find("Antes").unwrap();
        let fact = copied
            .text
            .find("HML & PRD")
            .unwrap_or_else(|| panic!("unexpected copied text: {:?}", copied.text));
        let after = copied.text.find("Depois").unwrap();
        assert!(before < fact && fact < after);
        assert!(copied.html.find("<p>Antes</p>").unwrap() < copied.html.find("<table>").unwrap());
        assert!(copied.html.find("<table>").unwrap() < copied.html.find("<p>Depois</p>").unwrap());
        assert!(copied.html.contains("&lt;b&gt;Ambiente&lt;/b&gt;"));
        assert!(copied.html.contains("HML &amp; PRD"));
        assert!(!copied.html.contains("<b>Ambiente</b>"));
    }

    #[test]
    fn adaptive_card_text_weight_and_size_are_preserved_for_render_and_copy() {
        let doc = Document::from_html(
            r#"<attachment contenttype="application/vnd.microsoft.card.adaptive">{"type":"AdaptiveCard","body":[{"type":"TextBlock","text":"Status","weight":"Bolder"},{"type":"TextBlock","text":"Deploy aprovado","size":"Large"}]}</attachment>"#,
        )
        .unwrap();
        let card = doc.blocks[0].card.as_ref().unwrap();
        assert_eq!(card.lines, ["Status", "Deploy aprovado"]);
        assert_eq!(
            card.line_styles,
            [
                CardLineStyle {
                    emphasis: CardLineEmphasis::Strong,
                    subtle: false,
                },
                CardLineStyle {
                    emphasis: CardLineEmphasis::Heading,
                    subtle: false,
                },
            ]
        );

        let clipboard = doc.clipboard_payload();
        assert!(clipboard.html.contains("<strong>Status</strong>"));
        assert!(clipboard.html.contains("<h4>Deploy aprovado</h4>"));
        assert_eq!(clipboard.text, "Cartão adaptável\nStatus\nDeploy aprovado");
    }

    #[test]
    fn adaptive_card_subtle_text_keeps_emphasis_in_render_model_and_copy() {
        let doc = Document::from_html(
            r#"<attachment contenttype="application/vnd.microsoft.card.adaptive">{"type":"AdaptiveCard","body":[{"type":"TextBlock","text":"Contexto","isSubtle":true},{"type":"TextBlock","text":"Status","weight":"Bolder","isSubtle":true},{"type":"TextBlock","text":"Resultado","size":"Large","isSubtle":true}]}</attachment>"#,
        )
        .unwrap();
        let card = doc.blocks[0].card.as_ref().unwrap();
        assert_eq!(
            card.line_styles,
            [
                CardLineStyle {
                    emphasis: CardLineEmphasis::Body,
                    subtle: true,
                },
                CardLineStyle {
                    emphasis: CardLineEmphasis::Strong,
                    subtle: true,
                },
                CardLineStyle {
                    emphasis: CardLineEmphasis::Heading,
                    subtle: true,
                },
            ]
        );
        let clipboard = doc.clipboard_payload();
        assert!(clipboard.html.contains("<small>Contexto</small>"));
        assert!(
            clipboard
                .html
                .contains("<strong><small>Status</small></strong>")
        );
        assert!(clipboard.html.contains("<h4><small>Resultado</small></h4>"));
        assert_eq!(
            clipboard.text,
            "Cartão adaptável\nContexto\nStatus\nResultado"
        );
    }

    #[test]
    fn history_layout_estimate_handles_wrapped_text_and_bounded_images() {
        let document = Document::from_html(
            "<p>abcd</p><p>éééé</p><img src='https://us-api.asm.skype.com/v1/objects/image-1/views/imgo' id='image-1' itemid='image-1' itemtype='http://schema.skype.com/AMSImage' width='640' height='320'>",
        )
        .unwrap();

        let (lines, image_height) = document.estimated_layout(2, 120.0);
        assert_eq!(lines, 4);
        assert_eq!(image_height, 60.0);
        assert_eq!(Document::estimated_plain_lines("a\nb\n", 2), 3);
    }

    #[test]
    fn message_cards_render_facts_and_safe_open_uri_actions() {
        let doc = Document::from_html(
            r#"<attachment contenttype="application/vnd.microsoft.card.o365connector">{"@type":"MessageCard","title":"Build concluído","text":"Pipeline aprovado","sections":[{"facts":[{"name":"Branch","value":"main"}]}],"potentialAction":[{"@type":"OpenUri","name":"Abrir build","targets":[{"os":"default","uri":"https://example.com/build"}]}]}</attachment>"#,
        )
        .unwrap();
        let card = doc.blocks[0].card.as_ref().unwrap();
        assert_eq!(card.kind, "Cartão de mensagem");
        assert_eq!(card.title.as_deref(), Some("Build concluído"));
        assert!(card.lines.iter().any(|line| line == "Pipeline aprovado"));
        assert!(card.lines.iter().any(|line| line == "Branch: main"));
        let action = card.action.as_ref().unwrap();
        assert_eq!(action.label, "Abrir build");
        assert_eq!(action.url, "https://example.com/build");

        let clipboard = doc.clipboard_payload();
        assert!(
            clipboard
                .text
                .contains("Cartão de mensagem\nBuild concluído")
        );
        assert!(
            clipboard
                .html
                .contains("href=\"https://example.com/build\"")
        );
    }

    #[test]
    fn card_links_only_become_actions_for_supported_open_url_types() {
        let doc = Document::from_html(
            r#"<attachment contenttype="application/vnd.microsoft.card.adaptive">{"type":"AdaptiveCard","body":[{"type":"TextBlock","text":"Build","url":"https://example.com/metadata"}],"actions":[{"type":"Action.Submit","title":"Enviar","url":"https://example.com/submit"},{"type":"Action.OpenUrl","title":"Abrir build","url":"https://example.com/build"}]}</attachment>"#,
        )
        .unwrap();
        let action = doc.blocks[0]
            .card
            .as_ref()
            .unwrap()
            .action
            .as_ref()
            .unwrap();
        assert_eq!(action.label, "Abrir build");
        assert_eq!(action.url, "https://example.com/build");
        assert_eq!(
            doc.blocks[0]
                .card
                .as_ref()
                .unwrap()
                .actions
                .iter()
                .map(|action| (action.label.as_str(), action.url.as_str()))
                .collect::<Vec<_>>(),
            [
                ("Enviar", ""),
                ("Abrir build", "https://example.com/build"),
            ]
        );
        assert!(!doc.blocks[0]
            .card
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .any(|line| line == "Enviar"));

        let unsupported = Document::from_html(
            r#"<attachment contenttype="application/vnd.microsoft.card.adaptive">{"type":"AdaptiveCard","body":[{"type":"TextBlock","text":"Build","url":"https://example.com/metadata"}],"actions":[{"type":"Action.Submit","title":"Enviar","url":"https://example.com/submit"}]}</attachment>"#,
        )
        .unwrap();
        let card = unsupported.blocks[0].card.as_ref().unwrap();
        assert!(card.action.is_none());
        assert_eq!(card.actions.len(), 1);
        assert_eq!(card.actions[0].label, "Enviar");
        assert!(card.actions[0].url.is_empty());
        assert!(!card.lines.iter().any(|line| line == "Enviar"));
        assert!(!card.lines.iter().any(|line| line.contains("example.com/submit")));
    }

    #[test]
    fn jira_adaptive_card_keeps_link_labels_status_row_and_action_buttons() {
        let doc = Document::from_html(
            r#"<attachment contenttype="application/vnd.microsoft.card.adaptive">{"type":"AdaptiveCard","body":[{"type":"ColumnSet","columns":[{"type":"Column","items":[{"type":"Image","url":"https://example.com/jira-logo.png","altText":"Jira"}]},{"type":"Column","items":[{"type":"TextBlock","text":"Jira","weight":"Bolder"}]}]},{"type":"ColumnSet","columns":[{"type":"Column","items":[{"type":"Image","url":"https://example.com/bug.png","altText":"Bug"}]},{"type":"Column","items":[{"type":"TextBlock","text":"[DEMO-1931: Fix the sample billing form](https://example.atlassian.net/browse/DEMO-1931?atlOrigin=eyJabc)"}]}]},{"type":"TextBlock","text":"Assigned to Sam","isSubtle":true},{"type":"ColumnSet","columns":[{"type":"Column","items":[{"type":"TextBlock","text":"**Low**"}]},{"type":"Column","items":[{"type":"TextBlock","text":"|"}]},{"type":"Column","items":[{"type":"TextBlock","text":"**EM ANDAMENTO**"}]}]}],"actions":[{"type":"Action.Submit","title":"Comment","style":"positive"},{"type":"Action.Submit","title":"Edit"},{"type":"Action.Submit","title":"Notify","iconUrl":"https://example.com/bell.png"}]}</attachment>"#,
        )
        .unwrap();
        let card = doc.blocks[0].card.as_ref().unwrap();
        assert!(card.action.is_none());
        assert_eq!(
            card.actions
                .iter()
                .map(|action| action.label.as_str())
                .collect::<Vec<_>>(),
            ["Comment", "Edit", "Notify"]
        );
        assert!(card.actions.iter().all(|action| action.url.is_empty()));
        assert!(card.actions[0].positive);
        assert!(card.actions[2].icon);
        assert!(!card.lines.iter().any(|line| line == "Comment"));
        assert_eq!(
            card.marks
                .iter()
                .map(|mark| (mark.line, mark.kind))
                .collect::<Vec<_>>(),
            [(0, CardMarkKind::Brand), (1, CardMarkKind::Issue)]
        );
        assert_eq!(priority_label("**Low**").as_deref(), Some("Low"));
        assert!(is_status_pill("**EM ANDAMENTO**"));
        assert!(!is_status_pill("**Low**"));
        assert_eq!(card.column_rows.len(), 1);
        assert_eq!(card.column_rows[0].columns.len(), 3);
        assert!(card.lines.iter().any(|line| line.contains("**Low**")));

        let copied = doc.clipboard_payload();
        assert!(copied.html.contains("<strong>Low</strong>"));
        assert!(copied.html.contains("<strong>EM ANDAMENTO</strong>"));
        assert!(!copied.html.contains("**Low**"));
        assert!(copied.html.contains(
            "href=\"https://example.atlassian.net/browse/DEMO-1931?atlOrigin=eyJabc\""
        ));
        assert!(copied
            .html
            .contains(">DEMO-1931: Fix the sample billing form</a>"));
        assert!(copied.text.contains("DEMO-1931: Fix the sample billing form"));
        assert!(!copied.text.contains("[DEMO-1931"));
        assert!(copied.text.contains("Low"));
        assert!(copied.text.contains("EM ANDAMENTO"));
        assert!(!copied.text.contains("**"));
        assert!(copied.text.contains("Comment"));
    }

    #[test]
    fn message_card_uses_default_platform_target_for_open_uri() {
        let doc = Document::from_html(
            r#"<attachment contenttype="application/vnd.microsoft.card.o365connector">{"@type":"MessageCard","potentialAction":[{"@type":"OpenUri","name":"Abrir build","targets":[{"os":"iOS","uri":"https://ios.example.com/build"},{"os":"default","uri":"https://example.com/build"}]}]}</attachment>"#,
        )
        .unwrap();
        let action = doc.blocks[0]
            .card
            .as_ref()
            .unwrap()
            .action
            .as_ref()
            .unwrap();
        assert_eq!(action.url, "https://example.com/build");
    }

    #[test]
    fn unknown_attachment_keeps_an_identifiable_readable_fallback() {
        let doc = Document::from_html(r#"<attachment id="opaque"></attachment>"#).unwrap();
        let card = doc.blocks[0].card.as_ref().unwrap();
        assert_eq!(card.kind, "Anexo sem prévia");
        assert_eq!(card.lines, ["A prévia deste conteúdo não está disponível."]);

        let unsafe_card = Document::from_html(
            r#"<attachment contenttype="application/vnd.microsoft.card.adaptive">{"type":"AdaptiveCard","title":"Aviso","actions":[{"type":"Action.OpenUrl","title":"Abrir","url":"javascript:alert(1)"}]}</attachment>"#,
        )
        .unwrap();
        assert!(
            unsafe_card.blocks[0]
                .card
                .as_ref()
                .unwrap()
                .action
                .is_none()
        );
    }
    #[test]
    fn tables_over_column_budget_use_the_plain_text_fallback() {
        let cells = (0..13)
            .map(|index| format!("<td>{index}</td>"))
            .collect::<String>();
        let html = format!("<table><tr>{cells}</tr></table>");
        assert!(Document::from_html(&html).is_none());
    }
    #[test]
    fn unsupported_active_content_is_ignored_and_links_are_only_opened_on_click() {
        let doc = Document::from_html("<script>secret()</script><style>p{display:none}</style><p><a href='javascript:alert(1)'>texto</a> <a href='file:///tmp/a'>arquivo</a> <at>Ana</at></p>").unwrap();
        assert!(!all_text(&doc).contains("secret"));
        assert!(doc.blocks[0].runs.iter().all(|r| r.style.link.is_none()));
        assert!(doc.blocks[0].runs.iter().any(|r| r.style.mention));
        for link in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,hi",
            "https://user:pass@example.com/",
            "https://example.com/\n",
        ] {
            assert!(valid_link(link).is_none());
        }
        assert!(valid_link("https://example.com/?a=1&b=2").is_some());
        assert!(valid_link("mailto:ana@example.com").is_some());
        let doc = Document::plain("Confira https://example.com/revisao. Depois seguimos.");
        assert!(doc.blocks[0].runs.iter().any(|r| r.style.link.as_deref()
            == Some("https://example.com/revisao")
            && r.text == "https://example.com/revisao"));
        assert_eq!(
            all_text(&Document::from_html("Linha 1\nLinha 2 &amp; fim").unwrap()),
            "Linha 1\nLinha 2 & fim"
        );
    }
    #[test]
    fn inline_colors_remain_readable_in_both_themes_and_long_messages_use_the_pane() {
        for appearance in [
            crate::preferences::Appearance::Dark,
            crate::preferences::Appearance::Light,
        ] {
            let theme = crate::appearance::teams_theme(appearance);
            for surface in [Tone::Incoming, Tone::Outgoing] {
                let bg = tone(&theme, surface);
                for color in [0, 0xffffff, 0xc4314b, 0x005a9e] {
                    let result = readable_color(Some(color), bg, tone(&theme, Tone::Text));
                    assert!(contrast(result, bg) >= 4.5);
                }
                let colored_cell = crate::appearance::rgb(0x8a2d65);
                let cell_text = readable_color(None, colored_cell, tone(&theme, Tone::Text));
                assert!(contrast(cell_text, colored_cell) >= 4.5);
            }
        }
        assert_eq!(bubble_width(1600.0, false, false), 976.0);
        assert_eq!(bubble_width(880.0, false, false), 790.0);
        assert!(bubble_width(363.0, false, true) + 42.0 + 24.0 <= 363.0);
    }
}
