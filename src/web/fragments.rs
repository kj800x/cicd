#![allow(clippy::expect_used)]

use crate::{
    build_status::BuildStatus,
    db::{git_commit::GitCommit, git_repo::GitRepo},
    kubernetes::{api::get_deploy_config, DeployConfig},
    prelude::*,
    web::{render_preview_content, Action, BuildFilter, HumanTime, ResolvedVersion},
};
use kube::Client;
use maud::{html, Markup};

pub async fn deploy_status(__selected_config: &DeployConfig) -> Vec<Markup> {
    // // Initialize Kubernetes client
    // let client = match Client::try_default().await {
    //     Ok(client) => client,
    //     Err(e) => {
    //         log::error!("Failed to initialize Kubernetes client: {}", e);
    //         panic!("Failed to initialize Kubernetes client");
    //     }
    // };
    // let namespace = selected_config.namespace().unwrap_or_default();
    // let name = selected_config.name_any();

    // let mut alerts = Vec::new();

    // if selected_config.wanted_sha() != selected_config.current_sha() {
    //     let markup = html! {
    //         div.alert.alert-warning {
    //             div class="alert-header" {
    //                 "Deploy change enqueued"
    //             }
    //             div class="alert-content" {
    //                 div class="details" {
    //                     span { "Deploy change request has not been completed by the reconciler yet" }
    //                 }
    //             }
    //         }
    //     };
    //     alerts.push(markup);
    // }

    // // Get the deployment status
    // let deployments_api: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
    // let deployment = match deployments_api.get(&name).await {
    //     Ok(deployment) => deployment,
    //     Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => {
    //         // Deployment doesn't exist yet
    //         return alerts;
    //     }
    //     Err(e) => {
    //         log::error!("Failed to get deployment: {}", e);
    //         panic!("Failed to get deployment status");
    //     }
    // };

    // // Check if deployment is actively rolling out
    // if let Some(status) = &deployment.status {
    //     if let Some(conditions) = &status.conditions {
    //         for condition in conditions {
    //             if condition.type_ == "Progressing"
    //                 && condition.status == "True"
    //                 && condition.reason.as_deref() == Some("NewReplicaSetAvailable")
    //             {
    //                 let transition_time = condition
    //                     .last_transition_time
    //                     .as_ref()
    //                     .map(|t| t.0.timestamp_millis() as u128)
    //                     .unwrap_or(0);

    //                 // only show if transition_time is within the last 5 minutes
    //                 #[allow(clippy::expect_used)]
    //                 if transition_time
    //                     > SystemTime::now()
    //                         .duration_since(UNIX_EPOCH)
    //                         .expect("System time should be after UNIX_EPOCH")
    //                         .as_millis()
    //                         - 300000
    //                 {
    //                     let markup = html! {
    //                         div.alert.alert-success {
    //                             div class="alert-header" {
    //                                 "Recent deployment succeeded"
    //                             }
    //                             div class="alert-content" {
    //                                 div class="details" {
    //                                     span { "Deployment completed successfully at " (HumanTime(transition_time as u64)) }
    //                                 }
    //                             }
    //                         }
    //                     };
    //                     alerts.push(markup);
    //                 }
    //             }

    //             if condition.type_ == "Progressing"
    //                 && condition.status == "True"
    //                 && condition.reason.as_deref() != Some("NewReplicaSetAvailable")
    //             {
    //                 let markup = html! {
    //                     div.alert.alert-warning {
    //                         div class="alert-header" {
    //                             "Deployment in progress"
    //                         }
    //                         div class="alert-content" {
    //                             div class="details" {
    //                                 @if let Some(reason) = &condition.reason {
    //                                     (reason)
    //                                 }
    //                                 @if let Some(message) = &condition.message {
    //                                     ": " (message)
    //                                 }
    //                             }
    //                         }
    //                     }
    //                 };
    //                 alerts.push(markup);
    //             }
    //         }
    //     }
    // }

    // // Check pod statuses
    // let pods_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    // let pods = match pods_api.list(&Default::default()).await {
    //     Ok(pods) => pods.items,
    //     Err(e) => {
    //         log::error!("Failed to list pods: {}", e);
    //         panic!("Failed to get pod status");
    //     }
    // };

    // // Filter pods for this deployment
    // let deployment_pods: Vec<&Pod> = pods
    //     .iter()
    //     .filter(|pod| {
    //         pod.metadata.owner_references.as_ref().is_some_and(|refs| {
    //             refs.iter()
    //                 .any(|ref_| ref_.kind == "ReplicaSet" && ref_.name.starts_with(&name))
    //         })
    //     })
    //     .collect();

    // // Check for pod errors
    // for pod in deployment_pods {
    //     if let Some(status) = &pod.status {
    //         // Check container statuses
    //         if let Some(container_statuses) = &status.container_statuses {
    //             for container_status in container_statuses {
    //                 if let Some(state) = &container_status.state {
    //                     if let Some(terminated) = &state.terminated {
    //                         if terminated.exit_code != 0 {
    //                             let markup = html! {
    //                                 div.alert.alert-danger {
    //                                     div class="alert-header" {
    //                                         "Pod error"
    //                                     }
    //                                     div class="alert-content" {
    //                                         div class="details" {
    //                                             "Container " (container_status.name) " failed with exit code " (terminated.exit_code)
    //                                             @if let Some(reason) = &terminated.reason {
    //                                                 " (" (reason) ")"
    //                                             }
    //                                             @if let Some(message) = &terminated.message {
    //                                                 ": " (message)
    //                                             }
    //                                         }
    //                                     }
    //                                 }
    //                             };
    //                             alerts.push(markup);
    //                         }
    //                     }
    //                     if let Some(waiting) = &state.waiting {
    //                         // Check for various error states
    //                         if let Some(reason) = &waiting.reason {
    //                             if reason == "CrashLoopBackOff"
    //                                 || reason == "ImagePullBackOff"
    //                                 || reason == "ErrImagePull"
    //                                 || reason == "CreateContainerError"
    //                                 || reason == "CreateContainerConfigError"
    //                             {
    //                                 let markup = html! {
    //                                     div.alert.alert-danger {
    //                                         div class="alert-header" {
    //                                             "Pod error"
    //                                         }
    //                                         div class="alert-content" {
    //                                             div class="details" {
    //                                                 "Container " (container_status.name) " is in " (reason)
    //                                                 @if let Some(message) = &waiting.message {
    //                                                     ": " (message)
    //                                                 }
    //                                             }
    //                                         }
    //                                     }
    //                                 };
    //                                 alerts.push(markup);
    //                             }
    //                         }
    //                     }
    //                 }
    //             }
    //         }
    //     }
    // }

    // alerts

    vec![]
}

pub async fn build_status(
    action: &Action,
    selected_config: &DeployConfig,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Vec<Markup> {
    let artifact_repository = selected_config
        .artifact_repository()
        .expect("Failed to get artifact repository");
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
                  @if let Some(build_url) = git_commit_build.map(|x| x.url) {
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
        }],
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

    let action = Action::from_query(&action_params);
    let markup = render_preview_content(&selected_config, &action, &conn).await;

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}
