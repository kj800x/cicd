use kube::{Api, Client, ResourceExt};

use crate::crab_ext::Octocrabs;
use crate::kubernetes::api::{
    delete_deploy_config, set_deploy_config_specs, update_deploy_config_status,
};
use crate::kubernetes::DeployConfigStatusBuilder;
use crate::webhooks::config_sync::fetch_deploy_config_by_sha;
use crate::{
    crab_ext::IRepo,
    error::{AppError, AppResult},
    kubernetes::{repo::ShaMaybeBranch, DeployConfig},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployAction {
    Deploy {
        name: String,
        artifact: Option<ShaMaybeBranch>,
        config: ShaMaybeBranch,
    },
    Undeploy {
        name: String,
    },
    ToggleAutodeploy {
        name: String,
    },
}

impl DeployAction {
    // FIXME: BUG: Does not properly handle namespace changes
    pub async fn execute(
        &self,
        client: &Client,
        octocrabs: &Octocrabs,
        repository: impl IRepo,
    ) -> AppResult<()> {
        match self {
            DeployAction::Deploy {
                name,
                artifact,
                config,
            } => {
                log::debug!("Updating to config sha: {}", config.sha);

                let desired_config =
                    fetch_deploy_config_by_sha(octocrabs, repository, &config.sha, name)
                        .await?
                        .ok_or(AppError::NotFound("Desired config not found".to_owned()))?;

                set_deploy_config_specs(
                    client,
                    &desired_config.namespace().unwrap_or_default(),
                    name,
                    desired_config.spec.spec.specs.clone(),
                )
                .await?;

                update_deploy_config_status(
                    client,
                    &desired_config.namespace().unwrap_or_default(),
                    name,
                    DeployConfigStatusBuilder::default()
                        .with_artifact(artifact.clone())
                        .with_config(Some(config.clone())),
                )
                .await?;

                Ok(())
            }

            DeployAction::Undeploy { name } => {
                let current_config = get_deploy_config(client, name)
                    .await?
                    .ok_or(AppError::NotFound("Current config not found".to_owned()))?;

                let namespace = current_config.namespace().unwrap_or_default();
                set_deploy_config_specs(client, &namespace, name, vec![]).await?;

                update_deploy_config_status(
                    client,
                    &namespace,
                    name,
                    DeployConfigStatusBuilder::default()
                        .with_artifact(None)
                        .with_config(None),
                )
                .await?;

                // When undeploying an orphaned config, we should fully delete it.
                if current_config
                    .status
                    .is_some_and(|s| s.orphaned.is_some_and(|x| x))
                {
                    delete_deploy_config(client, &namespace, name).await?;
                }

                Ok(())
            }

            DeployAction::ToggleAutodeploy { name } => {
                let current_config = get_deploy_config(client, name)
                    .await?
                    .ok_or(AppError::NotFound("Current config not found".to_owned()))?;

                let namespace = current_config.namespace().unwrap_or_default();

                let current_autodeploy = current_config
                    .status
                    .and_then(|s| s.autodeploy)
                    .unwrap_or(false);

                update_deploy_config_status(
                    client,
                    &namespace,
                    name,
                    DeployConfigStatusBuilder::default().with_autodeploy(Some(!current_autodeploy)),
                )
                .await?;

                Ok(())
            }
        }
    }
}

// FIXME: Move these to api.rs

pub async fn get_all_deploy_configs(client: &Client) -> AppResult<Vec<DeployConfig>> {
    // Get all DeployConfigs across all namespaces
    let deploy_configs_api: Api<DeployConfig> = Api::all(client.clone());
    let deploy_configs = match deploy_configs_api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            return Err(AppError::Kubernetes(e));
        }
    };

    Ok(deploy_configs)
}

pub async fn get_deploy_config(client: &Client, name: &str) -> AppResult<Option<DeployConfig>> {
    let deploy_configs = get_all_deploy_configs(client).await?;
    let deploy_config = deploy_configs
        .into_iter()
        .find(|config| config.name_any() == name);
    Ok(deploy_config)
}
