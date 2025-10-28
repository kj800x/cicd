use serde::{Deserialize, Serialize};

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

/// Represents a SHA and optionally a branch that the SHA came from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ShaMaybeBranch {
    #[serde(default)]
    pub sha: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum DeploymentState {
    Undeployed,
    DeployedOnlyConfig {
        config: ShaMaybeBranch,
    },
    DeployedWithArtifact {
        artifact: ShaMaybeBranch,
        config: ShaMaybeBranch,
    },
}
