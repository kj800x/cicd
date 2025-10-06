use std::collections::HashMap;

use kube::{api::ObjectMeta, Client};
use octocrab::models::repos::Content;
use serde_json::Value;

use crate::{
    crab_ext::{OctocrabExt, Octocrabs},
    kubernetes::{
        controller::update_deploy_configs_by_defining_repo,
        deployconfig::{DefiningRepo, DeployConfigSpec, DeployConfigSpecFields},
        Repository,
    },
    prelude::*,
};

fn default_branch() -> String {
    "master".to_string()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GitHubArtifactRepo {
    owner: String,
    repo: String,
    #[serde(default = "default_branch")]
    branch: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GitHubDeployConfig {
    #[serde(rename = "artifactRepo")]
    artifact_repo: GitHubArtifactRepo,
    team: String,
    kind: String,
    namespace: String,
    #[serde(default)]
    autodeploy: bool,
}

pub async fn manual_hello() -> impl Responder {
    HttpResponse::Ok().body("Hey there!")
}

pub async fn sync_deploy_configs() -> impl Responder {
    HttpResponse::Ok().body("Syncing deploy configs...")
}

pub async fn sync_repo_deploy_configs_impl(
    octocrabs: &Octocrabs,
    client: &Client,
    owner: String,
    repo: String,
) -> Result<(), anyhow::Error> {
    let result = octocrabs
        .crab_for(&DefiningRepo {
            owner: owner.clone(),
            repo: repo.clone(),
        })
        .await;

    let Some(crab) = result else {
        return Err(anyhow::anyhow!("No octocrab found for this repo"));
    };

    let content = match crab
        .repos(&owner, &repo)
        .get_content()
        .path(".deploy")
        .send()
        .await
    {
        Ok(content) => content,
        Err(__e) => {
            // FIXME: Should actually confirm that this is a 404 before saying Ok(())
            return Ok(());
            // if e.source().is_some_and(|e| e.to_string().contains("404")) {
            // } else {
            //     return Err(anyhow::anyhow!("Failed to read .deploy: {}", e));
            // }
        }
    };

    // create a map from name to Content
    let entries: HashMap<String, Content> = content
        .items
        .into_iter()
        .map(|item| (item.name.clone(), item))
        .collect();
    let configs: Vec<&str> = entries
        .keys()
        .filter(|name| name.ends_with(".yaml"))
        .map(|name| name.strip_suffix(".yaml").unwrap())
        .collect();

    let mut final_deploy_configs: Vec<DeployConfig> = vec![];

    for config_name in configs {
        let child_files = if entries.contains_key(config_name) {
            let Ok(content_items) = crab
                .repos(&owner, &repo)
                .get_content()
                .path(format!(".deploy/{}", config_name))
                .send()
                .await
            else {
                return Err(anyhow::anyhow!("Failed to read .deploy/{}", config_name));
            };

            let files = content_items.items;

            let mut result: Vec<String> = vec![];

            for file in files {
                let Ok(mut content) = crab
                    .repos(&owner, &repo)
                    .get_content()
                    .path(format!(".deploy/{}/{}", config_name, file.name))
                    .send()
                    .await
                else {
                    return Err(anyhow::anyhow!(
                        "Failed to read .deploy/{}/{}",
                        config_name,
                        file.name
                    ));
                };

                let contents = content.take_items();
                let c = &contents[0];
                let Some(decoded_content) = c.decoded_content() else {
                    return Err(anyhow::anyhow!("Failed to decode child file content"));
                };
                result.push(decoded_content);
            }

            result
        } else {
            vec![]
        };

        let Ok(mut config_content) = crab
            .repos(&owner, &repo)
            .get_content()
            .path(format!(".deploy/{}.yaml", config_name))
            .send()
            .await
        else {
            return Err(anyhow::anyhow!(
                "Failed to read .deploy/{}.yaml",
                config_name
            ));
        };

        let config_content = config_content.take_items();
        let c = &config_content[0];
        let Some(config_content) = c.decoded_content() else {
            return Err(anyhow::anyhow!("Failed to decode config content"));
        };

        let config: GitHubDeployConfig = serde_yaml::from_str(&config_content)
            .map_err(|e| anyhow::anyhow!("Failed to parse config as GitHubDeployConfig: {}", e))?;

        let child_files: Vec<Value> = child_files
            .into_iter()
            .map(|file| {
                serde_yaml::from_str(&file)
                    .map_err(|e| anyhow::anyhow!("Failed to parse child file as yaml: {}", e))
            })
            .collect::<Result<Vec<Value>, anyhow::Error>>()?;

        let dc = DeployConfig {
            spec: DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    repo: Repository {
                        owner: config.artifact_repo.owner,
                        repo: config.artifact_repo.repo,
                        default_branch: config.artifact_repo.branch,
                    },
                    autodeploy: config.autodeploy,
                    kind: config.kind,
                    specs: child_files,
                    team: config.team.clone(),
                },
            },
            metadata: ObjectMeta {
                name: Some(format!("{}-{}", config.team.clone(), config_name)),
                namespace: Some(config.namespace),
                ..ObjectMeta::default()
            },
            status: Some(DeployConfigStatus {
                defining_repo: Some(DefiningRepo {
                    owner: owner.clone(),
                    repo: repo.clone(),
                }),
                ..DeployConfigStatus::default()
            }),
        };

        final_deploy_configs.push(dc);
    }

    update_deploy_configs_by_defining_repo(
        &client,
        &final_deploy_configs,
        &DefiningRepo {
            owner: owner.clone(),
            repo: repo.clone(),
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to update deploy configs: {}", e))?;

    Ok(())
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
        let username = octocrab.current().user().await.unwrap().login;
        let first_page = octocrab
            .current()
            .list_repos_for_authenticated_user()
            .visibility("all")
            .affiliation("owner")
            .send()
            .await
            .unwrap();
        let user_repos = octocrab.all_pages(first_page).await.unwrap();
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

    // todo!();
    HttpResponse::Ok().body("Synced all deploy configs")
}
