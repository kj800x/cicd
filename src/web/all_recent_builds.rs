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
                (header::scripts())
            }
            body.recent-builds-page hx-ext="morph" {
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
