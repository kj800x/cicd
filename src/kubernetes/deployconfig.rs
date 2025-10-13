use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{CustomResource, ResourceExt};
use serde::{Deserialize, Serialize};

pub const DEPLOY_CONFIG_KIND: &str = if cfg!(feature = "test-crd") {
    "TestDeployConfig"
} else {
    "DeployConfig"
};

pub trait IRepo {
    fn owner(&self) -> &str;
    fn repo(&self) -> &str;
}

impl IRepo for Repository {
    fn owner(&self) -> &str {
        &self.owner
    }
    fn repo(&self) -> &str {
        &self.repo
    }
}

impl IRepo for RepositoryBranch {
    fn owner(&self) -> &str {
        &self.owner
    }
    fn repo(&self) -> &str {
        &self.repo
    }
}

/// Represents repository information (without branch) for a DeployConfig
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Repository {
    /// GitHub username or organization
    pub owner: String,

    /// Repository name
    pub repo: String,
}

/// Represents repository information (including branch) for a DeployConfig
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RepositoryBranch {
    /// GitHub username or organization
    pub owner: String,

    /// Repository name
    pub repo: String,

    /// Default Git branch to track
    pub branch: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DeployConfigArtifactStatus {
    /// The currently deployed Git commit SHA
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "currentSha"
    )]
    pub current_sha: Option<String>,

    /// The Git commit SHA that should be deployed
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "wantedSha")]
    pub wanted_sha: Option<String>,

    /// The latest Git commit SHA for the configured branch
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "latestSha")]
    pub latest_sha: Option<String>,

    /// The currently active branch
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "branch")]
    pub branch: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DeployConfigConfigStatus {
    /// GitHub username or organization containing this deploy config
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,

    /// Repository name containing this deploy config
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,

    /// The SHA for the current version of the config (specs and metadata)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

/// DeployConfig status information
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DeployConfigStatus {
    /// Information about the current state of the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<DeployConfigArtifactStatus>,

    /// Information about the current state of the config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<DeployConfigConfigStatus>,

    /// The current state of autodeploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autodeploy: Option<bool>,
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
    pub artifact: RepositoryBranch,

    /// Autodeploy flag
    #[serde(default)]
    pub autodeploy: bool,

    /// Array of Kubernetes resource manifests
    #[serde(default)] // FIXME: Should this be (default)?
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
    printcolumn = r#"{"name":"Repo", "jsonPath":".spec.artifact.repo", "type":"string"}"#,
    printcolumn = r#"{"name":"Branch", "jsonPath":".spec.artifact.branch", "type":"string"}"#,
    printcolumn = r#"{"name":"Current SHA", "jsonPath":".status.artifact.currentSha", "type":"string"}"#,
    printcolumn = r#"{"name":"Latest SHA", "jsonPath":".status.artifact.latestSha", "type":"string"}"#,
    printcolumn = r#"{"name":"Wanted SHA", "jsonPath":".status.artifact.wantedSha", "type":"string"}"#,
    printcolumn = r#"{"name":"Autodeploy", "jsonPath":".spec.autodeploy", "type":"boolean"}"#
)]
pub struct DeployConfigSpec {
    /// Repository information and resource spec
    #[serde(flatten)]
    pub spec: DeployConfigSpecFields,
}

#[cfg(feature = "test-crd")]
pub type DeployConfig = TestDeployConfig;

impl DeployConfig {
    /// Get the current autodeploy state, falling back to the spec's autodeploy if not set in status
    pub fn current_autodeploy(&self) -> bool {
        self.status
            .as_ref()
            .and_then(|s| s.autodeploy)
            .unwrap_or(self.spec.spec.autodeploy)
    }

    pub fn wanted_sha(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|s| s.artifact.as_ref().and_then(|a| a.wanted_sha.as_deref()))
    }

    pub fn latest_sha(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|s| s.artifact.as_ref().and_then(|a| a.latest_sha.as_deref()))
    }

    pub fn current_sha(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|s| s.artifact.as_ref().and_then(|a| a.current_sha.as_deref()))
    }

    pub fn current_branch(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|s| s.artifact.as_ref().and_then(|a| a.branch.as_deref()))
    }

    pub fn tracking_branch(&self) -> &str {
        self.status
            .as_ref()
            .and_then(|s| s.artifact.as_ref().and_then(|a| a.branch.as_deref()))
            .unwrap_or(&self.spec.spec.artifact.branch)
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

    /// Get the owner of the artifact repository
    pub fn artifact_owner(&self) -> &str {
        &self.spec.spec.artifact.owner
    }

    /// Get the name of the artifact repository
    pub fn artifact_repo(&self) -> &str {
        &self.spec.spec.artifact.repo
    }

    /// Get the default branch from spec
    pub fn default_branch(&self) -> &str {
        &self.spec.spec.artifact.branch
    }

    /// Get the team name
    pub fn team(&self) -> &str {
        &self.spec.spec.team
    }

    /// Get the kind
    pub fn kind(&self) -> &str {
        &self.spec.spec.kind
    }

    /// Check if tracking the default branch
    pub fn is_tracking_default_branch(&self) -> bool {
        self.tracking_branch() == self.default_branch()
    }

    /// Check if autodeploy matches spec
    pub fn autodeploy_matches_spec(&self) -> bool {
        self.current_autodeploy() == self.spec.spec.autodeploy
    }

    /// Get a Repository struct for the artifact
    pub fn artifact_repository(&self) -> Repository {
        Repository {
            owner: self.spec.spec.artifact.owner.clone(),
            repo: self.spec.spec.artifact.repo.clone(),
        }
    }

    /// Get fully qualified name (namespace/name)
    pub fn qualified_name(&self) -> String {
        format!(
            "{}/{}",
            self.namespace().unwrap_or_default(),
            self.name_any()
        )
    }

    /// Check if deployment is undeployed
    pub fn is_undeployed(&self) -> bool {
        self.wanted_sha().is_none()
    }

    /// Check if latest and wanted are in sync
    pub fn is_in_sync(&self) -> bool {
        match (self.latest_sha(), self.wanted_sha()) {
            (Some(latest), Some(wanted)) => latest == wanted,
            (None, None) => true,
            _ => false,
        }
    }

    /// Get the autodeploy value from the spec (not the current status)
    pub fn spec_autodeploy(&self) -> bool {
        self.spec.spec.autodeploy
    }

    /// Get the Kubernetes resource specs
    pub fn resource_specs(&self) -> &[serde_json::Value] {
        &self.spec.spec.specs
    }
}

