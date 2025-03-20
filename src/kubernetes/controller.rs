use futures_util::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use kube::{
    api::{Api, Patch, PatchParams, ResourceExt},
    client::Client,
    runtime::{controller::Action, watcher, Controller},
};
use std::{sync::Arc, time::Duration};

use super::DeployConfig;
use crate::{db::get_latest_successful_commit_for_branch, prelude::*};

trait WithVersion {
    fn with_version(&self, version: &str) -> Self;
    fn get_sha(&self) -> Option<&str>;
}

impl WithVersion for Deployment {
    fn with_version(&self, version: &str) -> Self {
        let mut deployment = self.clone();
        if let Some(spec) = deployment.spec.as_mut() {
            if let Some(template) = spec.template.spec.as_mut() {
                for container in &mut template.containers {
                    if let Some(image) = &container.image {
                        if let Some(idx) = image.rfind(':') {
                            container.image = Some(format!("{}:commit-{}", &image[..idx], version));
                        } else {
                            container.image = Some(format!("{}:commit-{}", image, version));
                        }
                    }
                }
            }
        }
        deployment
    }

    fn get_sha(&self) -> Option<&str> {
        self.spec.as_ref().and_then(|spec| {
            spec.template.spec.as_ref().and_then(|template| {
                template.containers.first().and_then(|container| {
                    container
                        .image
                        .as_ref()
                        .map(|image| {
                            let parts = image.split(":commit-").collect::<Vec<&str>>();
                            parts.get(1).copied()
                        })
                        .flatten()
                })
            })
        })
    }
}
/// Context for the controller
#[derive(Clone)]
pub struct ControllerContext {
    /// Kubernetes client
    client: Client,
    /// Database pool
    pool: Pool<SqliteConnectionManager>,
    /// Discord notifier (if enabled)
    discord_notifier: Option<DiscordNotifier>,
}

