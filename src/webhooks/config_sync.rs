use std::collections::HashMap;

use kube::{api::ObjectMeta, Client, ResourceExt};
use octocrab::{
    models::repos::{Content, Object},
    params::repos::Reference,
};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serenity::async_trait;

use crate::{
    crab_ext::{IRepo, OctocrabExt, Octocrabs},
    db::{
        deploy_config::DeployConfig as DbDeployConfig, deploy_config_version::DeployConfigVersion,
        git_repo::GitRepo,
    },
    kubernetes::{
        deploy_config::{
            DeployConfig, DeployConfigConfigStatus, DeployConfigSpec, DeployConfigSpecFields,
            DeployConfigStatus,
        },
        repo::RepositoryBranch,
    },
    webhooks::{
        models::{PushEvent, Repository},
        util::extract_branch_name,
        WebhookHandler,
    },
};

pub struct ConfigSyncHandler {
    pool: Pool<SqliteConnectionManager>,
    // client: Client,
    octocrabs: Octocrabs,
}

impl ConfigSyncHandler {
    pub fn new(pool: Pool<SqliteConnectionManager>, client: Client, octocrabs: Octocrabs) -> Self {
        Self {
            pool,
            // client,
            octocrabs,
        }
    }
}

#[async_trait]
impl WebhookHandler for ConfigSyncHandler {
    async fn handle_push(&self, event: PushEvent) -> Result<(), anyhow::Error> {
        log::info!("Received push event:\n{:#?}", event);

        // Push to default branch, so we need to sync the deploy configs
        if extract_branch_name(&event.r#ref) == Some(event.repository.default_branch.clone()) {
            let conn = self.pool.get()?;

            let deploy_configs =
                fetch_current_deploy_configs(&self.octocrabs, event.repository.clone()).await?;

            let existing_deploy_configs =
                DbDeployConfig::get_by_config_repo_id(event.repository.id, &conn)?;

            let current_deploy_config_names = deploy_configs
                .iter()
                .map(|dc| dc.name_any())
                .collect::<Vec<String>>();

            for deploy_config in deploy_configs {
                // FIXME: Cases - artifact_repo defined and present in db, artifact_repo defined and not present in db, artifact_repo is null in the deploy config spec (not yet part of the shape properly)
                // Right now we're treating "can't find artifact repo" as "no artifact repo", which is not correct.
                let artifact_repo_id = GitRepo::get_by_name(
                    deploy_config.artifact_owner(),
                    deploy_config.artifact_repo(),
                    &conn,
                )?;

                let db_config = DbDeployConfig {
                    name: deploy_config.name_any(),
                    team: deploy_config.team().to_string(),
                    kind: deploy_config.kind().to_string(),
                    config_repo_id: event.repository.id,
                    artifact_repo_id: artifact_repo_id.map(|r| r.id),
                    active: true,
                };

                DbDeployConfig::upsert(&db_config, &conn)?;

                #[allow(clippy::expect_used)]
                let config_commit_sha = event
                    .head_commit
                    .as_ref()
                    .expect("Head commit should be present")
                    .id
                    .clone();

                DeployConfigVersion::upsert(
                    &DeployConfigVersion {
                        name: deploy_config.name_any(),
                        config_repo_id: event.repository.id,
                        config_commit_sha,
                        hash: deploy_config.spec_hash(),
                    },
                    &conn,
                )?;
            }

            for existing_deploy_config in existing_deploy_configs {
                if !current_deploy_config_names.contains(&existing_deploy_config.name) {
                    DbDeployConfig::mark_inactive(&existing_deploy_config.name, &conn)?;
                }
            }

            // FIXME: Also sync with kubernetes

            // sync_repo_deploy_configs_impl(&self.octocrabs, &self.client, event.repository.clone())
            //     .await?;
        }

        Ok(())
    }
}

// MARK: Maybe move below this to a separate file?

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

pub async fn fetch_current_deploy_configs(
    octocrabs: &Octocrabs,
    repository: Repository,
) -> Result<Vec<DeployConfig>, anyhow::Error> {
    let result = octocrabs.crab_for(&repository).await;

    let Some(crab) = result else {
        return Err(anyhow::anyhow!("No octocrab found for this repo"));
    };

    let owner = repository.owner().to_string();
    let repo = repository.repo().to_string();
    let default_branch = match crab
        .repos(&owner, &repo)
        .get()
        .await
        .map(|r| r.default_branch)
    {
        Ok(Some(default_branch)) => default_branch,
        Ok(None) => return Ok(vec![]),
        Err(_) => return Ok(vec![]),
    };

    let sha = match crab
        .repos(&owner, &repo)
        .get_ref(&Reference::Branch(default_branch.to_string()))
        .await
    {
        Ok(r) => match r.object {
            Object::Commit { sha, .. } => sha,
            Object::Tag { .. } => return Ok(vec![]),
            _ => return Ok(vec![]),
        },
        Err(_) => return Ok(vec![]),
    };

    let content = match crab
        .repos(&owner, &repo)
        .get_content()
        .r#ref(&sha)
        .path(".deploy")
        .send()
        .await
    {
        Ok(content) => content,
        Err(__e) => {
            // FIXME: Should actually confirm that this is a 404 before saying Ok(())
            return Ok(vec![]);
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
        .filter_map(|name| name.strip_suffix(".yaml"))
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

        let mut dc = DeployConfig {
            spec: DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    artifact: RepositoryBranch {
                        owner: config.artifact_repo.owner,
                        repo: config.artifact_repo.repo,
                        branch: config.artifact_repo.branch,
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
            status: None,
        };

        dc.status = Some(DeployConfigStatus {
            config: Some(DeployConfigConfigStatus {
                owner: Some(owner.clone()),
                repo: Some(repo.clone()),
                // TODO: Semantically this is correct today (since we overwrite the
                // live config spec as soon as we get the webhook). In the future,
                // we will want to defer the update until the user actually does
                // a deploy.
                sha: Some(dc.spec_hash()),
            }),
            ..Default::default()
        });

        final_deploy_configs.push(dc);
    }

    Ok(final_deploy_configs)

    // update_deploy_configs_by_defining_repo(
    //     client,
    //     &final_deploy_configs,
    //     &Repository {
    //         owner: owner.clone(),
    //         repo: repo.clone(),
    //     },
    // )
    // .await
    // .map_err(|e| anyhow::anyhow!("Failed to update deploy configs: {}", e))?;

    // Ok(())
}
