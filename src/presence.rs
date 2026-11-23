use crate::i18n::tr;
use crate::{auth::TeamsSession, avatars};
use rand::RngCore;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::Read,
    sync::OnceLock,
    time::{Duration, Instant},
};
use ureq::Agent;

pub const SELECTABLE: [Availability; 6] = [
    Availability::Available,
    Availability::Busy,
    Availability::DoNotDisturb,
    Availability::BeRightBack,
    Availability::Away,
    Availability::Offline,
];

const MAX_CONTACTS_PER_REQUEST: usize = 100;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const STATUS_NOTE_MAX_CHARS: usize = 280;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Availability {
    Available,
    AvailableIdle,
    Busy,
    BusyIdle,
    DoNotDisturb,
    BeRightBack,
    Away,
    Offline,
    Unknown,
}

impl Availability {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "available" => Self::Available,
            "availableidle" => Self::AvailableIdle,
            "busy" => Self::Busy,
            "busyidle" => Self::BusyIdle,
            "donotdisturb" | "dnd" => Self::DoNotDisturb,
            "berightback" => Self::BeRightBack,
            "away" => Self::Away,
            "offline" | "offwork" => Self::Offline,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Available => tr("Available"),
            Self::AvailableIdle => tr("Available, inactive"),
            Self::Busy => tr("Busy"),
            Self::BusyIdle => tr("Busy, inactive"),
            Self::DoNotDisturb => tr("Do not disturb"),
            Self::BeRightBack => tr("Be right back"),
            Self::Away => tr("Away"),
            Self::Offline => tr("Appear offline"),
            Self::Unknown => tr("Status unavailable"),
        }
    }

    fn api_values(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Available => Some(("Available", "Available")),
            Self::Busy => Some(("Busy", "Busy")),
            Self::DoNotDisturb => Some(("DoNotDisturb", "DoNotDisturb")),
            Self::BeRightBack => Some(("BeRightBack", "BeRightBack")),
            Self::Away => Some(("Away", "Away")),
            Self::Offline => Some(("Offline", "OffWork")),
            Self::AvailableIdle | Self::BusyIdle | Self::Unknown => None,
        }
    }

    fn service_value(self) -> Result<&'static str, String> {
        match self {
            Self::Available => Ok("Available"),
            Self::Busy => Ok("Busy"),
            Self::DoNotDisturb => Ok("DoNotDisturb"),
            Self::BeRightBack => Ok("BeRightBack"),
            Self::Away => Ok("Away"),
            Self::Offline => Ok("Offline"),
            Self::AvailableIdle | Self::BusyIdle | Self::Unknown => {
                Err("Selecione um status que possa ser publicado.".into())
            }
        }
    }

    fn is_manual_choice(self) -> bool {
        self.api_values().is_some()
    }
}

impl fmt::Display for Availability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Available,
    Away,
    BeRightBack,
    Busy,
    DoNotDisturb,
    InACall,
    InAConferenceCall,
    Inactive,
    InAMeeting,
    Offline,
    OffWork,
    OutOfOffice,
    Presenting,
    Unknown,
}

