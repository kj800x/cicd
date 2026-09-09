//! What changed between two revisions of the same config.
//!
//! Revisions record every input to a deploy, so the history page can say
//! why a row exists: which parameter values moved and from which channel,
//! which patches came and went, whether the config commit changed, and
//! whether the deploy came from a temporary override. Each change is one
//! short line of text so the same list serves the page and the MCP output.

use std::collections::BTreeMap;

use crate::db::revision::{Revision, RevisionParameter};
use crate::kubernetes::patches::ManifestPatch;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// A parameter is deployed with a different value (or channel) than before.
    Parameter {
        name: String,
        from: Option<RevisionParameter>,
        to: Option<RevisionParameter>,
    },
    /// The config commit moved.
    Config {
        from: Option<String>,
        to: Option<String>,
    },
    PatchAdded(ManifestPatch),
    PatchRemoved(ManifestPatch),
    /// This revision undeployed the config.
    Undeployed,
    /// This revision deployed a config that was undeployed before.
    Deployed,
}

/// The first seven characters, as GitHub shows a commit.
fn format_short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// How a parameter's value is shown: `branch:sha` for a tracked commit,
/// `pinned sha` for a pinned one, the bare tag or value for everything
/// else. A tag's range is not repeated on every line; the value says
/// enough, and the revision detail shows the channel.
fn show(p: &RevisionParameter) -> String {
    if p.kind != "commit" {
        return p.value.clone();
    }
    let value = format_short_sha(&p.value);
    match &p.branch {
        Some(b) if !b.is_empty() => format!("{b}:{value}"),
        _ => format!("pinned {value}"),
    }
}

impl Change {
    /// One line, such as `SHA master:a1b2c3d → feature:9f8e7d6`.
    pub fn describe(&self) -> String {
        match self {
            Change::Parameter { name, from, to } => match (from, to) {
                (Some(f), Some(t)) => format!("{name} {} → {}", show(f), show(t)),
                (None, Some(t)) => format!("{name} added: {}", show(t)),
                (Some(f), None) => format!("{name} removed (was {})", show(f)),
                (None, None) => format!("{name} unchanged"),
            },
            Change::Config { from, to } => format!(
                "config {} → {}",
                from.as_deref().map(format_short_sha).unwrap_or("-"),
                to.as_deref().map(format_short_sha).unwrap_or("-")
            ),
            Change::PatchAdded(p) => format!("patch added: {}", p.describe()),
            Change::PatchRemoved(p) => format!("patch removed: {}", p.describe()),
            Change::Undeployed => "undeployed".to_string(),
            Change::Deployed => "deployed".to_string(),
        }
    }
}

fn patches_of(rev: &Revision) -> Vec<ManifestPatch> {
    rev.patches
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default()
}

fn is_deployed(rev: &Revision) -> bool {
    rev.action != "undeploy"
}

