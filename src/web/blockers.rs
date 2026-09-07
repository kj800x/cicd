//! Blocker management on the deploy page: the "held" strip, the alert on a
//! held config, the panel to add and clear blockers, and the two form
//! handlers behind it.

use std::collections::HashMap;

use actix_web::{post, web, HttpResponse, Responder};
use maud::{html, Markup};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::db::blocker::Blocker;
use crate::web::HumanTime;

/// Where the blocker forms send people afterwards. Only same-site paths are
/// accepted so the hidden field cannot be turned into an open redirect.
fn return_url(form: &HashMap<String, String>, config_name: &str) -> String {
    match form.get("return_url") {
        Some(url) if url.starts_with('/') && !url.starts_with("//") => url.clone(),
        _ => format!("/deploy?selected={config_name}"),
    }
}

fn who(form: &HashMap<String, String>) -> String {
    form.get("by").map(|s| s.trim()).unwrap_or("").to_string()
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
                    "Deploys are refused until every blocker is cleared. Undeploy, bounce and job execution still work."
                    ul.blocker-reasons {
                        @for b in blockers {
                            li { (b.reason) " " span.muted { "(" (b.created_by) ", " (HumanTime(b.created_at as u64)) ")" } }
                        }
                    }
                }
            }
        }
    }
}

/// The panel in the left column: active blockers with clear buttons, and a
/// form to add one.
pub fn render_blocker_panel(blockers: &[Blocker], config_name: &str, return_url: &str) -> Markup {
    html! {
        div.blocker-panel {
            h3 { "Blockers" }
            @if blockers.is_empty() {
                p.muted { "None. Deploys are allowed." }
            } @else {
                ul.blocker-list {
                    @for b in blockers {
                        li.blocker-item {
                            div.blocker-reason { (b.reason) }
                            div.blocker-meta.muted { (b.created_by) ", " (HumanTime(b.created_at as u64)) }
                            form action=(format!("/api/blockers/{}/{}/clear", config_name, b.id)) method="post" class="blocker-clear" {
                                input type="hidden" name="return_url" value=(return_url);
                                input type="text" name="by" placeholder="cleared by" aria-label="Cleared by";
                                button type="submit" class="secondary-button" { "Clear" }
                            }
                        }
                    }
                }
            }
            form action=(format!("/api/blockers/{}", config_name)) method="post" class="blocker-add" {
                input type="hidden" name="return_url" value=(return_url);
                div class="action-input" {
                    label for="blocker-reason" { "Add a blocker" }
                    input id="blocker-reason" type="text" name="reason" placeholder="Why deploys must wait" required;
                }
                div class="action-input" {
                    label for="blocker-by" { "Your name" }
                    input id="blocker-by" type="text" name="by" placeholder="who is holding it";
                }
                button type="submit" class="primary-action-button danger-button" { "Hold deploys" }
            }
        }
    }
}

/// One line at the top of the deploy page listing every held config.
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
        div.held-strip {
            i class="fa fa-hand-paper-o" {}
            " Held: "
            @for (i, name) in names.iter().enumerate() {
                @if i > 0 { ", " }
                a href=(format!("/deploy?selected={name}")) { (name) }
            }
        }
    }
}

#[post("/api/blockers/{name}")]
pub async fn add_blocker(
    path: web::Path<String>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    form: web::Form<HashMap<String, String>>,
) -> impl Responder {
    let name = path.into_inner();
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Failed to connect to database");
        }
    };
    let reason = form.get("reason").map(String::as_str).unwrap_or("");
    match Blocker::create(&conn, &name, reason, &who(&form)) {
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
        .append_header(("Location", return_url(&form, &name)))
        .finish()
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
        .append_header(("Location", return_url(&form, &name)))
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_url_only_accepts_same_site_paths() {
        let mut form = HashMap::new();
        assert_eq!(return_url(&form, "site"), "/deploy?selected=site");
        form.insert(
            "return_url".into(),
            "/deploy?selected=site&action=deploy".into(),
        );
        assert_eq!(
            return_url(&form, "site"),
            "/deploy?selected=site&action=deploy"
        );
        form.insert("return_url".into(), "https://evil.example/".into());
        assert_eq!(return_url(&form, "site"), "/deploy?selected=site");
        form.insert("return_url".into(), "//evil.example/".into());
        assert_eq!(return_url(&form, "site"), "/deploy?selected=site");
    }

    #[test]
    fn held_strip_lists_each_config_once() {
        let b = |name: &str, id: i64| Blocker {
            id,
            config_name: name.into(),
            reason: "r".into(),
            created_by: "k".into(),
            created_at: 0,
            cleared_by: None,
            cleared_at: None,
        };
        let html = render_held_strip(&[b("a", 1), b("b", 2), b("a", 3)]).into_string();
        assert_eq!(html.matches("selected=a").count(), 1);
        assert_eq!(html.matches("selected=b").count(), 1);
        assert!(render_held_strip(&[]).into_string().is_empty());
    }
}
