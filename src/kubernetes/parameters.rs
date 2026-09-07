//! Named parameters: where a DeployConfig's versions and values come from
//! (`spec.parameters`) and what is currently deployed for each of them
//! (`status.parameters`).
//!
//! Both maps are keyed by a user-facing parameter name. The legacy single
//! artifact repo lives under [`SHA_PARAMETER`], which is also the name that
//! gets substituted for `$SHA` in resource specs.
//!
//! Deserialization is deliberately strict: an entry with an unknown `type`
//! fails to deserialize. The CRD schema restricts `type` to the known values,
//! so a failure here means a newer controller or CRD wrote an object this
//! binary does not understand. Note the blast radius: kube-rs deserializes
//! whole lists, so one such object breaks every list/watch of DeployConfigs.
//! That is preferable to silently treating the config as undeployed, which
//! would make the reconciler prune its running workloads.
//!
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::kubernetes::repo::{RepositoryBranch, ShaMaybeBranch};

/// Key of the parameter that replaces the legacy `spec.artifact`.
pub const SHA_PARAMETER: &str = "SHA";

/// Where a parameter's candidate values come from (`spec.parameters.<key>`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ParameterSource {
    /// Built commits on a branch of a GitHub repository.
    Commit {
        owner: String,
        repo: String,
        /// Default Git branch to track
        branch: String,
    },
}

/// The value currently deployed for a parameter (`status.parameters.<key>`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ParameterValue {
    /// A commit SHA, optionally with the branch it was resolved from.
    Commit {
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
}

pub type ParameterSources = BTreeMap<String, ParameterSource>;
pub type ParameterValues = BTreeMap<String, ParameterValue>;

impl ParameterSource {
    /// The repository and tracking branch, for sources that are repo based.
    pub fn as_repository_branch(&self) -> Option<RepositoryBranch> {
        match self {
            ParameterSource::Commit {
                owner,
                repo,
                branch,
            } => Some(RepositoryBranch {
                owner: owner.clone(),
                repo: repo.clone(),
                branch: branch.clone(),
            }),
        }
    }

    /// Build the map for the legacy single-artifact shape: either empty or a
    /// single commit source under [`SHA_PARAMETER`].
    pub fn sha_map(artifact: Option<RepositoryBranch>) -> ParameterSources {
        artifact
            .map(|rb| BTreeMap::from([(SHA_PARAMETER.to_string(), ParameterSource::from(rb))]))
            .unwrap_or_default()
    }
}

impl From<RepositoryBranch> for ParameterSource {
    fn from(rb: RepositoryBranch) -> Self {
        ParameterSource::Commit {
            owner: rb.owner,
            repo: rb.repo,
            branch: rb.branch,
        }
    }
}

impl ParameterValue {
    /// The string substituted for `$NAME` in manifests.
    pub fn rendered(&self) -> String {
        match self {
            ParameterValue::Commit { value, .. } => value.clone(),
        }
    }

    /// The SHA and branch, for values that are commit based.
    pub fn as_sha_maybe_branch(&self) -> Option<ShaMaybeBranch> {
        match self {
            ParameterValue::Commit { value, branch } => Some(ShaMaybeBranch {
                sha: value.clone(),
                branch: branch.clone(),
            }),
        }
    }
}

impl From<ShaMaybeBranch> for ParameterValue {
    fn from(s: ShaMaybeBranch) -> Self {
        ParameterValue::Commit {
            value: s.sha,
            branch: s.branch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn source_round_trips_flat() -> Result<(), serde_json::Error> {
        let json = json!({"type": "commit", "owner": "kj800x", "repo": "cicd", "branch": "master"});
        let parsed: ParameterSource = serde_json::from_value(json.clone())?;
        assert_eq!(
            parsed,
            ParameterSource::Commit {
                owner: "kj800x".into(),
                repo: "cicd".into(),
                branch: "master".into(),
            }
        );
        assert_eq!(serde_json::to_value(&parsed)?, json);
        Ok(())
    }

    #[test]
    fn value_round_trips_with_and_without_branch() -> Result<(), serde_json::Error> {
        let with = json!({"type": "commit", "value": "abc", "branch": "master"});
        let parsed: ParameterValue = serde_json::from_value(with.clone())?;
        assert_eq!(serde_json::to_value(&parsed)?, with);

        let without = json!({"type": "commit", "value": "abc"});
        let parsed: ParameterValue = serde_json::from_value(without.clone())?;
        assert_eq!(
            parsed,
            ParameterValue::Commit {
                value: "abc".into(),
                branch: None
            }
        );
        assert_eq!(serde_json::to_value(&parsed)?, without);
        Ok(())
    }

    #[test]
    fn unknown_type_is_rejected() {
        let res: Result<ParameterSource, _> =
            serde_json::from_value(json!({"type": "tag", "owner": "a", "repo": "b"}));
        assert!(res.is_err());
        let res: Result<ParameterValue, _> =
            serde_json::from_value(json!({"type": "tag", "value": "v1"}));
        assert!(res.is_err());
    }

    #[test]
    fn maps_deserialize_and_serialize_by_key() -> Result<(), serde_json::Error> {
        let json = json!({
            "SHA": {"type": "commit", "owner": "o", "repo": "r", "branch": "b"}
        });
        let map: ParameterSources = serde_json::from_value(json.clone())?;
        assert_eq!(
            map.get(SHA_PARAMETER)
                .and_then(|s| s.as_repository_branch())
                .map(|r| r.repo),
            Some("r".into())
        );
        assert_eq!(serde_json::to_value(&map)?, json);
        Ok(())
    }

    #[test]
    fn conversions() {
        let rb = RepositoryBranch {
            owner: "o".into(),
            repo: "r".into(),
            branch: "b".into(),
        };
        let source = ParameterSource::from(rb.clone());
        assert_eq!(
            source.as_repository_branch().map(|r| r.branch),
            Some("b".into())
        );

        let map = ParameterSource::sha_map(Some(rb));
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(SHA_PARAMETER));
        assert!(ParameterSource::sha_map(None).is_empty());

        let s = ShaMaybeBranch {
            sha: "abc".into(),
            branch: None,
        };
        let v = ParameterValue::from(s.clone());
        assert_eq!(v.as_sha_maybe_branch(), Some(s));
    }
}
