use chrono::{Local, TimeZone};

/// Format a timestamp as a human-readable relative time
pub fn format_relative_time(timestamp: i64) -> String {
    let now = Local::now();
    let dt = match Local.timestamp_millis_opt(timestamp).single() {
        Some(dt) => dt,
        None => return format!("Invalid timestamp: {}", timestamp),
    };
    let duration = now.signed_duration_since(dt);

    if duration.num_days() > 0 {
        format!("{} days ago", duration.num_days())
    } else if duration.num_hours() > 0 {
        format!("{} hours ago", duration.num_hours())
    } else if duration.num_minutes() > 0 {
        format!("{} minutes ago", duration.num_minutes())
    } else {
        "just now".to_string()
    }
}

/// A compact relative time for strips and meta lines: `just now`,
/// `12m ago`, `2h ago`, `6d ago`.
pub fn format_ago_short(timestamp_ms: i64) -> String {
    let now = chrono::Utc::now().timestamp_millis();
    let elapsed = now.saturating_sub(timestamp_ms);
    if elapsed < 60_000 {
        "just now".to_string()
    } else {
        format!("{} ago", format_duration_short(elapsed))
    }
}

/// A duration in one unit: `35m`, `4h`, `2d`.
pub fn format_duration_short(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// `2026-09-07`, in Eastern time like the rest of the site.
pub fn format_date(timestamp_ms: i64) -> String {
    match chrono::Utc.timestamp_millis_opt(timestamp_ms).single() {
        Some(t) => t
            .with_timezone(&chrono_tz::America::New_York)
            .format("%Y-%m-%d")
            .to_string(),
        None => "invalid".to_string(),
    }
}

/// `09:41`, in Eastern time.
pub fn format_time_of_day(timestamp_ms: i64) -> String {
    match chrono::Utc.timestamp_millis_opt(timestamp_ms).single() {
        Some(t) => t
            .with_timezone(&chrono_tz::America::New_York)
            .format("%H:%M")
            .to_string(),
        None => "--:--".to_string(),
    }
}

/// `2026-09-08 09:26:41`, in Eastern time.
pub fn format_datetime(timestamp_ms: i64) -> String {
    match chrono::Utc.timestamp_millis_opt(timestamp_ms).single() {
        Some(t) => t
            .with_timezone(&chrono_tz::America::New_York)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => "invalid".to_string(),
    }
}

/// The calendar day of a timestamp in Eastern time.
pub fn local_date(timestamp_ms: i64) -> Option<chrono::NaiveDate> {
    chrono::Utc
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .map(|t| t.with_timezone(&chrono_tz::America::New_York).date_naive())
}

/// How a day is named in a feed: `Today`, `Yesterday`, the weekday within
/// the last week, otherwise the date.
pub fn day_label(day: chrono::NaiveDate, today: chrono::NaiveDate) -> String {
    let age = (today - day).num_days();
    match age {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        2..=6 => day.format("%A").to_string(),
        _ => day.format("%b %-e").to_string(),
    }
}

/// `09:26 today`, `18:12 yesterday`, `2026-08-30` for older.
pub fn format_when(timestamp_ms: i64) -> String {
    let today = chrono::Utc::now()
        .with_timezone(&chrono_tz::America::New_York)
        .date_naive();
    match local_date(timestamp_ms) {
        Some(day) if day == today => format!("{} today", format_time_of_day(timestamp_ms)),
        Some(day) if (today - day).num_days() == 1 => {
            format!("{} yesterday", format_time_of_day(timestamp_ms))
        }
        _ => format_date(timestamp_ms),
    }
}

/// Milliseconds since the epoch for an RFC 3339 timestamp, as selections
/// and patches record their `since`.
pub fn rfc3339_to_ms(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// Format a git sha as a short version (7 chars)
pub fn format_short_sha(sha: &str) -> &str {
    if sha.len() > 7 {
        &sha[0..7]
    } else {
        sha
    }
}

/// Format a duration in milliseconds as a compact human-readable string (e.g. "4m 23s", "1h 5m")
pub fn format_duration_ms(ms: u64) -> String {
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, secs)
    } else {
        format!("{}s", secs)
    }
}

/// Truncate a message to a maximum length
pub fn truncate_message(message: &str, max_length: usize) -> String {
    if message.len() <= max_length {
        message.to_string()
    } else {
        format!("{}...", &message[0..max_length])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_durations_use_one_unit() {
        assert_eq!(format_duration_short(5_000), "5s");
        assert_eq!(format_duration_short(35 * 60_000), "35m");
        assert_eq!(format_duration_short(4 * 3_600_000 + 20 * 60_000), "4h");
        assert_eq!(format_duration_short(2 * 86_400_000), "2d");
        assert_eq!(format_duration_short(-5), "0s");
    }

    #[test]
    fn ago_is_relative_to_now() {
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(format_ago_short(now), "just now");
        assert_eq!(format_ago_short(now - 2 * 3_600_000), "2h ago");
    }

    #[test]
    fn day_labels() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 8).unwrap_or_default();
        let d = |days: i64| today - chrono::Duration::days(days);
        assert_eq!(day_label(d(0), today), "Today");
        assert_eq!(day_label(d(1), today), "Yesterday");
        assert_eq!(day_label(d(3), today), "Saturday");
        assert_eq!(day_label(d(9), today), "Aug 30");
        assert_eq!(format_time_of_day(0), "19:00");
        assert_eq!(format_datetime(0), "1969-12-31 19:00:00");
    }

    #[test]
    fn rfc3339_parses() {
        assert_eq!(rfc3339_to_ms("1970-01-01T00:00:01+00:00"), Some(1_000));
        assert_eq!(rfc3339_to_ms("yesterday"), None);
        assert_eq!(format_date(0), "1969-12-31");
    }
}
