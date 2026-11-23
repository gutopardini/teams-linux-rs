//! Native agenda model, regional Teams calendar contract and overlap layout.

use crate::i18n::tr;
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarView {
    Day,
    #[default]
    WorkWeek,
    Week,
    Agenda,
}
impl CalendarView {
    pub const ALL: [Self; 4] = [Self::Day, Self::WorkWeek, Self::Week, Self::Agenda];
    pub fn days(self) -> i64 {
        match self {
            Self::Day => 1,
            Self::WorkWeek => 5,
            _ => 7,
        }
    }
    pub fn first_date(self, anchor: NaiveDate) -> NaiveDate {
        if matches!(self, Self::Week | Self::WorkWeek | Self::Agenda) {
            anchor
                .checked_sub_signed(chrono::Duration::days(i64::from(
                    anchor.weekday().num_days_from_monday(),
                )))
                .unwrap_or(anchor)
        } else {
            anchor
        }
    }
    pub fn step(self) -> i64 {
        if self == Self::Day { 1 } else { 7 }
    }
}
impl std::fmt::Display for CalendarView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Day => tr("Day"),
            Self::WorkWeek => tr("Work week"),
            Self::Week => tr("Week"),
            Self::Agenda => tr("Agenda"),
        })
    }
}

pub fn range_for(
    anchor: NaiveDate,
    view: CalendarView,
) -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
    let first = view.first_date(anchor);
    let last = first
        .checked_add_signed(chrono::Duration::days(view.days()))
        .ok_or("Data fora do intervalo.")?;
    let boundary = |date: NaiveDate| {
        date.and_hms_opt(0, 0, 0)
            .and_then(|date| Local.from_local_datetime(&date).earliest())
            .map(|date| date.with_timezone(&Utc))
            .ok_or_else(|| tr("Could not convert the local time zone.").to_owned())
    };
    Ok((boundary(first)?, boundary(last)?))
}

pub const PAGE_SIZE: usize = 50;
pub const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_EVENTS_PER_PAGE: usize = 200;
const MAX_NEXT_LINK_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarDateTime {
    pub value: String,
    pub time_zone: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CalendarEvent {
    pub id: String,
    pub subject: String,
    pub start: Option<CalendarDateTime>,
    pub end: Option<CalendarDateTime>,
    pub organizer: Option<String>,
    pub is_online: bool,
    pub join_url: Option<String>,
    pub all_day: bool,
    pub cancelled: bool,
    pub location: Option<String>,
    pub preview: Option<String>,
    /// Shown until a later agenda reload lists the same meeting.
    pub provisional: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CalendarPage {
    pub events: Vec<CalendarEvent>,
    pub next_link: Option<String>,
}

pub fn valid_teams_join_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value.trim()) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.fragment().is_none()
        && (host == "teams.microsoft.com"
            || host == "teams.live.com"
            || host.ends_with(".teams.microsoft.com")
            || host.ends_with(".teams.live.com"))
}

impl CalendarEvent {
    pub fn date_key(&self) -> Option<String> {
        let start = self.start.as_ref()?;
        if self.all_day {
            return date_prefix(&start.value).map(str::to_owned);
        }
        if let Some(local) = local_datetime(start) {
            return Some(local.format("%Y-%m-%d").to_string());
        }
        date_prefix(&start.value).map(str::to_owned)
    }

    pub fn date_heading(&self) -> Option<String> {
        let key = self.date_key()?;
        let date = NaiveDate::parse_from_str(&key, "%Y-%m-%d").ok()?;
        let today = Local::now().date_naive();
        match date.signed_duration_since(today).num_days() {
            0 => return Some(tr("Today").into()),
            1 => return Some(tr("Tomorrow").into()),
            _ => {}
        }
        let weekday = match date.weekday().num_days_from_monday() {
            0 => tr("Monday"),
            1 => tr("Tuesday"),
            2 => tr("Wednesday"),
            3 => tr("Thursday"),
            4 => tr("Friday"),
            5 => tr("Saturday"),
            _ => tr("Sunday"),
        };
        let month = match date.month() {
            1 => tr("January"),
            2 => tr("February"),
            3 => tr("March"),
            4 => tr("April"),
            5 => tr("May"),
            6 => tr("June"),
            7 => tr("July"),
            8 => tr("August"),
            9 => tr("September"),
            10 => tr("October"),
            11 => tr("November"),
            _ => tr("December"),
        };
        Some(crate::i18n::format("{weekday}, {month} {day}", &[
            ("weekday", &weekday), ("month", &month), ("day", &date.day()),
        ]))
    }

    /// Local start and end minutes, for the day column in the create card.
    pub fn local_span(&self) -> Option<(u16, u16)> {
        if self.all_day {
            return None;
        }
        let start = local_datetime(self.start.as_ref()?)?;
        let start_minute = u16::try_from(start.hour()).ok()? * 60
            + u16::try_from(start.minute()).unwrap_or(0);
        let end_minute = self
            .end
            .as_ref()
            .and_then(local_datetime)
            .and_then(|end| {
                let minute =
                    u16::try_from(end.hour()).ok()? * 60 + u16::try_from(end.minute()).unwrap_or(0);
                (minute > start_minute).then_some(minute)
            })
            .unwrap_or(start_minute.saturating_add(30));
        Some((start_minute, end_minute))
    }

    pub fn temporal_label(&self,now:DateTime<Local>)->Option<&'static str>{
        if self.cancelled{return Some("Cancelada");}
        if self.all_day{return None;}
        let start=self.start.as_ref().and_then(local_datetime)?;
        let end=self.end.as_ref().and_then(local_datetime)?;
        if end<=start{return None;}
        Some(if now<start {"Agendada"}else if now<end {"Agora"}else{"Encerrada"})
    }

    pub fn time_range(&self) -> Option<String> {
        if self.all_day {
            return Some("Dia inteiro".into());
        }
        let start = self.start.as_ref()?;
        let start_text = format_time(start)?;
        let end_text = self.end.as_ref().and_then(format_time);
        Some(match end_text {
            Some(end_text) => format!("{start_text} – {end_text}"),
            None => start_text,
        })
    }
}

fn date_prefix(value: &str) -> Option<&str> {
    let date = value.get(..10)?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(date)
}

