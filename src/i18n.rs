//! English is the source language. Locale catalogs translate the source messages.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{OnceLock, atomic::{AtomicU8, Ordering}};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Language {
    #[serde(rename = "pt-BR")]
    PortugueseBrazil = 1,
    #[default]
    #[serde(rename = "en", other)]
    English = 0,
}

impl Language {
    pub const ALL: [Self; 2] = [Self::English, Self::PortugueseBrazil];

    pub fn tag(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::PortugueseBrazil => "pt-BR",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "en" => Some(Self::English),
            "pt-BR" => Some(Self::PortugueseBrazil),
            _ => None,
        }
    }

    fn catalog(self) -> Option<&'static HashMap<String, String>> {
        match self {
            Self::English => None,
            Self::PortugueseBrazil => {
                static CATALOG: OnceLock<HashMap<String, String>> = OnceLock::new();
                Some(CATALOG.get_or_init(|| {
                    serde_json::from_str(include_str!("../locales/pt-BR.json"))
                        .expect("valid embedded Portuguese catalog")
                }))
            }
        }
    }

    pub fn translate(self, english: &'static str) -> &'static str {
        self.catalog().and_then(|catalog| catalog.get(english)).map_or(english, String::as_str)
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::English => "English",
            Self::PortugueseBrazil => "Português (Brasil)",
        })
    }
}

static LANGUAGE: AtomicU8 = AtomicU8::new(Language::English as u8);

pub fn set_language(language: Language) {
    LANGUAGE.store(language as u8, Ordering::Relaxed);
}

pub fn language() -> Language {
    let selected = LANGUAGE.load(Ordering::Relaxed);
    Language::ALL.into_iter().find(|language| *language as u8 == selected).unwrap_or_default()
}

pub fn tr(english: &'static str) -> &'static str {
    language().translate(english)
}

/// Interpolate named values so translations can change their order.
pub fn format(english: &'static str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut rest = tr(english);
    let mut output = String::with_capacity(rest.len());
    while let Some((prefix, variable)) = rest.split_once('{') {
        let Some((name, next)) = variable.split_once('}') else { break };
        output.push_str(prefix);
        if let Some((_, value)) = args.iter().find(|(key, _)| *key == name) {
            use std::fmt::Write;
            let _ = write!(output, "{value}");
        } else {
            output.push('{');
            output.push_str(name);
            output.push('}');
        }
        rest = next;
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_catalog_has_an_english_fallback() {
        assert_eq!(Language::default().translate("Settings"), "Settings");
        assert_eq!(Language::PortugueseBrazil.translate("Settings"), "Configurações");
        assert_eq!(Language::PortugueseBrazil.translate("Teams Linux"), "Teams Linux");
        assert_eq!(format("{name} is typing", &[("name", &"Alex")]), "Alex is typing");
    }

    #[test]
    fn saved_preferences_select_a_locale_and_default_to_english() {
        use crate::preferences::Preferences;
        let old: Preferences = serde_json::from_str(r#"{"message_size":18}"#).unwrap();
        assert_eq!(old.language, Language::English);
        let portuguese: Preferences = serde_json::from_str(r#"{"language":"pt-BR"}"#).unwrap();
        assert_eq!(portuguese.language, Language::PortugueseBrazil);
        let unknown: Preferences = serde_json::from_str(r#"{"language":"fr","message_size":18}"#).unwrap();
        assert_eq!(unknown.language, Language::English);
        assert_eq!(unknown.message_size, 18);
    }
}
