//! Native pre-entry. It never declares a call connected without a transport.
use crate::i18n::tr;
use crate::{AppMessage, TeamsApp, Tone, semibold, ui_button, ui_surface, ui_text, video};
use iced::widget::{
    button, column, container, image, mouse_area, opaque, pick_list, progress_bar, radio,
    row, slider, space, stack, text, text_input,
};
use iced::{Alignment, Element, Fill};

#[derive(Clone)]
pub struct JoinLink {
    url: String,
    chat: Option<(String, Option<String>)>,
    pub camera: Option<(video::CameraDevice, bool)>,
}

/// Legacy invitation coordinates. These are routing identities, never log them.
pub(crate) struct MeetingCoordinates {
    pub root_message_id: String,
    pub reply_chain_message_id: Option<String>,
    pub tenant_id: Option<String>,
    pub organizer_id: Option<String>,
}

/// Conversation identity resolved from this invitation; never print it in logs.
#[derive(Clone, PartialEq, Eq)]
pub struct ChatTarget(String);
impl ChatTarget {
    pub(crate) fn as_str(&self) -> &str { &self.0 }
    pub(crate) fn draft_key(&self) -> String { format!("meeting:{}", self.0) }
}
impl std::fmt::Debug for ChatTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ChatTarget([REDACTED])") }
}
impl JoinLink {
    fn is_meeting_id_path(path: &str) -> bool {
        let Some(id) = path.strip_prefix("/meet/") else { return false; };
        !id.is_empty()
            && id.len() <= 128
            && !id.contains('/')
            && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.len() > 16 * 1024 {
            return Err(tr("The meeting link is too long.").into());
        }
        let url = url::Url::parse(value.trim())
            .map_err(|_| tr("Paste a valid Teams meeting link.").to_owned())?;
        if url.scheme() != "https"
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !matches!(
                url.host_str(),
                Some("teams.microsoft.com" | "teams.live.com")
            )
        {
            return Err(tr("Use an HTTPS Microsoft Teams meeting invitation.").into());
        }
        let path = url.path();
        if !(path.starts_with("/l/meetup-join/") && path.len() > "/l/meetup-join/".len()
            || Self::is_meeting_id_path(path))
        {
            return Err(tr("The link does not identify a known meeting.").into());
        }
        let link = Self { url: url.to_string(), chat: None, camera: None };
        link.meeting_coordinates()?;
        Ok(link)
    }
    pub fn browser_url(&self) -> &str {
        &self.url
    }
    pub(crate) fn is_meeting_id_link(&self) -> bool {
        url::Url::parse(&self.url).is_ok_and(|url| Self::is_meeting_id_path(url.path()))
    }
    pub(crate) fn thread_id(&self) -> Option<String> {
        if let Some((thread, _)) = &self.chat { return Some(thread.clone()); }
        let url = url::Url::parse(&self.url).ok()?;
        let encoded = url.path().strip_prefix("/l/meetup-join/")?.split('/').next()?;
        let thread = urlencoding::decode(encoded).ok()?;
        (thread.starts_with("19:") && thread.ends_with("@thread.v2")
            && thread.len() <= 2048 && !thread.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '#')))
            .then(|| thread.into_owned())
    }
    pub(crate) fn meeting_coordinates(&self) -> Result<Option<MeetingCoordinates>, String> {
        if self.chat.is_some() || self.thread_id().is_none() { return Ok(None); }
        let invalid = || tr("The invitation is missing valid meeting details. Use the organizer's full link.").to_owned();
        let url = url::Url::parse(&self.url).map_err(|_| invalid())?;
        let path = url.path().strip_prefix("/l/meetup-join/").ok_or_else(invalid)?;
        let mut segments = path.split('/');
        segments.next();
        let encoded = segments.next().ok_or_else(invalid)?;
        if segments.next().is_some() { return Err(invalid()); }
        let decimal = |s: &str| !s.is_empty() && s.len() <= 128 && s.bytes().all(|c| c.is_ascii_digit());
        let root = urlencoding::decode(encoded).map_err(|_| invalid())?.into_owned();
        if !decimal(&root) { return Err(invalid()); }
        let mut coordinates = MeetingCoordinates {
            // The invitation's zero root is not a reply-chain identity. The
            // successful PWA resolution omits that optional field entirely.
            reply_chain_message_id: (root != "0").then(|| root.clone()), root_message_id: root,
            tenant_id: None, organizer_id: None,
        };
        let mut contexts = url.query_pairs().filter(|(key, _)| key == "context");
        if let Some((_, context)) = contexts.next() {
            if contexts.next().is_some() { return Err(invalid()); }
            // query_pairs has decoded once. Never decode the JSON a second time.
            let context: serde_json::Value = serde_json::from_str(&context).map_err(|_| invalid())?;
            let guid = |value: &str| value.len() == 36 && value.bytes().enumerate().all(|(i, c)|
                if [8,13,18,23].contains(&i) { c == b'-' } else { c.is_ascii_hexdigit() });
            let tenant = context.get("Tid").and_then(|v| v.as_str()).filter(|s| guid(s)).ok_or_else(invalid)?;
            let organizer = context.get("Oid").and_then(|v| v.as_str()).filter(|s| guid(s)).ok_or_else(invalid)?;
            coordinates.tenant_id = Some(tenant.to_owned());
            coordinates.organizer_id = Some(organizer.to_owned());
            match context.get("MessageId") {
                None | Some(serde_json::Value::Null) => {},
                Some(serde_json::Value::String(message)) if message.is_empty() => {},
                Some(serde_json::Value::String(message)) if decimal(message) => {
                    coordinates.reply_chain_message_id = (message != "0").then(|| message.clone());
                },
                Some(_) => return Err(invalid()),
            }
        }
        Ok(Some(coordinates))
    }
    pub(crate) fn chat_target(&self) -> Option<ChatTarget> { self.thread_id().map(ChatTarget) }
    /// Join the meeting that already lives on a group thread. A bare chat call
    /// starts a separate empty call; the meetup link with root `0` is the join
    /// the service uses for the meeting in progress.
    pub(crate) fn group_meetup(thread: &str) -> Result<Self, String> {
        if thread.len() > 2048 || !thread.starts_with("19:") || !thread.ends_with("@thread.v2")
            || thread.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '#')) {
            return Err(tr("This chat does not have a supported call target.").into());
        }
        Self::parse(&format!(
            "https://teams.microsoft.com/l/meetup-join/{}/0",
            urlencoding::encode(thread)
        ))
    }
    pub(crate) fn chat(thread: &str, peer: Option<&str>) -> Result<Self, String> {
        if thread.len() > 2048 || !thread.starts_with("19:")
            || !thread.ends_with(if peer.is_some() { "@unq.gbl.spaces" } else { "@thread.v2" })
            || thread.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '#')) {
            return Err(tr("This chat does not have a supported call target.").into());
        }
        let peer = peer.map(|id| crate::avatars::person_id(id)
            .map(|id| format!("8:orgid:{id}"))
            .ok_or_else(|| tr("This contact has no valid calling identity.").to_owned())).transpose()?;
        Ok(Self { url: String::new(), chat: Some((thread.into(), peer)), camera:None })
    }
    pub(crate) fn callee(&self) -> Option<&str> {
        self.chat.as_ref()?.1.as_deref()
    }
    /// Resolve public invitation redirects without account credentials, cookies
    /// or reading a response body. Every hop must remain a valid Teams invite.
    pub(crate) async fn resolve_thread(self) -> Result<Self, String> {
        // Meeting-ID links already carry the meetingUrl (including its opaque
        // passcode query) consumed by MeetingRequest. They do not redirect to
        // the thread-based `/l/meetup-join/` form and need no thread lookup.
        if self.thread_id().is_some() || self.is_meeting_id_link() { return Ok(self); }
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5)).build()
            .map_err(|_| tr("Could not prepare to resolve the invitation.").to_owned())?;
        tokio::time::timeout(std::time::Duration::from_secs(12), async move {
            let mut current = self;
            for _ in 0..5 {
                if current.thread_id().is_some() { return Ok(current); }
                let response = client.get(current.browser_url()).send().await
                    .map_err(|_| tr("Could not resolve the meeting invitation.").to_owned())?;
                if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) { break; }
                let location = response.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok())
                    .ok_or_else(|| tr("The invitation did not return a valid target.").to_owned())?;
                let target = url::Url::parse(current.browser_url()).unwrap().join(location)
                    .map_err(|_| tr("The invitation target is invalid.").to_owned())?;
                let camera=current.camera.take();
                current = Self::parse(target.as_str())?;
                current.camera=camera;
            }
            if current.thread_id().is_some() { Ok(current) }
            else { Err(tr("Could not identify the chat. Use the full /l/meetup-join/ link from the meeting invitation.").into()) }
        }).await.map_err(|_| tr("Resolving the invitation timed out.").to_owned())?
    }
}

pub struct Prejoin {
    pub chat_target: Option<JoinLink>,
    pub call_callbacks: Option<crate::call_context::CallCallbacks>,
    pub id: u64,
    pub subject: String,
    pub input: String,
    pub error: Option<String>,
    pub details: Option<crate::calendar::CalendarEvent>,
    pub metadata_error: Option<String>,
    pub resolving: bool,
    pub resolve_sequence: u64,
    pub editing_link: bool,
    pub microphone_muted: bool,
    pub devices: Vec<video::CameraDevice>,
    pub selected_camera: Option<video::CameraDevice>,
    pub devices_loading: bool,
    pub camera_sequence: u64,
    pub camera_starting: bool,
    pub preview: Option<video::Preview>,
    pub camera_cancel: Option<video::Cancellation>,
    pub frame: Option<image::Handle>,
    pub screen_frame: Option<image::Handle>,
    pub local_frame: Option<image::Handle>,
    pub call_panel: Option<CallPanel>,
    pub participant_search: String,
    /// Page of the side gallery while content is shared, or of a crowded grid.
    pub gallery_page: usize,
    pub connected_at: Option<std::time::Instant>,
    pub chat: MeetingChat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallPanel { Audio, Camera, Participants, Chat }

#[derive(Default)]
pub struct MeetingChat {
    pub target: Option<ChatTarget>,
    pub messages: Vec<crate::graph::GraphMessage>,
    pub draft: String,
    pub draft_revision: u64,
    pub loading: bool,
    pub loaded: bool,
    pub sending: bool,
    pub load_error: Option<String>,
    pub send_error: Option<String>,
    pub sequence: u64,
    pub send_sequence: u64,
    pub send_attempt: Option<crate::chat_state::SendAttempt>,
}
impl MeetingChat {
    /// A retry can reuse the prejoin generation; invalidate all old work while
    /// retaining an unsent draft only for the same conversation identity.
    pub fn end_session(&mut self) {
        self.sequence = self.sequence.wrapping_add(1);
        self.send_sequence = self.send_sequence.wrapping_add(1);
        self.messages.clear();
        self.loading = false;
        self.loaded = false;
        self.sending = false;
        self.load_error = None;
        self.send_error = None;
        self.send_attempt = None;
    }
    pub fn set_target(&mut self, target: Option<ChatTarget>) {
        if self.target != target {
            *self = Self { target, sequence: self.sequence.wrapping_add(1), ..Default::default() };
        }
    }
}
impl Prejoin {
    pub fn new(id: u64, subject: String, input: String) -> Self {
        let editing_link = input.is_empty();
        Self {
            chat_target: None,
            call_callbacks: None,
            id,
            subject,
            input,
            error: None,
            details: None,
            metadata_error: None,
            resolving: false,
            resolve_sequence: 0,
            editing_link,
            microphone_muted: true,
            devices: Vec::new(),
            selected_camera: None,
            devices_loading: true,
            camera_sequence: 0,
            camera_starting: false,
            preview: None,
            camera_cancel: None,
            frame: None,
            screen_frame: None,
            local_frame: None,
            call_panel: None,
            participant_search: String::new(),
            gallery_page: 0,
            connected_at: None,
            chat: MeetingChat::default(),
        }
    }
    pub fn invitation(&self) -> Result<JoinLink, String> {
        self.chat_target.clone().map(Ok).unwrap_or_else(|| JoinLink::parse(&self.input))
    }
    pub fn stop_camera(&mut self) {
        self.camera_sequence = self.camera_sequence.wrapping_add(1);
        self.camera_starting = false;
        if let Some(cancellation) = self.camera_cancel.take() {
            cancellation.cancel();
        }
        if let Some(preview) = self.preview.take() {
            preview.stop();
        }
        self.frame = None;
        self.local_frame = None;
    }

