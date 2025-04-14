use crate::db::BuildStatus;
use crate::prelude::*;
use crate::web::header;
use chrono::{Local, TimeZone};

#[derive(Debug)]
pub struct BranchData {
    pub branch_id: i64,
    pub branch_name: String,
    pub head_commit_sha: String,
    pub repo_id: i64,
    pub repo_name: String,
    pub repo_owner: String,
    pub default_branch: String,
    pub is_private: bool,
    pub language: Option<String>,
    pub is_default: bool,
    pub commits: Vec<CommitData>,
}

#[derive(Debug)]
pub struct CommitData {
    pub id: i64,
    pub sha: String,
    pub message: String,
    pub timestamp: i64,
    pub build_status: BuildStatus,
    pub build_url: Option<String>,
    pub parent_shas: Vec<String>,
}

/// Format a timestamp as a human-readable relative time
fn format_relative_time(timestamp: i64) -> String {
    let now = Local::now();
    let dt = Local.timestamp_millis_opt(timestamp).unwrap();
    let duration = now.signed_duration_since(dt);

    if duration.num_days() > 0 {
        format!("{} days ago", duration.num_days())
    } else if duration.num_hours() > 0 {
        format!("{} hours ago", duration.num_hours())
    } else if duration.num_minutes() > 0 {
        format!("{} minutes ago", duration.num_minutes())
    } else {
        "just now".to_string()
    }
}

/// Format a git sha as a short version
fn format_short_sha(sha: &str) -> &str {
    if sha.len() > 7 {
        &sha[0..7]
    } else {
        sha
    }
}

fn truncate_message(message: &str, max_length: usize) -> String {
    if message.len() <= max_length {
        message.to_string()
    } else {
        format!("{}...", &message[0..max_length])
    }
}

