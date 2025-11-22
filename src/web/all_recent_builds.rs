use crate::db::functions::get_commits_since;
use crate::db::git_repo::GitRepo;
use crate::prelude::*;
use crate::web::team_prefs::ReposCookie;
use crate::web::{build_status_helpers, formatting, header};

/// Generate the HTML rows for the build table tbody
fn render_build_rows(
    builds: &Vec<(
        crate::db::git_commit::GitCommit,
        GitRepo,
        Vec<crate::db::git_branch::GitBranch>,
        Option<crate::db::git_commit_build::GitCommitBuild>,
        Vec<crate::db::git_commit::GitCommit>,
    )>,
) -> Markup {
    html! {
        @for (commit, repo, branches, build_status, __parent_commits) in builds {
            @let status: crate::build_status::BuildStatus = build_status.clone().into();
            tr class=(format!("build-row {}", build_status_helpers::build_status_bg_class(&status))) {
                td class="status-cell" {
                    div class=(format!("status-indicator {}", build_status_helpers::build_status_class(&status))) {}
                }
                td class="repo-cell" {
                    (format!("{}/{}", repo.owner_name, repo.name))
                }
                td class="branch-cell" {
                    @if branches.is_empty() {
                        span class="no-branch" { "—" }
                    } @else {
                        @for (i, branch) in branches.iter().enumerate() {
                            @if i > 0 { ", " }
                            (branch.name)
                        }
                    }
                }
                td class="message-cell" {
                    span class="message-text" title=(commit.message.clone()) {
                        (formatting::truncate_message(&commit.message, 80))
                    }
                }
                td class="time-cell" {
                    (formatting::format_relative_time(commit.timestamp))
                }
                td class="links-cell" {
                    a href=(format!("https://github.com/{}/{}/commit/{}", repo.owner_name, repo.name, commit.sha))
                        target="_blank"
                        class="link-icon"
                        title="View commit" {
                        i class="fa fa-external-link" {}
                    }
                    @if let Some(url) = build_status.as_ref().map(|x| &x.url) {
                        a href=(url) target="_blank" class="link-icon" title="Build logs" {
                            i class="fa fa-file-text-o" {}
                        }
                    }
                }
            }
        }
    }
}

/// Generate the HTML fragment for the build grid content (full table)
pub fn render_build_grid_fragment(
    pool: &Pool<SqliteConnectionManager>,
    req: &actix_web::HttpRequest,
) -> Markup {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return html! { div { "Error: Failed to connect to database" } };
        }
    };
    let since = Utc::now() - chrono::Duration::hours(24);
    let commits_result = get_commits_since(&conn, since.timestamp_millis());

    let repos_cookie = ReposCookie::from_request(req);

    let mut builds = Vec::new();

    if let Ok(commits) = commits_result {
        for commit in commits {
            // FIXME:
            #[allow(clippy::expect_used)]
            let repo = GitRepo::get_by_id(&commit.repo_id, &conn)
                .expect("Expect")
                .expect("Expect");

            // Filter repos based on visibility cookie
            if !repos_cookie.is_visible(&repo.owner_name) {
                continue;
            }

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
            table class="build-table" {
                thead {
                    tr {
                        th class="col-status" { "" }
                        th class="col-repo" { "Repo" }
                        th class="col-branch" { "Branch" }
                        th class="col-message" { "Message" }
                        th class="col-time" { "Time" }
                        th class="col-links" { "" }
                    }
                }
                tbody hx-get="/build-grid-fragment"
                       hx-trigger="load, every 5s"
                       hx-swap="morph:innerHTML"
                       hx-ext="morph" {
                    (render_build_rows(&builds))
                }
            }
        }
    }
}

/// Handler for the build grid fragment endpoint (returns only tbody rows)
#[get("/build-grid-fragment")]
pub async fn build_grid_fragment(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    req: actix_web::HttpRequest,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Error: Failed to connect to database");
        }
    };
    let since = Utc::now() - chrono::Duration::hours(24);
    let commits_result = get_commits_since(&conn, since.timestamp_millis());

    let repos_cookie = ReposCookie::from_request(&req);

    let mut builds = Vec::new();

    if let Ok(commits) = commits_result {
        for commit in commits {
            // FIXME:
            #[allow(clippy::expect_used)]
            let repo = GitRepo::get_by_id(&commit.repo_id, &conn)
                .expect("Expect")
                .expect("Expect");

            // Filter repos based on visibility cookie
            if !repos_cookie.is_visible(&repo.owner_name) {
                continue;
            }

            let parent_commits = commit.get_parents(&conn).unwrap_or_default();
            let branches = commit.get_branches(&conn).unwrap_or_default();
            let build_status = commit.get_build_status(&conn).unwrap_or_default();

            builds.push((commit, repo, branches, build_status, parent_commits));
        }
    };

    let fragment = render_build_rows(&builds);

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(fragment.into_string())
}

/// Generate HTML for the all recent builds page that displays recent builds
#[get("/all-recent-builds")]
pub async fn all_recent_builds(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    req: actix_web::HttpRequest,
) -> impl Responder {
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
                div class="content no-padding" {
                    // Add container with HTMX attributes for auto-refresh
                    div id="build-grid-container" {
                        (render_build_grid_fragment(&pool, &req))
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}
