use std::collections::HashMap;

use actix_web::cookie::{time::Duration, Cookie};
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

#[get("/settings")]
pub async fn settings_index(req: actix_web::HttpRequest) -> impl Responder {
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
                div class="content" {
                    header {
                        h1 { "Settings" }
                        h3 { "Team visibility" }
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

                    div class="bootstrap-section" {
                        h3 { "Bootstrap" }
                        div class="subtitle" { "Initialize the database by scanning all accessible GitHub repositories." }
                        div class="bootstrap-actions" {
                            button
                                class="bootstrap-button"
                                hx-post="/bootstrap"
                                hx-swap="none"
                            { "Run bootstrap" }
                        }
                        div class="bootstrap-log-box" {
                            pre id="bootstrap-log"
                                hx-get="/bootstrap/log"
                                hx-trigger="load, every 1s"
                                hx-swap="innerHTML"
                                hx-on="htmx:afterSwap: this.scrollTop = this.scrollHeight"
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
