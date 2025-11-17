use crate::{
    build_status::BuildStatus,
    db::{
        git_branch::GitBranch,
        git_repo::GitRepo,
    },
    error::AppResult,
    kubernetes::{
        api::{get_all_deploy_configs, list_namespace_objects, ListMode},
        DeployConfig,
    },
    prelude::*,
    web::{
        resource_status::{from_dynamic_object, HandledResourceKind, is_error_reason, is_warn_reason},
        team_prefs::{ReposCookie, TeamsCookie},
    },
};
use actix_web::{web, HttpResponse, Responder};
use k8s_openapi::api::{
    apps::v1::{Deployment, ReplicaSet as KReplicaSet},
    batch::v1::{CronJob as KCronJob, Job as KJob},
    core::v1::Pod,
};
use kube::{api::DynamicObject, Client, ResourceExt};
use maud::{html, DOCTYPE};
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;

#[derive(Clone, Debug, PartialEq)]
enum HealthStatus {
    Healthy,
    Warning,
    Error,
    Unknown,
    Info, // For informational statuses like "currently executing"
}

struct RepoHealth {
    repo: GitRepo,
    status: HealthStatus,
    message: Option<String>,
}

struct DeployConfigHealth {
    config: DeployConfig,
    status: HealthStatus,
    namespace: String,
    message: Option<String>,
}

