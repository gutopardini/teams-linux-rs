use crate::i18n::tr;
use crate::{
    AppMessage, TeamsApp,
    appearance::{Tone, ui_button, ui_surface, ui_text},
    audio::AudioDevice,
    i18n::Language,
    preferences::{Appearance, FontFamily},
    semibold,
};
use iced::widget::{button, column, container, progress_bar, row, scrollable, slider, space, text};
use iced::{Alignment, Element, Fill};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Appearance,
    Chats,
    Accessibility,
    Notifications,
    Devices,
}

impl Page {
    pub const ALL: [Self; 5] = [Self::Appearance, Self::Chats, Self::Accessibility, Self::Notifications, Self::Devices];
    pub fn title(self) -> &'static str {
        match self {
            Self::Appearance => tr("Appearance"), Self::Chats => tr("Chats and channels"),
            Self::Accessibility => tr("Accessibility"), Self::Notifications => tr("Notifications and sounds"), Self::Devices => tr("Devices"),
        }
    }
    fn icon(self) -> crate::Icon {
        match self {
            Self::Appearance => crate::Icon::Theme, Self::Chats => crate::Icon::Chat,
            Self::Accessibility => crate::Icon::Format, Self::Notifications => crate::Icon::Activity, Self::Devices => crate::Icon::Speaker,
        }
    }
    pub fn matches(self, query: &str) -> bool {
        let terms = match self {
            Self::Appearance => "appearance theme light dark language skin tone emoji",
            Self::Chats => "chats channels sidebar list width conversation",
            Self::Accessibility => "accessibility reading font size zoom line spacing",
            Self::Notifications => "notifications sounds calls meetings alerts banners gnome",
            Self::Devices => "devices audio microphone speaker output test",
        };
        let normalize = |value: &str| value.to_lowercase().chars().map(|c| match c {
            'á' | 'à' | 'ã' | 'â' => 'a', 'é' | 'ê' => 'e', 'í' => 'i',
            'ó' | 'ô' | 'õ' => 'o', 'ú' => 'u', 'ç' => 'c', _ => c,
        }).collect::<String>();
        let terms = normalize(&format!(
            "{terms} {}", Language::PortugueseBrazil.translate(terms)
        ));
        normalize(query).split_whitespace().all(|word| terms.contains(word))
    }
    pub fn demo() -> Self {
        std::env::args().find_map(|arg| match arg.strip_prefix("--settings-page-demo=")? {
            "chats" => Some(Self::Chats), "accessibility" => Some(Self::Accessibility),
            "notifications" => Some(Self::Notifications), "devices" => Some(Self::Devices), _ => None,
        }).unwrap_or_default()
    }
}