fn local_datetime(value: &CalendarDateTime) -> Option<DateTime<Local>> {
    if let Ok(offset) = DateTime::parse_from_rfc3339(&value.value) {
        return Some(offset.with_timezone(&Local));
    }
    let time_zone = value.time_zone.as_deref().unwrap_or_default();
    if !matches!(time_zone, "UTC" | "Etc/UTC" | "Coordinated Universal Time") {
        return None;
    }
    let naive = NaiveDateTime::parse_from_str(&value.value, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    Some(Utc.from_utc_datetime(&naive).with_timezone(&Local))
}

fn format_time(value: &CalendarDateTime) -> Option<String> {
    if let Some(local) = local_datetime(value) {
        return Some(local.format("%H:%M").to_string());
    }
    let time = value.value.split_once('T')?.1;
    let hour_minute = time.get(..5)?;
    if NaiveDateTime::parse_from_str(&format!("2000-01-01T{hour_minute}:00"), "%Y-%m-%dT%H:%M:%S")
        .is_err()
    {
        return None;
    }
    Some(match value.time_zone.as_deref() {
        Some(zone) => format!("{hour_minute} · {zone}"),
        None => hour_minute.to_owned(),
    })
}

pub fn demo_events() -> Vec<CalendarEvent> {
    let first = Local::now()
        .date_naive()
        .and_hms_opt(9, 30, 0)
        .and_then(|date| Local.from_local_datetime(&date).earliest())
        .unwrap_or_else(Local::now);
    let second = Local::now()
        .date_naive()
        .succ_opt()
        .unwrap_or_else(|| Local::now().date_naive())
        .and_hms_opt(10, 30, 0)
        .map(|time| Local.from_local_datetime(&time).single())
        .flatten()
        .unwrap_or(first + chrono::Duration::days(1));
    let mut events = vec![
        CalendarEvent {
            id: "demo-calendar-1".into(),
            subject: "Alinhamento do projeto".into(),
            start: Some(CalendarDateTime {
                value: first.to_rfc3339(),
                time_zone: Some(tr("Local time").into()),
            }),
            end: Some(CalendarDateTime {
                value: (first + chrono::Duration::minutes(30)).to_rfc3339(),
                time_zone: Some(tr("Local time").into()),
            }),
            organizer: Some("Mariana Costa".into()),
            is_online: true,
            join_url: Some("https://teams.microsoft.com/l/meetup-join/demo".into()),
            all_day: false,
            cancelled: false,
            location: Some("Microsoft Teams".into()),
            preview: Some(tr("Review deliverables and next steps.").into()),
            provisional: false,
        },
        CalendarEvent {
            id: "demo-calendar-2".into(),
            subject: tr("Weekly review").into(),
            start: Some(CalendarDateTime {
                value: second.to_rfc3339(),
                time_zone: Some(tr("Local time").into()),
            }),
            end: Some(CalendarDateTime {
                value: (second + chrono::Duration::hours(1)).to_rfc3339(),
                time_zone: Some(tr("Local time").into()),
            }),
            organizer: Some(tr("Product team").into()),
            is_online: true,
            join_url: None,
            all_day: false,
            cancelled: false,
            location: None,
            preview: None,
            provisional: false,
        },
    ];
    let mut overlap = events[0].clone();
    overlap.id = "demo-calendar-overlap".into();
    overlap.subject = tr("Interface review").into();
    overlap.start = Some(CalendarDateTime {
        value: (first + chrono::Duration::minutes(15)).to_rfc3339(),
        time_zone: Some(tr("Local time").into()),
    });
    overlap.end = Some(CalendarDateTime {
        value: (first + chrono::Duration::minutes(60)).to_rfc3339(),
        time_zone: Some(tr("Local time").into()),
    });
    let mut all_day = events[0].clone();
    all_day.id = "demo-calendar-all-day".into();
    all_day.subject = "Planejamento do trimestre".into();
    all_day.all_day = true;
    all_day.join_url = None;
    all_day.start = Some(CalendarDateTime {
        value: format!("{}T00:00:00", first.date_naive()),
        time_zone: None,
    });
    all_day.end = Some(CalendarDateTime {
        value: format!(
            "{}T00:00:00",
            first.date_naive() + chrono::Duration::days(1)
        ),
        time_zone: None,
    });
    let mut cancelled = events[0].clone();
    cancelled.id = "demo-calendar-cancelled".into();
    cancelled.subject = tr("Follow-up meeting").into();
    cancelled.cancelled = true;
    cancelled.start = Some(CalendarDateTime {
        value: (first + chrono::Duration::hours(5)).to_rfc3339(),
        time_zone: Some(tr("Local time").into()),
    });
    cancelled.end = Some(CalendarDateTime {
        value: (first + chrono::Duration::hours(6)).to_rfc3339(),
        time_zone: Some(tr("Local time").into()),
    });
    events.extend([overlap, all_day, cancelled]);
    events
}

/// Exact native calendar route recovered from original worker module 91960;
/// regional middleTier concatenation is module21826. No Graph credential here.
pub(crate) fn native_base(root: &str) -> Result<Url, String> {
    let mut base =
        Url::parse(root).map_err(|_| tr("The calendar server is invalid.").to_owned())?;
    let host = base.host_str().unwrap_or_default();
    if base.scheme() != "https"
        || base.port().is_some()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || !base.path().starts_with("/api/mt/")
        || !(host == "teams.microsoft.com"
            || host.ends_with(".teams.microsoft.com")
            || host == "teams.office.com"
            || host.ends_with(".teams.office.com"))
    {
        return Err(tr("Teams returned an unexpected calendar server.").into());
    }
    base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
    Ok(base)
}

pub fn native_view_url(
    root: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    skip: usize,
) -> Result<String, String> {
    let base = native_base(root)?;
    let mut url = base
        .join("v2.1/me/calendars/calendarView")
        .map_err(|_| tr("The calendar URL is invalid.").to_owned())?;
    url.query_pairs_mut()
        .append_pair(
            "startDate",
            &start.to_rfc3339_opts(SecondsFormat::Secs, true),
        )
        .append_pair("endDate", &end.to_rfc3339_opts(SecondsFormat::Secs, true))
        .append_pair("$top", &PAGE_SIZE.to_string())
        .append_pair("$count", "true")
        .append_pair("$skip", &skip.min(10_000).to_string())
        .append_pair("$orderby", "startTime asc");
    Ok(url.into())
}

pub fn validate_native_page(
    root: &str,
    link: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<String, String> {
    if link.len() > MAX_NEXT_LINK_BYTES {
        return Err(tr("The calendar link is too long.").into());
    }
    let reference = native_view_url(root, start, end, 0)?;
    let url = Url::parse(link).map_err(|_| tr("The calendar link is invalid.").to_owned())?;
    let pairs: Vec<_> = url.query_pairs().collect();
    let skip: usize = pairs
        .iter()
        .find(|(key, _)| key == "$skip")
        .and_then(|(_, value)| value.parse().ok())
        .filter(|skip| *skip <= 10_000)
        .ok_or(tr("The calendar page is invalid."))?;
    // Rebuild, then compare: no foreign host, path, date range, extra/duplicate
    // parameters or arbitrary server nextLink can receive account credentials.
    let expected = native_view_url(root, start, end, skip)?;
    let parsed_reference = Url::parse(&reference).unwrap();
    let expected_url = Url::parse(&expected).unwrap();
    if url.origin() != parsed_reference.origin()
        || url.path() != parsed_reference.path()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || pairs.len() != expected_url.query_pairs().count()
        || !expected_url
            .query_pairs()
            .all(|pair| pairs.iter().filter(|other| **other == pair).count() == 1)
    {
        return Err(tr("The next-page link is for a different calendar.").into());
    }
    Ok(expected)
}

pub fn parse_native_page(
    json: &str,
    root: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    skip: usize,
) -> Result<CalendarPage, String> {
    if json.len() > MAX_RESPONSE_BYTES {
        return Err(tr("The calendar response exceeds the local limit.").into());
    }
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|_| tr("The calendar returned an invalid response.").to_owned())?;
    let items = value
        .get("value")
        .and_then(serde_json::Value::as_array)
        .ok_or(tr("The calendar did not return a list of events."))?;
    if items.len() > MAX_EVENTS_PER_PAGE {
        return Err(tr("The calendar page exceeds the local event limit.").into());
    }
    let events = items
        .iter()
        .map(parse_native_event)
        .collect::<Result<Vec<_>, _>>()?;
    let total = value
        .get("@odata.count")
        .and_then(serde_json::Value::as_u64);
    let loaded = skip.saturating_add(items.len());
    let more = !items.is_empty()
        && loaded < 10_000
        && total.map_or(items.len() >= PAGE_SIZE, |total| (loaded as u64) < total);
    let next_link = more
        .then(|| native_view_url(root, start, end, loaded))
        .transpose()?;
    Ok(CalendarPage { events, next_link })
}

/// Overlapping pages may contain a newer copy of an event (including its
/// cancellation). Update by opaque identity without duplicating or losing it.
pub fn merge_events(
    existing: Vec<CalendarEvent>,
    incoming: Vec<CalendarEvent>,
) -> Vec<CalendarEvent> {
    let mut positions = std::collections::HashMap::new();
    let mut events = Vec::with_capacity(existing.len() + incoming.len());
    for event in existing.into_iter().chain(incoming) {
        if event.id.is_empty() {
            continue;
        }
        if let Some(index) = positions.get(&event.id) {
            events[*index] = event;
        } else {
            positions.insert(event.id.clone(), events.len());
            events.push(event);
        }
    }
    events
}

/// Shared event normalization for calendarView and meeting-link lookup.
pub(crate) fn parse_native_event(item: &serde_json::Value) -> Result<CalendarEvent, String> {
    let field = |value: &serde_json::Value, name: &str, limit| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.chars().take(limit).collect::<String>())
    };
    let id =
        field(item, "objectId", 4096).ok_or(tr("The calendar event has no ID."))?;
    let all_day = item
        .get("isAllDayEvent")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let date_time = |name| {
        field(item, name, 100).map(|value| {
            let value = if all_day {
                DateTime::parse_from_rfc3339(&value)
                    .ok()
                    .map(|date| {
                        date.with_timezone(&Local)
                            .format("%Y-%m-%dT00:00:00")
                            .to_string()
                    })
                    .unwrap_or(value)
            } else {
                value
            };
            CalendarDateTime {
                value,
                time_zone: field(item, "eventTimeZone", 100),
            }
        })
    };
    let join_url = [
        "joinOnlineMeetingUrl",
        "skypeTeamsMeetingUrl",
        "shortOnlineMeetingJoinUrl",
    ]
    .into_iter()
    .filter_map(|name| field(item, name, 16 * 1024))
    .find(|link| valid_teams_join_url(link));
    let body = field(item, "bodyContent", 8000)
        .map(|body| {
            if item
                .get("bodyContentType")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("html"))
            {
                scraper::Html::parse_fragment(&body)
                    .root_element()
                    .text()
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                body
            }
        })
        .map(|body| body.chars().take(4000).collect());
    Ok(CalendarEvent {
        id,
        subject: field(item, "subject", 512).unwrap_or_else(|| tr("Untitled").into()),
        start: date_time("startTime"),
        end: date_time("endTime"),
        organizer: field(item, "organizerName", 256),
        location: field(item, "location", 512),
        is_online: item
            .get("isOnlineMeeting")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(join_url.is_some()),
        cancelled: item
            .get("isCancelled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        all_day,
        join_url,
        preview: body,
        provisional: false,
    })
}

