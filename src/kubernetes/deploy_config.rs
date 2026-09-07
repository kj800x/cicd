use std::collections::BTreeMap;

use crate::kubernetes::{
    parameters::{
        ParameterSource, ParameterSources, ParameterValue, ParameterValues, SHA_PARAMETER,
    },
    repo::{DeploymentState, RepositoryBranch, ShaMaybeBranch},
    selections::{Durability, Selection, Selections},
    Repository,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{api::DynamicObject, CustomResource, ResourceExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEPLOY_CONFIG_KIND: &str = if cfg!(feature = "test-crd") {
    "TestDeployConfig"
} else {
    "DeployConfig"
};

/// DeployConfig status information
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DeployConfigStatus {
    /// The value currently deployed for each named parameter.
    /// The legacy artifact lives under [`SHA_PARAMETER`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: ParameterValues,

    /// Information about the current state of the config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ShaMaybeBranch>,

    /// The current state of autodeploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autodeploy: Option<bool>,

    /// Whether the deploy config is orphaned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphaned: Option<bool>,
}

/// DeployConfig spec fields represent the desired state for a deployment
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeployConfigSpecFields {
    /// Team
    pub team: String,

    /// Kind of deployable.
    /// Typed as a string to allow for future flexibility.
    /// Right now valid values are "service", "worker", "job", "meta", etc.
    pub kind: String,

    /// Named parameters. The legacy artifact repo lives under
    /// [`SHA_PARAMETER`], whose value is substituted for `$SHA` in specs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: ParameterSources,

    /// What each parameter is following: an override channel, a pin, or
    /// (absent) the source's default channel. Written by the deploy handler
    /// only; config sync never touches it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub selections: Selections,

    /// Repository information
    pub config: Repository,

    /// Array of Kubernetes resource manifests
    #[serde(default)]
    pub specs: Vec<serde_json::Value>,
}

/// The DeployConfig CustomResource
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(
    feature = "test-crd",
    kube(kind = "TestDeployConfig", shortname = "tdc")
)]
#[cfg_attr(
    not(feature = "test-crd"),
    kube(kind = "DeployConfig", shortname = "dc")
)]
#[kube(
    group = "cicd.coolkev.com",
    version = "v1",
    namespaced,
    schema = "disabled",
    status = "DeployConfigStatus",
    printcolumn = r#"{"name":"Team", "jsonPath":".spec.team", "type": "string"}"#,
    printcolumn = r#"{"name":"Kind", "jsonPath":".spec.kind", "type": "string"}"#,
    printcolumn = r#"{"name":"Artifact Repo", "jsonPath":".spec.parameters.SHA.repo", "type":"string"}"#,
    printcolumn = r#"{"name":"Config Repo", "jsonPath":".spec.config.repo", "type":"string"}"#,
    printcolumn = r#"{"name":"Config SHA", "jsonPath":".status.config.sha", "type":"string"}"#,
    printcolumn = r#"{"name":"Artifact SHA", "jsonPath":".status.parameters.SHA.value", "type":"string"}"#,
    printcolumn = r#"{"name":"Autodeploy", "jsonPath":".status.autodeploy", "type":"boolean"}"#,
    printcolumn = r#"{"name":"Age", "jsonPath":".metadata.creationTimestamp", "type":"date"}"#,
    printcolumn = r#"{"name":"Orphaned", "jsonPath":".status.orphaned", "type":"boolean"}"#
)]
pub struct DeployConfigSpec {
    /// Repository information and resource spec
    #[serde(flatten)]
    pub spec: DeployConfigSpecFields,
}

#[cfg(feature = "test-crd")]
pub type DeployConfig = TestDeployConfig;

impl DeployConfig {
    pub fn autodeploy(&self) -> bool {
        self.status
            .as_ref()
            .and_then(|s| s.autodeploy)
            .unwrap_or(false)
    }

    pub fn is_orphaned(&self) -> bool {
        self.status
            .as_ref()
            .and_then(|s| s.orphaned)
            .unwrap_or(false)
    }