pub fn view(state: &TeamsApp, width: f32) -> Element<'_, AppMessage> {
    let page = state.settings_page;
    let narrow = width < 1050.0;
    let navigation_width = if narrow { 260.0 } else { 600.0_f32.min(width * 0.34) };
    let header_height = crate::appearance::metrics::CHAT_HEADER;
    let title = |label| container(text(label).size(22).font(semibold()).style(ui_text(Tone::Text)))
        .height(header_height).center_y(header_height).padding([0, 40]).width(Fill);
    let mut navigation = column![
        container(iced::widget::text_input(tr("Find a setting (Ctrl+Alt+F)"), &state.settings_query)
            .id("settings-search").on_input(AppMessage::SettingsSearchChanged)
            .size(18).padding([8, 16]).style(crate::appearance::ui_input(true)))
            .padding(iced::Padding { left: 20.0, ..Default::default() }),
    ].spacing(4);
    if !state.settings_query.is_empty() {
        navigation = navigation.push(button(text(tr("Clear search")).size(13)).on_press(AppMessage::SettingsSearchChanged(String::new()))
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Accent, 4.0)));
    }
    let mut count = 0;
    for candidate in Page::ALL {
        if !candidate.matches(&state.settings_query) { continue; }
        count += 1;
        let selected = candidate == page;
        navigation = navigation.push(button(row![
            crate::icon_tone(candidate.icon(), if selected { Tone::Accent } else { Tone::Secondary }, 20.0),
            text(candidate.title()).size(18),
        ].spacing(16).align_y(Alignment::Center))
            .width(Fill).padding([8, 20]).on_press(AppMessage::SelectSettingsPage(candidate))
            .style(ui_button(if selected { Tone::Selected } else { Tone::Transparent }, Tone::Hover, Tone::Text, 4.0)));
    }
    if count == 0 {
        navigation = navigation.push(text(tr("No settings found.")).size(14).style(ui_text(Tone::Secondary)));
    }
    let sidebar = container(column![title(tr("Settings")), crate::divider(),
        container(scrollable(navigation).style(crate::ui_scroll)).padding([14, 20]).height(Fill),
        container(button(row![crate::icon_tone(crate::Icon::Back, Tone::Secondary, 18.0),text(tr("Back to Teams")).size(16)].spacing(12).align_y(Alignment::Center))
            .on_press(AppMessage::CloseSettings).padding([8, 16]).style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)))
            .padding([18, 24]),
    ].spacing(0)).width(navigation_width).height(Fill).style(ui_surface(Tone::Sidebar, false, 0.0));
    let body = match page {
        Page::Appearance => appearance_content(state),
        Page::Chats => chats_content(state),
        Page::Accessibility => reading_content(state),
        Page::Notifications => notifications_content(state),
        Page::Devices => audio_content(state),
    };
    let mut content = column![body].spacing(16);
    if !state.settings_error.is_empty() {
        content = content.push(text(&state.settings_error).size(14).style(ui_text(Tone::Danger)))
            .push(button(text(tr("Try saving again"))).on_press(AppMessage::SavePreferences));
    } else if state.preferences_saving {
        content = content.push(text(tr("Saving changes…")).size(13).style(ui_text(Tone::Muted)));
    }
    let detail = column![
        container(row![text(page.title()).size(22).font(semibold()).style(ui_text(Tone::Text)),space::horizontal(),
            crate::hint(button(crate::icon_tone(crate::Icon::Back, Tone::Secondary, 20.0)).on_press(AppMessage::CloseSettings)
                .padding(8).style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)), tr("Back to Teams (Esc)"))
        ].align_y(Alignment::Center)).padding([0, 24]).height(header_height).center_y(header_height),
        crate::divider(),
        scrollable(container(content).width(Fill).padding([14, 24])).id("settings-content")
            .direction(crate::scroll_axis()).style(crate::ui_scroll).height(Fill),
    ].spacing(0).width(Fill).height(Fill);
    row![sidebar, container(space::vertical()).width(1).height(Fill).style(ui_surface(Tone::Border, false, 0.0)),
        container(detail).width(Fill).height(Fill).style(ui_surface(Tone::Canvas, false, 0.0))].height(Fill).into()
}

fn card<'a>(body: impl Into<Element<'a, AppMessage>>) -> Element<'a, AppMessage> {
    container(body).width(Fill).padding(24).style(|theme| {
        let mut style = ui_surface(Tone::Canvas, true, 8.0)(theme);
        style.border.color = crate::appearance::tone(theme, Tone::SettingsBorder);
        style
    }).into()
}

