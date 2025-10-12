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
        self.status.as_ref().and_then(|s| {
            s.artifact
                .as_ref()
                .and_then(|a| a.wanted_sha.as_ref().map(|s| s.as_str()))
        })
    }

    pub fn latest_sha(&self) -> Option<&str> {
        self.status.as_ref().and_then(|s| {
            s.artifact
                .as_ref()
                .and_then(|a| a.latest_sha.as_ref().map(|s| s.as_str()))
        })
    }

    pub fn current_sha(&self) -> Option<&str> {
        self.status.as_ref().and_then(|s| {
            s.artifact
                .as_ref()
                .and_then(|a| a.current_sha.as_ref().map(|s| s.as_str()))
        })
    }

    pub fn current_branch(&self) -> Option<&str> {
        self.status.as_ref().and_then(|s| {
            s.artifact
                .as_ref()
                .and_then(|a| a.branch.as_ref().map(|s| s.as_str()))
        })
    }

    pub fn tracking_branch(&self) -> &str {
        self.status
            .as_ref()
            .and_then(|s| {
                s.artifact
                    .as_ref()
                    .and_then(|a| a.branch.as_ref().map(|s| s.as_str()))
            })
            .unwrap_or(&self.spec.spec.artifact.branch)
    }

    /// Returns the owner reference to be applied to child resources
    pub fn child_owner_reference(&self) -> OwnerReference {
        OwnerReference {
            api_version: String::from("cicd.coolkev.com/v1"),
            kind: String::from(DEPLOY_CONFIG_KIND),
            name: self.name_any(),
            uid: self.uid().expect("DeployConfig should have a UID"),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }
}
