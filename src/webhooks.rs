use futures_util::StreamExt;
use regex::Regex;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue},
};

use crate::prelude::*;

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

async fn process_event(
    event: WebhookEvent,
    pool: &Pool<SqliteConnectionManager>,
    discord_notifier: &Option<DiscordNotifier>,
) {
    let conn = pool.get().unwrap();
    match event.event_type.as_str() {
        "push" => {
            match serde_json::from_value::<PushEvent>(event.payload.clone()) {
                Ok(payload) => {
                    println!("Received push event: {:?}", payload);

                    match upsert_repo(&payload.repository, &conn) {
                        Ok(repo_id) => {
                            for commit in &payload.commits {
                                let gh_commit = convert_to_gh_commit(commit);

                                if let Err(e) = upsert_commit(&gh_commit, repo_id, &conn) {
                                    println!("Error storing commit {}: {}", commit.id, e);
                                }
                            }

                            let head_gh_commit = convert_to_gh_commit(&payload.head_commit);
                            if let Err(e) = upsert_commit(&head_gh_commit, repo_id, &conn) {
                                println!(
                                    "Error storing head commit {}: {}",
                                    payload.head_commit.id, e
                                );
                            }

                            if let Some(branch_name) = extract_branch_name(&payload.r#ref) {
                                match upsert_branch(&branch_name, &payload.after, repo_id, &conn) {
                                    Ok(branch_id) => {
                                        for commit in &payload.commits {
                                            if let Err(e) = add_commit_to_branch(
                                                &commit.id, branch_id, repo_id, &conn,
                                            ) {
                                                println!("Error associating commit {} with branch {}: {}", commit.id, branch_name, e);
                                            }
                                        }

                                        // For the head commit, send a notification that build has started
                                        if let Some(notifier) = discord_notifier {
                                            // Get the full commit info from DB
                                            if let Ok(Some(commit)) = get_commit(
                                                &conn,
                                                repo_id as i64,
                                                payload.head_commit.id.clone(),
                                            ) {
                                                let _ = notifier
                                                    .notify_build_started(
                                                        &payload.repository.owner.login,
                                                        &payload.repository.name,
                                                        &payload.head_commit.id,
                                                        &payload.head_commit.message,
                                                        commit.build_url.as_deref(),
                                                    )
                                                    .await;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        println!("Error updating branch {}: {}", branch_name, e);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            println!(
                                "Error upserting repository {}: {}",
                                payload.repository.name, e
                            );
                        }
                    }
                }
                Err(e) => {
                    println!("Error parsing push event: {}", e);
                    println!("Raw payload: {}", &event.payload);
                }
            }
        }
        "ping" => match serde_json::from_value::<PingEvent>(event.payload.clone()) {
            Ok(payload) => {
                println!("Received ping event: {:?}", payload);
                if let Err(e) = upsert_repo(&payload.repository, &conn) {
                    println!("Error upserting repository from ping event: {}", e);
                }
            }
            Err(e) => {
                println!("Error parsing ping event: {}", e);
                println!("Raw payload: {}", &event.payload);
            }
        },
        "check_run" => match serde_json::from_value::<CheckRunEvent>(event.payload.clone()) {
            Ok(payload) => {
                println!("Received check_run event: {:?}", payload);
                match upsert_repo(&payload.repository, &conn) {
                    Ok(repo_id) => {
                        let build_status = BuildStatus::of(
                            &payload.check_run.check_suite.status,
                            &payload.check_run.check_suite.conclusion.as_deref(),
                        );

                        if let Err(e) = set_commit_status(
                            &payload.check_run.check_suite.head_sha,
                            build_status.clone(),
                            payload.check_run.details_url.clone(),
                            repo_id,
                            &conn,
                        ) {
                            println!("Error setting commit status: {}", e);
                        }

                        // Get the commit to get its message
                        if let Ok(Some(commit)) = get_commit(
                            &conn,
                            repo_id as i64,
                            payload.check_run.check_suite.head_sha.clone(),
                        ) {
                            // Send a notification that build has completed
                            if let Some(notifier) = discord_notifier {
                                let _ = notifier
                                    .notify_build_completed(
                                        &payload.repository.owner.login,
                                        &payload.repository.name,
                                        &payload.check_run.check_suite.head_sha,
                                        &commit.message,
                                        &build_status,
                                        Some(&payload.check_run.details_url),
                                    )
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        println!("Error upserting repository from check_run event: {}", e);
                    }
                }
            }
            Err(e) => {
                println!("Error parsing check_run event: {}", e);
                println!("Raw payload: {}", &event.payload);
            }
        },
        _ => {
            println!("Received unknown event: {}", event.event_type);
            println!("Raw payload: {}", &event.payload);
        }
    }
}

pub async fn start_websockets(
    websocket_url: String,
    client_secret: String,
    pool: Pool<SqliteConnectionManager>,
    discord_notifier: Option<DiscordNotifier>,
) {
    loop {
        println!(
            "Attempting to connect to webhook WebSocket at {}",
            websocket_url
        );

        let mut request = match websocket_url.clone().into_client_request() {
            Ok(request) => request,
            Err(e) => {
                eprintln!("Failed to create WebSocket request: {}", e);
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
                    eprintln!("Failed to create Authorization header: {}", e);
                    // Wait before retrying
                    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                    continue;
                }
            },
        );

        let connect_result = connect_async(request).await;

        match connect_result {
            Ok((ws_stream, _)) => {
                println!("Connection to webhooks websocket established");

                let (_, read) = ws_stream.split();
                let notifier_ref = &discord_notifier;
                let pool_ref = &pool;

                match read
                    .for_each(|message| async {
                        match message {
                            Ok(msg) => {
                                let data = msg.into_data();
                                match serde_json::from_slice::<WebhookEvent>(&data) {
                                    Ok(event) => process_event(event, pool_ref, notifier_ref).await,
                                    Err(e) => eprintln!("Error parsing webhook event: {}", e),
                                }
                            }
                            Err(e) => eprintln!("Error reading from websocket: {}", e),
                        }
                    })
                    .await
                {
                    // The for_each completes when the stream is closed
                    _ => eprintln!("WebSocket connection closed, will attempt to reconnect..."),
                }
            }
            Err(e) => {
                eprintln!("Failed to connect to WebSocket: {}", e);
            }
        }

        // Wait before retrying
        eprintln!("Reconnecting in 10 seconds...");
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
    }
}
