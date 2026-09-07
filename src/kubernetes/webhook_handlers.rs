use super::DeployConfig;
use super::Repository;
use crate::error::format_error_chain;
use crate::kubernetes::repo::DeploymentState;
use crate::kubernetes::{cr_writers, ensure_namespace_exists, Error};
use crate::prelude::*;
use itertools::Itertools;
use kube::api::DeleteParams;
use kube::{
    api::{Api, ResourceExt},
    client::Client,
};

/// Write the declared part of a config (parameters, config repo, kind,
/// team) under the config-sync field manager and mark it not orphaned.
/// Creates the object when it does not exist. Selections, patches and the
/// manifest templates belong to other managers and are never touched here.
///
/// Namespace changes are not supported: the object would be created anew
/// in the other namespace while the old one kept running.
async fn sync_deploy_config(
    client: &Client,
    existing_config: Option<&DeployConfig>,
    final_config: &DeployConfig,
) -> Result<(), Error> {
    let ns = final_config
        .namespace()
        .unwrap_or_else(|| "default".to_string());
    let name = final_config.name_any();

    if let Some(existing) = existing_config {
        let old_ns = existing
            .namespace()
            .unwrap_or_else(|| "default".to_string());
        if old_ns != ns {
            return Err(Error::App(AppError::Internal(
                "Namespace change not supported. You must undeploy, delete the config, and recreate it with the new namespace.".to_owned(),
            )));
        }
    }

    let template_namespace = std::env::var("TEMPLATE_NAMESPACE").ok();
    ensure_namespace_exists(client, &ns, template_namespace.as_deref())
        .await
        .map_err(Error::App)?;

    cr_writers::apply_config_sync(client, &ns, &name, &final_config.spec.spec)
        .await
        .map_err(Error::App)?;
    cr_writers::set_orphaned(client, &ns, &name, false)
        .await
        .map_err(Error::App)?;

    log::info!(
        "{} DeployConfig {}/{}",
        if existing_config.is_some() {
            "Updated"
        } else {
            "Created"
        },
        ns,
        name
    );
    Ok(())
}

async fn delete_deploy_config(
    client: &Client,
    existing_config: &DeployConfig,
) -> Result<(), Error> {
    let ns = existing_config
        .namespace()
        .unwrap_or_else(|| "default".to_string());
    let name = existing_config.name_any();
    let api: Api<DeployConfig> = Api::namespaced(client.clone(), &ns);

    if existing_config.deployment_state() == DeploymentState::Undeployed {
        api.delete(&name, &DeleteParams::default()).await?;

        log::info!("Deleted DeployConfig {}/{}", ns, name);
    } else {
        cr_writers::set_orphaned(client, &ns, &name, true)
            .await
            .map_err(Error::App)?;

        log::info!(
            "DeployConfig {}/{} currently deployed, marking as orphaned instead of deleting",
            ns,
            name
        );
    }

    Ok(())
}

/// Handle a repository that can no longer be used: deleted, archived, or
/// removed from the GitHub App installation.
///
/// Every DeployConfig that references `repo` as either its config repo or
/// its artifact repo is affected: without the config repo there is nothing
/// to sync from, and without the artifact repo there is nothing to build or
/// deploy. Deployed configs are marked orphaned (so only undeploy is
/// offered); undeployed ones are deleted outright, matching what happens
/// when a push removes a `.deploy/` file.
///
/// Each config is handled independently so one failure doesn't hide the
/// rest. Returns the names of the configs that were updated.
pub async fn orphan_deploy_configs_for_repo(
    client: &Client,
    repo: &Repository,
    reason: &str,
) -> Result<Vec<String>, Error> {
    let api: Api<DeployConfig> = Api::all(client.clone());
    let deploy_configs = match api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            log::error!("Failed to list DeployConfigs:\n{}", format_error_chain(&e));
            return Err(Error::Kube(e));
        }
    };

    let mut affected = Vec::new();
    let mut first_error = None;

    for dc in deploy_configs.iter().filter(|dc| dc.references_repo(repo)) {
        let name = dc.name_any();
        log::info!(
            "DeployConfig {} references {}/{}, which was {}",
            name,
            repo.owner,
            repo.repo,
            reason
        );

        match delete_deploy_config(client, dc).await {
            Ok(()) => affected.push(name),
            Err(e) => {
                log::error!(
                    "Failed to orphan DeployConfig {}:\n{}",
                    name,
                    format_error_chain(&e)
                );
                first_error.get_or_insert(e);
            }
        }
    }

    match first_error {
        Some(e) => Err(e),
        None => Ok(affected),
    }
}

pub async fn update_deploy_configs_by_defining_repo(
    client: &Client,
    final_deploy_configs: &[DeployConfig],
    deleted_deploy_config_names: &[String],
    __defining_repo: &Repository,
) -> Result<(), Error> {
    // Find all existing deploy configs for the defining repo
    let deploy_configs_api: Api<DeployConfig> = Api::all(client.clone());
    let deploy_configs = match deploy_configs_api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            log::error!("Failed to list DeployConfigs:\n{}", format_error_chain(&e));
            return Err(Error::Kube(e));
        }
    };

    let new_deploy_config_names = final_deploy_configs
        .iter()
        .map(|dc| dc.name_any())
        .collect::<Vec<String>>();

    // FIXME: We should do the check based on the defining_repo,
    // that way deploy configs get removed properly
    // But I'm having trouble with `status` right now.
    // ChatGPT says we should stop using status and use labels or annotations instead.

    // TODO: Right now we're always filtering to just configs that are present
    //in the new set, so we never clean up any configs.
    let matching_configs = deploy_configs
        .iter()
        // FIXME: This is not the best way to handle the deleted configs. We should
        // get this out of the kubernetes API instead of the database.
        .filter(|dc| {
            deleted_deploy_config_names.contains(&dc.name_any())
                || new_deploy_config_names.contains(&dc.name_any())
        })
        .collect::<Vec<&DeployConfig>>();

    let all_names = matching_configs
        .iter()
        .map(|dc| dc.name_any())
        .chain(final_deploy_configs.iter().map(|dc| dc.name_any()))
        .unique()
        .collect::<Vec<String>>();

    for name in all_names {
        log::info!("Updating DeployConfig {}", name);
        let existing_config = matching_configs.iter().find(|dc| dc.name_any() == name);
        let final_config = final_deploy_configs.iter().find(|dc| dc.name_any() == name);

        match (existing_config, final_config) {
            (existing_config, Some(final_config)) => {
                sync_deploy_config(client, existing_config.copied(), final_config).await?;
            }
            (Some(existing_config), None) => {
                delete_deploy_config(client, existing_config).await?;
            }
            (None, None) => {
                // Do nothing
            }
        }
    }

    Ok(())
}