/// Check if a repo's latest successful master build passed
fn check_repo_health(
    repo: &GitRepo,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> AppResult<(HealthStatus, Option<String>, Option<String>)> {
    // Get the master/default branch
    let branch = match GitBranch::get_by_name(&repo.default_branch, repo.id, conn)? {
        Some(b) => b,
        None => return Ok((HealthStatus::Unknown, None, Some("Branch not found".to_string()))),
    };

    // Get the latest successful build for this branch
    let latest_successful = branch.latest_successful_build(conn)?;

    match latest_successful {
        Some(commit) => {
            // Check if this commit's build status is Success
            let build_status = commit.get_build_status(conn)?;
            match build_status {
                Some(build) => {
                    let status: BuildStatus = build.clone().into();
                    match status {
                        BuildStatus::Success => Ok((HealthStatus::Healthy, Some(commit.sha), None)),
                        BuildStatus::Failure => Ok((HealthStatus::Error, Some(commit.sha), Some("Build failed".to_string()))),
                        BuildStatus::Pending => Ok((HealthStatus::Warning, Some(commit.sha), Some("Build pending".to_string()))),
                        BuildStatus::None => Ok((HealthStatus::Unknown, Some(commit.sha), Some("No build status".to_string()))),
                    }
                }
                None => Ok((HealthStatus::Unknown, Some(commit.sha), Some("No build status".to_string()))),
            }
        }
        None => Ok((HealthStatus::Unknown, None, Some("No successful build found".to_string()))),
    }
}

/// Check if a deploy config's resources are healthy
async fn check_deploy_config_health(
    config: &DeployConfig,
    client: &Client,
) -> AppResult<(HealthStatus, Option<String>)> {
    let namespace = config.namespace().unwrap_or_else(|| "default".to_string());

    // Get all resources in the namespace
    let namespaced_objs = match list_namespace_objects(client, &namespace, ListMode::All).await {
        Ok(objs) => objs,
        Err(e) => {
            log::warn!("Failed to list namespace objects for {}: {}", namespace, e);
            return Ok((HealthStatus::Unknown, Some(format!("Failed to list resources: {}", e))));
        }
    };

    // Build UID index for ownership checks
    let uid_index: std::collections::HashMap<String, &DynamicObject> = namespaced_objs
        .iter()
        .filter_map(|o| o.metadata.uid.as_ref().map(|uid| (uid.clone(), o)))
        .collect();

    // Helper to check if a resource is owned by the config
    fn is_owned_by_config(
        obj: &DynamicObject,
        config: &DeployConfig,
        uid_index: &std::collections::HashMap<String, &DynamicObject>,
    ) -> bool {
        // Direct owner check
        if config.owns(obj) {
            return true;
        }
        let Some(owners) = &obj.metadata.owner_references else {
            return false;
        };
        let Some(config_uid) = config.uid() else {
            return false;
        };
        for or in owners {
            if or.uid == config_uid {
                return true;
            }
            if let Some(parent) = uid_index.get(&or.uid) {
                if is_owned_by_config(parent, config, uid_index) {
                    return true;
                }
            }
        }
        false
    }

    // Get the resources that belong to this deploy config
    let resource_specs = config.resource_specs();
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut info_messages: Vec<String> = Vec::new();

    // First, check pods owned by the config (these have the most detailed error info)
    for obj in namespaced_objs.iter().filter(|o| {
        o.types
            .as_ref()
            .map(|t| t.kind.as_str() == "Pod")
            .unwrap_or(false)
            && is_owned_by_config(o, config, &uid_index)
    }) {
                let (health, msg) = check_resource_health_with_message(obj, &namespaced_objs);
                match health {
                    HealthStatus::Error => {
                        if let Some(m) = msg {
                            errors.push(m);
                        } else {
                            errors.push(format!("Pod {}: Error", obj.name_any()));
                        }
                    }
                    HealthStatus::Warning => {
                        if let Some(m) = msg {
                            warnings.push(m);
                        } else {
                            warnings.push(format!("Pod {}: Warning", obj.name_any()));
                        }
                    }
                    HealthStatus::Info => {
                        // Info statuses are informational
                        if let Some(m) = msg {
                            info_messages.push(m);
                        } else {
                            info_messages.push(format!("Pod {}: Info", obj.name_any()));
                        }
                    }
                    HealthStatus::Unknown => {
                        warnings.push(format!("Pod {}: Unknown status", obj.name_any()));
                    }
                    HealthStatus::Healthy => {}
                }
    }

    // Then check direct resources from specs (but skip if we already have pod errors)
    if errors.is_empty() {
        for spec in resource_specs {
            // Try to find the resource in the namespace
            let resource_name = spec
                .get("metadata")
                .and_then(|m| m.get("name"))
                .and_then(|n| n.as_str());
            let resource_kind = spec.get("kind").and_then(|k| k.as_str());

            if let (Some(name), Some(kind)) = (resource_name, resource_kind) {
                // Skip pods - we already checked them above
                if kind == "Pod" {
                    continue;
                }

                // Find the resource in namespaced_objs
                if let Some(obj) = namespaced_objs.iter().find(|o| {
                    o.name_any() == name
                        && o.types
                            .as_ref()
                            .map(|t| t.kind.as_str())
                            .unwrap_or("")
                            == kind
                }) {
                    // Check the health status of this resource
                    let (health, msg) = check_resource_health_with_message(obj, &namespaced_objs);
                    match health {
                        HealthStatus::Error => {
                            if let Some(m) = msg {
                                errors.push(format!("{} {}: {}", kind, name, m));
                            } else {
                                errors.push(format!("{} {}: Error", kind, name));
                            }
                        }
                        HealthStatus::Warning => {
                            if let Some(m) = msg {
                                warnings.push(format!("{} {}: {}", kind, name, m));
                            } else {
                                warnings.push(format!("{} {}: Warning", kind, name));
                            }
                        }
                        HealthStatus::Info => {
                            // Info statuses are informational (like "currently executing")
                            if let Some(m) = msg {
                                info_messages.push(format!("{} {}: {}", kind, name, m));
                            } else {
                                info_messages.push(format!("{} {}: Info", kind, name));
                            }
                        }
                        HealthStatus::Unknown => {
                            warnings.push(format!("{} {}: Unknown status", kind, name));
                        }
                        HealthStatus::Healthy => {}
                    }
                }
            }
        }
    }

    if !errors.is_empty() {
        Ok((HealthStatus::Error, Some(errors.join("; "))))
    } else if !warnings.is_empty() {
        Ok((HealthStatus::Warning, Some(warnings.join("; "))))
    } else if !info_messages.is_empty() {
        Ok((HealthStatus::Info, Some(info_messages.join("; "))))
    } else {
        Ok((HealthStatus::Healthy, None))
    }
}

/// Check the health status of a single resource with error message
fn check_resource_health_with_message(obj: &DynamicObject, namespaced_objs: &[DynamicObject]) -> (HealthStatus, Option<String>) {
    let kind_str = obj
        .types
        .as_ref()
        .map(|t| t.kind.as_str())
        .unwrap_or("");

    let kind: HandledResourceKind = match kind_str.parse() {
        Ok(k) => k,
        Err(_) => return (HealthStatus::Unknown, None),
    };

    // Use the same logic as resource_status.rs but return health status and message
    match kind {
        HandledResourceKind::Deployment => {
            match from_dynamic_object::<Deployment>(obj) {
                Ok(deployment) => check_deployment_health_with_message(&deployment),
                Err(_) => (HealthStatus::Unknown, None),
            }
        }
        HandledResourceKind::Pod => match from_dynamic_object::<Pod>(obj) {
            Ok(pod) => check_pod_health_with_message(&pod),
            Err(_) => (HealthStatus::Unknown, None),
        },
        HandledResourceKind::ReplicaSet => match from_dynamic_object::<KReplicaSet>(obj) {
            Ok(rs) => check_replicaset_health_with_message(&rs),
            Err(_) => (HealthStatus::Unknown, None),
        },
        HandledResourceKind::Job => match from_dynamic_object::<KJob>(obj) {
            Ok(job) => check_job_health_with_message(&job),
            Err(_) => (HealthStatus::Unknown, None),
        },
        HandledResourceKind::CronJob => match from_dynamic_object::<KCronJob>(obj) {
            Ok(cronjob) => check_cronjob_health_with_message(&cronjob, namespaced_objs),
            Err(_) => (HealthStatus::Unknown, None),
        },
        // Services and Ingresses are generally healthy if they exist
        HandledResourceKind::Service | HandledResourceKind::Ingress => (HealthStatus::Healthy, None),
        HandledResourceKind::Other(_) => (HealthStatus::Unknown, None),
    }
}

fn check_deployment_health_with_message(deployment: &Deployment) -> (HealthStatus, Option<String>) {
    let spec_replicas = deployment
        .spec
        .as_ref()
        .and_then(|s| s.replicas)
        .unwrap_or(0);
    let status = match deployment.status.as_ref() {
        Some(s) => s,
        None => return (HealthStatus::Unknown, Some("No status".to_string())),
    };

    let ready = status.ready_replicas.unwrap_or(0);
    let updated = status.updated_replicas.unwrap_or(0);
    let unavailable = status.unavailable_replicas.unwrap_or(0);
    let observed_generation = status.observed_generation.unwrap_or_default();
    let desired_generation = deployment.metadata.generation.unwrap_or_default();

    // Condition-based errors take precedence
    if let Some(conditions) = &status.conditions {
        for c in conditions {
            if c.type_ == "Progressing"
                && (c.reason.as_deref() == Some("ProgressDeadlineExceeded") || c.status == "False")
            {
                let reason = c.reason.as_deref().unwrap_or("Not progressing");
                return (HealthStatus::Error, Some(format!("{} ({} ready / {})", reason, ready, spec_replicas)));
            }
            if c.type_ == "ReplicaFailure" && c.status == "True" {
                let reason = c.reason.as_deref().unwrap_or("Replica failure");
                return (HealthStatus::Error, Some(format!("{} ({} ready / {})", reason, ready, spec_replicas)));
            }
        }
    }

    // Reconciling or updating
    if observed_generation < desired_generation {
        return (HealthStatus::Warning, Some(format!("Reconciling ({} ready / {})", ready, spec_replicas)));
    }
    if updated < spec_replicas {
        return (HealthStatus::Warning, Some(format!("Updating {} / {}", updated, spec_replicas)));
    }
    if ready < spec_replicas {
        return (HealthStatus::Warning, Some(format!("Waiting {} / {}", ready, spec_replicas)));
    }
    if unavailable > 0 {
        return (HealthStatus::Warning, Some(format!("Unavailable {}", unavailable)));
    }

    (HealthStatus::Healthy, None)
}

fn check_pod_health_with_message(pod: &Pod) -> (HealthStatus, Option<String>) {
    let pod_name = pod.name_any();

    if pod.metadata.deletion_timestamp.is_some() {
        return (HealthStatus::Warning, Some("Terminating".to_string()));
    }

    let status = match pod.status.as_ref() {
        Some(s) => s,
        None => return (HealthStatus::Unknown, Some("No status".to_string())),
    };

    // Completed pods are healthy
    if status.phase.as_deref() == Some("Succeeded") || status.reason.as_deref() == Some("Completed") {
        return (HealthStatus::Healthy, None);
    }

    if let Some(r) = &status.reason {
        if r == "Evicted" {
            return (HealthStatus::Error, Some("Evicted".to_string()));
        }
    }

    // Check container statuses
    if let Some(containers) = &status.container_statuses {
        for cs in containers {
            if let Some(state) = cs.state.as_ref() {
                if let Some(waiting) = state.waiting.as_ref() {
                    let reason = waiting.reason.as_deref().unwrap_or("Waiting");
                    if is_error_reason(reason) {
                        let msg = waiting.message.as_ref()
                            .map(|m| format!("{}: {}", reason, m))
                            .unwrap_or_else(|| reason.to_string());
                        return (HealthStatus::Error, Some(format!("{} / {}", pod_name, msg)));
                    }
                    if is_warn_reason(reason) {
                        return (HealthStatus::Warning, Some(format!("{} / {}: {}", pod_name, cs.name, reason)));
                    }
                }
                if let Some(terminated) = state.terminated.as_ref() {
                    if terminated.exit_code != 0 {
                        let reason_str = terminated
                            .reason
                            .as_deref()
                            .filter(|r| !r.is_empty())
                            .map(|r| r.to_string())
                            .unwrap_or_else(|| format!("ExitCode {}", terminated.exit_code));
                        let msg = terminated.message.as_ref()
                            .map(|m| format!("{}: {}", reason_str, m))
                            .unwrap_or_else(|| reason_str.clone());
                        return (HealthStatus::Error, Some(format!("{} / {}: {}", pod_name, cs.name, msg)));
                    }
                }
            }
            if !cs.ready {
                return (HealthStatus::Warning, Some(format!("{} / {}: NotReady", pod_name, cs.name)));
            }
        }
    }

    // Check phase
    match status.phase.as_deref() {
        Some("Running") | Some("Succeeded") => (HealthStatus::Healthy, None),
        Some("Failed") => (HealthStatus::Error, Some("Failed".to_string())),
        Some("Pending") | Some("Unknown") => (HealthStatus::Warning, Some(format!("Phase: {}", status.phase.as_deref().unwrap_or("Unknown")))),
        _ => (HealthStatus::Warning, Some("Unknown phase".to_string())),
    }
}

fn check_replicaset_health_with_message(rs: &KReplicaSet) -> (HealthStatus, Option<String>) {
    let desired = rs.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let status = rs.status.as_ref();
    let ready = status.and_then(|s| s.ready_replicas).unwrap_or(0);

    if desired == 0 {
        return (HealthStatus::Healthy, None); // Scaled to zero is healthy
    }

    if ready < desired {
        (HealthStatus::Warning, Some(format!("{} / {} ready", ready, desired)))
    } else {
        (HealthStatus::Healthy, None)
    }
}

fn check_job_health_with_message(job: &KJob) -> (HealthStatus, Option<String>) {
    let job_name = job.name_any();
    let status = match job.status.as_ref() {
        Some(s) => s,
        None => return (HealthStatus::Unknown, Some("No status".to_string())),
    };

    // Check conditions
    if let Some(conditions) = &status.conditions {
        for condition in conditions {
            if condition.type_ == "Failed" && condition.status == "True" {
                let reason = condition.reason.as_deref().unwrap_or("Failed");
                let message = condition.message.as_deref().unwrap_or("");
                if !message.is_empty() {
                    return (HealthStatus::Error, Some(format!("{}: {} - {}", job_name, reason, message)));
                } else {
                    return (HealthStatus::Error, Some(format!("{}: {}", job_name, reason)));
                }
            }
            if condition.type_ == "Complete" && condition.status == "True" {
                // Job completed successfully
                return (HealthStatus::Healthy, None);
            }
        }
    }

    // Check failed count
    if let Some(failed) = status.failed {
        if failed > 0 {
            return (HealthStatus::Error, Some(format!("{}: {} failed pods", job_name, failed)));
        }
    }

    // Check if job is still active
    if status.active.is_some_and(|a| a > 0) {
        // Job is still running - this is fine, pods will be checked separately
        return (HealthStatus::Healthy, None);
    }

    // If no conditions and no active pods, job might be pending or unknown
    if status.succeeded.is_some_and(|s| s > 0) {
        (HealthStatus::Healthy, None)
    } else {
        (HealthStatus::Warning, Some(format!("{}: Unknown state", job_name)))
    }
}

fn check_cronjob_health_with_message(cronjob: &KCronJob, namespaced_objs: &[DynamicObject]) -> (HealthStatus, Option<String>) {
    let cronjob_name = cronjob.name_any();
    let cronjob_uid = match cronjob.metadata.uid.as_ref() {
        Some(uid) => uid,
        None => return (HealthStatus::Unknown, Some("No UID".to_string())),
    };

    // Find active jobs owned by this cronjob
    let active_jobs: Vec<&DynamicObject> = namespaced_objs
        .iter()
        .filter(|o| {
            if o.types.as_ref().map(|t| t.kind.as_str()) != Some("Job") {
                return false;
            }
            if !o.metadata
                .owner_references
                .as_ref()
                .unwrap_or(&Vec::new())
                .iter()
                .any(|or| or.uid == *cronjob_uid)
            {
                return false;
            }
            // Check if job is active
            if let Ok(job) = from_dynamic_object::<KJob>(o) {
                if let Some(status) = job.status.as_ref() {
                    if let Some(active) = status.active {
                        return active > 0;
                    }
                }
            }
            false
        })
        .collect();

    if !active_jobs.is_empty() {
        return (HealthStatus::Info, Some(format!("CronJob {}: Currently executing", cronjob_name)));
    }

    // If there are no active jobs and no errors, CronJob is healthy
    (HealthStatus::Healthy, None)
}

/// Get all repos filtered by user's repo cookie
fn get_filtered_repos(
    conn: &PooledConnection<SqliteConnectionManager>,
    repos_cookie: &ReposCookie,
) -> AppResult<Vec<GitRepo>> {
    let all_repos = GitRepo::get_all(conn)?;
    let filtered: Vec<GitRepo> = all_repos
        .into_iter()
        .filter(|repo| repos_cookie.is_visible(&repo.owner_name))
        .collect();
    Ok(filtered)
}

/// Get all deploy configs filtered by user's team cookie
async fn get_filtered_deploy_configs(
    client: &Client,
    teams_cookie: &TeamsCookie,
) -> AppResult<Vec<DeployConfig>> {
    let all_configs = get_all_deploy_configs(client).await?;
    let filtered = teams_cookie.filter_configs(&all_configs);
    Ok(filtered)
}

#[get("/watchdog")]
pub async fn watchdog_page(
    _req: actix_web::HttpRequest,
    _pool: web::Data<Pool<SqliteConnectionManager>>,
    _client: web::Data<Client>,
) -> impl Responder {

    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                title { "Watchdog" }
                (crate::web::header::stylesheet_link())
                script src="/res/htmx.min.js" {}
                script src="/res/idiomorph.min.js" {}
                script src="/res/idiomorph-ext.min.js" {}
            }
            body.watchdog-page hx-ext="morph" {
                (crate::web::header::render("watchdog"))
                div.content {
                    div.watchdog-content
                        hx-get="/watchdog-fragment"
                        hx-trigger="load, every 10s"
                        hx-swap="morph:innerHTML" {
                        // Initial content will be loaded via HTMX
                        div { "Loading..." }
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

#[get("/watchdog-fragment")]
pub async fn watchdog_fragment(
    req: actix_web::HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    client: web::Data<Client>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to connect to database");
        }
    };

    let repos_cookie = ReposCookie::from_request(&req);
    let teams_cookie = TeamsCookie::from_request(&req);

    // Get filtered repos and deploy configs
    let repos = match get_filtered_repos(&conn, &repos_cookie) {
        Ok(r) => r,
        Err(e) => {
            log::error!("Failed to get repos: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to get repos");
        }
    };

    let deploy_configs = match get_filtered_deploy_configs(&client, &teams_cookie).await {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get deploy configs: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to get deploy configs");
        }
    };

    // Check repo health
    let mut repo_healths: Vec<RepoHealth> = Vec::new();
    for repo in repos {
        match check_repo_health(&repo, &conn) {
            Ok((status, _sha, message)) => {
                repo_healths.push(RepoHealth {
                    repo,
                    status,
                    message,
                });
            }
            Err(e) => {
                log::warn!("Failed to check health for repo {}/{}: {}", repo.owner_name, repo.name, e);
                repo_healths.push(RepoHealth {
                    repo,
                    status: HealthStatus::Unknown,
                    message: Some(format!("Error checking health: {}", e)),
                });
            }
        }
    }

    // Check deploy config health
    let mut deploy_config_healths: Vec<DeployConfigHealth> = Vec::new();
    for config in deploy_configs {
        let namespace = config.namespace().unwrap_or_else(|| "default".to_string());
        match check_deploy_config_health(&config, &client).await {
            Ok((status, message)) => {
                deploy_config_healths.push(DeployConfigHealth {
                    config,
                    status,
                    namespace,
                    message,
                });
            }
            Err(e) => {
                log::warn!("Failed to check health for deploy config {}: {}", config.name_any(), e);
                deploy_config_healths.push(DeployConfigHealth {
                    config,
                    status: HealthStatus::Unknown,
                    namespace,
                    message: Some(format!("Error checking health: {}", e)),
                });
            }
        }
    }

    // Sort by status (errors first, then warnings, then info, then healthy)
    repo_healths.sort_by(|a, b| {
        let priority = |s: &HealthStatus| match s {
            HealthStatus::Error => 0,
            HealthStatus::Warning => 1,
            HealthStatus::Info => 2,
            HealthStatus::Unknown => 3,
            HealthStatus::Healthy => 4,
        };
        priority(&a.status).cmp(&priority(&b.status))
    });

    deploy_config_healths.sort_by(|a, b| {
        let priority = |s: &HealthStatus| match s {
            HealthStatus::Error => 0,
            HealthStatus::Warning => 1,
            HealthStatus::Info => 2,
            HealthStatus::Unknown => 3,
            HealthStatus::Healthy => 4,
        };
        priority(&a.status).cmp(&priority(&b.status))
    });

    // Separate healthy from unhealthy (Info counts as "unhealthy" for display purposes - it needs an alert)
    let (healthy_repos, unhealthy_repos): (Vec<_>, Vec<_>) = repo_healths.into_iter().partition(|r| r.status == HealthStatus::Healthy);
    let (healthy_configs, unhealthy_configs): (Vec<_>, Vec<_>) = deploy_config_healths.into_iter().partition(|c| c.status == HealthStatus::Healthy);

    let markup = html! {
        div.watchdog-content {
            @if !unhealthy_repos.is_empty() || !unhealthy_configs.is_empty() {
                div.watchdog-alerts {
                    @for repo_health in &unhealthy_repos {
                        @match repo_health.status {
                            HealthStatus::Error => {
                                div.alert.alert-danger {
                                    div class="alert-header" {
                                        a href=(format!("https://github.com/{}/{}", repo_health.repo.owner_name, repo_health.repo.name)) target="_blank" {
                                            (repo_health.repo.owner_name) "/" (repo_health.repo.name)
                                        }
                                    }
                                    @if let Some(msg) = &repo_health.message {
                                        div class="alert-content" {
                                            div class="details" { (msg) }
                                        }
                                    }
                                }
                            }
                            HealthStatus::Warning => {
                                div.alert.alert-warning {
                                    div class="alert-header" {
                                        a href=(format!("https://github.com/{}/{}", repo_health.repo.owner_name, repo_health.repo.name)) target="_blank" {
                                            (repo_health.repo.owner_name) "/" (repo_health.repo.name)
                                        }
                                    }
                                    @if let Some(msg) = &repo_health.message {
                                        div class="alert-content" {
                                            div class="details" { (msg) }
                                        }
                                    }
                                }
                            }
                            HealthStatus::Info => {
                                div.alert.alert-success {
                                    div class="alert-header" {
                                        a href=(format!("https://github.com/{}/{}", repo_health.repo.owner_name, repo_health.repo.name)) target="_blank" {
                                            (repo_health.repo.owner_name) "/" (repo_health.repo.name)
                                        }
                                    }
                                    @if let Some(msg) = &repo_health.message {
                                        div class="alert-content" {
                                            div class="details" { (msg) }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    @for config_health in &unhealthy_configs {
                        @match config_health.status {
                            HealthStatus::Error => {
                                div.alert.alert-danger {
                                    div class="alert-header" {
                                        a href=(format!("/deploy?selected={}", config_health.config.name_any())) {
                                            (config_health.config.name_any())
                                        }
                                        " (" (config_health.namespace) ")"
                                    }
                                    @if let Some(msg) = &config_health.message {
                                        div class="alert-content" {
                                            div class="details" { (msg) }
                                        }
                                    }
                                }
                            }
                            HealthStatus::Warning => {
                                div.alert.alert-warning {
                                    div class="alert-header" {
                                        a href=(format!("/deploy?selected={}", config_health.config.name_any())) {
                                            (config_health.config.name_any())
                                        }
                                        " (" (config_health.namespace) ")"
                                    }
                                    @if let Some(msg) = &config_health.message {
                                        div class="alert-content" {
                                            div class="details" { (msg) }
                                        }
                                    }
                                }
                            }
                            HealthStatus::Info => {
                                div.alert.alert-success {
                                    div class="alert-header" {
                                        a href=(format!("/deploy?selected={}", config_health.config.name_any())) {
                                            (config_health.config.name_any())
                                        }
                                        " (" (config_health.namespace) ")"
                                    }
                                    @if let Some(msg) = &config_health.message {
                                        div class="alert-content" {
                                            div class="details" { (msg) }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            @if !healthy_repos.is_empty() || !healthy_configs.is_empty() {
                div.watchdog-healthy {
                    @if !healthy_repos.is_empty() {
                        div.watchdog-healthy-section {
                            @for repo_health in &healthy_repos {
                                span.watchdog-pill {
                                    a href=(format!("https://github.com/{}/{}", repo_health.repo.owner_name, repo_health.repo.name)) target="_blank" {
                                        (repo_health.repo.owner_name) "/" (repo_health.repo.name)
                                    }
                                }
                            }
                        }
                    }
                    @if !healthy_configs.is_empty() {
                        div.watchdog-healthy-section {
                            @for config_health in &healthy_configs {
                                span.watchdog-pill {
                                    a href=(format!("/deploy?selected={}", config_health.config.name_any())) {
                                        (config_health.config.name_any())
                                    }
                                }
                            }
                        }
                    }
                }
            }

            @if healthy_repos.is_empty() && healthy_configs.is_empty() && unhealthy_repos.is_empty() && unhealthy_configs.is_empty() {
                p { "No repositories or deploy configs found. Configure visibility in Settings." }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

