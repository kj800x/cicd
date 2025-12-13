use crate::error::{format_error_chain, AppError};
use crate::kubernetes::list_namespace_objects;
use crate::prelude::*;
use crate::web::header;
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{Api, DynamicObject, LogParams, ResourceExt},
    Client,
};
use maud::html;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Find a resource by UID from the namespace objects
fn find_resource_by_uid<'a>(
    uid: &str,
    namespaced_objs: &'a [DynamicObject],
) -> Option<&'a DynamicObject> {
    namespaced_objs
        .iter()
        .find(|o| o.metadata.uid.as_deref() == Some(uid))
}

/// Find a resource by UID in a specific namespace
async fn find_resource_by_uid_in_namespace(
    client: &Client,
    uid: &str,
    namespace: &str,
) -> AppResult<Option<(DynamicObject, Vec<DynamicObject>)>> {
    let objs = list_namespace_objects(client, namespace, crate::kubernetes::api::ListMode::All).await?;
    if let Some(obj) = find_resource_by_uid(uid, &objs) {
        Ok(Some((obj.clone(), objs)))
    } else {
        Ok(None)
    }
}

/// Find a resource by UID by searching all namespaces (fallback)
async fn find_resource_by_uid_all_namespaces(
    client: &Client,
    uid: &str,
) -> AppResult<Option<(DynamicObject, Vec<DynamicObject>, String)>> {
    let deploy_configs = crate::kubernetes::api::get_all_deploy_configs(client).await?;

    for config in deploy_configs {
        let ns = config.namespace().unwrap_or_else(|| "default".to_string());
        if let Ok(Some((obj, objs))) = find_resource_by_uid_in_namespace(client, uid, &ns).await {
            return Ok(Some((obj, objs, ns)));
        }
    }

    Ok(None)
}

/// Get pods owned by a resource
fn get_owned_pods<'a>(
    obj: &DynamicObject,
    namespaced_objs: &'a [DynamicObject],
) -> Vec<&'a DynamicObject> {
    let Some(obj_uid) = &obj.metadata.uid else {
        return vec![];
    };

    namespaced_objs
        .iter()
        .filter(|o| {
            o.types.as_ref().map(|t| t.kind.as_str()) == Some("Pod")
                && o.metadata
                    .owner_references
                    .as_ref()
                    .unwrap_or(&Vec::new())
                    .iter()
                    .any(|or| or.uid == *obj_uid)
        })
        .collect()
}

/// Deserialize a DynamicObject to a typed resource
fn from_dynamic_object<T: DeserializeOwned>(obj: &DynamicObject) -> Result<T, AppError> {
    let value: Value = serde_json::to_value(obj)
        .map_err(|e| AppError::Internal(format!("Failed to serialize DynamicObject: {}", e)))?;
    serde_json::from_value(value)
        .map_err(|e| AppError::Internal(format!("Failed to deserialize DynamicObject: {}", e)))
}

/// Get logs for a pod
async fn get_pod_logs(client: &Client, pod: &Pod, tail_lines: Option<u64>) -> AppResult<String> {
    let namespace = pod
        .namespace()
        .ok_or_else(|| AppError::Internal("Pod missing namespace".to_string()))?;
    let name = pod.name_any();

    let pods_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);

    let mut params = LogParams::default();
    if let Some(tail) = tail_lines {
        params.tail_lines = Some(tail as i64);
    }
    params.follow = false; // We'll poll via HTMX instead

    let logs = pods_api.logs(&name, &params).await?;
    Ok(logs)
}