/// The changes `rev` made relative to `prev`, the previous revision of the
/// same config. With no previous revision every parameter and patch shows
/// as added, since the list reached no further back. Parameters compare by
/// value and channel, so a pin of the SHA that was already deployed still
/// shows (the channel changed), which is the point: it is what the person
/// did.
pub fn diff(rev: &Revision, prev: Option<&Revision>) -> Vec<Change> {
    let mut changes = Vec::new();
    match (prev.map(is_deployed), is_deployed(rev)) {
        (Some(true), false) => {
            changes.push(Change::Undeployed);
            return changes;
        }
        (Some(false), true) => changes.push(Change::Deployed),
        (None, false) => {
            changes.push(Change::Undeployed);
            return changes;
        }
        _ => {}
    }

    let before: BTreeMap<&str, &RevisionParameter> = prev
        .filter(|p| is_deployed(p))
        .map(|p| p.parameters.iter().map(|x| (x.name.as_str(), x)).collect())
        .unwrap_or_default();
    let after: BTreeMap<&str, &RevisionParameter> = rev
        .parameters
        .iter()
        .map(|x| (x.name.as_str(), x))
        .collect();
    let names: std::collections::BTreeSet<&str> =
        before.keys().chain(after.keys()).copied().collect();
    for name in names {
        let from = before.get(name).copied();
        let to = after.get(name).copied();
        let same = match (from, to) {
            (Some(f), Some(t)) => f.value == t.value && f.branch == t.branch,
            _ => false,
        };
        if !same {
            changes.push(Change::Parameter {
                name: name.to_string(),
                from: from.cloned(),
                to: to.cloned(),
            });
        }
    }

    let prev_config = prev
        .filter(|p| is_deployed(p))
        .and_then(|p| p.config_sha.clone());
    if prev_config != rev.config_sha && (prev_config.is_some() || rev.config_sha.is_some()) {
        changes.push(Change::Config {
            from: prev_config,
            to: rev.config_sha.clone(),
        });
    }

    let old_patches = prev.map(patches_of).unwrap_or_default();
    let new_patches = patches_of(rev);
    for p in &new_patches {
        if !old_patches.contains(p) {
            changes.push(Change::PatchAdded(p.clone()));
        }
    }
    for p in &old_patches {
        if !new_patches.contains(p) {
            changes.push(Change::PatchRemoved(p.clone()));
        }
    }
    changes
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn param(name: &str, value: &str, branch: Option<&str>) -> RevisionParameter {
        RevisionParameter {
            name: name.into(),
            kind: if name == "SHA" { "commit" } else { "value" }.into(),
            value: value.into(),
            branch: branch.map(String::from),
        }
    }

    fn rev(
        id: i64,
        action: &str,
        config: Option<&str>,
        params: Vec<RevisionParameter>,
    ) -> Revision {
        Revision {
            id,
            config_name: "site".into(),
            created_at: id * 1000,
            actor: "web".into(),
            action: action.into(),
            reason: None,
            config_sha: config.map(String::from),
            config_branch: Some("master".into()),
            config_version_hash: None,
            patches: None,
            temporary: false,
            parameters: params,
        }
    }

    fn lines(rev: &Revision, prev: Option<&Revision>) -> Vec<String> {
        diff(rev, prev).iter().map(Change::describe).collect()
    }

    #[test]
    fn parameter_value_and_channel_changes_are_listed() {
        let a = rev(
            1,
            "deploy",
            Some("c1"),
            vec![param("SHA", "aaaaaaaa1", Some("master"))],
        );
        let b = rev(
            2,
            "deploy",
            Some("c1"),
            vec![
                param("SHA", "bbbbbbbb2", Some("feature")),
                param("GREETING", "howdy", None),
            ],
        );
        assert_eq!(
            lines(&b, Some(&a)),
            vec![
                "GREETING added: howdy",
                "SHA master:aaaaaaa → feature:bbbbbbb"
            ]
        );
        // Same SHA, now pinned: the channel changed, so it shows.
        let c = rev(
            3,
            "deploy",
            Some("c1"),
            vec![
                param("SHA", "bbbbbbbb2", None),
                param("GREETING", "howdy", None),
            ],
        );
        assert_eq!(
            lines(&c, Some(&b)),
            vec!["SHA feature:bbbbbbb → pinned bbbbbbb"]
        );
        // Nothing moved: no lines.
        assert!(lines(&c, Some(&c)).is_empty());
    }

    #[test]
    fn config_and_deploy_state_changes_are_listed() {
        let a = rev(
            1,
            "deploy",
            Some("c1"),
            vec![param("SHA", "a", Some("master"))],
        );
        let b = rev(
            2,
            "deploy",
            Some("c2"),
            vec![param("SHA", "a", Some("master"))],
        );
        assert_eq!(lines(&b, Some(&a)), vec!["config c1 → c2"]);

        let u = rev(3, "undeploy", None, vec![]);
        assert_eq!(lines(&u, Some(&b)), vec!["undeployed"]);
        // Redeploying after an undeploy lists everything as new.
        assert_eq!(
            lines(&b, Some(&u)),
            vec!["deployed", "SHA added: master:a", "config - → c2"]
        );
        // First known revision: everything is "added".
        assert_eq!(
            lines(&a, None),
            vec!["SHA added: master:a", "config - → c1"]
        );
    }

    #[test]
    fn patch_changes_are_listed() {
        let p1 = serde_json::json!({"target": {"kind": "Deployment", "name": "web"}, "op": "replace", "path": "/spec/replicas", "value": 3, "durability": "temporary"});
        let p2 = serde_json::json!({"target": {"kind": "Deployment", "name": "web"}, "op": "remove", "path": "/spec/x", "durability": "standing"});
        let mut a = rev(1, "deploy", Some("c1"), vec![]);
        a.patches = Some(serde_json::json!([p1]).to_string());
        let mut b = rev(2, "patch", Some("c1"), vec![]);
        b.patches = Some(serde_json::json!([p2]).to_string());
        let got = lines(&b, Some(&a));
        assert_eq!(got.len(), 2);
        assert!(got[0].starts_with("patch added: standing"), "{got:?}");
        assert!(got[1].starts_with("patch removed: temporary"), "{got:?}");
    }
}