/// Generate the HTML fragment for the branch grid content
pub fn render_branch_grid_fragment(pool: &Pool<SqliteConnectionManager>) -> Markup {
    let conn = pool.get().unwrap();

    // Reuse branch data fetching logic
    let mut branch_data_list = Vec::new();
    let query = r#"
        SELECT
            b.id, b.name, b.head_commit_sha, b.repo_id,
            r.name, r.owner_name, r.default_branch, r.private, r.language
        FROM git_branch b
        JOIN git_repo r ON b.repo_id = r.id
        ORDER BY b.id
    "#;

    let mut stmt = conn.prepare(query).unwrap();
    let branch_rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,            // branch.id
                row.get::<_, String>(1)?,         // branch.name
                row.get::<_, String>(2)?,         // branch.head_commit_sha
                row.get::<_, i64>(3)?,            // branch.repo_id
                row.get::<_, String>(4)?,         // repo.name
                row.get::<_, String>(5)?,         // repo.owner_name
                row.get::<_, String>(6)?,         // repo.default_branch
                row.get::<_, bool>(7)?,           // repo.private
                row.get::<_, Option<String>>(8)?, // repo.language
            ))
        })
        .unwrap();

    for (
        branch_id,
        branch_name,
        head_commit_sha,
        repo_id,
        repo_name,
        repo_owner,
        default_branch,
        is_private,
        language,
    ) in branch_rows.flatten()
    {
        let is_default = branch_name == default_branch;

        let commits_query = r#"
            SELECT c.id, c.sha, c.message, c.timestamp, c.build_status, c.build_url
            FROM git_commit c
            JOIN git_commit_branch cb ON c.sha = cb.commit_sha
            WHERE cb.branch_id = ?1
            ORDER BY c.timestamp DESC
            LIMIT 10
        "#;

        let mut commits_stmt = conn.prepare(commits_query).unwrap();
        let commit_rows = commits_stmt
            .query_map([branch_id], |row| {
                let build_status_str: Option<String> = row.get(4)?;
                let status: BuildStatus = build_status_str.into();

                let commit_id = row.get::<_, i64>(0)?;
                let commit_sha = row.get::<_, String>(1)?;
                let commit_message = row.get::<_, String>(2)?;
                let commit_timestamp = row.get::<_, i64>(3)?;
                let build_url = row.get::<_, Option<String>>(5)?;

                Ok((
                    commit_id,
                    commit_sha,
                    commit_message,
                    commit_timestamp,
                    status,
                    build_url,
                ))
            })
            .unwrap();

        let mut commits = Vec::new();
        for (commit_id, commit_sha, commit_message, commit_timestamp, build_status, build_url) in
            commit_rows.flatten()
        {
            let parent_shas = get_commit_parents(&commit_sha, &conn).unwrap_or_default();

            commits.push(CommitData {
                id: commit_id,
                sha: commit_sha,
                message: commit_message,
                timestamp: commit_timestamp,
                build_status,
                build_url,
                parent_shas,
            });
        }

        if !commits.is_empty() {
            branch_data_list.push(BranchData {
                branch_id,
                branch_name,
                head_commit_sha,
                repo_id,
                repo_name,
                repo_owner,
                default_branch,
                is_private,
                language,
                is_default,
                commits,
            });
        }
    }

    // Sort branches by timestamp of most recent commit
    branch_data_list.sort_by(|a, b| {
        let a_time = a.commits.first().map(|c| c.timestamp).unwrap_or(0);
        let b_time = b.commits.first().map(|c| c.timestamp).unwrap_or(0);
        b_time.cmp(&a_time) // Reverse order for newest first
    });

    html! {
        @if branch_data_list.is_empty() {
            div class="empty-state" {
                h2 { "No branches found" }
                p { "There are no branches with commits in the system." }
            }
        } @else {
            div class="branch-grid" {
                @for data in branch_data_list {
                    div class="branch-card" {
                        div class="branch-header" {
                            div class="branch-info" {
                                div class="repo-name" { (format!("{}/{}", data.repo_owner, data.repo_name)) }
                                div class="branch-name-wrapper" {
                                    @if data.is_default {
                                        span class="branch-badge default" { (data.branch_name) }
                                    } @else {
                                        span class="branch-badge" { (data.branch_name) }
                                    }
                                    span {
                                        // Show the latest commit time
                                        @if let Some(commit) = data.commits.first() {
                                            (format!("updated {}", format_relative_time(commit.timestamp)))
                                        }
                                    }
                                }
                            }
                        }

                        div class="commit-list" {
                            @for commit in &data.commits {
                                div class=(format!("commit-row bg-{}", match commit.build_status {
                                    BuildStatus::Success => "success",
                                    BuildStatus::Failure => "failure",
                                    BuildStatus::Pending => "pending",
                                    BuildStatus::None => "none",
                                })) {
                                    div class=(format!("commit-status status-{}", match commit.build_status {
                                        BuildStatus::Success => "success",
                                        BuildStatus::Failure => "failure",
                                        BuildStatus::Pending => "pending",
                                        BuildStatus::None => "none",
                                    })) {}

                                    div class="commit-sha" { (format_short_sha(&commit.sha)) }

                                    div class="commit-message-cell" {
                                        div class="commit-message-text tooltipped" data-tooltip=(commit.message) {
                                            (truncate_message(&commit.message, 60))
                                        }
                                    }

                                    div class="commit-time" {
                                        (format_relative_time(commit.timestamp))
                                    }

                                    div class="commit-links" {
                                        a href=(format!("https://github.com/{}/{}/commit/{}", data.repo_owner, data.repo_name, commit.sha)) target="_blank" { "Code" }

                                        @if let Some(url) = &commit.build_url {
                                            a href=(url) target="_blank" { "Logs" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Handler for the branch grid fragment endpoint
pub async fn branch_grid_fragment(
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let fragment = render_branch_grid_fragment(&pool);

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(fragment.into_string())
}

/// Generate HTML for the dashboard homepage that displays recent branches and their commits
pub async fn index(pool: web::Data<Pool<SqliteConnectionManager>>) -> impl Responder {
    // Render the HTML template using Maud
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { "CI/CD Build Status Dashboard" }
                style {
                    r#"
                    :root {
                        --success-color: #2ecc71;
                        --failure-color: #e74c3c;
                        --pending-color: #f39c12;
                        --none-color: #7f8c8d;
                        --bg-color: #f7f9fc;
                        --card-bg: #ffffff;
                        --text-color: #3a485a;
                        --primary-blue: #0969da;
                        --border-color: #d0d7de;
                        --header-bg: #24292e;
                    }
                    body {
                        font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
                        background-color: white;
                        color: var(--text-color);
                        margin: 0;
                        padding: 0;
                        line-height: 1.5;
                    }
                    "#
                    (header::styles())
                    r#"
                    .content {
                        padding: 24px;
                    }
                    header {
                        text-align: center;
                        margin-bottom: 30px;
                    }
                    h1 {
                        color: var(--primary-blue);
                        margin-bottom: 5px;
                    }
                    .subtitle {
                        color: #666;
                        font-size: 1.1rem;
                    }
                    .nav-links {
                        display: flex;
                        justify-content: center;
                        margin-bottom: 20px;
                    }
                    .nav-links a {
                        margin: 0 10px;
                        padding: 8px 16px;
                        color: var(--primary-blue);
                        text-decoration: none;
                        border-radius: 4px;
                        transition: background-color 0.2s;
                    }
                    .nav-links a:hover {
                        background-color: rgba(9, 105, 218, 0.1);
                    }
                    .nav-links a.active {
                        background-color: var(--primary-blue);
                        color: white;
                    }
                    .branch-grid {
                        display: grid;
                        grid-template-columns: 1fr;
                        grid-gap: 24px;
                        max-width: 1200px;
                        margin: 0 auto;
                    }
                    @media (min-width: 992px) {
                        .branch-grid {
                            grid-template-columns: repeat(auto-fill, minmax(800px, 1fr));
                        }
                    }
                    .branch-card {
                        background-color: var(--card-bg);
                        border-radius: 8px;
                        box-shadow: 0 2px 10px rgba(0, 0, 0, 0.08);
                        overflow: hidden;
                        position: relative;
                        transition: transform 0.2s;
                        display: flex;
                        flex-direction: column;
                        border: 2px solid var(--border-color);
                    }
                    .branch-card:hover {
                        transform: translateY(-3px);
                    }
                    .branch-header {
                        display: flex;
                        align-items: center;
                        padding: 16px;
                        border-bottom: 1px solid var(--border-color);
                        background-color: rgba(52, 152, 219, 0.05);
                    }
                    .branch-info {
                        flex-grow: 1;
                    }
                    .repo-name {
                        font-weight: 600;
                        font-size: 1.1rem;
                        margin-bottom: 8px;
                    }
                    .branch-name-wrapper {
                        display: flex;
                        align-items: center;
                    }
                    .branch-badge {
                        display: inline-block;
                        padding: 4px 10px;
                        border-radius: 12px;
                        border: 1px solid var(--border-color);
                        background-color: #f8f9fa;
                        font-size: 0.85rem;
                        color: #555;
                        margin-right: 8px;
                    }
                    .branch-badge.default {
                        background-color: #e3f2fd;
                        border-color: #2196f3;
                        color: #0d47a1;
                    }
                    .commit-list {
                        padding: 0;
                        margin: 0;
                        list-style-type: none;
                    }
                    .commit-row {
                        display: flex;
                        align-items: center;
                        padding: 12px 16px;
                        border-bottom: 1px solid var(--border-color);
                    }
                    .commit-row:last-child {
                        border-bottom: none;
                    }
                    .commit-row:hover {
                        background-color: rgba(0, 0, 0, 0.01);
                    }
                    .commit-row.bg-success {
                        background-color: rgba(46, 204, 113, 0.15);
                    }
                    .commit-row.bg-failure {
                        background-color: rgba(231, 76, 60, 0.15);
                    }
                    .commit-row.bg-pending {
                        background-color: rgba(243, 156, 18, 0.15);
                    }
                    .commit-row.bg-none {
                        background-color: rgba(127, 140, 141, 0.15);
                    }
                    .commit-status {
                        width: 16px;
                        height: 16px;
                        border-radius: 50%;
                        margin-right: 12px;
                        flex-shrink: 0;
                    }
                    .status-success {
                        background-color: var(--success-color);
                    }
                    .status-failure {
                        background-color: var(--failure-color);
                    }
                    .status-pending {
                        background-color: var(--pending-color);
                        position: relative;
                        overflow: hidden;
                    }
                    .status-pending::after {
                        content: '';
                        position: absolute;
                        top: 0;
                        left: -100%;
                        width: 200%;
                        height: 100%;
                        background: linear-gradient(to right, transparent, rgba(255,255,255,0.2), transparent);
                        animation: shimmer 1.5s infinite;
                    }
                    @keyframes shimmer {
                        100% {
                            transform: translateX(100%);
                        }
                    }
                    .status-none {
                        background-color: var(--none-color);
                    }
                    .commit-sha {
                        font-family: monospace;
                        font-size: 0.85rem;
                        width: 65px;
                        color: #555;
                        margin-right: 12px;
                    }
                    .commit-message-cell {
                        flex: 1;
                        min-width: 0;
                        margin-right: 12px;
                    }
                    .commit-message-text {
                        font-size: 0.95rem;
                        white-space: nowrap;
                        overflow: hidden;
                        text-overflow: ellipsis;
                    }
                    .commit-time {
                        font-size: 0.8rem;
                        color: #888;
                        white-space: nowrap;
                        margin-right: 12px;
                    }
                    .commit-links a {
                        color: var(--primary-blue);
                        text-decoration: none;
                        font-size: 0.85rem;
                        margin-left: 12px;
                    }
                    .commit-links a:hover {
                        text-decoration: underline;
                    }
                    .empty-state {
                        text-align: center;
                        padding: 40px;
                        color: #888;
                    }
                    .refresh {
                        display: inline-block;
                        margin-top: 20px;
                        padding: 8px 16px;
                        background-color: var(--primary-blue);
                        color: white;
                        border-radius: 4px;
                        text-decoration: none;
                        font-size: 0.9rem;
                        transition: background-color 0.2s;
                    }
                    .refresh:hover {
                        background-color: #2980b9;
                    }
                    .tooltipped {
                        position: relative;
                        cursor: pointer;
                    }
                    .tooltipped:hover::after {
                        content: attr(data-tooltip);
                        position: absolute;
                        z-index: 10;
                        bottom: 125%;
                        left: 50%;
                        transform: translateX(-50%);
                        width: max-content;
                        max-width: 300px;
                        padding: 6px 10px;
                        border-radius: 4px;
                        background-color: #333;
                        color: white;
                        font-size: 0.85rem;
                        pointer-events: none;
                        opacity: 0;
                        animation: fadeIn 0.2s ease-out forwards;
                    }
                    @keyframes fadeIn {
                        to {
                            opacity: 1;
                        }
                    }
                    "#
                }
                (header::scripts())
            }
            body hx-ext="morph" {
                (header::render("branches"))
                div class="content" {
                    header {
                        h1 { "CI/CD Build Status Dashboard" }
                        div class="subtitle" { "Recent branches and their commits" }
                    }

                    // Add container with HTMX attributes for auto-refresh
                    div id="branch-grid-container"
                        hx-get="/branch-grid-fragment"
                        hx-trigger="every 5s"
                        hx-swap="morph:innerHTML" {
                        (render_branch_grid_fragment(&pool))
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}
