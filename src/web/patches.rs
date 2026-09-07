//! The patches panel on the deploy page and the routes that add and remove
//! manifest patches.

use std::collections::HashMap;

use actix_web::{post, web, HttpResponse, Responder};
use kube::{Client, ResourceExt};
use maud::{html, Markup};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::crab_ext::Octocrabs;
use crate::deploys::PatchChange;
use crate::error::AppError;
use crate::kubernetes::api::get_deploy_config;
use crate::kubernetes::patches::{ManifestPatch, PatchOp, PatchTarget};
use crate::kubernetes::selections::Durability;
use crate::kubernetes::DeployConfig;

/// Active patches with remove buttons, and a form to add one. The kind and
/// name options come from the config's own templates so the common case is
/// a pick, not a guess.
pub fn render_patches_panel(config: &DeployConfig, return_url: &str) -> Markup {
    let name = config.name_any();
    let targets: Vec<(Option<String>, String, String)> = config
        .resource_templates()
        .into_iter()
        .filter_map(|t| {
            let kind = t.manifest.get("kind")?.as_str()?.to_string();
            let mname = t
                .manifest
                .get("metadata")?
                .get("name")?
                .as_str()?
                .to_string();
            Some((t.file, kind, mname))
        })
        .collect();
    html! {
        div.patches-panel {
            h3 { "Patches" }
            @if config.spec.spec.patches.is_empty() {
                p.muted { "None. The manifests deploy as written in the config repo." }
            } @else {
                ul.patch-list {
                    @for (i, p) in config.spec.spec.patches.iter().enumerate() {
                        li.patch-item {
                            code { (p.describe()) }
                            @if let Some(note) = &p.note { " " span.muted { "· " (note) } }
                            @if let Some(by) = &p.by { " " span.muted { "(" (by) ")" } }
                            form action=(format!("/api/patches/{}/{}/remove", name, i)) method="post" class="patch-remove" {
                                input type="hidden" name="return_url" value=(return_url);
                                button type="submit" class="secondary-button" { "Remove" }
                            }
                        }
                    }
                }
            }
            @if !targets.is_empty() {
                form action=(format!("/api/patches/{}", name)) method="post" class="patch-add" {
                    input type="hidden" name="return_url" value=(return_url);
                    select name="target" aria-label="Manifest" {
                        @for (file, kind, mname) in &targets {
                            @let value = format!("{}|{}|{}", file.clone().unwrap_or_default(), kind, mname);
                            option value=(value) { (kind) "/" (mname) @if let Some(f) = file { " (" (f) ")" } }
                        }
                    }
                    select name="op" aria-label="Operation" {
                        option value="replace" { "replace" }
                        option value="add" { "add" }
                        option value="remove" { "remove" }
                    }
                    input type="text" name="path" placeholder="/spec/replicas" aria-label="JSON pointer" required;
                    input type="text" name="value" placeholder="value (JSON, or plain text)" aria-label="Value";
                    select name="durability" aria-label="How long" {
                        option value="temporary" { "temporary" }
                        option value="standing" { "standing" }
                    }
                    input type="text" name="note" placeholder="why" aria-label="Why";
                    input type="text" name="by" placeholder="who" aria-label="Who";
                    button type="submit" class="primary-action-button" { "Add patch and apply" }
                }
            }
        }
    }
}

