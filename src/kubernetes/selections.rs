//! Selections: what each parameter is currently following.
//!
//! A selection is the sticky runtime intent for one parameter. Absent, the
//! parameter tracks its source's default channel. Present, it either tracks
//! a different channel (an override) or is pinned to an exact value. Every
//! override carries a durability: `temporary` means someone is babysitting
//! this deploy and expects to end it; `standing` means it is ordinary
//! operation (a pinned dependency, a long-lived branch). A deployment with
//! any temporary override active is a temporary deployment.
//!
//! Selections live under `spec.selections`, keyed by parameter name, and are
//! written by the deploy handler, never by config sync.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Durability {
    /// Being babysat; expected to be ended. Suspends autodeploy.
    Temporary,
    /// Ordinary operation. Respected by autodeploy.
    #[default]
    Standing,
}

impl Durability {
    pub fn as_str(self) -> &'static str {
        match self {
            Durability::Temporary => "temporary",
            Durability::Standing => "standing",
        }
    }
}

/// Follow a channel other than the source's default. The field is named
/// for the source type: a branch for commit sources, a semver range
/// (`pattern`) for tag sources. Exactly one is set.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Track {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

impl Track {
    /// The channel being followed, whichever kind it is.
    pub fn channel(&self) -> &str {
        self.branch
            .as_deref()
            .or(self.pattern.as_deref())
            .unwrap_or_default()
    }
}

/// Hold an exact value.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Pin {
    pub value: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Selection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<Track>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<Pin>,
    #[serde(default)]
    pub durability: Durability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// RFC 3339 timestamp of when the override was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

pub type Selections = BTreeMap<String, Selection>;

/// What a selection resolves to at deploy time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode<'a> {
    /// Follow the source's default channel.
    Default,
    /// Follow this channel instead.
    Track(&'a str),
    /// Hold this exact value.
    Pin(&'a str),
}

impl Selection {
    /// Track a branch (commit sources).
    pub fn track(branch: &str, durability: Durability) -> Self {
        Selection {
            track: Some(Track {
                branch: Some(branch.to_string()),
                pattern: None,
            }),
            durability,
            ..Default::default()
        }
    }

    /// Track a semver range (tag sources).
    pub fn track_pattern(pattern: &str, durability: Durability) -> Self {
        Selection {
            track: Some(Track {
                branch: None,
                pattern: Some(pattern.to_string()),
            }),
            durability,
            ..Default::default()
        }
    }

    pub fn pin(value: &str, durability: Durability) -> Self {
        Selection {
            pin: Some(Pin {
                value: value.to_string(),
            }),
            durability,
            ..Default::default()
        }
    }

    pub fn with_note(mut self, note: Option<&str>, by: Option<&str>) -> Self {
        self.note = note
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(String::from);
        self.by = by
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(String::from);
        self.since = Some(chrono::Utc::now().to_rfc3339());
        self
    }

    /// A pin wins over a track if both are somehow present; the CRD schema
    /// forbids that, so this is only a tie-break for hand-edited objects.
    pub fn mode(&self) -> Mode<'_> {
        if let Some(pin) = &self.pin {
            Mode::Pin(&pin.value)
        } else if let Some(track) = &self.track {
            Mode::Track(track.channel())
        } else {
            Mode::Default
        }
    }

    /// Whether this selection deviates from the default channel at all.
    pub fn is_override(&self) -> bool {
        self.pin.is_some() || self.track.is_some()
    }

    pub fn is_temporary(&self) -> bool {
        self.is_override() && self.durability == Durability::Temporary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_and_defaults() -> Result<(), serde_json::Error> {
        let json = json!({"track": {"branch": "feature-x"}, "durability": "temporary", "note": "testing", "by": "kevin"});
        let s: Selection = serde_json::from_value(json.clone())?;
        assert_eq!(s.mode(), Mode::Track("feature-x"));
        assert!(s.is_temporary());
        assert_eq!(serde_json::to_value(&s)?, json);

        let s: Selection = serde_json::from_value(json!({"pin": {"value": "abc"}}))?;
        assert_eq!(s.mode(), Mode::Pin("abc"));
        assert_eq!(
            s.durability,
            Durability::Standing,
            "durability defaults to standing"
        );
        assert!(s.is_override() && !s.is_temporary());

        let s: Selection = serde_json::from_value(json!({}))?;
        assert_eq!(s.mode(), Mode::Default);
        assert!(!s.is_override());
        Ok(())
    }

    #[test]
    fn constructors_trim_notes() {
        let s = Selection::track("b", Durability::Temporary).with_note(Some("  why  "), Some(""));
        assert_eq!(s.note.as_deref(), Some("why"));
        assert_eq!(s.by, None);
        assert!(s.since.is_some());
        let s = Selection::pin("v", Durability::Standing);
        assert_eq!(s.mode(), Mode::Pin("v"));
        assert!(s.since.is_none());
    }
}
