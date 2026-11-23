//! Image and clipboard helpers. Image wire contract provenance:
//! docs/research/image-messages.md. Clipboard RTF parsing is a bounded native
//! Linux interoperability feature, not a claim about the Teams wire contract.
use crate::{auth::TeamsSession, graph::GraphMessage};
use iced::widget::image::Handle;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::{Cursor, Read},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_CLIPBOARD_BYTES: usize = 2 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 64;
pub const MAX_IMAGES: usize = 8;
pub const INLINE_MAX_WIDTH: f32 = 400.0;
pub const INLINE_MAX_HEIGHT: f32 = 640.0;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InlineImage {
    pub id: String,
    pub url: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone)]
pub struct LoadedImage {
    pub handle: Handle,
    pub width: u32,
    pub height: u32,
    pub estimated_bytes: usize,
}

#[derive(Clone)]
pub struct OriginalImage {
    pub handle: Handle,
    pub width: u32,
    pub height: u32,
    pub bytes: Arc<Vec<u8>>,
}

#[derive(Clone)]
pub struct StagedImage {
    pub key: String,
    pub png: Arc<Vec<u8>>,
    pub preview: Handle,
    pub preview_bytes: usize,
    pub width: u32,
    pub height: u32,
    pub remote: Option<InlineImage>,
    pub uploaded: bool,
}

#[derive(Clone)]
pub enum Paste {
    Image(StagedImage),
    Html { html: String, plain: String },
    Text(String),
    Empty,
}

pub async fn read_clipboard() -> Result<Paste, String> {
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("teams-clipboard".into())
        .spawn(move || {
            let _ = sender.send(read_clipboard_blocking());
        })
        .map_err(|_| "Não foi possível iniciar a colagem.".to_owned())?;
    receiver.await.map_err(|_| "A leitura da área de transferência foi interrompida.".to_owned())?
}

trait ClipboardSource {
    fn image(&mut self) -> Result<Option<StagedImage>, String>;
    fn html(&mut self) -> Option<String>;
    fn rtf(&mut self) -> Option<Vec<u8>>;
    fn text(&mut self) -> Option<String>;
}

struct SystemClipboard {
    native: Option<arboard::Clipboard>,
}

impl ClipboardSource for SystemClipboard {
    fn image(&mut self) -> Result<Option<StagedImage>, String> {
        if let Some(bytes) = read_wayland_clipboard_image()? {
            return stage_image_bytes(bytes).map(Some);
        }
        match self.native.as_mut().and_then(|clipboard| clipboard.get_image().ok()) {
            Some(pixels) => stage_clipboard_pixels(pixels).map(Some),
            None => Ok(None),
        }
    }

    fn html(&mut self) -> Option<String> {
        read_wayland_clipboard_html().or_else(|| {
            self.native.as_mut().and_then(|clipboard| clipboard.get().html().ok())
        })
    }

    fn rtf(&mut self) -> Option<Vec<u8>> {
        read_wayland_clipboard_rtf()
    }

    fn text(&mut self) -> Option<String> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            && let Some(text) = read_wayland_clipboard_text().filter(|text| !text.is_empty())
        {
            return Some(text);
        }
        self.native
            .as_mut()
            .and_then(|clipboard| clipboard.get_text().ok())
            .or_else(read_wayland_clipboard_text)
    }
}

fn read_clipboard_blocking() -> Result<Paste, String> {
    let mut source = SystemClipboard { native: arboard::Clipboard::new().ok() };
    if source.native.is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err("Não foi possível acessar a área de transferência.".into());
    }
    read_clipboard_from(&mut source)
}

fn read_clipboard_from(source: &mut impl ClipboardSource) -> Result<Paste, String> {
    if let Some(image) = source.image()? {
        return Ok(Paste::Image(image));
    }
    let html = source.html().and_then(normalize_clipboard_html);
    let plain = source.text()
        .filter(|text| !text.is_empty() && text.len() <= MAX_CLIPBOARD_BYTES);
    if let Some(html) = html {
        return Ok(Paste::Html { html, plain: plain.unwrap_or_default() });
    }
    if let Some(html) = source.rtf().and_then(|rtf| clipboard_rtf_to_html(&rtf)) {
        return Ok(Paste::Html { html, plain: plain.unwrap_or_default() });
    }
    if let Some(plain) = plain {
        return Ok(Paste::Text(plain));
    }
    Ok(Paste::Empty)
}

fn clipboard_image_mime(offered: &HashSet<String>) -> Option<&'static str> {
    ["image/png", "image/jpeg", "image/jpg", "image/webp", "image/bmp", "image/x-bmp", "image/gif", "image/tiff", "image/x-tiff"]
        .into_iter()
        .find(|mime| offered.contains(*mime))
}

fn read_wayland_clipboard_image() -> Result<Option<Vec<u8>>, String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Ok(None);
    }
    use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents, get_mime_types};
    let Ok(offered) = get_mime_types(ClipboardType::Regular, Seat::Unspecified) else {
        return Ok(None);
    };
    let Some(mime) = clipboard_image_mime(&offered) else {
        return Ok(None);
    };
    let Ok((file, _)) = get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        MimeType::Specific(mime),
    ) else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Não foi possível ler a imagem copiada.".to_owned())?;
    if bytes.len() > MAX_BYTES {
        return Err("A imagem ultrapassa o limite de 32 MB.".into());
    }
    Ok(Some(bytes))
}