/// Get logs for a resource (handles Pods, Jobs, Deployments, ReplicaSets, CronJobs)
async fn get_resource_logs(
    client: &Client,
    obj: &DynamicObject,
    namespaced_objs: &[DynamicObject],
    tail_lines: Option<u64>,
) -> AppResult<String> {
    let kind = obj
        .types
        .as_ref()
        .map(|t| t.kind.as_str())
        .ok_or_else(|| AppError::Internal("Resource missing kind".to_string()))?;

    match kind {
        "Pod" => {
            let pod: Pod = from_dynamic_object(obj)?;
            get_pod_logs(client, &pod, tail_lines).await
        }
        "Job" => {
            // Get pods owned by the job
            let pods = get_owned_pods(obj, namespaced_objs);
            if pods.is_empty() {
                return Ok("No pods found for this job.\n".to_string());
            }

            // Aggregate logs from all pods
            let mut all_logs = Vec::new();
            for pod_obj in pods {
                let pod: Pod = from_dynamic_object(pod_obj)?;
                let pod_name = pod.name_any();
                let logs = get_pod_logs(client, &pod, tail_lines).await?;
                if !logs.is_empty() {
                    all_logs.push(format!("=== Pod: {} ===\n{}", pod_name, logs));
                }
            }

            if all_logs.is_empty() {
                Ok("No logs available yet.\n".to_string())
            } else {
                Ok(all_logs.join("\n\n"))
            }
        }
        "CronJob" => {
            // Find the latest job owned by this cronjob
            let cronjob_uid = obj
                .metadata
                .uid
                .as_ref()
                .ok_or_else(|| AppError::Internal("CronJob missing UID".to_string()))?;

            let jobs: Vec<&DynamicObject> = namespaced_objs
                .iter()
                .filter(|o| {
                    o.types.as_ref().map(|t| t.kind.as_str()) == Some("Job")
                        && o.metadata
                            .owner_references
                            .as_ref()
                            .unwrap_or(&Vec::new())
                            .iter()
                            .any(|or| or.uid == *cronjob_uid)
                })
                .collect();

            if jobs.is_empty() {
                return Ok("No jobs found for this cronjob.\n".to_string());
            }

            // Get the latest job by creation timestamp
            let latest_job = jobs
                .iter()
                .max_by_key(|j| {
                    j.metadata
                        .creation_timestamp
                        .as_ref()
                        .map(|t| t.0.timestamp_millis())
                        .unwrap_or(0)
                })
                .ok_or_else(|| AppError::Internal("Failed to find latest job".to_string()))?;

            // Recursively get logs for the job
            Box::pin(get_resource_logs(
                client,
                latest_job,
                namespaced_objs,
                tail_lines,
            ))
            .await
        }
        "Deployment" | "ReplicaSet" => {
            // Get pods owned by the deployment/replicaset
            let pods = get_owned_pods(obj, namespaced_objs);
            if pods.is_empty() {
                return Ok("No pods found for this resource.\n".to_string());
            }

            // Aggregate logs from all pods
            let mut all_logs = Vec::new();
            for pod_obj in pods {
                let pod: Pod = from_dynamic_object(pod_obj)?;
                let pod_name = pod.name_any();
                let logs = get_pod_logs(client, &pod, tail_lines).await?;
                if !logs.is_empty() {
                    all_logs.push(format!("=== Pod: {} ===\n{}", pod_name, logs));
                }
            }

            if all_logs.is_empty() {
                Ok("No logs available yet.\n".to_string())
            } else {
                Ok(all_logs.join("\n\n"))
            }
        }
        _ => Err(AppError::Internal(format!(
            "Logs not supported for resource kind: {}",
            kind
        ))),
    }
}

/// Get resource info (name, namespace, kind) for display
fn get_resource_info(obj: &DynamicObject) -> (String, String, String) {
    let name = obj.name_any();
    let namespace = obj.namespace().unwrap_or_else(|| "default".to_string());
    let kind = obj
        .types
        .as_ref()
        .map(|t| t.kind.as_str())
        .unwrap_or("Unknown");
    (name, namespace, kind.to_string())
}

#[derive(serde::Deserialize)]
struct LogsQuery {
    namespace: Option<String>,
}