/// A calendar meeting that is happening now and whose join link is this chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveMeeting {
    pub subject: String,
    pub join_url: String,
    pub elapsed_secs: u64,
    pub participants: Vec<String>,
    pub participant_count: usize,
}

pub fn format_elapsed(secs: u64) -> String {
    let secs = secs.min(99 * 3600 + 59 * 60 + 59);
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The meetup-join thread, when the link names one. Other invite shapes stay out.
pub fn join_thread_id(value: &str) -> Option<String> {
    if !valid_teams_join_url(value) {
        return None;
    }
    let url = Url::parse(value.trim()).ok()?;
    let encoded = url.path().strip_prefix("/l/meetup-join/")?.split('/').next()?;
    let thread = urlencoding::decode(encoded).ok()?.into_owned();
    (thread.starts_with("19:")
        && thread.ends_with("@thread.v2")
        && thread.len() <= 2048
        && !thread
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '#')))
        .then_some(thread)
}

/// The calendar occurrence tied to this chat, including one that is not happening now.
/// Recurring series keep several copies; the start closest to `now` wins.
pub fn event_for_thread<'a>(
    events: &'a [CalendarEvent],
    thread_id: &str,
    now: DateTime<Local>,
) -> Option<&'a CalendarEvent> {
    if thread_id.is_empty() {
        return None;
    }
    let mut best: Option<(i64, &CalendarEvent)> = None;
    for event in events {
        if event.cancelled {
            continue;
        }
        let Some(url) = event.join_url.as_deref() else {
            continue;
        };
        let Some(thread) = join_thread_id(url) else {
            continue;
        };
        if !thread.eq_ignore_ascii_case(thread_id) {
            continue;
        }
        let distance = event
            .start
            .as_ref()
            .and_then(local_datetime)
            .map(|start| (start - now).num_seconds().abs())
            .unwrap_or(i64::MAX);
        if best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, event));
        }
    }
    best.map(|(_, event)| event)
}

pub fn live_meeting(
    events: &[CalendarEvent],
    thread_id: &str,
    now: DateTime<Local>,
) -> Option<LiveMeeting> {
    if thread_id.is_empty() {
        return None;
    }
    let mut best: Option<(DateTime<Local>, LiveMeeting)> = None;
    for event in events {
        if event.cancelled || event.all_day {
            continue;
        }
        let Some(url) = event.join_url.as_deref() else {
            continue;
        };
        let Some(thread) = join_thread_id(url) else {
            continue;
        };
        if !thread.eq_ignore_ascii_case(thread_id) {
            continue;
        }
        let Some(start) = event.start.as_ref().and_then(local_datetime) else {
            continue;
        };
        let Some(end) = event.end.as_ref().and_then(local_datetime) else {
            continue;
        };
        if now < start || now >= end {
            continue;
        }
        let elapsed = (now - start).num_seconds().max(0) as u64;
        let replace = match &best {
            None => true,
            Some((best_start, _)) => start > *best_start,
        };
        if replace {
            best = Some((
                start,
                LiveMeeting {
                    subject: event.subject.clone(),
                    join_url: url.to_owned(),
                    elapsed_secs: elapsed,
                    participants: Vec::new(),
                    participant_count: 0,
                },
            ));
        }
    }
    best.map(|(_, meeting)| meeting)
}

/// Resolve a live-start signal only when exactly one nearby calendar entry
/// has the same validated chat thread. Ambiguous recurring entries are not
/// guessed; a schedule alone must never generate a start notification.
pub fn meeting_for_start_signal<'a>(events: &'a [CalendarEvent], thread_id: &str, now: DateTime<Local>) -> Option<&'a CalendarEvent> {
    let mut found = None;
    for event in events {
        if event.cancelled || event.all_day { continue; }
        let Some(url) = event.join_url.as_deref() else { continue; };
        if !join_thread_id(url).is_some_and(|thread| thread.eq_ignore_ascii_case(thread_id)) { continue; }
        let (Some(start), Some(end)) = (event.start.as_ref().and_then(local_datetime), event.end.as_ref().and_then(local_datetime)) else { continue; };
        let (Some(earliest), Some(latest)) = (start.checked_sub_signed(chrono::Duration::minutes(15)), end.checked_add_signed(chrono::Duration::minutes(5))) else { continue; };
        if end <= start || now < earliest || now >= latest { continue; }
        if found.is_some() { return None; }
        found = Some(event);
    }
    found
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleShowAs {
    Free,
    Tentative,
    Busy,
    Oof,
    Elsewhere,
}

