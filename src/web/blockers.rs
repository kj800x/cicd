//! Blockers: the page where holds are made and cleared, the "held" strip
//! at the top of the deploy page, the alert in a held config's preview,
//! and the form handlers behind them. The deploy page itself creates no
//! blockers; it only points here.

use std::collections::HashMap;

use actix_web::{get, post, web, HttpRequest, HttpResponse, Responder};
use kube::{Client, ResourceExt};
use maud::{html, Markup, DOCTYPE};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::blocker::Blocker;
use crate::web::formatting;
use crate::web::header;
use crate::web::team_prefs::TeamsCookie;

/// How many cleared blockers the page lists.
const CLEARED_LIMIT: usize = 20;

/// Where the blocker forms send people afterwards. Only same-site paths are
/// accepted so the hidden field cannot be turned into an open redirect.
fn return_url(form: &HashMap<String, String>, fallback: &str) -> String {
    match form.get("return_url") {
        Some(url) if url.starts_with('/') && !url.starts_with("//") => url.clone(),
        _ => fallback.to_string(),
    }
}

/// The forms ask no name; everything made here records as `web`.
fn who(form: &HashMap<String, String>) -> String {
    form.get("by")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("web")
        .to_string()
}

fn reason_meta(b: &Blocker) -> Markup {
    html! {
        span.muted { "(" (b.created_by) ", " (formatting::format_ago_short(b.created_at)) ")" }
    }
}

/// Alert shown in the (polled) preview while a config is held.
pub fn render_blocker_alert(blockers: &[Blocker]) -> Markup {
    html! {
        div.alert.alert-danger {
            div class="alert-header" {
                i class="fa fa-hand-paper-o" {}
                @if blockers.len() == 1 { " Held by a blocker" } @else { (format!(" Held by {} blockers", blockers.len())) }
            }
            div class="alert-content" {
                div class="details" {
                    ul.blocker-reasons {
                        @for b in blockers {
                            li { (b.reason) " " (reason_meta(b)) }
                        }
                    }
                    div.alert-link { a href="/blockers" { "Manage on Blockers" } }
                }
            }
        }
    }
}

/// The full-bleed strip under the nav listing every held config.
pub fn render_held_strip(blockers: &[Blocker]) -> Markup {
    if blockers.is_empty() {
        return html! {};
    }
    // One entry per config, keeping first-seen order (oldest blocker first).
    let mut names: Vec<&str> = Vec::new();
    for b in blockers {
        if !names.contains(&b.config_name.as_str()) {
            names.push(&b.config_name);
        }
    }
    html! {
        div.page-strip.page-strip--fail {
            i class="fa fa-hand-paper-o" {}
            span {
                "Held: "
                @for (i, name) in names.iter().enumerate() {
                    @if i > 0 { ", " }
                    a href=(format!("/deploy?selected={name}")) { (name) }
                }
                span.muted { " · " a href="/blockers" { "Blockers" } }
            }
        }
    }
}

fn render_active_list(blockers: &[Blocker]) -> Markup {
    html! {
        div.blocker-table {
            div.blocker-table__head {
                span { "Config" }
                span { "Reason" }
                span { "Held since" }
                span {}
            }
            @if blockers.is_empty() {
                div.blocker-table__empty { "None. Deploys are allowed everywhere." }
            }
            @for b in blockers {
                div.blocker-table__row {
                    a.blocker-table__config href=(format!("/deploy?selected={}", b.config_name)) { (b.config_name) }
                    span.blocker-table__reason { (b.reason) }
                    span.blocker-table__since {
                        (formatting::format_ago_short(b.created_at))
                        " · "
                        span.mono { (b.created_by) }
                    }
                    form action=(format!("/api/blockers/{}/{}/clear", b.config_name, b.id)) method="post" {
                        input type="hidden" name="return_url" value="/blockers";
                        button type="submit" class="secondary-button" { "Clear" }
                    }
                }
            }
        }
    }
}