/// Error type for controller operations
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Kube API error
    #[error("Kubernetes API error: {0}")]
    KubeError(#[from] kube::Error),

    /// Database error
    #[error("Database error: {0}")]
    DbError(#[from] rusqlite::Error),

    /// Other errors
    #[error("Other error: {0}")]
    Other(#[from] anyhow::Error),
}

/// Ensure the owner reference is set on a deployment
fn ensure_owner_reference(deployment: &mut Deployment, dc: &DeployConfig) {
    // Get the current owner references or create an empty vec
    let owner_refs = deployment
        .metadata
        .owner_references
        .get_or_insert_with(Vec::new);

    // Check if owner reference for this DeployConfig already exists
    let owner_ref_exists = owner_refs.iter().any(|ref_| {
        ref_.kind == "DeployConfig"
            && ref_.name == dc.name_any()
            && ref_.api_version == "cicd.coolkev.com/v1"
    });

    // If it doesn't exist, add it
    if !owner_ref_exists {
        owner_refs.push(
            k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                api_version: String::from("cicd.coolkev.com/v1"),
                kind: String::from("DeployConfig"),
                name: dc.name_any(),
                uid: dc.uid().expect("DeployConfig should have a UID"),
                controller: Some(true),
                block_owner_deletion: Some(true),
            },
        );
    }
}

/// The reconciliation function for DeployConfig resources
async fn reconcile(dc: Arc<DeployConfig>, ctx: Arc<ControllerContext>) -> Result<Action, Error> {
    let client = &ctx.client;
    let ns = dc.namespace().unwrap_or_else(|| "default".to_string());
    let name = dc.name_any();

    log::info!("Reconciling DeployConfig {}/{}", ns, name);

    // RULE: When a deployment's SHA doesn't match wantedSha, update the Deployment
    // or create/delete as needed based on wantedSha presence
    match &dc.status {
        Some(status) => {
            // Check if there's a wanted SHA set
            match &status.wanted_sha {
                Some(wanted_sha) => {
                    // Get the API for deployments
                    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &ns);

                    // Check if deployment exists
                    match deployments.get(&name).await {
                        Ok(deployment) => {
                            // Deployment exists - check if SHA matches wanted SHA
                            let current_sha = deployment.get_sha();

                            if current_sha.is_some_and(|v| v == wanted_sha) {
                                // Deployment is already at the desired version
                                log::info!(
                                    "Deployment {}/{} is already at the desired SHA: {}",
                                    ns,
                                    name,
                                    wanted_sha
                                );
                            } else {
                                // Deployment needs to be updated
                                log::info!(
                                    "Updating deployment {}/{} to use SHA {}",
                                    ns,
                                    name,
                                    wanted_sha
                                );

                                let mut deployment_to_update = deployment.clone();

                                // Ensure proper owner reference is set
                                ensure_owner_reference(&mut deployment_to_update, dc.as_ref());

                                // Update the deployment
                                let _ = deployments
                                    .patch(
                                        &name,
                                        &PatchParams::default(),
                                        &Patch::Merge(
                                            &deployment_to_update.with_version(wanted_sha),
                                        ),
                                    )
                                    .await?;

                                // Update the current SHA in status
                                update_deploy_config_status_current(client, &ns, &name, wanted_sha)
                                    .await?;

                                // Notify if discord is enabled for the update
                                if let Some(ref notifier) = ctx.discord_notifier {
                                    let _ = notifier
                                        .notify_k8s_deployment(
                                            &dc.spec.spec.repo.owner,
                                            &dc.spec.spec.repo.repo,
                                            &dc.spec.spec.repo.branch,
                                            wanted_sha,
                                            &name,
                                            &ns,
                                            "updated",
                                        )
                                        .await;
                                }
                            }
                        }
                        Err(kube::Error::Api(api_err)) if api_err.code == 404 => {
                            // Deployment does not exist, but wantedSHA exists - create it
                            log::info!(
                                "Creating new deployment {}/{} with SHA {}",
                                ns,
                                name,
                                wanted_sha
                            );

                            // Create deployment from DeployConfig spec
                            let deployment = create_deployment_from_config(dc.as_ref(), wanted_sha);
                            let _created =
                                deployments.create(&Default::default(), &deployment).await?;

                            // Update the current SHA in status
                            update_deploy_config_status_current(client, &ns, &name, wanted_sha)
                                .await?;

                            // Notify if discord is enabled
                            if let Some(ref notifier) = ctx.discord_notifier {
                                let _ = notifier
                                    .notify_k8s_deployment(
                                        &dc.spec.spec.repo.owner,
                                        &dc.spec.spec.repo.repo,
                                        &dc.spec.spec.repo.branch,
                                        wanted_sha,
                                        &name,
                                        &ns,
                                        "created",
                                    )
                                    .await;
                            }
                        }
                        Err(e) => {
                            log::error!("Error checking deployment: {:?}", e);
                            return Err(Error::KubeError(e));
                        }
                    }
                }
                None => {
                    // No wanted SHA - delete deployment if it exists
                    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &ns);

                    match deployments.get(&name).await {
                        Ok(_) => {
                            // Deployment exists but no wanted SHA - delete it
                            log::info!(
                                "Deleting deployment {}/{} as no wantedSha is set",
                                ns,
                                name
                            );

                            match deployments.delete(&name, &Default::default()).await {
                                Ok(_) => {
                                    // Clear the current SHA from status
                                    update_deploy_config_status_current_none(client, &ns, &name)
                                        .await?;

                                    // Notify if discord is enabled
                                    if let Some(ref notifier) = ctx.discord_notifier {
                                        let _ = notifier
                                            .notify_k8s_deployment(
                                                &dc.spec.spec.repo.owner,
                                                &dc.spec.spec.repo.repo,
                                                &dc.spec.spec.repo.branch,
                                                "none",
                                                &name,
                                                &ns,
                                                "deleted",
                                            )
                                            .await;
                                    }
                                }
                                Err(e) => {
                                    log::error!("Error deleting deployment: {:?}", e);
                                    return Err(Error::KubeError(e));
                                }
                            }
                        }
                        Err(kube::Error::Api(api_err)) if api_err.code == 404 => {
                            // No deployment and no wanted SHA - nothing to do
                            log::info!("No deployment and no wantedSha for {}/{}", ns, name);
                        }
                        Err(e) => {
                            log::error!("Error checking deployment: {:?}", e);
                            return Err(Error::KubeError(e));
                        }
                    }
                }
            }
        }
        None => {
            // No status yet - nothing to do
            log::info!("No status set for DeployConfig {}/{}", ns, name);
        }
    }

    // Requeue reconciliation
    Ok(Action::requeue(Duration::from_secs(60)))
}

