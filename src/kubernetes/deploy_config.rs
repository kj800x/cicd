use std::collections::BTreeMap;

use crate::kubernetes::{
    repo::{DeploymentState, RepositoryBranch, ShaMaybeBranch},
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
    /// Information about the current state of the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ShaMaybeBranch>,

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

    /// Repository information
    pub artifact: Option<RepositoryBranch>,

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
    printcolumn = r#"{"name":"Artifact Repo", "jsonPath":".spec.artifact.repo", "type":"string"}"#,
    printcolumn = r#"{"name":"Config Repo", "jsonPath":".spec.config.repo", "type":"string"}"#,
    printcolumn = r#"{"name":"Config SHA", "jsonPath":".status.config.sha", "type":"string"}"#,
    printcolumn = r#"{"name":"Artifact SHA", "jsonPath":".status.artifact.sha", "type":"string"}"#,
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
            if let Some(artifact) = self.status.as_ref().and_then(|s| s.artifact.as_ref()) {
                DeploymentState::DeployedWithArtifact {
                    artifact: artifact.clone(),
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

    /// Whether this deploy is "non-latest": a branch other than the artifact's
    /// tracking (default) branch, or a pinned SHA override (no branch).
    ///
    /// Consumers use this to fail closed on dangerous actions (e.g. refusing DB
    /// schema migrations on a branch/pinned deploy).
    pub fn is_non_latest_deploy(&self) -> bool {
        match self.deployment_state() {
            DeploymentState::DeployedWithArtifact { artifact, .. } => {
                // None (pinned SHA) never equals Some(default), so a pinned
                // deploy is always non-latest, as is any non-default branch.
                let default_branch = self.artifact_repository().map(|r| r.branch);
                artifact.branch.as_deref() != default_branch.as_deref()
            }
            // Config-only deploys don't track a config default branch yet, so
            // they can't be classified as non-latest. Wire this up when config
            // branch/SHA control lands.
            DeploymentState::DeployedOnlyConfig { .. } => false,
            DeploymentState::Undeployed => false,
        }
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

    /// Get a RepositoryBranch struct for the artifact
    pub fn artifact_repository(&self) -> Option<RepositoryBranch> {
        self.spec.spec.artifact.clone()
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
