use kube::{Api, Client, ResourceExt};

use crate::{
    error::{AppError, AppResult},
    kubernetes::{repo::ShaMaybeBranch, DeployConfig},
};

pub enum DeployAction {
    Deploy {
        name: String,
        artifact: Option<ShaMaybeBranch>,
        config: ShaMaybeBranch,
    },
    Undeploy {
        name: String,
    },
}

pub async fn execute_deploy_action(action: DeployAction) -> AppResult<()> {
    match action {
        DeployAction::Deploy {
            name,
            artifact,
            config,
        } => {
            todo!()
        }
        DeployAction::Undeploy { name } => {
            todo!()
        }
    }
}

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

pub async fn get_deploy_config(
    client: &Client,
    namespace: &str,
    name: &str,
) -> AppResult<Option<DeployConfig>> {
    let deploy_configs = get_all_deploy_configs(&client).await?;
    let deploy_config = deploy_configs.into_iter().find(|config| {
        config.namespace().unwrap_or_default() == namespace && config.name_any() == name
    });
    Ok(deploy_config)
}
