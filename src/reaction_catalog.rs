//! Public Teams emoji metadata, pinned at build time. No account or CDN access
//! occurs while browsing the picker. Keys are protocol identities, not Unicode.
use crate::*;
use serde::Deserialize;
use std::{collections::HashMap, sync::OnceLock};

#[derive(Deserialize)]
pub struct Entry {
    pub id: String,
    label: String,
    pub unicode: String,
    keywords: String,
    pub categories: Vec<usize>,
    pub diverse: bool,
    #[serde(default)]
    search: String,
    #[serde(skip)]
    english_label: String,
}
impl Entry {
    pub fn label(&self) -> &str {
        match crate::i18n::language() {
            crate::i18n::Language::English => &self.english_label,
            crate::i18n::Language::PortugueseBrazil => &self.label,
        }
    }
}
struct Catalog {
    entries: Vec<Entry>,
    keys: HashMap<String, usize>,
    unicode: HashMap<String, usize>,
}
fn catalog() -> &'static Catalog {
    static DATA: OnceLock<Catalog> = OnceLock::new();
    DATA.get_or_init(|| {
        let mut entries: Vec<Entry> = serde_json::from_str(include_str!("../assets/reactions/catalog.json")).expect("bundled reaction catalog");
        let mut english: HashMap<String, [String; 2]> =
            serde_json::from_str(include_str!("../locales/reactions-en.json")).expect("English reaction catalog");
        let mut keys = HashMap::new();
        let mut unicode = HashMap::new();
        for (index, entry) in entries.iter_mut().enumerate() {
            let [label, keywords] = english.remove(&entry.id).expect("English reaction metadata");
            entry.search = normalize(&format!("{} {} {} {label} {keywords}", entry.label, entry.keywords, entry.unicode));
            entry.english_label = label;
            assert!(keys.insert(service_key(&entry.id).to_owned(), index).is_none(), "duplicate reaction key");
            if !entry.unicode.is_empty() {
                unicode.entry(entry.unicode.clone()).or_insert(index);
            }
        }
        Catalog { entries, keys, unicode }
    })
}
pub fn for_unicode(value: &str) -> Option<&'static Entry> {
    let data = catalog();
    data.unicode.get(value).map(|index| &data.entries[*index])
}

pub fn matches_query(entry: &Entry, normalized_query: &str) -> bool {
    normalized_query.split_whitespace().all(|word| entry.search.contains(word))
}
// Original module 77708 explicitly translates these three catalog IDs.
pub fn service_key(id: &str) -> &str {
    match id { "like" => "likeWithFaceMSER", "yes" => "like", "star" => "starMSER", _ => id }
}
fn normalize(value: &str) -> String {
    value.to_lowercase().chars().map(|c| match c {
        'á' | 'à' | 'ã' | 'â' => 'a', 'é' | 'ê' => 'e', 'í' => 'i',
        'ó' | 'ô' | 'õ' => 'o', 'ú' => 'u', 'ç' => 'c', _ => c,
    }).collect()
}
pub fn resolve_key(key: &str) -> Option<(&'static Entry, usize)> {
    let data = catalog();
    if let Some(index) = data.keys.get(key) { return Some((&data.entries[*index], 0)); }
    let (base, suffix) = key.rsplit_once("-tone")?;
    let tone: usize = suffix.parse().ok()?;
    if !(1..=5).contains(&tone) || suffix.len() != 1 { return None; }
    let entry = &data.entries[*data.keys.get(base)?];
    entry.diverse.then_some((entry, tone))
}
pub fn token(entry: &Entry, tone: usize) -> String {
    let key = service_key(&entry.id);
    if tone > 0 && entry.diverse { return format!("{key}-tone{tone}"); }
    crate::reactions::SUPPORTED.iter().find(|(_, known)| *known == key)
        .map(|(emoji, _)| (*emoji).to_owned()).unwrap_or_else(|| key.to_owned())
}
pub fn tone_index(tone: preferences::SkinTone) -> usize {
    use preferences::SkinTone::*;
    match tone { Default => 0, Light => 1, MediumLight => 2, Medium => 3, MediumDark => 4, Dark => 5 }
}
pub fn visible(category: emoji::Category, query: &str, recent: &[String]) -> Vec<&'static Entry> {
    let data = catalog();
    let search = normalize(query);
    if !search.trim().is_empty() {
        return data.entries.iter().filter(|entry| matches_query(entry, &search)).collect();
    }
    if category == emoji::Category::Recent {
        return recent.iter().filter_map(|token| crate::reactions::key(token).and_then(resolve_key).map(|(entry,_)| entry)).collect();
    }
    let index = emoji::Category::ALL.iter().position(|item| *item == category).unwrap();
    data.entries.iter().filter(|entry| entry.categories.contains(&index)).collect()
}
pub fn remember(recent: &mut Vec<String>, token: &str) {
    if crate::reactions::key(token).is_none() { return; }
    recent.retain(|old| old != token && crate::reactions::key(old).is_some());
    recent.insert(0, token.into());
    recent.truncate(36);
}
pub fn category_glyph(category: emoji::Category, active: bool) -> Element<'static, AppMessage> {
    let icon = match category {
        emoji::Category::Recent => Icon::EmojiClock,
        emoji::Category::Faces => Icon::EmojiFace,
        emoji::Category::Hands => Icon::EmojiHands,
        emoji::Category::People => Icon::EmojiPeople,
        emoji::Category::Animals => Icon::EmojiAnimals,
        emoji::Category::Food => Icon::EmojiFood,
        emoji::Category::Places => Icon::EmojiPlaces,
        emoji::Category::Objects => Icon::EmojiObjects,
        emoji::Category::Sports => Icon::EmojiSports,
        emoji::Category::Symbols => Icon::EmojiSymbols,
    };
    icon_tone(icon, if active { Tone::Accent } else { Tone::Secondary }, 20.0)
}

