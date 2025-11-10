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
    let markup = render_preview_content(&selected_config, &action, &conn, &client).await;

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}
