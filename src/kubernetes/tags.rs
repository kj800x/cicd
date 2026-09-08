//! Resolving a tag parameter: which of an image's tags is "latest" for a
//! semver range.
//!
//! A tag qualifies when, after an optional leading `v`, it is a complete
//! version: three numeric components (`1.27.3`), optionally with a
//! prerelease or build suffix that semver understands (`1.27.3-alpine`).
//! Floating tags (`1.27`, `latest`, `stable`) never qualify; they are not
//! versions. The pattern is a semver range with Cargo's rules: `^1.27`
//! means `>=1.27.0, <2.0.0`; to stay on 1.27 write `~1.27` or `1.27.*`.
//! A suffixed tag is a prerelease and is never chosen by a range; pin it if
//! it is what you want.

// Consumed by the watchtower resolver in the next change.
#![allow(dead_code)]

use semver::{Version, VersionReq};

/// The version a tag names, if it names one.
pub fn parse_version(tag: &str) -> Option<Version> {
    let core = tag.strip_prefix('v').unwrap_or(tag);
    // Version::parse accepts `1.2.3-pre+build` but nothing shorter.
    Version::parse(core).ok()
}

/// A pattern that a person typed: a semver range.
pub fn parse_pattern(pattern: &str) -> Result<VersionReq, String> {
    VersionReq::parse(pattern.trim()).map_err(|e| format!("'{pattern}' is not a semver range: {e}"))
}

/// The highest tag among `candidates` that satisfies `pattern`, with the
/// version it parsed as, or `None` when no tag qualifies.
pub fn highest_matching<'a>(
    candidates: impl IntoIterator<Item = &'a str>,
    pattern: &str,
) -> Result<Option<String>, String> {
    let req = parse_pattern(pattern)?;
    let mut best: Option<(Version, String)> = None;
    for tag in candidates {
        let Some(version) = parse_version(tag) else {
            continue;
        };
        if !req.matches(&version) {
            continue;
        }
        match &best {
            Some((current, _)) if *current >= version => {}
            _ => best = Some((version, tag.to_string())),
        }
    }
    Ok(best.map(|(_, tag)| tag))
}

/// Whether one tag satisfies a pattern; `false` for anything that is not a
/// complete version.
pub fn matches(tag: &str, pattern: &str) -> bool {
    match (parse_version(tag), parse_pattern(pattern)) {
        (Some(v), Ok(req)) => req.matches(&v),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_complete_versions_qualify() {
        assert!(parse_version("1.27.3").is_some());
        assert!(parse_version("v1.27.3").is_some());
        assert!(parse_version("1.27.3-alpine").is_some());
        assert!(parse_version("1.27").is_none(), "floating");
        assert!(parse_version("latest").is_none());
        assert!(parse_version("1.27.3.4").is_none());
    }

    #[test]
    fn highest_in_range_wins_and_prereleases_are_skipped() {
        let tags = [
            "latest",
            "1.26.2",
            "1.27.0",
            "1.27.3",
            "1.27.3-alpine",
            "1.28.0",
            "v1.27.1",
        ];
        // Cargo caret: ^1.27 admits 1.28.
        assert_eq!(
            highest_matching(tags.iter().copied(), "^1.27")
                .unwrap()
                .as_deref(),
            Some("1.28.0")
        );
        for stay_on_127 in ["~1.27", "1.27.*"] {
            assert_eq!(
                highest_matching(tags.iter().copied(), stay_on_127)
                    .unwrap()
                    .as_deref(),
                Some("1.27.3"),
                "{stay_on_127}"
            );
        }
        assert_eq!(
            highest_matching(tags.iter().copied(), ">=1.2, <1.27")
                .unwrap()
                .as_deref(),
            Some("1.26.2")
        );
        assert_eq!(highest_matching(tags.iter().copied(), "^2").unwrap(), None);
        assert!(highest_matching(tags.iter().copied(), "not a range").is_err());
        assert!(matches("1.27.3", "~1.27"));
        assert!(!matches("1.28.0", "~1.27"));
        assert!(!matches("1.27.3-alpine", "^1.27"));
        assert!(!matches("latest", "^1.27"));
    }
}