fn read_wayland_clipboard_rtf() -> Option<Vec<u8>> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return None;
    }
    use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};

    for mime in [
        "text/rtf",
        "text/rtf;charset=utf-8",
        "application/rtf",
        "application/x-rtf",
        "text/richtext",
    ] {
        let Ok((file, _)) = get_contents(
            ClipboardType::Regular,
            Seat::Unspecified,
            MimeType::Specific(mime),
        ) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take((MAX_CLIPBOARD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .is_ok()
            && bytes.len() <= MAX_CLIPBOARD_BYTES
        {
            return Some(bytes);
        }
    }
    None
}

fn read_wayland_clipboard_html() -> Option<String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return None;
    }
    use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};

    for mime in ["text/html", "text/html;charset=utf-8"] {
        let Ok((file, _)) = get_contents(
            ClipboardType::Regular,
            Seat::Unspecified,
            MimeType::Specific(mime),
        ) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file
            .take((MAX_CLIPBOARD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .is_ok()
            && bytes.len() <= MAX_CLIPBOARD_BYTES
        {
            if let Ok(html) = String::from_utf8(bytes) {
                return Some(html);
            }
        }
    }
    None
}

fn read_wayland_clipboard_text() -> Option<String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return None;
    }
    use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};

    let (file, _) = get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        MimeType::Text,
    )
    .ok()?;
    let mut bytes = Vec::new();
    file.take((MAX_CLIPBOARD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_CLIPBOARD_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RtfStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    hidden: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RtfEncoding {
    Windows1252,
    Utf8,
}

#[derive(Clone, Copy, Debug)]
struct RtfState {
    style: RtfStyle,
    encoding: RtfEncoding,
    unicode_fallback: usize,
    skipped: bool,
}

impl Default for RtfState {
    fn default() -> Self {
        Self {
            style: RtfStyle::default(),
            encoding: RtfEncoding::Windows1252,
            unicode_fallback: 1,
            skipped: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RtfGroup {
    state: RtfState,
    at_start: bool,
    starred: bool,
}

/// Converts a small, deliberately conservative RTF subset into escaped HTML
/// understood by the native composer. This is for user-triggered clipboard
/// paste; no active content, links, images, or embedded objects are emitted.
fn clipboard_rtf_to_html(rtf: &[u8]) -> Option<String> {
    if rtf.len() > MAX_CLIPBOARD_BYTES || !rtf.starts_with(b"{\\rtf") {
        return None;
    }

    let mut parser = RtfParser {
        input: rtf,
        index: 0,
        state: RtfState::default(),
        groups: Vec::new(),
        byte_run: Vec::new(),
        html: String::new(),
        output_style: RtfStyle::default(),
        fallback_remaining: 0,
        pending_high_surrogate: None,
        root_closed: false,
    };
    parser.parse()?;
    (!parser.html.trim().is_empty()).then_some(parser.html)
}

struct RtfParser<'a> {
    input: &'a [u8],
    index: usize,
    state: RtfState,
    groups: Vec<RtfGroup>,
    byte_run: Vec<u8>,
    html: String,
    output_style: RtfStyle,
    fallback_remaining: usize,
    pending_high_surrogate: Option<u16>,
    root_closed: bool,
}

impl RtfParser<'_> {
    fn parse(&mut self) -> Option<()> {
        while self.index < self.input.len() {
            if self.root_closed {
                if !self.input[self.index].is_ascii_whitespace() {
                    return None;
                }
                self.index += 1;
                continue;
            }
            match self.input[self.index] {
                b'{' => self.open_group()?,
                b'}' => self.close_group()?,
                b'\\' => self.parse_escape()?,
                b'\r' | b'\n' => {
                    self.index += 1;
                }
                byte => {
                    self.mark_group_content();
                    self.index += 1;
                    if self.state.skipped {
                        continue;
                    }
                    if self.fallback_remaining > 0 {
                        self.fallback_remaining -= 1;
                    } else {
                        self.byte_run.push(byte);
                        if self.byte_run.len() > MAX_CLIPBOARD_BYTES {
                            return None;
                        }
                    }
                }
            }
        }
        if !self.root_closed || !self.groups.is_empty() {
            return None;
        }
        self.flush_bytes()?;
        self.flush_pending_surrogate()?;
        self.sync_style(RtfStyle::default())?;
        Some(())
    }

    fn open_group(&mut self) -> Option<()> {
        self.flush_bytes()?;
        if self.groups.len() >= 128 {
            return None;
        }
        self.index += 1;
        self.groups.push(RtfGroup {
            state: self.state,
            at_start: true,
            starred: false,
        });
        Some(())
    }

    fn close_group(&mut self) -> Option<()> {
        self.flush_bytes()?;
        self.flush_pending_surrogate()?;
        self.index += 1;
        let group = self.groups.pop()?;
        self.state = group.state;
        if self.groups.is_empty() {
            self.root_closed = true;
        }
        self.sync_style(self.state.style)?;
        Some(())
    }

    fn mark_group_content(&mut self) {
        if let Some(group) = self.groups.last_mut() {
            group.at_start = false;
            group.starred = false;
        }
    }

    fn parse_escape(&mut self) -> Option<()> {
        self.flush_bytes()?;
        self.index += 1;
        let next = *self.input.get(self.index)?;
        if next.is_ascii_alphabetic() {
            return self.parse_control_word();
        }
        self.index += 1;
        if self.state.skipped {
            return Some(());
        }
        match next {
            b'\\' | b'{' | b'}' => {
                self.mark_group_content();
                if self.fallback_remaining > 0 {
                    self.fallback_remaining -= 1;
                } else {
                    self.byte_run.push(next);
                }
            }
            b'\'' => {
                self.mark_group_content();
                let hi = *self.input.get(self.index)?;
                let lo = *self.input.get(self.index + 1)?;
                let value = (hex(hi)? << 4) | hex(lo)?;
                self.index += 2;
                if self.fallback_remaining > 0 {
                    self.fallback_remaining -= 1;
                } else {
                    self.byte_run.push(value);
                }
            }
            b'*' => {
                if let Some(group) = self.groups.last_mut()
                    && group.at_start
                {
                    group.starred = true;
                }
            }
            b'~' => {
                self.mark_group_content();
                self.consume_fallback_or_character('\u{00a0}')?;
            }
            b'_' => {
                self.mark_group_content();
                self.consume_fallback_or_character('\u{2011}')?;
            }
            b'-' => {
                self.mark_group_content();
                if self.fallback_remaining > 0 {
                    self.fallback_remaining -= 1;
                }
            }
            _ => self.mark_group_content(),
        }
        Some(())
    }

    fn parse_control_word(&mut self) -> Option<()> {
        let start = self.index;
        while self.input.get(self.index).is_some_and(u8::is_ascii_alphabetic) {
            self.index += 1;
        }
        let word = std::str::from_utf8(&self.input[start..self.index])
            .ok()?
            .to_ascii_lowercase();
        let negative = self.input.get(self.index) == Some(&b'-');
        if negative {
            self.index += 1;
        }
        let number_start = self.index;
        while self.input.get(self.index).is_some_and(u8::is_ascii_digit) {
            self.index += 1;
        }
        let number = if number_start == self.index {
            None
        } else {
            let value = std::str::from_utf8(&self.input[number_start..self.index])
                .ok()?
                .parse::<i32>()
                .ok()?;
            Some(if negative { -value } else { value })
        };
        if self.input.get(self.index) == Some(&b' ') {
            self.index += 1;
        }

        if self.state.skipped {
            return Some(());
        }
        if self.fallback_remaining > 0 {
            self.fallback_remaining -= 1;
            return Some(());
        }

        if let Some(group) = self.groups.last_mut()
            && group.at_start
        {
            group.at_start = false;
            if group.starred || is_rtf_destination(&word) {
                self.state.skipped = true;
                return Some(());
            }
        }
        self.mark_group_content();

        match word.as_str() {
            "ansicpg" => {
                self.flush_pending_surrogate()?;
                self.state.encoding = match number? {
                    1252 => RtfEncoding::Windows1252,
                    65001 => RtfEncoding::Utf8,
                    _ => return None,
                };
            }
            "ansi" => self.state.encoding = RtfEncoding::Windows1252,
            "uc" => self.state.unicode_fallback = number?.clamp(0, 16) as usize,
            "u" => {
                let value = number?;
                if !(i32::from(i16::MIN)..=i32::from(i16::MAX)).contains(&value) {
                    return None;
                }
                self.flush_pending_surrogate_if_not_pair(Some(value))?;
                let unit = value as i16 as u16;
                self.write_unicode_unit(unit)?;
                self.fallback_remaining = self.state.unicode_fallback;
            }
            "bin" => {
                let length = usize::try_from(number?).ok()?;
                let end = self.index.checked_add(length)?;
                if end > self.input.len() {
                    return None;
                }
                self.index = end;
            }
            "b" => self.state.style.bold = number.unwrap_or(1) != 0,
            "i" => self.state.style.italic = number.unwrap_or(1) != 0,
            "ul" | "uldb" | "ulw" | "uld" => {
                self.state.style.underline = number.unwrap_or(1) != 0;
            }
            "ulnone" | "ul0" => self.state.style.underline = false,
            "strike" | "striked" => self.state.style.strike = number.unwrap_or(1) != 0,
            "v" => self.state.style.hidden = number.unwrap_or(1) != 0,
            "plain" => self.state.style = RtfStyle::default(),
            "par" | "line" => {
                self.flush_pending_surrogate()?;
                if !self.state.style.hidden {
                    self.sync_style(self.state.style)?;
                    self.push_html("<br>")?;
                }
            }
            "tab" => self.consume_fallback_or_character('\t')?,
            "emdash" => self.consume_fallback_or_character('\u{2014}')?,
            "endash" => self.consume_fallback_or_character('\u{2013}')?,
            "bullet" => self.consume_fallback_or_character('\u{2022}')?,
            "lquote" => self.consume_fallback_or_character('\u{2018}')?,
            "rquote" => self.consume_fallback_or_character('\u{2019}')?,
            "ldblquote" => self.consume_fallback_or_character('\u{201c}')?,
            "rdblquote" => self.consume_fallback_or_character('\u{201d}')?,
            _ => {}
        }
        if matches!(
            word.as_str(),
            "b" | "i"
                | "ul"
                | "uldb"
                | "ulw"
                | "uld"
                | "ulnone"
                | "ul0"
                | "strike"
                | "striked"
                | "v"
                | "plain"
        ) {
            self.flush_pending_surrogate()?;
            self.sync_style(self.state.style)?;
        }
        Some(())
    }

    fn flush_bytes(&mut self) -> Option<()> {
        if self.byte_run.is_empty() {
            return Some(());
        }
        let bytes = std::mem::take(&mut self.byte_run);
        let text = match self.state.encoding {
            RtfEncoding::Windows1252 => {
                let (decoded, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(&bytes);
                decoded.into_owned()
            }
            RtfEncoding::Utf8 => String::from_utf8(bytes).ok()?,
        };
        self.flush_pending_surrogate()?;
        for character in text.chars() {
            self.append_character(character)?;
        }
        Some(())
    }

    fn flush_pending_surrogate_if_not_pair(&mut self, next: Option<i32>) -> Option<()> {
        if self.pending_high_surrogate.is_some()
            && !next.is_some_and(|value| (0xdc00..=0xdfff).contains(&(value as i16 as u16)))
        {
            self.flush_pending_surrogate()?;
        }
        Some(())
    }

    fn write_unicode_unit(&mut self, unit: u16) -> Option<()> {
        match unit {
            0xd800..=0xdbff => {
                self.flush_pending_surrogate()?;
                self.pending_high_surrogate = Some(unit);
            }
            0xdc00..=0xdfff => {
                if let Some(high) = self.pending_high_surrogate.take() {
                    let scalar = 0x10000
                        + (((u32::from(high) - 0xd800) << 10) | (u32::from(unit) - 0xdc00));
                    self.append_character(char::from_u32(scalar)?)?;
                } else {
                    self.append_character('\u{fffd}')?;
                }
            }
            _ => {
                self.flush_pending_surrogate()?;
                self.append_character(char::from_u32(u32::from(unit))?)?;
            }
        }
        Some(())
    }

    fn flush_pending_surrogate(&mut self) -> Option<()> {
        if self.pending_high_surrogate.take().is_some() {
            self.append_character('\u{fffd}')?;
        }
        Some(())
    }

    fn consume_fallback_or_character(&mut self, character: char) -> Option<()> {
        if self.fallback_remaining > 0 {
            self.fallback_remaining -= 1;
        } else {
            self.append_character(character)?;
        }
        Some(())
    }

    fn append_character(&mut self, character: char) -> Option<()> {
        if self.state.style.hidden {
            return Some(());
        }
        self.sync_style(self.state.style)?;
        let escaped = match character {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&#39;".to_owned(),
            other => other.to_string(),
        };
        self.push_html(&escaped)
    }

    fn sync_style(&mut self, style: RtfStyle) -> Option<()> {
        let style = if style.hidden { RtfStyle::default() } else { style };
        if style == self.output_style {
            return Some(());
        }
        for tag in active_rtf_tags(self.output_style).into_iter().rev() {
            self.push_html(&format!("</{tag}>"))?;
        }
        for tag in active_rtf_tags(style) {
            self.push_html(&format!("<{tag}>"))?;
        }
        self.output_style = style;
        Some(())
    }

    fn push_html(&mut self, html: &str) -> Option<()> {
        if self.html.len().checked_add(html.len())? > MAX_CLIPBOARD_BYTES {
            return None;
        }
        self.html.push_str(html);
        Some(())
    }
}

fn active_rtf_tags(style: RtfStyle) -> Vec<&'static str> {
    let mut tags = Vec::with_capacity(4);
    if style.bold {
        tags.push("strong");
    }
    if style.italic {
        tags.push("em");
    }
    if style.underline {
        tags.push("u");
    }
    if style.strike {
        tags.push("s");
    }
    tags
}

fn is_rtf_destination(word: &str) -> bool {
    matches!(
        word,
        "fonttbl"
            | "colortbl"
            | "stylesheet"
            | "info"
            | "generator"
            | "pict"
            | "object"
            | "objdata"
            | "header"
            | "headerl"
            | "headerr"
            | "footer"
            | "footerl"
            | "footerr"
            | "listtable"
            | "listoverridetable"
            | "revtbl"
            | "xmlnstbl"
            | "datastore"
            | "themedata"
            | "colorschememapping"
            | "latentstyles"
            | "fldinst"
    )
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Strip the byte-offset envelope used by CF_HTML, while bounding raw clipboard
/// markup before it reaches the HTML parser.
pub fn normalize_clipboard_html(html: String) -> Option<String> {
    if html.len() > MAX_CLIPBOARD_BYTES {
        return None;
    }
    if !html.as_bytes().starts_with(b"Version:") {
        return (!html.trim().is_empty()).then_some(html);
    }
    let lowercase = html.to_ascii_lowercase();
    let offset = |name: &str| -> Option<usize> {
        let index = lowercase.find(&name.to_ascii_lowercase())? + name.len();
        let value = lowercase[index..].lines().next()?.trim();
        value.parse().ok()
    };
    let start = offset("StartFragment:").or_else(|| offset("StartHTML:"))?;
    let end = offset("EndFragment:").or_else(|| offset("EndHTML:"))?;
    if start > end || end > html.len() {
        return None;
    }
    let fragment = html.get(start..end)?.trim();
    (!fragment.is_empty()).then(|| fragment.to_owned())
}

fn check_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || u64::from(width) * u64::from(height) > 64 * 1024 * 1024
    {
        Err("A imagem é grande demais. Use uma captura de até 64 megapixels.".into())
    } else {
        Ok(())
    }
}

fn decoded_preview(bytes: &[u8]) -> Result<(Handle, u32, u32, usize), String> {
    if bytes.len() > MAX_BYTES {
        return Err("A imagem ultrapassa o limite de 32 MB.".into());
    }
    let reader = || {
        image_codec::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|_| "Imagem inválida.".to_owned())
    };
    let (width, height) = reader()?
        .into_dimensions()
        .map_err(|_| "Não foi possível ler a imagem.".to_owned())?;
    check_dimensions(width, height)?;
    let mut reader = reader()?;
    let mut limits = image_codec::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    reader.limits(limits);
    let pixels = preview_bitmap(
        reader
            .decode()
            .map_err(|_| "Não foi possível decodificar a imagem.".to_owned())?,
    );
    let estimated_bytes = pixels.as_raw().len();
    Ok((
        Handle::from_rgba(pixels.width(), pixels.height(), pixels.into_raw()),
        width,
        height,
        estimated_bytes,
    ))
}

fn decoded_original_pixels(bytes: &[u8]) -> Result<image_codec::RgbaImage, String> {
    if bytes.len() > MAX_BYTES {
        return Err("A imagem ultrapassa o limite de 32 MB.".into());
    }
    let reader = image_codec::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "Imagem inválida.".to_owned())?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| "Não foi possível ler a imagem.".to_owned())?;
    check_dimensions(width, height)?;
    let mut reader = image_codec::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "Imagem inválida.".to_owned())?;
    let mut limits = image_codec::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    reader.limits(limits);
    reader
        .decode()
        .map(|image| image.to_rgba8())
        .map_err(|_| "Não foi possível decodificar a imagem original.".to_owned())
}

