//! Message header timestamp formatting for the room pane.
//!
//! Why not always show a relative "Nm/Nh ago" string: past 24h it stops
//! being useful (nobody parses "31h ago" faster than a date) and actively
//! misleads about which calendar day a message landed on, so we fall back
//! to an absolute `MMM-DD HH:MM` once the delta crosses a day.
use chrono::{DateTime, Duration, Utc};

/// `created_at` is stored as `%Y-%m-%dT%H:%M:%SZ` (RFC3339-compatible).
/// Unparsable input is returned unchanged rather than panicking — a display
/// concern, not a fatal one.
pub fn format_timestamp(created_at: &str, now: DateTime<Utc>) -> String {
    let Ok(parsed) = DateTime::parse_from_rfc3339(created_at) else {
        return created_at.to_owned();
    };
    let parsed = parsed.with_timezone(&Utc);
    let delta = now.signed_duration_since(parsed);

    if delta < Duration::zero() {
        // Clock skew / future timestamp: absolute is less confusing than "-1m ago".
        return parsed.format("%b-%d %H:%M").to_string();
    }
    if delta < Duration::hours(1) {
        return format!("{}m ago", delta.num_minutes());
    }
    if delta < Duration::hours(24) {
        return format!("{}h ago", delta.num_hours());
    }
    parsed.format("%b-%d %H:%M").to_string()
}

/// True when `created_at` falls within `window` of `now` — used for the
/// MEMBER column's "active in the last hour" dot, not for header display.
pub fn is_within(created_at: &str, now: DateTime<Utc>, window: Duration) -> bool {
    DateTime::parse_from_rfc3339(created_at)
        .map(|parsed| now.signed_duration_since(parsed.with_timezone(&Utc)) <= window)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_an_hour_is_rendered_as_minutes_ago() {
        let now = Utc::now();
        let created = (now - Duration::minutes(5)).to_rfc3339();
        assert_eq!(format_timestamp(&created, now), "5m ago");
    }

    #[test]
    fn between_one_and_twenty_four_hours_is_rendered_as_hours_ago() {
        let now = Utc::now();
        let created = (now - Duration::hours(3)).to_rfc3339();
        assert_eq!(format_timestamp(&created, now), "3h ago");
    }

    #[test]
    fn past_twenty_four_hours_falls_back_to_absolute_date() {
        let now = Utc::now();
        let past = now - Duration::hours(30);
        let expected = past.format("%b-%d %H:%M").to_string();
        assert_eq!(format_timestamp(&past.to_rfc3339(), now), expected);
    }

    #[test]
    fn is_within_respects_the_requested_window() {
        let now = Utc::now();
        let recent = (now - Duration::minutes(10)).to_rfc3339();
        let stale = (now - Duration::hours(2)).to_rfc3339();
        assert!(is_within(&recent, now, Duration::hours(1)));
        assert!(!is_within(&stale, now, Duration::hours(1)));
    }
}