impl Activity {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "available" => Self::Available,
            "away" => Self::Away,
            "berightback" => Self::BeRightBack,
            "busy" => Self::Busy,
            "donotdisturb" => Self::DoNotDisturb,
            "inacall" => Self::InACall,
            "inaconferencecall" => Self::InAConferenceCall,
            "inactive" => Self::Inactive,
            "inameeting" => Self::InAMeeting,
            "offline" => Self::Offline,
            "offwork" => Self::OffWork,
            "outofoffice" => Self::OutOfOffice,
            "presenting" => Self::Presenting,
            _ => Self::Unknown,
        }
    }

    fn label(self, availability: Availability) -> &'static str {
        match self {
            Self::InACall | Self::InAConferenceCall => tr("In a call"),
            Self::InAMeeting => tr("In a meeting"),
            Self::Presenting => tr("Presenting"),
            Self::OutOfOffice => tr("Out of office"),
            _ => availability.label(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusNote {
    pub message: String,
    pub expiry: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusNoteExpiry {
    #[default]
    Never,
    OneHour,
    FourHours,
    Today,
    ThisWeek,
}

impl StatusNoteExpiry {
    pub const ALL: [Self; 5] = [
        Self::Never,
        Self::OneHour,
        Self::FourHours,
        Self::Today,
        Self::ThisWeek,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Never => "Nunca",
            Self::OneHour => "1 hora",
            Self::FourHours => "4 horas",
            Self::Today => tr("Today"),
            Self::ThisWeek => tr("This week"),
        }
    }

    fn expiry_iso(self) -> String {
        use chrono::{Datelike, Duration as ChronoDuration, Local, TimeZone};

        match self {
            Self::Never => "9999-12-31T00:00:00.000Z".to_owned(),
            Self::OneHour => (chrono::Utc::now() + ChronoDuration::hours(1)).to_rfc3339(),
            Self::FourHours => (chrono::Utc::now() + ChronoDuration::hours(4)).to_rfc3339(),
            Self::Today | Self::ThisWeek => {
                let now = Local::now();
                let date = now.date_naive();
                let end_date = if self == Self::Today {
                    date
                } else {
                    date + ChronoDuration::days(i64::from(
                        (7 - now.weekday().num_days_from_sunday()) % 7,
                    ))
                };
                Local
                    .with_ymd_and_hms(
                        end_date.year(),
                        end_date.month(),
                        end_date.day(),
                        23,
                        59,
                        59,
                    )
                    .single()
                    .map(|value| value.with_timezone(&chrono::Utc).to_rfc3339())
                    .unwrap_or_else(|| {
                        (chrono::Utc::now() + ChronoDuration::hours(24)).to_rfc3339()
                    })
            }
        }
    }
}

impl fmt::Display for StatusNoteExpiry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersonPresence {
    pub availability: Availability,
    pub activity: Activity,
    pub status_note: Option<StatusNote>,
}

impl PersonPresence {
    pub fn label(&self) -> &'static str {
        self.activity.label(self.availability)
    }
}

#[derive(Clone, Debug)]
pub struct PresenceSnapshot {
    pub own: Result<PersonPresence, String>,
    pub contacts: Result<Vec<(String, PersonPresence)>, String>,
}

#[derive(Clone, Debug)]
pub struct ObservedPresence {
    pub value: PersonPresence,
    pub observed_at: Instant,
}

#[derive(Clone, Debug)]
pub struct RefreshTicket {
    sequence: u64,
    own_revision: u64,
    contact_revisions: HashMap<String, u64>,
    pub contact_ids: Vec<String>,
}

#[derive(Default)]
pub struct Store {
    own: Option<ObservedPresence>,
    own_checked_at: Option<Instant>,
    own_revision: u64,
    own_pending: Option<Availability>,
    own_published: Option<Availability>,
    own_status_note: Option<StatusNote>,
    own_error: Option<String>,
    contacts: HashMap<String, ObservedPresence>,
    contact_revisions: HashMap<String, u64>,
    contacts_error: Option<String>,
    refresh_sequence: u64,
    refresh_in_flight: bool,
    publish_sequence: u64,
    session_id: String,
}

impl Store {
    pub fn own(&self, now: Instant) -> Option<ObservedPresence> {
        self.own
            .as_ref()
            .filter(|presence| is_fresh(presence.observed_at, now))
            .cloned()
    }

    pub fn own_for_display(&self, now: Instant) -> Option<PersonPresence> {
        let status_note = self
            .own_status_note
            .clone()
            .filter(status_note_is_active)
            .or_else(|| {
                self.own(now)
                    .and_then(|presence| presence.value.status_note)
                    .filter(|note| status_note_is_active(note))
            });
        if let Some(availability) = self.own_pending.or(self.own_published) {
            return Some(PersonPresence {
                availability,
                activity: Activity::Unknown,
                status_note,
            });
        }
        if let Some(presence) = self.own(now) {
            let mut value = presence.value;
            if value.status_note.is_none() {
                value.status_note = status_note;
            }
            Some(value)
        } else {
            status_note.map(|status_note| PersonPresence {
                availability: Availability::Unknown,
                activity: Activity::Unknown,
                status_note: Some(status_note),
            })
        }
    }

    pub fn set_own_status_note(&mut self, status_note: Option<StatusNote>) {
        let now = Instant::now();
        self.own_status_note = status_note.clone();
        let mut value = self.own.as_ref().map_or(
            PersonPresence {
                availability: Availability::Unknown,
                activity: Activity::Unknown,
                status_note: None,
            },
            |presence| presence.value.clone(),
        );
        value.status_note = status_note;
        self.own_revision = self.own_revision.wrapping_add(1);
        self.own = Some(ObservedPresence {
            value,
            observed_at: now,
        });
        self.own_checked_at = Some(now);
    }

    pub fn selected_own(&self, now: Instant) -> Option<Availability> {
        self.own_pending
            .or(self.own_published)
            .or_else(|| self.own(now).map(|presence| presence.value.availability))
            .filter(|availability| *availability != Availability::Unknown)
    }

    pub fn contact(&self, person_id: &str, now: Instant) -> Option<ObservedPresence> {
        let person_id = avatars::person_id(person_id)?;
        self.contacts
            .get(&person_id)
            .cloned()
            .filter(|presence| is_fresh(presence.observed_at, now))
    }

    pub fn own_error(&self) -> Option<&str> {
        self.own_error.as_deref()
    }

    pub fn contacts_error(&self) -> Option<&str> {
        self.contacts_error.as_deref()
    }

    pub fn is_refreshing(&self) -> bool {
        self.refresh_in_flight
    }

    pub fn pending_status(&self) -> Option<Availability> {
        self.own_pending
    }

    pub fn published_status(&self) -> Option<Availability> {
        self.own_published
    }

    pub fn refresh_due(&self, own_id: &str, contact_ids: &[String], now: Instant) -> bool {
        if self.refresh_in_flight {
            return false;
        }
        if self
            .own_checked_at
            .is_none_or(|checked| now.saturating_duration_since(checked) >= REFRESH_INTERVAL)
        {
            return true;
        }
        let own_id = avatars::person_id(own_id);
        contact_ids.iter().any(|id| {
            let Some(id) = avatars::person_id(id) else {
                return false;
            };
            if Some(&id) == own_id.as_ref() {
                return false;
            }
            self.contacts
                .get(&id)
                .is_none_or(|presence| !is_fresh_with_interval(presence.observed_at, now))
        })
    }

    pub fn begin_refresh(
        &mut self,
        own_id: &str,
        contact_ids: &[String],
        now: Instant,
        force: bool,
    ) -> Option<RefreshTicket> {
        if self.refresh_in_flight || (!force && !self.refresh_due(own_id, contact_ids, now)) {
            return None;
        }
        let own_id = avatars::person_id(own_id)?;
        let mut unique = HashSet::new();
        let contact_ids = contact_ids
            .iter()
            .filter_map(|id| avatars::person_id(id))
            .filter(|id| id != &own_id && unique.insert(id.clone()))
            .take(MAX_CONTACTS_PER_REQUEST)
            .collect::<Vec<_>>();
        self.refresh_sequence = self.refresh_sequence.wrapping_add(1);
        self.refresh_in_flight = true;
        Some(RefreshTicket {
            sequence: self.refresh_sequence,
            own_revision: self.own_revision,
            contact_revisions: contact_ids
                .iter()
                .map(|id| (id.clone(), self.contact_revisions.get(id).copied().unwrap_or(0)))
                .collect(),
            contact_ids,
        })
    }

    pub fn complete_refresh(
        &mut self,
        ticket: RefreshTicket,
        snapshot: PresenceSnapshot,
        now: Instant,
    ) -> bool {
        if ticket.sequence != self.refresh_sequence {
            return false;
        }
        self.refresh_in_flight = false;
        if self.own_revision == ticket.own_revision {
            self.own_checked_at = Some(now);
            match snapshot.own {
                Ok(mut value) => {
                    if let Some(status_note) = value.status_note.clone() {
                        self.own_status_note = Some(status_note);
                    } else {
                        value.status_note = self.own_status_note.clone();
                    }
                    let availability = value.availability;
                    let observed = ObservedPresence {
                        value,
                        observed_at: now,
                    };
                    self.own = Some(observed);
                    self.own_error = None;
                    if self.own_published == Some(availability) {
                        self.own_published = None;
                    } else if self.own_published.is_some() {
                        self.own_published = None;
                        self.own_error =
                            Some("O Teams confirmou um status diferente do solicitado.".into());
                    }
                }
                Err(error) => self.own_error = Some(error),
            }
        }

        let mut contact_values = HashMap::new();
        let contacts_error = match snapshot.contacts {
            Ok(contacts) => {
                for (id, value) in contacts {
                    if let Some(id) = avatars::person_id(&id) {
                        contact_values.insert(id, value);
                    }
                }
                None
            }
            Err(error) => Some(error),
        };
        if snapshot_contacts_succeeded(&contacts_error) {
            for id in &ticket.contact_ids {
                let original_revision = ticket.contact_revisions.get(id).copied().unwrap_or(0);
                if self.contact_revisions.get(id).copied().unwrap_or(0) != original_revision {
                    continue;
                }
                let mut value = contact_values.remove(id).unwrap_or(PersonPresence {
                    availability: Availability::Unknown,
                    activity: Activity::Unknown,
                    status_note: None,
                });
                if value.status_note.is_none() {
                    value.status_note = self
                        .contacts
                        .get(id)
                        .and_then(|presence| presence.value.status_note.clone());
                }
                self.contacts.insert(
                    id.clone(),
                    ObservedPresence {
                        value,
                        observed_at: now,
                    },
                );
            }
        }
        self.contacts_error = contacts_error;
        true
    }

    pub fn apply_event(&mut self, person_id: &str, value: PersonPresence, own_id: &str, now: Instant) {
        let Some(person_id) = avatars::person_id(person_id) else {
            return;
        };
        if avatars::person_id(own_id).as_deref() == Some(person_id.as_str()) {
            let mut value = value;
            if let Some(status_note) = value.status_note.clone() {
                self.own_status_note = Some(status_note);
            } else {
                value.status_note = self.own_status_note.clone();
            }
            let availability = value.availability;
            let observed = ObservedPresence {
                value,
                observed_at: now,
            };
            self.own_revision = self.own_revision.wrapping_add(1);
            self.own_checked_at = Some(now);
            self.own = Some(observed);
            self.own_error = None;
            if let Some(expected) = self.own_published.take()
                && expected != availability
            {
                self.own_error = Some("O Teams confirmou um status diferente do solicitado.".into());
            }
        } else {
            let mut value = value;
            if value.status_note.is_none() {
                value.status_note = self
                    .contacts
                    .get(&person_id)
                    .and_then(|presence| presence.value.status_note.clone());
            }
            let observed = ObservedPresence {
                value,
                observed_at: now,
            };
            let revision = self.contact_revisions.entry(person_id.clone()).or_default();
            *revision = revision.wrapping_add(1);
            self.contacts.insert(person_id, observed);
        }
    }

    pub fn begin_publish(&mut self, availability: Availability) -> Option<(u64, String)> {
        if !availability.is_manual_choice() || self.own_pending.is_some() {
            return None;
        }
        self.publish_sequence = self.publish_sequence.wrapping_add(1);
        if self.session_id.is_empty() {
            self.session_id = format!("teams-linux-{:016x}", rand::random::<u64>());
        }
        self.own_pending = Some(availability);
        self.own_error = None;
        Some((self.publish_sequence, self.session_id.clone()))
    }

    pub fn complete_publish(
        &mut self,
        sequence: u64,
        availability: Availability,
        result: Result<(), String>,
    ) -> bool {
        if sequence != self.publish_sequence || self.own_pending != Some(availability) {
            return false;
        }
        self.own_pending = None;
        match result {
            Ok(()) => {
                self.own_published = Some(availability);
                self.own_checked_at = None;
                self.own_error = None;
            }
            Err(error) => {
                self.own_published = None;
                self.own_error = Some(error);
            }
        }
        true
    }

    pub fn set_demo_status(&mut self, availability: Availability, own_id: &str) {
        let value = PersonPresence {
            availability,
            activity: Activity::parse(availability.api_values().map_or("Unknown", |v| v.1)),
            status_note: self.own_status_note.clone(),
        };
        let _ = own_id;
        self.own_revision = self.own_revision.wrapping_add(1);
        self.own = Some(ObservedPresence {
            value,
            observed_at: Instant::now(),
        });
        self.own_checked_at = Some(Instant::now());
        self.own_error = None;
        self.own_pending = None;
        self.own_published = None;
    }

    pub fn set_demo_status_note(&mut self, message: String) {
        let note = (!message.trim().is_empty()).then(|| StatusNote {
            message: message.trim().to_owned(),
            expiry: None,
        });
        self.set_own_status_note(note);
    }

    pub fn set_demo_contact(&mut self, person_id: &str, availability: Availability) {
        let Some(person_id) = avatars::person_id(person_id) else {
            return;
        };
        let value = PersonPresence {
            availability,
            activity: Activity::parse(availability.api_values().map_or("Unknown", |v| v.1)),
            status_note: None,
        };
        self.contacts.insert(
            person_id,
            ObservedPresence {
                value,
                observed_at: Instant::now(),
            },
        );
    }
}

trait PresenceTransport {
    fn post_json(&self, path: &str, body: &Value) -> Result<Value, String>;
    fn put_empty(&self, path: &str, body: &Value) -> Result<(), String>;
    fn post_empty(&self, path: &str, body: &Value) -> Result<(), String> {
        self.post_json(path, body).map(|_| ())
    }
}

struct TeamsPresenceTransport {
    root: String,
    token: String,
}

impl PresenceTransport for TeamsPresenceTransport {
    fn post_empty(&self, path: &str, body: &Value) -> Result<(), String> {
        self.request("POST", path, body, false).map(|_| ())
    }
    fn post_json(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.request("POST", path, body, true)
    }

    fn put_empty(&self, path: &str, body: &Value) -> Result<(), String> {
        self.request("PUT", path, body, false).map(|_| ())
    }
}

impl TeamsPresenceTransport {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: &Value,
        read_json: bool,
    ) -> Result<Value, String> {
        let url = format!("{}{path}", self.root.trim_end_matches('/'));
        let request = match method {
            "POST" => agent().post(&url),
            "PUT" => agent().put(&url),
            _ => return Err(tr("Invalid local presence method.").into()),
        };
        let request = if let Some(endpoint) = body.get("endpointId").or_else(|| body.get("id")).and_then(Value::as_str) {
            request.header("x-ms-endpoint-id", endpoint)
        } else { request };
        let response = request
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .header("x-ms-client-user-agent", "Teams-V2-Desktop")
            .header("x-ms-correlation-id", correlation_id())
            .send_json(body)
            .map_err(|_| tr("Could not connect to the Teams presence service.").to_owned())?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(friendly_status_error(status));
        }
        if !read_json {
            return Ok(Value::Null);
        }
        read_response_json(response)
    }
}