fn appearance_content(state: &TeamsApp) -> Element<'_, AppMessage> {
    let prefs = &state.preferences;
    let language_picker =
        iced::widget::pick_list(Language::ALL, Some(prefs.language), AppMessage::SetLanguage)
            .text_size(16)
            .padding([6, 12])
            .style(crate::appearance::ui_pick_list());
    let modes = [Appearance::Light, Appearance::Dark];
    let picker = iced::widget::pick_list(modes, Some(prefs.appearance), AppMessage::SetAppearance)
        .text_size(16).padding([6, 12]).style(crate::appearance::ui_pick_list());
    let mut tones = row![].spacing(14).align_y(Alignment::Center);
    for tone in crate::preferences::SkinTone::ALL {
        let swatch = button(container(space::horizontal()).width(36).height(36).style(move |_| iced::widget::container::Style {
            background: Some(crate::rgb(tone.color()).into()), border: iced::Border::default().rounded(18), ..Default::default()
        })).padding(0).on_press(AppMessage::SkinToneChanged(tone))
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 18.0));
        tones = tones.push(crate::hint(column![swatch,
            container(space::horizontal()).width(36).height(4).style(ui_surface(
                if prefs.skin_tone == tone { Tone::Accent } else { Tone::Transparent }, false, 2.0)),
        ].spacing(6), format!("Skin tone: {}", tone.label())));
    }
    column![card(row![
        crate::icon_tone(crate::Icon::Settings, Tone::Secondary, 20.0),
        text(tr("App language")).size(18).width(Fill),
        language_picker,
    ].spacing(20).align_y(Alignment::Center)),
    card(row![
        crate::icon_tone(crate::Icon::Theme, Tone::Secondary, 20.0),
        column![text(tr("Theme")).size(18), text(tr("Choose how Teams looks.")).size(14).style(ui_text(Tone::Secondary))].spacing(4).width(Fill),
        picker,
    ].spacing(20).align_y(Alignment::Start).height(102)),
    card(row![crate::icon_tone(crate::Icon::Reaction, Tone::Secondary, 20.0),
        column![text(tr("Skin tone preference")).size(18),
            text(tr("Emojis added from the picker use this skin tone.")).size(14).style(ui_text(Tone::Secondary)),
            container(tones).padding([16, 0]),
        ].spacing(4).width(Fill),
    ].spacing(20))].spacing(14).into()
}

fn chats_content(state: &TeamsApp) -> Element<'_, AppMessage> {
    let prefs = &state.preferences;
    column![card(column![heading(tr("Chat and channel list")),
        text(tr("Chat list width")).size(16),
        row![slider(240..=560, prefs.chat_sidebar_width, AppMessage::SidebarWidthChanged).step(2u16).on_release(AppMessage::SavePreferences),
            text(format!("{} px", prefs.chat_sidebar_width)).size(14)].spacing(16).align_y(Alignment::Center),
        button(text(tr("Teams reference proportions")).size(14)).on_press(AppMessage::ApplyTeamsReference).padding([8, 12])
            .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 4.0)),
    ].spacing(16)),
    card(column![
        heading(tr("When writing a message, press Enter to")),
        text(tr("This setting applies to the plain message editor.")).size(14).style(ui_text(Tone::Secondary)),
        iced::widget::radio(tr("Send the message"), true, Some(prefs.enter_sends), AppMessage::EnterSendsChanged).size(18).text_size(16),
        iced::widget::radio(tr("Start a new line (select Send or press Ctrl+Enter to send)."), false, Some(prefs.enter_sends), AppMessage::EnterSendsChanged).size(18).text_size(16),
    ].spacing(16)),
    ].spacing(14).into()
}

/// The plain composer follows the user's Enter preference. Expanded formatting
/// retains Iced's editor bindings, and Ctrl+Enter remains in paste::rich_composer.
pub fn composer_key(key: iced::widget::text_editor::KeyPress, enter_sends: bool)
    -> Option<iced::widget::text_editor::Binding<AppMessage>>
{
    use iced::{keyboard::{Key, key::Named}, widget::text_editor::{Binding, Status}};
    if !enter_sends && matches!(key.status, Status::Focused { .. })
        && key.key == Key::Named(Named::Enter) && !key.modifiers.control() && !key.modifiers.alt()
    {
        Some(Binding::Enter)
    } else {
        crate::quoted_reply::composer_key(key)
    }
}

