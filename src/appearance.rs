use crate::preferences::Appearance;
use iced::widget::{button, container, pick_list, scrollable, text, text_input};
use iced::{Background, Border, Color, Theme};

/// Logical desktop sizes, scaled once by the user's interface zoom.
pub mod metrics {
    pub const TOP_BAR: f32 = 58.0;
    pub const NAV_RAIL: f32 = 80.0;
    pub const NAV_ITEM: f32 = 68.0;
    pub const CHAT_ROW: f32 = 64.0;
    pub const CHAT_AVATAR: f32 = 48.0;
    pub const CHAT_TITLE: f32 = 18.0;
    pub const CHAT_PREVIEW: f32 = 15.0;
    pub const MESSAGE_AVATAR: f32 = 44.0;
    pub const MESSAGE_AUTHOR: f32 = 15.0;
    pub const CHAT_HEADER: f32 = 88.0;
    /// Center the composer and message edges in the conversation pane.
    pub const COMPOSER_WIDTH: f32 = 1094.0;
    pub const MESSAGE_BUBBLE_WIDTH: f32 = 976.0;
    pub const HISTORY_GUTTER: f32 = 24.0;
    pub const MESSAGE_AVATAR_GAP: f32 = 10.0;
    pub const MESSAGE_LEADING: f32 = MESSAGE_AVATAR + MESSAGE_AVATAR_GAP;
    pub const HISTORY_WIDTH: f32 = COMPOSER_WIDTH + MESSAGE_LEADING + HISTORY_GUTTER * 2.0;
}

/// Semantic roles shared by the desktop shell, conversation list and chat.
#[derive(Clone, Copy)]
pub enum Tone {
    Transparent,
    TopBar,
    Rail,
    Sidebar,
    Canvas,
    CalendarCanvas,
    CalendarTodayCanvas,
    CalendarSidebar,
    CalendarToolbar,
    CalendarGrid,
    CalendarMinor,
    CalendarEvent,
    CalendarAccent,
    CalendarSelection,
    CalendarSelectedEvent,
    CalendarButtonText,
    Surface,
    Card,
    Hover,
    Selected,
    PickerSelected,
    Mention,
    MentionEveryone,
    SelectedHover,
    Border,
    /// Quiet stroke for quote cards and reaction rings.
    QuoteBar,
    SettingsBorder,
    MenuText,
    MenuBorder,
    Text,
    Secondary,
    Muted,
    Accent,
    Danger,
    Primary,
    OnPrimary,
    Incoming,
    Outgoing,
    MediaViewer,
    MediaViewerText,
    PresenceAvailable,
    PresenceBusy,
    PresenceAway,
    PresenceOffline,
}

