//! The parameters panel on the deploy page and the route that sets a
//! static parameter's value.

use std::collections::HashMap;

use actix_web::{post, web, HttpResponse, Responder};
use kube::{Client, ResourceExt};
use maud::{html, Markup};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::crab_ext::Octocrabs;
use crate::error::AppError;
use crate::kubernetes::api::get_deploy_config;
use crate::kubernetes::parameters::{ParameterSource, SHA_PARAMETER};
use crate::kubernetes::selections::Mode;
use crate::kubernetes::DeployConfig;
use crate::web::Action;

/// Every declared parameter with what it is set to. Static parameters get
/// a form to override or reset them; the SHA parameter points at the
/// deploy form above.
pub fn render_parameters_panel(config: &DeployConfig, return_url: &str) -> Markup {
    let name = config.name_any();
    let declared = &config.spec.spec.parameters;
    if declared.is_empty() {
        return html! {};
    }
    let deployed = config.parameter_values();
    html! {
        div.parameters-panel {
            h3 { "Parameters" }
            @for (pname, source) in declared {
                @let selection = config.selection(pname);
                div.parameter-item {
                    div.parameter-head {
                        code { "$" (pname) }
                        " "
                        span.muted { (source.type_name()) }
                        @if selection.is_override() {
                            " " span class=(format!("durability durability-{}", selection.durability.as_str())) { (selection.durability.as_str()) }
                        }
                    }
                    @match source {
                        ParameterSource::Commit { .. } => {
                            div.parameter-value {
                                "Deployed: " strong { (deployed.get(pname).map(|v| crate::web::formatting::format_short_sha(v).to_string()).unwrap_or_else(|| "-".into())) }
                                @if pname == SHA_PARAMETER { " " span.muted { "(set with the deploy form above)" } }
                            }
                        }
                        ParameterSource::Value { default } => {
                            @let current = deployed.get(pname).cloned().unwrap_or_else(|| default.clone());
                            div.parameter-value {
                                "Deployed: " strong { (current) }
                                @match selection.mode() {
                                    Mode::Pin(v) => { " " span.muted { "(overridden to " (v) ", default " (default) ")" } }
                                    _ => { " " span.muted { "(default)" } }
                                }
                                @if let Some(note) = &selection.note { " " span.muted { "· " (note) } }
                            }
                            form action=(format!("/api/parameters/{}/{}", name, pname)) method="post" class="parameter-set" {
                                input type="hidden" name="return_url" value=(return_url);
                                input type="text" name="value" placeholder=(format!("new value (default {})", default)) aria-label="Value";
                                select name="durability" aria-label="How long" {
                                    option value="standing" { "standing" }
                                    option value="temporary" { "temporary" }
                                }
                                input type="text" name="note" placeholder="why" aria-label="Why";
                                input type="text" name="by" placeholder="who" aria-label="Who";
                                button type="submit" class="secondary-button" { "Set and deploy" }
                                @if selection.is_override() {
                                    button type="submit" name="reset" value="1" class="secondary-button" { "Reset to default" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[post("/api/parameters/{name}/{parameter}")]
pub async fn set_parameter(
    path: web::Path<(String, String)>,
    client: Option<web::Data<Client>>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    octocrabs: web::Data<Octocrabs>,
    form: web::Form<HashMap<String, String>>,
) -> impl Responder {
    let (name, parameter) = path.into_inner();
    let Some(client) = client else {
        return HttpResponse::ServiceUnavailable()
            .body("Kubernetes client is not available. Deploy functionality is disabled.");
    };
    let config = match get_deploy_config(&client, &name).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            return HttpResponse::NotFound().body(format!("DeployConfig {name} not found."))
        }
        Err(e) => {
            log::error!("Failed to get DeployConfig {}: {}", name, e);
            return HttpResponse::NotFound().body(format!("DeployConfig {name} not found."));
        }
    };
    if config.is_orphaned() {
        return HttpResponse::BadRequest()
            .body("Cannot change parameters on an orphaned deploy config.");
    }

    let reset = form.get("reset").is_some_and(|r| !r.is_empty());
    let value = form
        .get("value")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty());
    let action = match (reset, value) {
        (true, _) | (false, None) => Action::SetParameter {
            parameter: parameter.clone(),
            value: None,
        },
        (false, Some(v)) => Action::SetParameter {
            parameter: parameter.clone(),
            value: Some(v.to_string()),
        },
    };
    let intent = crate::deploys::SelectionIntent::from_form(&form);
    let result =
        crate::deploys::run_action(&action, &config, &client, &octocrabs, &pool, "web", &intent)
            .await;
    crate::metrics::get().deploy_actions.add(
        1,
        &[
            opentelemetry::KeyValue::new("name", name.clone()),
            opentelemetry::KeyValue::new("action", action.action_type()),
            opentelemetry::KeyValue::new(
                "result",
                if result.is_ok() { "success" } else { "error" },
            ),
        ],
    );
    match result {
        Ok(_) => {}
        Err(AppError::InvalidInput(message)) => return HttpResponse::BadRequest().body(message),
        Err(AppError::Blocked(message)) => return HttpResponse::Conflict().body(message),
        Err(e) => {
            log::error!("Setting {} on {} failed: {}", parameter, name, e);
            return HttpResponse::InternalServerError().body(format!("Failed: {e}"));
        }
    }
    let return_url = match form.get("return_url") {
        Some(url) if url.starts_with('/') && !url.starts_with("//") => url.clone(),
        _ => format!("/deploy?selected={name}"),
    };
    HttpResponse::SeeOther()
        .append_header(("Location", return_url))
        .finish()
}