    pub fn prepare_call_callbacks(&mut self, context: Option<&crate::call_context::CallContext>) {
        let key = crate::call_lifecycle::CallKey {
            generation: self.id,
            id: format!("native-meeting-{}", self.id),
        };
        if self
            .call_callbacks
            .as_ref()
            .zip(context)
            .is_some_and(|(callbacks, context)| callbacks.is_current(&key, context))
        {
            return;
        }
        self.call_callbacks = context.and_then(|context| context.prepare_callbacks(key));
    }
    pub fn accepts_camera(&self, id: u64, sequence: u64) -> bool {
        self.id == id && self.camera_sequence == sequence && self.camera_starting
    }
}

impl Drop for Prejoin {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.camera_cancel {
            cancellation.cancel();
        }
        if let Some(preview) = &self.preview {
            preview.stop();
        }
    }
}

pub fn view(state: &TeamsApp) -> Element<'_, AppMessage> {
    let Some(join) = &state.prejoin else {
        return space::vertical().into();
    };
    if let Some(snapshot) = state
        .native_call_snapshot
        .as_ref()
        .filter(|snapshot| !snapshot.phase.terminal())
    {
        let matches = state
            .native_call
            .as_ref()
            .is_some_and(|call| call.key.generation == join.id);
        if matches || state.demo_mode {
            return native_view(state, join, snapshot);
        }
    }

    iced::widget::responsive(move |size| {
        container(prejoin_body(state, size))
            .width(Fill)
            .height(Fill)
            .style(ui_surface(Tone::Canvas, false, 0.0))
            .into()
    })
    .into()
}