/// Builder for patch updates to DeployConfigStatus.
/// Since the values are optional, we need to use Option<Option<String>> to represent them in this builder.
/// The outer Option is for if this builder will be setting the value in the patch, the inner Option is the actual value that will be set on the status.
/// For example, owner: None means no patch for owner, owner: Some(None) means patch for owner but set to None, owner: Some(Some(owner)) means patch for owner and set to the given value.
#[derive(Clone, Debug, Default)]
pub struct DeployConfigStatusBuilder {
    autodeploy: Option<Option<bool>>,
    config_owner: Option<Option<String>>,
    config_repo: Option<Option<String>>,
    config_sha: Option<Option<String>>,
    artifact_branch: Option<Option<String>>,
    artifact_current_sha: Option<Option<String>>,
    artifact_latest_sha: Option<Option<String>>,
    artifact_wanted_sha: Option<Option<String>>,
}

// FIXME: We shouldn't need to do this, because you don't need the existing status to build a patch for a deploy event.
impl From<&DeployConfigStatus> for DeployConfigStatusBuilder {
    fn from(val: &DeployConfigStatus) -> Self {
        Self {
            autodeploy: Some(val.autodeploy),
            config_owner: Some(val.config.as_ref().and_then(|c| c.owner.clone())),
            config_repo: Some(val.config.as_ref().and_then(|c| c.repo.clone())),
            config_sha: Some(val.config.as_ref().and_then(|c| c.sha.clone())),
            artifact_branch: Some(val.artifact.as_ref().and_then(|a| a.branch.clone())),
            artifact_current_sha: Some(val.artifact.as_ref().and_then(|a| a.current_sha.clone())),
            artifact_latest_sha: Some(val.artifact.as_ref().and_then(|a| a.latest_sha.clone())),
            artifact_wanted_sha: Some(val.artifact.as_ref().and_then(|a| a.wanted_sha.clone())),
        }
    }
}

impl From<DeployConfigStatusBuilder> for serde_json::Value {
    fn from(val: DeployConfigStatusBuilder) -> Self {
        let mut config = serde_json::json!({});
        if let Some(owner) = val.config_owner {
            config["owner"] = owner.into();
        }
        if let Some(repo) = val.config_repo {
            config["repo"] = repo.into();
        }
        if let Some(sha) = val.config_sha {
            config["sha"] = sha.into();
        }

        let mut artifact = serde_json::json!({});
        if let Some(branch) = val.artifact_branch {
            artifact["branch"] = branch.into();
        }
        if let Some(current_sha) = val.artifact_current_sha {
            artifact["currentSha"] = current_sha.into();
        }
        if let Some(latest_sha) = val.artifact_latest_sha {
            artifact["latestSha"] = latest_sha.into();
        }
        if let Some(wanted_sha) = val.artifact_wanted_sha {
            artifact["wantedSha"] = wanted_sha.into();
        }

        let mut status = serde_json::json!({});
        if let Some(autodeploy) = val.autodeploy {
            status["autodeploy"] = autodeploy.into();
        }
        if let Some(config) = config.as_object() {
            if !config.is_empty() {
                status["config"] = serde_json::Value::Object(config.clone());
            }
        }
        if let Some(artifact) = artifact.as_object() {
            if !artifact.is_empty() {
                status["artifact"] = serde_json::Value::Object(artifact.clone());
            }
        }

        serde_json::json!({
            "status": status,
        })
    }
}

impl DeployConfigStatusBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_autodeploy(mut self, autodeploy: bool) -> Self {
        self.autodeploy = Some(Some(autodeploy));
        self
    }

    pub fn with_config_owner(mut self, owner: String) -> Self {
        self.config_owner = Some(Some(owner));
        self
    }

    pub fn with_config_repo(mut self, repo: String) -> Self {
        self.config_repo = Some(Some(repo));
        self
    }

    pub fn with_config_sha(mut self, sha: String) -> Self {
        self.config_sha = Some(Some(sha));
        self
    }

    pub fn with_artifact_branch(mut self, branch: String) -> Self {
        self.artifact_branch = Some(Some(branch));
        self
    }

    pub fn with_artifact_current_sha(mut self, current_sha: String) -> Self {
        self.artifact_current_sha = Some(Some(current_sha));
        self
    }

    pub fn with_null_artifact_current_sha(mut self) -> Self {
        self.artifact_current_sha = Some(None);
        self
    }

    pub fn with_artifact_latest_sha(mut self, latest_sha: String) -> Self {
        self.artifact_latest_sha = Some(Some(latest_sha));
        self
    }

    pub fn with_artifact_wanted_sha(mut self, wanted_sha: String) -> Self {
        self.artifact_wanted_sha = Some(Some(wanted_sha));
        self
    }

    pub fn get_artifact_wanted_sha(&self) -> Option<&str> {
        self.artifact_wanted_sha
            .as_ref()
            .and_then(|s| s.as_ref().map(|s| s.as_str()))
    }
}
