use super::DeployConfig;
use crate::error::format_error_chain;
use crate::kubernetes::api::update_deploy_config_status;
use crate::kubernetes::spec_editing::WithVersion;
use crate::kubernetes::{
    apply, delete_dynamic_object, ensure_labels, ensure_owner_reference, is_owned_by,
    list_namespace_objects, DeployConfigStatusBuilder,
};
use crate::prelude::*;
use futures_util::StreamExt;
use kube::{
    api::{Api, DynamicObject, ResourceExt},
    client::Client,
    runtime::{controller::Action, watcher, Controller},
};
use std::{sync::Arc, time::Duration};

/// Context for the controller
#[derive(Clone)]
pub struct ControllerContext {
    /// Kubernetes client
    client: Client,
    /// Discord notifier (if enabled)
    #[allow(dead_code)]
    discord_notifier: Option<DiscordNotifier>,
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
            for resource in dc.resource_specs() {
                let mut obj: DynamicObject =
                    serde_json::from_value(resource.clone()).map_err(|e| {
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
            // FIXME: Does this have a bug if artifact sha stays the same but config sha changes?
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

            update_deploy_config_status(
                client,
                &ns,
                &name,
                DeployConfigStatusBuilder::new().with_artifact_current_sha(wanted_sha.to_string()),
            )
            .await?;
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
            for resource in dc.resource_specs() {
                let mut obj: DynamicObject =
                    serde_json::from_value(resource.clone()).map_err(|e| {
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

            update_deploy_config_status(
                client,
                &ns,
                &name,
                DeployConfigStatusBuilder::new().with_artifact_current_sha(wanted_sha.to_string()),
            )
            .await?;
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

            update_deploy_config_status(
                client,
                &ns,
                &name,
                DeployConfigStatusBuilder::new().with_null_artifact_current_sha(),
            )
            .await?;
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

/// Error handler for the controller
fn error_policy(_dc: Arc<DeployConfig>, error: &Error, _ctx: Arc<ControllerContext>) -> Action {
    log::error!(
        "Error during reconciliation:\n{}",
        format_error_chain(error)
    );
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
