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
    /// Tags of a container image, as watchtower sees them in the registry.
    /// The channel is a semver range; the default is `pattern`.
    Tag {
        /// `registry/name`, e.g. `docker.io/library/nginx` or
        /// `ghcr.io/kj800x/nginx`. A bare name means Docker Hub.
        image: String,
        pattern: String,
    },
    /// A static value with no feed. Its only channel is the default; a
    /// selection can pin it to something else.
    Value { default: String },
}

/// A container image split the way watchtower keys it.
#[allow(dead_code)] // consumed by the watchtower client in the next change
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef {
    pub registry: String,
    pub name: String,
}

impl ImageRef {
    /// `docker.io/library/nginx` -> docker.io + library/nginx;
    /// `ghcr.io/kj800x/nginx` -> ghcr.io + kj800x/nginx; `nginx` ->
    /// docker.io + library/nginx; `kj800x/nginx` -> docker.io + kj800x/nginx.
    /// The first segment is a registry when it contains a dot or a colon
    /// or is `localhost`, as the Docker CLI decides.
    pub fn parse(image: &str) -> Self {
        let image = image.trim().trim_end_matches('/');
        let (registry, name) = match image.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), rest.to_string())
            }
            _ => ("docker.io".to_string(), image.to_string()),
        };
        let name = if registry == "docker.io" && !name.contains('/') {
            format!("library/{name}")
        } else {
            name
        };
        ImageRef { registry, name }
    }
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
    /// An image tag, with the range it was resolved from (absent for a
    /// pin) and the digest it pointed at when deployed, if known.
    Tag {
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        digest: Option<String>,
    },
    /// The deployed value of a static parameter.
    Value { value: String },
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
            ParameterSource::Tag { .. } | ParameterSource::Value { .. } => None,
        }
    }

    /// The image a tag source watches.
    #[allow(dead_code)] // consumed by the watchtower client in the next change
    pub fn image_ref(&self) -> Option<ImageRef> {
        match self {
            ParameterSource::Tag { image, .. } => Some(ImageRef::parse(image)),
            _ => None,
        }
    }

    /// The default for a static parameter; `None` for feeds.
    pub fn default_value(&self) -> Option<&str> {
        match self {
            ParameterSource::Value { default } => Some(default),
            ParameterSource::Commit { .. } | ParameterSource::Tag { .. } => None,
        }
    }

    /// The default channel of a feed: a branch for commits, a semver range
    /// for tags. `None` for static values, whose only channel is the default.
    pub fn default_channel(&self) -> Option<&str> {
        match self {
            ParameterSource::Commit { branch, .. } => Some(branch),
            ParameterSource::Tag { pattern, .. } => Some(pattern),
            ParameterSource::Value { .. } => None,
        }
    }

    pub fn is_tag(&self) -> bool {
        matches!(self, ParameterSource::Tag { .. })
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            ParameterSource::Commit { .. } => "commit",
            ParameterSource::Tag { .. } => "tag",
            ParameterSource::Value { .. } => "value",
        }
    }

    /// Build the map for the legacy single-artifact shape: either empty or a
    /// single commit source under [`SHA_PARAMETER`]. Test convenience now
    /// that config sync reads a full parameters block.
    #[cfg(test)]
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
            ParameterValue::Commit { value, .. }
            | ParameterValue::Tag { value, .. }
            | ParameterValue::Value { value } => value.clone(),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            ParameterValue::Commit { .. } => "commit",
            ParameterValue::Tag { .. } => "tag",
            ParameterValue::Value { .. } => "value",
        }
    }

    /// The channel the value was resolved from: a branch for commits, a
    /// range for tags. `None` means it was pinned (or is a static value).
    pub fn channel(&self) -> Option<&str> {
        match self {
            ParameterValue::Commit { branch, .. } => branch.as_deref(),
            ParameterValue::Tag { pattern, .. } => pattern.as_deref(),
            ParameterValue::Value { .. } => None,
        }
    }

    #[allow(dead_code)] // shown on the deploy page in the next change
    pub fn digest(&self) -> Option<&str> {
        match self {
            ParameterValue::Tag { digest, .. } => digest.as_deref(),
            _ => None,
        }
    }

    /// The SHA and branch, for values that are commit based.
    pub fn as_sha_maybe_branch(&self) -> Option<ShaMaybeBranch> {
        match self {
            ParameterValue::Commit { value, branch } => Some(ShaMaybeBranch {
                sha: value.clone(),
                branch: branch.clone(),
            }),
            ParameterValue::Tag { .. } | ParameterValue::Value { .. } => None,
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
    fn value_parameters_round_trip() -> Result<(), serde_json::Error> {
        let src: ParameterSource =
            serde_json::from_value(json!({"type": "value", "default": "2"}))?;
        assert_eq!(src.default_value(), Some("2"));
        assert!(src.as_repository_branch().is_none());
        assert_eq!(
            serde_json::to_value(&src)?,
            json!({"type": "value", "default": "2"})
        );
        let val: ParameterValue = serde_json::from_value(json!({"type": "value", "value": "5"}))?;
        assert_eq!(val.rendered(), "5");
        assert!(val.as_sha_maybe_branch().is_none());
        assert_eq!(val.channel(), None);
        Ok(())
    }

    #[test]
    fn unknown_type_is_rejected() {
        let res: Result<ParameterSource, _> =
            serde_json::from_value(json!({"type": "digest", "image": "a"}));
        assert!(res.is_err());
        let res: Result<ParameterValue, _> =
            serde_json::from_value(json!({"type": "digest", "value": "v1"}));
        assert!(res.is_err());
    }

    #[test]
    fn tag_parameters_round_trip() -> Result<(), serde_json::Error> {
        let src: ParameterSource = serde_json::from_value(
            json!({"type": "tag", "image": "docker.io/library/nginx", "pattern": "^1.27"}),
        )?;
        assert_eq!(src.default_channel(), Some("^1.27"));
        assert!(src.default_value().is_none());
        assert_eq!(
            src.image_ref(),
            Some(ImageRef {
                registry: "docker.io".into(),
                name: "library/nginx".into()
            })
        );
        let tracked: ParameterValue = serde_json::from_value(
            json!({"type": "tag", "value": "1.27.3", "pattern": "^1.27", "digest": "sha256:ab"}),
        )?;
        assert_eq!(tracked.rendered(), "1.27.3");
        assert_eq!(tracked.channel(), Some("^1.27"));
        assert_eq!(tracked.digest(), Some("sha256:ab"));
        let pinned: ParameterValue =
            serde_json::from_value(json!({"type": "tag", "value": "1.26.0"}))?;
        assert_eq!(pinned.channel(), None);
        assert_eq!(
            serde_json::to_value(&pinned)?,
            json!({"type": "tag", "value": "1.26.0"})
        );
        Ok(())
    }

    #[test]
    fn image_refs_split_like_the_docker_cli() {
        let r = |s: &str| ImageRef::parse(s);
        assert_eq!(r("nginx").name, "library/nginx");
        assert_eq!(r("nginx").registry, "docker.io");
        assert_eq!(r("kj800x/nginx").name, "kj800x/nginx");
        assert_eq!(r("docker.io/library/nginx").name, "library/nginx");
        assert_eq!(r("ghcr.io/kj800x/nginx").registry, "ghcr.io");
        assert_eq!(r("ghcr.io/kj800x/nginx").name, "kj800x/nginx");
        assert_eq!(r("localhost:5000/x").registry, "localhost:5000");
        assert_eq!(
            r("registry.k8s.io/external-dns/external-dns").name,
            "external-dns/external-dns"
        );
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