fn render_cleared(cleared: &[Blocker]) -> Markup {
    html! {
        details.disclosure {
            summary.disclosure__summary {
                span.disclosure__caret {}
                span.disclosure__label { "Recently cleared" }
                span.disclosure__meta { "last " (CLEARED_LIMIT) " · newest first" }
            }
            div.disclosure__panel {
                @if cleared.is_empty() {
                    p.muted { "None" }
                } @else {
                    table.boxed-table {
                        thead {
                            tr {
                                th { "Config" }
                                th { "Reason" }
                                th { "Held" }
                                th { "Cleared by" }
                                th { "When" }
                            }
                        }
                        tbody {
                            @for b in cleared {
                                @let cleared_at = b.cleared_at.unwrap_or(b.created_at);
                                tr {
                                    td { a href=(format!("/deploy?selected={}", b.config_name)) { (b.config_name) } }
                                    td.truncate title=(b.reason) { (b.reason) }
                                    td { (formatting::format_duration_short(cleared_at.saturating_sub(b.created_at))) }
                                    td { (b.cleared_by.clone().unwrap_or_else(|| "unknown".to_string())) }
                                    td.mono { (formatting::format_date(cleared_at)) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn render_hold_form(config_names: &[String]) -> Markup {
    html! {
        section.hold-panel {
            h2.hold-panel__title { "Hold deploys" }
            form action="/api/blockers" method="post" {
                input type="hidden" name="return_url" value="/blockers";
                label.field-label for="hold-config" { "Deploy config" }
                @if config_names.is_empty() {
                    input id="hold-config" type="text" name="name" placeholder="deploy config name" required;
                } @else {
                    select id="hold-config" name="name" {
                        @for name in config_names {
                            option value=(name) { (name) }
                        }
                    }
                }
                label.field-label for="hold-reason" { "Reason" }
                input id="hold-reason" type="text" name="reason" placeholder="Why deploys must wait" required;
                button type="submit" class="primary-button danger block" { "Hold deploys" }
            }
        }
    }
}

/// The whole page: the active list, the cleared disclosure and the hold
/// form in a sticky rail.
pub fn render_blockers_page(
    active: &[Blocker],
    cleared: &[Blocker],
    config_names: &[String],
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { "Blockers" }
                (header::stylesheet_link())
                (header::scripts())
            }
            body.blockers-page {
                (header::render("blockers"))
                div class="content" {
                    header.page-title {
                        h1 { "Blockers" }
                        p.subtitle { "Deploys are refused while a config is held." }
                    }
                    div.blockers-container {
                        div.blockers-main {
                            (render_active_list(active))
                            (render_cleared(cleared))
                        }
                        (render_hold_form(config_names))
                    }
                }
            }
        }
    }
}

#[get("/blockers")]
pub async fn blockers_page(
    req: HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Failed to connect to database");
        }
    };
    let active = Blocker::all_active(&conn).unwrap_or_else(|e| {
        log::error!("Failed to load active blockers: {}", e);
        vec![]
    });
    let cleared = Blocker::recently_cleared(&conn, CLEARED_LIMIT).unwrap_or_else(|e| {
        log::error!("Failed to load cleared blockers: {}", e);
        vec![]
    });

    // The config list is for the picker; without a cluster the form falls
    // back to a text box.
    let mut config_names: Vec<String> = match Client::try_default().await {
        Ok(client) => match crate::kubernetes::api::get_all_deploy_configs(&client).await {
            Ok(configs) => TeamsCookie::from_request(&req)
                .filter_configs(&configs)
                .iter()
                .map(|c| c.name_any())
                .collect(),
            Err(e) => {
                log::warn!("Failed to list deploy configs for the blockers page: {}", e);
                vec![]
            }
        },
        Err(e) => {
            log::warn!("No Kubernetes client for the blockers page: {}", e);
            vec![]
        }
    };
    config_names.sort();

    let markup = render_blockers_page(&active, &cleared, &config_names);
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

fn create(
    pool: &Pool<SqliteConnectionManager>,
    name: &str,
    form: &HashMap<String, String>,
    fallback_url: &str,
) -> HttpResponse {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Failed to connect to database");
        }
    };
    let reason = form.get("reason").map(String::as_str).unwrap_or("");
    match Blocker::create(&conn, name, reason, &who(form)) {
        Ok(b) => log::info!(
            "Blocker {} added on {} by {}: {}",
            b.id,
            name,
            b.created_by,
            b.reason
        ),
        Err(crate::error::AppError::InvalidInput(msg)) => {
            return HttpResponse::BadRequest().body(msg);
        }
        Err(e) => {
            log::error!("Failed to add blocker on {}: {}", name, e);
            return HttpResponse::InternalServerError().body("Failed to add blocker");
        }
    }
    HttpResponse::SeeOther()
        .append_header(("Location", return_url(form, fallback_url)))
        .finish()
}

/// The blockers page's form: the config is a field, not a path segment.
#[post("/api/blockers")]
pub async fn add_blocker_for(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    form: web::Form<HashMap<String, String>>,
) -> impl Responder {
    let name = form.get("name").map(|s| s.trim()).unwrap_or("");
    if name.is_empty() {
        return HttpResponse::BadRequest().body("Pick a deploy config to hold");
    }
    create(&pool, name, &form, "/blockers")
}

#[post("/api/blockers/{name}")]
pub async fn add_blocker(
    path: web::Path<String>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    form: web::Form<HashMap<String, String>>,
) -> impl Responder {
    let name = path.into_inner();
    create(&pool, &name, &form, &format!("/deploy?selected={name}"))
}

#[post("/api/blockers/{name}/{id}/clear")]
pub async fn clear_blocker(
    path: web::Path<(String, i64)>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    form: web::Form<HashMap<String, String>>,
) -> impl Responder {
    let (name, id) = path.into_inner();
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Failed to connect to database");
        }
    };
    // The id in the path must belong to the named config; a stale link from
    // another page must not clear someone else's hold.
    match Blocker::get(&conn, id) {
        Ok(Some(b)) if b.config_name == name => {}
        Ok(_) => return HttpResponse::NotFound().body("No such blocker on this config"),
        Err(e) => {
            log::error!("Failed to look up blocker {}: {}", id, e);
            return HttpResponse::InternalServerError().body("Failed to look up blocker");
        }
    }
    match Blocker::clear(&conn, id, &who(&form)) {
        Ok(true) => log::info!("Blocker {} on {} cleared", id, name),
        Ok(false) => log::info!("Blocker {} on {} was already cleared", id, name),
        Err(e) => {
            log::error!("Failed to clear blocker {}: {}", id, e);
            return HttpResponse::InternalServerError().body("Failed to clear blocker");
        }
    }
    HttpResponse::SeeOther()
        .append_header((
            "Location",
            return_url(&form, &format!("/deploy?selected={name}")),
        ))
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_url_only_accepts_same_site_paths() {
        let mut form = HashMap::new();
        assert_eq!(return_url(&form, "/blockers"), "/blockers");
        form.insert(
            "return_url".into(),
            "/deploy?selected=site&action=deploy".into(),
        );
        assert_eq!(
            return_url(&form, "/blockers"),
            "/deploy?selected=site&action=deploy"
        );
        form.insert("return_url".into(), "https://evil.example/".into());
        assert_eq!(return_url(&form, "/blockers"), "/blockers");
        form.insert("return_url".into(), "//evil.example/".into());
        assert_eq!(return_url(&form, "/blockers"), "/blockers");
    }

    #[test]
    fn actor_defaults_to_web() {
        let mut form = HashMap::new();
        assert_eq!(who(&form), "web");
        form.insert("by".into(), "  ".into());
        assert_eq!(who(&form), "web");
        form.insert("by".into(), " kevin ".into());
        assert_eq!(who(&form), "kevin");
    }

    fn blocker(name: &str, id: i64) -> Blocker {
        Blocker {
            id,
            config_name: name.into(),
            reason: "r".into(),
            created_by: "web".into(),
            created_at: 0,
            cleared_by: None,
            cleared_at: None,
        }
    }

    #[test]
    fn held_strip_lists_each_config_once_and_links_to_the_page() {
        let html =
            render_held_strip(&[blocker("a", 1), blocker("b", 2), blocker("a", 3)]).into_string();
        assert_eq!(html.matches("selected=a").count(), 1);
        assert_eq!(html.matches("selected=b").count(), 1);
        assert!(html.contains("href=\"/blockers\""));
        assert!(render_held_strip(&[]).into_string().is_empty());
    }

    #[test]
    fn the_alert_counts_and_points_at_the_page() {
        let one = render_blocker_alert(&[blocker("a", 1)]).into_string();
        assert!(one.contains("Held by a blocker"));
        let two = render_blocker_alert(&[blocker("a", 1), blocker("a", 2)]).into_string();
        assert!(two.contains("Held by 2 blockers"));
        assert!(two.contains("Manage on Blockers"));
    }
}
