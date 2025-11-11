use serde::{Deserialize, Serialize};

use crate::crab_ext::IRepo;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RepoOwner {
    pub login: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Repository {
    pub id: u64,
    pub name: String,
    pub owner: RepoOwner,
    pub private: bool,
    pub language: Option<String>,
    pub default_branch: String,
}

impl IRepo for Repository {
    fn owner(&self) -> &str {
        &self.owner.login
    }
    fn repo(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CommitAuthor {
    pub name: String,
    pub email: String,
    // pub username: String,
}

impl From<&CommitAuthor> for String {
    fn from(author: &CommitAuthor) -> Self {
        format!("{} <{}>", author.name, author.email)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CheckSuite {
    pub id: u64,
    pub head_sha: String,
    pub head_commit: Option<PushCommit>,
    pub status: String,
    pub conclusion: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CheckRun {
    pub details_url: String,
    pub check_suite: CheckSuite,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CheckRunEvent {
    pub action: String,
    pub check_run: CheckRun,
    pub repository: Repository,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PushCommit {
    pub id: String, // sha
    pub message: String,
    pub timestamp: String,
    pub author: CommitAuthor,
    pub committer: CommitAuthor,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DeleteEvent {
    pub r#ref: String,
    pub repository: Repository,
    pub ref_type: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PushEvent {
    pub r#ref: String, // like "refs/heads/branch-name"
    pub after: String,
    pub repository: Repository,
    pub head_commit: Option<PushCommit>,
    pub commits: Vec<PushCommit>,
    pub deleted: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WebhookEvent {
    pub event_type: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CheckSuiteEvent {
    pub action: String,
    pub check_suite: CheckSuite,
    pub repository: Repository,
}
