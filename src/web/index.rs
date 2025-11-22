use crate::db::functions::get_branches_with_commits;
use crate::prelude::*;
use crate::web::team_prefs::ReposCookie;
use crate::web::{build_status_helpers, formatting, header};

/// Generate the HTML rows for the branch table tbody
fn render_branch_rows(
    conn: &r2d2::PooledConnection<r2d2_sqlite::SqliteConnectionManager>,
    branch_data_list: &[crate::db::functions::BranchWithCommits],
) -> Markup {
    html! {
        @for data in branch_data_list {
            @let is_default = data.branch.name == data.repo.default_branch;
            @let latest_commit = data.commits.first();
            @if let Some(commit) = latest_commit {
                @let build_status = commit.get_build_status(conn).unwrap_or_default();
                tr class=(format!("branch-row {}", build_status_helpers::build_status_bg_class(&build_status.clone().into()))) {
                    td class="status-cell" {
                        div class=(format!("status-indicator {}", build_status_helpers::build_status_class(&build_status.clone().into()))) {}
                    }
                    td class="repo-cell" {
                        (format!("{}/{}", data.repo.owner_name, data.repo.name))
                    }
                    td class="branch-cell" {
                        @if is_default {
                            span class="branch-badge default" { (data.branch.name) }
                        } @else {
                            span class="branch-badge" { (data.branch.name) }
                        }
                    }
                    td class="latest-cell" {
                        @let status: crate::build_status::BuildStatus = build_status.clone().into();
                        @if matches!(status, crate::build_status::BuildStatus::None) {
                            span class="no-status" { "—" }
                        } @else {
                            span class="build-status-text" {
                                (build_status_helpers::build_status_text(&status))
                            }
                        }
                    }
                    td class="message-cell" {
                        span class="message-text" title=(commit.message.clone()) {
                            (formatting::truncate_message(&commit.message, 70))
                        }
                    }
                    td class="time-cell" {
                        (formatting::format_relative_time(commit.timestamp))
                    }
                    td class="links-cell" {
                        a href=(format!("https://github.com/{}/{}/tree/{}", data.repo.owner_name, data.repo.name, data.branch.name))
                            target="_blank"
                            class="link-icon"
                            title="View branch" {
                            i class="fa fa-external-link" {}
                        }
                        @if let Some(url) = &build_status.map(|x| x.url) {
                            a href=(url) target="_blank" class="link-icon" title="Build logs" {
                                i class="fa fa-file-text-o" {}
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Generate the HTML fragment for the branch grid content (full table)
pub fn render_branch_grid_fragment(
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

    let repos_cookie = ReposCookie::from_request(req);

    // Get branches with their commits from the database
    let mut branch_data_list = get_branches_with_commits(&conn, 10).unwrap_or_default();

    // Filter branches based on repo visibility cookie
    branch_data_list.retain(|data| repos_cookie.is_visible(&data.repo.owner_name));

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
            table class="branch-table" {
                thead {
                    tr {
                        th class="col-status" { "" }
                        th class="col-repo" { "Repo" }
                        th class="col-branch" { "Branch" }
                        th class="col-latest" { "Latest Build" }
                        th class="col-message" { "Latest Commit" }
                        th class="col-time" { "Updated" }
                        th class="col-links" { "" }
                    }
                }
                tbody hx-get="/branch-grid-fragment"
                       hx-trigger="load, every 5s"
                       hx-swap="morph:innerHTML"
                       hx-ext="morph" {
                    (render_branch_rows(&conn, &branch_data_list))
                }
            }
        }
    }
}

/// Handler for the branch grid fragment endpoint (returns only tbody rows)
#[get("/branch-grid-fragment")]
pub async fn branch_grid_fragment(
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

    let repos_cookie = ReposCookie::from_request(&req);

    // Get branches with their commits from the database
    let mut branch_data_list = get_branches_with_commits(&conn, 10).unwrap_or_default();

    // Filter branches based on repo visibility cookie
    branch_data_list.retain(|data| repos_cookie.is_visible(&data.repo.owner_name));

    // Sort branches by timestamp of most recent commit
    branch_data_list.sort_by(|a, b| {
        let a_time = a.commits.first().map(|c| c.timestamp).unwrap_or(0);
        let b_time = b.commits.first().map(|c| c.timestamp).unwrap_or(0);
        b_time.cmp(&a_time) // Reverse order for newest first
    });

    let fragment = render_branch_rows(&conn, &branch_data_list);

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(fragment.into_string())
}

/// Root redirect handler - redirects to deploy configs page
#[get("/")]
pub async fn root() -> impl Responder {
    HttpResponse::Found()
        .append_header(("Location", "/deploy"))
        .finish()
}

/// Generate HTML for the dashboard homepage that displays recent branches and their commits
#[get("/branches")]
pub async fn index(
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
                title { "CI/CD Build Status Dashboard" }
                (header::stylesheet_link())
                (header::scripts())
            }
            body.index-page hx-ext="morph" {
                (header::render("branches"))
                div class="content" {
                    header {
                        h1 { "CI/CD Build Status Dashboard" }
                        div class="subtitle" { "Recent branches with latest build status" }
                    }

                    // Add container with HTMX attributes for auto-refresh
                    div id="branch-grid-container" {
                        (render_branch_grid_fragment(&pool, &req))
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}
