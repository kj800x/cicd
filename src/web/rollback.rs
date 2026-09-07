//! Roll a config back to an earlier revision.
//!
//! Rollback replays a revision's deployed values and then adds a blocker,
//! so autodeploy (when it exists) and ordinary deploys stay off the config
//! until a person decides the incident is over and clears it.

use std::collections::HashMap;

use actix_web::{post, web, HttpResponse, Responder};
use kube::Client;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::crab_ext::Octocrabs;
use crate::error::AppError;
use crate::kubernetes::api::get_deploy_config;
use crate::web::Action;

#[post("/api/rollback/{name}/{revision}")]
pub async fn rollback(
    path: web::Path<(String, i64)>,
    client: Option<web::Data<Client>>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    octocrabs: web::Data<Octocrabs>,
    form: web::Form<HashMap<String, String>>,
) -> impl Responder {
    let (name, revision) = path.into_inner();
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
            .body("Cannot roll back an orphaned deploy config; its config repository is gone.");
    }

    let action = Action::Rollback { revision };
    let result = crate::deploys::run_action(
        &action,
        &config,
        &client,
        &octocrabs,
        &pool,
        "web",
        &crate::deploys::SelectionIntent::default(),
    )
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
        Err(e) => {
            log::error!(
                "Rollback of {} to revision {} failed: {}",
                name,
                revision,
                e
            );
            return HttpResponse::InternalServerError().body(format!("Rollback failed: {e}"));
        }
    }

    let return_url = match form.get("return_url") {
        Some(url) if url.starts_with('/') && !url.starts_with("//") => url.clone(),
        _ => format!("/deploy-history?name={name}"),
    };
    HttpResponse::SeeOther()
        .append_header(("Location", return_url))
        .finish()
}