/// A value typed into the form: JSON if it parses, otherwise the raw string.
fn parse_value(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

fn patch_from_form(form: &HashMap<String, String>) -> Result<ManifestPatch, String> {
    let target = form.get("target").cloned().unwrap_or_default();
    let mut parts = target.splitn(3, '|');
    let file = parts.next().filter(|f| !f.is_empty()).map(String::from);
    let kind = parts.next().unwrap_or_default().to_string();
    let mname = parts.next().unwrap_or_default().to_string();
    if kind.is_empty() || mname.is_empty() {
        return Err("Pick a manifest to patch".to_string());
    }
    let op = match form.get("op").map(String::as_str) {
        Some("add") => PatchOp::Add,
        Some("remove") => PatchOp::Remove,
        _ => PatchOp::Replace,
    };
    let path = form.get("path").map(|p| p.trim()).unwrap_or("").to_string();
    if !path.starts_with('/') {
        return Err("The path must be a JSON pointer starting with /".to_string());
    }
    let value = form
        .get("value")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(parse_value);
    let durability = match form.get("durability").map(String::as_str) {
        Some("standing") => Durability::Standing,
        _ => Durability::Temporary,
    };
    let clean = |k: &str| {
        form.get(k)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    Ok(ManifestPatch {
        target: PatchTarget {
            file,
            kind,
            name: mname,
        },
        op,
        path,
        value,
        durability,
        note: clean("note"),
        by: clean("by"),
        since: Some(chrono::Utc::now().to_rfc3339()),
    })
}

async fn apply_change(
    name: &str,
    change: PatchChange,
    client: Option<web::Data<Client>>,
    pool: &Pool<SqliteConnectionManager>,
    octocrabs: &Octocrabs,
    form: &HashMap<String, String>,
) -> HttpResponse {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Failed to connect to database");
        }
    };
    let Some(client) = client else {
        return HttpResponse::ServiceUnavailable()
            .body("Kubernetes client is not available. Deploy functionality is disabled.");
    };
    let config = match get_deploy_config(&client, name).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            return HttpResponse::NotFound().body(format!("DeployConfig {name} not found."))
        }
        Err(e) => {
            log::error!("Failed to get DeployConfig {}: {}", name, e);
            return HttpResponse::NotFound().body(format!("DeployConfig {name} not found."));
        }
    };
    let actor = form
        .get("by")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("web");
    match crate::deploys::change_patches(&config, &client, &conn, actor, change, octocrabs).await {
        Ok(()) => {}
        Err(AppError::InvalidInput(message)) => return HttpResponse::BadRequest().body(message),
        Err(AppError::Blocked(message)) => return HttpResponse::Conflict().body(message),
        Err(e) => {
            log::error!("Patch change on {} failed: {}", name, e);
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

#[post("/api/patches/{name}")]
pub async fn add_patch(
    path: web::Path<String>,
    client: Option<web::Data<Client>>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    octocrabs: web::Data<Octocrabs>,
    form: web::Form<HashMap<String, String>>,
) -> impl Responder {
    let name = path.into_inner();
    let patch = match patch_from_form(&form) {
        Ok(p) => p,
        Err(message) => return HttpResponse::BadRequest().body(message),
    };
    apply_change(
        &name,
        PatchChange::Add(patch),
        client,
        &pool,
        &octocrabs,
        &form,
    )
    .await
}

#[post("/api/patches/{name}/{index}/remove")]
pub async fn remove_patch(
    path: web::Path<(String, usize)>,
    client: Option<web::Data<Client>>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    octocrabs: web::Data<Octocrabs>,
    form: web::Form<HashMap<String, String>>,
) -> impl Responder {
    let (name, index) = path.into_inner();
    apply_change(
        &name,
        PatchChange::Remove(index),
        client,
        &pool,
        &octocrabs,
        &form,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_values_parse_as_json_when_possible() {
        assert_eq!(parse_value("3"), serde_json::json!(3));
        assert_eq!(parse_value(r#"{"a":1}"#), serde_json::json!({"a": 1}));
        assert_eq!(parse_value("debug"), serde_json::json!("debug"));
    }

    #[test]
    fn form_to_patch_validates_target_and_path() {
        let mut form = HashMap::new();
        form.insert(
            "target".to_string(),
            "deployment.yaml|Deployment|web".to_string(),
        );
        form.insert("op".to_string(), "replace".to_string());
        form.insert("path".to_string(), "/spec/replicas".to_string());
        form.insert("value".to_string(), "5".to_string());
        let p = patch_from_form(&form).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.target.file.as_deref(), Some("deployment.yaml"));
        assert_eq!(p.target.kind, "Deployment");
        assert_eq!(p.value, Some(serde_json::json!(5)));
        assert_eq!(p.durability, Durability::Temporary);

        form.insert("path".to_string(), "spec/replicas".to_string());
        assert!(patch_from_form(&form).is_err());
        form.insert("path".to_string(), "/x".to_string());
        form.insert("target".to_string(), "||".to_string());
        assert!(patch_from_form(&form).is_err());
    }
}