pub fn decode_original(bytes: Vec<u8>) -> Result<OriginalImage, String> {
    let pixels = decoded_original_pixels(&bytes)?;
    let (width, height) = (pixels.width(), pixels.height());
    Ok(OriginalImage {
        handle: Handle::from_rgba(width, height, pixels.into_raw()),
        width,
        height,
        bytes: Arc::new(bytes),
    })
}

pub fn copy_original_to_clipboard(bytes: Arc<Vec<u8>>) -> Result<(), String> {
    let pixels = decoded_original_pixels(&bytes)?;
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|_| "Não foi possível acessar a área de transferência.".to_owned())?;
    clipboard
        .set_image(arboard::ImageData {
            width: pixels.width() as usize,
            height: pixels.height() as usize,
            bytes: std::borrow::Cow::Owned(pixels.into_raw()),
        })
        .map_err(|_| "Não foi possível copiar a imagem para a área de transferência.".to_owned())
}

pub fn save_original_with_dialog(
    image: OriginalImage,
) -> Result<Option<std::path::PathBuf>, String> {
    let extension = original_extension(&image.bytes);
    let filename = format!("teams-image.{extension}");
    let path = choose_save_path(&filename)?;
    let Some(path) = path else {
        return Ok(None);
    };
    let path = with_original_extension(path, extension);
    write_original_to_path(&path, image.bytes.as_slice())?;
    Ok(Some(path))
}

fn with_original_extension(mut path: std::path::PathBuf, extension: &str) -> std::path::PathBuf {
    let actual = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    let correct = match extension {
        "png" => actual.eq_ignore_ascii_case("png"),
        "jpg" => matches!(actual.to_ascii_lowercase().as_str(), "jpg" | "jpeg"),
        _ => false,
    };
    if !correct {
        path.set_extension(extension);
    }
    path
}

fn write_original_to_path(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes)
        .map_err(|_| "Não foi possível salvar a imagem escolhida.".to_owned())
}

fn choose_save_path(filename: &str) -> Result<Option<std::path::PathBuf>, String> {
    let zenity = std::process::Command::new("zenity")
        .args([
            "--file-selection",
            "--save",
            "--confirm-overwrite",
            "--title=Salvar imagem",
            &format!("--filename={filename}"),
            "--file-filter=Imagens | *.png *.jpg *.jpeg",
        ])
        .output();
    match zenity {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            return Ok((!path.is_empty()).then(|| path.into()));
        }
        Ok(output) if output.status.code() == Some(1) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Err("O seletor de arquivos do sistema falhou.".into()),
        Err(_) => return Err("Não foi possível abrir o seletor de arquivos do sistema.".into()),
    }

    let kdialog = std::process::Command::new("kdialog")
        .arg("--getsavefilename")
        .arg(filename)
        .arg("Imagens (*.png *.jpg *.jpeg)")
        .output();
    match kdialog {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            Ok((!path.is_empty()).then(|| path.into()))
        }
        Ok(output) if output.status.code() == Some(1) => Ok(None),
        Ok(_) => Err("O seletor de arquivos do sistema falhou.".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("Instale zenity ou kdialog para salvar imagens pelo seletor do sistema.".into())
        }
        Err(_) => Err("Não foi possível abrir o seletor de arquivos do sistema.".into()),
    }
}

fn original_extension(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "jpg"
    } else {
        "png"
    }
}

pub fn stage_png(png: Vec<u8>) -> Result<StagedImage, String> {
    if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("A captura não contém um PNG válido.".into());
    }
    let (preview, width, height, preview_bytes) = decoded_preview(&png)?;
    Ok(staged_image(png, preview, width, height, preview_bytes))
}

fn stage_clipboard_pixels(pixels: arboard::ImageData<'_>) -> Result<StagedImage, String> {
    let width = u32::try_from(pixels.width).map_err(|_| "Largura de imagem inválida.".to_owned())?;
    let height = u32::try_from(pixels.height).map_err(|_| "Altura de imagem inválida.".to_owned())?;
    check_dimensions(width, height)?;
    let expected = u64::from(width) * u64::from(height) * 4;
    if pixels.bytes.len() as u64 != expected {
        return Err("Os pixels da imagem copiada estão incompletos.".into());
    }
    let pixels = image_codec::RgbaImage::from_raw(width, height, pixels.bytes.into_owned())
        .ok_or_else(|| "Não foi possível ler os pixels da imagem copiada.".to_owned())?;
    let original = image_codec::DynamicImage::ImageRgba8(pixels);
    let preview = preview_bitmap(original.clone());
    let preview_bytes = preview.as_raw().len();
    let handle = Handle::from_rgba(preview.width(), preview.height(), preview.into_raw());
    let mut png = Cursor::new(Vec::new());
    original.write_to(&mut png, image_codec::ImageFormat::Png)
        .map_err(|_| "Não foi possível preparar a imagem copiada.".to_owned())?;
    let png = png.into_inner();
    if png.len() > MAX_BYTES {
        return Err("A imagem ultrapassa o limite de 32 MB.".into());
    }
    Ok(staged_image(png, handle, width, height, preview_bytes))
}

fn staged_image(
    png: Vec<u8>,
    preview: Handle,
    width: u32,
    height: u32,
    preview_bytes: usize,
) -> StagedImage {
    let key = format!("{:x}-{:x}", Sha256::digest(&png), rand::random::<u64>());
    StagedImage {
        key,
        png: Arc::new(png),
        preview,
        preview_bytes,
        width,
        height,
        remote: None,
        uploaded: false,
    }
}

pub fn stage_image_file(path: &std::path::Path) -> Result<StagedImage, String> {
    let file = std::fs::File::open(path)
        .map_err(|_| "Não foi possível ler o arquivo selecionado.".to_owned())?;
    let size = file
        .metadata()
        .map_err(|_| "Não foi possível verificar o tamanho do arquivo.".to_owned())?
        .len();
    if size > MAX_BYTES as u64 {
        return Err("A imagem ultrapassa o limite de 32 MB.".into());
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Não foi possível ler o arquivo selecionado.".to_owned())?;
    stage_image_bytes(bytes)
}

fn stage_image_bytes(bytes: Vec<u8>) -> Result<StagedImage, String> {
    if bytes.len() > MAX_BYTES {
        return Err("A imagem ultrapassa o limite de 32 MB.".into());
    }
    let reader = || {
        image_codec::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|_| "Imagem inválida.".to_owned())
    };
    match reader()?.format() {
        Some(image_codec::ImageFormat::Png) => stage_png(bytes),
        Some(
            image_codec::ImageFormat::Jpeg
            | image_codec::ImageFormat::WebP
            | image_codec::ImageFormat::Bmp
            | image_codec::ImageFormat::Gif
            | image_codec::ImageFormat::Tiff,
        ) => stage_png(encode_raster_image_to_png(&bytes)?),
        _ => Err("Anexe uma imagem PNG, JPEG, WebP, BMP, GIF ou TIFF.".into()),
    }
}

fn encode_raster_image_to_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let pixels = decoded_original_pixels(bytes)?;
    let mut png = Cursor::new(Vec::new());
    image_codec::DynamicImage::ImageRgba8(pixels)
        .write_to(&mut png, image_codec::ImageFormat::Png)
        .map_err(|_| "Não foi possível preparar a imagem.".to_owned())?;
    Ok(png.into_inner())
}

/// Open the system file chooser for one arbitrary attachment. Images continue
/// through the existing image pipeline after selection; other files are sent
/// as OneDrive/SharePoint chat attachments.
pub fn choose_attachment_path() -> Result<Option<std::path::PathBuf>, String> {
    let portal: Result<Option<std::path::PathBuf>, String> =
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "Não foi possível iniciar o seletor de arquivos.".to_owned())?
            .block_on(async {
                use ashpd::desktop::{ResponseError, file_chooser::SelectedFiles};

                let request = SelectedFiles::open_file()
                    .title("Anexar arquivo")
                    .accept_label("Anexar")
                    .multiple(false)
                    .modal(true)
                    .send()
                    .await
                    .map_err(|_| "O seletor do sistema não está disponível.".to_owned())?;
                let selected = match request.response() {
                    Ok(selected) => selected,
                    Err(ashpd::Error::Response(ResponseError::Cancelled)) => return Ok(None),
                    Err(_) => return Err("Não foi possível concluir a seleção do arquivo.".into()),
                };
                let Some(uri) = selected.uris().first() else {
                    return Ok(None);
                };
                let path = uri
                    .to_file_path()
                    .map_err(|_| "O seletor retornou um arquivo local inválido.".to_owned())?;
                Ok(Some(path))
            });

    match portal {
        Ok(selection) => Ok(selection),
        Err(_) => choose_attachment_path_with_zenity(),
    }
}

pub fn is_raster_attachment(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "bmp" | "gif" | "tif" | "tiff"
            )
        })
}

