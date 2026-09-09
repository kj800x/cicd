//! Resolving a tag parameter: which of an image's tags is "latest" for a
//! semver range.
//!
//! Watchtower reads every tag for us (`watchtower::Tag::version` and
//! `variant`): after an optional leading `v`, a version is two to four
//! numeric components (`15.11`, `1.27.3`, `4.0.19.2979`) and the variant
//! is whatever follows the first `-` (`alpine`, `rc1`, `ls323`). A tag it
//! reads no version from never qualifies: `latest`, `18`, `stable`,
//! `10.11.8ubu2404`.
//!
//! The pattern is a semver range with Cargo's rules: `^1.27` means
//! `>=1.27.0, <2.0.0`; to stay on 1.27 write `~1.27` or `1.27.*`; `=1.27.3`
//! is that one version. A two-part version is its `X.Y.0` and loses a tie
//! to `X.Y.0` written out, so it is chosen only when nothing more specific
//! names that version. A fourth component orders tags within a patch
//! (`4.0.19.2979` above `4.0.19`) and is otherwise ignored.
//!
//! A variant is a prerelease: a range never picks one, `=2.1.2-alpine`
//! does.

use semver::{BuildMetadata, Prerelease, Version, VersionReq};

use crate::watchtower::{Event, Tag};

/// A tag that names a version, as watchtower read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate<'a> {
    pub tag: &'a str,
    pub version: &'a str,
    pub variant: Option<&'a str>,
}

impl<'a> Candidate<'a> {
    pub fn new(tag: &'a str, version: &'a str, variant: Option<&'a str>) -> Self {
        Self {
            tag,
            version,
            variant,
        }
    }

    /// `None` when watchtower read no version from the tag.
    pub fn of(tag: &'a Tag) -> Option<Self> {
        tag.version
            .as_deref()
            .map(|version| Self::new(&tag.tag, version, tag.variant.as_deref()))
    }

    pub fn of_event(event: &'a Event) -> Option<Self> {
        event
            .version
            .as_deref()
            .map(|version| Self::new(&event.tag, version, event.variant.as_deref()))
    }
}

/// Where a candidate stands among the others: its components, then how
/// many were written (`8.11.0` above `8.11`), then its variant.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    components: [u64; 4],
    parts: usize,
    variant: Option<String>,
}

fn components(version: &str) -> Option<([u64; 4], usize)> {
    let mut out = [0u64; 4];
    let mut parts = 0;
    for piece in version.split('.') {
        if parts == 4 {
            return None;
        }
        out[parts] = piece.parse().ok()?;
        parts += 1;
    }
    (parts >= 2).then_some((out, parts))
}

/// The semver version a candidate matches as, and its rank; `None` when
/// the version or variant is not something semver can hold.
fn parsed(candidate: &Candidate) -> Option<(Version, Rank)> {
    let (components, parts) = components(candidate.version)?;
    let pre = match candidate.variant {
        Some(variant) => Prerelease::new(variant).ok()?,
        None => Prerelease::EMPTY,
    };
    let version = Version {
        major: components[0],
        minor: components[1],
        patch: components[2],
        pre,
        build: BuildMetadata::EMPTY,
    };
    let rank = Rank {
        components,
        parts,
        variant: candidate.variant.map(String::from),
    };
    Some((version, rank))
}

/// How a candidate orders against others of the same image.
pub fn rank(candidate: &Candidate) -> Option<Rank> {
    parsed(candidate).map(|(_, rank)| rank)
}

/// A pattern that a person typed: a semver range.
pub fn parse_pattern(pattern: &str) -> Result<VersionReq, String> {
    VersionReq::parse(pattern.trim()).map_err(|e| format!("'{pattern}' is not a semver range: {e}"))
}

/// The highest-ranked tag among `candidates` that satisfies `pattern`, or
/// `None` when no tag qualifies.
pub fn highest_matching<'a>(
    candidates: impl IntoIterator<Item = Candidate<'a>>,
    pattern: &str,
) -> Result<Option<String>, String> {
    let req = parse_pattern(pattern)?;
    let mut best: Option<(Rank, String)> = None;
    for candidate in candidates {
        let Some((version, rank)) = parsed(&candidate) else {
            continue;
        };
        if !req.matches(&version) {
            continue;
        }
        match &best {
            Some((current, _)) if *current >= rank => {}
            _ => best = Some((rank, candidate.tag.to_string())),
        }
    }
    Ok(best.map(|(_, tag)| tag))
}

