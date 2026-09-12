//! Resolving a tag parameter: which of an image's tags is "latest" for a
//! semver range.
//!
//! Watchtower reads every tag for us (`watchtower::Tag::version`,
//! `variant` and `build`): after an optional leading `v`, a version is two
//! to four numeric components (`15.11`, `1.27.3`, `4.0.19.2979`) and the
//! variant is whatever follows the first `-` (`alpine`, `rc1`). A tag it
//! reads no version from never qualifies: `latest`, `18`, `stable`,
//! `10.11.8ubu2404`.
//!
//! Build metadata is what watchtower splits off a linuxserver.io tag: the
//! `-lsNN` build number and whatever upstream glued onto its version, as
//! semver writes it after a `+`. `12.0ubu2604-ls48` is version `12.0`
//! with build `ubu2604.ls48`; `4.0.19.2979-ls324` is `4.0.19.2979` with
//! build `ls324`. Build metadata never affects whether a range matches;
//! among tags of one version it orders the rebuilds, identifier by
//! identifier with the trailing number compared as a number, and a tag
//! with a build outranks the same version without one (linuxserver
//! documents the `-lsNN` tag as the static one and the plain tag as an
//! unsupported alias).
//!
//! The pattern is a semver range with Cargo's rules: `^1.27` means
//! `>=1.27.0, <2.0.0`; to stay on 1.27 write `~1.27` or `1.27.*`; `=1.27.3`
//! is that one version. A two-part version is its `X.Y.0` and loses a tie
//! to `X.Y.0` written out, so it is chosen only when nothing more specific
//! names that version. A fourth component orders tags within a patch
//! (`4.0.19.2979` above `4.0.19`) and is otherwise ignored.
//!
//! A variant is a prerelease: a range never picks one, `=2.1.2-alpine`
//! does. A parameter that follows a variant (`variant: alpine`) sees only
//! the tags with exactly that suffix, compared on the version before it,
//! so `^2.1.2` then moves from `2.1.2-alpine` to `2.1.3-alpine`.

use semver::{BuildMetadata, Prerelease, Version, VersionReq};

use crate::watchtower::{Event, Tag};

/// A tag that names a version, as watchtower read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate<'a> {
    pub tag: &'a str,
    pub version: &'a str,
    pub variant: Option<&'a str>,
    pub build: Option<&'a str>,
}

impl<'a> Candidate<'a> {
    pub fn new(tag: &'a str, version: &'a str, variant: Option<&'a str>) -> Self {
        Self {
            tag,
            version,
            variant,
            build: None,
        }
    }

    pub fn with_build(self, build: Option<&'a str>) -> Self {
        Self { build, ..self }
    }

    /// `None` when watchtower read no version from the tag.
    pub fn of(tag: &'a Tag) -> Option<Self> {
        tag.version.as_deref().map(|version| {
            Self::new(&tag.tag, version, tag.variant.as_deref()).with_build(tag.build.as_deref())
        })
    }

    pub fn of_event(event: &'a Event) -> Option<Self> {
        event.version.as_deref().map(|version| {
            Self::new(&event.tag, version, event.variant.as_deref())
                .with_build(event.build.as_deref())
        })
    }
}

/// Where a candidate stands among the others: its components, then how
/// many were written (`8.11.0` above `8.11`), then its build (`-ls48`
/// above `-ls47`, and either above none), then its variant.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    components: [u64; 4],
    parts: usize,
    build: Vec<(String, u64)>,
    variant: Option<String>,
}