    /// Whether this config depends on `repo`, either as the config repo
    /// (where its `.deploy/` manifests live) or as the artifact repo (where
    /// its image is built). GitHub owner and repo names are
    /// case-insensitive, so the comparison is too.
    pub fn references_repo(&self, repo: &Repository) -> bool {
        let same = |owner: &str, name: &str| {
            owner.eq_ignore_ascii_case(&repo.owner) && name.eq_ignore_ascii_case(&repo.repo)
        };

        let config = &self.spec.spec.config;
        if same(&config.owner, &config.repo) {
            return true;
        }

        self.spec
            .spec
            .parameters
            .values()
            .filter_map(ParameterSource::as_repository_branch)
            .any(|source| same(&source.owner, &source.repo))
    }

    /// The source of the [`SHA_PARAMETER`] parameter.
    fn sha_source(&self) -> Option<RepositoryBranch> {
        self.spec
            .spec
            .parameters
            .get(SHA_PARAMETER)
            .and_then(ParameterSource::as_repository_branch)
    }

    /// The deployed value of the [`SHA_PARAMETER`] parameter.
    fn sha_value(&self) -> Option<ShaMaybeBranch> {
        self.status
            .as_ref()?
            .parameters
            .get(SHA_PARAMETER)
            .and_then(ParameterValue::as_sha_maybe_branch)
    }

    pub fn supports_bounce(&self) -> bool {
        self.resource_specs().iter().any(|spec| {
            spec.get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or_default()
                == "Deployment"
        })
    }

    pub fn supports_execute_job(&self) -> bool {
        self.resource_specs().iter().any(|spec| {
            spec.get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or_default()
                == "CronJob"
        })
    }

    pub fn deployment_state(&self) -> DeploymentState {
        if let Some(config) = self.status.as_ref().and_then(|s| s.config.as_ref()) {
            if let Some(artifact) = self.sha_value() {
                DeploymentState::DeployedWithArtifact {
                    artifact,
                    config: config.clone(),
                }
            } else {
                DeploymentState::DeployedOnlyConfig {
                    config: config.clone(),
                }
            }
        } else {
            DeploymentState::Undeployed
        }
    }

    /// The effective selection for a parameter: the explicit one under
    /// `spec.selections`, or one derived from what is deployed.
    ///
    /// The derivation is the rule the code always used implicitly: a
    /// deployed value with no branch is a pin, a branch other than the
    /// source's default is an override, the default branch is the default.
    /// Derived overrides default to temporary for branches and standing for
    /// pins, matching what the UI proposes when making them explicit.
    pub fn selection(&self, parameter: &str) -> Selection {
        if let Some(explicit) = self.spec.spec.selections.get(parameter) {
            return explicit.clone();
        }
        if parameter != SHA_PARAMETER {
            return Selection::default();
        }
        let default_branch = self.artifact_repository().map(|r| r.branch);
        match self.sha_value() {
            Some(ShaMaybeBranch { sha, branch: None }) => {
                Selection::pin(&sha, Durability::Standing)
            }
            Some(ShaMaybeBranch {
                branch: Some(branch),
                ..
            }) if Some(branch.as_str()) != default_branch.as_deref() => {
                Selection::track(&branch, Durability::Temporary)
            }
            _ => Selection::default(),
        }
    }

    /// Whether the `SHA` parameter is overridden or pinned, whatever the
    /// durability. Apps use this to fail closed on dangerous actions such
    /// as schema migrations.
    pub fn is_non_latest_deploy(&self) -> bool {
        match self.deployment_state() {
            DeploymentState::DeployedWithArtifact { .. } => {
                self.selection(SHA_PARAMETER).is_override()
            }
            DeploymentState::DeployedOnlyConfig { .. } | DeploymentState::Undeployed => false,
        }
    }

    /// A temporary deployment has at least one temporary override active.
    /// It is what the badge, the homepage list and autodeploy's suspension
    /// key on. Standing overrides (a pinned dependency, a replica bump) are
    /// ordinary operation and do not count.
    #[allow(dead_code)] // the deploy page and env vars use this in the next PRs
    pub fn is_temporary_deployment(&self) -> bool {
        if matches!(self.deployment_state(), DeploymentState::Undeployed) {
            return false;
        }
        let mut names: Vec<&str> = self
            .spec
            .spec
            .selections
            .keys()
            .map(String::as_str)
            .collect();
        if !names.contains(&SHA_PARAMETER) {
            names.push(SHA_PARAMETER);
        }
        names.into_iter().any(|n| self.selection(n).is_temporary())
    }

