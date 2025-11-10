use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};

use crate::{
    crab_ext::IRepo,
    db::{git_branch::GitBranch, git_repo::GitRepo},
    error::AppError,
    web::BuildFilter,
};

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

impl Repository {
    pub fn with_branch(&self, branch: &str) -> RepositoryBranch {
        RepositoryBranch {
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            branch: branch.to_string(),
        }
    }
}

impl RepositoryBranch {
    pub fn into_repo(self) -> Repository {
        Repository {
            owner: self.owner,
            repo: self.repo,
        }
    }
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
impl ShaMaybeBranch {
    pub fn latest_for_branch(
        repo: Repository,
        branch: &str,
        build_filter: crate::web::BuildFilter,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> Result<ShaMaybeBranch, AppError> {
        let repo = GitRepo::get(repo.clone(), conn)?.ok_or(AppError::NotFound(format!(
            "Repository not found: {}/{}",
            repo.owner(),
            repo.repo()
        )))?;

        let branch =
            GitBranch::get_by_name(branch, repo.id, conn)?.ok_or(AppError::NotFound(format!(
                "Branch not found: {} in {}/{}",
                repo.owner(),
                repo.repo(),
                branch
            )))?;

        let commit = match build_filter {
            BuildFilter::Any => branch.latest_build(conn).ok().flatten(),
            BuildFilter::Completed => branch.latest_completed_build(conn).ok().flatten(),
            BuildFilter::Successful => branch.latest_successful_build(conn).ok().flatten(),
        }
        .ok_or(AppError::NotFound(format!(
            "No build found for branch: {} in {}/{}",
            repo.owner(),
            repo.repo(),
            branch.name
        )))?;

        Ok(ShaMaybeBranch {
            sha: commit.sha,
            branch: Some(branch.name),
        })
    }
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

impl DeploymentState {
    // FIXME: This probably should be called "tracking branch" or something
    pub fn artifact_branch(&self) -> Option<&str> {
        match self {
            DeploymentState::DeployedWithArtifact { artifact, .. } => artifact.branch.as_deref(),
            DeploymentState::DeployedOnlyConfig { config, .. } => config.branch.as_deref(),
            DeploymentState::Undeployed => None,
        }
    }
}
