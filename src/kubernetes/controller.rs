use super::DeployConfig;
use crate::error::format_error_chain;
use crate::kubernetes::api::ListMode;
use crate::kubernetes::repo::DeploymentState;
use crate::kubernetes::spec_editing::WithVersion;
use crate::kubernetes::{apply, delete_dynamic_object, list_namespace_objects};
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
}

/// The reconciliation function for DeployConfig resources
async fn reconcile(dc: Arc<DeployConfig>, ctx: Arc<ControllerContext>) -> AppResult<Action> {
    let client = &ctx.client;
    let ns = dc.namespace().unwrap_or_else(|| "default".to_string());
    let name = dc.name_any();

    log::debug!("Reconciling DeployConfig {}/{}", ns, name);

    // Create or update resources as needed
    for resource in dc.resource_specs() {
        let mut obj: DynamicObject = serde_json::from_value(resource.clone()).map_err(|e| {
            AppError::Internal(format!(
                "JSON didn't look like a Kubernetes object (apiVersion/kind/metadata): {}",
                e
            ))
        })?;

        if let DeploymentState::DeployedWithArtifact { artifact, .. } = dc.deployment_state() {
            obj = obj.with_version(&artifact.sha);
        }

        dc.ensure_owner_reference(&mut obj);
        dc.ensure_labels(&mut obj);
        dc.ensure_annotations(&mut obj);
        apply(client, &ns, obj).await?;
    }

    // Prune stale resources
    log::debug!("Pruning stale resources...");
    let objects = list_namespace_objects(client, &ns, ListMode::Owned).await?;
    log::debug!("Got objects in namespace {}/{}", ns, name);
    log::trace!("Objects: {objects:#?}");
    let stale_objects: Vec<DynamicObject> = objects
        .into_iter()
        .filter(|o| dc.owns(o))
        .filter(|o| !dc.child_is_up_to_date(o))
        .collect();
    log::debug!("Stale objects: {stale_objects:#?}");
    for object in stale_objects {
        log::debug!("Deleting stale resource {}/{}", ns, object.name_any());
        delete_dynamic_object(client.clone(), &object).await?;
    }
    log::debug!("Pruning stale resources complete");

    // Requeue reconciliation
    Ok(Action::requeue(Duration::from_secs(5)))
}

/// Error handler for the controller
fn error_policy(_dc: Arc<DeployConfig>, error: &AppError, _ctx: Arc<ControllerContext>) -> Action {
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
    // discord_notifier: Option<DiscordNotifier>,
) -> AppResult<()> {
    let context = Arc::new(ControllerContext {
        client: client.clone(),
        // discord_notifier,
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
