//! Settings > Webhook debug: shows the last few webhook events received in-process.

use actix_web::web;
use maud::{html, Markup};

use crate::prelude::*;
use crate::webhooks::recent::{self, RecordedWebhook, CAPACITY};

pub fn webhook_debug_fragment() -> HttpResponse {
    let markup = html! {
        header {
            h1 { "Webhook debug" }
            div class="subtitle" {
                (format!("The last {} webhook events received by this process. Held in memory only; cleared on restart.", CAPACITY))
            }
        }
        div class="webhook-events"
            id="webhook-events"
            hx-get="/webhooks/recent"
            hx-trigger="load, every 2s"
            hx-swap="innerHTML"
        { }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

fn render_event_row(event: &RecordedWebhook) -> Markup {
    let id = format!("webhook-{}", event.id);
    // hx-preserve keeps already-rendered rows (and their expanded state / loaded payload)
    // across polls; rows are immutable so this is safe.
    html! {
        details
            id=(id)
            class="webhook-event"
            hx-preserve="true"
            hx-get=(format!("/webhooks/recent/{}", event.id))
            hx-trigger="toggle once"
            hx-target="find .webhook-payload"
            hx-swap="innerHTML"
        {
            summary class="webhook-event-summary" {
                span class="webhook-event-time" title=(event.received_at.to_rfc3339()) {
                    (event.received_at.format("%Y-%m-%d %H:%M:%S UTC"))
                }
                span class="webhook-event-type" { (event.event_type) }
                span class="webhook-event-desc" { (event.summary()) }
            }
            div class="webhook-payload" { "Loading…" }
        }
    }
}

#[get("/webhooks/recent")]
pub async fn webhook_recent_list() -> impl Responder {
    let events = recent::list();
    let markup = html! {
        @if events.is_empty() {
            div class="empty-state" {
                h2 { "No webhook events yet" }
                p { "Events will appear here as they are received." }
            }
        } @else {
            @for e in &events {
                (render_event_row(e))
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

#[get("/webhooks/recent/{id}")]
pub async fn webhook_recent_payload(path: web::Path<u64>) -> impl Responder {
    let id = path.into_inner();
    let markup = match recent::get(id) {
        Some(e) => {
            let pretty =
                serde_json::to_string_pretty(&e.payload).unwrap_or_else(|_| e.payload.to_string());
            html! { pre class="webhook-payload-json" { (pretty) } }
        }
        None => html! {
            div class="webhook-payload-missing" {
                "This event is no longer retained (it has been evicted from the buffer)."
            }
        },
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}
