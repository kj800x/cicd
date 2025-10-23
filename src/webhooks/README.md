use std::sync::Arc;
use std::sync::RwLock;

use crate::crab_ext::Octocrabs;
use crate::error::{format_anyhow_chain, format_error_chain};
use crate::kubernetes;
use crate::prelude::*;
use anyhow::Context;
use futures_util::SinkExt;
use futures_util::StreamExt;
use kube::Client as KubeClient;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serenity::async_trait;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue},
};

/// Helper function to handle a new commit with no build status yet.
/// Goals:
/// - Upsert repo
/// - Upsert branch
/// - Upsert commit
/// - See if this affects any DeployConfigs and if so, track their new spec sha256 in the db
/// - In the rare case where a DeployConfig is found, it has autodeploy, and it has no artifact at all, update the kube resource.
async fn handle_push(
    payload: PushEvent,
    conn: &PooledConnection<SqliteConnectionManager>,
    __discord_notifier: &Option<DiscordNotifier>,
    kube_client: &Option<KubeClient>,
    octocrabs: &Octocrabs,
) -> Result<(), anyhow::Error> {
    log::debug!(
        "Received push event for {}/{}, ref: {}",
        payload.repository.owner.login,
        payload.repository.name,
        payload.r#ref
    );

    let repo = &payload.repository;
    let r#ref = &payload.r#ref;
    let commit_sha = &payload.head_commit.id;
    let commit_message = &payload.head_commit.message;
    let author = &payload.head_commit.author;
    let committer = &payload.head_commit.committer;
    let timestamp = &payload.head_commit.timestamp;
    let parent_shas = payload
        .head_commit
        .parents
        .as_ref()
        .map(|parents| parents.iter().map(|p| p.sha.clone()).collect());

    if let Some(kube_client) = kube_client {
        sync_repo_deploy_configs_impl(
            octocrabs,
            kube_client,
            repo.owner.login.clone(),
            repo.name.clone(),
        )
        .await
        .context("Error syncing deploy configs")?;
    } else {
        log::warn!("Kubernetes client not available, skipping DeployConfig updates");
    }

    let repo_id = upsert_repo(repo, conn).context("Error upserting repository")?;

    // Store the commit info but don't set any build status
    upsert_commit(
        &GhCommit {
            id: commit_sha.to_string(),
            message: commit_message.to_string(),
            timestamp: timestamp.to_string(),
            author: author.clone(),
            committer: committer.clone(),
            parent_shas,
        },
        repo_id,
        conn,
    )
    .context("Error storing commit")?;

    // Extract branch name if this is a branch push
    if let Some(branch_name) = extract_branch_name(r#ref) {
        upsert_branch(&branch_name, commit_sha, repo_id, conn).context("Error updating branch")?;
    }

    Ok(())
}

/// Helper function to mark a build as in progress
/// Goals:
/// - Upsert repo
/// - Set commit status to Pending
/// - Send Discord notification
async fn handle_build_started(
    repo: &Repository,
    commit_sha: &str,
    commit_message: &str,
    build_url: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
    discord_notifier: &Option<DiscordNotifier>,
) -> Result<(), anyhow::Error> {
    let repo_id = upsert_repo(repo, conn).context("Error upserting repository")?;

    // Set the commit status to Pending
    set_commit_status(
        commit_sha,
        BuildStatus::Pending,
        build_url.to_string(),
        repo_id,
        conn,
    )
    .context("Error setting commit status to Pending")?;

    // Send notification that build has started
    if let Some(notifier) = discord_notifier {
        notifier
            .notify_build_started(
                &repo.owner.login,
                &repo.name,
                commit_sha,
                commit_message,
                Some(build_url),
            )
            .await
            .map_err(anyhow::Error::msg)
            .context("Failed to send Discord notification")?;
        log::debug!("Discord notification sent for build start");
    }

    Ok(())
}