/// Error handler for the controller
fn error_policy(_dc: Arc<DeployConfig>, error: &Error, _ctx: Arc<ControllerContext>) -> Action {
    log::error!("Error during reconciliation: {:?}", error);
    Action::requeue(Duration::from_secs(60))
}

/// Create a new deployment from the DeployConfig
fn create_deployment_from_config(dc: &DeployConfig, commit_sha: &str) -> Deployment {
    let deployment = Deployment {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(dc.name_any()),
            namespace: dc.namespace(),
            labels: Some(
                [(
                    "app.kubernetes.io/managed-by".to_string(),
                    "cicd-controller".to_string(),
                )]
                .iter()
                .cloned()
                .collect(),
            ),
            // Set owner reference to enable garbage collection and proper resource tracking
            owner_references: Some(vec![
                k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                    api_version: String::from("cicd.coolkev.com/v1"),
                    kind: String::from("DeployConfig"),
                    name: dc.name_any(),
                    uid: dc.uid().expect("DeployConfig should have a UID"),
                    controller: Some(true),
                    block_owner_deletion: Some(true),
                },
            ]),
            ..Default::default()
        },
        spec: Some(*dc.spec.spec.spec.clone()),
        status: None,
    };

    deployment.with_version(commit_sha)
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

/// Start the Kubernetes controller
pub async fn start_controller(
    client: Client,
    pool: Pool<SqliteConnectionManager>,
    discord_notifier: Option<DiscordNotifier>,
) -> Result<(), Error> {
    // Create the controller context
    let context = Arc::new(ControllerContext {
        client: client.clone(),
        pool,
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
                Ok(o) => log::info!("Reconciliation completed: {:?}", o),
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
) -> Result<(), Error> {
    log::info!(
        "Build completed for {}/{} branch {} with SHA {}",
        owner,
        repo,
        branch,
        sha
    );

    // Find all DeployConfigs that match this repo and branch
    let deploy_configs_api: Api<DeployConfig> = Api::all(client.clone());
    let deploy_configs = match deploy_configs_api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            log::error!("Failed to list DeployConfigs: {}", e);
            return Err(Error::KubeError(e));
        }
    };

    let matching_configs = deploy_configs.iter().filter(|dc| {
        dc.spec.spec.repo.owner == owner
            && dc.spec.spec.repo.repo == repo
            && dc.spec.spec.repo.branch == branch
    });

    for config in matching_configs {
        let ns = config.namespace().unwrap_or_else(|| "default".to_string());
        let name = config.name_any();

        log::info!(
            "Updating DeployConfig {}/{} with latest SHA {}",
            ns,
            name,
            sha
        );

        // RULE: When a build completes, update latestSha for all matching DeployConfigs
        update_deploy_config_status_latest(client, &ns, &name, sha).await?;

        // RULE: If autodeploy is enabled, also update wantedSha
        if config.spec.spec.autodeploy {
            log::info!(
                "DeployConfig {}/{} has autodeploy enabled - setting wantedSha to {}",
                ns,
                name,
                sha
            );
            update_deploy_config_status_wanted(client, &ns, &name, sha).await?;
        }
    }

    Ok(())
}