fn reading_content(state: &TeamsApp) -> Element<'_, AppMessage> {
    let prefs = &state.preferences;
    let mut families = row![].spacing(8);
    for family in [FontFamily::Selawik, FontFamily::Inter] {
        let selected = prefs.font_family == family;
        families = families.push(
            button(text(family.label()).font(crate::typography::for_family(family))
                .size(15).width(Fill).align_x(Alignment::Center))
                .width(Fill).padding(12).on_press(AppMessage::SetFontFamily(family))
                .style(ui_button(
                    if selected { Tone::Selected } else { Tone::Surface },
                    Tone::SelectedHover,
                    if selected { Tone::Accent } else { Tone::Text },
                    7.0,
                )),
        );
    }
    let font_controls = row![
        button(text("A−").size(15).font(semibold()).width(Fill).align_x(Alignment::Center))
            .width(42)
            .padding([8, 0])
            .on_press_maybe((prefs.message_size > 14).then_some(AppMessage::MessageSizeStep(-1)))
            .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 6.0)),
        slider(14..=32, prefs.message_size, AppMessage::MessageSizeChanged)
            .step(1u16)
            .on_release(AppMessage::SavePreferences)
            .width(Fill),
        button(text("A+").size(15).font(semibold()).width(Fill).align_x(Alignment::Center))
            .width(42)
            .padding([8, 0])
            .on_press_maybe((prefs.message_size < 32).then_some(AppMessage::MessageSizeStep(1)))
            .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 6.0)),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    let mut zoom_presets = row![].spacing(8);
    for zoom in [100u16, 125, 150, 200] {
        zoom_presets = zoom_presets.push(
            button(
                text(format!("{zoom}%"))
                    .size(13)
                    .width(Fill)
                    .align_x(Alignment::Center),
            )
            .width(Fill)
            .padding([8, 0])
            .on_press(AppMessage::ZoomPreset(zoom))
            .style(ui_button(
                if prefs.interface_zoom == zoom {
                    Tone::Selected
                } else {
                    Tone::Surface
                },
                Tone::SelectedHover,
                if prefs.interface_zoom == zoom {
                    Tone::Accent
                } else {
                    Tone::Text
                },
                6.0,
            )),
        );
    }
    let mut spacing = row![].spacing(8);
    for (roomy, label) in [(false, "Normal"), (true, tr("Comfortable"))] {
        spacing = spacing.push(
            button(text(label).size(13).width(Fill).align_x(Alignment::Center))
                .width(Fill)
                .padding([10, 0])
                .on_press(AppMessage::RoomyLinesChanged(roomy))
                .style(ui_button(
                    if prefs.roomy_lines == roomy {
                        Tone::Selected
                    } else {
                        Tone::Surface
                    },
                    Tone::SelectedHover,
                    if prefs.roomy_lines == roomy {
                        Tone::Accent
                    } else {
                        Tone::Text
                    },
                    6.0,
                )),
        );
    }
    let preview = container(column![
        text(tr("CHAT PREVIEW")).size(11).font(semibold()).style(ui_text(Tone::Secondary)),
        text("Ana Paula · 10:42").size(12).style(ui_text(Tone::Muted)),
        text(tr("Set the text size to make conversations easier to read."))
            .font(crate::typography::for_family(prefs.font_family)).size(u32::from(prefs.message_size)).line_height(prefs.line_height()).style(ui_text(Tone::Text)),
    ].spacing(12)).padding(20).width(Fill).style(ui_surface(Tone::Incoming, true, 8.0));

    column![
        heading(tr("Message text size")),
        font_controls,
        row![text(tr("Size")).size(14), space::horizontal(), text(format!("{} px", prefs.message_size)).size(14)].spacing(12),
        heading(tr("Interface font")), families,
        text(if prefs.font_family == crate::typography::active() { tr("The selected font is in use.") } else { tr("Reopen the app to apply the font across the interface.") }).size(13).style(ui_text(Tone::Secondary)),
        heading(tr("Line spacing")), spacing, preview,
        heading(tr("Interface zoom")),
        slider(80..=200, prefs.interface_zoom, AppMessage::InterfaceZoomChanged).step(5u16).on_release(AppMessage::SavePreferences),
        zoom_presets,
        button(text(tr("Reset reading settings")).size(14)).padding([8, 12]).on_press(AppMessage::ResetReading).style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 4.0)),
    ].spacing(16).into()
}

