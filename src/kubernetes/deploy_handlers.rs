use kube::{Client, ResourceExt};

use crate::crab_ext::Octocrabs;
use crate::kubernetes::api::{
    delete_deploy_config, get_deploy_config, set_deploy_config_specs, update_deploy_config_status,
};
use crate::kubernetes::DeployConfigStatusBuilder;
use crate::webhooks::config_sync::fetch_deploy_config_by_sha;
use crate::{
    crab_ext::IRepo,
    error::{AppError, AppResult},
    kubernetes::repo::ShaMaybeBranch,
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
    // KNOWN LIMITATION: Changing a DeployConfig's namespace is not supported.
    // The deploy operation applies resources to the namespace in the current config,
    // not the new namespace specified in the updated .deploy/*.yaml file.
    //
    // To change a config's namespace:
    // 1. Undeploy the config from its current namespace
    // 2. Update the .deploy/*.yaml file with the new namespace
    // 3. Push to master to sync the config
    // 4. Deploy to the new namespace
    //
    // This limitation exists because the DeployAction executor uses the existing
    // config's namespace, not the desired config's namespace.
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
