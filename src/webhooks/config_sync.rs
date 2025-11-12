use std::collections::HashMap;

use kube::{api::ObjectMeta, Client, ResourceExt};
use octocrab::models::repos::Content;
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
    error::{AppError, AppResult},
    kubernetes::{
        deploy_config::{DeployConfig, DeployConfigSpec, DeployConfigSpecFields},
        repo::RepositoryBranch,
        webhook_handlers::update_deploy_configs_by_defining_repo,
        Repository,
    },
    webhooks::{models::PushEvent, util::extract_branch_name, WebhookHandler},
};

pub struct ConfigSyncHandler {
    pool: Pool<SqliteConnectionManager>,
    client: Client,
    octocrabs: Octocrabs,
}

impl ConfigSyncHandler {
    pub fn new(pool: Pool<SqliteConnectionManager>, client: Client, octocrabs: Octocrabs) -> Self {
        Self {
            pool,
            client,
            octocrabs,
        }
    }
}

/// Sync deploy configs for a repository at a specific commit SHA.
/// This is idempotent - calling multiple times with the same SHA won't create duplicates.
pub async fn sync_deploy_configs_for_commit(
    octocrabs: &Octocrabs,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    repo_owner: &str,
    repo_name: &str,
    repo_id: u64,
    commit_sha: &str,
) -> Result<(), anyhow::Error> {
    let repository = Repository {
        owner: repo_owner.to_string(),
        repo: repo_name.to_string(),
    };

    let deploy_configs =
        fetch_deploy_configs_by_sha(octocrabs, repository.clone(), commit_sha).await?;

    // Get connection after async work is done
    let conn = pool.get()?;

    let existing_deploy_configs = DbDeployConfig::get_by_config_repo_id(repo_id, &conn)?;

    let current_deploy_config_names = deploy_configs
        .iter()
        .map(|dc| dc.name_any())
        .collect::<Vec<String>>();

    for deploy_config in deploy_configs.clone() {
        let artifact_repo_id = match deploy_config.artifact_repository() {
            Some(repository) => {
                let repo = GitRepo::get_by_name(&repository.owner, &repository.repo, &conn)?
                    .ok_or_else(|| AppError::NotFound(format!(
                        "Artifact repository {}/{} not found in database. Use the bootstrap feature to sync this repository first.",
                        repository.owner, repository.repo
                    )))?;
                Some(repo.id)
            }
            None => None,
        };

        let db_config = DbDeployConfig {
            name: deploy_config.name_any(),
            team: deploy_config.team().to_string(),
            kind: deploy_config.kind().to_string(),
            config_repo_id: repo_id,
            artifact_repo_id,
            active: true,
        };

        DbDeployConfig::upsert(&db_config, &conn)?;

        DeployConfigVersion::upsert(
            &DeployConfigVersion {
                name: deploy_config.name_any(),
                config_repo_id: repo_id,
                config_commit_sha: commit_sha.to_string(),
                hash: deploy_config.spec_hash(),
            },
            &conn,
        )?;
    }

    let deleted_deploy_config_names = existing_deploy_configs
        .iter()
        .filter(|dc| !current_deploy_config_names.contains(&dc.name))
        .map(|dc| dc.name.clone())
        .collect::<Vec<String>>();

    for deleted_deploy_config_name in &deleted_deploy_config_names {
        DbDeployConfig::mark_inactive(deleted_deploy_config_name, &conn)?;
    }

    // Drop connection before async work
    drop(conn);

    update_deploy_configs_by_defining_repo(
        client,
        &deploy_configs,
        &deleted_deploy_config_names,
        &repository,
    )
    .await?;

    Ok(())
}

#[async_trait]
impl WebhookHandler for ConfigSyncHandler {
    async fn handle_push(&self, event: PushEvent) -> Result<(), anyhow::Error> {
        log::debug!("Received push event:\n{:#?}", event);

        // Push to default branch, so we need to sync the deploy configs
        if extract_branch_name(&event.r#ref) == Some(event.repository.default_branch.clone()) {
            #[allow(clippy::expect_used)]
            let config_commit_sha = event
                .head_commit
                .as_ref()
                .expect("Head commit should be present")
                .id
                .clone();

            sync_deploy_configs_for_commit(
                &self.octocrabs,
                &self.client,
                &self.pool,
                &event.repository.owner.login,
                &event.repository.name,
                event.repository.id,
                &config_commit_sha,
            )
            .await?;
        }

        Ok(())
    }
}

// MARK: Maybe move everything below this to a separate file?

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
    artifact_repo: Option<GitHubArtifactRepo>,
    team: String,
    kind: String,
    namespace: String,
}

pub async fn fetch_deploy_config_by_sha(
    octocrabs: &Octocrabs,
    repository: impl IRepo,
    sha: &str,
    config_name: &str,
) -> AppResult<Option<DeployConfig>> {
    log::debug!(
        "fetch_deploy_config_by_sha: repo={}/{}, sha={}, config_name={}",
        repository.owner(),
        repository.repo(),
        sha,
        config_name
    );
    let configs = fetch_deploy_configs_by_sha(octocrabs, repository, sha).await?;
    log::debug!("Found {} deploy configs total", configs.len());
    for config in &configs {
        log::debug!(
            "  - config '{}': {} specs",
            config.name_any(),
            config.spec.spec.specs.len()
        );
    }
    let config = configs
        .into_iter()
        .find(|config| config.name_any() == config_name);
    if config.is_some() {
        log::debug!("Found matching config for '{}'", config_name);
    } else {
        log::warn!("No matching config found for '{}'", config_name);
    }
    Ok(config)
}

