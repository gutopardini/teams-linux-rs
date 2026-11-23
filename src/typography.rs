use crate::preferences::FontFamily;
use iced::{Font, font};
use std::sync::OnceLock;

static ACTIVE_FONT: OnceLock<FontFamily> = OnceLock::new();

pub fn initialize(family: FontFamily) {
    let _ = ACTIVE_FONT.set(family);
}
pub fn active() -> FontFamily {
    ACTIVE_FONT.get().copied().unwrap_or_default()
}
pub fn for_family(family: FontFamily) -> Font {
    match family {
        FontFamily::Selawik => REGULAR,
        FontFamily::Inter => Font {
            family: font::Family::Name("Inter"),
            ..Font::DEFAULT
        },
    }
}
pub fn regular() -> Font {
    for_family(active())
}
pub fn semibold() -> Font {
    Font { weight: font::Weight::Semibold, ..regular() }
}

pub const REGULAR: Font = Font {
    family: font::Family::Name("Selawik"),
    ..Font::DEFAULT
};
pub const REGULAR_BYTES: &[u8] = include_bytes!("../assets/fonts/Selawik-Regular.ttf");
pub const INTER_REGULAR_BYTES: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
pub const INTER_SEMIBOLD_BYTES: &[u8] = include_bytes!("../assets/fonts/Inter-SemiBold.ttf");
pub const MEDIUM_BYTES: &[u8] = include_bytes!("../assets/fonts/Inter-Medium.ttf");
pub const SEMIBOLD_BYTES: &[u8] = include_bytes!("../assets/fonts/Selawik-Semibold.ttf");
pub const ITALIC_BYTES: &[u8] = include_bytes!("../assets/fonts/Inter-Italic.ttf");
pub const SEMIBOLD_ITALIC_BYTES: &[u8] = include_bytes!("../assets/fonts/Inter-SemiBoldItalic.ttf");

pub const BOLD_BYTES: &[u8] = include_bytes!("../assets/fonts/Selawik-Bold.ttf");
// Selawik has no italic release. Keep real italic faces instead of silently
// dropping formatting when the requested style has no Selawik face.
pub const ITALIC_FALLBACK: Font = Font {
    family: font::Family::Name("Inter"),
    ..REGULAR
};

pub const EMOJI: Font = Font {
    family: font::Family::Name("Teams Linux Emoji"),
    ..Font::DEFAULT
};
pub const EMOJI_BYTES: &[u8] = include_bytes!("../assets/fonts/TeamsLinuxEmoji.ttf");

/// Font boundaries follow whole graphemes, keeping flags and ZWJ sequences intact.
pub fn emoji_segments(value: &str) -> impl Iterator<Item = (&str, bool)> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut parts = value.grapheme_indices(true).peekable();
    std::iter::from_fn(move || {
        let (start, first) = parts.next()?;
        let emoji = is_color_emoji(first);
        while parts.peek().is_some_and(|(_, part)| is_color_emoji(part) == emoji) {
            parts.next();
        }
        let end = parts.peek().map_or(value.len(), |(offset, _)| *offset);
        Some((&value[start..end], emoji))
    })
}

pub fn is_color_emoji(grapheme: &str) -> bool {
    !grapheme.contains('\u{fe0e}') && grapheme.chars().any(|c| {
        matches!(c as u32, 0x1f000..=0x1faff | 0x2600..=0x27bf | 0xfe0f | 0x20e3)
    })
}

pub fn contains_color_emoji(value: &str) -> bool {
    emoji_segments(value).any(|(_, emoji)| emoji)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_font_segments_preserve_sequences_and_text() {
        let source = "Alerta 🚨✅: família 👨‍👩‍👧‍👦, 🇧🇷, 1️⃣ e texto.";
        let segments: Vec<_> = emoji_segments(source).collect();
        assert_eq!(segments.iter().map(|(value, _)| *value).collect::<String>(), source);
        for emoji in ["🚨✅", "👨‍👩‍👧‍👦", "🇧🇷", "1️⃣"] {
            assert!(segments.contains(&(emoji, true)));
        }
        assert_eq!(emoji_segments("ABC 123").collect::<Vec<_>>(), [("ABC 123", false)]);
        assert_eq!(emoji_segments("❤\u{fe0e}").collect::<Vec<_>>(), [("❤\u{fe0e}", false)]);
    }
}