fn audio_content(state: &TeamsApp) -> Element<'_, AppMessage> {
    let prefs = &state.preferences;
    let mut content = column![heading(tr("Audio"))]
        .spacing(16)
        .push(
            text(tr("Choose a microphone and audio output. Start each test manually."))
                .size(13)
                .style(ui_text(Tone::Secondary)),
        );
    if state.demo_mode {
        content = content.push(
            text(tr("Audio devices are disabled in the demo."))
                .size(13)
                .style(ui_text(Tone::Muted)),
        );
    } else {
        content = content
            .push(audio_picker(
                tr("Microphone"),
                &state.audio_devices.inputs,
                &prefs.audio_input,
                true,
            ))
            .push(audio_picker(
                tr("Audio output"),
                &state.audio_devices.outputs,
                &prefs.audio_output,
                false,
            ));
        if state.audio_devices_loading {
            content = content.push(
                text(tr("Finding devices…"))
                    .size(13)
                    .style(ui_text(Tone::Muted)),
            );
        }
        if let Some(error) = state.audio_devices_error.as_deref() {
            content = content.push(
                text(error)
                    .size(13)
                    .style(crate::text_style(crate::rgb(0xa4262c))),
            );
        }
        content = content.push(
            row![
                button(text(tr("Test audio")).size(14))
                    .height(44)
                    .padding([10, 16])
                    .on_press_maybe(
                        (state.audio_test_busy.is_none() && state.audio_wizard_step.is_none())
                            .then_some(AppMessage::OpenAudioWizard),
                    )
                    .style(ui_button(Tone::Primary, Tone::Primary, Tone::OnPrimary, 6.0)),
                space::horizontal(),
                button(
                    text(if state.audio_devices_loading {
                        tr("Refreshing…")
                    } else {
                        tr("Refresh")
                    })
                    .size(13),
                )
                    .padding([8, 12])
                    .on_press_maybe(
                        (!state.audio_devices_loading).then_some(AppMessage::RefreshAudioDevices),
                    )
                    .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 6.0)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
        if let Some(step) = state.audio_wizard_step {
            let busy = state.audio_test_busy.is_some();
            let close = button(text(tr("Close test")).size(13))
                .height(40)
                .padding([8, 12])
                .on_press(AppMessage::CloseAudioWizard)
                .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 6.0));
            let mut wizard = column![].spacing(12);
            match step {
                crate::AudioWizardStep::Speaker => {
                    let (feedback_label, feedback_message) =
                        if state.audio_wizard_action_attempted {
                            (
                                tr("I did not hear it"),
                                AppMessage::AudioWizardSpeakerFeedback(false),
                            )
                        } else {
                            (tr("Skip"), AppMessage::AudioWizardSkipStep)
                        };
                    wizard = wizard
                        .push(
                            row![
                                column![
                                    text(tr("1 of 3 · Speaker"))
                                        .size(16)
                                        .font(semibold())
                                        .style(ui_text(Tone::Text)),
                                    text(tr("Play a tone and confirm whether you hear it."))
                                        .size(13)
                                        .style(ui_text(Tone::Secondary)),
                                ]
                                .spacing(4),
                                space::horizontal(),
                                close,
                            ]
                            .align_y(Alignment::Center),
                        )
                        .push(
                            row![
                                button(text(
                                    if state.audio_test_busy == Some(crate::AudioTestKind::Speaker) {
                                        tr("Playing…")
                                    } else {
                                        tr("Play tone")
                                    }
                                ))
                                .height(44)
                                .padding([10, 16])
                                .on_press_maybe((!busy).then_some(AppMessage::StartSpeakerTest))
                                .style(ui_button(
                                    Tone::Primary,
                                    Tone::Primary,
                                    Tone::OnPrimary,
                                    6.0,
                                )),
                                button(text(tr("I heard the tone")).size(13))
                                    .height(44)
                                    .padding([10, 14])
                                    .on_press_maybe(state.audio_wizard_action_ready.then_some(
                                        AppMessage::AudioWizardSpeakerFeedback(true),
                                    ))
                                    .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 6.0)),
                                button(text(feedback_label).size(13))
                                    .height(44)
                                    .padding([10, 14])
                                    .on_press_maybe((!busy).then_some(feedback_message))
                                    .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 6.0)),
                            ]
                            .spacing(8)
                            .align_y(Alignment::Center),
                        );
                }
                crate::AudioWizardStep::Microphone => {
                    let (feedback_label, feedback_message) =
                        if state.audio_wizard_action_attempted {
                            (
                                tr("I did not hear it"),
                                AppMessage::AudioWizardMicrophoneFeedback(false),
                            )
                        } else {
                            (tr("Skip"), AppMessage::AudioWizardSkipStep)
                        };
                    wizard = wizard
                        .push(
                            row![
                                column![
                                    text(tr("2 of 3 · Microphone"))
                                        .size(16)
                                        .font(semibold())
                                        .style(ui_text(Tone::Text)),
                                    text(
                                        tr("The app will record one second in memory and play it back. Confirm whether you heard your voice."),
                                    )
                                        .size(13)
                                        .style(ui_text(Tone::Secondary)),
                                ]
                                .spacing(4),
                                space::horizontal(),
                                close,
                            ]
                            .align_y(Alignment::Center),
                        )
                        .push(
                            row![
                                button(text(
                                    if state.audio_test_busy == Some(crate::AudioTestKind::LocalEcho) {
                                        tr("Recording and playing back…")
                                    } else {
                                        tr("Record and play back")
                                    }
                                ))
                                .height(44)
                                .padding([10, 16])
                                .on_press_maybe((!busy).then_some(AppMessage::StartLocalEchoTest))
                                .style(ui_button(
                                    Tone::Primary,
                                    Tone::Primary,
                                    Tone::OnPrimary,
                                    6.0,
                                )),
                                button(text(tr("I heard my voice")).size(13))
                                    .height(44)
                                    .padding([10, 14])
                                    .on_press_maybe(state.audio_wizard_action_ready.then_some(
                                        AppMessage::AudioWizardMicrophoneFeedback(true),
                                    ))
                                    .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 6.0)),
                                button(text(feedback_label).size(13))
                                    .height(44)
                                    .padding([10, 14])
                                    .on_press_maybe((!busy).then_some(feedback_message))
                                    .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 6.0)),
                                button(text(tr("Back")).size(13))
                                    .height(44)
                                    .padding([10, 14])
                                    .on_press_maybe((!busy).then_some(AppMessage::AudioWizardBack))
                                    .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 6.0)),
                            ]
                            .spacing(8)
                            .align_y(Alignment::Center),
                        );
                }
                crate::AudioWizardStep::Summary => {
                    let speaker_result = audio_result_label(state.audio_wizard_speaker_heard);
                    let microphone_result =
                        audio_result_label(state.audio_wizard_microphone_heard);
                    let microphone_level = state
                        .audio_mic_level_dbfs
                        .map(|level| format!("{level:.0} dBFS"))
                        .unwrap_or_else(|| tr("Not measured").into());
                    wizard = wizard
                        .push(
                            row![
                                column![
                                    text(tr("3 of 3 · Results"))
                                        .size(16)
                                        .font(semibold())
                                        .style(ui_text(Tone::Text)),
                                    text(tr("These are the results of the local audio test."))
                                        .size(13)
                                        .style(ui_text(Tone::Secondary)),
                                ]
                                .spacing(4),
                                space::horizontal(),
                                close,
                            ]
                            .align_y(Alignment::Center),
                        )
                        .push(
                            column![
                                audio_result_row(tr("Speaker"), speaker_result),
                                audio_result_row(tr("Voice playback"), microphone_result),
                                audio_result_row(tr("Captured level"), microphone_level),
                            ]
                            .spacing(8),
                        )
                        .push(
                            row![
                                button(text(tr("Test again")).size(13))
                                    .height(44)
                                    .padding([10, 14])
                                    .on_press(AppMessage::AudioWizardRetest)
                                    .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 6.0)),
                                space::horizontal(),
                                button(text(tr("Done")).size(13))
                                    .height(44)
                                    .padding([10, 14])
                                    .on_press(AppMessage::CloseAudioWizard)
                                    .style(ui_button(Tone::Primary, Tone::Primary, Tone::OnPrimary, 6.0)),
                            ]
                            .align_y(Alignment::Center),
                        );
                }
            }
            if !state.audio_test_status.is_empty() {
                wizard = wizard.push(
                    text(state.audio_test_status.as_str())
                        .size(13)
                        .style(ui_text(Tone::Secondary)),
                );
            }
            let wizard_panel = container(wizard)
                .padding(16)
                .width(Fill)
                .style(ui_surface(Tone::Incoming, true, 8.0));
            content = content.push(wizard_panel);
        }
        if !state.audio_test_status.is_empty() {
            if state.audio_wizard_step.is_none() {
                content = content.push(
                    text(state.audio_test_status.as_str())
                        .size(13)
                        .style(ui_text(Tone::Secondary)),
                );
            }
        }
        if let Some(level) = state.audio_mic_level_dbfs {
            content = content.push(
                column![
                    row![
                        text(tr("Average input level"))
                            .size(13)
                            .style(ui_text(Tone::Secondary)),
                        space::horizontal(),
                        text(format!("{level:.0} dBFS"))
                            .size(13)
                            .font(semibold())
                            .style(ui_text(Tone::Accent))
                    ]
                    .align_y(Alignment::Center),
                    progress_bar(0.0..=1.0, ((level + 60.0) / 60.0).clamp(0.0, 1.0)),
                ]
                .spacing(7),
            );
        }
        let echo_note = format!(
            "The test records for {} s after you start it. Audio is kept temporarily in memory for playback.",
            crate::audio::LOCAL_ECHO_DURATION_MS / 1_000
        );
        content = content.push(
            text(echo_note)
                .size(12)
                .style(ui_text(Tone::Muted)),
        );
    }
    content.into()
}