impl ScheduleShowAs {
    pub const ALL: [Self; 5] = [
        Self::Free,
        Self::Tentative,
        Self::Busy,
        Self::Oof,
        Self::Elsewhere,
    ];

    fn as_graph(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Tentative => "tentative",
            Self::Busy => "busy",
            Self::Oof => "oof",
            Self::Elsewhere => "workingElsewhere",
        }
    }
}

impl std::fmt::Display for ScheduleShowAs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Free => tr("Free"),
            Self::Tentative => tr("Tentative"),
            Self::Busy => tr("Busy"),
            Self::Oof => tr("Away"),
            Self::Elsewhere => "Trabalhando em outro lugar",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleRepeat {
    Once,
    Daily,
    Weekly,
    Weekdays,
}

impl ScheduleRepeat {
    pub const ALL: [Self; 4] = [Self::Once, Self::Daily, Self::Weekly, Self::Weekdays];
}

impl std::fmt::Display for ScheduleRepeat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Once => tr("Series"),
            Self::Daily => tr("Daily"),
            Self::Weekly => tr("Weekly"),
            Self::Weekdays => tr("Weekdays"),
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ScheduleAttendee {
    pub name: String,
    pub email: String,
    pub required: bool,
}

impl std::fmt::Debug for ScheduleAttendee {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleAttendee")
            .field("required", &self.required)
            .finish()
    }
}

/// Graph `POST /me/calendar/events` inputs. Times are UTC
/// `YYYY-MM-DDTHH:MM:SS`, the shape `validate_schedule` already accepts.
#[derive(Clone, PartialEq, Eq)]
pub struct ScheduleRequest {
    pub subject: String,
    pub start: String,
    pub end: String,
    pub time_zone: &'static str,
    pub online: bool,
    pub attendees: Vec<ScheduleAttendee>,
    pub location: String,
    pub body: String,
    pub show_as: ScheduleShowAs,
    pub repeat: ScheduleRepeat,
    pub repeat_on: NaiveDate,
    pub repeat_until: Option<NaiveDate>,
}

impl std::fmt::Debug for ScheduleRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleRequest")
            .field("online", &self.online)
            .field("attendees", &self.attendees.len())
            .field("show_as", &self.show_as)
            .field("repeat", &self.repeat)
            .finish()
    }
}

pub fn attendee_from_query(query: &str) -> Result<ScheduleAttendee, String> {
    let query = query.trim();
    let (name, email) = if let Some((name, rest)) = query.rsplit_once('<') {
        (name.trim(), rest.trim().trim_end_matches('>').trim())
    } else {
        (query, query)
    };
    attendee_from_person(name, email)
}

pub fn attendee_from_person(name: &str, email: &str) -> Result<ScheduleAttendee, String> {
    let email = email.trim();
    if !valid_email(email) {
        return Err(tr("Enter a valid email address to invite someone.").into());
    }
    let name = name.trim();
    let name = if name.is_empty() || name.eq_ignore_ascii_case(email) {
        email
    } else {
        name
    };
    if name.chars().count() > 120 || name.chars().any(|c| c.is_control()) {
        return Err(tr("The attendee name is invalid.").into());
    }
    Ok(ScheduleAttendee {
        name: name.to_owned(),
        email: email.to_owned(),
        required: true,
    })
}

pub fn push_attendee(
    attendees: &mut Vec<ScheduleAttendee>,
    person: ScheduleAttendee,
) -> Result<(), String> {
    if attendees.len() >= 25 {
        return Err(tr("A meeting can have up to 25 attendees.").into());
    }
    if attendees
        .iter()
        .any(|existing| existing.email.eq_ignore_ascii_case(&person.email))
    {
        return Err(tr("This attendee is already invited.").into());
    }
    attendees.push(person);
    Ok(())
}

fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && email.len() <= 254
        && email.is_ascii()
        && email.chars().all(|c| !c.is_whitespace() && !c.is_control())
        && local.chars().all(|c| c != '<' && c != '>')
        && domain.chars().all(|c| c != '<' && c != '>')
}

pub fn insert_agenda(body: &str) -> String {
    if body
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("agenda"))
    {
        return body.to_owned();
    }
    let mut next = body.trim_end().to_owned();
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str("Agenda\n");
    next
}

pub fn parse_schedule_date(value: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| tr("Enter a date in YYYY-MM-DD format.").to_owned())
}

fn parse_clock(value: &str) -> Result<(u32, u32), String> {
    let Some((hour, minute)) = value.split_once(':') else {
        return Err(tr("Enter a time in HH:MM format.").into());
    };
    if hour.len() != 2
        || minute.len() != 2
        || !hour.chars().all(|c| c.is_ascii_digit())
        || !minute.chars().all(|c| c.is_ascii_digit())
    {
        return Err(tr("Enter a time in HH:MM format.").into());
    }
    let hour: u32 = hour
        .parse()
        .map_err(|_| tr("Enter a time in HH:MM format.").to_owned())?;
    let minute: u32 = minute
        .parse()
        .map_err(|_| tr("Enter a time in HH:MM format.").to_owned())?;
    if hour > 23 || minute > 59 {
        return Err(tr("Enter a time in HH:MM format.").into());
    }
    Ok((hour, minute))
}

fn local_wall(date: NaiveDate, hour: u32, minute: u32) -> Result<DateTime<Local>, String> {
    date.and_hms_opt(hour, minute, 0)
        .and_then(|value| Local.from_local_datetime(&value).earliest())
        .ok_or_else(|| tr("Could not convert the local time.").to_owned())
}

pub fn clock_minutes(value: &str) -> Option<u16> {
    let (hour, minute) = value.split_once(':')?;
    if hour.len() != 2
        || minute.len() != 2
        || !hour.chars().all(|c| c.is_ascii_digit())
        || !minute.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let hour: u16 = hour.parse().ok()?;
    let minute: u16 = minute.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(hour * 60 + minute)
}

pub fn schedule_day_label(value: &str) -> String {
    let Ok(date) = parse_schedule_date(value) else {
        return value.to_owned();
    };
    let weekday = [tr("Mon"), tr("Tue"), tr("Wed"), tr("Thu"), tr("Fri"), tr("Sat"), tr("Sun")]
        [date.weekday().num_days_from_monday() as usize];
    format!(
        "{weekday}, {:02}/{:02}/{}",
        date.day(),
        date.month(),
        date.year()
    )
}

