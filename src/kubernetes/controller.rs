use super::DeployConfig;
use crate::error::format_error_chain;
use crate::kubernetes::api::list_namespace_objects_live;
use crate::kubernetes::api::ListMode;
use crate::kubernetes::repo::DeploymentState;
use crate::kubernetes::{apply, delete_dynamic_object, ensure_namespace_exists};
use crate::prelude::*;
use futures_util::StreamExt;
use kube::{
    api::{Api, DynamicObject, ResourceExt},
    client::Client,
    runtime::{controller::Action, watcher, Controller},
};
use std::{sync::Arc, time::Duration};

/// How often an unchanged config is reconciled again to catch drift.
const RESYNC: Duration = Duration::from_secs(300);

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

    // Ensure namespace exists (safety check - DeployConfig should already exist in target namespace)
    let template_namespace = std::env::var("TEMPLATE_NAMESPACE").ok();
    if let Err(e) = ensure_namespace_exists(client, &ns, template_namespace.as_deref()).await {
        log::warn!(
            "Failed to ensure namespace {} exists during reconciliation: {}",
            ns,
            e
        );
        // Continue anyway - namespace might already exist
    }

    // Undeployed means "run nothing": apply no manifests and let the prune
    // step below remove anything still owned. Manifest templates can be
    // present without a deployed version for a moment during a deploy (they
    // are written before the status), and applying them then would render
    // `$SHA` literally and roll out an image that does not exist.
    let state = dc.deployment_state();
    let resources: Vec<serde_json::Value> = if state == DeploymentState::Undeployed {
        if !dc.resource_specs().is_empty() {
            log::debug!(
                "DeployConfig {}/{} is undeployed; not applying its {} manifest templates",
                ns,
                name,
                dc.resource_specs().len()
            );
        }
        vec![]
    } else {
        // Every declared parameter the templates use must have a deployed
        // value; otherwise this errors and nothing is applied or pruned.
        dc.render_manifests()?
    };

    // Create or update resources as needed
    for obj in dc.child_objects(resources)? {
        apply(client, &ns, obj).await?;
    }

    // Prune stale resources
    log::debug!("Pruning stale resources...");
    // Live, never from the cache: this list decides what gets deleted.
    let objects = list_namespace_objects_live(client, &ns, ListMode::Owned).await?;
    log::debug!("Got objects in namespace {}/{}", ns, name);
    log::trace!("Objects: {objects:#?}");
    let stale_objects: Vec<DynamicObject> = objects
        .into_iter()
        .filter(|o| dc.owns(o))
        .filter(|o| !dc.child_is_up_to_date(o))
        .collect();
    log::debug!("Stale objects: {stale_objects:#?}");
    let prune_disabled = prune_disabled();
    for object in stale_objects {
        if prune_disabled {
            log::warn!(
                "Prune disabled: would delete stale resource {}/{} ({}) owned by DeployConfig {}",
                ns,
                object.name_any(),
                object
                    .types
                    .as_ref()
                    .map(|t| t.kind.as_str())
                    .unwrap_or("?"),
                name
            );
            continue;
        }
        log::debug!("Deleting stale resource {}/{}", ns, object.name_any());
        delete_dynamic_object(client.clone(), &object).await?;
    }
    log::debug!("Pruning stale resources complete");

    // Requeue reconciliation
    // Changes to the DeployConfig reconcile immediately through the watch;
    // this requeue only catches drift in the children. Five seconds across
    // sixty configs, each listing every kind in its namespace, was most of
    // the API server's load.
    Ok(Action::requeue(RESYNC))
}

/// Whether the stale-resource prune step is switched off.
///
/// Set `CICD_DISABLE_PRUNE=true` on the controller to make reconcile log what
/// it would have deleted instead of deleting it. Intended as a safety valve
/// during schema migrations, where a misread DeployConfig could otherwise
/// cause running workloads to be pruned. Everything else about reconcile
/// (applying manifests, annotations, owner references) is unchanged.
fn prune_disabled() -> bool {
    std::env::var("CICD_DISABLE_PRUNE")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
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