pub(crate) fn signaling_failure_code(payload: &str) -> &'static str {
    let value = (payload.len() <= 1024)
        .then(|| serde_json::from_str::<serde_json::Value>(payload).ok())
        .flatten();
    match value
        .as_ref()
        .and_then(|value| value.get("reason"))
        .and_then(|value| value.as_str())
    {
        Some("negotiation") => "negotiation",
        Some("session") => "session",
        Some("socket") => "socket",
        Some("handshake") => "handshake",
        Some("registration") => "registration",
        Some("credential") => "credential",
        Some("metadata") => "metadata",
        Some("metadata_format") => "metadata_format",
        Some("metadata_endpoint") => "metadata_endpoint",
        Some("metadata_expired") => "metadata_expired",
        Some("metadata_address") => "metadata_address",
        Some("metadata_host") => "metadata_host",
        Some("metadata_port") => "metadata_port",
        _ => "connection",
    }
}
fn signaling_message(code: &str) -> &'static str {
    match code {
        "negotiation" => tr("Could not negotiate signaling. Reconnecting…"),
        "session" => tr("Could not prepare the calling channel. Reconnecting…"),
        "socket" | "handshake" => {
            tr("Could not connect the calling channel. Reconnecting…")
        }
        "registration" => tr("Could not register the calling channel. Reconnecting…"),
        "credential" => tr("The session did not provide a signaling credential."),
        "metadata" => tr("The calling channel did not provide the required information."),
        "metadata_format" => tr("The connection returned invalid call metadata (format)."),
        "metadata_endpoint" => {
            tr("The connection did not return a valid calling identity (endpoint).")
        }
        "metadata_expired" => tr("The calling connection expired during setup. Reconnecting…"),
        "metadata_address" => {
            tr("The call callback address has an incompatible format.")
        }
        "metadata_host" => {
            tr("The call callback address does not belong to a supported calling domain.")
        }
        "metadata_port" => tr("The call callback address uses an unsupported port."),
        _ => tr("Signaling was interrupted. Reconnecting…"),
    }
}
fn prejoin_body(state: &TeamsApp, size: iced::Size) -> Element<'_, AppMessage> {
    let join = state.prejoin.as_ref().unwrap();
    let camera_on = join.camera_starting || join.preview.is_some();
    let narrow = size.width < 860.0;
    let preview_height = if narrow || size.height < 640.0 {
        (size.height * 0.32).clamp(160.0, 240.0)
    } else {
        (size.height * 0.42).clamp(220.0, 380.0)
    };
    let preview: Element<'_, AppMessage> = if let Some(frame) = &join.frame {
        crate::video_surface::view(join.id, frame.clone(), true)
    } else {
        container(
            column![
                crate::icon_tone(crate::Icon::Camera, Tone::Muted, 28.0),
                text(if camera_on {
                    tr("Opening camera…")
                } else {
                    tr("Your camera is off")
                })
                .size(15)
                .font(semibold())
                .style(ui_text(Tone::Muted)),
            ]
            .spacing(16)
            .align_x(Alignment::Center),
        )
        .center_x(Fill)
        .center_y(Fill)
        .into()
    };
    let camera = container(column![
        container(preview).width(Fill).height(if narrow { iced::Length::Fixed(preview_height) } else { Fill }),
        crate::divider(),
        row![
            crate::hint(
                button(crate::icon_tone(
                    crate::Icon::Refresh,
                    Tone::Secondary,
                    18.0
                ))
                .on_press_maybe(
                    (!join.devices_loading).then_some(AppMessage::RefreshMeetingCameras)
                )
                .padding(8)
                .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0)),
                tr("Refresh cameras")
            ),
            pick_list(
                join.devices.clone(),
                join.selected_camera.clone(),
                AppMessage::SelectMeetingCamera
            )
            .placeholder(if join.devices_loading {
                tr("Finding cameras…")
            } else {
                tr("No camera")
            })
            .text_size(13)
            .width(Fill),
            button(text(if camera_on { tr("Turn off") } else { tr("Turn on") }).size(13))
                .on_press_maybe(
                    (join.selected_camera.is_some() || camera_on)
                        .then_some(AppMessage::ToggleMeetingCamera)
                )
                .padding([8, 12])
                .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 4.0)),
        ]
        .spacing(8)
        .padding(12)
        .align_y(Alignment::Center),
    ]
    .height(if narrow { iced::Length::Shrink } else { Fill }))
    .width(Fill)
    .height(if narrow { iced::Length::Shrink } else { Fill })
    .style(ui_surface(Tone::Surface, true, 4.0));
    let mut audio = column![
        row![
            crate::icon_tone(crate::Icon::AudioSelected, Tone::Accent, 22.0),
            text(tr("Computer audio")).size(16).font(semibold())
        ]
        .spacing(12)
        .padding(20)
        .align_y(Alignment::Center),
        crate::divider(),
    ]
    .spacing(0);
    for (input, title, devices, selected, icon) in [
        (
            true,
            tr("Microphone"),
            &state.audio_devices.inputs,
            &state.preferences.audio_input,
            crate::Icon::Microphone,
        ),
        (
            false,
            tr("Speaker"),
            &state.audio_devices.outputs,
            &state.preferences.audio_output,
            crate::Icon::Speaker,
        ),
    ] {
        let mut choices: Vec<_> = devices
            .iter()
            .map(|device| AudioChoice {
                key: device.key.clone(),
                label: device.label.clone(),
            })
            .collect();
        choices.insert(
            0,
            AudioChoice {
                key: String::new(),
                label: tr("System default").into(),
            },
        );
        let selection = choices
            .iter()
            .find(|choice| choice.key == *selected)
            .cloned();
        let mut controls = row![
            crate::icon_tone(icon, Tone::Secondary, 22.0),
            column![
                text(title).size(13).style(ui_text(Tone::Secondary)),
                pick_list(choices, selection, move |choice| if input {
                    AppMessage::SelectAudioInput(choice.key)
                } else {
                    AppMessage::SelectAudioOutput(choice.key)
                })
                .text_size(14)
                .width(Fill)
            ]
            .spacing(6)
            .width(Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if input {
            controls = controls.push(
                iced::widget::toggler(!join.microphone_muted)
                    .label(if join.microphone_muted {
                        tr("Off")
                    } else {
                        tr("On")
                    })
                    .text_size(12)
                    .size(18)
                    .on_toggle(|enabled| AppMessage::MeetingMicrophoneMuted(!enabled)),
            );
        }
        audio = audio.push(container(controls).padding([12, 20]));
    }
    audio = audio.push(
        container(
            column![
                text(if join.chat_target.as_ref().is_some_and(|target| target.callee().is_some()) {
                    if join.microphone_muted { tr("Your microphone will be off when you start the call.") }
                    else { tr("Your microphone will turn on when the other person answers.") }
                } else if join.microphone_muted {
                    tr("Your microphone will be off when you join.")
                } else {
                    tr("Your microphone will turn on when the meeting confirms audio.")
                })
                .size(13)
                .style(ui_text(Tone::Secondary)),
                text(if join.chat_target.as_ref().is_some_and(|target| target.callee().is_some()) {
                    tr("The camera will turn on after the other person answers.")
                } else {
                    tr("The camera will turn on after the meeting confirms audio.")
                })
                    .size(12)
                    .style(ui_text(Tone::Muted)),
            ]
            .spacing(8),
        )
        .padding([12, 20]),
    );
    let audio = container(iced::widget::scrollable(audio).height(if narrow { iced::Length::Shrink } else { Fill }))
        .width(Fill)
        .height(if narrow { iced::Length::Shrink } else { Fill })
        .style(ui_surface(Tone::Surface, true, 4.0));
    let content_width = (size.width - 40.0).clamp(280.0, 1080.0);
    let panels: Element<'_, AppMessage> = if narrow {
        column![camera, audio].spacing(16).width(Fill).into()
    } else {
        row![
            container(camera).width(iced::Length::FillPortion(6)).height(Fill),
            container(audio).width(iced::Length::FillPortion(5)).height(Fill)
        ]
        .spacing(16)
        .height(Fill)
        .width(Fill)
        .into()
    };
    let account = container(
        row![
            state.photo_avatar(&state.user_name, Some(&state.user_id), 40.0),
            column![
                text(&state.user_name).size(14).font(semibold()),
                text(&state.user_email)
                    .size(12)
                    .style(ui_text(Tone::Secondary))
            ]
            .spacing(4)
            .width(Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .width(Fill),
    )
    .padding(16)
    .width(Fill)
    .max_width(360)
    .style(ui_surface(Tone::Surface, false, 4.0));
    let mut body = column![
        row![space::horizontal(), account].width(Fill).align_y(Alignment::Center),
        column![
            iced::widget::svg(iced::widget::svg::Handle::from_memory(
                include_bytes!("../assets/teams-linux-rs.svg").to_vec()
            ))
            .width(40)
            .height(40),
            text(&join.subject)
                .size(if narrow { 20 } else { 24 })
                .font(semibold())
                .align_x(Alignment::Center)
                .width(Fill),
        ]
        .spacing(12)
        .align_x(Alignment::Center)
        .width(Fill),
        panels,
    ]
    .spacing(16)
    .width(Fill)
    .height(if narrow { iced::Length::Shrink } else { Fill })
    .align_x(Alignment::Center);
    if join.editing_link {
        body = body.push(
            text_input(tr("Paste the meeting link"), &join.input)
                .id("meeting-link")
                .on_input(AppMessage::MeetingLinkChanged)
                .size(14),
        );
    }
    let mut footer = column![].spacing(12).width(Fill);
    if let Some(error) = join.error.as_ref().or(state.audio_devices_error.as_ref()) {
        footer = footer.push(text(error).size(14).style(ui_text(Tone::Danger)));
    }
    if join.error.is_none() && state.native_call_snapshot.as_ref().is_some_and(|snapshot| snapshot.phase == crate::native_call::Phase::Ended) {
        footer = footer.push(text(if join.chat_target.as_ref().is_some_and(|target| target.callee().is_some()) {
            tr("The call ended. You can call again.")
        } else {
            tr("The meeting ended. You can try joining again.")
        })
            .size(14).style(ui_text(Tone::Secondary)));
    }
    if join.details.as_ref().is_some_and(|event| event.cancelled) {
        footer = footer.push(
            text(tr("Meeting canceled"))
                .size(14)
                .style(ui_text(Tone::Danger)),
        );
    }
    let ready = state.demo_mode || state.call_hub.context().is_some();
    if !ready {
        footer = footer.push(
            row![
                text(
                    state
                        .trouter_failure
                        .map(signaling_message)
                        .unwrap_or(tr("Connecting to the calling channel…"))
                )
                .size(13)
                .style(ui_text(Tone::Secondary))
                .width(Fill),
                button(text(tr("Reconnect")).size(13))
                    .padding([8, 12])
                    .on_press(AppMessage::ReconnectMeetingSignaling)
                    .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 4.0)),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        );
    }
    let link_action: Element<'_, AppMessage> = if join.chat_target.is_none() {
        button(text(tr("Change meeting link")).size(13))
            .on_press(AppMessage::EditMeetingLink)
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Accent, 4.0)).into()
    } else if join.chat_target.as_ref().is_some_and(|target| target.callee().is_some()) {
        text(tr("Call")).size(13).style(ui_text(Tone::Secondary)).into()
    } else { text(tr("Meeting")).size(13).style(ui_text(Tone::Secondary)).into() };
    footer = footer.push(
        row![
            link_action,
            space::horizontal(),
            button(text(tr("Cancel")).size(14))
                .padding([10, 18])
                .on_press(AppMessage::CloseMeeting)
                .style(ui_button(Tone::Surface, Tone::Hover, Tone::Text, 4.0)),
            button(text(if join.chat_target.as_ref().is_some_and(|t| t.callee().is_some()) { tr("Call now") } else { tr("Join now") }).size(14).font(semibold()))
                .padding([10, 18])
                .on_press_maybe(
                    (join.invitation().is_ok() && !join.devices_loading && !join.camera_starting
                        && state.native_call.is_none()
                        && ready
                        && !join.details.as_ref().is_some_and(|event| event.cancelled))
                    .then_some(AppMessage::JoinNativeMeeting)
                )
                .style(ui_button(
                    Tone::Primary,
                    Tone::SelectedHover,
                    Tone::OnPrimary,
                    4.0
                )),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    );
    let middle: Element<'_, AppMessage> = if narrow {
        iced::widget::scrollable(body).height(Fill).width(Fill).into()
    } else {
        body.into()
    };
    let sheet = column![
        middle,
        crate::divider(),
        container(footer).padding([12, 0]).width(Fill),
    ]
    .spacing(0)
    .width(iced::Length::Fixed(content_width))
    .height(Fill)
    .align_x(Alignment::Center);
    container(sheet)
        .padding(20)
        .center_x(Fill)
        .width(Fill)
        .height(Fill)
        .style(ui_surface(Tone::Canvas, false, 0.0))
        .into()
}
#[derive(Clone, PartialEq, Eq)]
struct AudioChoice {
    key: String,
    label: String,
}
impl std::fmt::Display for AudioChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn group_meetup_joins_the_thread_meeting_instead_of_an_empty_chat_call() {
        let link = super::JoinLink::group_meetup("19:warroom_fixture@thread.v2").unwrap();
        assert!(link.browser_url().starts_with("https://teams.microsoft.com/l/meetup-join/"));
        let coordinates = link.meeting_coordinates().unwrap().unwrap();
        assert_eq!(coordinates.root_message_id, "0");
        assert!(coordinates.reply_chain_message_id.is_none());
        assert!(super::JoinLink::chat("19:warroom_fixture@thread.v2", None).unwrap().browser_url().is_empty());
        assert!(super::JoinLink::group_meetup("19:peer@unq.gbl.spaces").is_err());
    }
    #[test]
    fn chat_calls_require_one_valid_peer_and_do_not_become_browser_urls() {
        let peer = "aaaaaaaa-1111-2222-3333-444444444444";
        let target = super::JoinLink::chat("19:fixture@unq.gbl.spaces",Some(peer)).unwrap();
        assert_eq!(target.callee(),Some("8:orgid:aaaaaaaa-1111-2222-3333-444444444444"));
        assert!(target.browser_url().is_empty());
        assert_eq!(target.thread_id().as_deref(),Some("19:fixture@unq.gbl.spaces"));
        assert!(super::JoinLink::chat("19:fixture@unq.gbl.spaces",Some("unknown")).is_err());
        assert!(super::JoinLink::chat("https://evil.example",Some(peer)).is_err());
        assert!(super::JoinLink::chat("19:fixture@thread.v2",Some(peer)).is_err());
        assert!(super::JoinLink::chat("19:fixture@thread.v2",None).is_ok());
    }
    #[test]
    fn invitation_thread_decodes_once_and_rejects_encoded_separators() {
        let valid = super::JoinLink::parse("https://teams.microsoft.com/l/meetup-join/19%3Afixture%40thread.v2/0").unwrap();
        assert_eq!(valid.thread_id().as_deref(), Some("19:fixture@thread.v2"));
        for segment in ["19%3Afixture%2Fother%40thread.v2", "19%253Afixture%2540thread.v2", "19%3Afixture%0A%40thread.v2"] {
            let link = super::JoinLink::parse(&format!("https://teams.microsoft.com/l/meetup-join/{segment}/0")).unwrap();
            assert!(link.thread_id().is_none());
        }
    }
    use super::*;
    #[test]
    fn invitation_parser_preserves_encoded_context_but_rejects_other_targets() {
        let link = "https://teams.microsoft.com/l/meetup-join/19%3ameeting_fixture%40thread.v2/0?context=%7B%22Tid%22%3A%2211111111-1111-4111-8111-111111111111%22%2C%22Oid%22%3A%2222222222-2222-4222-8222-222222222222%22%7D";
        assert_eq!(JoinLink::parse(link).unwrap().browser_url(), link);
        assert!(JoinLink::parse("https://teams.microsoft.com/meet/123456?p=fixture").is_ok());
        for bad in [
            "http://teams.microsoft.com/meet/1",
            "https://teams.microsoft.com.evil.test/meet/1",
            "https://teams.microsoft.com@evil.test/meet/1",
            "https://teams.microsoft.com/",
            "https://teams.microsoft.com/meet/",
            "https://teams.microsoft.com:444/meet/1",
            "file:///dev/video0",
        ] {
            assert!(JoinLink::parse(bad).is_err());
        }
    }
    #[test]
    fn meeting_coordinates_preserve_root_and_context_without_becoming_group_call() {
        let context = serde_json::json!({"Tid":"11111111-1111-4111-8111-111111111111", "Oid":"22222222-2222-4222-8222-222222222222", "MessageId":"123456789"}).to_string();
        let base = "https://teams.microsoft.com/l/meetup-join/19%3Ameeting_fixture%40thread.v2/0";
        let link = JoinLink::parse(&format!("{base}?context={}",urlencoding::encode(&context))).unwrap();
        let coordinates = link.meeting_coordinates().unwrap().unwrap();
        assert_eq!(coordinates.root_message_id,"0");
        assert_eq!(coordinates.reply_chain_message_id.as_deref(),Some("123456789"));
        assert_eq!(coordinates.tenant_id.as_deref(),Some("11111111-1111-4111-8111-111111111111"));
        assert_eq!(coordinates.organizer_id.as_deref(),Some("22222222-2222-4222-8222-222222222222"));
        assert!(JoinLink::chat("19:fixture@thread.v2",None).unwrap().meeting_coordinates().unwrap().is_none());
        for message in [serde_json::Value::Null, serde_json::json!("")] {
            let mut empty: serde_json::Value = serde_json::from_str(&context).unwrap();
            empty["MessageId"] = message;
            let link = JoinLink::parse(&format!("{base}?context={}",urlencoding::encode(&empty.to_string()))).unwrap();
            assert_eq!(link.meeting_coordinates().unwrap().unwrap().reply_chain_message_id, None);
        }
        for context in ["{}", "[]", "not-json", "%7B%7D", r#"{"Tid":"invalid","Oid":"invalid"}"#] {
            assert!(JoinLink::parse(&format!("{base}?context={}",urlencoding::encode(context))).is_err());
        }
        assert!(JoinLink::parse(&format!("{base}?context={0}&context={0}",urlencoding::encode(&context))).is_err());
        assert!(JoinLink::parse(&base.replace("/0","/not-a-message")).is_err());
    }
    #[test]
    fn switching_off_invalidates_inflight_camera_start() {
        let mut join = Prejoin::new(4, "Test".into(), String::new());
        join.camera_starting = true;
        let cancellation = video::Cancellation::default();
        join.camera_cancel = Some(cancellation.clone());
        assert!(join.accepts_camera(4, 0));
        join.stop_camera();
        assert!(cancellation.is_cancelled());
        assert!(!join.accepts_camera(4, 0));
        join.camera_starting = true;
        assert!(join.accepts_camera(4, 1));
        assert!(!join.accepts_camera(5, 1));
    }
}

fn admission_candidate(person: &crate::call_roster::RosterFace) -> bool {
    !person.is_self
        && (person.is_waiting || (!person.admission_known && !person.has_endpoints))
}

fn admission_available(
    state: &TeamsApp,
    snapshot: &crate::native_call::Snapshot,
    person: &crate::call_roster::RosterFace,
) -> bool {
    admission_candidate(person)
        && !person.participant_id.is_empty()
        && !state.demo_mode
        && snapshot.phase == crate::native_call::Phase::Connected
        && !matches!(
            snapshot.admission_state,
            crate::native_call::AdmissionState::Sending
                | crate::native_call::AdmissionState::WaitingForConfirmation
        )
}

