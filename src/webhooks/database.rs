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
        git_repo::GitRepo,
    },
    webhooks::{
        models::{CheckRunEvent, DeleteEvent, PushEvent},
        util::{extract_branch_name, rfc3339_to_millis},
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

        let run = &payload.check_run;

        // We track build state at the check-run level (the actual unit of work),
        // not the check-suite level. GitHub auto-creates a suite per installed
        // App on every push even when that App never runs anything (e.g. an
        // empty "queued" suite with zero runs); keying on suites lets those
        // phantoms pin a commit at Pending forever. Runs only exist when there
        // is real work, and they carry their own status/conclusion/timestamps.
        let build_status = BuildStatus::of(&run.status, &run.conclusion.as_deref());

        let repo: GitRepo = payload.repository.clone().into();
        repo.upsert(&conn).context("Error upserting repository")?;

        let head_commit = GitCommit::get_by_sha(&run.check_suite.head_sha, repo.id, &conn)
            .context("Error getting commit")?
            .ok_or(anyhow::Error::msg("Commit not found"))?;

        let check_name = format!("run-{}", run.id);

        let url = run
            .html_url
            .clone()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| run.details_url.clone());

        let commit_build = GitCommitBuild {
            repo_id: repo.id,
            commit_id: head_commit.id,
            check_name,
            status: build_status.into(),
            url,
            // Prefer GitHub's own timestamps; fall back to event-receipt time
            // for start so an in-flight run still shows elapsed progress.
            start_time: rfc3339_to_millis(run.started_at.as_deref())
                .or_else(|| Some(Utc::now().timestamp_millis() as u64)),
            settle_time: rfc3339_to_millis(run.completed_at.as_deref()),
            app_id: run.app.as_ref().map(|a| a.id),
        };
        GitCommitBuild::upsert(&commit_build, &conn).context("Error upserting commit build")?;

        Ok(())
    }

    // check_suite events are intentionally not used to write build rows. Suites
    // are containers, not units of work, and an empty/abandoned suite would
    // otherwise mask the real per-run status. See handle_check_run above.

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
