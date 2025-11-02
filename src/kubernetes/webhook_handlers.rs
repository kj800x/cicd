use super::DeployConfig;
use super::Repository;
use crate::error::format_error_chain;
use crate::kubernetes::repo::DeploymentState;
use crate::kubernetes::Error;
use crate::prelude::*;
use itertools::Itertools;
use kube::api::{DeleteParams, PostParams};
use kube::{
    api::{Api, Patch, PatchParams, ResourceExt},
    client::Client,
};

// Goals: sync spec.config, spec.artifact, spec.team, spec.kind, status.orphaned (always false here)
// NON-GOALS: spec.specs (since that is updated ONLY by deploy events)
// TODO: There's some other semantics here that need to be figured out, but lets get this online again first.
async fn update_deploy_config(
    client: &Client,
    existing_config: &DeployConfig,
    final_config: &DeployConfig,
) -> Result<(), Error> {
    let ns = existing_config
        .namespace()
        .unwrap_or_else(|| "default".to_string());
    let name = existing_config.name_any();

    // We always use the existing config's specs, since specs are only updated by deploy events.
    let mut merge_patch = final_config.clone();
    merge_patch.spec.spec.specs = existing_config.spec.spec.specs.clone();

    let api: Api<DeployConfig> = Api::namespaced(client.clone(), &ns);
    api.patch(&name, &PatchParams::default(), &Patch::Merge(&merge_patch))
        .await?;

    api.patch_status(
        &name,
        &PatchParams::default(),
        &Patch::Merge(&serde_json::json!({
            "status": {
              "orphaned": false,
            }
        })),
    )
    .await?;

    log::info!("Updated DeployConfig {}/{}", ns, name);

    Ok(())
}

async fn create_deploy_config(client: &Client, final_config: &DeployConfig) -> Result<(), Error> {
    let ns = final_config
        .namespace()
        .unwrap_or_else(|| "default".to_string());
    let name = final_config.name_any();

    let api: Api<DeployConfig> = Api::namespaced(client.clone(), &ns);

    // We always create new configs without their specs, since specs are only updated by deploy events.
    let mut create_config = final_config.clone();
    create_config.spec.spec.specs = vec![];

    api.create(&PostParams::default(), &create_config).await?;

    // FIXME: Does patch-status make sense here?
    #[allow(clippy::expect_used)]
    api.replace_status(
        &name,
        &PostParams::default(),
        serde_json::to_vec(&serde_json::json!({
            "status": {
              "orphaned": false,
            }
        }))
        .expect("Should be able to serialize DeployConfig status"),
    )
    .await?;

    log::info!("Created DeployConfig {}/{}", ns, name);

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
        // FIXME: Does patch-status make sense here?
        #[allow(clippy::expect_used)]
        api.replace_status(
            &name,
            &PostParams::default(),
            serde_json::to_vec(&serde_json::json!({
                "status": {
                  "orphaned": true,
                }
            }))
            .expect("Should be able to serialize DeployConfig status"),
        )
        .await?;

        log::info!(
            "DeployConfig {}/{} currently deployed, marking as orphaned instead of deleting",
            ns,
            name
        );
    }

    Ok(())
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
            (Some(existing_config), Some(final_config)) => {
                update_deploy_config(client, existing_config, final_config).await?;
            }
            (None, Some(final_config)) => {
                create_deploy_config(client, final_config).await?;
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