/// Heading of the day column: `Qui, 05 Nov, 2026`.
pub fn schedule_preview_heading(value: &str) -> String {
    let Ok(date) = parse_schedule_date(value) else {
        return value.to_owned();
    };
    let weekday = [tr("Mon"), tr("Tue"), tr("Wed"), tr("Thu"), tr("Fri"), tr("Sat"), tr("Sun")]
        [date.weekday().num_days_from_monday() as usize];
    let month = [
        tr("Jan"), tr("Feb"), tr("Mar"), tr("Apr"), tr("May"), tr("Jun"),
        tr("Jul"), tr("Aug"), tr("Sep"), tr("Oct"), tr("Nov"), tr("Dec"),
    ][date.month() as usize - 1];
    format!("{weekday}, {:02} {month}, {}", date.day(), date.year())
}

/// Thirty-minute cell on the day grid. `start_minute` is 0, 30, 60, and so on.
pub fn schedule_slot(date: NaiveDate, start_minute: u16) -> Result<(String, String, String), String> {
    if start_minute % 30 != 0 || start_minute >= 24 * 60 {
        return Err(tr("The time is outside the grid.").into());
    }
    let end_minute = u32::from(start_minute) + 30;
    let start = format!("{:02}:{:02}", start_minute / 60, start_minute % 60);
    let end = if end_minute >= 24 * 60 {
        "00:00".to_owned()
    } else {
        format!("{:02}:{:02}", end_minute / 60, end_minute % 60)
    };
    Ok((date.format("%Y-%m-%d").to_string(), start, end))
}

/// Next half-hour on the selected day, kept on that day. Another day starts at 09:00.
pub fn default_schedule_fields(anchor: NaiveDate, now: DateTime<Local>) -> (String, String, String) {
    let (hour, minute) = if anchor == now.date_naive() {
        let total = now.hour() * 60 + now.minute();
        let next = (total / 30 + 1) * 30;
        if next + 30 >= 24 * 60 {
            (23, 0)
        } else {
            (next / 60, next % 60)
        }
    } else {
        (9, 0)
    };
    let end = hour * 60 + minute + 30;
    (
        anchor.format("%Y-%m-%d").to_string(),
        format!("{hour:02}:{minute:02}"),
        format!("{:02}:{:02}", end / 60, end % 60),
    )
}

pub fn prepare_schedule(
    subject: &str,
    date: NaiveDate,
    start_hm: &str,
    end_hm: &str,
    online: bool,
) -> Result<ScheduleRequest, String> {
    let subject = subject.trim();
    if subject.is_empty() {
        return Err(tr("Enter a meeting title.").into());
    }
    if subject.chars().count() > 120 || subject.chars().any(|c| c.is_control()) {
        return Err(tr("The title can contain up to 120 characters.").into());
    }
    let (start_hour, start_minute) = parse_clock(start_hm)?;
    let (end_hour, end_minute) = parse_clock(end_hm)?;
    let start_local = local_wall(date, start_hour, start_minute)?;
    let mut end_date = date;
    if (end_hour, end_minute) <= (start_hour, start_minute) {
        end_date = end_date
            .succ_opt()
            .ok_or_else(|| "Data fora do intervalo.".to_owned())?;
    }
    let end_local = local_wall(end_date, end_hour, end_minute)?;
    let minutes = end_local.signed_duration_since(start_local).num_minutes();
    if minutes <= 0 {
        return Err(tr("The end time must be after the start time.").into());
    }
    if minutes > 12 * 60 {
        return Err(tr("A meeting can last up to 12 hours.").into());
    }
    let start = start_local
        .with_timezone(&Utc)
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let end = end_local
        .with_timezone(&Utc)
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    ost::api::validate_schedule(subject, &start, &end, "UTC")
        .map_err(|_| tr("Check the meeting title and time.").to_owned())?;
    Ok(ScheduleRequest {
        subject: subject.to_owned(),
        start,
        end,
        time_zone: "UTC",
        online,
        attendees: Vec::new(),
        location: String::new(),
        body: String::new(),
        show_as: ScheduleShowAs::Busy,
        repeat: ScheduleRepeat::Once,
        repeat_on: date,
        repeat_until: None,
    })
}

pub fn attach_details(
    request: &mut ScheduleRequest,
    attendees: Vec<ScheduleAttendee>,
    location: &str,
    body: &str,
    show_as: ScheduleShowAs,
    repeat: ScheduleRepeat,
    repeat_until: Option<NaiveDate>,
) -> Result<(), String> {
    if attendees.len() > 25 {
        return Err(tr("A meeting can have up to 25 attendees.").into());
    }
    let mut seen = std::collections::HashSet::new();
    for person in &attendees {
        if !valid_email(&person.email) || !seen.insert(person.email.to_ascii_lowercase()) {
            return Err(tr("Check the meeting attendees.").into());
        }
    }
    let location = location.trim();
    if location.chars().count() > 200 || location.chars().any(|c| c.is_control()) {
        return Err(tr("The location can contain up to 200 characters.").into());
    }
    let body = body.trim();
    if body.chars().count() > 4000
        || body
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(tr("The description can contain up to 4000 characters.").into());
    }
    let repeat_until = if repeat == ScheduleRepeat::Once {
        None
    } else {
        let until = repeat_until.ok_or(tr("Enter when the series ends."))?;
        if until < request.repeat_on {
            return Err(tr("The series must end on or after its start date.").into());
        }
        Some(until)
    };
    request.attendees = attendees;
    request.location = location.to_owned();
    request.body = body.to_owned();
    request.show_as = show_as;
    request.repeat = repeat;
    request.repeat_until = repeat_until;
    Ok(())
}

pub fn graph_event_body(request: &ScheduleRequest) -> serde_json::Value {
    let mut body = ost::api::schedule_event_body(
        &request.subject,
        &request.start,
        &request.end,
        request.time_zone,
        request.online,
    );
    body["showAs"] = serde_json::json!(request.show_as.as_graph());
    if !request.location.is_empty() {
        body["location"] = serde_json::json!({ "displayName": request.location });
    }
    if !request.body.is_empty() {
        body["body"] = serde_json::json!({
            "contentType": "text",
            "content": request.body,
        });
    }
    if !request.attendees.is_empty() {
        body["attendees"] = serde_json::json!(
            request
                .attendees
                .iter()
                .map(|person| {
                    serde_json::json!({
                        "emailAddress": {"address": person.email, "name": person.name},
                        "type": if person.required { "required" } else { "optional" },
                    })
                })
                .collect::<Vec<_>>()
        );
    }
    if let Some(recurrence) = recurrence_body(request) {
        body["recurrence"] = recurrence;
    }
    body
}

fn recurrence_body(request: &ScheduleRequest) -> Option<serde_json::Value> {
    let until = request.repeat_until?;
    if request.repeat == ScheduleRepeat::Once {
        return None;
    }
    let pattern = match request.repeat {
        ScheduleRepeat::Once => return None,
        ScheduleRepeat::Daily => serde_json::json!({ "type": "daily", "interval": 1 }),
        ScheduleRepeat::Weekly => serde_json::json!({
            "type": "weekly",
            "interval": 1,
            "daysOfWeek": [graph_weekday(request.repeat_on)],
            "firstDayOfWeek": "sunday",
        }),
        ScheduleRepeat::Weekdays => serde_json::json!({
            "type": "weekly",
            "interval": 1,
            "daysOfWeek": ["monday", "tuesday", "wednesday", "thursday", "friday"],
            "firstDayOfWeek": "sunday",
        }),
    };
    Some(serde_json::json!({
        "pattern": pattern,
        "range": {
            "type": "endDate",
            "startDate": request.repeat_on.format("%Y-%m-%d").to_string(),
            "endDate": until.format("%Y-%m-%d").to_string(),
            "recurrenceTimeZone": "UTC",
        },
    }))
}