fn stage_label(person: &crate::call_roster::RosterFace, own_name: &str) -> String {
    if person.is_self {
        let name = own_name.trim();
        if name.is_empty() { tr("You").to_owned() } else { format!("{name} (you)") }
    } else if person.label.trim().is_empty() {
        tr("Participant").to_owned()
    } else {
        person.label.clone()
    }
}

fn stage_muted(person: &crate::call_roster::RosterFace, self_muted: bool) -> bool {
    if person.is_self { self_muted } else { person.muted == Some(true) }
}

/// People in the call come first, then you, then anyone still in the lobby.
fn stage_order(roster: &[crate::call_roster::RosterFace]) -> Vec<usize> {
    let mut present = Vec::new();
    let mut own = Vec::new();
    let mut waiting = Vec::new();
    for (index, person) in roster.iter().enumerate() {
        if person.is_self {
            own.push(index);
        } else if person.is_waiting {
            waiting.push(index);
        } else {
            present.push(index);
        }
    }
    present.extend(own);
    present.extend(waiting);
    present
}

/// One remote camera can be shown. Two cameras share one decoded stream, so
/// neither card claims the picture.
fn remote_camera_index(roster: &[crate::call_roster::RosterFace]) -> Option<usize> {
    let mut found = None;
    for (index, person) in roster.iter().enumerate() {
        if person.is_self || !person.video_on {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(index);
    }
    found
}

fn gallery_columns(count: usize) -> usize {
    match count {
        0 | 1 => 1,
        2..=4 => 2,
        _ => 3,
    }
}

fn tile_avatar_size(count: usize) -> f32 {
    match count {
        0 | 1 => 148.0,
        2 => 104.0,
        3 | 4 => 96.0,
        _ => 88.0,
    }
}

fn tile_name(person: &crate::call_roster::RosterFace, own_name: &str) -> String {
    if person.is_self {
        let name = own_name.trim();
        if name.is_empty() { tr("You").to_owned() } else { name.to_owned() }
    } else if person.label.trim().is_empty() {
        tr("Participant").to_owned()
    } else {
        person.label.clone()
    }
}

fn face_avatar<'a>(
    state: &'a TeamsApp,
    person: &'a crate::call_roster::RosterFace,
    size: f32,
) -> Element<'a, AppMessage> {
    let name = stage_label(person, &state.user_name);
    if person.is_self {
        state.person_avatar(&state.user_name, Some(&state.user_id), size)
    } else if person.participant_id.starts_with("8:") {
        state.person_avatar(&name, Some(person.participant_id.as_str()), size)
    } else {
        crate::avatar(&name, size)
    }
}

fn person_card<'a>(
    state: &'a TeamsApp,
    snapshot: &'a crate::native_call::Snapshot,
    person: &'a crate::call_roster::RosterFace,
    avatar_size: f32,
    width: f32,
    height: f32,
    camera: Option<(u64, image::Handle, bool)>,
) -> Element<'a, AppMessage> {
    let name = tile_name(person, &state.user_name);
    let mut title = row![
        text(crate::preview_line(&name, 28))
            .size(13)
            .style(ui_text(Tone::Text)),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    if stage_muted(person, snapshot.muted) {
        title = title.push(crate::icon_tone(crate::Icon::MicrophoneOff, Tone::Text, 14.0));
    }
    let picture: Element<'a, AppMessage> = if let Some((stream, frame, mirror)) = camera {
        container(crate::video_surface::view(stream, frame, mirror))
            .width(Fill)
            .height(Fill)
            .into()
    } else {
        face_avatar(state, person, avatar_size).into()
    };
    let face = container(picture)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill);
    let mut caption = column![title].spacing(6).align_x(Alignment::Start);
    if person.is_waiting && !person.is_self {
        caption = caption.push(text(tr("In the lobby")).size(11).style(ui_text(Tone::Secondary)));
    }
    if admission_candidate(person) {
        caption = caption.push(
            button(text(tr("Admit")).size(12))
                .padding([4, 8])
                .on_press_maybe(
                    admission_available(state, snapshot, person)
                        .then(|| AppMessage::AdmitMeetingParticipant(person.participant_id.clone())),
                )
                .style(ui_button(Tone::Surface, Tone::Hover, Tone::Accent, 4.0)),
        );
    }
    let label = container(caption)
        .width(Fill)
        .height(Fill)
        .align_left(Fill)
        .align_bottom(Fill)
        .padding(iced::Padding { left: 12.0, bottom: 10.0, right: 12.0, ..Default::default() });
    container(stack![face, label])
        .width(width)
        .height(height)
        .clip(true)
        .style(ui_surface(Tone::Surface, false, 8.0))
        .into()
}

const RAIL_COLUMNS: usize = 2;

struct ContentRail {
    page: usize,
    pages: usize,
    visible: Vec<usize>,
    self_index: Option<usize>,
}

/// People already in the call, excluding the lobby. You stay out of the
/// paged grid so the self tile can stay pinned under the page control.
fn content_rail(roster: &[crate::call_roster::RosterFace], requested_page: usize, rows: usize) -> ContentRail {
    let mut others = Vec::new();
    let mut self_index = None;
    for (index, person) in roster.iter().enumerate() {
        if person.is_waiting {
            continue;
        }
        if person.is_self {
            self_index = Some(index);
        } else {
            others.push(index);
        }
    }
    let page_size = rows.max(1) * RAIL_COLUMNS;
    let pages = others.len().div_ceil(page_size).max(1);
    let page = requested_page.min(pages - 1);
    let start = page * page_size;
    let visible = others.into_iter().skip(start).take(page_size).collect();
    ContentRail { page, pages, visible, self_index }
}

/// Rows that fit beside shared content after the pinned self tile and pager.
fn rail_rows(height: f32) -> usize {
    let available = (height - 168.0).max(72.0);
    ((available / 86.0).floor() as usize).clamp(2, 7)
}

fn stage_is_sharing(join: &Prejoin, snapshot: &crate::native_call::Snapshot) -> bool {
    join.screen_frame.is_some()
        || snapshot.screen_sharing
        || snapshot.roster.iter().any(|person| !person.is_self && !person.is_waiting && person.screen_on)
}

fn in_call_count(roster: &[crate::call_roster::RosterFace]) -> usize {
    roster.iter().filter(|person| !person.is_waiting).count()
}

/// A crowded call without shared content pages a 4-column grid instead of
/// clipping the roster at twelve cards.
fn crowded_gallery_plan(count: usize, requested_page: usize, height: f32) -> (usize, usize, usize, usize) {
    let columns = 4;
    let rows = ((height - 56.0) / 168.0).floor().clamp(2.0, 4.0) as usize;
    let page_size = columns * rows.max(1);
    let pages = count.div_ceil(page_size).max(1);
    let page = requested_page.min(pages - 1);
    (columns, rows, page, pages)
}

fn gallery_pager<'a>(page: usize, pages: usize) -> Element<'a, AppMessage> {
    if pages <= 1 {
        return space::vertical().height(0).into();
    }
    let previous = (page > 0).then_some(AppMessage::MeetingGalleryStep(-1));
    let next = (page + 1 < pages).then_some(AppMessage::MeetingGalleryStep(1));
    let step = button(crate::icon_tone(crate::Icon::ChevronLeft, Tone::Secondary, 14.0))
        .padding(4)
        .on_press_maybe(previous)
        .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0));
    let forward = button(crate::icon_tone(crate::Icon::ChevronRight, Tone::Secondary, 14.0))
        .padding(4)
        .on_press_maybe(next)
        .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0));
    container(
        row![
            step,
            text(format!("{}/{}", page + 1, pages)).size(13).style(ui_text(Tone::Secondary)),
            forward,
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .width(Fill)
    .align_x(Alignment::Center)
    .into()
}

fn rail_camera(
    join: &Prejoin,
    snapshot: &crate::native_call::Snapshot,
    person: &crate::call_roster::RosterFace,
    index: usize,
) -> Option<(u64, image::Handle, bool)> {
    let camera_owner = remote_camera_index(&snapshot.roster);
    if camera_owner == Some(index) {
        join.frame.clone().map(|frame| (join.id, frame, false))
    } else if person.is_self && snapshot.camera_enabled {
        join.local_frame.clone().map(|frame| (join.id.wrapping_add(1 << 63), frame, true))
    } else {
        None
    }
}

fn rail_tile<'a>(
    own_name: &'a str,
    snapshot: &'a crate::native_call::Snapshot,
    join: &'a Prejoin,
    index: usize,
    width: f32,
    height: f32,
) -> Element<'a, AppMessage> {
    let person = &snapshot.roster[index];
    let avatar = (height * 0.52).clamp(36.0, 84.0);
    let name = tile_name(person, own_name);
    let picture: Element<'a, AppMessage> = if let Some((stream, frame, mirror)) = rail_camera(join, snapshot, person, index) {
        container(crate::video_surface::view(stream, frame, mirror))
            .width(Fill)
            .height(Fill)
            .into()
    } else {
        container(crate::avatar(&name, avatar))
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .into()
    };
    let body = if stage_muted(person, snapshot.muted) {
        let badge = container(crate::icon_tone(crate::Icon::MicrophoneOff, Tone::Text, 12.0))
            .padding(4)
            .style(ui_surface(Tone::Canvas, false, 4.0));
        let overlay = container(badge)
            .width(Fill)
            .height(Fill)
            .align_left(Fill)
            .align_bottom(Fill)
            .padding(6);
        stack![picture, overlay].into()
    } else {
        picture
    };
    container(body)
        .width(width)
        .height(height)
        .clip(true)
        .style(ui_surface(Tone::Surface, false, 8.0))
        .into()
}

fn shared_content<'a>(
    state: &'a TeamsApp,
    join: &'a Prejoin,
    snapshot: &'a crate::native_call::Snapshot,
) -> Element<'a, AppMessage> {
    let picture: Element<'a, AppMessage> = if let Some(screen) = join.screen_frame.clone() {
        container(crate::video_surface::view(join.id ^ 0x5c4e, screen, false))
            .width(Fill)
            .height(Fill)
            .into()
    } else {
        let message = if snapshot.screen_sharing {
            tr("You are sharing your screen")
        } else {
            tr("Waiting for shared content")
        };
        container(text(message).size(16).style(ui_text(Tone::Secondary)))
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .into()
    };
    let sharer = snapshot.roster.iter().find(|person| !person.is_self && person.screen_on);
    let caption = if snapshot.screen_sharing && sharer.is_none() {
        let name = state.user_name.trim();
        if name.is_empty() { tr("You").to_owned() } else { format!("{name} (you)") }
    } else {
        sharer.map(|person| tile_name(person, &state.user_name)).unwrap_or_else(|| tr("Shared content").to_owned())
    };
    let mut title = row![text(crate::preview_line(&caption, 48)).size(13).style(ui_text(Tone::Text))]
        .spacing(6)
        .align_y(Alignment::Center);
    let muted = sharer.map(|person| stage_muted(person, snapshot.muted)).unwrap_or(snapshot.screen_sharing && snapshot.muted);
    if muted {
        title = title.push(crate::icon_tone(crate::Icon::MicrophoneOff, Tone::Text, 14.0));
    }
    let label = container(title).padding([6, 10]).style(ui_surface(Tone::Canvas, false, 4.0));
    let overlay = container(label)
        .width(Fill)
        .height(Fill)
        .align_left(Fill)
        .align_bottom(Fill)
        .padding(12);
    container(stack![picture, overlay]).width(Fill).height(Fill).padding(8).into()
}

