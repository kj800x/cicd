use crate::prelude::*;
use crate::web::header;
use chrono::{Local, TimeZone};

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

/// Generate HTML for the all recent builds page that displays recent builds
pub async fn all_recent_builds(pool: web::Data<Pool<SqliteConnectionManager>>) -> impl Responder {
    // Get recent builds from the database
    let conn = pool.get().unwrap();
    let since = Utc::now() - chrono::Duration::hours(24); // Show last 24 hours of builds
    let commits_result = get_commits_since(&conn, since.timestamp_millis());

    let mut builds = Vec::new();

    if let Ok(commits) = commits_result {
        for commit_with_repo in commits {
            let parent_shas =
                get_commit_parents(&commit_with_repo.commit.sha, &conn).unwrap_or_default();
            let branches =
                get_branches_for_commit(&commit_with_repo.commit.sha, &conn).unwrap_or_default();

            builds.push((
                commit_with_repo.commit,
                commit_with_repo.repo,
                branches,
                parent_shas,
            ));
        }
    }

    // Render the HTML template using Maud
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                meta http-equiv="refresh" content="3";
                title { "CI/CD Build Status Dashboard - All Recent Builds" }
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
                    .build-grid {
                        display: grid;
                        grid-template-columns: 1fr;
                        grid-gap: 16px;
                        max-width: 1200px;
                        margin: 0 auto;
                    }
                    @media (min-width: 768px) {
                        .build-grid {
                            grid-template-columns: repeat(auto-fill, minmax(500px, 1fr));
                        }
                    }
                    .build-card {
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
                    .build-card:hover {
                        transform: translateY(-3px);
                    }
                    .build-header {
                        display: flex;
                        align-items: center;
                        padding: 16px;
                        border-bottom: 1px solid var(--border-color);
                    }
                    .status-indicator {
                        width: 20px;
                        height: 20px;
                        border-radius: 50%;
                        margin-right: 12px;
                        flex-shrink: 0;
                    }

                    /* Status-specific styling */
                    .card-status-success {
                        border-color: var(--success-color);
                        background-color: rgba(46, 204, 113, 0.05);
                    }
                    .card-status-success .build-header {
                        background-color: rgba(46, 204, 113, 0.15);
                        border-bottom-color: var(--success-color);
                    }

                    .card-status-failure {
                        border-color: var(--failure-color);
                        background-color: rgba(231, 76, 60, 0.05);
                    }
                    .card-status-failure .build-header {
                        background-color: rgba(231, 76, 60, 0.15);
                        border-bottom-color: var(--failure-color);
                    }

                    .card-status-pending {
                        border-color: var(--pending-color);
                        background-color: rgba(243, 156, 18, 0.05);
                    }
                    .card-status-pending .build-header {
                        background-color: rgba(243, 156, 18, 0.15);
                        border-bottom-color: var(--pending-color);
                    }

                    .card-status-none {
                        border-color: var(--none-color);
                        background-color: rgba(127, 140, 141, 0.05);
                    }
                    .card-status-none .build-header {
                        background-color: rgba(127, 140, 141, 0.15);
                        border-bottom-color: var(--none-color);
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
                    .status-success {
                        background-color: var(--success-color);
                    }
                    .status-failure {
                        background-color: var(--failure-color);
                    }
                    .status-none {
                        background-color: var(--none-color);
                    }
                    .build-info {
                        flex-grow: 1;
                    }
                    .repo-name {
                        font-weight: 600;
                        font-size: 1.1rem;
                        margin-bottom: 4px;
                    }
                    .branch-name {
                        color: #555;
                        font-size: 0.9rem;
                    }
                    .build-time {
                        font-size: 0.85rem;
                        color: #888;
                        text-align: right;
                        min-width: 100px;
                    }
                    .build-body {
                        padding: 16px;
                        flex: 1;
                    }
                    .commit-message {
                        font-size: 1rem;
                        line-height: 1.4;
                        margin-bottom: 12px;
                        word-break: break-word;
                    }
                    .build-footer {
                        display: flex;
                        justify-content: space-between;
                        align-items: center;
                        padding: 12px 16px;
                        background-color: rgba(0, 0, 0, 0.02);
                        border-top: 1px solid var(--border-color);
                    }
                    .sha {
                        font-family: monospace;
                        font-size: 0.85rem;
                        color: #555;
                    }
                    .links a {
                        color: var(--primary-blue);
                        text-decoration: none;
                        font-size: 0.9rem;
                        margin-left: 16px;
                    }
                    .links a:hover {
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
                        background-color: #05509d;
                    }
                    "#
                }
                (header::scripts())
            }
            body {
                (header::render("builds"))
                div class="content" {
                    header {
                        h1 { "CI/CD Build Status Dashboard" }
                        div class="subtitle" { "Recent builds from the last 24 hours" }
                    }

                    @if builds.is_empty() {
                        div class="empty-state" {
                            h2 { "No builds found" }
                            p { "There are no builds in the last 24 hours." }
                            a href="/all-recent-builds" class="refresh" { "Refresh" }
                        }
                    } @else {
                        div class="build-grid" {
                            @for (commit, repo, branches, _) in &builds {
                                // Determine status for styling
                                @let status_class = match commit.build_status {
                                    BuildStatus::Success => "card-status-success",
                                    BuildStatus::Failure => "card-status-failure",
                                    BuildStatus::Pending => "card-status-pending",
                                    BuildStatus::None => "card-status-none",
                                };
                                div class=(format!("build-card {}", status_class)) {
                                    div class="build-header" {
                                        div class=(format!("status-indicator status-{}", match commit.build_status {
                                            BuildStatus::Success => "success",
                                            BuildStatus::Failure => "failure",
                                            BuildStatus::Pending => "pending",
                                            BuildStatus::None => "none",
                                        })) {}
                                        div class="build-info" {
                                            div class="repo-name" { (format!("{}/{}", repo.owner_name, repo.name)) }
                                            div class="branch-name" {
                                                @if branches.is_empty() {
                                                    "No branch"
                                                } @else {
                                                    @for (i, branch) in branches.iter().enumerate() {
                                                        @if i > 0 { ", " }
                                                        (branch.name)
                                                    }
                                                }
                                            }
                                        }
                                        div class="build-time" {
                                            (format_relative_time(commit.timestamp))
                                        }
                                    }
                                    div class="build-body" {
                                        div class="commit-message" { (commit.message) }
                                    }
                                    div class="build-footer" {
                                        div class="sha" { (format_short_sha(&commit.sha)) }
                                        div class="links" {
                                            // Link to GitHub code (assuming GitHub)
                                            a href=(format!("https://github.com/{}/{}/commit/{}",
                                                            repo.owner_name, repo.name, commit.sha))
                                                target="_blank" { "View code" }

                                            // Link to build logs if available
                                            @if let Some(url) = &commit.build_url {
                                                a href=(url) target="_blank" { "Build logs" }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        div style="text-align: center; margin-top: 30px;" {
                            a href="/all-recent-builds" class="refresh" { "Refresh" }
                        }
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}
