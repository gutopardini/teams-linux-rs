//! Last-message labels use the viewer's local date, never the UTC date.
use chrono::{DateTime, FixedOffset};

pub fn label(value: Option<&str>, now: DateTime<FixedOffset>) -> Option<String> {
    let time = DateTime::parse_from_rfc3339(value?).ok()?.with_timezone(now.offset());
    Some(clock(time, now))
}

/// Quote and forwarded clocks follow the chat list: local time today, day/month otherwise.
pub fn stamp_millis(millis: i64, now: DateTime<FixedOffset>) -> Option<String> {
    let time = DateTime::from_timestamp_millis(millis)?.with_timezone(now.offset());
    Some(clock(time, now))
}

fn clock(time: DateTime<FixedOffset>, now: DateTime<FixedOffset>) -> String {
    time.format(if time.date_naive() == now.date_naive() {
        "%H:%M"
    } else {
        "%d/%m"
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn today_is_compared_in_the_viewers_timezone() {
        let now = DateTime::parse_from_rfc3339("2026-10-28T22:30:00-03:00").unwrap();
        assert_eq!(label(Some("2026-10-29T01:00:00Z"), now).as_deref(), Some("22:00"));
        assert_eq!(label(Some("2026-10-28T01:00:00Z"), now).as_deref(), Some("27/10"));
        assert_eq!(label(Some("2026-10-28T18:24:00-03:00"), now).as_deref(), Some("18:24"));
        assert_eq!(
            stamp_millis(now.timestamp_millis(), now).as_deref(),
            Some("22:30")
        );
        let older = DateTime::parse_from_rfc3339("2026-11-06T14:45:00-03:00").unwrap();
        assert_eq!(stamp_millis(older.timestamp_millis(), now).as_deref(), Some("06/11"));
    }
    #[test]
    fn missing_and_invalid_times_do_not_invent_an_activity_time() {
        let now = DateTime::parse_from_rfc3339("2026-10-28T22:30:00-03:00").unwrap();
        assert!(label(None, now).is_none());
        assert!(label(Some("not a timestamp"), now).is_none());
        assert!(label(Some(""), now).is_none());
    }
}