fn shared_stage<'a>(
    state: &'a TeamsApp,
    join: &'a Prejoin,
    snapshot: &'a crate::native_call::Snapshot,
    usable_width: f32,
    stage_h: f32,
) -> Element<'a, AppMessage> {
    let rows = rail_rows(stage_h);
    let rail = content_rail(&snapshot.roster, join.gallery_page, rows);
    let rail_width = (usable_width * 0.36).clamp(200.0, 460.0);
    let gap = 6.0;
    let tile_w = ((rail_width - 16.0 - gap) / RAIL_COLUMNS as f32).max(96.0);
    let pager_h = if rail.pages > 1 { 32.0 } else { 0.0 };
    let self_h = if rail.self_index.is_some() { 108.0 } else { 0.0 };
    let grid_h = (stage_h - self_h - pager_h - 24.0).max(120.0);
    let tile_h = ((grid_h - gap * rows.saturating_sub(1) as f32) / rows as f32).clamp(56.0, 132.0);
    let mut grid = column![].spacing(gap);
    for chunk in rail.visible.chunks(RAIL_COLUMNS) {
        let mut line = row![].spacing(gap);
        for index in chunk {
            line = line.push(rail_tile(&state.user_name, snapshot, join, *index, tile_w, tile_h));
        }
        grid = grid.push(line);
    }
    let mut side = column![container(grid).width(Fill)].spacing(6).width(Fill).height(Fill);
    side = side.push(space::vertical().height(Fill));
    if rail.pages > 1 {
        side = side.push(gallery_pager(rail.page, rail.pages));
    }
    if let Some(index) = rail.self_index {
        side = side.push(rail_tile(&state.user_name, snapshot, join, index, rail_width - 16.0, self_h));
    }
    let side = container(side).width(rail_width).height(Fill).padding([4, 8]);
    row![
        container(shared_content(state, join, snapshot)).width(Fill).height(Fill),
        side,
    ]
    .height(Fill)
    .into()
}

fn crowded_gallery<'a>(
    state: &'a TeamsApp,
    join: &'a Prejoin,
    snapshot: &'a crate::native_call::Snapshot,
    usable_width: f32,
    stage_h: f32,
) -> Element<'a, AppMessage> {
    let order = stage_order(&snapshot.roster);
    let present: Vec<usize> = order.into_iter().filter(|index| !snapshot.roster[*index].is_waiting).collect();
    let (cols, rows, page, pages) = crowded_gallery_plan(present.len(), join.gallery_page, stage_h);
    let start = page * cols * rows;
    let shown: Vec<usize> = present.into_iter().skip(start).take(cols * rows).collect();
    let gap = 8.0;
    let tile_w = ((usable_width - gap * (cols.saturating_sub(1) as f32)) / cols as f32).max(140.0);
    let tile_h = ((stage_h - 48.0 - gap * rows.saturating_sub(1) as f32) / rows as f32).clamp(120.0, tile_w * 0.72);
    let avatar = tile_avatar_size(shown.len());
    let camera_owner = remote_camera_index(&snapshot.roster);
    let mut grid = column![].spacing(gap);
    for chunk in shown.chunks(cols) {
        let mut line = row![].spacing(gap).align_y(Alignment::Center);
        for index in chunk {
            let person = &snapshot.roster[*index];
            let camera = if camera_owner == Some(*index) {
                join.frame.clone().map(|frame| (join.id, frame, false))
            } else if person.is_self && snapshot.camera_enabled {
                join.local_frame.clone().map(|frame| (join.id.wrapping_add(1 << 63), frame, true))
            } else {
                None
            };
            line = line.push(person_card(state, snapshot, person, avatar, tile_w, tile_h, camera));
        }
        grid = grid.push(line);
    }
    container(column![grid, gallery_pager(page, pages)].spacing(8).align_x(Alignment::Center))
        .padding([8, 12])
        .width(Fill)
        .into()
}

fn people_gallery<'a>(
    state: &'a TeamsApp,
    join: &'a Prejoin,
    snapshot: &'a crate::native_call::Snapshot,
    usable_width: f32,
    usable_height: f32,
) -> Element<'a, AppMessage> {
    let order = stage_order(&snapshot.roster);
    let shown: Vec<usize> = order.into_iter().take(12).collect();
    let cols = gallery_columns(shown.len()).max(1);
    let gap = 8.0;
    let tile_w = ((usable_width - gap * (cols.saturating_sub(1) as f32)) / cols as f32).max(160.0);
    let rows = shown.len().div_ceil(cols).max(1);
    let tile_h = if rows <= 2 {
        ((usable_height - gap * rows.saturating_sub(1) as f32) / rows as f32).clamp(150.0, tile_w * 0.72)
    } else {
        ((usable_height - gap) / 2.15).clamp(150.0, tile_w * 0.72)
    };
    let avatar = tile_avatar_size(shown.len());
    let camera_owner = remote_camera_index(&snapshot.roster);
    let mut grid = column![].spacing(gap);
    for chunk in shown.chunks(cols) {
        let mut line = row![].spacing(gap).align_y(Alignment::Center);
        for index in chunk {
            let person = &snapshot.roster[*index];
            let camera = if camera_owner == Some(*index) {
                join.frame.clone().map(|frame| (join.id, frame, false))
            } else if person.is_self && snapshot.camera_enabled {
                join.local_frame.clone().map(|frame| (join.id.wrapping_add(1 << 63), frame, true))
            } else {
                None
            };
            line = line.push(person_card(
                state,
                snapshot,
                person,
                avatar,
                tile_w,
                tile_h,
                camera,
            ));
        }
        grid = grid.push(line);
    }
    container(grid).padding([8, 12]).width(Fill).into()
}

fn people_strip<'a>(
    state: &'a TeamsApp,
    snapshot: &'a crate::native_call::Snapshot,
) -> Element<'a, AppMessage> {
    let order = stage_order(&snapshot.roster);
    let extra = order.len().saturating_sub(8);
    let mut line = row![].spacing(8).align_y(Alignment::Center);
    for index in order.into_iter().take(8) {
        let person = &snapshot.roster[index];
        let name = stage_label(person, &state.user_name);
        let mut card = row![
            face_avatar(state, person, 36.0),
            text(crate::preview_line(&name, 16)).size(13).style(ui_text(Tone::Text)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if stage_muted(person, snapshot.muted) {
            card = card.push(crate::icon_tone(crate::Icon::MicrophoneOff, Tone::Secondary, 14.0));
        }
        line = line.push(
            container(card)
                .padding([8, 10])
                .style(ui_surface(Tone::Surface, false, 8.0)),
        );
    }
    if extra > 0 {
        line = line.push(
            text(format!("+{extra}"))
                .size(13)
                .style(ui_text(Tone::Secondary)),
        );
    }
    iced::widget::scrollable(container(line).padding([8, 16]))
        .direction(iced::widget::scrollable::Direction::Horizontal(
            iced::widget::scrollable::Scrollbar::new().width(4).scroller_width(3),
        ))
        .width(Fill)
        .into()
}

fn microphone_icon(level: u8, muted: bool) -> Element<'static, AppMessage> {
    if muted {
        return crate::icon_tone(crate::Icon::MicrophoneOff, Tone::Secondary, 24.0);
    }
    use iced::widget::svg;
    static LEVELS: std::sync::OnceLock<[svg::Handle; 11]> = std::sync::OnceLock::new();
    let handles = LEVELS.get_or_init(|| std::array::from_fn(|step| {
        let height = 8.0 * step as f32 / 10.0;
        let base = include_str!("../assets/icons/microphone.svg");
        let fill = format!("<rect x=\"10\" y=\"{}\" width=\"4\" height=\"{}\"/>", 14.0 - height, height);
        svg::Handle::from_memory(base.replace("</svg>", &format!("{fill}</svg>")).into_bytes())
    }));
    let index = usize::from((level.min(100) + 5) / 10);
    svg(handles[index].clone()).width(24).height(24)
        .style(|theme, _| svg::Style { color: Some(crate::appearance::tone(theme, Tone::Accent)) })
        .into()
}

fn is_direct_call(join: &Prejoin) -> bool {
    join.chat_target.as_ref().is_some_and(|target| target.callee().is_some())
}

fn leave_action(phase: crate::native_call::Phase, direct_call: bool) -> (&'static str, &'static str, AppMessage) {
    if matches!(
        phase,
        crate::native_call::Phase::Ending
            | crate::native_call::Phase::Ended
            | crate::native_call::Phase::Failed
    ) {
        (tr("Close"), if direct_call { tr("Close the call window") } else { tr("Close the meeting window") }, AppMessage::CloseMeeting)
    } else if phase == crate::native_call::Phase::Connected {
        if direct_call { (tr("Hang up"), tr("End the call"), AppMessage::LeaveNativeMeeting) }
        else { (tr("Leave"), tr("Leave the meeting"), AppMessage::LeaveNativeMeeting) }
    } else if direct_call {
        (tr("Cancel"), tr("Cancel the call"), AppMessage::LeaveNativeMeeting)
    } else {
        (tr("Cancel"), tr("Cancel joining the meeting"), AppMessage::LeaveNativeMeeting)
    }
}

fn screen_control<'a>(
    snapshot: &'a crate::native_call::Snapshot,
    connected: bool,
    compact: bool,
) -> Element<'a, AppMessage> {
    call_control(
        crate::icon_tone(
            crate::Icon::External,
            if snapshot.screen_sharing { Tone::Accent } else { Tone::Secondary },
            24.0,
        ),
        tr("Screen"),
        if snapshot.screen_sharing { tr("Stop sharing") } else { tr("Share screen") },
        connected.then_some(AppMessage::ToggleScreenShare),
        compact,
    )
}

fn control_glyph<'a>(icon: Element<'a, AppMessage>) -> Element<'a, AppMessage> {
    container(icon)
        .width(32)
        .height(32)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .into()
}

fn call_control<'a>(
    icon: Element<'a, AppMessage>,
    label: &'static str,
    description: &'static str,
    action: Option<AppMessage>,
    compact: bool,
) -> Element<'a, AppMessage> {
    let glyph = control_glyph(icon);
    let body: Element<'a, AppMessage> = if compact {
        glyph
    } else {
        column![glyph, text(label).size(12).style(ui_text(Tone::Text))]
            .spacing(2)
            .align_x(Alignment::Center)
            .into()
    };
    let control = button(body)
        .padding(if compact {
            iced::Padding::from(6)
        } else {
            iced::Padding { top: 0.0, right: 10.0, bottom: 0.0, left: 10.0 }
        })
        .on_press_maybe(action)
        .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 5.0));
    crate::hint(control, description)
}