/// Build identifiers as they order: each split into its text and the
/// number it ends in (`ls48` is `("ls", 48)`, `ubu2604` is
/// `("ubu", 2604)`, `abc` is `("abc", 0)`), so that `ls48` follows `ls9`.
fn build_order(build: Option<&str>) -> Vec<(String, u64)> {
    build
        .into_iter()
        .flat_map(|b| b.split('.'))
        .map(|identifier| {
            let digits = identifier
                .bytes()
                .rev()
                .take_while(|b| b.is_ascii_digit())
                .count();
            let (text, number) = identifier.split_at(identifier.len() - digits);
            (text.to_string(), number.parse().unwrap_or(0))
        })
        .collect()
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
/// the version or variant is not something semver can hold, or when the
/// parameter follows a variant and this tag is not of it.
fn parsed(candidate: &Candidate, variant: Option<&str>) -> Option<(Version, Rank)> {
    let (components, parts) = components(candidate.version)?;
    let pre = match (variant, candidate.variant) {
        (Some(wanted), Some(actual)) if wanted == actual => Prerelease::EMPTY,
        (Some(_), _) => return None,
        (None, Some(actual)) => Prerelease::new(actual).ok()?,
        (None, None) => Prerelease::EMPTY,
    };
    let version = Version {
        major: components[0],
        minor: components[1],
        patch: components[2],
        pre,
        build: candidate
            .build
            .and_then(|b| BuildMetadata::new(b).ok())
            .unwrap_or(BuildMetadata::EMPTY),
    };
    let rank = Rank {
        components,
        parts,
        build: build_order(candidate.build),
        variant: candidate.variant.map(String::from),
    };
    Some((version, rank))
}

/// How a candidate orders against others of the same image.
pub fn rank(candidate: &Candidate) -> Option<Rank> {
    parsed(candidate, None).map(|(_, rank)| rank)
}

/// A pattern that a person typed: a semver range.
pub fn parse_pattern(pattern: &str) -> Result<VersionReq, String> {
    VersionReq::parse(pattern.trim()).map_err(|e| format!("'{pattern}' is not a semver range: {e}"))
}

/// The highest-ranked tag among `candidates` that satisfies `pattern`, or
/// `None` when no tag qualifies. With a `variant`, only tags of that
/// variant are candidates.
pub fn highest_matching<'a>(
    candidates: impl IntoIterator<Item = Candidate<'a>>,
    pattern: &str,
    variant: Option<&str>,
) -> Result<Option<String>, String> {
    let req = parse_pattern(pattern)?;
    let mut best: Option<(Rank, String)> = None;
    for candidate in candidates {
        let Some((version, rank)) = parsed(&candidate, variant) else {
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

/// Whether one candidate satisfies a pattern, for the variant followed.
pub fn matches(candidate: &Candidate, pattern: &str, variant: Option<&str>) -> bool {
    match (parsed(candidate, variant), parse_pattern(pattern)) {
        (Some((version, _)), Ok(req)) => req.matches(&version),
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Tags as watchtower would read them: the version is the tag up to
    /// the first dash, `v` dropped; the rest is the variant, except that
    /// a trailing `-lsNN` and any glue on the version are the build.
    fn read(tag: &str) -> Candidate<'_> {
        let stripped = tag.strip_prefix('v').unwrap_or(tag);
        let (core, rest) = match stripped.split_once('-') {
            Some((core, rest)) => (core, Some(rest)),
            None => (stripped, None),
        };
        let is_ls = |s: &str| {
            s.strip_prefix("ls")
                .is_some_and(|n| n.parse::<u64>().is_ok())
        };
        let (variant, ls) = match rest {
            Some(rest) if is_ls(rest) => (None, Some(rest)),
            Some(rest) => match rest.rsplit_once('-') {
                Some((variant, ls)) if is_ls(ls) => (Some(variant), Some(ls)),
                _ => (Some(rest), None),
            },
            None => (None, None),
        };
        let glue_at = core
            .bytes()
            .position(|b| !(b.is_ascii_digit() || b == b'.'))
            .unwrap_or(core.len());
        let (version, glue) = core.split_at(glue_at);
        let glue = glue.trim_start_matches('_');
        let build = match (glue, ls) {
            ("", None) => None,
            ("", Some(ls)) => Some(ls.to_string()),
            (glue, Some(ls)) => Some(format!("{glue}.{ls}")),
            (glue, None) => Some(glue.to_string()),
        };
        // The build is not a substring of the tag; leaking it keeps the
        // helper's signature the same as before (tests only).
        let build = build.map(|b| &*Box::leak(b.into_boxed_str()));
        Candidate::new(tag, version, variant).with_build(build)
    }

    fn best(tags: &[&str], pattern: &str) -> Option<String> {
        highest_matching(tags.iter().map(|t| read(t)), pattern, None).unwrap()
    }

    fn best_of(tags: &[&str], pattern: &str, variant: &str) -> Option<String> {
        highest_matching(tags.iter().map(|t| read(t)), pattern, Some(variant)).unwrap()
    }

    #[test]
    fn candidates_come_from_what_watchtower_read() {
        let tag = Tag {
            tag: "v1.27.3-alpine".into(),
            active: true,
            version: Some("1.27.3".into()),
            variant: Some("alpine".into()),
            build: None,
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
            build: None,
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
        assert!(highest_matching(tags.iter().map(|t| read(t)), "not a range", None).is_err());
        assert!(matches(&read("1.27.3"), "~1.27", None));
        assert!(!matches(&read("1.28.0"), "~1.27", None));
        assert!(!matches(&read("1.27.3-alpine"), "^1.27", None));
        assert!(!matches(&read("latest"), "^1.27", None), "no version");
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
        assert_eq!(
            best(&sonarr, "^4.0.18").as_deref(),
            Some("4.0.19.2979-ls323"),
            "four parts beat three; the build number is not a prerelease"
        );
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

    #[test]
    fn a_linuxserver_build_outranks_the_plain_tag_and_older_builds() {
        // Every release gets a static `-lsNN` tag and an unsupported plain
        // alias; the newest build of the version is the one to run.
        let sonarr = ["4.0.19", "4.0.19.2979-ls323", "4.0.19.2979-ls324", "4.0.18"];
        assert_eq!(
            best(&sonarr, "=4.0.19").as_deref(),
            Some("4.0.19.2979-ls324")
        );
        assert!(rank(&read("4.0.19.2979-ls324")) > rank(&read("4.0.19.2979-ls323")));
        assert!(rank(&read("4.0.19.2979-ls324")) > rank(&read("4.0.19")));
        assert!(
            rank(&read("1.2.3-ls10")) > rank(&read("1.2.3-ls9")),
            "build numbers compare as numbers"
        );
        assert!(rank(&read("1.2.3-ls10")) > rank(&read("1.2.3")));
        assert!(
            rank(&read("1.2.4")) > rank(&read("1.2.3-ls10")),
            "a newer version beats any build"
        );
        let radarr = [
            "6.3.0",
            "6.3.0.10514-ls315",
            "6.3.0-nightly",
            "6.3.0-develop",
        ];
        assert_eq!(
            best(&radarr, "=6.3.0").as_deref(),
            Some("6.3.0.10514-ls315")
        );
        // The plain tag alone still resolves, as it always did.
        assert_eq!(
            best(&["4.0.19", "4.0.18"], "=4.0.19").as_deref(),
            Some("4.0.19")
        );
    }

    #[test]
    fn glue_on_a_linuxserver_version_is_build_metadata_not_a_variant() {
        // Jellyfin 12 is a two-part version with the distro glued on; a
        // range must still pick it, and the newer distro build wins a tie.
        let jellyfin = [
            "latest",
            "10.11.11",
            "10.11.11ubu2404-ls42",
            "10.11.11ubu2604-ls43",
            "10.11.11ubu2604-ls47",
            "12.0ubu2604-ls48",
        ];
        assert_eq!(
            best(&jellyfin, "^12.0").as_deref(),
            Some("12.0ubu2604-ls48")
        );
        assert_eq!(best(&jellyfin, "^12").as_deref(), Some("12.0ubu2604-ls48"));
        assert_eq!(
            best(&jellyfin, "10.11.*").as_deref(),
            Some("10.11.11ubu2604-ls47")
        );
        assert_eq!(best(&jellyfin, "*").as_deref(), Some("12.0ubu2604-ls48"));
        assert!(rank(&read("10.11.11ubu2604-ls43")) > rank(&read("10.11.11ubu2404-ls42")));
        assert!(matches(&read("12.0ubu2604-ls48"), "^12.0", None));
        assert!(
            !matches(&read("12.0ubu2604-ls48"), "^12.0", Some("alpine")),
            "no variant"
        );
        // A two-part version with a build still loses to the next patch.
        assert!(rank(&read("12.0.1")) > rank(&read("12.0ubu2604-ls48")));
        // qbittorrent: the libtorrent version is glue too.
        let qbittorrent = [
            "5.2.3",
            "5.2.3_v2.0.13-ls470",
            "5.2.3_v2.0.14-ls475",
            "5.2.3-libtorrentv1",
        ];
        assert_eq!(
            best(&qbittorrent, "=5.2.3").as_deref(),
            Some("5.2.3_v2.0.14-ls475")
        );
        assert_eq!(
            read("5.2.3_v2.0.14-ls475").build,
            Some("v2.0.14.ls475"),
            "the test reader mirrors watchtower"
        );
        assert_eq!(
            Candidate::of(&Tag {
                tag: "12.0ubu2604-ls48".into(),
                active: true,
                version: Some("12.0".into()),
                variant: None,
                build: Some("ubu2604.ls48".into()),
                history: vec![],
            }),
            Some(Candidate::new("12.0ubu2604-ls48", "12.0", None).with_build(Some("ubu2604.ls48")))
        );
    }

    #[test]
    fn following_a_variant_sees_only_its_tags_compared_on_the_version() {
        let mosquitto = [
            "latest",
            "2",
            "2.1-alpine",
            "2.1.1-alpine",
            "2.1.2-alpine",
            "2.1.3",
            "2.1.3-openssl",
        ];
        assert_eq!(
            best_of(&mosquitto, "^2.1.2", "alpine").as_deref(),
            Some("2.1.2-alpine")
        );
        assert_eq!(
            best_of(&mosquitto, "=2.1.1", "alpine").as_deref(),
            Some("2.1.1-alpine")
        );
        assert_eq!(
            best_of(&mosquitto, "^2.1.2", "openssl").as_deref(),
            Some("2.1.3-openssl")
        );
        assert_eq!(best_of(&mosquitto, "^2.1.2", "bookworm"), None);
        // Without a variant the same tags are prereleases, as before.
        assert_eq!(best(&mosquitto, "^2.1.2").as_deref(), Some("2.1.3"));
        assert!(matches(&read("2.1.3-alpine"), "^2.1.2", Some("alpine")));
        assert!(
            !matches(&read("2.1.3"), "^2.1.2", Some("alpine")),
            "bare tag is not the variant"
        );
        assert!(
            !matches(&read("2.1.3-alpine3.22"), "^2.1.2", Some("alpine")),
            "exact suffix"
        );
        let minecraft = [
            "2026.9.0",
            "2026.9.0-java25",
            "2026.9.1-java25",
            "2026.9.1-java21",
        ];
        assert_eq!(
            best_of(&minecraft, "^2026.9.0", "java25").as_deref(),
            Some("2026.9.1-java25")
        );
    }
}
