use futures_util::StreamExt;
use itertools::Itertools;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{DeleteParams, GroupVersionKind, PostParams, TypeMeta};
use kube::discovery::pinned_kind;
use kube::Resource;
use kube::{
    api::{Api, DynamicObject, ListParams, Patch, PatchParams, ResourceExt},
    client::Client,
    core::discovery,
    runtime::{controller::Action, watcher, Controller},
    Discovery,
};
use std::collections::BTreeMap;
use std::{sync::Arc, time::Duration};

use super::DeployConfig;
use super::Repository;
use crate::db::{insert_deploy_event, DeployEvent};
use crate::kubernetes::deployconfig::DEPLOY_CONFIG_KIND;
use crate::prelude::*;

pub async fn apply(
    client: &Client,
    ns: &str,
    obj: DynamicObject,
) -> Result<DynamicObject, anyhow::Error> {
    // require name + type info
    let name = obj
        .metadata
        .name
        .clone()
        .ok_or_else(|| anyhow::anyhow!("metadata.name required"))?;
    let gvk = GroupVersionKind::try_from(
        obj.types
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing types on DynamicObject"))?,
    )
    .map_err(|e| anyhow::anyhow!("failed parsing GVK: {}", e))?;

    log::debug!("Applying {}/{}", ns, name);

    // resolve ApiResource and scope
    let (ar, caps) = pinned_kind(client, &gvk)
        .await
        .map_err(|e| anyhow::anyhow!("GVK {gvk:?} not found via discovery: {}", e))?;

    let api: Api<DynamicObject> = match caps.scope {
        discovery::Scope::Namespaced => Api::namespaced_with(client.clone(), &ns, &ar),
        discovery::Scope::Cluster => Api::all_with(client.clone(), &ar),
    };

    // SSA upsert
    let pp = PatchParams::apply("cicd-controller").force(); // drop .force() if you prefer conflicts to surface
    api.patch(&name, &pp, &Patch::Apply(obj))
        .await
        .map_err(|e| anyhow::anyhow!("failed to apply object: {}", e))
}

/// Delete a DynamicObject
pub async fn delete_dynamic_object(
    client: Client,
    obj: &DynamicObject,
) -> Result<(), anyhow::Error> {
    log::debug!(
        "Deleting {}/{}",
        obj.namespace().unwrap_or_else(|| "default".to_string()),
        obj.name_any()
    );

    let name = obj.name_any();
    let ns = obj.metadata.namespace.clone(); // may be None for cluster-scoped
    let gvk = GroupVersionKind::try_from(
        obj.types
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing types"))?,
    )
    .map_err(|e| anyhow::anyhow!("failed to parse GVK: {}", e))?;

    let (ar, caps) = pinned_kind(&client, &gvk).await?;

    let api: Api<DynamicObject> = match caps.scope {
        discovery::Scope::Namespaced => {
            let ns = ns
                .ok_or_else(|| anyhow::anyhow!("namespaced resource missing metadata.namespace"))?;
            Api::namespaced_with(client, &ns, &ar)
        }
        discovery::Scope::Cluster => Api::all_with(client, &ar),
    };

    let result = api.delete(&name, &DeleteParams::default()).await;
    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("failed to delete object: {}", e)),
    }
}

/// Return all DynamicObjects in `ns`
pub async fn list_namespace_objects(
    client: Client,
    ns: &str,
) -> Result<Vec<DynamicObject>, anyhow::Error> {
    let disc = Discovery::new(client.clone()).run().await?;
    let mut out = Vec::new();

    for group in disc.groups() {
        for (ar, caps) in group.resources_by_stability() {
            // Only namespaced top-level resources (skip subresources like */status)
            if caps.scope != discovery::Scope::Namespaced || ar.plural.contains("/") {
                continue;
            }
            let types = TypeMeta {
                api_version: ar.api_version.clone(),
                kind: ar.kind.clone(),
            };

            let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), ns, &ar);

            // Paginate to avoid truncation on large lists
            // only what *we* manage
            let mut lp = ListParams::default()
                .labels("app.kubernetes.io/managed-by=cicd-controller")
                .limit(500);
            let mut continue_token: Option<String> = None;

            loop {
                if let Some(token) = continue_token.clone() {
                    lp = ListParams {
                        continue_token: Some(token),
                        ..lp.clone()
                    };
                }

                let res = api.list(&lp).await;
                let list = match res {
                    Ok(l) => l,
                    // 405 = method not allowed (common for subresources/misreported caps)
                    Err(kube::Error::Api(e)) if e.code == 405 => break,
                    // 403/404/etc.: skip this kind but keep going
                    Err(_) => break,
                };

                out.extend(list.items.into_iter().map(|mut o| {
                    o.types = o.types.or(Some(types.clone()));
                    o
                }));

                // FIXME: ChatGPT maybe suggests a "stall guard" (check to see if continue token is the same as the last one) to avoid k8s bugs.
                continue_token =
                    list.metadata
                        .continue_
                        .and_then(|x| if x == "" { None } else { Some(x) });

                if continue_token.is_none() {
                    break;
                }
            }
        }
    }

    Ok(out)
}