fn people_control<'a>(open: bool, count: usize, compact: bool) -> Element<'a, AppMessage> {
    let icon = crate::icon_tone(
        crate::Icon::GroupMembers,
        if open { Tone::Accent } else { Tone::Secondary },
        24.0,
    );
    let glyph = if count == 0 {
        control_glyph(icon)
    } else {
        let label = if count > 99 { "99+".to_owned() } else { count.to_string() };
        let badge = container(text(label).size(10).style(ui_text(Tone::OnPrimary)))
            .padding([0.0, 4.0])
            .style(ui_surface(Tone::Danger, false, 8.0));
        stack![
            control_glyph(icon),
            container(badge).width(Fill).height(Fill).align_right(Fill).align_top(Fill),
        ]
        .into()
    };
    let body: Element<'a, AppMessage> = if compact {
        glyph
    } else {
        column![glyph, text(tr("People")).size(12).style(ui_text(Tone::Text))]
            .spacing(2)
            .align_x(Alignment::Center)
            .into()
    };
    let control = button(body)
        .padding(if compact { iced::Padding::from(6) } else { iced::Padding { top: 0.0, right: 10.0, bottom: 0.0, left: 10.0 } })
        .on_press(AppMessage::ToggleCallPanel(CallPanel::Participants))
        .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 5.0));
    crate::hint(control, tr("Show participants"))
}

fn media_control<'a>(
    icon: Element<'a, AppMessage>,
    label: &'static str,
    description: &'static str,
    action: Option<AppMessage>,
    panel: CallPanel,
    open: bool,
    compact: bool,
) -> Element<'a, AppMessage> {
    let toggle = crate::hint(
        button(control_glyph(icon))
            .width(32)
            .height(32)
            .padding(0)
            .on_press_maybe(action.clone())
            .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 5.0)),
        description,
    );
    let chevron = crate::hint(
        button(crate::icon_tone(
            crate::Icon::ChevronDown,
            if open { Tone::Accent } else { Tone::Secondary },
            12.0,
        ))
        .width(16)
        .height(32)
        .padding(0)
        .on_press(AppMessage::ToggleCallPanel(panel))
        .style(ui_button(
            if open { Tone::Selected } else { Tone::Transparent },
            Tone::Hover,
            Tone::Text,
            4.0,
        )),
        if panel == CallPanel::Audio { tr("Audio options") } else { tr("Camera options") },
    );
    let icons = row![toggle, chevron].spacing(0).align_y(Alignment::Center);
    if compact {
        return icons.into();
    }
    let caption = button(text(label).size(12).style(ui_text(Tone::Text)))
        .padding(0)
        .on_press_maybe(action)
        .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Text, 4.0));
    column![icons, caption]
        .spacing(2)
        .align_x(Alignment::Center)
        .into()
}

fn elapsed_label(start: Option<std::time::Instant>) -> String {
    let seconds = start.map(|at| at.elapsed().as_secs()).unwrap_or(0);
    if seconds >= 3600 { format!("{:02}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60) }
    else { format!("{:02}:{:02}", seconds / 60, seconds % 60) }
}

fn audio_devices<'a>(state: &'a TeamsApp, input: bool) -> Element<'a, AppMessage> {
    let (devices, chosen) = if input {
        (&state.audio_devices.inputs, &state.preferences.audio_input)
    } else {
        (&state.audio_devices.outputs, &state.preferences.audio_output)
    };
    let mut options = Vec::with_capacity(devices.len() + 1);
    options.push((String::new(), tr("System default").to_owned()));
    options.extend(devices.iter().map(|device| (device.key.clone(), device.label.clone())));
    let selected = options.iter().position(|(key, _)| key == chosen);
    let mut list = column![].spacing(7);
    for (index, (key, label)) in options.into_iter().enumerate() {
        list = list.push(radio(label, index, selected, move |_| {
            if input { AppMessage::SelectAudioInput(key) } else { AppMessage::SelectAudioOutput(key) }
        }).size(18).text_size(14).spacing(9));
    }
    if selected.is_none() {
        list = list.push(text(tr("The selected device is unavailable.")).size(12).style(ui_text(Tone::Danger)));
    }
    list.into()
}

fn audio_panel<'a>(state: &'a TeamsApp, snapshot: &'a crate::native_call::Snapshot, height: f32, width: f32) -> Element<'a, AppMessage> {
    let content = column![
        row![text(tr("Audio")).size(16).font(semibold()).style(ui_text(Tone::Text)).width(Fill),
            button(text(if state.audio_devices_loading { tr("Refreshing…") } else { tr("Refresh") }).size(12))
                .on_press_maybe((!state.audio_devices_loading && !state.demo_mode).then_some(AppMessage::RefreshAudioDevices))
                .style(ui_button(Tone::Transparent, Tone::Hover, Tone::Accent, 4.0))].align_y(Alignment::Center),
        text(tr("Device choices apply to the next call.")).size(12).style(ui_text(Tone::Secondary)),
        crate::divider(),
        text(tr("Speaker")).size(13).font(semibold()).style(ui_text(Tone::Text)),
        audio_devices(state, false),
        row![crate::icon_tone(crate::Icon::Speaker,Tone::Secondary,18.0),
            slider(0..=100,snapshot.speaker_volume,AppMessage::NativeCallVolume).width(Fill),
            text(format!("{}%",snapshot.speaker_volume)).size(12).style(ui_text(Tone::Secondary))].spacing(8).align_y(Alignment::Center),
        crate::divider(),
        text(tr("Microphone")).size(13).font(semibold()).style(ui_text(Tone::Text)),
        audio_devices(state, true),
        row![microphone_icon(snapshot.microphone_level, snapshot.muted),
            container(progress_bar(0.0..=100.0, f32::from(snapshot.microphone_level)).girth(8)).width(Fill),
        ].spacing(10).align_y(Alignment::Center),
        text(if snapshot.muted {tr("Microphone off")} else {tr("Input level")}).size(12).style(ui_text(Tone::Secondary)),
    ].spacing(12).padding(16);
    let mut content = content;
    if let Some(error) = &state.audio_devices_error {
        content = content.push(text(error.as_str()).size(12).style(ui_text(Tone::Danger)));
    }
    container(iced::widget::scrollable(content).height(Fill)).width(width).height(height)
        .style(ui_surface(Tone::Surface, true, 5.0)).into()
}

fn camera_notice(snapshot: &crate::native_call::Snapshot) -> Option<&'static str> {
    if snapshot.camera_capture_failed {
        Some(tr("Could not open the selected camera. Audio is still available."))
    } else if let Some(failure) = snapshot.camera_negotiation_failure {
        Some(failure.message())
    } else if snapshot.camera_enabled && !snapshot.camera_capture_ready {
        Some(tr("Starting the camera…"))
    } else if snapshot.camera_enabled && !snapshot.camera_sending {
        Some(tr("Your preview is ready. Connecting video to the call…"))
    } else {
        None
    }
}

fn camera_panel<'a>(join: &'a Prejoin, snapshot: &'a crate::native_call::Snapshot, height: f32, width: f32) -> Element<'a, AppMessage> {
    let preview: Element<'a, AppMessage> = if snapshot.camera_enabled {
        if let Some(frame) = &join.local_frame {
            container(crate::video_surface::view(join.id.wrapping_add(1u64 << 63), frame.clone(), true))
                .width(Fill).height(170).into()
        } else {
            container(text(if snapshot.camera_capture_failed { tr("Preview unavailable") } else { tr("Preparing preview…") }).size(13).style(ui_text(Tone::Secondary))).width(Fill).height(170).center_x(Fill).center_y(Fill).style(ui_surface(Tone::Canvas,false,4.0)).into()
        }
    } else {
        container(column![crate::icon_tone(crate::Icon::CameraOff,Tone::Secondary,30.0),
            text(tr("Camera off")).size(13).style(ui_text(Tone::Secondary))].spacing(8).align_x(Alignment::Center))
            .width(Fill).height(170).center_x(Fill).center_y(Fill).style(ui_surface(Tone::Canvas,false,4.0)).into()
    };
    let label = join.selected_camera.as_ref().map(|device| device.label.as_str()).unwrap_or(tr("No camera selected"));
    let content = column![
        preview,
        text(tr("Camera")).size(13).font(semibold()).style(ui_text(Tone::Text)),
        text(label).size(14).style(ui_text(Tone::Text)),
        text(tr("Choose your camera before joining the meeting.")).size(12).style(ui_text(Tone::Secondary)),
        text(camera_notice(snapshot).unwrap_or("")).size(12).style(ui_text(Tone::Secondary)),
    ].spacing(10).padding(16);
    container(iced::widget::scrollable(content).height(Fill)).width(width).height(height)
        .style(ui_surface(Tone::Surface, true, 5.0)).into()
}

fn participant_matches(person: &crate::call_roster::RosterFace, query: &str, own_name: &str) -> bool {
    query.is_empty() || person.label.to_lowercase().contains(query)
        || (person.is_self && (tr("you").contains(query) || own_name.to_lowercase().contains(query)))
}