pub(crate) fn automatic_endpoint_id() -> String { correlation_id() }

pub(crate) fn report_automatic_activity(
    session: TeamsSession, endpoint: &str, register: bool, active: bool,
) -> Result<(), String> {
    let (root, token) = session.presence_credentials()?;
    let transport = TeamsPresenceTransport { root, token };
    automatic_activity_with_transport(&transport, endpoint, register, active)
}

pub(crate) fn delete_automatic_endpoint(session: TeamsSession, endpoint: &str) -> Result<(), String> {
    let (root, token) = session.presence_credentials()?;
    let response = agent().delete(&format!("{}/v1/me/endpoints/{endpoint}", root.trim_end_matches('/')))
        .header("Authorization", &format!("Bearer {token}"))
        .header("x-ms-client-user-agent", "Teams-V2-Desktop")
        .header("x-ms-endpoint-id", endpoint)
        .header("x-ms-correlation-id", correlation_id())
        .call().map_err(|_| tr("Could not stop automatic presence.").to_owned())?;
    if response.status().is_success() || response.status().as_u16() == 404 { Ok(()) }
    else { Err(friendly_status_error(response.status().as_u16())) }
}

fn automatic_activity_with_transport(
    transport: &impl PresenceTransport, endpoint: &str, register: bool, active: bool,
) -> Result<(), String> {
    if register {
        let availability = if active { "Available" } else { "Away" };
        transport.put_empty("/v1/me/endpoints/", &json!({
            "id": endpoint, "availability": availability, "activity": availability,
            "deviceType": "Desktop", "activityReporting": "Direct"
        }))?;
    }
    transport.post_empty("/v1/me/reportmyactivity/", &json!({ "endpointId": endpoint, "isActive": active }))
}

fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(12)))
            .build()
            .new_agent()
    })
}

fn correlation_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

fn friendly_status_error(status: u16) -> String {
    match status {
        401 => tr("The Teams session expired. Sign in again to check presence.").into(),
        403 => tr("The Teams presence service refused access (HTTP 403).").into(),
        3..=399 => tr("The Teams presence service requested an unrecognized redirect.").into(),
        _ => format!("The Teams presence service rejected the request (HTTP {status})."),
    }
}

fn read_response_json(mut response: ureq::http::Response<ureq::Body>) -> Result<Value, String> {
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| tr("Could not read the presence response.").to_owned())?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(tr("The presence response exceeds the local limit.").into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| tr("The presence service returned an invalid response.").into())
}

async fn run_on_worker<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("teams-presence".into())
        .spawn(move || {
            let _ = sender.send(job());
        })
        .map_err(|_| tr("Could not start the presence lookup.").to_owned())?;
    receiver
        .await
        .map_err(|_| tr("The presence lookup was interrupted.").to_owned())
}

pub async fn fetch(
    session: TeamsSession,
    own_id: String,
    contact_ids: Vec<String>,
) -> PresenceSnapshot {
    match run_on_worker(move || fetch_blocking(session, own_id, contact_ids)).await {
        Ok(snapshot) => snapshot,
        Err(error) => PresenceSnapshot {
            own: Err(error.clone()),
            contacts: Err(error),
        },
    }
}

fn fetch_blocking(
    session: TeamsSession,
    own_id: String,
    contact_ids: Vec<String>,
) -> PresenceSnapshot {
    let (root, token) = match session.presence_credentials() {
        Ok(credentials) => credentials,
        Err(error) => {
            return PresenceSnapshot {
                own: Err(error.clone()),
                contacts: Err(error),
            };
        }
    };
    let transport = TeamsPresenceTransport { root, token };
    fetch_with_transport(&transport, own_id, contact_ids)
}

