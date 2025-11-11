use crate::db::deploy_event::DeployEvent;
use crate::db::git_repo::GitRepo;
use crate::prelude::*;
use crate::web::team_prefs::TeamsCookie;
use crate::web::{formatting, header};
use chrono::TimeZone;
use chrono_tz::America::New_York;
use maud::{html, Markup, DOCTYPE};
use std::collections::HashMap;

fn format_et_time(timestamp_ms: i64) -> String {
    let utc = match Utc.timestamp_millis_opt(timestamp_ms).single() {
        Some(t) => t,
        None => return "Invalid time".to_string(),
    };
    let local = utc.with_timezone(&New_York);
    local.format("%b %-e, %Y %-I:%M %p").to_string()
}

fn render_event_row(conn: &PooledConnection<SqliteConnectionManager>, e: &DeployEvent) -> Markup {
    let when_abs = format_et_time(e.timestamp);

    html! {
        tr {
            td class="config-name" {
                a href=(format!("/deploy-history?name={}", e.name)) { (e.name.clone()) }
            }
            (render_artifact_cell(conn, e))
            (render_config_cell(conn, e))
            td class="time-cell" { (when_abs) }
            td class="actions-cell" {
                button class="link-button" onclick="alert('todo, not implemented yet')" { "Revert to this deploy" }
            }
        }
    }
}

fn render_artifact_cell(
    conn: &PooledConnection<SqliteConnectionManager>,
    e: &DeployEvent,
) -> Markup {
    let content = render_sha_maybe_branch(e.artifact_branch.as_ref(), e.artifact_sha.as_ref());
    let compare = if let (Some(curr), Some(prev), Some(repo_id)) = (
        e.artifact_sha.as_ref(),
        e.prev_artifact_sha.as_ref(),
        e.artifact_repo_id,
    ) {
        if let Ok(repo) = GitRepo::get_by_id(&(repo_id as u64), conn) {
            repo.map(|repo| {
                format!(
                    "https://github.com/{}/{}/compare/{}...{}",
                    repo.owner_name, repo.name, prev, curr
                )
            })
        } else {
            None
        }
    } else {
        None
    };
    html! {
        td class="sha-cell" {
            (content)
            @if let Some(url) = compare {
                " "
                a class="link-button" href=(url) target="_blank" { "[compare]" }
            }
        }
    }
}

fn render_config_cell(conn: &PooledConnection<SqliteConnectionManager>, e: &DeployEvent) -> Markup {
    let content = render_sha_maybe_branch(e.config_branch.as_ref(), e.config_sha.as_ref());
    let changed = match (&e.config_version_hash, &e.prev_config_version_hash) {
        (Some(cur), Some(prev)) => cur != prev,
        _ => false,
    };
    let compare = if let (Some(curr), Some(prev), Some(repo_id)) = (
        e.config_sha.as_ref(),
        e.prev_config_sha.as_ref(),
        e.config_repo_id,
    ) {
        if let Ok(repo) = GitRepo::get_by_id(&(repo_id as u64), conn) {
            repo.map(|repo| {
                format!(
                    "https://github.com/{}/{}/compare/{}...{}",
                    repo.owner_name, repo.name, prev, curr
                )
            })
        } else {
            None
        }
    } else {
        None
    };
    html! {
        td class="sha-cell" {
            (content)
            @if changed {
                " "
                span { "[changed]" }
            }
            @if let Some(url) = compare {
                " "
                a class="link-button" href=(url) target="_blank" { "[compare]" }
            }
        }
    }
}

fn render_sha_maybe_branch(branch: Option<&String>, sha: Option<&String>) -> Markup {
    match sha {
        Some(sha) => {
            let short = formatting::format_short_sha(sha);
            match branch {
                Some(b) if !b.is_empty() => html! { (b) ":" (short) },
                _ => html! { (short) },
            }
        }
        None => html! { "-" },
    }
}