    /// The `CICD_*` environment variables to inject into every container of the
    /// deployed workloads. Describes the deploy so apps can report their version
    /// and make deploy-aware decisions (see [`Self::is_non_latest_deploy`]).
    pub fn deploy_env_vars(&self) -> Vec<(String, String)> {
        let mut vars: Vec<(String, String)> = vec![
            ("CICD_DEPLOY_CONFIG".to_string(), self.name_any()),
            ("CICD_TEAM".to_string(), self.team().to_string()),
            (
                "CICD_NON_LATEST_DEPLOY".to_string(),
                self.is_non_latest_deploy().to_string(),
            ),
        ];

        if let Some(default_branch) = self.artifact_repository().map(|r| r.branch) {
            vars.push(("CICD_DEFAULT_BRANCH".to_string(), default_branch));
        }

        match self.deployment_state() {
            DeploymentState::DeployedWithArtifact { artifact, config } => {
                vars.push(("CICD_ARTIFACT_SHA".to_string(), artifact.sha));
                vars.push((
                    "CICD_ARTIFACT_BRANCH".to_string(),
                    artifact.branch.unwrap_or_default(),
                ));
                vars.push(("CICD_CONFIG_SHA".to_string(), config.sha));
                vars.push((
                    "CICD_CONFIG_BRANCH".to_string(),
                    config.branch.unwrap_or_default(),
                ));
            }
            DeploymentState::DeployedOnlyConfig { config } => {
                vars.push(("CICD_CONFIG_SHA".to_string(), config.sha));
                vars.push((
                    "CICD_CONFIG_BRANCH".to_string(),
                    config.branch.unwrap_or_default(),
                ));
            }
            DeploymentState::Undeployed => {}
        }

        vars
    }