fn fetch_with_transport<T: PresenceTransport>(
    transport: &T,
    own_id: String,
    contact_ids: Vec<String>,
) -> PresenceSnapshot {
    let Some(own_id) = avatars::person_id(&own_id) else {
        return PresenceSnapshot {
            own: Err(tr("The current account identity is invalid.").into()),
            contacts: Err(tr("The current account identity is invalid.").into()),
        };
    };
    let contact_ids = contact_ids
        .into_iter()
        .filter_map(|id| avatars::person_id(&id))
        .filter(|id| id != &own_id)
        .take(MAX_CONTACTS_PER_REQUEST.saturating_sub(1))
        .collect::<Vec<_>>();
    let mut ids = Vec::with_capacity(contact_ids.len() + 1);
    ids.push(own_id.clone());
    ids.extend(contact_ids.iter().cloned());
    let presences = match fetch_many(transport, &ids) {
        Ok(presences) => presences,
        Err(error) => {
            return PresenceSnapshot {
                own: Err(error.clone()),
                contacts: Err(error),
            };
        }
    };
    let mut presence_by_id = presences.into_iter().collect::<HashMap<_, _>>();
    let own = presence_by_id
        .remove(&own_id)
        .ok_or_else(|| tr("Teams did not return presence for the current account.").to_owned());
    let contacts = Ok(contact_ids
        .into_iter()
        .filter_map(|id| presence_by_id.remove(&id).map(|presence| (id, presence)))
        .collect());
    PresenceSnapshot { own, contacts }
}

pub async fn publish(
    session: TeamsSession,
    availability: Availability,
    _session_id: String,
) -> Result<(), String> {
    run_on_worker(move || publish_blocking(session, availability)).await?
}

fn publish_blocking(session: TeamsSession, availability: Availability) -> Result<(), String> {
    let (root, token) = session.presence_credentials()?;
    let transport = TeamsPresenceTransport { root, token };
    publish_with_transport(&transport, availability)
}

fn publish_with_transport<T: PresenceTransport>(
    transport: &T,
    availability: Availability,
) -> Result<(), String> {
    let availability_value = availability.service_value()?;
    let body = json!({ "availability": availability_value });
    transport
        .put_empty("/v1/me/forceavailability/", &body)
}

fn fetch_many<T: PresenceTransport>(
    transport: &T,
    user_ids: &[String],
) -> Result<Vec<(String, PersonPresence)>, String> {
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    let body = Value::Array(
        user_ids
            .iter()
            .map(|id| json!({ "mri": format!("8:orgid:{id}") }))
            .collect(),
    );
    let value = transport.post_json("/v1/presence/getpresence/", &body)?;
    let items = value
        .get("unifiedPresenceStates")
        .and_then(Value::as_array)
        .or_else(|| value.get("value").and_then(Value::as_array))
        .or_else(|| value.get("presenceStates").and_then(Value::as_array))
        .or_else(|| value.as_array())
        .ok_or_else(|| tr("The service did not return the expected presence list.").to_owned())?;
    Ok(items.iter().filter_map(parse_batch_item).collect())
}

pub async fn publish_status_note(
    session: TeamsSession,
    message: String,
    expiry: StatusNoteExpiry,
) -> Result<Option<StatusNote>, String> {
    if message.chars().count() > STATUS_NOTE_MAX_CHARS {
        return Err(format!("The status message can contain up to {STATUS_NOTE_MAX_CHARS} characters."));
    }
    let message = message.trim().to_owned();
    run_on_worker(move || {
        let (root, token) = session.presence_credentials()?;
        let transport = TeamsPresenceTransport { root, token };
        publish_status_note_with_transport(&transport, message, expiry)
    })
    .await?
}

fn publish_status_note_with_transport<T: PresenceTransport>(
    transport: &T,
    message: String,
    expiry: StatusNoteExpiry,
) -> Result<Option<StatusNote>, String> {
    if message.chars().count() > STATUS_NOTE_MAX_CHARS {
        return Err(format!("The status message can contain up to {STATUS_NOTE_MAX_CHARS} characters."));
    }
    let message = message.trim().to_owned();
    let body = if message.is_empty() {
        json!({ "message": "" })
    } else {
        json!({ "message": message, "expiry": expiry.expiry_iso() })
    };
    transport.put_empty("/v1/me/publishnote", &body)?;
    if body["message"].as_str().is_some_and(str::is_empty) {
        Ok(None)
    } else {
        Ok(Some(StatusNote {
            message: body["message"].as_str().unwrap_or_default().to_owned(),
            expiry: body["expiry"].as_str().map(str::to_owned),
        }))
    }
}

fn parse_batch_item(value: &Value) -> Option<(String, PersonPresence)> {
    Some((id_from_value(value)?, parse_presence_fields(value)?))
}

fn parse_presence_fields(value: &Value) -> Option<PersonPresence> {
    let nested = value.get("presence").unwrap_or(value);
    let availability = nested
        .get("availability")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("availability").and_then(Value::as_str))
        })
        .map(Availability::parse)
        .unwrap_or(Availability::Unknown);
    let activity = nested
        .get("activity")
        .and_then(Value::as_str)
        .map(Activity::parse)
        .unwrap_or(Activity::Unknown);
    let status_note = parse_status_note(nested).or_else(|| parse_status_note(value));
    if availability == Availability::Unknown && status_note.is_none() {
        return None;
    }
    Some(PersonPresence {
        availability,
        activity,
        status_note,
    })
}

fn parse_status_note(value: &Value) -> Option<StatusNote> {
    let note = value
        .get("note")
        .or_else(|| value.get("statusNote"))
        .or_else(|| value.get("statusMessage"))?;
    let message = note
        .as_str()
        .or_else(|| note.get("message").and_then(Value::as_str))
        .or_else(|| note.get("text").and_then(Value::as_str))?
        .trim();
    (!message.is_empty()).then(|| StatusNote {
        message: message.to_owned(),
        expiry: note
            .get("expiry")
            .and_then(Value::as_str)
            .or_else(|| note.pointer("/expiry/expiryDate").and_then(Value::as_str))
            .or_else(|| note.get("expirationTime").and_then(Value::as_str))
            .or_else(|| note.get("expiresAt").and_then(Value::as_str))
            .map(str::to_owned),
    })
}

