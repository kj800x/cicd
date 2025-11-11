use crate::db::functions::get_commits_since;
use crate::db::git_repo::GitRepo;
use crate::prelude::*;
use crate::web::{build_status_helpers, formatting, header};

/// Generate the HTML fragment for the build grid content
pub fn render_build_grid_fragment(pool: &Pool<SqliteConnectionManager>) -> Markup {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return html! { div { "Error: Failed to connect to database" } };
        }
    };
    let since = Utc::now() - chrono::Duration::hours(24);
    let commits_result = get_commits_since(&conn, since.timestamp_millis());

    let mut builds = Vec::new();

    if let Ok(commits) = commits_result {
        for commit in commits {
            // FIXME:
            #[allow(clippy::expect_used)]
            let repo = GitRepo::get_by_id(&commit.repo_id, &conn)
                .expect("Expect")
                .expect("Expect");

            let parent_commits = commit.get_parents(&conn).unwrap_or_default();
            let branches = commit.get_branches(&conn).unwrap_or_default();
            let build_status = commit.get_build_status(&conn).unwrap_or_default();

            builds.push((commit, repo, branches, build_status, parent_commits));
        }
    };

    html! {
        @if builds.is_empty() {
            div class="empty-state" {
                h2 { "No builds found" }
                p { "There are no builds in the last 24 hours." }
            }
        } @else {
            div class="build-grid" {
                @for (commit, repo, branches, build_status, __parent_commits) in builds {
                    div class=(format!("build-card {}", build_status_helpers::build_card_status_class(&build_status.clone().into()))) {
                        div class="build-header" {
                            div class=(format!("status-indicator {}", build_status_helpers::build_status_class(&build_status.clone().into()))) {}
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
                                a href=(format!("https://github.com/{}/{}/commit/{}", repo.owner_name, repo.name, commit.sha))
                                    target="_blank" { "View code" }

                                @if let Some(url) = &build_status.map(|x| x.url) {
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