pub async fn fetch_deploy_configs_by_sha(
    octocrabs: &Octocrabs,
    repository: impl IRepo,
    sha: &str,
) -> AppResult<Vec<DeployConfig>> {
    log::debug!(
        "fetch_deploy_configs_by_sha: repo={}/{}, sha={}",
        repository.owner(),
        repository.repo(),
        sha
    );
    let result = octocrabs.crab_for(&repository).await;

    let Some(crab) = result else {
        log::error!(
            "No octocrab found for repo {}/{}",
            repository.owner(),
            repository.repo()
        );
        return Err(AppError::NotFound(
            "No octocrab found for this repo".to_owned(),
        ));
    };

    let owner = repository.owner().to_string();
    let repo = repository.repo().to_string();

    log::debug!(
        "Fetching .deploy directory from {}/{} at {}",
        owner,
        repo,
        sha
    );
    let content = match crab
        .repos(&owner, &repo)
        .get_content()
        .r#ref(sha)
        .path(".deploy")
        .send()
        .await
    {
        Ok(content) => content,
        Err(e) => {
            log::warn!("Failed to fetch .deploy directory: {:?}", e);
            // FIXME: Should actually confirm that this is a 404 before saying Ok(())
            return Ok(vec![]);
        }
    };

    log::debug!("Found {} items in .deploy directory", content.items.len());

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
        log::debug!("Processing config: {}", config_name);
        let child_files = if entries.contains_key(config_name) {
            log::debug!("Found directory .deploy/{}", config_name);
            let Ok(content_items) = crab
                .repos(&owner, &repo)
                .get_content()
                .r#ref(sha)
                .path(format!(".deploy/{}", config_name))
                .send()
                .await
            else {
                log::error!("Failed to read .deploy/{}", config_name);
                return Err(AppError::NotFound(format!(
                    "Failed to read .deploy/{}",
                    config_name
                )));
            };

            let files = content_items.items;
            log::debug!("Found {} files in .deploy/{}", files.len(), config_name);

            let mut result: Vec<String> = vec![];

            for file in files {
                log::debug!("  Reading file: {}", file.name);
                let Ok(mut content) = crab
                    .repos(&owner, &repo)
                    .get_content()
                    .r#ref(sha)
                    .path(format!(".deploy/{}/{}", config_name, file.name))
                    .send()
                    .await
                else {
                    log::error!("Failed to read .deploy/{}/{}", config_name, file.name);
                    return Err(AppError::NotFound(format!(
                        "Failed to read .deploy/{}/{}",
                        config_name, file.name
                    )));
                };

                let contents = content.take_items();
                let c = &contents[0];
                let Some(decoded_content) = c.decoded_content() else {
                    log::error!("Failed to decode .deploy/{}/{}", config_name, file.name);
                    return Err(AppError::Internal(
                        "Failed to decode child file content".to_owned(),
                    ));
                };
                log::debug!(
                    "  File {} content length: {} bytes",
                    file.name,
                    decoded_content.len()
                );
                if decoded_content.is_empty() {
                    log::warn!("  WARNING: File {} is empty!", file.name);
                }
                result.push(decoded_content);
            }

            result
        } else {
            log::debug!(
                "No directory found for .deploy/{}, using only .yaml file",
                config_name
            );
            vec![]
        };

        let Ok(mut config_content) = crab
            .repos(&owner, &repo)
            .get_content()
            .r#ref(sha)
            .path(format!(".deploy/{}.yaml", config_name))
            .send()
            .await
        else {
            return Err(AppError::NotFound(format!(
                "Failed to read .deploy/{}.yaml",
                config_name
            )));
        };

        let config_content = config_content.take_items();
        let c = &config_content[0];
        let Some(config_content) = c.decoded_content() else {
            return Err(AppError::Internal(
                "Failed to decode config content".to_owned(),
            ));
        };

        let config: GitHubDeployConfig =
            serde_yaml::from_str(&config_content).map_err(AppError::Yaml)?;

        log::debug!(
            "Parsing {} child YAML files for config {}",
            child_files.len(),
            config_name
        );
        let child_files: Vec<Value> = child_files
            .into_iter()
            .enumerate()
            .map(|(idx, file)| {
                log::debug!("  Parsing child file [{}]", idx);
                let parsed: Value = serde_yaml::from_str(&file).map_err(AppError::Yaml)?;
                log::debug!("  Parsed child file [{}]: {}", idx, parsed);
                if parsed.is_null() {
                    log::warn!(
                        "  WARNING: Child file [{}] parsed as null! Content was: {}",
                        idx,
                        file
                    );
                }
                Ok(parsed)
            })
            .collect::<Result<Vec<Value>, AppError>>()?;

        log::debug!(
            "Creating DeployConfig {} with {} specs",
            config_name,
            child_files.len()
        );
        let dc = DeployConfig {
            spec: DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    // FIXME: Is GitHubArtifactRepo and RepositoryBranch the exact same struct?
                    artifact: config.artifact_repo.map(|artifact_repo| RepositoryBranch {
                        owner: artifact_repo.owner.clone(),
                        repo: artifact_repo.repo.clone(),
                        branch: artifact_repo.branch.clone(),
                    }),
                    config: Repository {
                        owner: owner.clone(),
                        repo: repo.clone(),
                    },
                    kind: config.kind,
                    specs: child_files,
                    team: config.team.clone(),
                },
            },
            metadata: ObjectMeta {
                name: Some(config_name.to_owned()),
                namespace: Some(config.namespace),
                ..ObjectMeta::default()
            },
            status: None,
        };

        final_deploy_configs.push(dc);
    }

    Ok(final_deploy_configs)
}