#[get("/deploy-history/{name}")]
pub async fn deploy_history(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    path: web::Path<String>,
) -> impl Responder {
    let name = path.into_inner();

    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to connect to database");
        }
    };

    let mut events: Vec<DeployEvent> = Vec::new();
    match conn.prepare("SELECT name, timestamp, initiator, config_sha, artifact_sha, artifact_branch, config_branch, prev_artifact_sha, prev_config_sha, artifact_repo_id, config_repo_id, config_version_hash, prev_config_version_hash FROM deploy_event WHERE name = ?1 ORDER BY timestamp DESC") {
		Ok(mut stmt) => {
            let rows = stmt
                .query_map(params![name], |row| {
                    Ok(DeployEvent {
                        name: row.get(0)?,
                        timestamp: row.get(1)?,
                        initiator: row.get(2)?,
                        config_sha: row.get(3)?,
                        artifact_sha: row.get(4)?,
                        artifact_branch: row.get(5)?,
                        config_branch: row.get(6)?,
                        prev_artifact_sha: row.get(7)?,
                        prev_config_sha: row.get(8)?,
                        artifact_repo_id: row.get(9)?,
                        config_repo_id: row.get(10)?,
                        config_version_hash: row.get(11)?,
                        prev_config_version_hash: row.get(12)?,
                    })
                })
				.and_then(|mapped| mapped.collect::<Result<Vec<_>, _>>());
			match rows {
				Ok(list) => {
					events = list;
				}
				Err(e) => {
					log::error!("Failed to fetch deploy history: {}", e);
				}
			}
		}
		Err(e) => {
			log::error!("Failed to prepare deploy history query: {}", e);
		}
	}

    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (format!("Deploy history - {}", name)) }
                (header::stylesheet_link())
                (header::scripts())
            }
            body.deploy-history-page hx-ext="morph" {
                (header::render("history"))
                div class="content" {
                    header {
                        h1 { (format!("Deploy history for {}", name)) }
                        div class="subtitle" { "Most recent first" }
                    }
                    @if events.is_empty() {
                        div class="empty-state" {
                            h2 { "No history found" }
                            p { "There are no deploy events recorded for this config." }
                        }
                    } @else {
                        table class="history-table" {
                            thead {
                                tr {
                                    th { "Deploy config" }
                                    th { "Artifact" }
                                    th { "Config" }
                                    th { "Deploy time" }
                                    th { "" }
                                }
                            }
                            tbody id="history-tbody"
                                hx-get=(format!("/deploy-history-fragment?name={}", name))
                                hx-trigger="load, every 5s"
                                hx-swap="morph:innerHTML" {
                                @for e in &events {
                                    (render_event_row(&conn, e))
                                }
                            }
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

fn fetch_events_for_name(
    conn: &PooledConnection<SqliteConnectionManager>,
    name: &str,
) -> Vec<DeployEvent> {
    let mut events: Vec<DeployEvent> = Vec::new();
    match conn.prepare("SELECT name, timestamp, initiator, config_sha, artifact_sha, artifact_branch, config_branch, prev_artifact_sha, prev_config_sha, artifact_repo_id, config_repo_id, config_version_hash, prev_config_version_hash FROM deploy_event WHERE name = ?1 ORDER BY timestamp DESC") {
		Ok(mut stmt) => {
			let rows = stmt
				.query_map(params![name], |row| {
                    Ok(DeployEvent {
						name: row.get(0)?,
						timestamp: row.get(1)?,
						initiator: row.get(2)?,
						config_sha: row.get(3)?,
						artifact_sha: row.get(4)?,
                        artifact_branch: row.get(5)?,
                        config_branch: row.get(6)?,
                        prev_artifact_sha: row.get(7)?,
                        prev_config_sha: row.get(8)?,
                        artifact_repo_id: row.get(9)?,
                        config_repo_id: row.get(10)?,
                        config_version_hash: row.get(11)?,
                        prev_config_version_hash: row.get(12)?,
					})
				})
				.and_then(|mapped| mapped.collect::<Result<Vec<_>, _>>());
			if let Ok(list) = rows {
				events = list;
			}
		}
		Err(e) => {
			log::error!("Failed to prepare deploy history query: {}", e);
		}
	}
    events
}

fn fetch_events_for_team(
    conn: &PooledConnection<SqliteConnectionManager>,
    team: &str,
) -> Vec<DeployEvent> {
    let mut events: Vec<DeployEvent> = Vec::new();
    match conn.prepare("SELECT de.name, de.timestamp, de.initiator, de.config_sha, de.artifact_sha, de.artifact_branch, de.config_branch, de.prev_artifact_sha, de.prev_config_sha, de.artifact_repo_id, de.config_repo_id, de.config_version_hash, de.prev_config_version_hash FROM deploy_event de WHERE de.name IN (SELECT name FROM deploy_config WHERE team = ?1) ORDER BY de.timestamp DESC") {
		Ok(mut stmt) => {
            let rows = stmt
                .query_map(params![team], |row| {
                    Ok(DeployEvent {
                        name: row.get(0)?,
                        timestamp: row.get(1)?,
                        initiator: row.get(2)?,
                        config_sha: row.get(3)?,
                        artifact_sha: row.get(4)?,
                        artifact_branch: row.get(5)?,
                        config_branch: row.get(6)?,
                        prev_artifact_sha: row.get(7)?,
                        prev_config_sha: row.get(8)?,
                        artifact_repo_id: row.get(9)?,
                        config_repo_id: row.get(10)?,
                        config_version_hash: row.get(11)?,
                        prev_config_version_hash: row.get(12)?,
                    })
                })
				.and_then(|mapped| mapped.collect::<Result<Vec<_>, _>>());
			if let Ok(list) = rows {
				events = list;
			}
		}
		Err(e) => {
			log::error!("Failed to prepare team deploy history query: {}", e);
		}
	}
    events
}

#[get("/deploy-history")]
pub async fn deploy_history_index(
    req: actix_web::HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    query: web::Query<HashMap<String, String>>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to connect to database");
        }
    };

    let name_param = query.get("name").cloned();
    let team_param = query.get("team").cloned();

    let (title, events): (String, Vec<DeployEvent>) = if let Some(name) = name_param {
        (
            format!("Deploy history for {}", name),
            fetch_events_for_name(&conn, &name),
        )
    } else if let Some(team) = team_param {
        (
            format!("Deploy history for team {}", team),
            fetch_events_for_team(&conn, &team),
        )
    } else {
        // Use teams cookie visibility
        let teams_cookie = TeamsCookie::from_request(&req);
        let mut acc: Vec<DeployEvent> = Vec::new();
        if let Some(set) = teams_cookie.0 {
            for team in set {
                let mut v = fetch_events_for_team(&conn, &team);
                acc.append(&mut v);
            }
        }
        // Sort reverse-chronological
        acc.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        ("Deploy history".to_string(), acc)
    };

    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (title.clone()) }
                (header::stylesheet_link())
                (header::scripts())
            }
            body.deploy-history-page hx-ext="morph" {
                (header::render("history"))
                div class="content" {
                    header {
                        h1 { (title) }
                    }
                    @if events.is_empty() {
                        div class="empty-state" {
                            h2 { "No history found" }
                            p { "There are no deploy events matching this filter." }
                        }
                    } @else {
                        table class="history-table" {
                            thead {
                                tr {
                                    th { "Deploy config" }
                                    th { "Artifact" }
                                    th { "Config" }
                                    th { "Deploy time" }
                                    th { "" }
                                }
                            }
                            tbody id="history-tbody"
                                hx-get=(if let Some(name) = query.get("name") { format!("/deploy-history-fragment?name={}", name) } else if let Some(team) = query.get("team") { format!("/deploy-history-fragment?team={}", team) } else { "/deploy-history-fragment".to_string() })
                                hx-trigger="load, every 5s"
                                hx-swap="morph:innerHTML" {
                                @for e in &events {
                                    (render_event_row(&conn, e))
                                }
                            }
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

#[get("/deploy-history-fragment")]
pub async fn deploy_history_fragment(
    req: actix_web::HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    query: web::Query<HashMap<String, String>>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to connect to database");
        }
    };

    let name_param = query.get("name").cloned();
    let team_param = query.get("team").cloned();

    let events: Vec<DeployEvent> = if let Some(name) = name_param {
        fetch_events_for_name(&conn, &name)
    } else if let Some(team) = team_param {
        fetch_events_for_team(&conn, &team)
    } else {
        let teams_cookie = TeamsCookie::from_request(&req);
        let mut acc: Vec<DeployEvent> = Vec::new();
        if let Some(set) = teams_cookie.0 {
            for team in set {
                let mut v = fetch_events_for_team(&conn, &team);
                acc.append(&mut v);
            }
        }
        acc.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        acc
    };

    let fragment = html! {
        @for e in &events {
            (render_event_row(&conn, e))
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(fragment.into_string())
}