pub fn tone(theme: &Theme, role: Tone) -> Color {
    let bg = theme.palette().background;
    let dark = bg.r + bg.g + bg.b < 1.5;
    let (light, dark_color) = match role {
        Tone::Transparent => return Color::TRANSPARENT,
        Tone::TopBar => (0xf5f5f5, 0x0c0c0c),
        Tone::Rail => (0xf0f0f0, 0x0c0c0c),
        Tone::Sidebar => (0xfafafa, 0x181818),
        Tone::CalendarCanvas => (0xffffff, 0x212121),
        Tone::CalendarTodayCanvas => (0xffffff,0x252525),
        Tone::CalendarSidebar => (0xfafafa,0x080808),
        Tone::CalendarToolbar => (0xf5f5f5, 0x252525),
        Tone::CalendarGrid => (0xd6d6d6, 0x545454),
        Tone::CalendarMinor => (0xeeeeee, 0x131313),
        // Calendar shares the Teams blue with chat; only the calendar
        // surfaces remain semantically separate for selection and events.
        Tone::CalendarAccent => (0x5b5fc7, 0x7e84f4),
        Tone::CalendarSelection => (0xe8eafa, 0x2b2b3e),
        Tone::CalendarSelectedEvent => (0xdadaf5, 0x42457f),
        Tone::CalendarButtonText => (0xffffff, 0xffffff),
        Tone::CalendarEvent => (0xe8eafa, 0x1f1f2f),
        Tone::Canvas => (0xffffff, 0x1c1c1c),
        Tone::Surface => (0xffffff, 0x252525),
        Tone::Card => (0xffffff, 0x2e2e2e),
        Tone::Hover => (0xf0f0f4, 0x252525),
        Tone::MentionEveryone => (0x953e46, 0xe27c7f),
        Tone::Mention => (0x9b3a1c, 0xe8825e),
        Tone::PickerSelected => (0xe8eafa, 0x333333),
        Tone::Selected => (0xe8eafa, 0x212121),
        Tone::SelectedHover => (0xdfe2f6, 0x282828),
        Tone::Border => (0xe1e1e6, 0x303030),
        Tone::QuoteBar => (0x686873, 0x6e6e6e),
        Tone::MenuText => (0x24242b, 0xd5d5d5),
        Tone::MenuBorder => (0xe1e1e6, 0x4b4b4b),
        Tone::SettingsBorder => (0xe1e1e6, 0x4a4a4a),
        Tone::Text => (0x24242b, 0xffffff),
        Tone::Secondary => (0x585862, 0xb3b3b3),
        Tone::Muted => (0x686873, 0xa3a3a3),
        Tone::Accent => (0x5054b4, 0x7e84f4),
        Tone::Danger => (0xb42318, 0xff8a80),
        Tone::Primary => (0x5b5fc7, 0x5b5fc7),
        Tone::OnPrimary => (0xffffff, 0xffffff),
        Tone::Incoming => (0xf5f5f8, 0x252525),
        Tone::Outgoing => (0xe8eafa, 0x24243c),
        Tone::MediaViewer => (0x111216, 0x111216),
        Tone::MediaViewerText => (0xf4f4f6, 0xf4f4f6),
        Tone::PresenceAvailable => (0x107c10, 0x42963d),
        Tone::PresenceBusy => (0xc4314b, 0xc4314b),
        Tone::PresenceAway => (0xe6a700, 0xeaa300),
        Tone::PresenceOffline => (0x8a8886, 0x8a8886),
    };
    rgb(if dark { dark_color } else { light })
}

pub fn ui_text(role: Tone) -> impl Fn(&Theme) -> text::Style {
    move |theme| text::Style {
        color: Some(tone(theme, role)),
    }
}

/// Quote and reaction outlines sit close to the fill, so the stroke stays visible without a hard edge.
pub fn quiet_frame(fill: Tone, radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let background = tone(theme, fill);
        let palette = theme.palette().background;
        let line = if palette.r + palette.g + palette.b < 1.5 {
            let step = 8.0 / 255.0;
            Color {
                r: (background.r + step).min(1.0),
                g: (background.g + step).min(1.0),
                b: (background.b + step).min(1.0),
                a: 1.0,
            }
        } else {
            tone(theme, Tone::Border)
        };
        container::Style {
            background: Some(Background::Color(background)),
            border: Border::default().rounded(radius).width(1.0).color(line),
            ..Default::default()
        }
    }
}

pub fn quote_mark(radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |theme| container::Style {
        background: Some(Background::Color(tone(theme, Tone::QuoteBar))),
        border: Border::default().rounded(radius),
        ..Default::default()
    }
}

pub fn ui_surface(role: Tone, border: bool, radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |theme| container::Style {
        background: Some(Background::Color(tone(theme, role))),
        border: Border::default()
            .rounded(radius)
            .color(tone(theme, Tone::Border))
            .width(if border { 1.0 } else { 0.0 }),
        ..Default::default()
    }
}

