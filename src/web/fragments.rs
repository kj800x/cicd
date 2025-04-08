use crate::prelude::*;
use kube::{Api, Client, ResourceExt};
use maud::{html, Markup};

/// HTMX endpoint for fetching deploy status
pub async fn deploy_status(__path: web::Path<(String, String)>) -> impl Responder {
    // let (namespace, name) = path.into_inner();

    // let markup = html! {
    //     div class="alert alert-info" {
    //         "Deploy status for {namespace}/{name} (stub)"
    //     }
    // };

    HttpResponse::NoContent().body("")
    // .content_type("text/html; charset=utf-8")
    // .body(markup.into_string())
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
