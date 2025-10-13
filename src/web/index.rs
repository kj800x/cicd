use crate::db::BuildStatus;
use crate::prelude::*;
use crate::web::{build_status_helpers, formatting, header};

#[derive(Debug)]
#[allow(dead_code)]
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
#[allow(dead_code)]
pub struct CommitData {
    pub id: i64,
    pub sha: String,
    pub message: String,
    pub timestamp: i64,
    pub build_status: BuildStatus,
    pub build_url: Option<String>,
    pub parent_shas: Vec<String>,
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
                                            (format!("updated {}", formatting::format_relative_time(commit.timestamp)))
                                        }
                                    }
                                }
                            }
                        }

                        div class="commit-list" {
                            @for commit in &data.commits {
                                div class=(format!("commit-row {}", build_status_helpers::build_status_bg_class(&commit.build_status))) {
                                    div class=(format!("commit-status {}", build_status_helpers::build_status_class(&commit.build_status))) {}

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