pub fn ui_button(
    base: Tone,
    hover: Tone,
    foreground: Tone,
    radius: f32,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let (background, text) = match status {
            button::Status::Hovered => (tone(theme, hover), tone(theme, foreground)),
            button::Status::Pressed => {
                (tone(theme, hover).scale_alpha(0.8), tone(theme, foreground))
            }
            button::Status::Disabled => (tone(theme, base), tone(theme, Tone::Muted)),
            _ => (tone(theme, base), tone(theme, foreground)),
        };
        button::Style {
            background: Some(background.into()),
            text_color: text,
            border: Border::default().rounded(radius),
            ..Default::default()
        }
    }
}

/// Compact outlined filters keep a stable boundary in every pointer state.
pub fn ui_filter(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = ui_button(
            if selected {
                Tone::Selected
            } else {
                Tone::Transparent
            },
            Tone::Hover,
            if selected {
                Tone::Accent
            } else {
                Tone::Secondary
            },
            16.0,
        )(theme, status);
        style.border = Border::default().rounded(16).width(1.0).color(
            tone(theme, if selected { Tone::Accent } else { Tone::Border })
                .scale_alpha(if selected { 0.5 } else { 1.0 }),
        );
        style
    }
}

pub fn ui_input(framed: bool) -> impl Fn(&Theme, text_input::Status) -> text_input::Style {
    move |theme, status| text_input::Style {
        background: if framed {
            tone(theme, Tone::Surface).into()
        } else {
            Color::TRANSPARENT.into()
        },
        border: Border::default().rounded(6).width(1.0).color(match status {
            text_input::Status::Focused { .. } => tone(theme, Tone::Accent),
            _ if framed => tone(theme, Tone::Border),
            _ => Color::TRANSPARENT,
        }),
        icon: tone(theme, Tone::Secondary),
        placeholder: tone(theme, Tone::Muted),
        value: tone(theme, Tone::Text),
        selection: tone(theme, Tone::SelectedHover),
    }
}

pub fn ui_pick_list() -> impl Fn(&Theme, pick_list::Status) -> pick_list::Style {
    move |theme, status| {
        let hovered = matches!(
            status,
            pick_list::Status::Hovered | pick_list::Status::Opened { .. }
        );
        pick_list::Style {
            text_color: tone(theme, Tone::Text),
            placeholder_color: tone(theme, Tone::Muted),
            handle_color: tone(theme, Tone::Secondary),
            background: tone(theme, if hovered { Tone::Hover } else { Tone::Surface }).into(),
            border: Border::default().rounded(6).width(1.0).color(if hovered {
                tone(theme, Tone::Accent)
            } else {
                tone(theme, Tone::Border)
            }),
        }
    }
}

pub fn ui_scroll(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    let hovered = !matches!(status, scrollable::Status::Active { .. });
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: tone(theme, if hovered { Tone::Muted } else { Tone::Border }).into(),
            border: Border::default().rounded(3),
        },
    };
    style.vertical_rail = rail;
    style.horizontal_rail = rail;
    style
}