fn is_owned_by(obj: &DynamicObject, owner: &OwnerReference) -> bool {
    let Some(owners) = &obj.metadata.owner_references else {
        return false;
    };

    owners.iter().any(|or| or.uid == owner.uid)
}

trait WithInterpolatedVersion {
    fn with_interpolated_version(&self, version: &str) -> Self;
}

impl WithInterpolatedVersion for serde_json::Value {
    fn with_interpolated_version(&self, version: &str) -> Self {
        match self {
            serde_json::Value::Object(json) => {
                let mut new_json = serde_json::Map::new();
                for (key, value) in json {
                    new_json.insert(key.clone(), value.with_interpolated_version(version));
                }
                serde_json::Value::Object(new_json)
            }
            serde_json::Value::Array(array) => {
                let mut new_array = Vec::new();
                for value in array {
                    new_array.push(value.with_interpolated_version(version));
                }
                serde_json::Value::Array(new_array)
            }
            serde_json::Value::String(string) => {
                serde_json::Value::String(string.replace("$SHA", version))
            }
            _ => self.clone(),
        }
    }
}

impl WithInterpolatedVersion for serde_json::Map<String, serde_json::Value> {
    fn with_interpolated_version(&self, version: &str) -> Self {
        serde_json::Value::Object(self.clone())
            .with_interpolated_version(version)
            .as_object()
            .unwrap()
            .clone()
    }
}

trait WithVersion {
    fn with_version(&self, version: &str) -> Self;
    fn get_sha(&self) -> Option<&str>;
}

impl WithVersion for DynamicObject {
    /// Sets metadata.annotations.currentSha to the given version and interpolates the data with the given version
    fn with_version(&self, version: &str) -> Self {
        let mut obj = self.clone();

        if obj.meta_mut().annotations.is_none() {
            obj.meta_mut().annotations = Some(BTreeMap::new());
        }

        obj.meta_mut()
            .annotations
            .as_mut()
            .unwrap()
            .insert("currentSha".to_owned(), version.to_owned());

        obj.data = obj.data.with_interpolated_version(version);
        obj
    }

    fn get_sha(&self) -> Option<&str> {
        if let Some(annotations) = &self.meta().annotations {
            annotations.get("currentSha").map(|s| s.as_str())
        } else {
            None
        }
    }
}

/// Context for the controller
#[derive(Clone)]
pub struct ControllerContext {
    /// Kubernetes client
    client: Client,
    /// Discord notifier (if enabled)
    #[allow(dead_code)]
    discord_notifier: Option<DiscordNotifier>,
}

/// Error type for controller operations
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Kube API error
    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),

    /// Database error
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// Other errors
    #[error("Other error: {0}")]
    Other(#[from] anyhow::Error),
}

/// Ensure the labels are set on a resource
fn ensure_labels<T: ResourceExt>(resource: &mut T) {
    let labels = resource.meta_mut().labels.get_or_insert_with(BTreeMap::new);
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "cicd-controller".to_string(),
    );
}

/// Ensure the owner reference is set on a resource
fn ensure_owner_reference<T: ResourceExt>(resource: &mut T, dc: &DeployConfig) {
    // Get the current owner references or create an empty vec
    let owner_refs = resource
        .meta_mut()
        .owner_references
        .get_or_insert_with(Vec::new);

    // Check if owner reference for this DeployConfig already exists
    let owner_ref_exists = owner_refs.iter().any(|ref_| {
        ref_.kind == DEPLOY_CONFIG_KIND
            && ref_.name == dc.name_any()
            && ref_.api_version == "cicd.coolkev.com/v1"
    });

    // If it doesn't exist, add it
    if !owner_ref_exists {
        owner_refs.push(dc.child_owner_reference());
    }
}

