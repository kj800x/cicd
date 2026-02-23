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
