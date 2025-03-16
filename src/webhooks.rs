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
    parents: Vec<ParentCommit>,
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
    let parent_shas = if !push_commit.parents.is_empty() {
        let mut shas = Vec::with_capacity(push_commit.parents.len());
        for parent in &push_commit.parents {
            shas.push(parent.sha.clone());
        }
        Some(shas)
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

async fn process_event(event: WebhookEvent, pool: &Pool<SqliteConnectionManager>) {
    let conn = pool.get().unwrap();
    match event.event_type.as_str() {
        "push" => {
            let payload: PushEvent = serde_json::from_value(event.payload).unwrap();
            println!("Received push event: {:?}", payload);

            // Upsert the repository
            let repo_id = upsert_repo(&payload.repository, &conn).unwrap();

            // Process all commits in the push
            for commit in &payload.commits {
                // Convert to our commit type with parent information
                let gh_commit = convert_to_gh_commit(commit);

                // Store the commit
                upsert_commit(&gh_commit, repo_id, &conn).unwrap();
            }

            // Process the head commit specifically
            let head_gh_commit = convert_to_gh_commit(&payload.head_commit);
            upsert_commit(&head_gh_commit, repo_id, &conn).unwrap();

            // If this is a branch, update the branch information
            if let Some(branch_name) = extract_branch_name(&payload.r#ref) {
                let branch_id =
                    upsert_branch(&branch_name, &payload.after, repo_id, &conn).unwrap();

                // Add all commits in this push to the branch
                for commit in &payload.commits {
                    add_commit_to_branch(&commit.id, branch_id, repo_id, &conn).unwrap();
                }
            }
        }
        "ping" => {
            let payload: PingEvent = serde_json::from_value(event.payload).unwrap();
            println!("Received ping event: {:?}", payload);
            upsert_repo(&payload.repository, &conn).unwrap();
        }
        "check_run" => {
            let payload: CheckRunEvent = serde_json::from_value(event.payload).unwrap();
            println!("Received check_run event: {:?}", payload);
            let repo_id = upsert_repo(&payload.repository, &conn).unwrap();
            set_commit_status(
                &payload.check_run.check_suite.head_sha,
                BuildStatus::of(
                    &payload.check_run.check_suite.status,
                    &payload.check_run.check_suite.conclusion.as_deref(),
                ),
                // FIXME: Technically this isn't fully correct because a commit
                // can have multiple check runs. If we wanted to do this right,
                // we'd need to track each individual check run in the database.
                // As another option, we could try to only initial details url
                // and also maybe override it if any of them fail, since that's
                // the one that we care about more.
                payload.check_run.details_url,
                repo_id,
                &conn,
            )
            .unwrap();
        }
        _ => {
            println!("Received unknown event: {:?}", event.event_type);
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
