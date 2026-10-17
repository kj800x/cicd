//! The pre-check strip at the top of the deploy page: what the config's
//! pre-checks would say about a deploy right now, and a waiting
//! autodeploy if there is one.
//!
//! It loads as its own fragment, once, outside the preview that polls
//! every two seconds: a check is a request to another service and must not
//! be asked that often. "Ask again" reloads it. A soft block carries the
//! "deploy anyway" checkbox, which belongs to the deploy form through its
//! `form` attribute, so the gate sees the acknowledgement. The gate asks
//! the checks again when the deploy is submitted; this is only a preview.
use actix_web::{get, web, HttpResponse, Responder};
use kube::{Client, ResourceExt};
use maud::{html, Markup};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::checks::{ask_pre_checks, Ask, Mode, PreOutcome, PreStatus};
use crate::db::deploy_run::DeployRun;
use crate::kubernetes::api::get_deploy_config;
use crate::web::formatting;
use crate::web::preview::{render_alert, render_strip};

/// The empty slot the deploy page renders; it fills itself on load.
pub fn render_slot(namespace: &str, name: &str) -> Markup {
    html! {
        div #pre-checks hx-get=(fragment_url(namespace, name)) hx-trigger="load" hx-swap="innerHTML" {
            (render_strip("neutral", html! { "Asking pre-checks…" }))
        }
    }
}

fn fragment_url(namespace: &str, name: &str) -> String {
    format!("/fragments/pre-checks/{namespace}/{name}")
}

#[get("/fragments/pre-checks/{namespace}/{name}")]
pub async fn pre_checks_fragment(
    path: web::Path<(String, String)>,
    client: web::Data<Client>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    let config = match get_deploy_config(&client, &name).await {
        Ok(Some(config)) => config,
        _ => return HttpResponse::NotFound().body(format!("DeployConfig {name} not found.")),
    };
    let from = config
        .status
        .as_ref()
        .map(|s| {
            s.parameters
                .iter()
                .map(|(k, v)| (k.clone(), v.rendered()))
                .collect()
        })
        .unwrap_or_default();
    let ask = Ask {
        trigger: "web".to_string(),
        actor: "web".to_string(),
        from,
        attempt: 1,
        ..Default::default()
    };
    let outcomes = ask_pre_checks(&client, &config, Mode::Preview, &ask).await;
    let waiting = pool
        .get()
        .ok()
        .and_then(|conn| DeployRun::waiting(&conn).ok())
        .and_then(|runs| {
            runs.into_iter()
                .find(|r| r.config_name == config.name_any())
        });
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render(&namespace, &name, &outcomes, waiting.as_ref()).into_string())
}

/// The strip: a waiting autodeploy, then each check that would hold a
/// deploy, or one line when every check is OK.
pub fn render(
    namespace: &str,
    name: &str,
    outcomes: &[PreOutcome],
    waiting: Option<&DeployRun>,
) -> Markup {
    let holding: Vec<&PreOutcome> = outcomes
        .iter()
        .filter(|o| o.status != PreStatus::Ok)
        .collect();
    let again = html! {
        button.link-button type="button" hx-get=(fragment_url(namespace, name)) hx-target="#pre-checks" hx-swap="innerHTML" { "Ask again" }
    };
    html! {
        @if let Some(run) = waiting {
            (render_alert(
                "warning",
                html! { i class="fa fa-hourglass-half" {} " Autodeploy waiting since " (formatting::format_ago_short(run.created_at)) },
                html! {
                    @if let Some(message) = &run.message { (message) }
                    @if let Some(next) = run.next_check_at { " · asks again " (formatting::format_when(next)) }
                },
            ))
        }
        @for o in &holding {
            @let hard = o.status == PreStatus::HardBlock;
            (render_alert(
                if hard { "danger" } else { "warning" },
                html! {
                    i class=(if hard { "fa fa-ban" } else { "fa fa-exclamation-triangle" }) {}
                    " " (o.name) ": " (o.title.as_deref().unwrap_or(o.status.as_str()))
                },
                html! {
                    @if let Some(message) = &o.message { div { (message) } }
                    @if let Some(url) = &o.details_url { div { a href=(url) target="_blank" { "Details" } } }
                    @if hard {
                        div { "Deploys are refused until it clears; autodeploy waits." }
                    } @else {
                        label.pre-check-ack {
                            input type="checkbox" name=(format!("ack_{}", o.name)) value="on" form="deployForm";
                            " Deploy anyway"
                        }
                    }
                },
            ))
        }
        @if holding.is_empty() && !outcomes.is_empty() {
            (render_strip("neutral", html! {
                "Pre-checks OK: "
                @for (i, o) in outcomes.iter().enumerate() {
                    @if i > 0 { ", " }
                    (o.name)
                    @if let Some(title) = &o.title { span.muted { " (" (title) ")" } }
                }
                " · " (again)
            }))
        } @else if !outcomes.is_empty() {
            div.pre-checks__again { (again) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(name: &str, status: PreStatus, title: &str) -> PreOutcome {
        PreOutcome {
            name: name.into(),
            status,
            title: Some(title.into()),
            message: Some("alice on TV".into()),
            details_url: Some("https://j/x".into()),
            started_at: 0,
            finished_at: 0,
        }
    }

    #[test]
    fn soft_blocks_offer_deploy_anyway_and_hard_blocks_do_not() {
        let html = render(
            "media",
            "jellyfin",
            &[
                outcome("streams", PreStatus::SoftBlock, "2 active streams"),
                outcome("window", PreStatus::HardBlock, "maintenance"),
            ],
            None,
        )
        .into_string();
        assert!(html.contains("streams: 2 active streams"));
        assert!(html.contains(r#"name="ack_streams""#));
        assert!(html.contains(r#"form="deployForm""#));
        assert!(!html.contains("ack_window"));
        assert!(html.contains("Deploys are refused until it clears"));
    }

    #[test]
    fn all_ok_is_one_line() {
        let html = render(
            "media",
            "jellyfin",
            &[outcome("streams", PreStatus::Ok, "idle")],
            None,
        )
        .into_string();
        assert!(html.contains("Pre-checks OK: "));
        assert!(!html.contains("ack_"));
    }
}