/// Handler for the log page
#[get("/resource-logs/{uid}")]
pub async fn resource_logs_page(
    client: web::Data<Client>,
    _pool: web::Data<Pool<SqliteConnectionManager>>,
    path: web::Path<String>,
    query: web::Query<LogsQuery>,
) -> impl Responder {
    let uid = path.into_inner();

    // Try to find the resource - use namespace from query if provided, otherwise search all
    let (obj, namespace) = if let Some(ref ns) = query.namespace {
        // Fast path: namespace provided in query parameter
        match find_resource_by_uid_in_namespace(&client, &uid, ns).await {
            Ok(Some((obj, _))) => (obj, ns.clone()),
            Ok(None) => {
                return HttpResponse::NotFound()
                    .content_type("text/html; charset=utf-8")
                    .body("Resource not found in specified namespace");
            }
            Err(e) => {
                log::error!("Failed to find resource in namespace {}: {}", ns, e);
                return HttpResponse::InternalServerError()
                    .content_type("text/html; charset=utf-8")
                    .body("Failed to fetch resource");
            }
        }
    } else {
        // Fallback: search all namespaces
        match find_resource_by_uid_all_namespaces(&client, &uid).await {
            Ok(Some((obj, _, ns))) => (obj, ns),
            Ok(None) => {
                return HttpResponse::NotFound()
                    .content_type("text/html; charset=utf-8")
                    .body("Resource not found");
            }
            Err(e) => {
                log::error!("Failed to search for resource: {}", e);
                return HttpResponse::InternalServerError()
                    .content_type("text/html; charset=utf-8")
                    .body("Failed to search for resource");
            }
        }
    };

    let (name, _, kind) = get_resource_info(&obj);

    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { (format!("{} {} Logs", kind, name)) }
                (header::stylesheet_link())
                (header::scripts())
            }
            body.resource-logs-page hx-ext="morph" {
                (header::render(""))
                div.content {
                    header {
                        h1 { (format!("{}: {}", kind, name)) }
                        div.subtitle {
                            "Namespace: " (namespace)
                            " · "
                            a href=(format!("/resource-logs-download/{}?namespace={}", uid, namespace)) { "Download logs" }
                        }
                    }
                    div.resource-logs__container
                        hx-get=(format!("/resource-logs-fragment/{}?namespace={}", uid, namespace))
                        hx-trigger="load, every 2s"
                        hx-swap="morph:innerHTML" {
                        pre.resource-logs__content {
                            "Loading logs..."
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

/// Handler for the log fragment (HTMX polling)
#[get("/resource-logs-fragment/{uid}")]
pub async fn resource_logs_fragment(
    client: web::Data<Client>,
    _pool: web::Data<Pool<SqliteConnectionManager>>,
    path: web::Path<String>,
    query: web::Query<LogsQuery>,
) -> impl Responder {
    let uid = path.into_inner();

    // Try to find the resource - use namespace from query if provided, otherwise search all
    let (obj, namespaced_objs) = if let Some(ref ns) = query.namespace {
        // Fast path: namespace provided in query parameter
        match find_resource_by_uid_in_namespace(&client, &uid, ns).await {
            Ok(Some((obj, objs))) => (obj, objs),
            Ok(None) => {
                return HttpResponse::NotFound()
                    .content_type("text/html; charset=utf-8")
                    .body("Resource not found in specified namespace");
            }
            Err(e) => {
                log::error!("Failed to find resource in namespace {}: {}", ns, e);
                return HttpResponse::InternalServerError()
                    .content_type("text/html; charset=utf-8")
                    .body(format!("Error: {}", format_error_chain(&e)));
            }
        }
    } else {
        // Fallback: search all namespaces
        match find_resource_by_uid_all_namespaces(&client, &uid).await {
            Ok(Some((obj, objs, _))) => (obj, objs),
            Ok(None) => {
                return HttpResponse::NotFound()
                    .content_type("text/html; charset=utf-8")
                    .body("Resource not found");
            }
            Err(e) => {
                log::error!("Failed to search for resource: {}", e);
                return HttpResponse::InternalServerError()
                    .content_type("text/html; charset=utf-8")
                    .body(format!("Error: {}", format_error_chain(&e)));
            }
        }
    };

    // Fetch logs (tail last 1000 lines for performance)
    let logs = match get_resource_logs(&client, &obj, &namespaced_objs, Some(1000)).await {
        Ok(l) => l,
        Err(e) => {
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body(format!("Error fetching logs: {}", format_error_chain(&e)));
        }
    };

    // Split logs into lines and reverse for column-reverse display
    // With column-reverse, the first element appears last visually,
    // so we reverse the lines (newest first) so they appear at the bottom
    let lines: Vec<&str> = logs.lines().collect();
    let reversed_lines: Vec<&str> = lines.iter().rev().copied().collect();

    let markup = html! {
        div.resource-logs__content {
            @for line in reversed_lines.iter() {
                div.resource-logs__line { (line) }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

/// Handler for downloading logs as plaintext
#[get("/resource-logs-download/{uid}")]
pub async fn resource_logs_download(
    client: web::Data<Client>,
    _pool: web::Data<Pool<SqliteConnectionManager>>,
    path: web::Path<String>,
    query: web::Query<LogsQuery>,
) -> impl Responder {
    let uid = path.into_inner();

    // Try to find the resource - use namespace from query if provided, otherwise search all
    let (obj, namespaced_objs) = if let Some(ref ns) = query.namespace {
        // Fast path: namespace provided in query parameter
        match find_resource_by_uid_in_namespace(&client, &uid, ns).await {
            Ok(Some((obj, objs))) => (obj, objs),
            Ok(None) => {
                return HttpResponse::NotFound()
                    .content_type("text/plain")
                    .body("Resource not found in specified namespace");
            }
            Err(e) => {
                log::error!("Failed to find resource in namespace {}: {}", ns, e);
                return HttpResponse::InternalServerError()
                    .content_type("text/plain")
                    .body(format!("Error: {}", format_error_chain(&e)));
            }
        }
    } else {
        // Fallback: search all namespaces
        match find_resource_by_uid_all_namespaces(&client, &uid).await {
            Ok(Some((obj, objs, _))) => (obj, objs),
            Ok(None) => {
                return HttpResponse::NotFound()
                    .content_type("text/plain")
                    .body("Resource not found");
            }
            Err(e) => {
                log::error!("Failed to search for resource: {}", e);
                return HttpResponse::InternalServerError()
                    .content_type("text/plain")
                    .body(format!("Error: {}", format_error_chain(&e)));
            }
        }
    };

    // Fetch logs (no tail limit for download)
    let logs = match get_resource_logs(&client, &obj, &namespaced_objs, None).await {
        Ok(l) => l,
        Err(e) => {
            return HttpResponse::InternalServerError()
                .content_type("text/plain")
                .body(format!("Error fetching logs: {}", format_error_chain(&e)));
        }
    };

    let (name, _, kind) = get_resource_info(&obj);
    let filename = format!("{}-{}.log", kind.to_lowercase(), name);

    HttpResponse::Ok()
        .content_type("text/plain; charset=utf-8")
        .append_header((
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        ))
        .body(logs)
}