fn snapshot_contacts_succeeded(error: &Option<String>) -> bool {
    error.is_none()
}

fn status_note_is_active(note: &StatusNote) -> bool {
    note.expiry.as_deref().is_none_or(|expiry| {
        chrono::DateTime::parse_from_rfc3339(expiry)
            .map(|expiry| expiry > chrono::Utc::now())
            .unwrap_or(true)
    })
}

fn id_from_value(value: &Value) -> Option<String> {
    ["id", "userId", "user_id", "userObjectId", "mri"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .find_map(avatars::person_id)
}

pub fn parse_event(payload_json: &str) -> Vec<(String, PersonPresence)> {
    if payload_json.len() > MAX_RESPONSE_BYTES {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(payload_json) else {
        return Vec::new();
    };
    let mut found = HashMap::new();
    collect_event_presence(&value, 0, &mut found);
    found.into_iter().collect()
}

fn collect_event_presence(
    value: &Value,
    depth: usize,
    found: &mut HashMap<String, PersonPresence>,
) {
    if depth >= 24 {
        return;
    }
    match value {
        Value::Object(object) => {
            if let Some(id) = id_from_value(value)
                && let Some(presence) = parse_event_presence(value)
            {
                found.insert(id, presence);
            }
            for child in object.values() {
                collect_event_presence(child, depth + 1, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_event_presence(item, depth + 1, found);
            }
        }
        Value::String(text) if text.len() <= MAX_RESPONSE_BYTES => {
            if let Ok(nested) = serde_json::from_str::<Value>(text) {
                collect_event_presence(&nested, depth + 1, found);
            }
        }
        _ => {}
    }
}

fn parse_event_presence(value: &Value) -> Option<PersonPresence> {
    parse_presence_fields(value)
}

fn is_fresh(observed_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(observed_at) < Duration::from_secs(90)
}

fn is_fresh_with_interval(observed_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(observed_at) < REFRESH_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const ANA: &str = "11111111-1111-1111-1111-111111111111";
    const OWNER: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

    #[test]
    fn availability_parses_service_variants_and_uses_pt_labels() {
        assert_eq!(Availability::parse("Available"), Availability::Available);
        assert_eq!(Availability::parse("BusyIdle"), Availability::BusyIdle);
        assert_eq!(
            Availability::parse("DoNotDisturb"),
            Availability::DoNotDisturb
        );
        assert_eq!(Availability::parse("future-state"), Availability::Unknown);
        assert_eq!(Availability::DoNotDisturb.label(), "Do not disturb");
        assert!(!Availability::Unknown.is_manual_choice());
    }

    #[test]
    fn person_card_label_prefers_confirmed_activity_over_base_availability() {
        let presence = PersonPresence {
            availability: Availability::Busy,
            activity: Activity::InAMeeting,
            status_note: None,
        };
        assert_eq!(presence.label(), "In a meeting");
    }

    #[test]
    fn batch_status_decoder_requires_a_valid_person_guid_and_known_status() {
        let decoded = parse_batch_item(
            &json!({
                "id": format!("8:orgid:{ANA}"),
                "availability": "Available",
                "activity": "Available"
            }),
        );
        assert_eq!(
            decoded,
            Some((
                ANA.to_owned(),
                PersonPresence {
                    availability: Availability::Available,
                    activity: Activity::Available,
                    status_note: None,
                }
            ))
        );
        assert!(parse_batch_item(&json!({"id":"not-a-guid","availability":"Busy"})).is_none());
        assert!(parse_batch_item(&json!({"id":ANA,"availability":"FutureStatus"})).is_none());
        assert_eq!(
            parse_presence_fields(&json!({
                "availability": "Busy",
                "activity": "InAMeeting"
            })),
            Some(PersonPresence {
                availability: Availability::Busy,
                activity: Activity::InAMeeting,
                status_note: None,
            })
        );
    }

    #[test]
    fn status_note_is_read_from_nested_or_outer_presence_payloads() {
        let nested = parse_presence_fields(&json!({
            "presence": {"availability": "Away"},
            "statusNote": {"message": "Em reunião", "expiry": "2030-01-01T12:00:00Z"}
        }))
        .unwrap();
        assert_eq!(nested.status_note.unwrap().message, "Em reunião");

        let outer = parse_presence_fields(&json!({
            "availability": "Busy",
            "statusMessage": "Foco até o almoço"
        }))
        .unwrap();
        assert_eq!(outer.status_note.unwrap().message, "Foco até o almoço");
    }

    #[test]
    fn event_decoder_extracts_explicit_status_without_guessing_missing_identity() {
        let event = json!({
            "resourceType": "PresenceUpdate",
            "resource": {
                "userId": ANA,
                "presence": {"availability": "DoNotDisturb", "activity": "Presenting"}
            }
        })
        .to_string();
        assert_eq!(
            parse_event(&event),
            vec![(
                ANA.to_owned(),
                PersonPresence {
                    availability: Availability::DoNotDisturb,
                    activity: Activity::Presenting,
                    status_note: None,
                }
            )]
        );
        assert!(parse_event(r#"{"resourceType":"PresenceUpdate","availability":"Busy"}"#).is_empty());
    }

    #[test]
    fn a_newer_push_wins_over_an_in_flight_presence_poll() {
        let now = Instant::now();
        let mut state = Store::default();
        let ticket = state
            .begin_refresh(OWNER, &[ANA.into()], now, true)
            .unwrap();
        let pushed = PersonPresence {
            availability: Availability::Busy,
            activity: Activity::Busy,
            status_note: None,
        };
        state.apply_event(ANA, pushed.clone(), OWNER, now + Duration::from_secs(1));
        state.complete_refresh(
            ticket,
            PresenceSnapshot {
                own: Err("offline".into()),
                contacts: Ok(vec![(
                    ANA.into(),
                    PersonPresence {
                        availability: Availability::Available,
                        activity: Activity::Available,
                        status_note: None,
                    },
                )]),
            },
            now + Duration::from_secs(2),
        );
        assert_eq!(state.contact(ANA, now + Duration::from_secs(2)).unwrap().value, pushed);
    }

    #[test]
    fn missing_or_expired_status_is_never_presented_as_online() {
        let now = Instant::now();
        let mut state = Store::default();
        let ticket = state
            .begin_refresh(OWNER, &[ANA.into()], now, true)
            .unwrap();
        state.complete_refresh(
            ticket,
            PresenceSnapshot {
                own: Err("offline".into()),
                contacts: Ok(Vec::new()),
            },
            now,
        );
        assert!(state.contact(ANA, now).is_some());
        assert!(state.contact(ANA, now + Duration::from_secs(91)).is_none());
    }

    #[test]
    fn failed_contact_refresh_keeps_only_the_last_known_value_until_it_expires() {
        let now = Instant::now();
        let mut state = Store::default();
        let known = PersonPresence {
            availability: Availability::Available,
            activity: Activity::Available,
            status_note: None,
        };
        state.apply_event(ANA, known.clone(), OWNER, now);
        let ticket = state
            .begin_refresh(OWNER, &[ANA.into()], now, true)
            .unwrap();
        state.complete_refresh(
            ticket,
            PresenceSnapshot {
                own: Err("not available".into()),
                contacts: Err("permission denied".into()),
            },
            now + Duration::from_secs(10),
        );
        assert_eq!(state.contact(ANA, now + Duration::from_secs(10)).unwrap().value, known);
        assert!(state.contacts_error().is_some());
        assert!(state.contact(ANA, now + Duration::from_secs(91)).is_none());
    }

    #[test]
    fn own_status_selection_distinguishes_pending_published_and_confirmed() {
        let now = Instant::now();
        let mut state = Store::default();
        state.set_demo_status(Availability::Available, "demo-user");
        assert_eq!(state.selected_own(now), Some(Availability::Available));

        let (sequence, _) = state.begin_publish(Availability::Busy).unwrap();
        assert_eq!(state.selected_own(now), Some(Availability::Busy));
        assert_eq!(state.pending_status(), Some(Availability::Busy));
        state.complete_publish(sequence, Availability::Busy, Ok(()));
        assert_eq!(state.pending_status(), None);
        assert_eq!(state.published_status(), Some(Availability::Busy));

        let ticket = state.begin_refresh(OWNER, &[], now, true).unwrap();
        state.complete_refresh(
            ticket,
            PresenceSnapshot {
                own: Ok(PersonPresence {
                    availability: Availability::Busy,
                    activity: Activity::Busy,
                    status_note: None,
                }),
                contacts: Ok(Vec::new()),
            },
            now + Duration::from_secs(1),
        );
        assert_eq!(state.selected_own(now + Duration::from_secs(1)), Some(Availability::Busy));
        assert_eq!(state.published_status(), None);
        assert_eq!(state.own_error(), None);
    }

    #[test]
    fn own_presence_event_confirms_or_rejects_the_published_status() {
        let now = Instant::now();
        let mut state = Store::default();
        state.set_demo_status(Availability::Available, OWNER);
        let (sequence, _) = state.begin_publish(Availability::Busy).unwrap();
        state.complete_publish(sequence, Availability::Busy, Ok(()));
        state.apply_event(
            OWNER,
            PersonPresence {
                availability: Availability::Available,
                activity: Activity::Available,
                status_note: None,
            },
            OWNER,
            now + Duration::from_secs(1),
        );
        assert_eq!(state.published_status(), None);
        assert_eq!(state.selected_own(now + Duration::from_secs(1)), Some(Availability::Available));
        assert!(state.own_error().is_some());
    }

    #[test]
    fn editable_status_note_survives_presence_refreshes_and_availability_changes() {
        let now = Instant::now();
        let mut state = Store::default();
        let note = StatusNote {
            message: "Em atendimento".into(),
            expiry: None,
        };
        state.set_own_status_note(Some(note.clone()));
        state.apply_event(
            OWNER,
            PersonPresence {
                availability: Availability::Available,
                activity: Activity::Available,
                status_note: None,
            },
            OWNER,
            now,
        );
        assert_eq!(
            state
                .own_for_display(now)
                .and_then(|presence| presence.status_note),
            Some(note.clone())
        );

        let ticket = state.begin_refresh(OWNER, &[], now, true).unwrap();
        state.complete_refresh(
            ticket,
            PresenceSnapshot {
                own: Ok(PersonPresence {
                    availability: Availability::Busy,
                    activity: Activity::Busy,
                    status_note: None,
                }),
                contacts: Ok(Vec::new()),
            },
            now + Duration::from_secs(1),
        );
        state.set_demo_status(Availability::Away, OWNER);
        assert_eq!(
            state
                .own_for_display(now + Duration::from_secs(1))
                .and_then(|presence| presence.status_note),
            Some(note)
        );

        state.set_own_status_note(None);
        assert_eq!(
            state
                .own_for_display(now + Duration::from_secs(2))
                .and_then(|presence| presence.status_note),
            None
        );
    }

    #[test]
    fn presence_http_contract_runs_against_a_local_fixture_server() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let root = format!("http://{}", server.server_addr());
        let (seen_tx, seen_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            for _ in 0..4 {
                let mut request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .expect("presence request reaches local fixture");
                let method = request.method().to_string();
                let path = request.url().to_owned();
                let authorization = request
                    .headers()
                    .iter()
                    .find(|header| header.field.equiv("Authorization"))
                    .map(|header| header.value.as_str().to_owned());
                let client_agent = request
                    .headers()
                    .iter()
                    .find(|header| header.field.equiv("x-ms-client-user-agent"))
                    .map(|header| header.value.as_str().to_owned());
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                seen_tx
                    .send((method, path.clone(), authorization, client_agent, body))
                    .unwrap();
                let response = match path.as_str() {
                    "/v1/presence/getpresence/" => tiny_http::Response::from_string(format!(
                        r#"{{"unifiedPresenceStates":[{{"mri":"8:orgid:{OWNER}","presence":{{"availability":"Busy","activity":"InAMeeting","note":{{"message":"Em foco","expiry":"2030-01-01T00:00:00Z"}}}}}},{{"mri":"8:orgid:{ANA}","presence":{{"availability":"Available","activity":"Available"}}}}]}}"#
                    )),
                    "/v1/me/forceavailability/" | "/v1/me/publishnote" => {
                        tiny_http::Response::from_string("{}")
                    }
                    _ => tiny_http::Response::from_string("not found").with_status_code(404),
                };
                request.respond(response).unwrap();
            }
        });
        let transport = TeamsPresenceTransport {
            root,
            token: "fixture-token".into(),
        };
        let snapshot = fetch_with_transport(
            &transport,
            OWNER.to_owned(),
            vec![ANA.to_owned(), OWNER.to_owned()],
        );
        assert_eq!(
            snapshot.own.unwrap(),
            PersonPresence {
                availability: Availability::Busy,
                activity: Activity::InAMeeting,
                status_note: Some(StatusNote {
                    message: "Em foco".into(),
                    expiry: Some("2030-01-01T00:00:00Z".into()),
                }),
            }
        );
        assert_eq!(
            snapshot.contacts.unwrap(),
            vec![(
                ANA.to_owned(),
                PersonPresence {
                    availability: Availability::Available,
                    activity: Activity::Available,
                    status_note: None,
                }
            )]
        );
        publish_with_transport(&transport, Availability::DoNotDisturb).unwrap();
        let note = publish_status_note_with_transport(
            &transport,
            "  Em atendimento  ".into(),
            StatusNoteExpiry::FourHours,
        )
        .unwrap()
        .unwrap();
        assert_eq!(note.message, "Em atendimento");
        assert!(note.expiry.is_some());
        assert_eq!(
            publish_status_note_with_transport(&transport, " ".into(), StatusNoteExpiry::Never)
                .unwrap(),
            None
        );
        worker.join().unwrap();
        let requests = seen_rx.into_iter().collect::<Vec<_>>();
        assert_eq!(requests.len(), 4);
        assert!(requests.iter().all(|request| {
            request.2.as_deref() == Some("Bearer fixture-token")
                && request.3.as_deref() == Some("Teams-V2-Desktop")
        }));
        assert_eq!(requests[0].0, "POST");
        assert_eq!(requests[0].1, "/v1/presence/getpresence/");
        let batch: Value = serde_json::from_str(&requests[0].4).unwrap();
        assert_eq!(batch, json!([
            { "mri": format!("8:orgid:{OWNER}") },
            { "mri": format!("8:orgid:{ANA}") }
        ]));
        assert_eq!(requests[1].0, "PUT");
        assert_eq!(requests[1].1, "/v1/me/forceavailability/");
        let publish: Value = serde_json::from_str(&requests[1].4).unwrap();
        assert_eq!(publish, json!({ "availability": "DoNotDisturb" }));
        assert_eq!(requests[2].0, "PUT");
        assert_eq!(requests[2].1, "/v1/me/publishnote");
        let set_note: Value = serde_json::from_str(&requests[2].4).unwrap();
        assert_eq!(set_note["message"], "Em atendimento");
        assert!(set_note["expiry"].as_str().is_some());
        assert_eq!(requests[3].0, "PUT");
        assert_eq!(requests[3].1, "/v1/me/publishnote");
        let clear_note: Value = serde_json::from_str(&requests[3].4).unwrap();
        assert_eq!(clear_note, json!({ "message": "" }));
    }
}

#[cfg(test)]
mod automatic_activity_tests {
    use super::*;
    #[test]
    fn endpoint_activity_uses_original_routes_and_never_manual_availability() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let root = format!("http://{}", server.server_addr());
        let worker = std::thread::spawn(move || {
            for (method, path, active) in [
                ("PUT", "/v1/me/endpoints/", None),
                ("POST", "/v1/me/reportmyactivity/", Some(true)),
                ("POST", "/v1/me/reportmyactivity/", Some(false)),
            ] {
                let mut request = server.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
                assert_eq!(request.method().as_str(), method);
                assert_eq!(request.url(), path);
                assert!(request.headers().iter().any(|h| h.field.equiv("x-ms-endpoint-id") && h.value.as_str() == "fixture-endpoint"));
                let mut body = String::new(); request.as_reader().read_to_string(&mut body).unwrap();
                let body: Value = serde_json::from_str(&body).unwrap();
                if let Some(active) = active {
                    assert_eq!(body, json!({"endpointId":"fixture-endpoint","isActive":active}));
                } else {
                    assert_eq!(body, json!({"id":"fixture-endpoint","availability":"Available","activity":"Available","deviceType":"Desktop","activityReporting":"Direct"}));
                }
                request.respond(tiny_http::Response::empty(204)).unwrap();
            }
        });
        let transport = TeamsPresenceTransport { root, token: "synthetic".into() };
        automatic_activity_with_transport(&transport, "fixture-endpoint", true, true).unwrap();
        automatic_activity_with_transport(&transport, "fixture-endpoint", false, false).unwrap();
        worker.join().unwrap();
    }
}
