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

    // Get the latest commit for the repo and branch
    let commit = match get_latest_successful_commit_for_branch(
        &dc.spec.spec.repo.owner,
        &dc.spec.spec.repo.repo,
        &dc.spec.spec.repo.branch,
        &ctx.pool.get().unwrap(),
    ) {
        Ok(Some(commit)) => commit,
        Ok(None) => {
            log::warn!(
                "No successful commits found for {}/{} branch {}",
                dc.spec.spec.repo.owner,
                dc.spec.spec.repo.repo,
                dc.spec.spec.repo.branch
            );
            // Requeue after some time to check again
            return Ok(Action::requeue(Duration::from_secs(300)));
        }
        Err(e) => {
            log::error!("Error getting commit: {:?}", e);
            return Err(Error::Other(anyhow::anyhow!(
                "Failed to get commit: {:?}",
                e
            )));
        }
    };

    // Update the latest SHA in status
    update_deploy_config_status_latest(client, &ns, &name, &commit.sha).await?;

    // if autodeploy is enabled, set it as the wanted SHA too
    if dc.spec.spec.autodeploy {
        update_deploy_config_status_wanted(client, &ns, &name, &commit.sha).await?;
    }

    // Get the API for deployments
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &ns);

    // Check if deployment exists
    let existing = deployments.get(&name).await;

    match existing {
        Ok(deployment) => {
            if deployment.get_sha().is_some_and(|v| v == commit.sha) {
                log::info!(
                    "Deployment {} is already using SHA {}, no notification needed",
                    name,
                    &commit.sha
                );
                return Ok(Action::requeue(Duration::from_secs(60)));
            }

            // Deployment exists but is not on the right sha, update
            log::info!(
                "Deployment {} exists, updating to SHA {}",
                name,
                &commit.sha
            );

            // Update sha in the deployment itself
            let mut deployment_to_update = deployment.clone();

            // Ensure proper owner reference is set
            ensure_owner_reference(&mut deployment_to_update, dc.as_ref());

            let _ = deployments
                .patch(
                    &name,
                    &PatchParams::default(),
                    &Patch::Merge(&deployment_to_update.with_version(&commit.sha)),
                )
                .await?;

            update_deploy_config_status_current(client, &ns, &name, &commit.sha).await?;

            // Notify if discord is enabled for the update AND the SHA has changed
            if let Some(ref notifier) = ctx.discord_notifier {
                let _ = notifier
                    .notify_k8s_deployment(
                        &dc.spec.spec.repo.owner,
                        &dc.spec.spec.repo.repo,
                        &dc.spec.spec.repo.branch,
                        &commit.sha,
                        &name,
                        &ns,
                        "updated", // This is an existing deployment being updated
                    )
                    .await;
            }

            Ok(Action::requeue(Duration::from_secs(60)))
        }
        Err(kube::Error::Api(api_err)) if api_err.code == 404 => {
            // Deployment does not exist, create it
            log::info!("Creating new deployment {} with SHA {}", name, &commit.sha);

            // Create deployment from DeployConfig spec
            let deployment = create_deployment_from_config(dc.as_ref(), &commit.sha);
            let _created = deployments.create(&Default::default(), &deployment).await?;

            // Update status
            update_deploy_config_status_current(client, &ns, &name, &commit.sha).await?;

            // Notify if discord is enabled
            if let Some(ref notifier) = ctx.discord_notifier {
                // Use our specialized notification method for Kubernetes deployments
                let _ = notifier
                    .notify_k8s_deployment(
                        &dc.spec.spec.repo.owner,
                        &dc.spec.spec.repo.repo,
                        &dc.spec.spec.repo.branch,
                        &commit.sha,
                        &name,
                        &ns,
                        "created", // This is a new deployment
                    )
                    .await;
            }

            Ok(Action::requeue(Duration::from_secs(60)))
        }
        Err(e) => {
            log::error!("Error getting deployment: {:?}", e);
            Err(Error::KubeError(e))
        }
    }
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