fn choose_attachment_path_with_zenity() -> Result<Option<std::path::PathBuf>, String> {
    match std::process::Command::new("zenity")
        .args(["--file-selection", "--title=Anexar arquivo"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            Ok((!path.is_empty()).then(|| path.into()))
        }
        Ok(output) if output.status.code() == Some(1) => Ok(None),
        Ok(_) => Err("O seletor de arquivos do sistema falhou.".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::process::Command::new("kdialog")
                .args(["--getopenfilename", "", "*", "--title", "Anexar arquivo"])
                .output()
            {
                Ok(output) if output.status.success() => {
                    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    Ok((!path.is_empty()).then(|| path.into()))
                }
                Ok(output) if output.status.code() == Some(1) => Ok(None),
                Ok(_) => Err("O seletor de arquivos do sistema falhou.".into()),
                Err(_) => Err("Instale um portal de arquivos, zenity ou kdialog.".into()),
            }
        }
        Err(_) => Err("Não foi possível abrir o seletor de arquivos do sistema.".into()),
    }
}

pub fn demo_image() -> StagedImage {
    let pixels = image_codec::RgbImage::from_fn(1280, 720, |x, y| {
        image_codec::Rgb(if y < 56 {
            [36, 38, 48]
        } else if x < 240 {
            [54, 56, 72]
        } else if y > 560 {
            [228, 231, 244]
        } else if (x / 160 + y / 100) % 2 == 0 {
            [111, 121, 206]
        } else {
            [179, 188, 231]
        })
    });
    let mut bytes = Cursor::new(Vec::new());
    image_codec::DynamicImage::ImageRgb8(pixels)
        .write_to(&mut bytes, image_codec::ImageFormat::Png)
        .expect("valid demo PNG");
    stage_png(bytes.into_inner()).expect("bounded demo capture")
}

pub fn fingerprint(caption: &str, pictures: &[StagedImage]) -> String {
    let mut hash = Sha256::new();
    hash.update(caption.as_bytes());
    for picture in pictures {
        hash.update([0]);
        hash.update(picture.key.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

/// File previews use the same proven AMS transport as inline images. SharePoint
/// object URLs and non-image previews remain file cards, never image downloads.
pub fn file_preview_url(file: &ost::api::ChatFileRef) -> Option<&str> {
    let kind = file.file_type.as_deref().or_else(|| file.name.rsplit_once('.').map(|(_, ext)| ext))?;
    if !["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"].iter().any(|ext|kind.eq_ignore_ascii_case(ext)) {
        return None;
    }
    let preview = file.preview.as_ref()?;
    valid_media_url(&preview.url).then_some(preview.url.as_str())
}

pub fn file_preview(file: &ost::api::ChatFileRef) -> Option<InlineImage> {
    let url = file_preview_url(file)?;
    let preview = file.preview.as_ref()?;
    Some(InlineImage {
        id: file.attachment_id.clone().unwrap_or_default(),
        url: url.to_owned(), width: preview.width, height: preview.height,
    })
}

pub fn parse_inline_images(html: &str) -> Vec<InlineImage> {
    let html = scraper::Html::parse_fragment(html);
    let selector = scraper::Selector::parse("img[src]").expect("static image selector");
    html.select(&selector)
        .filter_map(|node| parse_inline_image_attributes(node.value()))
        .take(MAX_IMAGES)
        .collect()
}

pub fn parse_inline_image_attributes(element: &scraper::node::Element) -> Option<InlineImage> {
    let url = element.attr("src")?;
    if !valid_media_url(url) {
        return None;
    }
    let id = element
        .attr("itemid")
        .or_else(|| element.attr("id"))
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 200
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .unwrap_or_default()
        .to_owned();
    let width = safe_dimension(element.attr("width"));
    let height = safe_dimension(element.attr("height"));
    let (width, height) =
        if width > 0 && height > 0 && u64::from(width) * u64::from(height) <= 64 * 1024 * 1024 {
            (width, height)
        } else {
            (0, 0)
        };
    Some(InlineImage {
        id,
        url: url.to_owned(),
        width,
        height,
    })
}

fn safe_dimension(value: Option<&str>) -> u32 {
    value
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| (1..=16_384).contains(value))
        .unwrap_or(0)
}

pub fn display_size(
    image: &InlineImage,
    natural_width: u32,
    natural_height: u32,
    max_width: f32,
    max_height: f32,
) -> (f32, f32) {
    let (width, height) = match (natural_width, natural_height, image.width, image.height) {
        (width, height, _, _) if width > 0 && height > 0 => (width as f32, height as f32),
        (_, _, width, height) if width > 0 && height > 0 => (width as f32, height as f32),
        _ => (320.0, 240.0),
    };
    let max_width = if max_width.is_finite() {
        max_width.clamp(1.0, 960.0)
    } else {
        480.0
    };
    let max_height = if max_height.is_finite() {
        max_height.clamp(1.0, 960.0)
    } else {
        480.0
    };
    fit_size(width, height, max_width, max_height, false)
}

pub fn fit_size(width: f32, height: f32, max_width: f32, max_height: f32, upscale: bool) -> (f32, f32) {
    let scale = (max_width / width).min(max_height / height);
    let scale = if upscale { scale } else { scale.min(1.0) };
    (width * scale, height * scale)
}

pub fn valid_media_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    let host = url.host_str().unwrap_or("");
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && (host == "api.asm.skype.com"
            || host.ends_with(".asm.skype.com")
            || host.ends_with(".asyncgw.teams.microsoft.com"))
        && url.path().starts_with("/v1/objects/")
        && url.path().contains("/views/")
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .new_agent()
    })
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn message_body(caption: &str, images: &[InlineImage], client_id: &str) -> Value {
    let mut content = if caption.is_empty() {
        String::new()
    } else {
        format!("<p>{}</p>", escape(caption))
    };
    for image in images {
        content.push_str(&format!("<img src=\"{}\" href=\"{}\" target-src=\"{}\" id=\"{}\" itemid=\"{}\" itemtype=\"http://schema.skype.com/AMSImage\" width=\"{}\" height=\"{}\" alt=\"Imagem\" />",
            escape(&image.url), escape(&image.url), escape(&image.url), escape(&image.id), escape(&image.id), image.width, image.height));
    }
    json!({"messagetype":"RichText/Html", "contenttype":"text", "content":content,
        "clientmessageid":client_id, "amsreferences":images.iter().map(|image| &image.id).collect::<Vec<_>>(),
        "properties":{"importance":"normal"}})
}

#[derive(Clone)]
pub struct SendResult {
    pub message: GraphMessage,
    pub pictures: Vec<StagedImage>,
}
#[derive(Clone)]
pub struct SendError {
    pub message: String,
    pub pictures: Vec<StagedImage>,
    pub publication_started: bool,
}

pub fn send(
    session: TeamsSession,
    chat_id: &str,
    caption: &str,
    mut pictures: Vec<StagedImage>,
    client_id: &str,
    retry_publication: bool,
) -> Result<SendResult, SendError> {
    let mut publication_started = retry_publication;
    let outcome = (|| {
        if retry_publication {
            // A lost acknowledgement must not create another copy of the image.
            // If verification fails, preserve the draft instead of posting blind.
            let (_, page) = crate::graph::load_chat_page(
                session.clone(),
                chat_id,
                &session.account_hint().0,
                None,
            )?;
            if let Some(message) = find_prior_publication(page.messages, client_id) {
                return Ok(message);
            }
        }
        let (chat_base, media_base, token) = session.image_credentials()?;
        for picture in &mut pictures {
            upload(&media_base, &token, chat_id, picture)?;
        }
        let images: Vec<_> = pictures.iter().filter_map(|p| p.remote.clone()).collect();
        publication_started = true;
        publish(
            &chat_base,
            &token,
            chat_id,
            caption,
            images,
            client_id,
            &session.account_hint().1,
        )
    })();
    match outcome {
        Ok(message) => Ok(SendResult { message, pictures }),
        Err(message) => Err(SendError {
            message,
            pictures,
            publication_started,
        }),
    }
}

fn find_prior_publication(messages: Vec<GraphMessage>, client_id: &str) -> Option<GraphMessage> {
    messages
        .into_iter()
        .find(|message| message.client_message_id.as_deref() == Some(client_id))
}

