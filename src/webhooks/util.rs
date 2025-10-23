use regex::Regex;

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
