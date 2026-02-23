use anyhow::Context;
use chrono::Utc;
use opentelemetry::KeyValue;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serenity::async_trait;

use crate::{
    build_status::BuildStatus,
    db::{git_commit::GitCommit, git_commit_build::GitCommitBuild, git_repo::GitRepo},
    webhooks::{
        models::{CheckRunEvent, CheckSuiteEvent, PushEvent},
        util::extract_branch_name,
        WebhookHandler,
    },
};

pub struct MetricsHandler {
    pool: Pool<SqliteConnectionManager>,
}

impl MetricsHandler {
    pub fn new(pool: Pool<SqliteConnectionManager>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl WebhookHandler for MetricsHandler {
    async fn handle_push(&self, payload: PushEvent) -> Result<(), anyhow::Error> {
        let Some(branch) = extract_branch_name(&payload.r#ref) else {
            return Ok(());
        };

        let count = payload.commits.len() as u64;
        if count > 0 {
            let repo_label = format!(
                "{}/{}",
                payload.repository.owner.login, payload.repository.name
            );
            crate::metrics::get().commits_observed.add(
                count,
                &[
                    KeyValue::new("repo", repo_label),
                    KeyValue::new("branch", branch),
                ],
            );
        }

        Ok(())
    }

    async fn handle_check_run(&self, payload: CheckRunEvent) -> Result<(), anyhow::Error> {
        if payload.action.as_str() != "created" {
            return Ok(());
        }

        let repo_label = format!(
            "{}/{}",
            payload.repository.owner.login, payload.repository.name
        );
        crate::metrics::get().builds_started.add(
            1,
            &[
                KeyValue::new("repo", repo_label),
                KeyValue::new("check_name", "default"),
            ],
        );

        Ok(())
    }

    async fn handle_check_suite(&self, payload: CheckSuiteEvent) -> Result<(), anyhow::Error> {
        if payload.action.as_str() != "completed" {
            return Ok(());
        }

        let repo_label = format!(
            "{}/{}",
            payload.repository.owner.login, payload.repository.name
        );

        let build_status = BuildStatus::of(
            &payload.check_suite.status,
            &payload.check_suite.conclusion.as_deref(),
        );
        let status_str: String = build_status.into();

        crate::metrics::get().builds_resolved.add(
            1,
            &[
                KeyValue::new("repo", repo_label.clone()),
                KeyValue::new("check_name", "default"),
                KeyValue::new("status", status_str.clone()),
            ],
        );

        // Look up the build to get start_time for duration calculation.
        // DatabaseHandler runs first, so the build row is already present.
        let conn = self
            .pool
            .get()
            .context("Failed to get database connection")?;

        let repo: GitRepo = payload.repository.clone().into();

        let maybe_commit =
            GitCommit::get_by_sha(&payload.check_suite.head_sha, repo.id, &conn)
                .context("Error getting commit")?;

        if let Some(commit) = maybe_commit {
            let maybe_build = GitCommitBuild::get_by_commit_id(&commit.id, &repo.id, &conn)
                .context("Error getting build")?;

            if let Some(build) = maybe_build {
                if let Some(start_time) = build.start_time {
                    let now_ms = Utc::now().timestamp_millis() as u64;
                    if now_ms > start_time {
                        let duration_secs = (now_ms - start_time) as f64 / 1000.0;
                        crate::metrics::get().build_duration_seconds.record(
                            duration_secs,
                            &[
                                KeyValue::new("repo", repo_label),
                                KeyValue::new("check_name", "default"),
                                KeyValue::new("status", status_str),
                            ],
                        );
                    }
                }
            }
        }

        Ok(())
    }
}
