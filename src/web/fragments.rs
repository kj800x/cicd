#![allow(clippy::expect_used)]

use crate::{
    build_status::BuildStatus,
    db::{git_commit::GitCommit, git_commit_build::GitCommitBuild, git_repo::GitRepo},
    kubernetes::{
        api::{get_deploy_config, ListMode},
        list_namespace_objects, DeployConfig,
    },
    prelude::*,
    web::{formatting, render_preview_content, Action, BuildFilter, HumanTime, ResolvedVersion},
};
use k8s_openapi::api::{apps::v1::Deployment, core::v1::Pod};
use kube::{api::DynamicObject, Client, ResourceExt};
use maud::{html, Markup};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn deploy_status(
    selected_config: &DeployConfig,
    namespaced_objs: &[DynamicObject],
) -> Vec<Markup> {
    // Helpers
    fn from_dynamic_object<T: DeserializeOwned>(obj: &DynamicObject) -> Option<T> {
        let value: Value = serde_json::to_value(obj).ok()?;
        serde_json::from_value(value).ok()
    }
    fn is_container_error_reason(reason: &str) -> bool {
        matches!(
            reason,
            "CrashLoopBackOff"
                | "ErrImagePull"
                | "ImagePullBackOff"
                | "CreateContainerConfigError"
                | "CreateContainerError"
                | "RunContainerError"
                | "InvalidImageName"
                | "ContainerCannotRun"
        )
    }
    fn build_uid_index(objs: &[DynamicObject]) -> HashMap<String, &DynamicObject> {
        let mut idx = HashMap::new();
        for o in objs {
            if let Some(uid) = &o.metadata.uid {
                idx.insert(uid.clone(), o);
            }
        }
        idx
    }
    fn is_owned_by_config(
        obj: &DynamicObject,
        config: &DeployConfig,
        uid_index: &HashMap<String, &DynamicObject>,
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

    let mut alerts: Vec<Markup> = Vec::new();
    let uid_index = build_uid_index(namespaced_objs);

    // Deployments: progressing or recently succeeded
    for obj in namespaced_objs.iter().filter(|o| {
        o.types
            .as_ref()
            .map(|t| t.kind.as_str() == "Deployment")
            .unwrap_or(false)
            && is_owned_by_config(o, selected_config, &uid_index)
    }) {
        if let Some(dep) = from_dynamic_object::<Deployment>(obj) {
            if let Some(status) = &dep.status {
                if let Some(conditions) = &status.conditions {
                    for condition in conditions {
                        if condition.type_ == "Progressing"
                            && condition.status == "True"
                            && condition.reason.as_deref() == Some("NewReplicaSetAvailable")
                        {
                            let transition_time = condition
                                .last_transition_time
                                .as_ref()
                                .map(|t| t.0.timestamp_millis() as u128)
                                .unwrap_or(0);
                            if transition_time
                                > SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .expect("System time should be after UNIX_EPOCH")
                                    .as_millis()
                                    - 300000
                            {
                                alerts.push(html! {
                                    div.alert.alert-success {
                                        div class="alert-header" {
                                            "Recent deployment succeeded"
                                        }
                                        div class="alert-content" {
                                            div class="details" {
                                                span { "Deployment completed successfully at " (HumanTime(transition_time as u64)) }
                                            }
                                        }
                                    }
                                });
                            }
                        }

                        if condition.type_ == "Progressing"
                            && condition.status == "True"
                            && condition.reason.as_deref() != Some("NewReplicaSetAvailable")
                        {
                            alerts.push(html! {
                                div.alert.alert-warning {
                                    div class="alert-header" {
                                        "Deployment in progress"
                                    }
                                    div class="alert-content" {
                                        div class="details" {
                                            @if let Some(reason) = &condition.reason {
                                                (reason)
                                            }
                                            @if let Some(message) = &condition.message {
                                                ": " (message)
                                            }
                                        }
                                    }
                                }
                            });
                        }
                    }
                }
            }
        }
    }

    // Pods: critical failures and scheduling issues
    for obj in namespaced_objs.iter().filter(|o| {
        o.types
            .as_ref()
            .map(|t| t.kind.as_str() == "Pod")
            .unwrap_or(false)
            && is_owned_by_config(o, selected_config, &uid_index)
    }) {
        if let Some(pod) = from_dynamic_object::<Pod>(obj) {
            let pod_name = pod.name_any();
            if let Some(status) = &pod.status {
                // Unschedulable
                if let Some(conditions) = &status.conditions {
                    if let Some(cond) = conditions
                        .iter()
                        .find(|c| c.type_ == "PodScheduled" && c.status == "False")
                    {
                        let reason = cond
                            .reason
                            .clone()
                            .unwrap_or_else(|| "Unschedulable".to_string());
                        let message = cond.message.clone().unwrap_or_default();
                        alerts.push(html! {
                            div.alert.alert-warning {
                                div class="alert-header" {
                                    "Pod unschedulable"
                                }
                                div class="alert-content" {
                                    div class="details" {
                                        (pod_name) ": " (reason)
                                        @if !message.is_empty() {
                                            ": " (message)
                                        }
                                    }
                                }
                            }
                        });
                    }
                }

                // Container state errors
                let mut push_container_error =
                    |container_name: &str, reason: &str, message: Option<&String>| {
                        alerts.push(html! {
                            div.alert.alert-danger {
                                div class="alert-header" {
                                    "Pod error"
                                }
                                div class="alert-content" {
                                    div class="details" {
                                        (pod_name.clone()) " / " (container_name) ": " (reason)
                                        @if let Some(msg) = message {
                                            ": " (msg)
                                        }
                                    }
                                }
                            }
                        });
                    };

                let check_container =
                    |cs: &k8s_openapi::api::core::v1::ContainerStatus,
                     push: &mut dyn FnMut(&str, &str, Option<&String>)| {
                        if let Some(state) = &cs.state {
                            if let Some(waiting) = &state.waiting {
                                if let Some(r) = &waiting.reason {
                                    if is_container_error_reason(r) {
                                        push(&cs.name, r, waiting.message.as_ref());
                                    }
                                }
                            }
                            if let Some(terminated) = &state.terminated {
                                if terminated.exit_code != 0 {
                                    let reason = terminated.reason.clone().unwrap_or_else(|| {
                                        format!("ExitCode {}", terminated.exit_code)
                                    });
                                    push(&cs.name, &reason, terminated.message.as_ref());
                                }
                            }
                        }
                        if let Some(last) = &cs.last_state {
                            if let Some(terminated) = &last.terminated {
                                if let Some(r) = &terminated.reason {
                                    if r == "OOMKilled" {
                                        push(&cs.name, r, terminated.message.as_ref());
                                    }
                                }
                            }
                        }
                    };

                if let Some(inits) = &status.init_container_statuses {
                    for cs in inits {
                        check_container(cs, &mut push_container_error);
                    }
                }
                if let Some(containers) = &status.container_statuses {
                    for cs in containers {
                        check_container(cs, &mut push_container_error);
                    }
                }
            }
        }
    }

    alerts
}

pub async fn build_status(
    action: &Action,
    selected_config: &DeployConfig,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Vec<Markup> {
    let Some(artifact_repository) = selected_config.artifact_repository() else {
        return vec![];
    };
    let owner = artifact_repository.owner;
    let repo = artifact_repository.repo;
    let repo = GitRepo::get_by_name(&owner, &repo, conn).ok().flatten();
    let Some(repo) = repo else {
        return vec![];
    };

    let resolved_version =
        ResolvedVersion::from_action(action, selected_config, conn, BuildFilter::Any);

    let commit = match &resolved_version {
        ResolvedVersion::UnknownSha { .. } => {
            let markup = html! {
                div.alert.alert-danger {
                  div class="alert-header" {
                    "Build status of unknown sha cannot be determined"
                  }
                }
            };

            return vec![markup];
        }
        ResolvedVersion::TrackedSha { sha, .. } => {
            GitCommit::get_by_sha(sha, repo.id, conn).ok().flatten()
        }
        ResolvedVersion::BranchTracked { sha, .. } => {
            GitCommit::get_by_sha(sha, repo.id, conn).ok().flatten()
        }
        ResolvedVersion::Undeployed => None,
        ResolvedVersion::ResolutionFailed => None,
    };

    let Some(commit) = commit else {
        return vec![];
    };

    let git_commit_build = commit.get_build_status(conn).ok().flatten();
    let build_status: BuildStatus = git_commit_build.clone().into();

    let build_url = git_commit_build.as_ref().map(|x| x.url.clone());
    let build_start_time = git_commit_build.as_ref().and_then(|x| x.start_time);

    // For pending builds, compute elapsed time, avg duration, percent complete, and remaining.
    struct PendingProgress {
        elapsed_ms: u64,
        pct: Option<u64>,        // 0-100, None if no historical data
        remaining_ms: Option<u64>,
    }
    let pending_progress: Option<PendingProgress> = if matches!(build_status, BuildStatus::Pending) {
        let now_ms = Utc::now().timestamp_millis() as u64;
        build_start_time.map(|start| {
            let elapsed_ms = now_ms.saturating_sub(start);
            let avg_duration_ms = GitCommitBuild::avg_build_duration_ms(repo.id, 10, conn)
                .ok()
                .flatten();
            let pct = avg_duration_ms
                .filter(|&avg| avg > 0)
                .map(|avg| ((elapsed_ms * 100) / avg).min(100));
            let remaining_ms = avg_duration_ms.map(|avg| avg.saturating_sub(elapsed_ms));
            PendingProgress { elapsed_ms, pct, remaining_ms }
        })
    } else {
        None
    };

    match build_status {
        BuildStatus::Success => vec![],
        BuildStatus::Pending | BuildStatus::None | BuildStatus::Failure => vec![html! {
          div.alert.alert-danger[matches!(build_status, BuildStatus::Failure)].alert-warning[matches!(build_status, BuildStatus::None | BuildStatus::Pending)] {
            div class="alert-header" {
              @match build_status {
                BuildStatus::Pending => "New builds detected",
                BuildStatus::Failure => "New builds failed",
                BuildStatus::None => "New builds pending",
                BuildStatus::Success => "",
              }
            }
            div class="alert-content" {
              div class="details" {
                div {
                  (resolved_version.format(None, &owner, &repo.name))
                  @match build_status {
                    BuildStatus::Pending => " is actively being built.",
                    BuildStatus::Failure => " failed to build.",
                    BuildStatus::None => " is pending build.",
                    BuildStatus::Success => "",
                  }
                  @if let Some(ref url) = build_url {
                    " "
                    a href=(url) { "Build log" }
                    "."
                  }
                }
                @if let Some(ref p) = pending_progress {
                  div {
                    "Running for " (formatting::format_duration_ms(p.elapsed_ms)) "."
                  }
                  @if let Some(pct) = p.pct {
                    div class="build-progress" {
                      div class="build-progress__bar" {
                        div class="build-progress__fill" style=(format!("width: {}%", pct)) {}
                      }
                      span class="build-progress__label" {
                        (pct) "%"
                        @if let Some(remaining_ms) = p.remaining_ms {
                          " — "
                          (format_remaining(remaining_ms))
                          " estimated remaining"
                        }
                      }
                    }
                  }
                }
                div {
                  "Committed at "
                  (HumanTime(commit.timestamp as u64))
                  "."
                }
              }
              pre.commit-message {
                (commit.message)
              }
            }
          }
        }],
    }
}

fn format_remaining(ms: u64) -> String {
    let total_secs = ms / 1000;
    let minutes = total_secs / 60;
    let secs = total_secs % 60;
    match (minutes, secs) {
        (0, s) => format!("{} second{}", s, if s == 1 { "" } else { "s" }),
        (m, s) => format!(
            "{} minute{} {} second{}",
            m, if m == 1 { "" } else { "s" },
            s, if s == 1 { "" } else { "s" }
        ),
    }
}

#[get("/fragments/deploy-preview/{namespace}/{name}")]
pub async fn deploy_preview(
    path: web::Path<(String, String)>,
    query: web::Query<std::collections::HashMap<String, String>>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    // Initialize Kubernetes client
    // FIXME: Should this come from web::Data?
    let client = match Client::try_default().await {
        Ok(client) => client,
        Err(e) => {
            log::error!("Failed to initialize Kubernetes client: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to connect to Kubernetes".to_string());
        }
    };

    let (namespace, name) = path.into_inner();
    let action_params = query.into_inner();

    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Failed to connect to database");
        }
    };

    // FIXME: expect_used for now
    #[allow(clippy::expect_used)]
    let selected_config = match get_deploy_config(&client, &name)
        .await
        .expect("Failed to get deploy config")
    {
        Some(config) => config,
        None => {
            log::error!("Deploy config not found: {}/{}", namespace, name);
            return HttpResponse::NotFound()
                .body(format!("Deploy config not found: {}/{}", namespace, name));
        }
    };

    let Ok(namespaced_objs) = list_namespace_objects(&client, &namespace, ListMode::All).await
    else {
        return HttpResponse::InternalServerError().body("Failed to get namespaced objects");
    };

    let action = Action::from_query(&action_params);
    let markup = render_preview_content(&selected_config, &action, &conn, &namespaced_objs).await;

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}