fn publish(
    chat_base: &str,
    token: &str,
    chat_id: &str,
    caption: &str,
    images: Vec<InlineImage>,
    client_id: &str,
    display_name: &str,
) -> Result<GraphMessage, String> {
    let mut body = message_body(caption, &images, client_id);
    body["imdisplayname"] = json!(display_name);
    let rich = body["content"]
        .as_str()
        .and_then(crate::message_body::Document::from_html)
        .map(std::sync::Arc::new);
    let path = format!(
        "{}/v1/users/ME/conversations/{}/messages",
        chat_base.trim_end_matches('/'),
        urlencoding::encode(chat_id)
    );
    let mut response = agent()
        .post(&path)
        .header("Authentication", &format!("skypetoken={token}"))
        .send_json(&body)
        .map_err(|_| {
            "A confirmação do envio não chegou. Tente novamente para verificar antes de reenviar."
                .to_owned()
        })?;
    if !response.status().is_success() {
        return Err(format!(
            "O Teams não aceitou a mensagem (HTTP {}). O rascunho foi mantido.",
            response.status().as_u16()
        ));
    }
    let receipt: Value = response.body_mut().read_json().unwrap_or(Value::Null);
    let now = chrono::Local::now();
    Ok(GraphMessage {
        id: receipt["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("local:{client_id}")),
        client_message_id: Some(client_id.into()),
        timestamp: now.with_timezone(&chrono::Utc).to_rfc3339(),
        date: now.format("%d/%m/%Y").to_string(),
        author: "Você".into(),
        time: now.format("%H:%M").to_string(),
        body: caption.into(),
        is_mine: true,
        rich,
        images,
        ..Default::default()
    })
}

fn upload(base: &str, token: &str, chat_id: &str, picture: &mut StagedImage) -> Result<(), String> {
    let base = base.trim_end_matches('/');
    if picture.remote.is_none() {
        let permissions = json!({chat_id:["read"]});
        let body = json!({"type":"pish/image", "permissions":permissions, "sharingMode":"Inline", "filename":"Captura.png"});
        let mut response = agent()
            .post(format!("{base}/v1/objects/"))
            .header("Authorization", &format!("skype_token {token}"))
            .header("x-ms-client-version", "1415/1.0.0.2018090100")
            .header("x-ams-post-sharing-mode", "Inline")
            .send_json(&body)
            .map_err(|_| "Não foi possível criar o anexo no serviço de mídia.".to_owned())?;
        if !response.status().is_success() {
            return Err(format!(
                "O serviço de mídia recusou o anexo (HTTP {}).",
                response.status().as_u16()
            ));
        }
        let reply: Value = response
            .body_mut()
            .read_json()
            .map_err(|_| "O serviço de mídia não retornou o identificador da imagem.".to_owned())?;
        let id = reply["id"]
            .as_str()
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 200
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .ok_or_else(|| {
                "O identificador da imagem retornado pelo Teams é inválido.".to_owned()
            })?;
        picture.remote = Some(InlineImage {
            id: id.into(),
            url: format!("{base}/v1/objects/{id}/views/imgo"),
            width: picture.width,
            height: picture.height,
        });
    }
    if !picture.uploaded {
        let id = &picture.remote.as_ref().expect("media reserved above").id;
        let response = agent()
            .put(format!("{base}/v1/objects/{id}/content/imgpsh"))
            .header("Authorization", &format!("skype_token {token}"))
            .header("x-ms-client-version", "1415/1.0.0.2018090100")
            .header("Content-Type", "application/octet-stream")
            .send(picture.png.as_slice())
            .map_err(|_| {
                "O upload não terminou. A imagem foi mantida para tentar novamente.".to_owned()
            })?;
        if !response.status().is_success() {
            return Err(format!(
                "O upload da imagem falhou (HTTP {}).",
                response.status().as_u16()
            ));
        }
        picture.uploaded = true;
    }
    Ok(())
}

pub fn download(session: TeamsSession, url: &str) -> Result<LoadedImage, String> {
    let bytes = download_bytes(session, url)?;
    decoded_preview(&bytes).map(|(handle, width, height, estimated_bytes)| LoadedImage {
        handle,
        width,
        height,
        estimated_bytes,
    })
}

/// Chat HTML usually points at a small AMS view (`imgt1`, `imgt2`,
/// `imgpsh_mobile_save`). The original bytes are the `imgo` view of the
/// same object. Views that are already full stay untouched.
pub fn full_resolution_url(url: &str) -> String {
    if !valid_media_url(url) {
        return url.to_owned();
    }
    let Some(marker) = url.find("/views/") else {
        return url.to_owned();
    };
    let name_start = marker + "/views/".len();
    let tail = &url[name_start..];
    let name_len = tail.find(['/', '?', '#']).unwrap_or(tail.len());
    let name = &tail[..name_len];
    if name.is_empty()
        || ["imgo", "imgpsh_fullsize", "imgpsh_fullsize_anim"]
            .iter()
            .any(|view| view.eq_ignore_ascii_case(name))
    {
        return url.to_owned();
    }
    let mut full = String::with_capacity(url.len() - name.len() + 4);
    full.push_str(&url[..name_start]);
    full.push_str("imgo");
    full.push_str(&url[name_start + name_len..]);
    if valid_media_url(&full) {
        full
    } else {
        url.to_owned()
    }
}

fn preview_bitmap(image: image_codec::DynamicImage) -> image_codec::RgbaImage {
    if image.width() <= 1024 && image.height() <= 768 {
        return image.to_rgba8();
    }
    image
        .resize(1024, 768, image_codec::imageops::FilterType::Lanczos3)
        .to_rgba8()
}

fn view_span(url: &str) -> Option<(usize, usize)> {
    if !valid_media_url(url) {
        return None;
    }
    let marker = url.find("/views/")?;
    let name_start = marker + "/views/".len();
    let tail = &url[name_start..];
    let name_len = tail.find(['/', '?', '#']).unwrap_or(tail.len());
    if name_len == 0 {
        return None;
    }
    Some((name_start, name_start + name_len))
}

fn replace_view(url: &str, view: &str) -> Option<String> {
    let (start, end) = view_span(url)?;
    let mut full = String::with_capacity(url.len() - (end - start) + view.len());
    full.push_str(&url[..start]);
    full.push_str(view);
    full.push_str(&url[end..]);
    valid_media_url(&full).then_some(full)
}

/// Full AMS views of the same object, largest first. The published chat URL
/// is only the last resort: it is often `imgt1` or `imgpsh_mobile_save`.
fn original_candidate_urls(url: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut push = |candidate: String| {
        if valid_media_url(&candidate) && urls.iter().all(|have| have != &candidate) {
            urls.push(candidate);
        }
    };
    let animated = view_span(url).is_some_and(|(start, end)| {
        url[start..end].to_ascii_lowercase().ends_with("_anim")
    });
    if !animated && let Some(fullsize) = replace_view(url, "imgpsh_fullsize") {
        push(fullsize);
    }
    push(full_resolution_url(url));
    push(url.to_owned());
    urls
}

pub fn download_original(session: TeamsSession, url: &str) -> Result<OriginalImage, String> {
    let candidates = original_candidate_urls(url);
    let (full, fallback) = if candidates.len() > 1 {
        (
            candidates[..candidates.len() - 1].to_vec(),
            candidates.last().cloned(),
        )
    } else {
        (candidates, None)
    };
    match race_originals(session.clone(), &full) {
        Ok(image) => Ok(image),
        Err(error) => match fallback {
            Some(fallback) if !full.iter().any(|have| have == &fallback) => {
                download_bytes_with_timeout(session, &fallback, VIEWER_FETCH_TIMEOUT)
                    .and_then(decode_original)
            }
            _ => Err(error),
        },
    }
}

const VIEWER_FETCH_TIMEOUT: Duration = Duration::from_secs(12);

fn keep_larger(best: &mut Option<OriginalImage>, image: OriginalImage) -> bool {
    let pixels = u64::from(image.width) * u64::from(image.height);
    let replace = match best {
        None => true,
        Some(have) => pixels > u64::from(have.width) * u64::from(have.height),
    };
    if replace {
        *best = Some(image);
    }
    best.as_ref()
        .is_some_and(|image| image.width.max(image.height) >= 2048)
}

/// Full views start together. The first sharp decode wins, so a missing view
/// cannot add its own wait in front of the file the viewer will show.
fn race_originals(session: TeamsSession, urls: &[String]) -> Result<OriginalImage, String> {
    if urls.is_empty() {
        return Err("Não foi possível carregar a imagem.".into());
    }
    if urls.len() == 1 {
        return download_bytes_with_timeout(session, &urls[0], VIEWER_FETCH_TIMEOUT)
            .and_then(decode_original);
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut started = 0usize;
    for url in urls {
        let sender = sender.clone();
        let session = session.clone();
        let url = url.clone();
        if std::thread::Builder::new()
            .name("teams-image".into())
            .spawn(move || {
                let result = download_bytes_with_timeout(session, &url, VIEWER_FETCH_TIMEOUT)
                    .and_then(decode_original);
                let _ = sender.send(result);
            })
            .is_ok()
        {
            started += 1;
        }
    }
    drop(sender);
    if started == 0 {
        return download_bytes_with_timeout(session, &urls[0], VIEWER_FETCH_TIMEOUT)
            .and_then(decode_original);
    }
    let mut best: Option<OriginalImage> = None;
    let mut last_error = "Não foi possível carregar a imagem.".to_owned();
    let mut remaining = started;
    while remaining > 0 {
        let Ok(result) = receiver.recv_timeout(VIEWER_FETCH_TIMEOUT + Duration::from_secs(2))
        else {
            break;
        };
        remaining -= 1;
        match result {
            Ok(image) => {
                if keep_larger(&mut best, image) {
                    return best.ok_or(last_error);
                }
            }
            Err(error) => last_error = error,
        }
    }
    best.ok_or(last_error)
}

fn download_bytes(session: TeamsSession, url: &str) -> Result<Vec<u8>, String> {
    download_bytes_with_timeout(session, url, Duration::from_secs(60))
}

fn download_bytes_with_timeout(
    session: TeamsSession,
    url: &str,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    if !valid_media_url(url) {
        return Err("Endereço de imagem inesperado.".into());
    }
    let (_, _, token) = session.image_credentials()?;
    let mut response = agent()
        .get(url)
        .header("Authorization", &format!("skype_token {token}"))
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .map_err(|_| "Não foi possível carregar a imagem.".to_owned())?;
    if !response.status().is_success() {
        return Err("Imagem indisponível nesta conta.".into());
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Download interrompido.".to_owned())?;
    if bytes.len() > MAX_BYTES {
        return Err("A imagem ultrapassa o limite de 32 MB.".into());
    }
    Ok(bytes)
}

#[derive(Default)]
pub struct MediaCache {
    entries: HashMap<String, (Option<LoadedImage>, Instant, usize)>,
    running: std::collections::HashSet<String>,
    queue: std::collections::VecDeque<String>,
    cached_bytes: usize,
}
impl MediaCache {
    pub fn get(&self, url: &str) -> Option<&LoadedImage> {
        self.entries
            .get(url)
            .and_then(|(handle, _, _)| handle.as_ref())
    }

    pub fn estimated_bytes(&self) -> usize {
        self.cached_bytes
    }
    pub fn request(&mut self, url: &str) -> bool {
        if self.running.contains(url)
            || self
                .entries
                .get(url)
                .is_some_and(|(_, until, _)| *until > Instant::now())
        {
            return false;
        }
        if self.running.len() >= 4 {
            if self.queue.len() < 64 && !self.queue.iter().any(|pending| pending == url) {
                self.queue.push_back(url.into());
            }
            return false;
        }
        self.queue.retain(|pending| pending != url);
        self.running.insert(url.into());
        true
    }
    pub fn next(&mut self) -> Option<String> {
        self.queue.pop_front()
    }
    pub fn retry(&mut self, url: &str) {
        if self.get(url).is_none()
            && let Some((_, _, bytes)) = self.entries.remove(url)
        {
            self.cached_bytes = self.cached_bytes.saturating_sub(bytes);
        }
    }
    pub fn cancel_queued(&mut self) {
        self.queue.clear();
    }
    pub fn cancel_queued_url(&mut self, url: &str) {
        self.queue.retain(|pending| pending != url);
    }
    pub fn loading(&self, url: &str) -> bool {
        self.running.contains(url) || self.queue.iter().any(|pending| pending == url)
    }
    pub fn complete(&mut self, url: String, handle: Option<LoadedImage>) {
        self.running.remove(&url);
        if let Some((_, _, previous_bytes)) = self.entries.remove(&url) {
            self.cached_bytes = self.cached_bytes.saturating_sub(previous_bytes);
        }
        let handle = handle.filter(|image| image_bytes(image) <= MAX_CACHE_BYTES);
        let bytes = handle.as_ref().map_or(0, image_bytes);
        while self.entries.len() >= MAX_CACHE_ENTRIES
            || self.cached_bytes.saturating_add(bytes) > MAX_CACHE_BYTES
        {
            let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, until, _))| *until)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some((_, _, removed_bytes)) = self.entries.remove(&key) {
                self.cached_bytes = self.cached_bytes.saturating_sub(removed_bytes);
            }
        }
        let ttl = if handle.is_some() { 24 * 3600 } else { 120 };
        self.entries.insert(
            url,
            (handle, Instant::now() + Duration::from_secs(ttl), bytes),
        );
        self.cached_bytes = self.cached_bytes.saturating_add(bytes);
    }
}

