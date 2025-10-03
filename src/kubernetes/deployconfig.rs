use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{CustomResource, ResourceExt};
use serde::{Deserialize, Serialize};

/// Represents repository information for a DeployConfig
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Repository {
    /// GitHub username or organization
    pub owner: String,
    /// Repository name
    pub repo: String,
    /// Default Git branch to track
    #[serde(rename = "defaultBranch")]
    pub default_branch: String,
}

/// DeployConfig status information
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DeployConfigStatus {
    /// The currently deployed Git commit SHA
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "currentSha"
    )]
    pub current_sha: Option<String>,
    /// The latest Git commit SHA for the configured branch
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "latestSha")]
    pub latest_sha: Option<String>,
    /// The Git commit SHA that should be deployed
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "wantedSha")]
    pub wanted_sha: Option<String>,
    /// The currently active branch
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "currentBranch"
    )]
    pub current_branch: Option<String>,
    /// The current state of autodeploy
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "autodeploy"
    )]
    pub autodeploy: Option<bool>,
}

/// DeployConfig spec fields represent the desired state for a deployment
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeployConfigSpecFields {
    /// Repository information
    pub repo: Repository,
    /// Autodeploy flag
    #[serde(default)]
    pub autodeploy: bool,
    /// Array of Kubernetes resource manifests
    #[serde(default)]
    pub specs: Vec<serde_json::Value>,
}

/// The DeployConfig CustomResource
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize)]
#[kube(
    group = "cicd.coolkev.com",
    version = "v1",
    kind = "TestDeployConfig",
    shortname = "tdc",
    namespaced,
    schema = "disabled",
    status = "DeployConfigStatus",
    printcolumn = r#"{"name":"Repo", "jsonPath":".spec.repo.repo", "type":"string"}"#,
    printcolumn = r#"{"name":"Branch", "jsonPath":".spec.repo.default_branch", "type":"string"}"#,
    printcolumn = r#"{"name":"Current SHA", "jsonPath":".status.currentSha", "type":"string"}"#,
    printcolumn = r#"{"name":"Latest SHA", "jsonPath":".status.latestSha", "type":"string"}"#,
    printcolumn = r#"{"name":"Wanted SHA", "jsonPath":".status.wantedSha", "type":"string"}"#,
    printcolumn = r#"{"name":"Autodeploy", "jsonPath":".spec.autodeploy", "type":"boolean"}"#
)]
pub struct DeployConfigSpec {
    /// Repository information and resource spec
    #[serde(flatten)]
    pub spec: DeployConfigSpecFields,
}

impl TestDeployConfig {
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
            .and_then(|s| s.wanted_sha.as_ref().map(|s| s.as_str()))
    }

    pub fn latest_sha(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|s| s.latest_sha.as_ref().map(|s| s.as_str()))
    }

    pub fn current_sha(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|s| s.current_sha.as_ref().map(|s| s.as_str()))
    }

    pub fn current_branch(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|s| s.current_branch.as_ref().map(|s| s.as_str()))
    }

    /// Returns the owner reference to be applied to child resources
    pub fn child_owner_reference(&self) -> OwnerReference {
        OwnerReference {
            api_version: String::from("cicd.coolkev.com/v1"),
            kind: String::from("DeployConfig"),
            name: self.name_any(),
            uid: self.uid().expect("DeployConfig should have a UID"),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }
}
