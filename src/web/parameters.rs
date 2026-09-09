//! The route that sets a static parameter's value. The deploy page now
//! changes parameters through the advanced form; this stays for direct
//! callers.

use std::collections::HashMap;

use actix_web::{post, web, HttpResponse, Responder};
use kube::Client;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::crab_ext::Octocrabs;
use crate::error::AppError;
use crate::kubernetes::api::get_deploy_config;
use crate::web::Action;

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
        Err(AppError::Unavailable(message)) => {
            return HttpResponse::ServiceUnavailable().body(message)
        }
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
