use anyhow::Context;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serenity::async_trait;

use crate::{
    crab_ext::Octocrabs,
    db::{
        delete_branch, get_commit, set_commit_status, upsert_branch, upsert_commit, upsert_repo,
        BuildStatus,
    },
    webhooks::{
        models::{CheckRunEvent, CheckSuiteEvent, DeleteEvent, GhCommit, PushEvent},
        util::extract_branch_name,
        WebhookHandler,
    },
};

pub struct DatabaseHandler {
    pool: Pool<SqliteConnectionManager>,

    #[allow(dead_code)]
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

        let repo = &payload.repository;
        let r#ref = &payload.r#ref;

        // If no head commit, this is probably branch deleted which is handled by the delete handler
        let Some(head_commit) = payload.head_commit else {
            return Ok(());
        };

        let commit_sha = &head_commit.id;
        let commit_message = &head_commit.message;
        let author = &head_commit.author;
        let committer = &head_commit.committer;
        let timestamp = &head_commit.timestamp;

        // FIXME: Move to separate handler
        // if let Some(kube_client) = kube_client {
        //     sync_repo_deploy_configs_impl(
        //         octocrabs,
        //         kube_client,
        //         repo.owner.login.clone(),
        //         repo.name.clone(),
        //     )
        //     .await
        //     .context("Error syncing deploy configs")?;
        // } else {
        //     log::warn!("Kubernetes client not available, skipping DeployConfig updates");
        // }

        let repo_id = upsert_repo(repo, &conn).context("Error upserting repository")?;

        // Store the commit info but don't set any build status
        upsert_commit(
            &GhCommit {
                id: commit_sha.to_string(),
                message: commit_message.to_string(),
                timestamp: timestamp.to_string(),
                author: author.clone(),
                committer: committer.clone(),
                // FIXME: GH doesn't send parent shas in webhooks, need to fetch separately later
                parent_shas: None,
            },
            repo_id,
            &conn,
        )
        .context("Error storing commit")?;

        // Extract branch name if this is a branch push
        if let Some(branch_name) = extract_branch_name(r#ref) {
            upsert_branch(&branch_name, commit_sha, repo_id, &conn)
                .context("Error updating branch")?;
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
            let repo_id =
                upsert_repo(&payload.repository, &conn).context("Error upserting repository")?;

            let head_commit = get_commit(
                &conn,
                repo_id as i64,
                payload.check_run.check_suite.head_sha.clone(),
            )
            .context("Error getting commit")?
            .ok_or(anyhow::Error::msg("Commit not found"))?;

            // Set the commit status to Pending
            set_commit_status(
                &head_commit.sha,
                BuildStatus::Pending,
                payload.check_run.details_url.clone(),
                repo_id,
                &conn,
            )
            .context("Error setting commit status to Pending")?;
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

            let repo_id =
                upsert_repo(&payload.repository, &conn).context("Error upserting repository")?;

            // Set the commit status
            set_commit_status(
                payload.check_suite.head_sha.as_str(),
                build_status.clone(),
                format!(
                    "https://github.com/{}/{}/commit/{}/checks",
                    payload.repository.owner.login,
                    payload.repository.name,
                    payload.check_suite.head_sha
                ),
                repo_id,
                &conn,
            )
            .context("Error setting commit status")?;
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
            let repo_id =
                upsert_repo(&payload.repository, &conn).context("Error upserting repository")?;
            let branch_name = &payload.r#ref;
            delete_branch(branch_name, repo_id, &conn).context("Error deleting branch")?;
        }

        Ok(())
    }

    async fn handle_unknown(&self, event_type: &str) -> Result<(), anyhow::Error> {
        log::debug!("Received unknown event: {}", event_type);
        Ok(())
    }
}
