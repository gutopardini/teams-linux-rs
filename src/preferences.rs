use crate::i18n::tr;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkinTone {
    Light,
    MediumLight,
    Medium,
    MediumDark,
    Dark,
    #[default]
    #[serde(other)]
    Default,
}

impl SkinTone {
    pub const ALL: [Self; 6] = [Self::Default, Self::Light, Self::MediumLight, Self::Medium, Self::MediumDark, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => tr("Default"), Self::Light => tr("Light"), Self::MediumLight => tr("Medium light"),
            Self::Medium => tr("Medium"), Self::MediumDark => tr("Medium dark"), Self::Dark => tr("Dark"),
        }
    }

    pub fn color(self) -> u32 {
        match self {
            Self::Default => 0xffce3c, Self::Light => 0xe5b49c, Self::MediumLight => 0xcfa185,
            Self::Medium => 0xac8675, Self::MediumDark => 0x906b60, Self::Dark => 0x60483f,
        }
    }

    pub fn apply(self, emoji: &str) -> String {
        let modifier = match self {
            Self::Default => "", Self::Light => "🏻", Self::MediumLight => "🏼",
            Self::Medium => "🏽", Self::MediumDark => "🏾", Self::Dark => "🏿",
        };
        if modifier.is_empty() { return emoji.into(); }
        if matches!(emoji, "👍" | "🙏" | "👊" | "🙌" | "👏" | "🫶" | "👎" | "☝️" | "🤞" | "👋" | "👌" | "💪") {
            format!("{}{modifier}", emoji.trim_end_matches('\u{fe0f}'))
        } else { emoji.into() }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FontFamily {
    Inter,
    #[default]
    #[serde(other)]
    Selawik,
}

impl FontFamily {
    pub fn label(self) -> &'static str {
        match self { Self::Selawik => "Selawik", Self::Inter => "Inter" }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    Light,
    #[default]
    Dark,
}

impl Appearance {
    pub fn toggle(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Light,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => tr("Light"),
            Self::Dark => tr("Dark"),
        }
    }
}

impl std::fmt::Display for Appearance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub alerts: Alerts,
    pub language: crate::i18n::Language,
    pub appearance: Appearance,
    pub font_family: FontFamily,
    pub message_size: u16,
    pub interface_zoom: u16,
    pub chat_sidebar_width: u16,
    #[serde(default)]
    pub(crate) sidebar_width_updated: bool,
    #[serde(default)]
    pub(crate) sidebar_proportions_updated: bool,
    pub roomy_lines: bool,
    pub enter_sends: bool,
    pub skin_tone: SkinTone,
    pub recent_emojis: Vec<String>,
    pub recent_reactions: Vec<String>,
    pub quick_reactions: Vec<String>,
    pub calendar_view: crate::calendar::CalendarView,
    pub audio_input: String,
    pub audio_output: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            alerts: Alerts::default(),
            language: crate::i18n::Language::English,
            appearance: Appearance::Dark,
            font_family: FontFamily::Selawik,
            message_size: 16,
            interface_zoom: 100,
            chat_sidebar_width: 426,
            sidebar_width_updated: true,
            sidebar_proportions_updated: true,
            roomy_lines: false,
            enter_sends: true,
            skin_tone: SkinTone::default(),
            recent_emojis: Vec::new(),
            recent_reactions: Vec::new(),
            quick_reactions: vec!["👍".into(),"❤️".into(),"😂".into(),"😮".into()],
            calendar_view: crate::calendar::CalendarView::default(),
            audio_input: String::new(),
            audio_output: String::new(),
        }
    }
}

