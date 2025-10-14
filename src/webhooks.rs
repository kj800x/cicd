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
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue},
};

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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CommitAuthor {
    pub name: String,
    pub email: String,
    // pub username: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GhCommit {
    pub id: String, // sha
    pub message: String,
    pub timestamp: String, // "2024-05-12T15:35:17-04:00",
    pub author: CommitAuthor,
    pub committer: CommitAuthor,
    pub parent_shas: Option<Vec<String>>, // Parent commit SHAs
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckSuite {
    pub id: u64,
    pub head_sha: String,
    pub head_commit: Option<PushCommit>,
    pub status: String,
    pub conclusion: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckRun {
    pub details_url: String,
    pub check_suite: CheckSuite,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckRunEvent {
    pub action: String,
    pub check_run: CheckRun,
    pub repository: Repository,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct PingEvent {
    pub zen: String,
    pub repository: Repository,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PushCommit {
    pub id: String, // sha
    pub message: String,
    pub timestamp: String,
    pub author: CommitAuthor,
    pub committer: CommitAuthor,
    pub parents: Option<Vec<ParentCommit>>, // Make parents optional
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ParentCommit {
    pub sha: String,
    pub url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PushEvent {
    pub r#ref: String, // like "refs/heads/branch-name"
    pub after: String,
    pub repository: Repository,
    pub head_commit: PushCommit,
    pub commits: Vec<PushCommit>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookEvent {
    pub event_type: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CheckSuiteEvent {
    pub action: String,
    pub check_suite: CheckSuite,
    pub repository: Repository,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkflowRunEvent {
    pub action: String,
    pub workflow_run: WorkflowRun,
    pub repository: Repository,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub head_sha: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub html_url: String,
}

// Pushes can be for reasons other than branches, such as tags
fn extract_branch_name(r#ref: &str) -> Option<String> {
    // This regex is a compile-time constant pattern, so expect is appropriate
    #[allow(clippy::expect_used)]
    let branch_regex =
        Regex::new(r"^refs/heads/(.+)$").expect("Branch regex pattern should be valid");
    if let Some(captures) = branch_regex.captures(r#ref) {
        captures.get(1).map(|m| m.as_str().to_string())
    } else {
        None
    }
}

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

async fn process_event(
    event: WebhookEvent,
    pool: &Pool<SqliteConnectionManager>,
    discord_notifier: &Option<DiscordNotifier>,
    kube_client: &Option<KubeClient>,
    octocrabs: &Octocrabs,
) {
    let conn = match pool.get() {
        Ok(conn) => conn,
        Err(e) => {
            log::error!("Error getting connection from pool: {}", e);
            return;
        }
    };

    log::debug!("Received event: {}", event.event_type);

    match event.event_type.as_str() {
        "push" => match serde_json::from_value::<PushEvent>(event.payload.clone()) {
            Ok(payload) => {
                let handler_result =
                    handle_push(payload, &conn, discord_notifier, kube_client, octocrabs).await;

                if let Err(e) = handler_result {
                    log::error!("Error handling new commit:\n{}", format_anyhow_chain(&e));
                }
            }
            Err(e) => log::error!("Error parsing push event: {}", e),
        },
        "check_run" => match serde_json::from_value::<CheckRunEvent>(event.payload) {
            Ok(payload) => {
                let handler_result =
                    handle_check_run(payload, &conn, discord_notifier, kube_client).await;

                if let Err(e) = handler_result {
                    log::error!("Error handling check run:\n{}", format_anyhow_chain(&e));
                }
            }
            Err(e) => log::error!("Failed to parse check run event: {}", e),
        },
        "check_suite" => match serde_json::from_value::<CheckSuiteEvent>(event.payload) {
            Ok(payload) => {
                let handler_result =
                    handle_check_suite(payload, &conn, discord_notifier, kube_client).await;

                if let Err(e) = handler_result {
                    log::error!("Error handling check suite:\n{}", format_anyhow_chain(&e));
                }
            }
            Err(e) => log::error!("Failed to parse check suite event: {}", e),
        },
        _ => log::debug!("Received unknown event: {}", event.event_type),
    }
}

pub async fn start_websockets(
    pool: Pool<SqliteConnectionManager>,
    discord_notifier: Option<DiscordNotifier>,
    octocrabs: Octocrabs,
) {
    // Get environment variables with defaults for development
    let websocket_url = std::env::var("WEBSOCKET_URL").unwrap_or_else(|_| {
        log::warn!("WEBSOCKET_URL not set, using default for development");
        "wss://example.com/ws".to_string()
    });

    let client_secret = std::env::var("CLIENT_SECRET").unwrap_or_else(|_| {
        log::warn!("CLIENT_SECRET not set, using default for development");
        "development_secret".to_string()
    });

    // Initialize Kubernetes client for DeployConfig updates
    let kube_client = match KubeClient::try_default().await {
        Ok(client) => {
            log::info!("Successfully initialized Kubernetes client for webhook handler");
            Some(client)
        }
        Err(e) => {
            log::warn!(
                "Failed to initialize Kubernetes client for webhook handler: {}",
                e
            );
            log::warn!("Auto-updates for DeployConfigs will be unavailable");
            None
        }
    };

    loop {
        log::info!(
            "Attempting to connect to webhook WebSocket at {}",
            websocket_url
        );

        let mut request = match websocket_url.clone().into_client_request() {
            Ok(request) => request,
            Err(e) => {
                log::error!("Failed to create WebSocket request: {}", e);
                // Wait before retrying
                tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                continue;
            }
        };

        request.headers_mut().insert(
            "Authorization",
            match format!("Bearer {}", client_secret).parse::<HeaderValue>() {
                Ok(header) => header,
                Err(e) => {
                    log::error!("Failed to create Authorization header: {}", e);
                    // Wait before retrying
                    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                    continue;
                }
            },
        );

        let connect_result = connect_async(request).await;

        match connect_result {
            Ok((ws_stream, _)) => {
                log::info!("Connection to webhooks websocket established");

                let (mut write, mut read) = ws_stream.split();
                let notifier_ref = &discord_notifier;
                let pool_ref = &pool;
                let kube_client_ref = &kube_client;
                let octocrabs_ref = &octocrabs;

                let last_pong = Arc::new(RwLock::new(Box::new(std::time::Instant::now())));
                let last_pong_clone = last_pong.clone();

                let mut ping_closure = async || loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                    log::debug!("Sending ping message");
                    if let Err(e) = write
                        .send(Message::Text(
                            "{\"event_type\":\"conn_ping\",\"payload\":{}}".to_string(),
                        ))
                        .await
                    {
                        log::error!("Failed to send ping message: {}", e);
                        break;
                    }
                };

                let mut message_closure = async || loop {
                    let message = match read.next().await {
                        Some(msg) => msg,
                        None => {
                            log::warn!("WebSocket stream ended");
                            break;
                        }
                    };
                    match message {
                        Ok(msg) => {
                            let data = msg.into_data();
                            if let Ok(mut last_pong) = last_pong_clone.write() {
                                *last_pong.as_mut() = std::time::Instant::now();
                            }
                            match serde_json::from_slice::<WebhookEvent>(&data) {
                                Ok(event) => {
                                    if event.event_type == "conn_ping" {
                                        log::debug!("Got conn_ping reply");
                                    } else {
                                        process_event(
                                            event,
                                            pool_ref,
                                            notifier_ref,
                                            kube_client_ref,
                                            octocrabs_ref,
                                        )
                                        .await
                                    }
                                }
                                Err(e) => log::error!("Error parsing webhook event: {}", e),
                            }
                        }
                        Err(e) => log::error!("Error reading from websocket: {}", e),
                    }
                };

                let watchdog_closure = async || loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(20)).await;
                    let last_pong = match last_pong_clone.read() {
                        Ok(pong) => pong,
                        Err(e) => {
                            log::error!("Failed to read last_pong: {}", e);
                            break;
                        }
                    };
                    if last_pong.elapsed() > tokio::time::Duration::from_secs(10) {
                        log::debug!("Watchdog failed");
                        break;
                    } else {
                        log::debug!("Watchdog passed");
                    }
                };

                tokio::select! {
                    _ = ping_closure() => {}
                    _ = message_closure() => {}
                    _ = watchdog_closure() => {}
                }

                log::error!("WebSocket connection closed, will attempt to reconnect...");
            }
            Err(e) => {
                log::error!("Failed to connect to WebSocket: {}", e);
            }
        }

        // Wait before retrying
        log::warn!("Reconnecting in 10 seconds...");
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
    }
}