fn audio_result_row<'a>(label: &'a str, result: impl Into<String>) -> Element<'a, AppMessage> {
    row![
        text(label).size(13).style(ui_text(Tone::Secondary)),
        space::horizontal(),
        text(result.into())
            .size(13)
            .font(semibold())
            .style(ui_text(Tone::Accent)),
    ]
    .align_y(Alignment::Center)
    .into()
}

fn audio_result_label(heard: Option<bool>) -> &'static str {
    match heard {
        Some(true) => tr("Heard"),
        Some(false) => tr("Not heard"),
        None => tr("Not tested"),
    }
}

fn heading(label: &str) -> iced::widget::Text<'_> {
    text(label)
        .size(17)
        .font(semibold())
        .style(ui_text(Tone::Text))
}

fn audio_picker<'a>(
    title: &'a str,
    devices: &'a [AudioDevice],
    selected: &'a str,
    input: bool,
) -> Element<'a, AppMessage> {
    let select_default = if input {
        AppMessage::SelectAudioInput(String::new())
    } else {
        AppMessage::SelectAudioOutput(String::new())
    };
    let mut choices = column![
        text(title)
            .size(14)
            .font(semibold())
            .style(ui_text(Tone::Text)),
        button(text(tr("System default")).size(13))
            .width(Fill)
            .padding([8, 10])
            .on_press(select_default)
            .style(ui_button(
                if selected.is_empty() {
                    Tone::Selected
                } else {
                    Tone::Surface
                },
                Tone::SelectedHover,
                if selected.is_empty() {
                    Tone::Accent
                } else {
                    Tone::Text
                },
                6.0,
            )),
    ]
    .spacing(6);
    if selected_audio_device_missing(selected, devices) {
        choices = choices.push(
            text(tr("The saved device is unavailable."))
                .size(12)
                .style(ui_text(Tone::Danger)),
        );
    }
    for device in devices {
        let message = if input {
            AppMessage::SelectAudioInput(device.key.clone())
        } else {
            AppMessage::SelectAudioOutput(device.key.clone())
        };
        let is_selected = selected == device.key;
        let label = if device.is_default {
            format!("{} · System default", device.label)
        } else {
            device.label.clone()
        };
        choices = choices.push(
            button(text(label).size(13))
                .width(Fill)
                .padding([8, 10])
                .on_press(message)
                .style(ui_button(
                    if is_selected {
                        Tone::Selected
                    } else {
                        Tone::Surface
                    },
                    Tone::SelectedHover,
                    if is_selected {
                        Tone::Accent
                    } else {
                        Tone::Text
                    },
                    6.0,
                )),
        );
    }
    container(choices)
        .padding(12)
        .width(Fill)
        .style(ui_surface(Tone::Incoming, false, 8.0))
        .into()
}

