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
            // A check that has been requested or is running but hasn't concluded.
            "queued" | "in_progress" | "waiting" | "requested" | "pending" => BuildStatus::Pending,
            "completed" => Self::from_conclusion(*conclusion),
            _ => BuildStatus::None,
        }
    }

    /// Map a GitHub check-run/check-suite `conclusion` to a build status.
    ///
    /// The REST "list check runs for a git ref" endpoint returns runs without a
    /// separate `status` field, so a missing conclusion means the run is still
    /// in flight (queued/in_progress) and should be treated as Pending.
    pub fn from_conclusion(conclusion: Option<&str>) -> Self {
        match conclusion {
            None => BuildStatus::Pending,
            // Treat non-failing terminal conclusions as success so they don't
            // block deploys (matches GitHub's own green-check rollup).
            Some("success") | Some("neutral") | Some("skipped") | Some("cancelled")
            | Some("stale") => BuildStatus::Success,
            Some("failure") | Some("timed_out") | Some("action_required")
            | Some("startup_failure") => BuildStatus::Failure,
            // Unknown/unexpected conclusions: don't block, but don't claim success.
            Some(_) => BuildStatus::None,
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
