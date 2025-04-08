use crate::prelude::*;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::{Api, Client, ResourceExt};
use maud::{html, Markup};

/// HTMX endpoint for fetching deploy status
pub async fn deploy_status(path: web::Path<(String, String)>) -> impl Responder {
    let (namespace, name) = path.into_inner();

    // Initialize Kubernetes client
    let client = match Client::try_default().await {
        Ok(client) => client,
        Err(e) => {
            log::error!("Failed to initialize Kubernetes client: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to connect to Kubernetes".to_string());
        }
    };
    let deploy_configs_api: Api<DeployConfig> = Api::all(client.clone());
    let deploy_configs = match deploy_configs_api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            log::error!("Failed to list DeployConfigs: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to list DeployConfigs".to_string());
        }
    };

    let selected_config = deploy_configs
        .into_iter()
        .find(|config| {
            config.namespace().unwrap_or_default() == namespace && config.name_any() == name
        })
        .unwrap();

    // Get the deployment status
    let deployments_api: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
    let deployment = match deployments_api.get(&name).await {
        Ok(deployment) => deployment,
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => {
            // Deployment doesn't exist yet
            return HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body("");
        }
        Err(e) => {
            log::error!("Failed to get deployment: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to get deployment status".to_string());
        }
    };

    let mut alerts = Vec::new();

    // Check if deployment is actively rolling out
    if let Some(status) = &deployment.status {
        if let Some(conditions) = &status.conditions {
            for condition in conditions {
                if condition.type_ == "Progressing"
                    && condition.status == "True"
                    && condition.reason.as_deref() != Some("NewReplicaSetAvailable")
                {
                    let markup = html! {
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
                    };
                    alerts.push(markup);
                }
            }
        }
    }

    // Check pod statuses
    let pods_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let pods = match pods_api.list(&Default::default()).await {
        Ok(pods) => pods.items,
        Err(e) => {
            log::error!("Failed to list pods: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to get pod status".to_string());
        }
    };

    // Filter pods for this deployment
    let deployment_pods: Vec<&Pod> = pods
        .iter()
        .filter(|pod| {
            pod.metadata
                .owner_references
                .as_ref()
                .map_or(false, |refs| {
                    refs.iter()
                        .any(|ref_| ref_.kind == "ReplicaSet" && ref_.name.starts_with(&name))
                })
        })
        .collect();

    // Check for pod errors
    for pod in deployment_pods {
        if let Some(status) = &pod.status {
            // Check container statuses
            if let Some(container_statuses) = &status.container_statuses {
                for container_status in container_statuses {
                    if let Some(state) = &container_status.state {
                        if let Some(terminated) = &state.terminated {
                            if terminated.exit_code != 0 {
                                let markup = html! {
                                    div.alert.alert-danger {
                                        div class="alert-header" {
                                            "Pod error"
                                        }
                                        div class="alert-content" {
                                            div class="details" {
                                                "Container " (container_status.name) " failed with exit code " (terminated.exit_code)
                                                @if let Some(reason) = &terminated.reason {
                                                    " (" (reason) ")"
                                                }
                                                @if let Some(message) = &terminated.message {
                                                    ": " (message)
                                                }
                                            }
                                        }
                                    }
                                };
                                alerts.push(markup);
                            }
                        }
                        if let Some(waiting) = &state.waiting {
                            // Check for various error states
                            if let Some(reason) = &waiting.reason {
                                if reason == "CrashLoopBackOff"
                                    || reason == "ImagePullBackOff"
                                    || reason == "ErrImagePull"
                                    || reason == "CreateContainerError"
                                    || reason == "CreateContainerConfigError"
                                {
                                    let markup = html! {
                                        div.alert.alert-danger {
                                            div class="alert-header" {
                                                "Pod error"
                                            }
                                            div class="alert-content" {
                                                div class="details" {
                                                    "Container " (container_status.name) " is in " (reason)
                                                    @if let Some(message) = &waiting.message {
                                                        ": " (message)
                                                    }
                                                }
                                            }
                                        }
                                    };
                                    alerts.push(markup);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Return all alerts or empty response
    if !alerts.is_empty() {
        let markup = html! {
            @for alert in alerts {
                (alert)
            }
        };
        HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(markup.into_string())
    } else {
        HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body("")
    }
}

/// HTMX endpoint for fetching build status
pub async fn build_status(
    path: web::Path<(String, String)>,
    query: web::Query<std::collections::HashMap<String, String>>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    let action_params = query.into_inner();
    let conn = pool.get().unwrap();

    // Initialize Kubernetes client
    let client = match Client::try_default().await {
        Ok(client) => client,
        Err(e) => {
            log::error!("Failed to initialize Kubernetes client: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to connect to Kubernetes".to_string());
        }
    };
    let deploy_configs_api: Api<DeployConfig> = Api::all(client.clone());
    let deploy_configs = match deploy_configs_api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            log::error!("Failed to list DeployConfigs: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to list DeployConfigs".to_string());
        }
    };
    let selected_config = deploy_configs
        .into_iter()
        .find(|config| {
            config.namespace().unwrap_or_default() == namespace && config.name_any() == name
        })
        .unwrap();

    let action = Action::from_query(&action_params);

    let resolved_version =
        ResolvedVersion::from_action(&action, &selected_config, &conn, BuildFilter::Any);

    if let ResolvedVersion::UnknownSha { .. } = resolved_version {
        let markup = html! {
            div.alert.alert-danger {
              div class="alert-header" {
                "Build status of unknown sha cannot be determined"
              }
            }
        };

        return HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(markup.into_string());
    }

    let commit = match &resolved_version {
        ResolvedVersion::UnknownSha { .. } => {
            panic!("unreachable")
        }
        ResolvedVersion::TrackedSha { sha, .. } => get_commit_by_sha(&sha, &conn),
        ResolvedVersion::BranchTracked { sha, .. } => get_commit_by_sha(&sha, &conn),
        ResolvedVersion::Undeployed => None,
        ResolvedVersion::ResolutionFailed => None,
    };

    let commit = match commit {
        Some(commit) => commit,
        None => {
            return HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body("")
        }
    };

    let markup: Option<Markup> = match commit.build_status {
        BuildStatus::Success => None,
        BuildStatus::Pending | BuildStatus::None | BuildStatus::Failure => Some(html! {
          div.alert.alert-danger[matches!(commit.build_status, BuildStatus::Failure)].alert-warning[matches!(commit.build_status, BuildStatus::None | BuildStatus::Pending)] {
            div class="alert-header" {
              @match commit.build_status {
                BuildStatus::Pending => "New builds detected",
                BuildStatus::Failure => "New builds failed",
                BuildStatus::None => "New builds pending",
                BuildStatus::Success => "",
              }
            }
            div class="alert-content" {
              div class="details" {
                div {
                  (resolved_version.format(None, &selected_config.spec.spec.repo.owner, &selected_config.spec.spec.repo.repo))
                  @match commit.build_status {
                    BuildStatus::Pending => " is actively being built.",
                    BuildStatus::Failure => " failed to build.",
                    BuildStatus::None => " is pending build.",
                    BuildStatus::Success => "",
                  }
                  @if let Some(build_url) = commit.build_url {
                    " "
                    a href=(build_url) { "Build log" }
                    "."
                  }
                }
                commit.timestamp {
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
        }),
    };

    match markup {
        Some(markup) => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(markup.into_string()),
        None => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(""),
    }
}