    /// Returns the owner reference to be applied to child resources
    pub fn child_owner_reference(&self) -> OwnerReference {
        OwnerReference {
            api_version: String::from("cicd.coolkev.com/v1"),
            kind: String::from(DEPLOY_CONFIG_KIND),
            name: self.name_any(),
            #[allow(clippy::expect_used)]
            uid: self.uid().expect("DeployConfig should have a UID"),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    /// Get the team name
    pub fn team(&self) -> &str {
        &self.spec.spec.team
    }

    /// Get the kind
    pub fn kind(&self) -> &str {
        &self.spec.spec.kind
    }

    /// Get a RepositoryBranch struct for the artifact (the [`SHA_PARAMETER`] parameter)
    pub fn artifact_repository(&self) -> Option<RepositoryBranch> {
        self.sha_source()
    }

    /// Get a Repository struct for the config
    pub fn config_repository(&self) -> Repository {
        self.spec.spec.config.clone()
    }

    /// Get the Kubernetes resource specs
    pub fn resource_specs(&self) -> &[serde_json::Value] {
        &self.spec.spec.specs
    }

    #[allow(clippy::expect_used)]
    pub fn spec_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(serde_json::to_string(&self.spec.spec).expect("Failed to serialize spec"));
        format!("{:x}", hasher.finalize())
    }

    pub fn owns(&self, obj: &DynamicObject) -> bool {
        let Some(owners) = &obj.metadata.owner_references else {
            return false;
        };

        #[allow(clippy::expect_used)]
        owners
            .iter()
            .any(|or| or.uid == self.uid().expect("DeployConfig should have a UID"))
    }

    pub fn child_is_up_to_date(&self, obj: &DynamicObject) -> bool {
        match self.deployment_state() {
            DeploymentState::DeployedWithArtifact { artifact, config } => obj
                .metadata
                .annotations
                .as_ref()
                .map(|a| {
                    a.get("artifactSha").is_some_and(|sha| sha == &artifact.sha)
                        && a.get("configSha").is_some_and(|sha| sha == &config.sha)
                })
                .unwrap_or(false),

            DeploymentState::DeployedOnlyConfig { config } => obj
                .metadata
                .annotations
                .as_ref()
                .map(|a| a.get("configSha").is_some_and(|sha| sha == &config.sha))
                .unwrap_or(false),

            DeploymentState::Undeployed => false,
        }
    }

    /// Ensure the annotations are set on a child resource
    pub fn ensure_annotations<T: ResourceExt>(&self, resource: &mut T) {
        let annotations = resource
            .meta_mut()
            .annotations
            .get_or_insert_with(BTreeMap::new);

        match self.deployment_state() {
            DeploymentState::DeployedWithArtifact { artifact, config } => {
                annotations.insert("artifactSha".to_string(), artifact.sha);
                annotations.insert("configSha".to_string(), config.sha);
            }
            DeploymentState::DeployedOnlyConfig { config } => {
                annotations.insert("configSha".to_string(), config.sha);
            }
            DeploymentState::Undeployed => {}
        }
    }

    /// Ensure the labels are set on a child resource
    pub fn ensure_labels<T: ResourceExt>(&self, resource: &mut T) {
        let labels = resource.meta_mut().labels.get_or_insert_with(BTreeMap::new);
        labels.insert(
            "app.kubernetes.io/managed-by".to_string(),
            "cicd-controller".to_string(),
        );
    }

    /// Ensure the owner reference is set on a child resource
    pub fn ensure_owner_reference<T: ResourceExt>(&self, resource: &mut T) {
        // Get the current owner references or create an empty vec
        let owner_refs = resource
            .meta_mut()
            .owner_references
            .get_or_insert_with(Vec::new);

        // Check if owner reference for this DeployConfig already exists
        let owner_ref_exists = owner_refs.iter().any(|ref_| {
            ref_.kind == DEPLOY_CONFIG_KIND
                && ref_.name == self.name_any()
                && ref_.api_version == "cicd.coolkev.com/v1"
        });

        // If it doesn't exist, add it
        if !owner_ref_exists {
            owner_refs.push(self.child_owner_reference());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(owner: &str, repo: &str) -> Repository {
        Repository {
            owner: owner.to_string(),
            repo: repo.to_string(),
        }
    }

    fn config(config_repo: Repository, artifact_repo: Option<Repository>) -> DeployConfig {
        DeployConfig::new(
            "test",
            DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    team: "test".to_string(),
                    kind: "service".to_string(),
                    parameters: ParameterSource::sha_map(
                        artifact_repo.map(|r| r.with_branch("master")),
                    ),
                    selections: Selections::default(),
                    config: config_repo,
                    specs: vec![],
                },
            },
        )
    }

    #[test]
    fn references_config_repo() {
        let dc = config(repo("kj800x", "app"), None);
        assert!(dc.references_repo(&repo("kj800x", "app")));
        assert!(!dc.references_repo(&repo("kj800x", "other")));
        assert!(!dc.references_repo(&repo("someone-else", "app")));
    }

    #[test]
    fn references_artifact_repo() {
        let dc = config(
            repo("sqrt10pi", "wr-rs"),
            Some(repo("kj800x", "site-server")),
        );
        assert!(dc.references_repo(&repo("sqrt10pi", "wr-rs")));
        assert!(dc.references_repo(&repo("kj800x", "site-server")));
        assert!(!dc.references_repo(&repo("kj800x", "wr-rs")));
    }

    #[test]
    fn references_repo_is_case_insensitive() {
        let dc = config(repo("kj800x", "Hello-World"), None);
        assert!(dc.references_repo(&repo("KJ800X", "hello-world")));
    }

    #[test]
    fn references_repo_sees_parameter_sources() {
        let mut dc = config(repo("kj800x", "app"), None);
        dc.spec.spec.parameters.insert(
            SHA_PARAMETER.to_string(),
            ParameterSource::from(repo("kj800x", "image").with_branch("master")),
        );
        assert!(dc.references_repo(&repo("kj800x", "image")));
        assert!(!dc.references_repo(&repo("kj800x", "unrelated")));
    }

    fn dc_json(
        spec_extra: serde_json::Value,
        status_extra: serde_json::Value,
    ) -> serde_json::Value {
        let mut json = serde_json::json!({
            "apiVersion": "cicd.coolkev.com/v1",
            "kind": DEPLOY_CONFIG_KIND,
            "metadata": {"name": "cicd", "namespace": "cicd"},
            "spec": {
                "config": {"owner": "kj800x", "repo": "cicd"},
                "kind": "service",
                "team": "cluster-infra",
                "specs": []
            },
            "status": {
                "config": {"sha": "cfg", "branch": "master"},
                "orphaned": false
            }
        });
        if let (Some(spec), Some(extra)) = (json["spec"].as_object_mut(), spec_extra.as_object()) {
            spec.extend(extra.clone());
        }
        if let (Some(status), Some(extra)) =
            (json["status"].as_object_mut(), status_extra.as_object())
        {
            status.extend(extra.clone());
        }
        json
    }

    fn param_source() -> serde_json::Value {
        serde_json::json!({"parameters": {"SHA": {"type": "commit", "owner": "kj800x", "repo": "cicd", "branch": "master"}}})
    }

    fn param_value(sha: &str) -> serde_json::Value {
        serde_json::json!({"parameters": {"SHA": {"type": "commit", "value": sha, "branch": "master"}}})
    }

    fn deployed_sha(dc: &DeployConfig) -> Option<String> {
        match dc.deployment_state() {
            DeploymentState::DeployedWithArtifact { artifact, .. } => Some(artifact.sha),
            _ => None,
        }
    }

    #[test]
    fn parameters_shape_reads() -> Result<(), serde_json::Error> {
        let dc: DeployConfig = serde_json::from_value(dc_json(param_source(), param_value("new")))?;
        assert_eq!(
            dc.artifact_repository().map(|r| r.branch),
            Some("master".into())
        );
        assert_eq!(deployed_sha(&dc), Some("new".into()));
        assert!(!dc.is_non_latest_deploy());
        Ok(())
    }

    fn with_status(mut dc: DeployConfig, sha: &str, branch: Option<&str>) -> DeployConfig {
        let mut status = DeployConfigStatus {
            config: Some(ShaMaybeBranch {
                sha: "cfg".into(),
                branch: Some("master".into()),
            }),
            ..Default::default()
        };
        status.parameters.insert(
            SHA_PARAMETER.into(),
            ParameterValue::from(ShaMaybeBranch {
                sha: sha.into(),
                branch: branch.map(String::from),
            }),
        );
        dc.status = Some(status);
        dc
    }

    #[test]
    fn selection_is_derived_from_the_deployed_branch() {
        use crate::kubernetes::selections::Mode;
        let base = config(repo("kj800x", "app"), Some(repo("kj800x", "app")));

        let tracking = with_status(base.clone(), "abc", Some("master"));
        assert_eq!(tracking.selection(SHA_PARAMETER).mode(), Mode::Default);
        assert!(!tracking.is_non_latest_deploy());
        assert!(!tracking.is_temporary_deployment());

        let branch = with_status(base.clone(), "abc", Some("feature"));
        assert_eq!(
            branch.selection(SHA_PARAMETER).mode(),
            Mode::Track("feature")
        );
        assert!(branch.is_non_latest_deploy());
        assert!(
            branch.is_temporary_deployment(),
            "derived branch overrides are temporary"
        );

        let pinned = with_status(base.clone(), "abc", None);
        assert_eq!(pinned.selection(SHA_PARAMETER).mode(), Mode::Pin("abc"));
        assert!(pinned.is_non_latest_deploy());
        assert!(
            !pinned.is_temporary_deployment(),
            "derived pins are standing"
        );

        assert!(
            !base.is_temporary_deployment(),
            "undeployed is never temporary"
        );
    }

    #[test]
    fn explicit_selection_wins_over_derivation() {
        use crate::kubernetes::selections::Mode;
        let mut dc = with_status(
            config(repo("kj800x", "app"), Some(repo("kj800x", "app"))),
            "abc",
            None,
        );
        dc.spec.spec.selections.insert(
            SHA_PARAMETER.into(),
            Selection::pin("abc", Durability::Temporary),
        );
        assert_eq!(dc.selection(SHA_PARAMETER).mode(), Mode::Pin("abc"));
        assert!(
            dc.is_temporary_deployment(),
            "explicit durability is respected"
        );
        let out = serde_json::to_value(&dc).unwrap_or_default();
        assert_eq!(out["spec"]["selections"]["SHA"]["pin"]["value"], "abc");
        assert_eq!(out["spec"]["selections"]["SHA"]["durability"], "temporary");
    }

    #[test]
    fn no_sha_anywhere_is_config_only() -> Result<(), serde_json::Error> {
        let dc: DeployConfig =
            serde_json::from_value(dc_json(serde_json::json!({}), serde_json::json!({})))?;
        assert!(dc.artifact_repository().is_none());
        assert!(matches!(
            dc.deployment_state(),
            DeploymentState::DeployedOnlyConfig { .. }
        ));
        Ok(())
    }

    #[test]
    fn empty_maps_are_not_serialized() -> Result<(), serde_json::Error> {
        let dc: DeployConfig =
            serde_json::from_value(dc_json(serde_json::json!({}), serde_json::json!({})))?;
        let out = serde_json::to_value(&dc)?;
        assert!(out["spec"].get("parameters").is_none());
        assert!(out["status"].get("parameters").is_none());
        Ok(())
    }
}