impl Preferences {
    fn path() -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|base| base.join("teams-linux-rs/preferences.json"))
    }

    pub fn load() -> Self {
        let mut preferences: Self = Self::path()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        preferences.upgrade_sidebar_width();
        preferences.normalize();
        preferences
    }

    fn upgrade_sidebar_width(&mut self) {
        if !self.sidebar_width_updated && self.chat_sidebar_width == 312 {
            self.chat_sidebar_width = 388;
        }
        self.sidebar_width_updated = true;
        if !self.sidebar_proportions_updated && self.chat_sidebar_width == 388 {
            self.chat_sidebar_width = 426;
        }
        self.sidebar_proportions_updated = true;
    }

    pub fn normalize(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.recent_emojis.retain(|value| crate::emoji::supported(value) && seen.insert(value.clone()));
        self.recent_emojis.truncate(36);
        let mut seen = std::collections::HashSet::new();
        self.recent_reactions.retain(|token| crate::reactions::key(token).is_some() && seen.insert(token.clone()));
        self.recent_reactions.truncate(36);
        self.quick_reactions.retain(|token| crate::reactions::key(token).is_some());
        self.quick_reactions.truncate(4);
        if self.quick_reactions.is_empty() { self.quick_reactions = Self::default().quick_reactions; }
        self.chat_sidebar_width = self.chat_sidebar_width.clamp(240, 560);
        self.message_size = self.message_size.clamp(14, 32);
        self.interface_zoom = self.interface_zoom.clamp(80, 200);
    }

    /// Visual reference preset. Keep user zoom, theme and device choices.
    pub fn apply_teams_reference(&mut self) {
        self.message_size = 16;
        self.roomy_lines = false;
        self.chat_sidebar_width = 426;
        self.sidebar_proportions_updated = true;
    }

    pub fn line_height(&self) -> f32 {
        if self.roomy_lines { 1.65 } else { 1.35 }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path().ok_or(tr("Could not locate the preferences folder."))?;
        let parent = path.parent().ok_or(tr("The preferences folder is invalid."))?;
        fs::create_dir_all(parent)
            .map_err(|_| tr("Could not create the preferences folder."))?;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|_| tr("Could not save preferences."))?;
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, bytes)
            .and_then(|_| fs::rename(&temp, path))
            .map_err(|_| tr("Could not save preferences.").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chat_options_roundtrip_and_old_preferences_keep_enter_to_send() {
        let old: Preferences = serde_json::from_str("{}").unwrap();
        assert!(old.enter_sends);
        assert_eq!(old.skin_tone, SkinTone::Default);
        for tone in SkinTone::ALL {
            let prefs = Preferences { skin_tone: tone, enter_sends: false, ..old.clone() };
            let restored: Preferences = serde_json::from_slice(&serde_json::to_vec(&prefs).unwrap()).unwrap();
            assert_eq!(restored, prefs);
            assert_eq!(tone.apply("😀"), "😀");
            assert_eq!(tone.apply("❤️"), "❤️");
            assert!(tone.apply("👍").starts_with('👍'));
            assert_eq!(unicode_segmentation::UnicodeSegmentation::graphemes(tone.apply("🙏").as_str(), true).count(), 1);
        }
        assert_eq!(SkinTone::Dark.apply("👍"), "👍🏿");
        let unknown: Preferences = serde_json::from_str(r#"{"skin_tone":"future"}"#).unwrap();
        assert_eq!(unknown.skin_tone, SkinTone::Default);
        let mut recent: Preferences = serde_json::from_str(r#"{"recent_emojis":["😀","invalid","😀","🇧🇷"]}"#).unwrap();
        recent.normalize();
        assert_eq!(recent.recent_emojis, ["😀", "🇧🇷"]);
    }
    #[test]
    fn font_selection_roundtrips_and_old_or_unknown_values_are_safe() {
        let mut prefs: Preferences = serde_json::from_str(r#"{"appearance":"light"}"#).unwrap();
        assert_eq!(prefs.font_family, FontFamily::Selawik);
        prefs.font_family = FontFamily::Inter;
        let restored: Preferences = serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
        assert_eq!(restored.font_family, FontFamily::Inter);
        let unknown: Preferences = serde_json::from_str(r#"{"font_family":"future","appearance":"light"}"#).unwrap();
        assert_eq!(unknown.font_family, FontFamily::Selawik);
        assert_eq!(unknown.appearance, Appearance::Light);
    }
    #[test]
    fn old_sidebar_default_is_upgraded_once_and_custom_widths_survive() {
        let mut old: Preferences = serde_json::from_str(r#"{"chat_sidebar_width":312}"#).unwrap();
        old.upgrade_sidebar_width();
        assert_eq!(old.chat_sidebar_width, 426);
        old.chat_sidebar_width = 312;
        let mut saved: Preferences = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        saved.upgrade_sidebar_width();
        assert_eq!(saved.chat_sidebar_width, 312);
        let mut custom: Preferences = serde_json::from_str(r#"{"chat_sidebar_width":420}"#).unwrap();
        custom.upgrade_sidebar_width();
        assert_eq!(custom.chat_sidebar_width, 420);
        let mut previous_default: Preferences = serde_json::from_str(r#"{"chat_sidebar_width":388,"sidebar_width_updated":true}"#).unwrap();
        previous_default.upgrade_sidebar_width();
        assert_eq!(previous_default.chat_sidebar_width, 426);
    }
    #[test]
    fn reference_preset_preserves_zoom_theme_calendar_and_audio_devices() {
        let mut preferences = Preferences {
            message_size: 19, roomy_lines: true, chat_sidebar_width: 407,
            interface_zoom: 125, appearance: Appearance::Light,
            audio_input: "microphone".into(), audio_output: "headphones".into(),
            calendar_view: crate::calendar::CalendarView::Agenda,
            ..Default::default()
        };
        preferences.apply_teams_reference();
        assert_eq!(preferences.message_size, 16);
        assert!(!preferences.roomy_lines);
        assert_eq!(preferences.chat_sidebar_width, 426);
        assert_eq!(preferences.interface_zoom, 125);
        assert_eq!(preferences.calendar_view, crate::calendar::CalendarView::Agenda);
        assert_eq!(preferences.appearance, Appearance::Light);
        assert_eq!(preferences.audio_input, "microphone");
        assert_eq!(preferences.audio_output, "headphones");
    }

    #[test]
    fn appearance_roundtrips_and_toggle_is_reversible() {
        let mode = Appearance::Dark;
        assert_eq!(mode.toggle().toggle(), mode);
        let value = serde_json::to_string(&Preferences {
            appearance: mode,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Preferences>(&value)
                .unwrap()
                .appearance,
            mode
        );
    }

    #[test]
    fn old_preferences_keep_the_theme_and_new_controls_have_safe_bounds() {
        let mut preferences: Preferences =
            serde_json::from_str(r#"{"appearance":"light"}"#).unwrap();
        assert_eq!(preferences.appearance, Appearance::Light);
        assert_eq!(
            (preferences.message_size, preferences.interface_zoom),
            (16, 100)
        );
        preferences.message_size = 200;
        preferences.interface_zoom = 1;
        preferences.normalize();
        assert_eq!(
            (preferences.message_size, preferences.interface_zoom),
            (32, 80)
        );
        assert!(preferences.audio_input.is_empty());
        assert!(preferences.audio_output.is_empty());
        let copy: Preferences =
            serde_json::from_str(&serde_json::to_string(&preferences).unwrap()).unwrap();
        assert_eq!(copy, preferences);
    }

    #[test]
    fn audio_device_keys_roundtrip_without_display_metadata() {
        let preferences = Preferences {
            audio_input: "alsa_input.usb-microphone".into(),
            audio_output: "alsa_output.usb-headset".into(),
            ..Default::default()
        };
        let copy: Preferences =
            serde_json::from_str(&serde_json::to_string(&preferences).unwrap()).unwrap();
        assert_eq!(copy.audio_input, "alsa_input.usb-microphone");
        assert_eq!(copy.audio_output, "alsa_output.usb-headset");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Alerts { pub banners: bool, pub sounds: bool, pub meeting_sounds: bool }
impl Default for Alerts {fn default()->Self{Self{banners:true,sounds:true,meeting_sounds:true}}}

#[cfg(test)]
mod alert_tests {
    use super::*;
    #[test]
    fn existing_preferences_enable_alerts_and_preserve_explicit_choices_on_roundtrip() {
        let mut prefs:Preferences=serde_json::from_str(r#"{"appearance":"light"}"#).unwrap();
        assert_eq!(prefs.alerts,Alerts::default());
        prefs.alerts=Alerts{banners:false,sounds:false,meeting_sounds:false};
        let restored:Preferences=serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
        assert_eq!(restored.alerts,prefs.alerts);
    }
}