/// The reconciliation function for DeployConfig resources
async fn reconcile(dc: Arc<DeployConfig>, ctx: Arc<ControllerContext>) -> Result<Action, Error> {
    let client = &ctx.client;
    let ns = dc.namespace().unwrap_or_else(|| "default".to_string());
    let name = dc.name_any();

    log::debug!("Reconciling DeployConfig {}/{}", ns, name);

    let wanted_sha = dc.wanted_sha();
    let current_sha = dc.current_sha();

    match (wanted_sha, current_sha) {
        (Some(wanted_sha), Some(current_sha)) => {
            if wanted_sha == current_sha {
                log::debug!(
                    "DeployConfig {}/{} is already in the desired state. Updating in case DeployConfig specs itself has changed.",
                    ns,
                    name
                );
            } else {
                log::info!(
                    "DeployConfig {}/{} current SHA differs from wanted SHA, updating resources",
                    ns,
                    name
                );
            }

            // Create or update resources as needed
            let resources = dc.spec.spec.specs.clone();
            for resource in resources {
                let mut obj: DynamicObject = serde_json::from_value(resource).map_err(|e| {
                    anyhow::anyhow!(
                        "JSON didn't look like a Kubernetes object (apiVersion/kind/metadata): {}",
                        e
                    )
                })?;
                obj = obj.with_version(wanted_sha);
                ensure_owner_reference(&mut obj, &dc);
                ensure_labels(&mut obj);
                apply(client, &ns, obj).await?;
            }

            // Prune stale resources
            log::debug!("Pruning stale resources...");
            let objects = list_namespace_objects(client.clone(), &ns).await?;
            log::debug!("Got objects in namespace {}/{}", ns, name);
            log::debug!("Objects: {objects:#?}");
            let stale_objects = objects
                .iter()
                .filter(|o| is_owned_by(o, &dc.child_owner_reference()))
                .filter(|o| o.get_sha() != Some(wanted_sha));
            log::debug!("Stale objects: {stale_objects:#?}");
            for object in stale_objects {
                log::debug!("Deleting stale resource {}/{}", ns, object.name_any());
                delete_dynamic_object(client.clone(), object).await?;
            }
            log::debug!("Pruning stale resources complete");

            update_deploy_config_status_current(client, &ns, &name, wanted_sha).await?;
            if wanted_sha != current_sha {
                log::info!(
                    "Resources for DeployConfig {}/{} have been synced",
                    ns,
                    name
                );
            }
        }
        (Some(wanted_sha), None) => {
            log::info!(
                "DeployConfig {}/{} has no current SHA, it's resources must be created",
                ns,
                name
            );

            // Create the resources for the first time
            let resources = dc.spec.spec.specs.clone();
            for resource in resources {
                let mut obj: DynamicObject = serde_json::from_value(resource).map_err(|e| {
                    anyhow::anyhow!(
                        "JSON didn't look like a Kubernetes object (apiVersion/kind/metadata): {}",
                        e
                    )
                })?;
                obj = obj.with_version(wanted_sha);
                ensure_owner_reference(&mut obj, &dc);
                ensure_labels(&mut obj);
                log::debug!("Creating resource {}/{}", ns, obj.name_any());
                apply(client, &ns, obj).await?;
            }

            update_deploy_config_status_current(client, &ns, &name, wanted_sha).await?;
            log::info!("DeployConfig {}/{} has created its resources", ns, name);
        }
        (None, Some(_)) => {
            log::info!(
                "DeployConfig {}/{} has no wanted SHA, it's resources must be undeployed",
                ns,
                name
            );

            let objects = list_namespace_objects(client.clone(), &ns).await?;

            // find objects that are owned by this DeployConfig
            let owned_objects = objects
                .iter()
                .filter(|o| is_owned_by(o, &dc.child_owner_reference()));

            for object in owned_objects {
                log::debug!("Deleting resource {}/{}", ns, object.name_any());
                delete_dynamic_object(client.clone(), object).await?;
            }

            update_deploy_config_status_current_none(client, &ns, &name).await?;
            log::info!(
                "Resources for DeployConfig {}/{} have been undeployed",
                ns,
                name
            );
        }
        (None, None) => {
            log::debug!("DeployConfig {}/{} is already undeployed", ns, name);

            return Ok(Action::requeue(Duration::from_secs(5)));
        }
    }

    // Requeue reconciliation
    Ok(Action::requeue(Duration::from_secs(5)))
}

/// Update the DeployConfig status with wanted SHA
async fn update_deploy_config_status_wanted(
    client: &Client,
    namespace: &str,
    name: &str,
    sha: &str,
) -> Result<(), Error> {
    // Get the API for DeployConfig resources
    let api: Api<DeployConfig> = Api::namespaced(client.clone(), namespace);

    // Determine the status
    let status = serde_json::json!({
        "status": {
            "wantedSha": sha,
        }
    });

    // Apply the status update
    let patch = Patch::Merge(&status);
    let params = PatchParams::default();

    api.patch_status(name, &params, &patch).await?;

    Ok(())
}

