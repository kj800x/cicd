use chrono::DateTime;
use regex::Regex;

/// Parse an optional RFC3339 timestamp (as GitHub returns on check runs) into
/// epoch milliseconds. Returns None if absent or unparseable.
pub fn rfc3339_to_millis(ts: Option<&str>) -> Option<u64> {
    let ts = ts?;
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp_millis())
        .filter(|ms| *ms >= 0)
        .map(|ms| ms as u64)
}

// FIXME: This can be a method impl on the PushEvent itself

// Pushes can be for reasons other than branches, such as tags
pub fn extract_branch_name(r#ref: &str) -> Option<String> {
    // This regex is a compile-time constant pattern, so expect is appropriate
    #[allow(clippy::expect_used)]
    let branch_regex =
        Regex::new(r"^refs/heads/(.+)$").expect("Branch regex pattern should be valid");
    if let Some(captures) = branch_regex.captures(r#ref) {
        captures.get(1).map(|m| m.as_str().to_string())
    } else {
        None
    }
}