fn selected_audio_device_missing(selected: &str, devices: &[AudioDevice]) -> bool {
    !selected.is_empty() && !devices.iter().any(|device| device.key == selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_search_matches_accents_and_multiple_terms_without_hiding_unrelated_pages() {
        assert!(Page::Appearance.matches("APARENCIA"));
        assert!(Page::Accessibility.matches("fonte zoom"));
        assert!(Page::Devices.matches("audio microfone"));
        assert!(!Page::Chats.matches("audio"));
        assert!(!Page::Devices.matches("audio inexistente"));
        assert!(Page::ALL.iter().all(|page| page.matches("  ")));
    }

    #[test]
    fn newline_preference_changes_enter_but_preserves_shift_enter_and_unfocused_editors() {
        use iced::{keyboard::{Key, Modifiers, key::{Code, Named, Physical}}, widget::text_editor::{Binding, KeyPress, Status}};
        let press = |modifiers, status| KeyPress {
            key: Key::Named(Named::Enter), modified_key: Key::Named(Named::Enter),
            physical_key: Physical::Code(Code::Enter), modifiers, text: None, status,
        };
        let focused = Status::Focused { is_hovered: false };
        assert!(matches!(composer_key(press(Modifiers::empty(), focused), true), Some(Binding::Custom(AppMessage::SendMessage))));
        assert!(matches!(composer_key(press(Modifiers::empty(), focused), false), Some(Binding::Enter)));
        assert!(matches!(composer_key(press(Modifiers::SHIFT, focused), false), Some(Binding::Enter)));
        assert!(composer_key(press(Modifiers::empty(), Status::Active), false).is_none());
    }

    #[test]
    fn saved_audio_device_is_missing_only_when_non_default_selection_is_unavailable() {
        let devices = [AudioDevice {
            key: "usb.mic".into(),
            label: "Microfone USB".into(),
            is_default: false,
        }];

        assert!(!selected_audio_device_missing("", &devices));
        assert!(!selected_audio_device_missing("usb.mic", &devices));
        assert!(selected_audio_device_missing("removed.usb.mic", &devices));
        assert!(selected_audio_device_missing("removed.usb.mic", &[]));
    }
}

fn notifications_content(state: &TeamsApp) -> Element<'_, AppMessage> {
    let prefs=state.preferences.alerts;
    let toggle=|label:&'static str,value,on_toggle:fn(bool)->AppMessage| {
        iced::widget::checkbox(value).label(label).on_toggle(on_toggle).size(18).text_size(16)
    };
    let test_button=|label:&'static str,enabled,action|button(text(label).size(14)).padding([10,14])
        .on_press_maybe((enabled && !state.demo_mode).then_some(action))
        .style(ui_button(Tone::Surface,Tone::Hover,Tone::Text,6.0));
    column![
        card(column![heading(tr("Desktop notifications")),
            toggle(tr("Show banners for messages, calls, and meetings"),prefs.banners,AppMessage::AlertBannersChanged),
            toggle(tr("Play a sound for desktop notifications"),prefs.sounds,AppMessage::AlertSoundsChanged),
            text(tr("GNOME manages banners, volume, and Do Not Disturb. Muted chats and messages you are reading stay quiet.")).size(14).style(ui_text(Tone::Secondary)),
            test_button(tr("Test notification"),prefs.banners,AppMessage::TestDesktopNotification),
        ].spacing(18)),
        card(column![heading(tr("Meetings")),
            toggle(tr("Play a sound when joining or leaving a meeting"),prefs.meeting_sounds,AppMessage::MeetingSoundsChanged),
            text(tr("The join sound plays after the connection is confirmed. Sounds follow your system sound theme and Do Not Disturb settings.")).size(14).style(ui_text(Tone::Secondary)),
            row![test_button(tr("Play join sound"),prefs.meeting_sounds,AppMessage::TestMeetingSound(crate::alert_sounds::Cue::Joined)),
                test_button(tr("Play leave sound"),prefs.meeting_sounds,AppMessage::TestMeetingSound(crate::alert_sounds::Cue::Left))].spacing(10),
        ].spacing(18)),
        text(if state.demo_mode {tr("Sound and notification tests are disabled in the demo.")} else {tr("The tests run locally.")}).size(13).style(ui_text(Tone::Muted)),
    ].spacing(20).into()
}
