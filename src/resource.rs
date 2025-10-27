use std::collections::HashMap;

use kube::{api::ObjectMeta, Client};
use octocrab::{
    models::repos::{Content, Object},
    params::repos::Reference,
};
use serde_json::Value;

use crate::{
    crab_ext::{OctocrabExt, Octocrabs},
    kubernetes::{
        deploy_config::{DeployConfigConfigStatus, DeployConfigSpec, DeployConfigSpecFields},
        repo::{Repository, RepositoryBranch},
        webhook_handlers::update_deploy_configs_by_defining_repo,
    },
    prelude::*,
};

#[get("/api/hey")]
pub async fn manual_hello() -> impl Responder {
    HttpResponse::Ok().body("Hey there!")
}

pub async fn sync_deploy_configs() -> impl Responder {
    HttpResponse::Ok().body("Syncing deploy configs...")
}

#[get("/api/sync-repo/{owner}/{repo}")]
pub async fn sync_repo_deploy_configs(
    path: web::Path<(String, String)>,
    client: web::Data<Client>,
    octocrabs: web::Data<Octocrabs>,
) -> impl Responder {
    let (owner, repo) = path.into_inner();
    log::info!("Request to sync deploy configs for {}/{}...", owner, repo);

    match sync_repo_deploy_configs_impl(
        octocrabs.get_ref(),
        client.get_ref(),
        owner.clone(),
        repo.clone(),
    )
    .await
    {
        Ok(_) => {
            log::info!("Synced deploy configs for {}/{}", owner, repo);
            HttpResponse::Ok().body(format!("Synced deploy configs for {}/{}", owner, repo))
        }
        Err(e) => {
            log::error!(
                "Failed to sync deploy configs for {}/{}: {}",
                owner,
                repo,
                e
            );
            HttpResponse::InternalServerError().body(format!(
                "Failed to sync deploy configs for {}/{}: {}",
                owner, repo, e
            ))
        }
    }
}

#[get("/api/sync-all")]
pub async fn sync_all_deploy_configs(
    client: web::Data<Client>,
    octocrabs: web::Data<Octocrabs>,
) -> impl Responder {
    log::info!("Request to sync all deploy configs...");

    for octocrab in octocrabs.get_ref().clone() {
        let username = match octocrab.current().user().await {
            Ok(user) => user.login,
            Err(e) => {
                log::error!("Failed to get current user: {}", e);
                continue;
            }
        };

        let first_page = match octocrab
            .current()
            .list_repos_for_authenticated_user()
            .visibility("all")
            .affiliation("owner")
            .send()
            .await
        {
            Ok(page) => page,
            Err(e) => {
                log::error!("Failed to list repos for user {}: {}", username, e);
                continue;
            }
        };

        let user_repos = match octocrab.all_pages(first_page).await {
            Ok(repos) => repos,
            Err(e) => {
                log::error!("Failed to get all pages for user {}: {}", username, e);
                continue;
            }
        };

        let short_crabs = vec![octocrab];

        for repo in user_repos {
            log::info!("Syncing deploy configs for {}/{}...", username, repo.name);
            if let Err(e) = sync_repo_deploy_configs_impl(
                &short_crabs,
                client.get_ref(),
                username.clone(),
                repo.name.clone(),
            )
            .await
            {
                log::error!(
                    "Failed to sync deploy configs for {}/{}: {}",
                    username,
                    repo.name,
                    e
                );
            }
        }
    }

    HttpResponse::Ok().body("Synced all deploy configs")
}
