use super::DeployConfig;
use super::Repository;
use crate::db::{insert_deploy_event, DeployEvent};
use crate::error::format_error_chain;
use crate::kubernetes::api::update_deploy_config_status;
use crate::kubernetes::controller::Error;
use crate::kubernetes::DeployConfigStatusBuilder;
use crate::prelude::*;
use itertools::Itertools;
use kube::api::{DeleteParams, PostParams};
use kube::{
    api::{Api, Patch, PatchParams, ResourceExt},
    client::Client,
};

/// Handle build completion events by updating relevant DeployConfigs
pub async fn handle_build_completed(
    client: &Client,
    owner: &str,
    repo: &str,
    branch: &str,
    sha: &str,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<(), Error> {
    log::debug!(
        "Build completed for {}/{} branch {} with SHA {}",
        owner,
        repo,
        branch,
        &sha[0..7]
    );

    // Find all DeployConfigs that match this repo and branch
    let deploy_configs_api: Api<DeployConfig> = Api::all(client.clone());
    let deploy_configs = match deploy_configs_api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            log::error!("Failed to list DeployConfigs:\n{}", format_error_chain(&e));
            return Err(Error::Kube(e));
        }
    };

    let matching_configs = deploy_configs.iter().filter(|dc| {
        dc.artifact_owner() == owner && dc.artifact_repo() == repo && dc.tracking_branch() == branch
    });

    for config in matching_configs {
        let ns = config.namespace().unwrap_or_else(|| "default".to_string());
        let name = config.name_any();

        log::info!(
            "Updating DeployConfig {}/{} with latest SHA {}",
            ns,
            name,
            &sha[0..7]
        );

        // RULE: When a build completes, update latestSha for all matching DeployConfigs
        update_deploy_config_status(
            client,
            &ns,
            &name,
            DeployConfigStatusBuilder::new().with_artifact_latest_sha(sha.to_string()),
        )
        .await?;

        // RULE: If autodeploy is enabled, also update wantedSha
        if config.current_autodeploy() {
            log::info!(
                "DeployConfig {}/{} has autodeploy enabled - setting wantedSha to {}",
                ns,
                name,
                &sha[0..7]
            );
            insert_deploy_event(
                &DeployEvent {
                    deploy_config: name.to_string(),
                    team: config.team().to_string(),
                    timestamp: Utc::now().timestamp(),
                    initiator: "autodeploy".to_string(),
                    status: "SUCCESS".to_string(),
                    branch: Some(branch.to_string()),
                    sha: Some(sha.to_string()),
                },
                conn,
            )?;
            update_deploy_config_status(
                client,
                &ns,
                &name,
                DeployConfigStatusBuilder::new().with_artifact_wanted_sha(sha.to_string()),
            )
            .await?;
        }
    }

    Ok(())
}

// MARK: - update_deploy_configs_by_defining_repo

async fn update_deploy_config(
    client: &Client,
    existing_config: &DeployConfig,
    final_config: &DeployConfig,
) -> Result<(), Error> {
    let ns = existing_config
        .namespace()
        .unwrap_or_else(|| "default".to_string());
    let name = existing_config.name_any();

    let api: Api<DeployConfig> = Api::namespaced(client.clone(), &ns);
    api.patch(&name, &PatchParams::default(), &Patch::Merge(&final_config))
        .await?;
    // FIXME: Does patch-status make sense here?
    api.patch_status(
      &name,
      &PatchParams::default(),
      &Patch::Merge(&serde_json::json!({
          "status": {
              "config": {
                  "repo": final_config.status.as_ref().and_then(|s| s.config.as_ref().and_then(|c| c.repo.clone())),
                  "sha": final_config.status.as_ref().and_then(|s| s.config.as_ref().and_then(|c| c.sha.clone())),
                  "owner": final_config.status.as_ref().and_then(|s| s.config.as_ref().and_then(|c| c.owner.clone())),
              },
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

    api.create(&PostParams::default(), final_config).await?;

    // FIXME: Does patch-status make sense here?
    #[allow(clippy::expect_used)]
  api.replace_status(
      &name,
      &PostParams::default(),
      serde_json::to_vec(&serde_json::json!({
          "status": {
              "config": {
                  "repo": final_config.status.as_ref().and_then(|s| s.config.as_ref().and_then(|c| c.repo.clone())),
                  "sha": final_config.status.as_ref().and_then(|s| s.config.as_ref().and_then(|c| c.sha.clone())),
                  "owner": final_config.status.as_ref().and_then(|s| s.config.as_ref().and_then(|c| c.owner.clone())),
              },
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
    api.delete(&name, &DeleteParams::default()).await?;

    log::info!("Deleted DeployConfig {}/{}", ns, name);

    Ok(())
}

pub async fn update_deploy_configs_by_defining_repo(
    client: &Client,
    final_deploy_configs: &[DeployConfig],
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
    let matching_configs = deploy_configs
        .iter()
        .filter(|dc| new_deploy_config_names.contains(&dc.name_any()))
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