fn graph_weekday(date: NaiveDate) -> &'static str {
    match date.weekday() {
        chrono::Weekday::Mon => "monday",
        chrono::Weekday::Tue => "tuesday",
        chrono::Weekday::Wed => "wednesday",
        chrono::Weekday::Thu => "thursday",
        chrono::Weekday::Fri => "friday",
        chrono::Weekday::Sat => "saturday",
        chrono::Weekday::Sun => "sunday",
    }
}

pub fn scheduled_event(
    request: &ScheduleRequest,
    id: &str,
    join_url: Option<&str>,
) -> Result<CalendarEvent, String> {
    let id = id.trim();
    if id.is_empty()
        || id.len() > 256
        || id.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(tr("The meeting was created, but Teams returned an invalid ID.").into());
    }
    let join_url = join_url
        .map(str::trim)
        .filter(|url| valid_teams_join_url(url))
        .map(str::to_owned);
    Ok(CalendarEvent {
        id: format!("scheduled:{id}"),
        subject: request.subject.clone(),
        start: Some(CalendarDateTime {
            value: request.start.clone(),
            time_zone: Some(request.time_zone.into()),
        }),
        end: Some(CalendarDateTime {
            value: request.end.clone(),
            time_zone: Some(request.time_zone.into()),
        }),
        organizer: None,
        is_online: request.online,
        join_url,
        all_day: false,
        cancelled: false,
        location: if request.location.is_empty() {
            request.online.then(|| "Microsoft Teams".into())
        } else {
            Some(request.location.clone())
        },
        preview: (!request.body.is_empty()).then(|| request.body.chars().take(280).collect()),
        provisional: true,
    })
}

pub fn same_slot(left: &CalendarEvent, right: &CalendarEvent) -> bool {
    left.subject.trim().eq_ignore_ascii_case(right.subject.trim())
        && left.date_key() == right.date_key()
        && left.time_range() == right.time_range()
}

/// Keeps a meeting we just created when the following reload has not listed it yet.
pub fn retain_provisional(
    previous: Vec<CalendarEvent>,
    mut incoming: Vec<CalendarEvent>,
) -> Vec<CalendarEvent> {
    for held in previous.into_iter().filter(|event| event.provisional) {
        if !incoming.iter().any(|event| same_slot(event, &held)) {
            incoming.push(held);
        }
    }
    incoming
}

#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub event: usize,
    pub start: u16,
    pub end: u16,
    pub lane: usize,
    pub lanes: usize,
}

pub fn on_date(event: &CalendarEvent, date: NaiveDate) -> bool {
    if event.all_day {
        let Some(start) = event
            .start
            .as_ref()
            .and_then(|v| date_prefix(&v.value))
            .and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok())
        else {
            return false;
        };
        let end = event
            .end
            .as_ref()
            .and_then(|v| date_prefix(&v.value))
            .and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok())
            .unwrap_or(start + chrono::Duration::days(1));
        return date >= start && date < end;
    }
    minute_span(event, date).is_some()
}

fn minute_span(event: &CalendarEvent, date: NaiveDate) -> Option<(u16, u16)> {
    use chrono::Timelike;
    if event.all_day {
        return None;
    }
    let start = local_datetime(event.start.as_ref()?)?;
    let end = event
        .end
        .as_ref()
        .and_then(local_datetime)
        .filter(|end| end > &start)
        .unwrap_or(start + chrono::Duration::minutes(30));
    if date < start.date_naive()
        || date > end.date_naive()
        || (date == end.date_naive() && end.time().num_seconds_from_midnight() == 0)
    {
        return None;
    }
    let first = if start.date_naive() < date {
        0
    } else {
        (start.hour() * 60 + start.minute()) as u16
    };
    let last = if end.date_naive() > date {
        1440
    } else {
        (end.hour() * 60 + end.minute()) as u16
    };
    Some((first, last.max(first + 1).min(1440)))
}

/// Sweep connected overlap clusters. Adjacent events can reuse a lane; all
/// events in a connected cluster share its width to avoid hiding later events.
pub fn placements(events: &[CalendarEvent], date: NaiveDate) -> Vec<Placement> {
    let mut placed: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(event, item)| {
            let (start, end) = minute_span(item, date)?;
            Some(Placement {
                event,
                start,
                end: end.max(start.saturating_add(24)).min(1440),
                lane: 0,
                lanes: 1,
            })
        })
        .collect();
    placed.sort_by_key(|p| (p.start, p.end, p.event));
    let mut cluster = 0;
    while cluster < placed.len() {
        let mut end = cluster;
        let mut until = placed[cluster].end;
        while end < placed.len() && placed[end].start < until {
            until = until.max(placed[end].end);
            end += 1;
        }
        let mut lanes: Vec<u16> = Vec::new();
        for p in &mut placed[cluster..end] {
            let lane = lanes
                .iter()
                .position(|end| *end <= p.start)
                .unwrap_or(lanes.len());
            if lane == lanes.len() {
                lanes.push(p.end);
            } else {
                lanes[lane] = p.end;
            }
            p.lane = lane;
        }
        for p in &mut placed[cluster..end] {
            p.lanes = lanes.len();
        }
        cluster = end;
    }
    placed
}

