use crate::db::functions::get_branches_with_commits;
use crate::prelude::*;
use crate::web::{build_status_helpers, formatting, header};

/// Generate the HTML fragment for the branch grid content
pub fn render_branch_grid_fragment(pool: &Pool<SqliteConnectionManager>) -> Markup {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return html! { div { "Error: Failed to connect to database" } };
        }
    };

    // Get branches with their commits from the database
    let mut branch_data_list = get_branches_with_commits(&conn, 10).unwrap_or_default();

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
                    @let is_default = data.branch.name == data.repo.default_branch;
                    div class="branch-card" {
                        div class="branch-header" {
                            div class="branch-info" {
                                div class="repo-name" { (format!("{}/{}", data.repo.owner_name, data.repo.name)) }
                                div class="branch-name-wrapper" {
                                    @if is_default {
                                        span class="branch-badge default" { (data.branch.name) }
                                    } @else {
                                        span class="branch-badge" { (data.branch.name) }
                                    }
                                    span {
                                        // Show the latest commit time
                                        @if let Some(commit) = data.commits.first() {
                                            (format!("updated {}", formatting::format_relative_time(commit.timestamp)))
                                        }
                                    }
                                }
                            }
                        }

                        div class="commit-list" {
                            @for commit in &data.commits {
                                @let build_status = commit.get_build_status(&conn).unwrap_or_default();
                                div class=(format!("commit-row {}", build_status_helpers::build_status_bg_class(&build_status.clone().into()))) {
                                    div class=(format!("commit-status {}", build_status_helpers::build_status_class(&build_status.clone().into()))) {}

                                    div class="commit-sha" { (formatting::format_short_sha(&commit.sha)) }

                                    div class="commit-message-cell" {
                                        div class="commit-message-text tooltipped" data-tooltip=(commit.message) {
                                            (formatting::truncate_message(&commit.message, 60))
                                        }
                                    }

                                    div class="commit-time" {
                                        (formatting::format_relative_time(commit.timestamp))
                                    }

                                    div class="commit-links" {
                                        a href=(format!("https://github.com/{}/{}/commit/{}", data.repo.owner_name, data.repo.name, commit.sha)) target="_blank" { "Code" }

                                        @if let Some(url) = &build_status.map(|x| x.url) {
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
#[get("/branch-grid-fragment")]
pub async fn branch_grid_fragment(
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let fragment = render_branch_grid_fragment(&pool);

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(fragment.into_string())
}

/// Generate HTML for the dashboard homepage that displays recent branches and their commits
#[get("/")]
pub async fn index(pool: web::Data<Pool<SqliteConnectionManager>>) -> impl Responder {
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