fn participants_panel<'a>(state: &'a TeamsApp, join: &'a Prejoin, snapshot: &'a crate::native_call::Snapshot, width: f32) -> Element<'a, AppMessage> {
    let query = join.participant_search.trim().to_lowercase();
    let mut people = column![].spacing(8);
    let mut matches = 0;
    let waiting_count = snapshot.roster.iter().filter(|person| person.is_waiting && !person.is_self).count();
    let unknown_admission_count = snapshot.roster.iter()
        .filter(|person| !person.is_self && !person.admission_known && !person.has_endpoints).count();
    for person in snapshot.roster.iter().filter(|person| participant_matches(person, &query, &state.user_name)) {
        matches += 1;
        let waiting = person.is_waiting && !person.is_self;
        let admission_candidate = admission_candidate(person);
        let name = if person.is_self { format!("{} (you)", state.user_name) }
            else if person.label.trim().is_empty() { tr("Participant").to_owned() } else { person.label.clone() };
        let mut badges = Vec::new();
        if waiting { badges.push(tr("Waiting in the lobby")); }
        else if admission_candidate { badges.push(tr("No media. Check whether they are in the lobby")); }
        if person.muted == Some(true) { badges.push(tr("microphone off")); }
        if person.video_on { badges.push(tr("camera on")); }
        if person.screen_on { badges.push(tr("sharing screen")); }
        let avatar = if person.is_self { state.person_avatar(&state.user_name,Some(&state.user_id),36.0) }
            else { crate::avatar(&name,36.0) };
        let mut details = column![crate::hint(text(crate::preview_line(&name, 30)).size(14).style(ui_text(Tone::Text)), name)].spacing(2);
        if !badges.is_empty() { details = details.push(text(badges.join(" · ")).size(12).style(ui_text(Tone::Secondary))); }
        let can_admit = admission_available(state, snapshot, person);
        let admit = button(text(tr("Admit")).size(12)).padding([6,9])
            .on_press_maybe(can_admit.then(|| AppMessage::AdmitMeetingParticipant(person.participant_id.clone())))
            .style(ui_button(Tone::Surface,Tone::Hover,Tone::Accent,4.0));
        let mut entry = row![avatar,details].spacing(10).align_y(Alignment::Center).width(Fill);
        if admission_candidate {
            entry = entry.push(space::horizontal()).push(admit);
        }
        people = people.push(entry);
    }
    if snapshot.roster.is_empty() {
        people = people.push(text(tr("Waiting for the participant list…")).size(13).style(ui_text(Tone::Secondary)));
    } else if matches == 0 {
        people = people.push(text(tr("No participants found.")).size(13).style(ui_text(Tone::Secondary)));
    }
    let can_copy = join.invitation().is_ok_and(|link| !link.browser_url().is_empty());
    let heading = row![text(tr("Participants")).size(17).font(semibold()).style(ui_text(Tone::Text)).width(Fill),
        crate::hint(button(text("×").size(22)).padding([3,8]).on_press(AppMessage::CloseCallPanel)
            .style(ui_button(Tone::Transparent,Tone::Hover,Tone::Secondary,4.0)), tr("Close participants"))].align_y(Alignment::Center);
    container(column![
        container(heading).padding([12,16]),crate::divider(),
        container(column![
            text_input(tr("Type a name"), &join.participant_search).id("call-participant-search")
                .on_input(AppMessage::MeetingParticipantSearch).padding([9,10]).size(14),
            button(text(tr("Copy invite")).size(13)).width(Fill).padding(9)
                .on_press_maybe(can_copy.then_some(AppMessage::CopyMeetingInvite))
                .style(|theme, status| {
                    let mut style = ui_button(Tone::Surface,Tone::Hover,Tone::Text,4.0)(theme,status);
                    style.border = iced::Border::default().rounded(4).width(1).color(crate::appearance::tone(theme,Tone::Border));
                    style
                }),
            text(format!("In this meeting ({})", snapshot.roster.len())).size(12).style(ui_text(Tone::Secondary)),
            text(if waiting_count > 0 { format!("{waiting_count} waiting in the lobby") }
                else if unknown_admission_count > 0 {
                    format!("{unknown_admission_count} participant(s) without media. Check whether they are waiting in the lobby.")
                } else { tr("Teams has reported nobody waiting yet.").to_owned() })
                .size(12).style(ui_text(Tone::Secondary)),
            match snapshot.admission_state {
                crate::native_call::AdmissionState::Idle => text("").size(1),
                crate::native_call::AdmissionState::Sending => text(tr("Sending request to Teams…")).size(12).style(ui_text(Tone::Secondary)),
                crate::native_call::AdmissionState::WaitingForConfirmation => text(tr("Request sent. Waiting for Teams to confirm…")).size(12).style(ui_text(Tone::Secondary)),
                crate::native_call::AdmissionState::Admitted => text(tr("Teams confirmed admission.")).size(12).style(ui_text(Tone::Accent)),
                crate::native_call::AdmissionState::Failed => text(tr("Could not admit the participant. Check the participant and try again.")).size(12).style(ui_text(Tone::Danger)),
            },
            iced::widget::scrollable(people).height(Fill),
        ].spacing(14).height(Fill)).padding(16).height(Fill),
    ].height(Fill)).width(width).height(Fill).style(ui_surface(Tone::Surface,true,0.0)).into()
}

fn meeting_chat_panel<'a>(state: &'a TeamsApp, join: &'a Prejoin, snapshot: &'a crate::native_call::Snapshot, width: f32) -> Element<'a, AppMessage> {
    let chat = &join.chat;
    let mut history = column![].spacing(12);
    for message in &chat.messages {
        let body = if message.is_deleted { tr("Deleted message").to_owned() }
            else if message.body.trim().is_empty() && !message.images.is_empty() { tr("Image").to_owned() }
            else if message.body.trim().is_empty() && !message.files.is_empty() { tr("File attached").to_owned() }
            else { message.body.clone() };
        let mut line = column![
            text(if message.is_mine { tr("You") } else if message.author.trim().is_empty() { tr("Participant") } else { message.author.as_str() })
                .size(12).font(semibold()).style(ui_text(Tone::Secondary)),
            text(body).size(14).style(ui_text(Tone::Text)),
        ].spacing(4);
        if !message.time.is_empty() { line = line.push(text(message.time.as_str()).size(11).style(ui_text(Tone::Muted))); }
        let bubble = container(line).padding(10).max_width(280)
            .style(ui_surface(if message.is_mine { Tone::Outgoing } else { Tone::Incoming },false,6.0));
        history = history.push(container(bubble).width(Fill).align_x(if message.is_mine { Alignment::End } else { Alignment::Start }));
    }
    if chat.messages.is_empty() {
        history = history.push(text(if chat.loading { tr("Loading chat…") } else if chat.loaded { tr("No messages in this chat.") } else { tr("Waiting for meeting chat…") })
            .size(13).style(ui_text(Tone::Secondary)));
    }
    let can_edit = (state.drafts_loaded || state.demo_mode)
        && snapshot.phase == crate::native_call::Phase::Connected
        && snapshot.chat_target.as_ref() == chat.target.as_ref() && chat.target.is_some();
    let can_send = can_edit
        && chat.loaded && !chat.sending && !chat.draft.trim().is_empty();
    let mut feedback = column![].spacing(3);
    if let Some(error) = &chat.load_error { feedback = feedback.push(text(error.as_str()).size(12).style(ui_text(Tone::Danger))); }
    if let Some(error) = &chat.send_error { feedback = feedback.push(text(error.as_str()).size(12).style(ui_text(Tone::Danger))); }
    let header = row![text(tr("Meeting chat")).size(17).font(semibold()).style(ui_text(Tone::Text)).width(Fill),
        crate::hint(button(text("×").size(22)).padding([3,8]).on_press(AppMessage::CloseCallPanel)
            .style(ui_button(Tone::Transparent,Tone::Hover,Tone::Secondary,4.0)), tr("Close chat"))].align_y(Alignment::Center);
    container(column![
        container(header).padding([12,16]),crate::divider(),
        container(iced::widget::scrollable(history).anchor_bottom().height(Fill)).height(Fill).padding(14),
        container(column![feedback,
            text_input(tr("Type a message"), &chat.draft).on_input_maybe(can_edit.then_some(AppMessage::MeetingChatDraftChanged))
                .on_submit_maybe(can_send.then_some(AppMessage::MeetingChatSend)).padding(10).size(14),
            row![space::horizontal().width(Fill),
                button(text(if chat.sending { tr("Sending…") } else { tr("Send") }).size(13)).padding([8,14])
                    .on_press_maybe(can_send.then_some(AppMessage::MeetingChatSend))
                    .style(ui_button(Tone::Primary,Tone::SelectedHover,Tone::OnPrimary,4.0))]
        ].spacing(8)).padding([10,14]),
    ].height(Fill)).width(width).height(Fill).style(ui_surface(Tone::Surface,true,0.0)).into()
}

fn native_view<'a>(
    state: &'a TeamsApp,
    join: &'a Prejoin,
    snapshot: &'a crate::native_call::Snapshot,
) -> Element<'a, AppMessage> {
    iced::widget::responsive(move |size| native_view_at(state, join, snapshot, size)).into()
}

fn native_view_at<'a>(
    state: &'a TeamsApp,
    join: &'a Prejoin,
    snapshot: &'a crate::native_call::Snapshot,
    size: iced::Size,
) -> Element<'a, AppMessage> {
    let connected = snapshot.phase == crate::native_call::Phase::Connected;
    let direct = is_direct_call(join);
    let compact = size.width < 980.0;
    let microphone_action = snapshot.microphone_action();
    let mic = media_control(
        microphone_icon(snapshot.microphone_level, snapshot.muted),
        tr("Microphone"),
        if !connected && direct { tr("The microphone turns on when the other person answers") }
        else if !connected { tr("The microphone turns on when the meeting confirms audio") }
        else if snapshot.muted { tr("Unmute microphone") } else { tr("Mute microphone") },
        microphone_action.map(AppMessage::NativeCallMute),
        CallPanel::Audio,
        join.call_panel == Some(CallPanel::Audio),
        compact,
    );
    let camera_action = connected.then_some(AppMessage::ToggleNativeCamera);
    let camera = media_control(
        crate::icon_tone(
            if snapshot.camera_enabled { crate::Icon::Camera } else { crate::Icon::CameraOff },
            if snapshot.camera_enabled { Tone::Accent } else { Tone::Secondary }, 24.0,
        ),
        tr("Camera"),
        if !connected && direct { tr("The camera turns on when the other person answers") }
        else if !connected { tr("The camera turns on when the meeting confirms audio") }
        else if snapshot.camera_enabled { tr("Turn off camera") } else { tr("Turn on camera") },
        camera_action,
        CallPanel::Camera,
        join.call_panel == Some(CallPanel::Camera),
        compact,
    );
    let people = people_control(
        join.call_panel == Some(CallPanel::Participants),
        in_call_count(&snapshot.roster),
        compact,
    );
    let chat = call_control(crate::icon_tone(crate::Icon::Chat,
            if join.call_panel == Some(CallPanel::Chat) { Tone::Accent } else { Tone::Secondary }, 24.0),
        "Chat", if snapshot.chat_target.is_none() { tr("Chat unavailable for this invitation") } else if direct { tr("Open call chat") } else { tr("Open meeting chat") },
        (connected && snapshot.chat_target.is_some()).then_some(AppMessage::ToggleCallPanel(CallPanel::Chat)),
        compact);
    let (leave_label, leave_hint, leave_message) = leave_action(snapshot.phase, direct);
    let leave = call_control(
        crate::icon_tone(crate::Icon::Calls, Tone::Danger, 24.0),
        leave_label,
        leave_hint,
        Some(leave_message),
        compact,
    );
    let mut heading = row![
        text(if connected { elapsed_label(join.connected_at) } else { snapshot.phase.status(direct).to_owned() })
            .size(13).style(ui_text(Tone::Secondary)),
    ].spacing(8).align_y(Alignment::Center);
    if size.width >= 860.0 && !connected {
        let chars = ((size.width - 640.0) / 14.0).clamp(8.0, 36.0) as usize;
        heading = heading.push(text(crate::preview_line(&join.subject, chars)).size(12).font(semibold()).width(Fill)
            .wrapping(iced::widget::text::Wrapping::None).style(ui_text(Tone::Text)));
    }
    let toolbar = container(row![
        container(heading).width(Fill),
        row![mic, camera, screen_control(snapshot, connected, compact), chat, people].spacing(4).align_y(Alignment::Center),
        container(leave).width(Fill).align_x(iced::alignment::Horizontal::Right),
    ].align_y(Alignment::Center).padding([8, 16]))
        .width(Fill).style(ui_surface(Tone::Surface, true, 0.0));

    let mut notices = column![].spacing(4).align_x(Alignment::Center);
    if snapshot.command_failed {
        notices = notices.push(text(tr("Could not confirm the microphone change. It is still muted.")).size(13).style(ui_text(Tone::Danger)));
    } else if snapshot.mute_pending && !snapshot.requested_muted {
        notices = notices.push(text(tr("Unmuting microphone…")).size(13).style(ui_text(Tone::Secondary)));
    }
    if let Some(message) = camera_notice(snapshot) {
        let failed = snapshot.camera_capture_failed || snapshot.camera_negotiation_failure.is_some();
        let notice = text(message).size(13).style(ui_text(if failed { Tone::Danger } else { Tone::Secondary }));
        if failed && connected {
            notices = notices.push(row![notice, button(text(tr("Try again")).size(12))
                .on_press(AppMessage::RetryNativeCamera)].spacing(8).align_y(Alignment::Center));
        } else {
            notices = notices.push(notice);
        }
    }

    let has_remote = snapshot.roster.iter().any(|person| !person.is_self);
    let sharing = stage_is_sharing(join, snapshot);
    let crowded = !sharing && in_call_count(&snapshot.roster) > 12;
    let people_width = (size.width * 0.34).clamp(240.0, 320.0);
    let chat_width = (size.width * 0.36).clamp(260.0, 360.0);
    let docked = match join.call_panel {
        Some(CallPanel::Participants) => people_width,
        Some(CallPanel::Chat) => chat_width,
        _ => 0.0,
    };
    let usable = (size.width - docked - 32.0).max(220.0);
    let stage_h = (size.height - 78.0).max(220.0);
    let camera_on_card = has_remote && remote_camera_index(&snapshot.roster).is_some() && join.frame.is_some();
    let stage: Element<'a, AppMessage> = if sharing {
        shared_stage(state, join, snapshot, usable, stage_h)
    } else if crowded {
        iced::widget::scrollable(
            container(crowded_gallery(state, join, snapshot, usable, stage_h)).width(Fill),
        )
        .width(Fill)
        .height(Fill)
        .into()
    } else if camera_on_card {
        iced::widget::scrollable(
            container(people_gallery(state, join, snapshot, usable, stage_h))
                .width(Fill),
        )
        .width(Fill)
        .height(Fill)
        .into()
    } else if let Some((frame, local)) = join.frame.as_ref().map(|frame| (frame, false))
        .or_else(|| join.local_frame.as_ref().filter(|_| snapshot.camera_enabled).map(|frame| (frame, true))) {
        let video = container(crate::video_surface::view(join.id, frame.clone(), local))
            .width(Fill).height(Fill).padding(8);
        if has_remote {
            column![video, people_strip(state, snapshot)].height(Fill).into()
        } else {
            video.into()
        }
    } else if has_remote {
        iced::widget::scrollable(
            container(people_gallery(state, join, snapshot, usable, stage_h))
                .width(Fill),
        )
        .width(Fill)
        .height(Fill)
        .into()
    } else {
        let status = if connected && !direct && !snapshot.roster.is_empty() {
            tr("Waiting for other participants to join…")
        } else {
            snapshot.phase.status(direct)
        };
        let avatar = (size.height * 0.22).clamp(72.0, 148.0);
        let center = column![
            state.person_avatar(&state.user_name, Some(&state.user_id), avatar),
            text(status).size(if size.width < 720.0 { 18 } else { 22 }).font(semibold()).style(ui_text(Tone::Text)),
        ].spacing(28).align_x(Alignment::Center);
        container(center).center_x(Fill).center_y(Fill).padding(24).into()
    };
    let mut stage_row = row![container(stage).width(Fill).height(Fill)].height(Fill);
    if join.call_panel == Some(CallPanel::Participants) {
        stage_row = stage_row.push(participants_panel(state, join, snapshot, people_width));
    } else if join.call_panel == Some(CallPanel::Chat) {
        stage_row = stage_row.push(meeting_chat_panel(state, join, snapshot, chat_width));
    }
    let base: Element<'a, AppMessage> = container(column![toolbar, container(notices).width(Fill).center_x(Fill), stage_row])
        .width(Fill).height(Fill).style(ui_surface(Tone::Canvas, false, 0.0)).into();
    let Some(panel @ (CallPanel::Audio | CallPanel::Camera)) = join.call_panel else { return base; };
    let desired_height = if panel == CallPanel::Audio {
        285.0 + 25.0 * (state.audio_devices.inputs.len() + state.audio_devices.outputs.len() + 2) as f32
    } else { 330.0 };
    let panel_height = desired_height.min((size.height - 78.0).max(180.0)).min(570.0);
    let flyout = (size.width - 24.0).clamp(240.0, 360.0);
    let panel_view = if panel == CallPanel::Audio { audio_panel(state, snapshot, panel_height, flyout) }
        else { camera_panel(join, snapshot, panel_height, flyout) };
    let x = ((size.width - flyout) / 2.0).max(12.0);
    let outside = mouse_area(container(space::vertical()).width(Fill).height(Fill)).on_press(AppMessage::CloseCallPanel);
    let positioned = container(opaque(panel_view)).width(Fill).height(Fill).align_left(Fill).align_top(Fill)
        .padding(iced::Padding { top: 66.0, left: x, ..Default::default() });
    stack![base, outside, positioned].width(Fill).height(Fill).into()
}

