use crate::prelude::*;
use crate::web::{build_status_helpers, formatting, header};

/// Generate the HTML fragment for the build grid content
pub fn render_build_grid_fragment(pool: &Pool<SqliteConnectionManager>) -> Markup {
    let conn = pool.get().unwrap();
    let since = Utc::now() - chrono::Duration::hours(24);
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

    html! {
        @if builds.is_empty() {
            div class="empty-state" {
                h2 { "No builds found" }
                p { "There are no builds in the last 24 hours." }
            }
        } @else {
            div class="build-grid" {
                @for (commit, repo, branches, _) in builds {
                    div class=(format!("build-card {}", build_status_helpers::build_card_status_class(&commit.build_status))) {
                        div class="build-header" {
                            div class=(format!("status-indicator {}", build_status_helpers::build_status_class(&commit.build_status))) {}
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
                                (formatting::format_relative_time(commit.timestamp))
                            }
                        }
                        div class="build-body" {
                            div class="commit-message" { (commit.message) }
                        }
                        div class="build-footer" {
                            div class="sha" { (formatting::format_short_sha(&commit.sha)) }
                            div class="links" {
                                a href=(format!("https://github.com/{}/{}/commit/{}",
                                                repo.owner_name, repo.name, commit.sha))
                                    target="_blank" { "View code" }

                                @if let Some(url) = &commit.build_url {
                                    a href=(url) target="_blank" { "Build logs" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Handler for the build grid fragment endpoint
#[get("/build-grid-fragment")]
pub async fn build_grid_fragment(pool: web::Data<Pool<SqliteConnectionManager>>) -> impl Responder {
    let fragment = render_build_grid_fragment(&pool);

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(fragment.into_string())
}

/// Generate HTML for the all recent builds page that displays recent builds
#[get("/all-recent-builds")]
pub async fn all_recent_builds(pool: web::Data<Pool<SqliteConnectionManager>>) -> impl Responder {
    // Render the HTML template using Maud
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { "CI/CD Build Status Dashboard - All Recent Builds" }
                (header::stylesheet_link())
                style {
                    r#"
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
                    "#
                }
                (header::scripts())
            }
            body hx-ext="morph" {
                (header::render("builds"))
                div class="content" {
                    header {
                        h1 { "CI/CD Build Status Dashboard" }
                        div class="subtitle" { "Recent builds from the last 24 hours" }
                    }

                    // Add container with HTMX attributes for auto-refresh
                    div id="build-grid-container"
                        hx-get="/build-grid-fragment"
                        hx-trigger="every 5s"
                        hx-swap="morph:innerHTML" {
                        (render_build_grid_fragment(&pool))
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}