pub fn date_label(date: NaiveDate) -> String {
    let months = [
        tr("January"),
        tr("February"),
        tr("March"),
        tr("April"),
        tr("May"),
        tr("June"),
        tr("July"),
        tr("August"),
        tr("September"),
        tr("October"),
        tr("November"),
        tr("December"),
    ];
    crate::i18n::format("{month} {day}, {year}", &[
        ("day", &date.day()), ("month", &months[date.month0() as usize]), ("year", &date.year()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "https://teams.microsoft.com/api/mt/amer/";
    fn window() -> (DateTime<Utc>, DateTime<Utc>) {
        (
            "2026-10-26T00:00:00Z".parse().unwrap(),
            "2026-11-02T00:00:00Z".parse().unwrap(),
        )
    }
    fn event_at(start: &str, end: &str, all_day: bool) -> CalendarEvent {
        CalendarEvent {
            id: start.into(),
            subject: "Evento".into(),
            all_day,
            start: Some(CalendarDateTime {
                value: start.into(),
                time_zone: Some("UTC".into()),
            }),
            end: Some(CalendarDateTime {
                value: end.into(),
                time_zone: Some("UTC".into()),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn repeated_page_updates_cancellation_without_duplicating_or_reordering_identity() {
        let mut original = demo_events().remove(0);
        let mut other = original.clone();
        other.id = "other".into();
        let first = vec![original.clone(), other.clone()];
        original.cancelled = true;
        original.subject = "Updated".into();
        let merged = merge_events(first, vec![original, other]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, "demo-calendar-1");
        assert!(merged[0].cancelled);
        assert_eq!(merged[0].subject, "Updated");
        assert_eq!(merged[1].id, "other");
    }

    #[test]
    fn week_navigation_crosses_month_and_year_without_changing_weekday() {
        let date = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        assert_eq!(
            CalendarView::WorkWeek.first_date(date).to_string(),
            "2026-12-28"
        );
        assert_eq!(
            CalendarView::Week.first_date(date).to_string(),
            "2026-12-28"
        );
        assert_eq!(CalendarView::Day.first_date(date), date);
        let (start, end) = range_for(date, CalendarView::Week).unwrap();
        assert!(end > start);
        assert_eq!(
            start.with_timezone(&Local).date_naive().to_string(),
            "2026-12-28"
        );
        assert_eq!(
            end.with_timezone(&Local).date_naive().to_string(),
            "2027-01-04"
        );
    }

    #[test]
    fn all_day_end_is_exclusive_and_does_not_shift_with_local_offset() {
        let event = event_at("2026-10-28T00:00:00", "2026-10-30T00:00:00", true);
        assert_eq!(event.date_key().as_deref(), Some("2026-10-28"));
        assert_eq!(event.time_range().as_deref(), Some("Dia inteiro"));
        assert!(on_date(
            &event,
            NaiveDate::from_ymd_opt(2026, 10, 29).unwrap()
        ));
        assert!(!on_date(
            &event,
            NaiveDate::from_ymd_opt(2026, 10, 30).unwrap()
        ));
        assert!(placements(&[event], NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()).is_empty());
    }

    #[test]
    fn overlapping_events_use_distinct_lanes_and_adjacent_events_reuse_one() {
        // RFC3339 carries the actual local offset; tests do not mutate process TZ.
        let date = Local::now().date_naive();
        let stamp = |hour, minute| {
            Local
                .from_local_datetime(&date.and_hms_opt(hour, minute, 0).unwrap())
                .earliest()
                .unwrap()
                .to_rfc3339()
        };
        let events = [
            event_at(&stamp(9, 0), &stamp(10, 0), false),
            event_at(&stamp(9, 30), &stamp(11, 0), false),
            event_at(&stamp(10, 0), &stamp(10, 30), false),
            event_at(&stamp(11, 0), &stamp(12, 0), false),
        ];
        let layout = placements(&events, date);
        assert_eq!(
            layout.iter().map(|p| (p.lane, p.lanes)).collect::<Vec<_>>(),
            vec![(0, 2), (1, 2), (0, 2), (0, 1)]
        );
        for (i, a) in layout.iter().enumerate() {
            for b in &layout[i + 1..] {
                if a.start < b.end && b.start < a.end {
                    assert_ne!(a.lane, b.lane);
                }
            }
        }
    }

    #[test]
    fn native_calendar_maps_original_fields_and_safe_join_link() {
        let (start, end) = window();
        let page = parse_native_page(&serde_json::json!({"value":[{
            "objectId":"event-fixture", "subject":"Revisão", "startTime":"2026-10-28T12:00:00Z", "endTime":"2026-10-28T13:00:00Z", "eventTimeZone":"UTC",
            "organizerName":"Ana", "isCancelled":true, "location":"Sala 4", "isOnlineMeeting":true,
            "joinOnlineMeetingUrl":"https://teams.microsoft.com/l/meetup-join/fixture/0",
            "bodyContent":"<p>Plano <b>semanal</b></p>", "bodyContentType":"html"
        }]}).to_string(),ROOT,start,end,0).unwrap();
        let event = &page.events[0];
        assert_eq!(event.id, "event-fixture");
        assert_eq!(event.organizer.as_deref(), Some("Ana"));
        assert!(event.cancelled);
        assert_eq!(event.location.as_deref(), Some("Sala 4"));
        assert!(event.join_url.is_some());
        assert!(!event.preview.as_ref().unwrap().contains('<'));
        assert!(page.next_link.is_none());
    }

    #[test]
    fn native_pagination_is_constructed_locally_and_bound_to_range_and_origin() {
        let (start, end) = window();
        let rows: Vec<_> = (0..50)
            .map(|id| serde_json::json!({"objectId":format!("event-{id}")}))
            .collect();
        let page = parse_native_page(
            &serde_json::json!({"value":rows,"@odata.count":100,
            "@odata.nextLink":"https://evil.test/steal"})
            .to_string(),
            ROOT,
            start,
            end,
            0,
        )
        .unwrap();
        let link = page.next_link.unwrap();
        assert!(link.contains("%24skip=50"));
        assert_eq!(validate_native_page(ROOT, &link, start, end).unwrap(), link);
        for bad in [
            link.replace("teams.microsoft.com", "evil.test"),
            format!("{link}&%24skip=1"),
            format!("{link}&redirect=evil"),
            link.replace("2026-10-26", "2026-10-25"),
            link.replace("calendarView", "other"),
        ] {
            assert!(validate_native_page(ROOT, &bad, start, end).is_err());
        }
        assert!(
            native_view_url(
                "https://teams.microsoft.com.evil.test/api/mt/amer/",
                start,
                end,
                0
            )
            .is_err()
        );
        assert!(native_view_url("http://teams.microsoft.com/api/mt/amer/", start, end, 0).is_err());
        assert!(native_view_url("https://teams.microsoft.com/api/other/", start, end, 0).is_err());
    }

    #[test]
    fn native_errors_and_limits_do_not_become_an_empty_agenda() {
        let (start, end) = window();
        for bad in [
            "not json".to_owned(),
            r#"{"error":{"code":"Denied"}}"#.into(),
            " ".repeat(MAX_RESPONSE_BYTES + 1),
            serde_json::json!({"value":[{}]}).to_string(),
            serde_json::json!({"value":vec![serde_json::json!({"objectId":"x"});201]}).to_string(),
        ] {
            assert!(parse_native_page(&bad, ROOT, start, end, 0).is_err());
        }
    }

    #[test]
    fn calendar_display_converts_utc_and_preserves_unknown_wall_clock_zone() {
        let utc = CalendarDateTime {
            value: "2026-10-27T16:30:00.0000000".into(),
            time_zone: Some("UTC".into()),
        };
        assert!(format_time(&utc).unwrap().contains(':'));
        let unknown = CalendarDateTime {
            value: "2026-10-27T09:15:00".into(),
            time_zone: Some("Pacific Standard Time".into()),
        };
        assert_eq!(
            format_time(&unknown).as_deref(),
            Some("09:15 · Pacific Standard Time")
        );
    }

    #[test]
    fn schedule_request_uses_utc_and_the_graph_online_meeting_shape() {
        let date = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        assert!(prepare_schedule("  ", date, "09:00", "09:30", true).is_err());
        assert!(prepare_schedule("Títu\nlo", date, "09:00", "09:30", true).is_err());
        assert!(prepare_schedule("Longa", date, "10:00", "09:00", true).is_err());
        let request = prepare_schedule("  Planejamento  ", date, "09:00", "09:30", true).unwrap();
        assert_eq!(request.subject, "Planejamento");
        assert!(request.start < request.end);
        assert_eq!(request.time_zone, "UTC");
        let body = ost::api::schedule_event_body(
            &request.subject,
            &request.start,
            &request.end,
            request.time_zone,
            request.online,
        );
        assert_eq!(body["isOnlineMeeting"], true);
        assert_eq!(body["onlineMeetingProvider"], "teamsForBusiness");
        assert_eq!(body["start"]["timeZone"], "UTC");
        let event = scheduled_event(
            &request,
            "graph-id",
            Some("https://evil.example/join"),
        )
        .unwrap();
        assert!(event.provisional);
        assert!(event.join_url.is_none());
        assert_eq!(event.id, "scheduled:graph-id");
        let listed = CalendarEvent {
            id: "server".into(),
            subject: "planejamento".into(),
            start: event.start.clone(),
            end: event.end.clone(),
            ..Default::default()
        };
        let kept = retain_provisional(vec![event.clone()], Vec::new());
        assert_eq!(kept.len(), 1);
        let replaced = retain_provisional(vec![event], vec![listed.clone()]);
        assert_eq!(replaced, vec![listed]);
        let mut request = request;
        attach_details(
            &mut request,
            vec![attendee_from_query("Ana <ana@example.com>").unwrap()],
            "Sala 2",
            "Pauta",
            ScheduleShowAs::Busy,
            ScheduleRepeat::Weekly,
            Some(NaiveDate::from_ymd_opt(2026, 12, 4).unwrap()),
        )
        .unwrap();
        let posted = graph_event_body(&request);
        assert_eq!(posted["attendees"][0]["emailAddress"]["address"], "ana@example.com");
        assert_eq!(posted["attendees"][0]["type"], "required");
        assert_eq!(posted["location"]["displayName"], "Sala 2");
        assert_eq!(posted["body"]["content"], "Pauta");
        assert_eq!(posted["showAs"], "busy");
        assert_eq!(posted["recurrence"]["pattern"]["type"], "weekly");
        assert_eq!(posted["recurrence"]["pattern"]["daysOfWeek"][0], "friday");
        assert!(attendee_from_query("sem-email").is_err());
        assert_eq!(insert_agenda("Pauta"), "Pauta\n\nAgenda\n");
        let previous_day = date.pred_opt().unwrap().and_hms_opt(12, 0, 0).unwrap();
        let fixed_now = Local.from_local_datetime(&previous_day).earliest().unwrap();
        let (day, start, end) = default_schedule_fields(date, fixed_now);
        assert_eq!((day.as_str(), start.as_str(), end.as_str()), ("2026-11-06", "09:00", "09:30"));
        assert_eq!(
            schedule_slot(date, 14 * 60 + 30).unwrap(),
            ("2026-11-06".into(), "14:30".into(), "15:00".into())
        );
        assert_eq!(
            schedule_slot(date, 23 * 60 + 30).unwrap().1,
            "23:30"
        );
        assert!(schedule_slot(date, 15).is_err());
    }

    #[test]
    fn live_meeting_matches_only_the_open_thread_during_its_window() {
        let thread = "19:meeting_allhands@thread.v2";
        let url = "https://teams.microsoft.com/l/meetup-join/19%3Ameeting_allhands%40thread.v2/0";
        let event = CalendarEvent {
            id: "evt".into(),
            subject: "All Hands".into(),
            start: Some(CalendarDateTime {
                value: "2026-11-05T18:00:00+00:00".into(),
                time_zone: None,
            }),
            end: Some(CalendarDateTime {
                value: "2026-11-05T19:00:00+00:00".into(),
                time_zone: None,
            }),
            is_online: true,
            join_url: Some(url.into()),
            ..CalendarEvent::default()
        };
        let now = DateTime::parse_from_rfc3339("2026-11-05T18:53:43+00:00")
            .unwrap()
            .with_timezone(&Local);
        let live = live_meeting(&[event.clone()], thread, now).unwrap();
        let later = event.clone();
        let mut sooner = event.clone();
        sooner.id = "sooner".into();
        sooner.start = Some(CalendarDateTime { value: "2026-11-05T18:50:00+00:00".into(), time_zone: None });
        let occurrences = [later, sooner];
        let picked = event_for_thread(&occurrences, thread, now).unwrap();
        assert_eq!(picked.id, "sooner");
        assert!(event_for_thread(std::slice::from_ref(&event), "19:other@thread.v2", now).is_none());
        assert_eq!(live.subject, "All Hands");
        assert_eq!(live.join_url, url);
        assert_eq!(live.elapsed_secs, 53 * 60 + 43);
        assert_eq!(format_elapsed(live.elapsed_secs), "53:43");
        assert_eq!(format_elapsed(3600 + 2 * 60 + 3), "1:02:03");
        let before = now - chrono::Duration::hours(2);
        assert!(live_meeting(&[event.clone()], thread, before).is_none());
        event_cancelled_and_other_thread(&event, thread, now);
    }

    fn event_cancelled_and_other_thread(event: &CalendarEvent, thread: &str, now: DateTime<Local>) {
        let mut cancelled = event.clone();
        cancelled.cancelled = true;
        assert!(live_meeting(&[cancelled], thread, now).is_none());
        assert!(live_meeting(&[event.clone()], "19:other@thread.v2", now).is_none());
        let mut all_day = event.clone();
        all_day.all_day = true;
        assert!(live_meeting(&[all_day], thread, now).is_none());
    }

    #[test]
    fn meeting_started_match_requires_one_nearby_exact_calendar_thread() {
        let now = DateTime::parse_from_rfc3339("2026-11-06T13:00:00Z").unwrap().with_timezone(&Local);
        let event = CalendarEvent {
            id: "fixture".into(), subject: "Reunião fictícia".into(), is_online: true,
            join_url: Some("https://teams.microsoft.com/l/meetup-join/19%3Ameeting_fixture%40thread.v2/0".into()),
            start: Some(CalendarDateTime { value:"2026-11-06T13:10:00Z".into(), time_zone:None }),
            end: Some(CalendarDateTime { value:"2026-11-06T13:30:00Z".into(), time_zone:None }),
            ..Default::default()
        };
        let thread = "19:meeting_fixture@thread.v2";
        assert!(meeting_for_start_signal(&[event.clone()], thread, now).is_some());
        assert!(meeting_for_start_signal(&[event.clone()], "19:other@thread.v2", now).is_none());
        assert!(meeting_for_start_signal(&[event.clone(), event.clone()], thread, now).is_none());
        assert!(meeting_for_start_signal(&[CalendarEvent { cancelled:true, ..event.clone() }], thread, now).is_none());
        assert!(meeting_for_start_signal(&[event], thread, now - chrono::Duration::minutes(20)).is_none());
    }
    #[test]
    fn temporal_labels_use_real_start_end_and_do_not_invent_response_status(){
        let now=Local::now();
        let mut event=CalendarEvent{start:Some(CalendarDateTime{value:(now-chrono::Duration::minutes(5)).to_rfc3339(),time_zone:None}),end:Some(CalendarDateTime{value:(now+chrono::Duration::minutes(25)).to_rfc3339(),time_zone:None}),..Default::default()};
        assert_eq!(event.temporal_label(now),Some("Agora"));
        assert_eq!(event.temporal_label(now-chrono::Duration::hours(1)),Some("Agendada"));
        assert_eq!(event.temporal_label(now+chrono::Duration::hours(1)),Some("Encerrada"));
        event.cancelled=true;assert_eq!(event.temporal_label(now),Some("Cancelada"));
        event.cancelled=false;event.end=None;assert_eq!(event.temporal_label(now),None);
    }

}