fn image_bytes(image: &LoadedImage) -> usize {
    image.estimated_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FixtureClipboard {
        image: Option<StagedImage>,
        html: Option<String>,
        rtf: Option<Vec<u8>>,
        text: Option<String>,
    }

    impl ClipboardSource for FixtureClipboard {
        fn image(&mut self) -> Result<Option<StagedImage>, String> { Ok(self.image.take()) }
        fn html(&mut self) -> Option<String> { self.html.take() }
        fn rtf(&mut self) -> Option<Vec<u8>> { self.rtf.take() }
        fn text(&mut self) -> Option<String> { self.text.take() }
    }

    #[test]
    fn file_previews_only_promote_raster_files_on_the_existing_media_transport() {
        let mut file = ost::api::ChatFileRef {
            preview:Some(ost::api::ChatFilePreview {url:"https://api.asm.skype.com/v1/objects/0-test/views/imgo".into(),width:500,height:300}),
            attachment_id:Some("test".into()), name:"capture.png".into(),file_type:Some("png".into()),
            object_url:"https://example.sharepoint.com/capture.png".into(),share_url:None,sender:None,time:None,
        };
        assert!(file_preview(&file).is_some());
        file.file_type=Some("pdf".into()); assert!(file_preview(&file).is_none());
        file.file_type=Some("png".into());
        for url in ["https://example.sharepoint.com/capture.png", "http://api.asm.skype.com/v1/objects/0-test/views/imgo", "https://api.asm.skype.com.evil.test/v1/objects/0-test/views/imgo", "https://user:pass@api.asm.skype.com/v1/objects/0-test/views/imgo"] {
            file.preview.as_mut().unwrap().url=url.into(); assert!(file_preview(&file).is_none());
        }
    }

    #[test]
    fn clipboard_uses_only_offered_image_formats_and_prefers_native_png() {
        let mut offered = HashSet::from(["text/html".into(), "text/plain".into()]);
        assert_eq!(clipboard_image_mime(&offered), None);
        offered.insert("image/jpeg".into());
        assert_eq!(clipboard_image_mime(&offered), Some("image/jpeg"));
        offered.insert("image/png".into());
        assert_eq!(clipboard_image_mime(&offered), Some("image/png"));
        for mime in ["image/webp", "image/bmp", "image/x-bmp", "image/gif", "image/tiff", "image/x-tiff"] {
            assert_eq!(clipboard_image_mime(&HashSet::from([mime.to_owned()])), Some(mime));
        }
    }

    #[test]
    fn clipboard_image_reaches_staging_before_text_fallback_without_upload() {
        let raw = vec![240, 20, 80, 255, 20, 160, 40, 128];
        let image = stage_clipboard_pixels(arboard::ImageData {
            width: 2,
            height: 1,
            bytes: std::borrow::Cow::Owned(raw.clone()),
        }).unwrap();
        let mut source = FixtureClipboard {
            image: Some(image),
            html: Some("<p>alternate clipboard format</p>".into()),
            text: Some("alternate clipboard format".into()),
            ..Default::default()
        };

        let Paste::Image(image) = read_clipboard_from(&mut source).unwrap() else {
            panic!("clipboard image must not disappear as empty or text");
        };
        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(decoded_original_pixels(&image.png).unwrap().as_raw(), &raw);
        assert!(!image.uploaded);
        assert!(image.remote.is_none());
        assert!(source.html.is_some());
        assert!(source.text.is_some());
    }

    #[test]
    fn clipboard_reader_retains_html_and_plain_text_when_no_image_is_offered() {
        let mut rich = FixtureClipboard {
            html: Some("<strong>Revisão</strong>".into()),
            text: Some("Revisão".into()),
            ..Default::default()
        };
        assert!(matches!(read_clipboard_from(&mut rich).unwrap(), Paste::Html { html, plain }
            if html == "<strong>Revisão</strong>" && plain == "Revisão"));
        let mut plain = FixtureClipboard {
            text: Some("linha 1\nlinha 2".into()),
            ..Default::default()
        };
        assert!(matches!(read_clipboard_from(&mut plain).unwrap(), Paste::Text(text)
            if text == "linha 1\nlinha 2"));
    }

    #[test]
    fn clipboard_raw_pixels_reject_truncation_and_oversized_dimensions() {
        for (width, height, bytes) in [(2, 2, vec![0; 4]), (usize::MAX, 1, Vec::new())] {
            assert!(stage_clipboard_pixels(arboard::ImageData {
                width,
                height,
                bytes: std::borrow::Cow::Owned(bytes),
            }).is_err());
        }
    }

    #[test]
    fn attachment_picker_routes_raster_extensions_to_image_pipeline() {
        assert!(is_raster_attachment(std::path::Path::new("print.PNG")));
        assert!(is_raster_attachment(std::path::Path::new("photo.tiff")));
        assert!(!is_raster_attachment(std::path::Path::new("report.pdf")));
        assert!(!is_raster_attachment(std::path::Path::new("notes.docx")));
    }

    fn cache_image(estimated_bytes: usize) -> LoadedImage {
        LoadedImage {
            handle: Handle::from_rgba(1, 1, vec![0; 4]),
            width: 1,
            height: 1,
            estimated_bytes,
        }
    }

    #[test]
    fn media_cache_evicts_to_stay_within_its_decoded_rgba_budget() {
        let mut cache = MediaCache::default();
        let preview_bytes = 20 * 1024 * 1024;
        cache.complete("first".into(), Some(cache_image(preview_bytes)));
        assert_eq!(cache.estimated_bytes(), preview_bytes);

        cache.complete("second".into(), Some(cache_image(preview_bytes)));
        assert!(cache.get("first").is_none());
        assert!(cache.get("second").is_some());
        assert_eq!(cache.estimated_bytes(), preview_bytes);
        assert!(cache.estimated_bytes() <= MAX_CACHE_BYTES);
    }

    #[test]
    fn single_image_larger_than_cache_budget_is_not_retained() {
        let mut cache = MediaCache::default();
        cache.complete(
            "oversized".into(),
            Some(cache_image(MAX_CACHE_BYTES + 1)),
        );

        assert!(cache.get("oversized").is_none());
        assert_eq!(cache.estimated_bytes(), 0);
    }

    #[test]
    fn original_image_keeps_the_downloaded_bytes_and_full_dimensions() {
        let staged = demo_image();
        let expected = staged.png.as_ref().clone();
        let original = decode_original(expected.clone()).unwrap();
        assert_eq!((original.width, original.height), (1280, 720));
        assert_eq!(original.bytes.as_slice(), expected.as_slice());
    }

    #[test]
    fn file_import_converts_jpeg_to_the_existing_png_staging_contract() {
        let jpeg_path = std::env::temp_dir().join(format!(
            "teams-linux-image-import-{}.jpg",
            rand::random::<u64>()
        ));
        let jpeg = image_codec::DynamicImage::ImageRgb8(image_codec::RgbImage::from_pixel(
            3,
            2,
            image_codec::Rgb([90, 120, 150]),
        ));
        let mut bytes = Cursor::new(Vec::new());
        jpeg.write_to(&mut bytes, image_codec::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&jpeg_path, bytes.into_inner()).unwrap();

        let staged = stage_image_file(&jpeg_path).unwrap();
        std::fs::remove_file(jpeg_path).unwrap();

        assert!(staged.png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!((staged.width, staged.height), (3, 2));
        assert!(!staged.uploaded);
        assert!(staged.remote.is_none());
    }

    #[test]
    fn file_import_accepts_the_raster_formats_offered_by_the_picker() {
        let image = image_codec::DynamicImage::ImageRgb8(image_codec::RgbImage::from_pixel(
            3,
            2,
            image_codec::Rgb([90, 120, 150]),
        ));
        for (format, extension) in [
            (image_codec::ImageFormat::Png, "png"),
            (image_codec::ImageFormat::Jpeg, "jpg"),
            (image_codec::ImageFormat::WebP, "webp"),
            (image_codec::ImageFormat::Bmp, "bmp"),
            (image_codec::ImageFormat::Gif, "gif"),
            (image_codec::ImageFormat::Tiff, "tiff"),
        ] {
            let path = std::env::temp_dir().join(format!(
                "teams-linux-image-import-{}.{}",
                rand::random::<u64>(),
                extension
            ));
            let mut bytes = Cursor::new(Vec::new());
            image.clone().write_to(&mut bytes, format).unwrap();
            std::fs::write(&path, bytes.into_inner()).unwrap();

            let result = stage_image_file(&path);
            std::fs::remove_file(path).unwrap();
            let staged = result.unwrap();

            assert!(staged.png.starts_with(b"\x89PNG\r\n\x1a\n"));
            assert_eq!((staged.width, staged.height), (3, 2));
            assert!(!staged.uploaded);
            assert!(staged.remote.is_none());
        }
    }

    #[test]
    fn image_bytes_use_the_existing_png_staging_contract() {
        let jpeg = image_codec::DynamicImage::ImageRgb8(image_codec::RgbImage::from_pixel(
            3,
            2,
            image_codec::Rgb([20, 80, 140]),
        ));
        let mut bytes = Cursor::new(Vec::new());
        jpeg.write_to(&mut bytes, image_codec::ImageFormat::Jpeg)
            .unwrap();

        let staged = stage_image_bytes(bytes.into_inner()).unwrap();

        assert!(staged.png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!((staged.width, staged.height), (3, 2));
        assert!(!staged.uploaded);
        assert!(staged.remote.is_none());
    }

    #[test]
    fn file_import_rejects_non_image_data_without_creating_a_staged_attachment() {
        let path = std::env::temp_dir().join(format!(
            "teams-linux-image-import-{}.bin",
            rand::random::<u64>()
        ));
        std::fs::write(&path, b"not an image").unwrap();
        let result = stage_image_file(&path);
        std::fs::remove_file(path).unwrap();
        assert!(matches!(
            result,
            Err(error) if error == "Anexe uma imagem PNG, JPEG, WebP, BMP, GIF ou TIFF."
        ));
    }

    #[test]
    fn clipboard_html_extracts_cf_html_fragment_by_utf8_byte_offsets() {
        let html = "<b>Árvore</b>";
        let body = format!(
            "<html><body><!--StartFragment-->{html}<!--EndFragment--></body></html>"
        );
        let header_len = "Version:1.0\r\nStartHTML:00000000\r\nEndHTML:00000000\r\nStartFragment:00000000\r\nEndFragment:00000000\r\n".len();
        let start_fragment = header_len + "<html><body><!--StartFragment-->".len();
        let end_fragment = start_fragment + html.len();
        let envelope = format!(
            "Version:1.0\r\nStartHTML:{header_len:08}\r\nEndHTML:{end_html:08}\r\nStartFragment:{start_fragment:08}\r\nEndFragment:{end_fragment:08}\r\n{body}",
            end_html = header_len + body.len(),
        );
        assert_eq!(normalize_clipboard_html(envelope), Some(html.into()));
    }

    #[test]
    fn clipboard_html_rejects_oversized_or_malformed_cf_html() {
        assert!(normalize_clipboard_html("x".repeat(MAX_CLIPBOARD_BYTES + 1)).is_none());
        assert!(normalize_clipboard_html(
            "Version:1.0\r\nStartFragment:99999999\r\nEndFragment:99999999\r\n<b>x</b>".into()
        )
        .is_none());
        assert_eq!(normalize_clipboard_html(" <b>safe</b> ".into()), Some(" <b>safe</b> ".into()));
    }

    #[test]
    fn clipboard_rtf_converts_inline_styles_and_paragraph_breaks_for_the_composer() {
        let html = clipboard_rtf_to_html(
            br"{\rtf1\ansi Texto \b forte\b0, \i leve\i0\par outra linha}",
        )
        .unwrap();
        let paste = crate::message_body::Document::from_clipboard_html(&html).unwrap();

        assert_eq!(paste.text, "Texto forte, leve\noutra linha");
        assert!(paste.formatting.has_mark(6, 11, crate::message_body::InlineMark::Bold));
        assert!(paste.formatting.has_mark(13, 17, crate::message_body::InlineMark::Italic));
    }

    #[test]
    fn clipboard_rtf_restores_group_styles_and_skips_metadata_destinations() {
        let html = clipboard_rtf_to_html(
            br"{\rtf1\ansi{\fonttbl{\f0 Arial;}}\b antes {\i junto} depois\b0 {\*\unknown segredo}}",
        )
        .unwrap();
        let paste = crate::message_body::Document::from_clipboard_html(&html).unwrap();

        assert_eq!(paste.text, "antes junto depois");
        assert!(paste.formatting.has_mark(0, 5, crate::message_body::InlineMark::Bold));
        assert!(paste.formatting.has_mark(6, 11, crate::message_body::InlineMark::Bold));
        assert!(paste.formatting.has_mark(6, 11, crate::message_body::InlineMark::Italic));
        assert!(paste.formatting.has_mark(12, 18, crate::message_body::InlineMark::Bold));
        assert!(!paste.text.contains("Arial"));
        assert!(!paste.text.contains("segredo"));
    }

    #[test]
    fn clipboard_rtf_decodes_ansi_escapes_unicode_and_escaped_braces() {
        let html = clipboard_rtf_to_html(
            br"{\rtf1\ansi caf\'e9 \{chaves\} \u233? \u-10179?\u-8704?}",
        )
        .unwrap();
        let paste = crate::message_body::Document::from_clipboard_html(&html).unwrap();

        assert_eq!(paste.text, "café {chaves} é 😀");
    }

    #[test]
    fn clipboard_rtf_rejects_malformed_unsupported_and_oversized_input() {
        assert!(clipboard_rtf_to_html(b"plain text").is_none());
        assert!(clipboard_rtf_to_html(br"{\rtf1\ansicpg932 text}").is_none());
        assert!(clipboard_rtf_to_html(&vec![b'x'; MAX_CLIPBOARD_BYTES + 1]).is_none());
        assert!(clipboard_rtf_to_html(br"{\rtf1 unclosed").is_none());
    }

    #[test]
    fn clipboard_rtf_escapes_markup_and_skips_binary_payload_braces() {
        let html = clipboard_rtf_to_html(br"{\rtf1 before\bin4 }abc <script>& after}").unwrap();

        assert_eq!(html, "before &lt;script&gt;&amp; after");
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn original_save_preserves_bytes_at_the_chosen_path() {
        let path = std::env::temp_dir().join(format!(
            "teams-linux-image-{}-{}.png",
            std::process::id(),
            rand::random::<u64>()
        ));
        let bytes = b"synthetic image bytes";
        write_original_to_path(&path, bytes).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn original_file_extension_follows_the_encoded_format() {
        assert_eq!(original_extension(b"\x89PNG\r\n\x1a\nrest"), "png");
        assert_eq!(original_extension(&[0xff, 0xd8, 0xff, 0]), "jpg");
    }

    #[test]
    fn original_save_keeps_the_downloaded_format_extension() {
        assert_eq!(
            with_original_extension("capture.txt".into(), "png"),
            std::path::PathBuf::from("capture.png")
        );
        assert_eq!(
            with_original_extension("capture.JPEG".into(), "jpg"),
            std::path::PathBuf::from("capture.JPEG")
        );
    }

    #[test]
    fn media_cache_bounds_parallel_downloads_and_drops_old_viewport_queue() {
        let mut cache = MediaCache::default();
        let urls = (0..6)
            .map(|index| format!("https://api.asm.skype.com/v1/objects/0-{index}/views/imgo"))
            .collect::<Vec<_>>();
        for url in &urls[..4] {
            assert!(cache.request(url));
        }
        assert!(!cache.request(&urls[4]));
        assert!(!cache.request(&urls[4]));
        assert!(!cache.request(&urls[5]));
        assert!(cache.loading(&urls[4]));
        assert_eq!(cache.running.len(), 4);
        assert_eq!(cache.queue.len(), 2);

        cache.cancel_queued_url(&urls[4]);
        assert!(!cache.loading(&urls[4]));
        assert!(cache.loading(&urls[5]));
        cache.cancel_queued();
        assert!(!cache.loading(&urls[5]));
        cache.complete(urls[0].clone(), None);
        assert!(cache.request(&urls[4]));
        assert_eq!(cache.running.len(), 4);
    }

    #[test]
    fn viewer_asks_for_the_original_ams_view() {
        assert_eq!(
            full_resolution_url("https://us-api.asm.skype.com/v1/objects/0-abc/views/imgt1"),
            "https://us-api.asm.skype.com/v1/objects/0-abc/views/imgo"
        );
        assert_eq!(
            full_resolution_url(
                "https://us-api.asm.skype.com/v1/objects/0-abc/views/imgt1?x=1"
            ),
            "https://us-api.asm.skype.com/v1/objects/0-abc/views/imgo?x=1"
        );
        assert_eq!(
            full_resolution_url(
                "https://us-api.asm.skype.com/v1/objects/0-abc/views/imgpsh_mobile_save_anim"
            ),
            "https://us-api.asm.skype.com/v1/objects/0-abc/views/imgo"
        );
        assert_eq!(
            full_resolution_url("https://api.asm.skype.com/v1/objects/0-abc/views/imgo"),
            "https://api.asm.skype.com/v1/objects/0-abc/views/imgo"
        );
        assert_eq!(
            full_resolution_url(
                "https://euno-prod.asyncgw.teams.microsoft.com/v1/objects/0-abc/views/imgpsh_fullsize"
            ),
            "https://euno-prod.asyncgw.teams.microsoft.com/v1/objects/0-abc/views/imgpsh_fullsize"
        );
        assert_eq!(
            full_resolution_url(
                "https://euno-prod.asyncgw.teams.microsoft.com/v1/objects/0-abc/views/imgpsh_fullsize_anim"
            ),
            "https://euno-prod.asyncgw.teams.microsoft.com/v1/objects/0-abc/views/imgpsh_fullsize_anim"
        );
        let untouched = "https://example.com/v1/objects/0/views/imgt1";
        assert_eq!(full_resolution_url(untouched), untouched);
        assert_eq!(
            original_candidate_urls(
                "https://us-api.asm.skype.com/v1/objects/0-abc/views/imgt1?x=1"
            ),
            vec![
                "https://us-api.asm.skype.com/v1/objects/0-abc/views/imgpsh_fullsize?x=1"
                    .to_owned(),
                "https://us-api.asm.skype.com/v1/objects/0-abc/views/imgo?x=1".to_owned(),
                "https://us-api.asm.skype.com/v1/objects/0-abc/views/imgt1?x=1".to_owned(),
            ]
        );
        assert_eq!(
            original_candidate_urls(
                "https://us-api.asm.skype.com/v1/objects/0-abc/views/imgpsh_fullsize"
            )
            .len(),
            1
        );
        assert_eq!(
            original_candidate_urls(
                "https://euno-prod.asyncgw.teams.microsoft.com/v1/objects/0-abc/views/imgpsh_fullsize_anim"
            ),
            vec![
                "https://euno-prod.asyncgw.teams.microsoft.com/v1/objects/0-abc/views/imgpsh_fullsize_anim"
                    .to_owned(),
            ]
        );
        let mut best = None;
        assert!(!keep_larger(
            &mut best,
            OriginalImage {
                handle: iced::widget::image::Handle::from_rgba(2, 2, vec![0; 16]),
                width: 2,
                height: 2,
                bytes: Arc::new(Vec::new()),
            }
        ));
        assert!(keep_larger(
            &mut best,
            OriginalImage {
                handle: iced::widget::image::Handle::from_rgba(2048, 1, vec![0; 2048 * 4]),
                width: 2048,
                height: 1,
                bytes: Arc::new(Vec::new()),
            }
        ));
        assert_eq!(best.unwrap().width, 2048);
    }

    #[test]
    fn chat_preview_keeps_small_pixels_and_fits_wide_ones() {
        let small = preview_bitmap(image_codec::DynamicImage::ImageRgb8(
            image_codec::RgbImage::from_pixel(4, 2, image_codec::Rgb([10, 20, 30])),
        ));
        assert_eq!((small.width(), small.height()), (4, 2));
        let wide = preview_bitmap(image_codec::DynamicImage::ImageRgb8(
            image_codec::RgbImage::from_pixel(2000, 40, image_codec::Rgb([10, 20, 30])),
        ));
        assert!(wide.width() <= 1024 && wide.height() <= 768);
        assert!(wide.width() > 512);
    }

    #[test]
    fn inline_dimensions_scale_proportionally_without_upscaling() {
        let wide = InlineImage {
            id: "0-wide".into(),
            url: "https://us-api.asm.skype.com/v1/objects/0-wide/views/imgo".into(),
            width: 1920,
            height: 1080,
        };
        assert_eq!(display_size(&wide, 0, 0, 480.0, 480.0), (480.0, 270.0));

        let tall = InlineImage {
            width: 0,
            height: 0,
            ..wide.clone()
        };
        assert_eq!(display_size(&tall, 320, 640, 480.0, 480.0), (240.0, 480.0));
        assert_eq!(display_size(&tall, 320, 640, 1280.0, 960.0), (320.0, 640.0));
        assert_eq!(display_size(&tall, 0, 0, 480.0, 480.0), (320.0, 240.0));
        // Decoded dimensions are authoritative when HTML preview dimensions disagree.
        assert_eq!(display_size(&wide, 600, 1200, 480.0, 480.0), (240.0, 480.0));
        assert_eq!(fit_size(800.0, 600.0, 2400.0, 1400.0, false), (800.0, 600.0));
        assert_eq!(fit_size(200.0, 100.0, 1000.0, 600.0, true), (1000.0, 500.0));
        assert_eq!(fit_size(100.0, 200.0, 1000.0, 600.0, true), (300.0, 600.0));
        let (width, height) = fit_size(1.0, 10000.0, 720.0, 640.0, false);
        assert!((width / height - 0.0001).abs() < 0.000001);
    }

    #[test]
    fn inline_html_dimensions_are_bounded_before_layout() {
        let parsed = parse_inline_images(
            "<img src='https://api.asm.skype.com/v1/objects/0-a/views/imgo' itemtype='http://schema.skype.com/AMSImage' width='50000' height='70000' />\
             <img src='https://api.asm.skype.com/v1/objects/0-b/views/imgo' itemtype='http://schema.skype.com/AMSImage' width='9000' height='8000' />",
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!((parsed[0].width, parsed[0].height), (0, 0));
        assert_eq!((parsed[1].width, parsed[1].height), (0, 0));
    }

    #[test]
    fn original_wire_contract_keeps_permissions_references_and_safe_html() {
        let image = InlineImage {
            id: "0-test".into(),
            url: "https://us-api.asm.skype.com/v1/objects/0-test/views/imgo".into(),
            width: 1280,
            height: 720,
        };
        let body = message_body("caption <&>", &[image.clone()], "1234");
        assert_eq!(body["amsreferences"], json!(["0-test"]));
        assert_eq!(body["messagetype"], "RichText/Html");
        assert!(
            body["content"]
                .as_str()
                .unwrap()
                .contains("caption &lt;&amp;&gt;")
        );
        assert_eq!(
            parse_inline_images(body["content"].as_str().unwrap()),
            vec![image]
        );
        assert!(!valid_media_url(
            "https://api.asm.skype.com.evil.test/v1/objects/x/views/imgo"
        ));
        assert!(!valid_media_url(
            "https://user@api.asm.skype.com/v1/objects/x/views/imgo"
        ));
    }

    #[test]
    fn multiple_image_publication_preserves_order_in_references_html_and_local_echo() {
        let images = [
            InlineImage {
                id: "0-first".into(),
                url: "https://us-api.asm.skype.com/v1/objects/0-first/views/imgo".into(),
                width: 640,
                height: 480,
            },
            InlineImage {
                id: "0-second".into(),
                url: "https://us-api.asm.skype.com/v1/objects/0-second/views/imgo".into(),
                width: 1280,
                height: 720,
            },
        ];
        let payload = message_body("legenda", &images, "image-batch");
        assert_eq!(payload["amsreferences"], json!(["0-first", "0-second"]));
        let html = payload["content"].as_str().unwrap();
        assert!(html.find("0-first").unwrap() < html.find("0-second").unwrap());
        assert_eq!(parse_inline_images(html), images);

        let document = crate::message_body::Document::from_html(html).unwrap();
        let rendered_order = document
            .blocks
            .iter()
            .filter_map(|block| block.media.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(rendered_order, images.iter().collect::<Vec<_>>());
    }

    #[test]
    fn two_image_upload_fixture_keeps_object_and_message_order() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let root = format!("http://{}", server.server_addr());
        let worker = std::thread::spawn(move || {
            for id in ["0-first", "0-second"] {
                let mut request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .expect("ordered image upload");
                assert_eq!(request.method(), &tiny_http::Method::Put);
                assert_eq!(request.url(), format!("/v1/objects/{id}/content/imgpsh"));
                assert!(request.headers().iter().any(|header| {
                    header.field.equiv("Authorization")
                        && header.value.as_str() == "skype_token fixture-token"
                }));
                let mut bytes = Vec::new();
                request.as_reader().read_to_end(&mut bytes).unwrap();
                assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
                request.respond(tiny_http::Response::empty(201)).unwrap();
            }
            let mut request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .expect("ordered image publication");
            assert_eq!(request.method(), &tiny_http::Method::Post);
            let mut bytes = Vec::new();
            request.as_reader().read_to_end(&mut bytes).unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["amsreferences"], json!(["0-first", "0-second"]));
            let content = body["content"].as_str().unwrap();
            assert!(content.find("0-first").unwrap() < content.find("0-second").unwrap());
            request
                .respond(
                    tiny_http::Response::from_string("{\"id\":\"published-two-images\"}")
                        .with_status_code(201),
                )
                .unwrap();
        });

        let mut pictures = vec![demo_image(), demo_image()];
        for (picture, id) in pictures.iter_mut().zip(["0-first", "0-second"]) {
            picture.remote = Some(InlineImage {
                id: id.into(),
                url: format!("https://api.asm.skype.com/v1/objects/{id}/views/imgo"),
                width: 1280,
                height: 720,
            });
        }
        for picture in &mut pictures {
            upload(&root, "fixture-token", "19:fixture", picture).unwrap();
        }
        let images = pictures
            .iter()
            .filter_map(|picture| picture.remote.clone())
            .collect();
        let message = publish(
            &root,
            "fixture-token",
            "19:fixture",
            "legenda",
            images,
            "image-batch",
            "Fixture",
        )
        .unwrap();
        assert_eq!(message.id, "published-two-images");
        assert_eq!(message.images[0].id, "0-first");
        assert_eq!(message.images[1].id, "0-second");
        let rendered_ids = message
            .rich
            .as_ref()
            .unwrap()
            .blocks
            .iter()
            .filter_map(|block| block.media.as_ref().map(|media| media.id.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(rendered_ids, vec!["0-first", "0-second"]);
        worker.join().unwrap();
    }

    #[test]
    fn lost_publication_receipt_matches_the_stable_client_id_before_retrying() {
        let expected = GraphMessage {
            id: "service-message-id".into(),
            client_message_id: Some("stable-image-client-id".into()),
            ..Default::default()
        };
        let found = find_prior_publication(
            vec![
                GraphMessage {
                    id: "other".into(),
                    client_message_id: Some("other-client".into()),
                    ..Default::default()
                },
                expected.clone(),
            ],
            "stable-image-client-id",
        );
        assert!(found.is_some_and(|message| message.id == expected.id));
        assert!(find_prior_publication(Vec::new(), "stable-image-client-id").is_none());
    }

    #[test]
    fn failed_upload_reuses_reserved_object_and_preserves_original_png() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let root = format!("http://{}", server.server_addr());
        let mut picture = demo_image();
        let original = picture.png.clone();
        let expected = original.clone();
        let worker = std::thread::spawn(move || {
            for index in 0..4 {
                let mut request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .expect("image upload request");
                assert!(request.headers().iter().any(|header| {
                    if index == 3 {
                        header.field.equiv("Authentication")
                            && header.value.as_str() == "skypetoken=image-fixture"
                    } else {
                        header.field.equiv("Authorization")
                            && header.value.as_str() == "skype_token image-fixture"
                    }
                }));
                let mut body = Vec::new();
                request.as_reader().read_to_end(&mut body).unwrap();
                if index == 0 {
                    assert_eq!(request.url(), "/v1/objects/");
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(body["type"], "pish/image");
                    assert_eq!(body["permissions"], json!({"19:fixture":["read"]}));
                    assert_eq!(body["sharingMode"], "Inline");
                    request
                        .respond(
                            tiny_http::Response::from_string("{\"id\":\"0-ams-test\"}")
                                .with_status_code(201),
                        )
                        .unwrap();
                } else if index == 3 {
                    assert_eq!(request.method(), &tiny_http::Method::Post);
                    assert_eq!(
                        request.url(),
                        "/v1/users/ME/conversations/19%3Afixture/messages"
                    );
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(body["clientmessageid"], "image-client-1");
                    assert_eq!(body["amsreferences"], json!(["0-ams-test"]));
                    assert!(
                        body["content"]
                            .as_str()
                            .unwrap()
                            .contains("http://schema.skype.com/AMSImage")
                    );
                    assert!(body["content"].as_str().unwrap().contains("width=\"1280\""));
                    request
                        .respond(
                            tiny_http::Response::from_string("{\"id\":\"987\"}")
                                .with_status_code(201),
                        )
                        .unwrap();
                } else {
                    assert_eq!(request.method(), &tiny_http::Method::Put);
                    assert_eq!(request.url(), "/v1/objects/0-ams-test/content/imgpsh");
                    assert_eq!(body.as_slice(), expected.as_slice());
                    request
                        .respond(tiny_http::Response::empty(if index == 1 {
                            503
                        } else {
                            201
                        }))
                        .unwrap();
                }
            }
        });
        assert!(upload(&root, "image-fixture", "19:fixture", &mut picture).is_err());
        assert_eq!(picture.remote.as_ref().unwrap().id, "0-ams-test");
        assert!(!picture.uploaded);
        upload(&root, "image-fixture", "19:fixture", &mut picture).unwrap();
        assert!(picture.uploaded);
        let message = publish(
            &root,
            "image-fixture",
            "19:fixture",
            "caption",
            vec![picture.remote.clone().unwrap()],
            "image-client-1",
            "Fixture",
        )
        .unwrap();
        assert_eq!(message.id, "987");
        assert_eq!(message.images.len(), 1);
        worker.join().unwrap();
        // Already uploaded: a retry performs no metadata POST or image PUT.
        upload(&root, "image-fixture", "19:fixture", &mut picture).unwrap();
        assert_eq!(picture.png.as_slice(), original.as_slice());
        assert_eq!((picture.width, picture.height), (1280, 720));
    }
}