/// Helper function to mark a build as completed
/// Goals:
/// - Upsert repo
/// - Set commit status to Success/Failure/Pending
/// - Send Discord notification
/// - If build was successful, update Kubernetes DeployConfigs
#[allow(clippy::too_many_arguments)]
pub async fn handle_build_completed(
    repo: &Repository,
    commit_sha: &str,
    commit_message: &str,
    build_status: BuildStatus,
    build_url: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
    discord_notifier: &Option<DiscordNotifier>,
    kube_client: &Option<KubeClient>,
) -> Result<(), anyhow::Error> {
    let repo_id = upsert_repo(repo, conn).context("Error upserting repository")?;

    // Set the commit status
    set_commit_status(
        commit_sha,
        build_status.clone(),
        build_url.to_string(),
        repo_id,
        conn,
    )
    .context("Error setting commit status")?;

    // Send notification that build has completed
    if let Some(notifier) = discord_notifier {
        notifier
            .notify_build_completed(
                &repo.owner.login,
                &repo.name,
                commit_sha,
                commit_message,
                &build_status,
                Some(build_url),
            )
            .await
            .map_err(anyhow::Error::msg)
            .context("Failed to send Discord notification")?;
        log::debug!("Discord notification sent for build completion");
    }

    // If build was successful, update Kubernetes DeployConfigs
    if matches!(build_status, BuildStatus::Success) {
        if let Some(kube_client) = kube_client {
            // Get branches for this commit
            if let Ok(branches) = get_branches_for_commit(commit_sha, conn) {
                for branch in branches {
                    if commit_sha != branch.head_commit_sha {
                        log::debug!(
                            "Commit {} is not the latest on branch {}, not updating DeployConfigs",
                            commit_sha,
                            branch.name
                        );
                        continue;
                    }

                    // For each branch, update DeployConfigs
                    match kubernetes::webhook_handlers::handle_build_completed(
                        kube_client,
                        &repo.owner.login,
                        &repo.name,
                        &branch.name,
                        commit_sha,
                        conn,
                    )
                    .await
                    {
                        Ok(_) => {
                            log::info!(
                                "Updated DeployConfigs for {}/{} branch {} with SHA {}",
                                repo.owner.login,
                                repo.name,
                                branch.name,
                                &commit_sha[0..7]
                            );
                        }
                        Err(e) => {
                            log::error!(
                                "Failed to update DeployConfigs:\n{}",
                                format_error_chain(&e)
                            );
                        }
                    }
                }
            } else {
                log::warn!("Could not find branches for commit {}", commit_sha);
            }
        } else {
            log::warn!("Kubernetes client not available, skipping DeployConfig updates");
        }
    }

    Ok(())
}

async fn handle_check_run(
    payload: CheckRunEvent,
    conn: &PooledConnection<SqliteConnectionManager>,
    discord_notifier: &Option<DiscordNotifier>,
    __kube_client: &Option<KubeClient>,
) -> Result<(), anyhow::Error> {
    if payload.action.as_str() == "created" {
        let repo_id = match upsert_repo(&payload.repository, conn) {
            Ok(id) => id,
            Err(e) => {
                log::error!("Failed to upsert repository: {}", e);
                return Err(e.into());
            }
        };

        let head_commit = match get_commit(
            conn,
            repo_id as i64,
            payload.check_run.check_suite.head_sha.clone(),
        ) {
            Ok(Some(commit)) => commit,
            Ok(None) => {
                log::error!(
                    "Commit not found: {}",
                    payload.check_run.check_suite.head_sha
                );
                return Err(anyhow::Error::msg("Commit not found"));
            }
            Err(e) => {
                log::error!("Failed to get commit: {}", e);
                return Err(e.into());
            }
        };

        if let Err(e) = handle_build_started(
            &payload.repository,
            &head_commit.sha,
            &head_commit.message,
            &format!(
                "https://github.com/{}/{}/commit/{}/checks",
                payload.repository.owner.login, payload.repository.name, head_commit.sha
            ),
            conn,
            discord_notifier,
        )
        .await
        {
            log::error!("Error handling build start:\n{}", format_anyhow_chain(&e));
        }
    }

    Ok(())
}

async fn handle_check_suite(
    payload: CheckSuiteEvent,
    conn: &PooledConnection<SqliteConnectionManager>,
    discord_notifier: &Option<DiscordNotifier>,
    kube_client: &Option<KubeClient>,
) -> Result<(), anyhow::Error> {
    if payload.action.as_str() == "completed" {
        let build_status = BuildStatus::of(
            &payload.check_suite.status,
            &payload.check_suite.conclusion.as_deref(),
        );

        let head_commit_message = payload
            .check_suite
            .head_commit
            .as_ref()
            .map(|c| c.message.as_str())
            .unwrap_or("No commit message");

        if let Err(e) = handle_build_completed(
            &payload.repository,
            &payload.check_suite.head_sha,
            head_commit_message,
            build_status,
            &format!(
                "https://github.com/{}/{}/commit/{}/checks",
                payload.repository.owner.login,
                payload.repository.name,
                payload.check_suite.head_sha
            ),
            conn,
            discord_notifier,
            kube_client,
        )
        .await
        {
            log::error!(
                "Error handling build completion:\n{}",
                format_anyhow_chain(&e)
            );
        }
    }

    Ok(())
}
