use super::DeployConfig;
use super::Repository;
use crate::error::format_error_chain;
use crate::kubernetes::repo::DeploymentState;
use crate::kubernetes::{ensure_namespace_exists, Error};
use crate::prelude::*;
use itertools::Itertools;
use kube::api::{DeleteParams, PostParams};
use kube::{
    api::{Api, Patch, PatchParams, ResourceExt},
    client::Client,
};
use serde_json::Value;

// Goals: sync spec.config, spec.artifact, spec.team, spec.kind, status.orphaned (always false here)
// NON-GOALS: spec.specs (since that is updated ONLY by deploy events)
// TODO: There's some other semantics here that need to be figured out, but lets get this online again first.
// TODO: update_deploy_config does not handle namespace changes
async fn update_deploy_config(
    client: &Client,
    existing_config: &DeployConfig,
    final_config: &DeployConfig,
) -> Result<(), Error> {
    let ns = existing_config
        .namespace()
        .unwrap_or_else(|| "default".to_string());
    let new_ns = final_config
        .namespace()
        .unwrap_or_else(|| "default".to_string());
    let name = existing_config.name_any();

    if new_ns != ns {
        return Err(Error::App(AppError::Internal(
            "Namespace change not supported. You must undeploy, delete the config, and recreate it with the new namespace.".to_owned(),
        )));
    }

    // Ensure namespace exists (in case namespace changed)
    // TODO: We don't currently support namespace changes without an undeploy so this is a no-op, but it's a reminder that if we ever support it we need to do this too.
    let template_namespace = std::env::var("TEMPLATE_NAMESPACE").ok();
    ensure_namespace_exists(client, &ns, template_namespace.as_deref())
        .await
        .map_err(Error::App)?;

    // We always use the existing config's specs, since specs are only updated by deploy events.
    let mut merge_patch = final_config.clone();
    merge_patch.spec.spec.specs = existing_config.spec.spec.specs.clone();
    let merge_patch = spec_merge_patch(existing_config, &merge_patch)?;

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

/// Serialize the desired config as a merge patch, adding an explicit `null`
/// for every parameter the existing config has but the desired one does not.
/// A merge patch cannot remove a map key any other way, and `parameters` is
/// omitted from the serialized spec when empty, so without this a parameter
/// removed from the config repo would linger on the resource.
fn spec_merge_patch(existing: &DeployConfig, desired: &DeployConfig) -> Result<Value, Error> {
    let mut patch = serde_json::to_value(desired)
        .map_err(|e| Error::App(AppError::Internal(format!("serialize DeployConfig: {e}"))))?;

    let removed: Vec<&String> = existing
        .spec
        .spec
        .parameters
        .keys()
        .filter(|key| !desired.spec.spec.parameters.contains_key(*key))
        .collect();
    if !removed.is_empty() {
        let params = &mut patch["spec"]["parameters"];
        if !params.is_object() {
            *params = serde_json::json!({});
        }
        for key in removed {
            params[key] = Value::Null;
        }
    }

    Ok(patch)
}

async fn create_deploy_config(client: &Client, final_config: &DeployConfig) -> Result<(), Error> {
    let ns = final_config
        .namespace()
        .unwrap_or_else(|| "default".to_string());
    let name = final_config.name_any();

    // Ensure namespace exists before creating DeployConfig
    let template_namespace = std::env::var("TEMPLATE_NAMESPACE").ok();
    ensure_namespace_exists(client, &ns, template_namespace.as_deref())
        .await
        .map_err(Error::App)?;

    let api: Api<DeployConfig> = Api::namespaced(client.clone(), &ns);

    // We always create new configs without their specs, since specs are only updated by deploy events.
    let mut create_config = final_config.clone();
    create_config.spec.spec.specs = vec![];

    api.create(&PostParams::default(), &create_config).await?;

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
        api.patch_status(
            &name,
            &PatchParams::default(),
            &Patch::Merge(&serde_json::json!({
                "status": {
                  "orphaned": true,
                }
            })),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::{
        deploy_config::{DeployConfigSpec, DeployConfigSpecFields},
        parameters::{ParameterSource, SHA_PARAMETER},
        repo::RepositoryBranch,
        Repository,
    };

    fn dc(params: &[&str]) -> DeployConfig {
        let mut parameters = Default::default();
        for key in params {
            let rb = RepositoryBranch {
                owner: "o".into(),
                repo: (*key).to_lowercase(),
                branch: "master".into(),
            };
            let mut one = ParameterSource::sha_map(Some(rb));
            let source = one.remove(SHA_PARAMETER);
            if let Some(source) = source {
                let map: &mut std::collections::BTreeMap<String, ParameterSource> = &mut parameters;
                map.insert((*key).to_string(), source);
            }
        }
        DeployConfig::new(
            "test",
            DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    team: "t".into(),
                    kind: "service".into(),
                    parameters,
                    selections: Default::default(),
                    patches: vec![],
                    config: Repository {
                        owner: "o".into(),
                        repo: "cfg".into(),
                    },
                    specs: vec![],
                },
            },
        )
    }

    #[test]
    fn removed_parameters_become_explicit_nulls() -> Result<(), Error> {
        let existing = dc(&["SHA", "OTHER"]);
        let desired = dc(&["SHA"]);
        let patch = spec_merge_patch(&existing, &desired)?;
        assert!(patch["spec"]["parameters"]["SHA"].is_object());
        assert_eq!(patch["spec"]["parameters"]["OTHER"], Value::Null);
        Ok(())
    }

    #[test]
    fn dropping_every_parameter_still_nulls_them() -> Result<(), Error> {
        let existing = dc(&["SHA"]);
        let desired = dc(&[]);
        let patch = spec_merge_patch(&existing, &desired)?;
        assert_eq!(patch["spec"]["parameters"]["SHA"], Value::Null);
        // Legacy artifact is serialized as null too, clearing it as before.
        assert_eq!(patch["spec"]["artifact"], Value::Null);
        Ok(())
    }

    #[test]
    fn unchanged_parameters_add_nothing() -> Result<(), Error> {
        let existing = dc(&["SHA"]);
        let desired = dc(&["SHA"]);
        let patch = spec_merge_patch(&existing, &desired)?;
        assert!(patch["spec"]["parameters"].get("OTHER").is_none());
        assert!(patch["spec"]["parameters"]["SHA"].is_object());
        Ok(())
    }
}
