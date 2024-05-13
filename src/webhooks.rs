use futures_util::StreamExt;
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
struct PushEvent {
    r#ref: String, // like "refs/heads/branch-name"
    after: String,
    repository: Repository,
    head_commit: GhCommit,
}

#[derive(Debug, Serialize, Deserialize)]
struct WebhookEvent {
    #[serde(rename = "githubEvent")]
    github_event: String,
    timestamp: u64,
    payload: serde_json::Value,
}

// Pushes can be for reasons other than branches, such as tags
fn extract_branch_name(r#ref: &str) -> Option<String> {
    if r#ref.starts_with("refs/heads/") {
        Some(r#ref["refs/heads/".len()..].to_string())
    } else {
        None
    }
}

async fn process_event(event: WebhookEvent, pool: &Pool<SqliteConnectionManager>) {
    match event.github_event.as_str() {
        "push" => {
            let payload: PushEvent = serde_json::from_value(event.payload).unwrap();
            println!("Received push event: {:?}", payload);
            let repo_id = upsert_repo(&payload.repository, pool).await.unwrap();
            upsert_commit(&payload.head_commit, repo_id, pool)
                .await
                .unwrap();
            if let Some(branch_name) = extract_branch_name(&payload.r#ref) {
                upsert_branch(&branch_name, &payload.head_commit.id, repo_id, pool)
                    .await
                    .unwrap();
            }
        }
        "ping" => {
            let payload: PingEvent = serde_json::from_value(event.payload).unwrap();
            println!("Received ping event: {:?}", payload);
            upsert_repo(&payload.repository, pool).await.unwrap();
        }
        "check_run" => {
            let payload: CheckRunEvent = serde_json::from_value(event.payload).unwrap();
            println!("Received check_run event: {:?}", payload);
            let repo_id = upsert_repo(&payload.repository, pool).await.unwrap();
            set_commit_status(
                &payload.check_run.check_suite.head_sha,
                BuildStatus::of(
                    &payload.check_run.check_suite.status,
                    &payload.check_run.check_suite.conclusion.as_deref(),
                ),
                repo_id,
                pool,
            )
            .await
            .unwrap();
        }
        _ => {
            println!("Received unknown event: {:?}", event.github_event);
        }
    }
}

pub async fn start_websockets(
    websocket_url: String,
    client_secret: String,
    pool: Pool<SqliteConnectionManager>,
) {
    let mut request = websocket_url.into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", client_secret)
            .parse::<HeaderValue>()
            .unwrap(),
    );

    let (ws_stream, _) = connect_async(request).await.expect("Failed to connect");

    println!("Connection to webhooks websocket established");

    let (_, read) = ws_stream.split();

    read.for_each(|message| async {
        let data = message.unwrap().into_data();
        let event: WebhookEvent = serde_json::from_slice(&data).unwrap();
        process_event(event, &pool).await;
    })
    .await
}