pub fn rgb(value: u32) -> Color {
    Color::from_rgb8((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

pub fn teams_theme(mode: Appearance) -> Theme {
    let dark = mode == Appearance::Dark;
    Theme::custom(
        if dark { "Teams Dark" } else { "Teams Light" },
        iced::theme::Palette {
            background: rgb(if dark { 0x1c1c1c } else { 0xffffff }),
            text: rgb(if dark { 0xffffff } else { 0x24242b }),
            primary: rgb(if dark { 0x7e84f4 } else { 0x5b5fc7 }),
            success: rgb(if dark { 0x6cc46f } else { 0x107c10 }),
            warning: rgb(0xfce100),
            danger: rgb(if dark { 0xff9b9f } else { 0xc4314b }),
        },
    )
}

pub fn shade(theme: &Theme, light: Color, surface: bool) -> Color {
    let background = theme.palette().background;
    if background.r + background.g + background.b > 1.5 || light.a == 0.0 {
        return light;
    }
    let key = ((light.r * 255.0).round() as u32) << 16
        | ((light.g * 255.0).round() as u32) << 8
        | (light.b * 255.0).round() as u32;
    let value = match key {
        0xffffff if surface => 0x171717,
        0xf5f5f5 => 0x131313,
        0xf3f2f1 => 0x0e0e0e,
        0xf7f7f7 | 0xf8f8fb => 0x1c1c1c,
        0xf0f0f0 | 0xe9e9e9 => 0x282828,
        0xe8e7f3 | 0xe9e8f4 | 0xdeddef => 0x2d2c48,
        0xf0eff8 => 0x2e2b47,
        0xe1dfdd => 0x303030,
        0xd1d1d1 | 0xc8c6c4 => 0x484848,
        0xd1d0e5 => 0x4f4c6f,
        0x242424 => 0xf5f5f5,
        0x424242 | 0x505050 => 0xe0e0e0,
        0x5b5b5b | 0x616161 => 0xbdbdbd,
        0x737373 => 0xaaaaaa,
        0x6264a7 if surface => 0x5b5fc7,
        0x6264a7 | 0x4f52a0 => 0xb6b9ff,
        0x55579a => 0x7478df,
        0x13a10e => 0x6cc46f,
        0xa4262c => 0xffa1a5,
        0xfdf3f4 => 0x341c21,
        0xf1c7cb => 0x753f45,
        _ => key,
    };
    Color {
        a: light.a,
        ..rgb(value)
    }
}

pub fn text_style(light: Color) -> impl Fn(&Theme) -> text::Style {
    move |theme| text::Style {
        color: Some(shade(theme, light, false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn luminance(color: Color) -> f32 {
        let channel = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
    }
    #[test]
    fn chat_tokens_meet_text_contrast_in_both_themes() {
        for mode in [Appearance::Light, Appearance::Dark] {
            let theme = teams_theme(mode);
            for surface in [
                Tone::Sidebar,
                Tone::Canvas,
                Tone::Surface,
                Tone::Selected,
                Tone::Incoming,
                Tone::Outgoing,
                Tone::CalendarCanvas,
                Tone::CalendarTodayCanvas,
                Tone::CalendarToolbar,
                Tone::CalendarEvent,
                Tone::CalendarSelection,
            ] {
                for foreground in [Tone::Text, Tone::Secondary, Tone::Muted] {
                    let a = luminance(tone(&theme, foreground));
                    let b = luminance(tone(&theme, surface));
                    assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5);
                }
            }
            let a=luminance(tone(&theme,Tone::CalendarButtonText));
            let b=luminance(tone(&theme,Tone::CalendarAccent));
            assert!((a.max(b)+0.05)/(a.min(b)+0.05)>=4.5);
            let a=luminance(tone(&theme,Tone::Text));
            let b=luminance(tone(&theme,Tone::CalendarSelectedEvent));
            assert!((a.max(b)+0.05)/(a.min(b)+0.05)>=4.5);
            let a = luminance(tone(&theme, Tone::OnPrimary));
            let b = luminance(tone(&theme, Tone::Primary));
            assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5);
            let a = luminance(tone(&theme, Tone::MediaViewerText));
            let b = luminance(tone(&theme, Tone::MediaViewer));
            assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5);
        }
    }
    #[test]
    fn dark_text_remains_readable_on_surfaces_and_own_messages() {
        let theme = teams_theme(Appearance::Dark);
        for surface in [Color::WHITE, rgb(0xf0eff8), rgb(0xf5f5f5)] {
            for foreground in [0x242424, 0x616161, 0x737373] {
                let a = luminance(shade(&theme, rgb(foreground), false));
                let b = luminance(shade(&theme, surface, true));
                assert!((a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5);
            }
        }
        assert_eq!(shade(&theme, Color::TRANSPARENT, true), Color::TRANSPARENT);
        assert_eq!(shade(&theme, Color::WHITE, false), Color::WHITE);
    }
}
