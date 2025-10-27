use anyhow::Context;
use chrono::{DateTime, Utc};
use itertools::Itertools;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serenity::async_trait;

use crate::{
    build_status::BuildStatus,
    crab_ext::{OctocrabExt, Octocrabs},
    db::{
        git_branch::{GitBranch, GitBranchEgg},
        git_commit::{GitCommit, GitCommitEgg},
        git_commit_build::GitCommitBuild,
        git_repo::{GitRepo, GitRepoEgg},
    },
    webhooks::{
        models::{CheckRunEvent, CheckSuiteEvent, DeleteEvent, GhCommit, PushEvent},
        util::extract_branch_name,
        WebhookHandler,
    },
};

pub struct DatabaseHandler {
    pool: Pool<SqliteConnectionManager>,
    octocrabs: Octocrabs,
}

impl DatabaseHandler {
    pub fn new(pool: Pool<SqliteConnectionManager>, octocrabs: Octocrabs) -> Self {
        Self { pool, octocrabs }
    }
}

#[async_trait]
impl WebhookHandler for DatabaseHandler {
    async fn handle_push(&self, payload: PushEvent) -> Result<(), anyhow::Error> {
        log::debug!(
            "Received push event for {}/{}, ref: {}",
            payload.repository.owner.login,
            payload.repository.name,
            payload.r#ref
        );

        let conn = self
            .pool
            .get()
            .context("Failed to get database connection")?;

        let repo: GitRepo = payload.repository.clone().into();
        repo.upsert(&conn).context("Error upserting repository")?;

        // If no head commit, this is probably branch deleted which is handled by the delete handler
        let Some(head_commit) = payload.head_commit else {
            return Ok(());
        };

        let branch_egg = GitBranchEgg {
            name: extract_branch_name(&payload.r#ref).context("Error extracting branch name")?,
            head_commit_sha: head_commit.id.clone(),
            repo_id: repo.id,
            active: true,
        };
        let branch = branch_egg.upsert(&conn).context("Error upserting branch")?;

        for commit in payload.commits {
            let commit_sha = &commit.id;
            let commit_message = &commit.message;
            let author = &commit.author;
            let committer = &commit.committer;
            let timestamp = &commit.timestamp;

            let commit_ts = DateTime::parse_from_rfc3339(timestamp)?;
            let commit_ts = commit_ts.with_timezone(&Utc).timestamp_millis();

            let commit_egg = GitCommitEgg {
                sha: commit_sha.to_string(),
                repo_id: repo.id,
                message: commit_message.to_string(),
                timestamp: commit_ts,
                author: author.into(),
                committer: committer.into(),
            };
            let commit = GitCommit::upsert(&commit_egg, &conn).context("Error upserting commit")?;

            let octocrab = self
                .octocrabs
                .crab_for(&repo)
                .await
                .context("Error getting octocrab")?;

            let parents = octocrab
                .repos(repo.owner_name.clone(), repo.name.clone())
                .list_commits()
                .sha(commit.sha.clone())
                .send()
                .await?
                .items
                .first()
                .iter()
                .flat_map(|c| c.parents.iter().flat_map(|p| p.sha.clone()))
                .collect_vec();

            commit
                .add_parent_shas(parents, &conn)
                .context("Error adding parent SHAs")?;

            commit
                .add_branch(branch.id, &conn)
                .context("Error adding branch")?;
        }

        Ok(())
    }

    async fn handle_check_run(&self, payload: CheckRunEvent) -> Result<(), anyhow::Error> {
        log::debug!("Received check run event:\n{:#?}", payload);

        let conn = self
            .pool
            .get()
            .context("Failed to get database connection")?;

        if payload.action.as_str() == "created" {
            let build_status = BuildStatus::of(
                &payload.check_run.check_suite.status,
                &payload.check_run.check_suite.conclusion.as_deref(),
            );

            let repo: GitRepo = payload.repository.clone().into();
            repo.upsert(&conn).context("Error upserting repository")?;

            let head_commit = GitCommit::get_by_sha(
                &payload.check_run.check_suite.head_sha.clone(),
                repo.id,
                &conn,
            )
            .context("Error getting commit")?
            .ok_or(anyhow::Error::msg("Commit not found"))?;

            let commit_build = GitCommitBuild {
                repo_id: repo.id,
                commit_id: head_commit.id,
                check_name: "default".to_string(), // FIXME: Why can't we get the check name? Do we have to fetch it separately?
                status: build_status.into(),
                url: payload.check_run.details_url.clone(),
            };
            GitCommitBuild::upsert(&commit_build, &conn).context("Error upserting commit build")?;
        }

        Ok(())
    }

    async fn handle_check_suite(&self, payload: CheckSuiteEvent) -> Result<(), anyhow::Error> {
        log::debug!("Received check suite event:\n{:#?}", payload);

        let conn = self
            .pool
            .get()
            .context("Failed to get database connection")?;

        if payload.action.as_str() == "completed" {
            let build_status = BuildStatus::of(
                &payload.check_suite.status,
                &payload.check_suite.conclusion.as_deref(),
            );

            let repo: GitRepo = payload.repository.clone().into();
            repo.upsert(&conn).context("Error upserting repository")?;

            let head_commit =
                GitCommit::get_by_sha(&payload.check_suite.head_sha.clone(), repo.id, &conn)
                    .context("Error getting commit")?
                    .ok_or(anyhow::Error::msg("Commit not found"))?;

            let build_url = format!(
                "https://github.com/{}/{}/commit/{}/checks",
                payload.repository.owner.login,
                payload.repository.name,
                payload.check_suite.head_sha
            );
            let commit_build = GitCommitBuild {
                repo_id: repo.id,
                commit_id: head_commit.id,
                check_name: "default".to_string(), // FIXME: Why can't we get the check name? Do we have to fetch it separately?
                status: build_status.into(),
                url: build_url,
            };
            GitCommitBuild::upsert(&commit_build, &conn).context("Error upserting commit build")?;
        }

        Ok(())
    }

    async fn handle_delete(&self, payload: DeleteEvent) -> Result<(), anyhow::Error> {
        log::debug!("Received delete event:\n{:#?}", payload);

        let conn = self
            .pool
            .get()
            .context("Failed to get database connection")?;

        if payload.ref_type == "branch" {
            let repo: GitRepo = payload.repository.clone().into();
            repo.upsert(&conn).context("Error upserting repository")?;

            let branch_name = &payload.r#ref;
            let branch = GitBranch::get_by_name(branch_name, repo.id, &conn)
                .context("Error getting branch")?;

            if let Some(mut branch) = branch {
                branch
                    .mark_inactive(&conn)
                    .context("Error marking branch inactive")?;
            }
        }

        Ok(())
    }

    async fn handle_unknown(&self, event_type: &str) -> Result<(), anyhow::Error> {
        log::debug!("Received unknown event: {}", event_type);
        Ok(())
    }
}