fn unicode_glyph(unicode: &str, tone: usize) -> String {
    const MODIFIERS: [&str; 5] = ["🏻", "🏼", "🏽", "🏾", "🏿"];
    let Some(modifier) = tone.checked_sub(1).and_then(|index| MODIFIERS.get(index)) else {
        return unicode.to_owned();
    };
    let Some(first) = unicode.chars().next() else {
        return String::new();
    };
    let (base, rest) = unicode.split_at(first.len_utf8());
    let rest = rest.strip_prefix('\u{fe0f}').unwrap_or(rest);
    format!("{base}{modifier}{rest}")
}

pub fn glyph(token: &str, size: f32) -> Option<Element<'static, AppMessage>> {
    let (entry, tone) = resolve_key(crate::reactions::key(token)?)?;
    Some(text(unicode_glyph(&entry.unicode, tone))
        .font(typography::EMOJI)
        .size(size)
        .shaping(iced::widget::text::Shaping::Advanced)
        .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pinned_catalog_unicode_and_wire_identity_cover_every_visible_entry_and_tone() {
        let data = catalog();
        assert!(data.entries.len() > 1500);
        for entry in &data.entries {
            assert!(entry.categories.iter().all(|category| (1..=9).contains(category)));
            for tone in 0..if entry.diverse { 6 } else { 1 } {
                let token = token(entry, tone);
                let (resolved, actual_tone) = resolve_key(crate::reactions::key(&token).unwrap()).unwrap();
                assert_eq!(resolved.id, entry.id);
                assert_eq!(actual_tone, tone);
                assert!(!unicode_glyph(&resolved.unicode, tone).is_empty());
            }
        }
    }
    #[test]
    fn skin_tone_keeps_the_joined_sequence() {
        assert_eq!(unicode_glyph("👩‍💻", 3), "👩🏽‍💻");
        assert_eq!(unicode_glyph("✌️", 1), "✌🏻");
        assert_eq!(unicode_glyph("❤️", 0), "❤️");
    }
    #[test]
    fn localized_search_crosses_categories_and_recents_are_actual_unique_choices() {
        assert!(visible(emoji::Category::Faces, "foguete", &[]).iter().any(|entry| entry.id == "launch"));
        assert!(visible(emoji::Category::Hands, "cafe", &[]).iter().any(|entry| entry.unicode == "☕"));
        assert!(visible(emoji::Category::Recent, "", &[]).is_empty());
        let mut recent = Vec::new();
        remember(&mut recent, "1f44a_oncomingfist-tone1");
        remember(&mut recent, "👍"); remember(&mut recent, "👍");
        remember(&mut recent, "invented-reaction");
        assert_eq!(recent, ["👍", "1f44a_oncomingfist-tone1"]);
        assert_eq!(visible(emoji::Category::Recent, "", &recent)[0].id, "yes");
    }
}