/// Whether one candidate satisfies a pattern.
pub fn matches(candidate: &Candidate, pattern: &str) -> bool {
    match (parsed(candidate), parse_pattern(pattern)) {
        (Some((version, _)), Ok(req)) => req.matches(&version),
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Tags as watchtower would read them: the version is the tag up to
    /// the first dash, `v` dropped; the rest is the variant.
    fn read(tag: &str) -> Candidate<'_> {
        let stripped = tag.strip_prefix('v').unwrap_or(tag);
        match stripped.split_once('-') {
            Some((version, variant)) => Candidate::new(tag, version, Some(variant)),
            None => Candidate::new(tag, stripped, None),
        }
    }

    fn best(tags: &[&str], pattern: &str) -> Option<String> {
        highest_matching(tags.iter().map(|t| read(t)), pattern).unwrap()
    }

    #[test]
    fn candidates_come_from_what_watchtower_read() {
        let tag = Tag {
            tag: "v1.27.3-alpine".into(),
            active: true,
            version: Some("1.27.3".into()),
            variant: Some("alpine".into()),
            history: vec![],
        };
        assert_eq!(
            Candidate::of(&tag),
            Some(Candidate::new("v1.27.3-alpine", "1.27.3", Some("alpine")))
        );
        let floating = Tag {
            tag: "latest".into(),
            active: true,
            version: None,
            variant: None,
            history: vec![],
        };
        assert_eq!(Candidate::of(&floating), None);
    }

    #[test]
    fn highest_in_range_wins_and_variants_are_skipped() {
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
        assert_eq!(best(&tags, "^1.27").as_deref(), Some("1.28.0"));
        for stay_on_127 in ["~1.27", "1.27.*"] {
            assert_eq!(
                best(&tags, stay_on_127).as_deref(),
                Some("1.27.3"),
                "{stay_on_127}"
            );
        }
        assert_eq!(best(&tags, ">=1.2, <1.27").as_deref(), Some("1.26.2"));
        assert_eq!(
            best(&tags, "=1.27.3-alpine").as_deref(),
            Some("1.27.3-alpine")
        );
        assert_eq!(best(&tags, "^2"), None);
        assert!(highest_matching(tags.iter().map(|t| read(t)), "not a range").is_err());
        assert!(matches(&read("1.27.3"), "~1.27"));
        assert!(!matches(&read("1.28.0"), "~1.27"));
        assert!(!matches(&read("1.27.3-alpine"), "^1.27"));
        assert!(!matches(&read("latest"), "^1.27"), "no version");
    }

    #[test]
    fn two_part_versions_qualify_and_lose_ties_to_full_ones() {
        // postgres publishes nothing longer than two parts.
        let postgres = ["15", "15.11", "15.19", "18.1", "latest", "15.19-alpine"];
        assert_eq!(best(&postgres, "=15.11.0").as_deref(), Some("15.11"));
        assert_eq!(best(&postgres, "^15.11").as_deref(), Some("15.19"));
        assert_eq!(best(&postgres, "^18.1.0").as_deref(), Some("18.1"));
        assert_eq!(best(&postgres, "^15.20"), None);
        // redis publishes both; the written-out patch wins the tie, and the
        // floating minor never beats a newer patch.
        let redis = ["8.11", "8.11.0", "8.10.1", "8.10"];
        assert_eq!(best(&redis, "^8.10.1").as_deref(), Some("8.11.0"));
        assert_eq!(best(&redis, "~8.10").as_deref(), Some("8.10.1"));
        assert!(rank(&read("8.11.0")) > rank(&read("8.11")));
        assert!(rank(&read("8.11")) > rank(&read("8.10.1")));
    }

    #[test]
    fn a_fourth_component_orders_within_a_patch() {
        let sonarr = ["4.0.19", "4.0.19.2979-ls323", "4.0.18", "4.0.18.2971"];
        assert_eq!(best(&sonarr, "^4.0.18").as_deref(), Some("4.0.19"));
        assert_eq!(best(&sonarr, "=4.0.18").as_deref(), Some("4.0.18.2971"));
        assert!(rank(&read("4.0.18.2971")) > rank(&read("4.0.18")));
        assert!(rank(&read("4.0.19")) > rank(&read("4.0.18.2971")));
        assert_eq!(rank(&Candidate::new("x", "1.2.3.4.5", None)), None);
        assert_eq!(rank(&Candidate::new("x", "1.2.a", None)), None);
        assert_eq!(
            rank(&Candidate::new("x", "5.2.3", Some("v2_0"))),
            None,
            "not semver"
        );
    }
}