/// Update the DeployConfig status with latest SHA
async fn update_deploy_config_status_latest(
    client: &Client,
    namespace: &str,
    name: &str,
    sha: &str,
) -> Result<(), Error> {
    // Get the API for DeployConfig resources
    let api: Api<DeployConfig> = Api::namespaced(client.clone(), namespace);

    // Determine the status
    let status = serde_json::json!({
        "status": {
            "latestSha": sha,
        }
    });

    // Apply the status update
    let patch = Patch::Merge(&status);
    let params = PatchParams::default();

    api.patch_status(name, &params, &patch).await?;

    Ok(())
}

/// Update the DeployConfig status with current SHA
async fn update_deploy_config_status_current(
    client: &Client,
    namespace: &str,
    name: &str,
    sha: &str,
) -> Result<(), Error> {
    // Get the API for DeployConfig resources
    let api: Api<DeployConfig> = Api::namespaced(client.clone(), namespace);

    // Determine the status
    let status = serde_json::json!({
        "status": {
            "currentSha": sha,
        }
    });

    // Apply the status update
    let patch = Patch::Merge(&status);
    let params = PatchParams::default();

    api.patch_status(name, &params, &patch).await?;

    Ok(())
}

/// Update the DeployConfig status with current SHA
async fn update_deploy_config_status_current_none(
    client: &Client,
    namespace: &str,
    name: &str,
) -> Result<(), Error> {
    // Get the API for DeployConfig resources
    let api: Api<DeployConfig> = Api::namespaced(client.clone(), namespace);

    // Determine the status
    let status = serde_json::json!({
        "status": {
            "currentSha": null,
        }
    });

    // Apply the status update
    let patch = Patch::Merge(&status);
    let params = PatchParams::default();

    api.patch_status(name, &params, &patch).await?;

    Ok(())
}

/// Error handler for the controller
fn error_policy(_dc: Arc<DeployConfig>, error: &Error, _ctx: Arc<ControllerContext>) -> Action {
    log::error!("Error during reconciliation: {:?}", error);
    Action::requeue(Duration::from_secs(5))
}

/// Start the Kubernetes controller
pub async fn start_controller(
    client: Client,
    _pool: Pool<SqliteConnectionManager>,
    discord_notifier: Option<DiscordNotifier>,
) -> Result<(), Error> {
    let context = Arc::new(ControllerContext {
        client: client.clone(),
        discord_notifier,
    });

    // Create the API for DeployConfig resources
    let deploy_configs: Api<DeployConfig> = Api::all(client.clone());

    // Start the controller
    log::info!("Starting DeployConfig controller");

    // Create and start the controller
    Controller::new(deploy_configs, watcher::Config::default())
        .run(reconcile, error_policy, context.clone())
        .for_each(|res| async move {
            match res {
                Ok(o) => log::debug!("Reconciliation completed: {:?}", o),
                Err(e) => log::error!("Reconciliation error: {:?}", e),
            }
        })
        .await;

    Ok(())
}

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
            log::error!("Failed to list DeployConfigs: {}", e);
            return Err(Error::Kube(e));
        }
    };

    let matching_configs = deploy_configs.iter().filter(|dc| {
        dc.artifact_owner() == owner
            && dc.artifact_repo() == repo
            && dc.tracking_branch() == branch
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
        update_deploy_config_status_latest(client, &ns, &name, sha).await?;

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
                &conn,
            )?;
            update_deploy_config_status_wanted(client, &ns, &name, sha).await?;
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
                "artifact": {
                    "branch": final_config.status.as_ref().and_then(|s| s.artifact.as_ref().and_then(|a| a.branch.clone())),
                    "currentSha": final_config.status.as_ref().and_then(|s| s.artifact.as_ref().and_then(|a| a.current_sha.clone())),
                    "latestSha": final_config.status.as_ref().and_then(|s| s.artifact.as_ref().and_then(|a| a.latest_sha.clone())),
                    "wantedSha": final_config.status.as_ref().and_then(|s| s.artifact.as_ref().and_then(|a| a.wanted_sha.clone())),
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
                "artifact": {
                    "branch": final_config.status.as_ref().and_then(|s| s.artifact.as_ref().and_then(|a| a.branch.clone())),
                    "currentSha": final_config.status.as_ref().and_then(|s| s.artifact.as_ref().and_then(|a| a.current_sha.clone())),
                    "latestSha": final_config.status.as_ref().and_then(|s| s.artifact.as_ref().and_then(|a| a.latest_sha.clone())),
                    "wantedSha": final_config.status.as_ref().and_then(|s| s.artifact.as_ref().and_then(|a| a.wanted_sha.clone())),
                },
            }
        }))
        .unwrap(),
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
            log::error!("Failed to list DeployConfigs: {}", e);
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
