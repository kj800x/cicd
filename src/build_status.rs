use serde::{Deserialize, Serialize};

use crate::db::git_commit_build::GitCommitBuild;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum BuildStatus {
    None,
    Pending,
    Success,
    Failure,
}

impl From<Option<String>> for BuildStatus {
    fn from(s: Option<String>) -> Self {
        match s {
            Some(s) => match s.as_str() {
                "None" => BuildStatus::None,
                "Pending" => BuildStatus::Pending,
                "Success" => BuildStatus::Success,
                "Failure" => BuildStatus::Failure,
                _ => BuildStatus::None,
            },
            None => BuildStatus::None,
        }
    }
}

impl BuildStatus {
    pub fn of(status: &str, conclusion: &Option<&str>) -> Self {
        match status {
            "queued" => BuildStatus::Pending,
            "completed" => match conclusion {
                Some("success") => BuildStatus::Success,
                Some("failure") => BuildStatus::Failure,
                _ => BuildStatus::None,
            },
            _ => BuildStatus::None,
        }
    }
}

impl From<BuildStatus> for String {
    fn from(val: BuildStatus) -> Self {
        match val {
            BuildStatus::None => "None".to_string(),
            BuildStatus::Pending => "Pending".to_string(),
            BuildStatus::Success => "Success".to_string(),
            BuildStatus::Failure => "Failure".to_string(),
        }
    }
}

impl From<GitCommitBuild> for BuildStatus {
    fn from(value: GitCommitBuild) -> Self {
        match value.status.as_str() {
            "None" => BuildStatus::None,
            "Pending" => BuildStatus::Pending,
            "Success" => BuildStatus::Success,
            "Failure" => BuildStatus::Failure,
            _ => BuildStatus::None,
        }
    }
}

impl From<Option<GitCommitBuild>> for BuildStatus {
    fn from(value: Option<GitCommitBuild>) -> Self {
        match value {
            Some(build) => build.into(),
            None => BuildStatus::None,
        }
    }
}