#[cfg(test)]
mod signaling_feedback_tests {
    use super::*;
    #[test]
    fn finished_call_button_closes_the_window_instead_of_staying_on_ending() {
        let (label, hint, message) = leave_action(crate::native_call::Phase::Ended, false);
        assert_eq!(label, "Close");
        assert_eq!(hint, "Close the meeting window");
        assert!(matches!(message, AppMessage::CloseMeeting));
        let (_, _, ending) = leave_action(crate::native_call::Phase::Ending, false);
        assert!(matches!(ending, AppMessage::CloseMeeting));
        let (label, _, message) = leave_action(crate::native_call::Phase::Connected, false);
        assert_eq!(label, "Leave");
        assert!(matches!(message, AppMessage::LeaveNativeMeeting));
        let (label, hint, _) = leave_action(crate::native_call::Phase::Connected, true);
        assert_eq!(label, "Hang up");
        assert_eq!(hint, "End the call");
    }

    #[test]
    fn diagnostic_feedback_accepts_only_known_codes_without_echoing_server_data() {
        assert_eq!(
            signaling_failure_code(r#"{"reason":"registration"}"#),
            "registration"
        );
        assert_eq!(
            signaling_failure_code(r#"{"reason":"metadata"}"#),
            "metadata"
        );
        for payload in [
            r#"{"reason":"https://secret.invalid/?token=private"}"#,
            "not json",
            &"x".repeat(1025),
        ] {
            let code = signaling_failure_code(payload);
            assert_eq!(code, "connection");
            assert!(!signaling_message(code).contains("private"));
        }
    }
    #[test]
    fn participant_search_is_local_and_recognizes_self() {
        let person = crate::call_roster::RosterFace {
            participant_id: "8:orgid:ana".into(),
            label: "Ana Paula".into(), video_on: false, screen_on: false,
            muted: Some(true), is_self: false, is_waiting: false, admission_known: true, has_endpoints: true,
        };
        assert!(participant_matches(&person, "ana", "Alex"));
        assert!(!participant_matches(&person, "bruno", "Alex"));
        let own = crate::call_roster::RosterFace { is_self: true, label: String::new(), ..person };
        assert!(participant_matches(&own, "alex", "Alex Silva"));
        assert!(participant_matches(&own, "you", "Alex Silva"));
        assert!(!participant_matches(&own, "ana", "Alex Silva"));
    }

    fn face(label: &str, is_self: bool, waiting: bool, muted: Option<bool>) -> crate::call_roster::RosterFace {
        crate::call_roster::RosterFace {
            participant_id: format!("8:orgid:{label}"),
            label: label.into(),
            video_on: false,
            screen_on: false,
            muted,
            is_self,
            is_waiting: waiting,
            admission_known: true,
            has_endpoints: !waiting,
        }
    }

    #[test]
    fn voice_stage_shows_each_person_before_you_and_keeps_the_lobby_visible() {
        let roster = vec![
            face("Alex Silva", true, false, Some(true)),
            face("Ana Paula", false, true, None),
            face("Robson", false, false, Some(false)),
        ];
        let order = stage_order(&roster);
        assert_eq!(
            order.iter().map(|index| stage_label(&roster[*index], "Alex Silva")).collect::<Vec<_>>(),
            vec!["Robson".to_owned(), "Alex Silva (you)".to_owned(), "Ana Paula".to_owned()],
        );
        assert!(stage_muted(&roster[order[0]], true) == false);
        assert!(stage_muted(&roster[order[1]], true));
        assert_eq!(gallery_columns(order.len()), 2);
        assert_eq!(gallery_columns(1), 1);
        assert_eq!(gallery_columns(6), 3);
        let mut crowd = Vec::new();
        crowd.push(face("Alex Silva", true, false, Some(true)));
        for index in 0..98 {
            let mut person = face(&format!("Pessoa {index}"), false, false, Some(true));
            if index == 3 { person.screen_on = true; }
            if index == 90 { person.is_waiting = true; }
            crowd.push(person);
        }
        let rail = content_rail(&crowd, 0, 7);
        assert_eq!(rail.pages, 7);
        assert_eq!(rail.visible.len(), 14);
        assert_eq!(rail.self_index, Some(0));
        assert!(rail.visible.iter().all(|index| *index != 0 && !crowd[*index].is_waiting));
        let last = content_rail(&crowd, 99, 7);
        assert_eq!(last.page, 6);
        assert_eq!(last.visible.len(), 13);
        assert_eq!(rail_rows(900.0), 7);
        assert!(stage_is_sharing(
            &Prejoin::new(1, "Reunião".into(), String::new()),
            &crate::native_call::Snapshot { roster: crowd.clone(), ..Default::default() },
        ));
        let (columns, rows, page, pages) = crowded_gallery_plan(20, 0, 760.0);
        assert_eq!((columns, rows), (4, 4));
        assert_eq!(page, 0);
        assert!(pages >= 2);
        assert_eq!(remote_camera_index(&roster), None);
        let mut with_camera = roster.clone();
        with_camera[2].video_on = true;
        assert_eq!(remote_camera_index(&with_camera), Some(2));
        with_camera[1].video_on = true;
        assert_eq!(remote_camera_index(&with_camera), None);
        assert_eq!(tile_avatar_size(2), 104.0);

        let mut state = TeamsApp::from_client_id(String::new());
        state.user_name = "Alex Silva".into();
        state.prejoin = Some(Prejoin::new(7, "Ligação".into(), String::new()));
        state.native_call_snapshot = Some(crate::native_call::Snapshot {
            phase: crate::native_call::Phase::Connected,
            muted: true,
            roster,
            ..Default::default()
        });
        {
            let join = state.prejoin.as_ref().unwrap();
            let snapshot = state.native_call_snapshot.as_ref().unwrap();
            drop(native_view_at(&state, join, snapshot, iced::Size::new(1040.0, 680.0)));
            drop(native_view_at(&state, join, snapshot, iced::Size::new(720.0, 480.0)));
        }
        drop(prejoin_body(&state, iced::Size::new(720.0, 480.0)));
        drop(prejoin_body(&state, iced::Size::new(1280.0, 800.0)));
        state.native_call_snapshot.as_mut().unwrap().roster[2].video_on = true;
        state.native_call_snapshot.as_mut().unwrap().roster[2].screen_on = true;
        state.prejoin.as_mut().unwrap().frame = Some(iced::widget::image::Handle::from_rgba(
            2,
            2,
            vec![9, 8, 7, 255, 6, 5, 4, 255, 3, 2, 1, 255, 0, 0, 0, 255],
        ));
        state.prejoin.as_mut().unwrap().call_panel = Some(CallPanel::Participants);
        {
            let join = state.prejoin.as_ref().unwrap();
            let snapshot = state.native_call_snapshot.as_ref().unwrap();
            drop(native_view_at(&state, join, snapshot, iced::Size::new(1040.0, 680.0)));
        }
    }
}
