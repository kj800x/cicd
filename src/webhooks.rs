use crate::kubernetes;
use crate::prelude::*;
use futures_util::StreamExt;
use kube::Client as KubeClient;
use regex::Regex;
use serde::{Deserialize, Serialize};
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
    pub username: String,
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
    pub status: String,
    pub conclusion: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CheckRun {
    details_url: String,
    check_suite: CheckSuite,
}

#[derive(Debug, Serialize, Deserialize)]
struct CheckRunEvent {
    action: String,
    check_run: CheckRun,
    repository: Repository,
}

#[derive(Debug, Serialize, Deserialize)]
struct PingEvent {
    zen: String,
    repository: Repository,
}

#[derive(Debug, Serialize, Deserialize)]
struct PushCommit {
    id: String, // sha
    message: String,
    timestamp: String,
    author: CommitAuthor,
    committer: CommitAuthor,
    parents: Option<Vec<ParentCommit>>, // Make parents optional
}

#[derive(Debug, Serialize, Deserialize)]
struct ParentCommit {
    sha: String,
    url: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PushEvent {
    r#ref: String, // like "refs/heads/branch-name"
    after: String,
    repository: Repository,
    head_commit: PushCommit,
    commits: Vec<PushCommit>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WebhookEvent {
    event_type: String,
    payload: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct CheckSuiteEvent {
    action: String,
    check_suite: CheckSuite,
    repository: Repository,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkflowRunEvent {
    action: String,
    workflow_run: WorkflowRun,
    repository: Repository,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkflowRun {
    id: u64,
    head_sha: String,
    status: String,
    conclusion: Option<String>,
    html_url: String,
}

// Pushes can be for reasons other than branches, such as tags
fn extract_branch_name(r#ref: &str) -> Option<String> {
    let branch_regex = Regex::new(r"^refs/heads/(.+)$").unwrap();
    if let Some(captures) = branch_regex.captures(r#ref) {
        captures.get(1).map(|m| m.as_str().to_string())
    } else {
        None
    }
}

// Convert PushCommit to GhCommit, extracting all parents
fn convert_to_gh_commit(push_commit: &PushCommit) -> GhCommit {
    let parent_shas = if let Some(parents) = &push_commit.parents {
        if !parents.is_empty() {
            let mut shas = Vec::with_capacity(parents.len());
            for parent in parents {
                shas.push(parent.sha.clone());
            }
            Some(shas)
        } else {
            None
        }
    } else {
        None
    };

    GhCommit {
        id: push_commit.id.clone(),
        message: push_commit.message.clone(),
        timestamp: push_commit.timestamp.clone(),
        author: push_commit.author.clone(),
        committer: push_commit.committer.clone(),
        parent_shas,
    }
}

/// Helper function to handle a new commit with no build status yet
async fn handle_new_commit(
    repo: &Repository,
    commit_sha: &str,
    commit_message: &str,
    author: &CommitAuthor,
    committer: &CommitAuthor,
    timestamp: &str,
    parent_shas: Option<Vec<String>>,
    conn: &PooledConnection<SqliteConnectionManager>,
    discord_notifier: &Option<DiscordNotifier>,
) -> Result<(), String> {
    match upsert_repo(repo, conn) {
        Ok(repo_id) => {
            // Store the commit info but don't set any build status
            if let Err(e) = upsert_commit(
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
            ) {
                return Err(format!("Error storing commit: {}", e));
            }

            // Extract branch name if this is a branch push
            if let Some(branch_name) = extract_branch_name(&repo.default_branch) {
                if let Err(e) = upsert_branch(&branch_name, commit_sha, repo_id, conn) {
                    return Err(format!("Error updating branch: {}", e));
                }
            }

            Ok(())
        }
        Err(e) => Err(format!("Error upserting repository: {}", e)),
    }
}

/// Helper function to mark a build as in progress
async fn handle_build_started(
    repo: &Repository,
    commit_sha: &str,
    commit_message: &str,
    build_url: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
    discord_notifier: &Option<DiscordNotifier>,
) -> Result<(), String> {
    match upsert_repo(repo, conn) {
        Ok(repo_id) => {
            // Set the commit status to Pending
            if let Err(e) = set_commit_status(
                commit_sha,
                BuildStatus::Pending,
                build_url.to_string(),
                repo_id,
                conn,
            ) {
                return Err(format!("Error setting commit status to Pending: {}", e));
            }

            // Send notification that build has started
            if let Some(notifier) = discord_notifier {
                match notifier
                    .notify_build_started(
                        &repo.owner.login,
                        &repo.name,
                        commit_sha,
                        commit_message,
                        Some(build_url),
                    )
                    .await
                {
                    Ok(_) => log::debug!("Discord notification sent for build start"),
                    Err(e) => return Err(format!("Failed to send Discord notification: {}", e)),
                }
            }

            Ok(())
        }
        Err(e) => Err(format!("Error upserting repository: {}", e)),
    }
}

/// Helper function to mark a build as completed
async fn handle_build_completed(
    repo: &Repository,
    commit_sha: &str,
    commit_message: &str,
    build_status: BuildStatus,
    build_url: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
    discord_notifier: &Option<DiscordNotifier>,
    kube_client: &Option<KubeClient>,
) -> Result<(), String> {
    match upsert_repo(repo, conn) {
        Ok(repo_id) => {
            // Set the commit status
            if let Err(e) = set_commit_status(
                commit_sha,
                build_status.clone(),
                build_url.to_string(),
                repo_id,
                conn,
            ) {
                return Err(format!("Error setting commit status: {}", e));
            }

            // Send notification that build has completed
            if let Some(notifier) = discord_notifier {
                match notifier
                    .notify_build_completed(
                        &repo.owner.login,
                        &repo.name,
                        commit_sha,
                        commit_message,
                        &build_status,
                        Some(build_url),
                    )
                    .await
                {
                    Ok(_) => log::debug!("Discord notification sent for build completion"),
                    Err(e) => return Err(format!("Failed to send Discord notification: {}", e)),
                }
            }

            // If build was successful, update Kubernetes DeployConfigs
            if matches!(build_status, BuildStatus::Success) {
                if let Some(kube_client) = kube_client {
                    // Get branches for this commit
                    if let Ok(branches) = get_branches_for_commit(commit_sha, conn) {
                        for branch in branches {
                            if commit_sha != branch.head_commit_sha {
                                log::debug!("Commit {} is not the latest on branch {}, not updating DeployConfigs", commit_sha, branch.name);
                                continue;
                            }

                            // For each branch, update DeployConfigs
                            match kubernetes::handle_build_completed(
                                kube_client,
                                &repo.owner.login,
                                &repo.name,
                                &branch.name,
                                commit_sha,
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
                                    log::error!("Failed to update DeployConfigs: {:?}", e);
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
        Err(e) => Err(format!("Error upserting repository: {}", e)),
    }
}

async fn process_event(
    event: WebhookEvent,
    pool: &Pool<SqliteConnectionManager>,
    discord_notifier: &Option<DiscordNotifier>,
    kube_client: &Option<KubeClient>,
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
        "push" => {
            match serde_json::from_value::<PushEvent>(event.payload.clone()) {
                Ok(payload) => {
                    log::debug!(
                        "Received push event for {}/{}, ref: {}",
                        payload.repository.owner.login,
                        payload.repository.name,
                        payload.r#ref
                    );

                    // Handle new commits but don't set any build status
                    if let Err(e) = handle_new_commit(
                        &payload.repository,
                        &payload.head_commit.id,
                        &payload.head_commit.message,
                        &payload.head_commit.author,
                        &payload.head_commit.committer,
                        &payload.head_commit.timestamp,
                        payload
                            .head_commit
                            .parents
                            .as_ref()
                            .map(|parents| parents.iter().map(|p| p.sha.clone()).collect()),
                        &conn,
                        discord_notifier,
                    )
                    .await
                    {
                        log::error!("Error handling new commit: {}", e);
                    }
                }
                Err(e) => {
                    log::error!("Error parsing push event: {}", e);
                }
            }
        }
        "workflow_run" => {
            if let Ok(payload) = serde_json::from_value::<WorkflowRunEvent>(event.payload) {
                match payload.action.as_str() {
                    "requested" | "in_progress" => {
                        if let Err(e) = handle_build_started(
                            &payload.repository,
                            &payload.workflow_run.head_sha,
                            &payload.workflow_run.head_sha, // TODO: Get actual commit message
                            &payload.workflow_run.html_url,
                            &conn,
                            discord_notifier,
                        )
                        .await
                        {
                            log::error!("Error handling build start: {}", e);
                        }
                    }
                    "completed" => {
                        let build_status = match payload.workflow_run.conclusion.as_deref() {
                            Some("success") => BuildStatus::Success,
                            Some("failure") => BuildStatus::Failure,
                            _ => BuildStatus::None,
                        };

                        if let Err(e) = handle_build_completed(
                            &payload.repository,
                            &payload.workflow_run.head_sha,
                            &payload.workflow_run.head_sha, // TODO: Get actual commit message
                            build_status,
                            &payload.workflow_run.html_url,
                            &conn,
                            discord_notifier,
                            kube_client,
                        )
                        .await
                        {
                            log::error!("Error handling build completion: {}", e);
                        }
                    }
                    _ => {}
                }
            }
        }
        "check_suite" => {
            if let Ok(payload) = serde_json::from_value::<CheckSuiteEvent>(event.payload) {
                let build_status = BuildStatus::of(
                    &payload.check_suite.status,
                    &payload.check_suite.conclusion.as_deref(),
                );

                if let Err(e) = handle_build_completed(
                    &payload.repository,
                    &payload.check_suite.head_sha,
                    &payload.check_suite.head_sha, // TODO: Get actual commit message
                    build_status,
                    &format!(
                        "https://github.com/{}/{}/commit/{}/checks",
                        payload.repository.owner.login,
                        payload.repository.name,
                        payload.check_suite.head_sha
                    ),
                    &conn,
                    discord_notifier,
                    kube_client,
                )
                .await
                {
                    log::error!("Error handling build completion: {}", e);
                }
            }
        }
        _ => {
            log::debug!("Received unknown event: {}", event.event_type);
        }
    }
}

// Helper function to truncate long payloads
fn truncate_payload(payload: &str, max_length: usize) -> String {
    if payload.len() <= max_length {
        payload.to_string()
    } else {
        format!("{}...[truncated]", &payload[0..max_length])
    }
}

pub async fn start_websockets(
    websocket_url: String,
    client_secret: String,
    pool: Pool<SqliteConnectionManager>,
    discord_notifier: Option<DiscordNotifier>,
) {
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

                let (_, read) = ws_stream.split();
                let notifier_ref = &discord_notifier;
                let pool_ref = &pool;
                let kube_client_ref = &kube_client;

                read.for_each(|message| async {
                    match message {
                        Ok(msg) => {
                            let data = msg.into_data();
                            match serde_json::from_slice::<WebhookEvent>(&data) {
                                Ok(event) => {
                                    process_event(event, pool_ref, notifier_ref, kube_client_ref)
                                        .await
                                }
                                Err(e) => log::error!("Error parsing webhook event: {}", e),
                            }
                        }
                        Err(e) => log::error!("Error reading from websocket: {}", e),
                    }
                })
                .await;
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
