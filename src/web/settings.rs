use std::collections::HashMap;

use actix_web::cookie::{time::Duration, Cookie};
use actix_web::web;
use kube::Client;
use maud::{html, Markup, DOCTYPE};

use crate::kubernetes::api::get_all_deploy_configs;
use crate::prelude::*;
use crate::web::header;
use crate::web::team_prefs::{TeamsCookie, TEAMS_COOKIE};

fn render_team_row(team: &str, is_member: bool) -> Markup {
    let toggle_label = if is_member { "Visible" } else { "Hidden" };
    let toggle_class = if is_member {
        "team-toggle on"
    } else {
        "team-toggle off"
    };
    let target_id = format!("#team-{}", team);

    html! {
        div id=(format!("team-{}", team)) class="team-row" {
            div class="team-name" { (team) }
            button
                class=(toggle_class)
                hx-post="/teams/toggle"
                hx-vals=(format!(r#"{{"team":"{}"}}"#, team))
                hx-target=(target_id)
                hx-swap="outerHTML"
            {
                (toggle_label)
            }
        }
    }
}

fn render_sidebar(section: &str) -> Markup {
    html! {
        nav class="settings-sidebar" {
            h2 class="settings-sidebar-title" { "Settings" }
            a
                class=(if section == "rate-limits" { "settings-nav-link active" } else { "settings-nav-link" })
                href="/settings?section=rate-limits"
                hx-get="/settings-fragment?section=rate-limits"
                hx-target="#settings-content"
                hx-swap="morph:innerHTML"
                hx-push-url="/settings?section=rate-limits"
                onclick="document.querySelectorAll('.settings-nav-link').forEach(l => l.classList.remove('active')); this.classList.add('active');"
            {
                "GitHub API Rate Limits"
            }
            a
                class=(if section == "team-visibility" { "settings-nav-link active" } else { "settings-nav-link" })
                href="/settings?section=team-visibility"
                hx-get="/settings-fragment?section=team-visibility"
                hx-target="#settings-content"
                hx-swap="morph:innerHTML"
                hx-push-url="/settings?section=team-visibility"
                onclick="document.querySelectorAll('.settings-nav-link').forEach(l => l.classList.remove('active')); this.classList.add('active');"
            {
                "Team visibility"
            }
            a
                class=(if section == "bootstrap" { "settings-nav-link active" } else { "settings-nav-link" })
                href="/settings?section=bootstrap"
                hx-get="/settings-fragment?section=bootstrap"
                hx-target="#settings-content"
                hx-swap="morph:innerHTML"
                hx-push-url="/settings?section=bootstrap"
                onclick="document.querySelectorAll('.settings-nav-link').forEach(l => l.classList.remove('active')); this.classList.add('active');"
            {
                "Bootstrap"
            }
        }
    }
}

#[get("/settings")]
pub async fn settings_index(req: actix_web::HttpRequest) -> impl Responder {
    let query: web::Query<HashMap<String, String>> = web::Query::from_query(req.query_string())
        .unwrap_or_else(|_| web::Query(HashMap::new()));
    let section = query.get("section").map(|s| s.as_str()).unwrap_or("rate-limits");


    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { "Settings" }
                (header::stylesheet_link())
                (header::scripts())
            }
            body.settings-page hx-ext="morph" {
                (header::render("settings"))
                div class="settings-container" {
                    (render_sidebar(section))
                    div class="settings-content-wrapper" {
                        div id="settings-content" {
                            div
                                hx-get=(format!("/settings-fragment?section={}", section))
                                hx-trigger="load"
                                hx-swap="morph:innerHTML"
                            { }
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

#[get("/settings-fragment")]
pub async fn settings_fragment(
    req: actix_web::HttpRequest,
    query: web::Query<HashMap<String, String>>,
) -> impl Responder {
    let section = query.get("section").map(|s| s.as_str()).unwrap_or("rate-limits");

    match section {
        "team-visibility" => team_visibility_fragment(req).await,
        "rate-limits" => rate_limits_fragment().await,
        "bootstrap" => bootstrap_fragment().await,
        _ => team_visibility_fragment(req).await,
    }
}


async fn team_visibility_fragment(req: actix_web::HttpRequest) -> HttpResponse {
    // Initialize Kubernetes client
    let client = match Client::try_default().await {
        Ok(client) => client,
        Err(e) => {
            log::error!("Failed to initialize Kubernetes client: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to connect to Kubernetes".to_string());
        }
    };

    let all_configs = match get_all_deploy_configs(&client).await {
        Ok(cfgs) => cfgs,
        Err(e) => {
            log::error!("Failed to get all deploy configs: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to get all deploy configs".to_string());
        }
    };

    let mut all_teams: Vec<String> = all_configs
        .into_iter()
        .map(|c| c.team().to_string())
        .collect();
    all_teams.sort_unstable();
    all_teams.dedup();

    let memberships = TeamsCookie::from_request(&req);

    let markup = html! {
        header {
            h1 { "Team visibility" }
            div class="subtitle" { "Choose which teams' deploy configs to show" }
        }

        @if all_teams.is_empty() {
            div class="empty-state" {
                h2 { "No teams found" }
                p { "No teams were discovered across DeployConfigs." }
            }
        } @else {
            div class="teams-list" {
                @for t in &all_teams {
                    (render_team_row(t, memberships.is_member(t)))
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

async fn rate_limits_fragment() -> HttpResponse {
    let markup = html! {
        header {
            h1 { "GitHub API Rate Limits" }
            div class="subtitle" { "Current rate limit status for configured tokens." }
        }
        div class="rate-limits-display"
            id="rate-limits"
            hx-get="/rate-limits"
            hx-trigger="revealed, every 5s"
            hx-swap="innerHTML"
        { }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

async fn bootstrap_fragment() -> HttpResponse {
    let markup = html! {
        header {
            h1 { "Bootstrap" }
            div class="subtitle" { "Initialize or update the database by scanning GitHub repositories." }
        }

        div class="bootstrap-mode" {
            h4 { "Quick Scan" }
            div class="bootstrap-description" { "Scan all owner repos, import 1 commit from default branch" }
            button
                class="bootstrap-button"
                hx-post="/bootstrap/quick"
                hx-swap="none"
            { "Run Quick Scan" }
        }

        div class="bootstrap-mode" {
            h4 { "Owner Sync" }
            div class="bootstrap-description" { "Scan all owner repos, import 10 commits from default branch" }
            button
                class="bootstrap-button"
                hx-post="/bootstrap/owner"
                hx-swap="none"
            { "Run Owner Sync" }
        }

        div class="bootstrap-mode" {
            h4 { "Deep Repo Scan" }
            div class="bootstrap-description" { "Scan all branches of a specific repo, import 50 commits each" }
            div class="repo-input-group" {
                input type="text" id="repo-owner" placeholder="owner" class="repo-input" {}
                span class="repo-separator" { "/" }
                input type="text" id="repo-name" placeholder="repo" class="repo-input" {}
                button
                    class="bootstrap-button"
                    onclick="fetch('/bootstrap/repo', { method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({ owner: document.getElementById('repo-owner').value, repo: document.getElementById('repo-name').value }) })"
                { "Run Deep Scan" }
            }
        }

        div class="bootstrap-mode" {
            h4 { "Repo Resync" }
            div class="bootstrap-description" { "Quick resync: fetch latest commit from default branch and update deploy configs" }
            div class="repo-input-group" {
                input type="text" id="resync-owner" placeholder="owner" class="repo-input" {}
                span class="repo-separator" { "/" }
                input type="text" id="resync-repo" placeholder="repo" class="repo-input" {}
                button
                    class="bootstrap-button"
                    onclick="fetch('/bootstrap/repo/resync', { method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({ owner: document.getElementById('resync-owner').value, repo: document.getElementById('resync-repo').value }) })"
                { "Resync Repo" }
            }
        }

        div class="bootstrap-log-box"
            id="bootstrap-log"
            hx-get="/bootstrap/log"
            hx-trigger="load, every 1s"
            hx-swap="innerHTML"
        { }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

#[post("/teams/toggle")]
pub async fn toggle_team(
    req: actix_web::HttpRequest,
    form: web::Form<HashMap<String, String>>,
) -> impl Responder {
    let team = form.get("team").cloned().unwrap_or_default();
    if team.is_empty() {
        return HttpResponse::BadRequest().body("Missing 'team'");
    }

    // Load, mutate, and persist
    let mut set = TeamsCookie::from_request(&req).0.unwrap_or_default();
    let new_state = if set.contains(&team) {
        set.remove(&team);
        false
    } else {
        set.insert(team.clone());
        true
    };

    let cookie_val = TeamsCookie(Some(set)).serialize();
    let cookie = Cookie::build(TEAMS_COOKIE, cookie_val)
        .path("/")
        .http_only(false)
        .max_age(Duration::days(365))
        .finish();

    let fragment = render_team_row(&team, new_state).into_string();

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .cookie(cookie)
        .body(fragment)
}
