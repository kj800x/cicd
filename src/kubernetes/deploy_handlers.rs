use chrono::Utc;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{ObjectMeta, PostParams};
use kube::{Api, Client, ResourceExt};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::crab_ext::Octocrabs;
use crate::kubernetes::api::{
    delete_deploy_config, get_deploy_config, set_deploy_config_specs, update_deploy_config_status,
};
use crate::kubernetes::DeployConfigStatusBuilder;
use crate::webhooks::config_sync::fetch_deploy_config_by_sha;
use crate::{
    crab_ext::IRepo,
    error::{AppError, AppResult},
    kubernetes::repo::ShaMaybeBranch,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployAction {
    Bounce {
        name: String,
    },
    ExecuteJob {
        name: String,
    },
    Deploy {
        name: String,
        artifact: Option<ShaMaybeBranch>,
        config: ShaMaybeBranch,
    },
    Undeploy {
        name: String,
    },
    ToggleAutodeploy {
        name: String,
    },
}

impl DeployAction {
    pub fn config_name(&self) -> &str {
        match self {
            DeployAction::Bounce { name } => name,
            DeployAction::ExecuteJob { name } => name,
            DeployAction::Deploy { name, .. } => name,
            DeployAction::Undeploy { name } => name,
            DeployAction::ToggleAutodeploy { name } => name,
        }
    }

    pub fn action_type(&self) -> &'static str {
        match self {
            DeployAction::Bounce { .. } => "bounce",
            DeployAction::ExecuteJob { .. } => "execute_job",
            DeployAction::Deploy { .. } => "deploy",
            DeployAction::Undeploy { .. } => "undeploy",
            DeployAction::ToggleAutodeploy { .. } => "toggle_autodeploy",
        }
    }

    // KNOWN LIMITATION: Changing a DeployConfig's namespace is not supported.
    // The deploy operation applies resources to the namespace in the current config,
    // not the new namespace specified in the updated .deploy/*.yaml file.
    //
    // To change a config's namespace:
    // 1. Undeploy the config from its current namespace
    // 2. Update the .deploy/*.yaml file with the new namespace
    // 3. Push to master to sync the config
    // 4. Deploy to the new namespace
    //
    // This limitation exists because the DeployAction executor uses the existing
    // config's namespace, not the desired config's namespace.
    pub async fn execute(
        &self,
        client: &Client,
        octocrabs: &Octocrabs,
        repository: impl IRepo,
    ) -> AppResult<()> {
        match self {
            DeployAction::Deploy {
                name,
                artifact,
                config,
            } => {
                log::debug!(
                    "Deploy action: name={}, artifact={:?}, config={:?}",
                    name,
                    artifact,
                    config
                );
                log::debug!(
                    "Fetching config from repo {}/{} at sha: {}",
                    repository.owner(),
                    repository.repo(),
                    config.sha
                );

                let desired_config =
                    fetch_deploy_config_by_sha(octocrabs, repository, &config.sha, name)
                        .await?
                        .ok_or(AppError::NotFound("Desired config not found".to_owned()))?;

                log::debug!(
                    "Fetched config for {}: {} specs found",
                    name,
                    desired_config.spec.spec.specs.len()
                );
                for (idx, spec) in desired_config.spec.spec.specs.iter().enumerate() {
                    log::debug!("  spec[{}]: {}", idx, spec);
                }

                set_deploy_config_specs(
                    client,
                    &desired_config.namespace().unwrap_or_default(),
                    name,
                    desired_config.spec.spec.specs.clone(),
                )
                .await?;

                update_deploy_config_status(
                    client,
                    &desired_config.namespace().unwrap_or_default(),
                    name,
                    DeployConfigStatusBuilder::default()
                        .with_artifact(artifact.clone())
                        .with_config(Some(config.clone())),
                )
                .await?;

                Ok(())
            }

            DeployAction::Undeploy { name } => {
                let current_config = get_deploy_config(client, name)
                    .await?
                    .ok_or(AppError::NotFound("Current config not found".to_owned()))?;

                let namespace = current_config.namespace().unwrap_or_default();
                set_deploy_config_specs(client, &namespace, name, vec![]).await?;

                update_deploy_config_status(
                    client,
                    &namespace,
                    name,
                    DeployConfigStatusBuilder::default()
                        .with_artifact(None)
                        .with_config(None),
                )
                .await?;

                // When undeploying an orphaned config, we should fully delete it.
                if current_config
                    .status
                    .is_some_and(|s| s.orphaned.is_some_and(|x| x))
                {
                    delete_deploy_config(client, &namespace, name).await?;
                }

                Ok(())
            }

            DeployAction::ToggleAutodeploy { name } => {
                let current_config = get_deploy_config(client, name)
                    .await?
                    .ok_or(AppError::NotFound("Current config not found".to_owned()))?;

                let namespace = current_config.namespace().unwrap_or_default();

                let current_autodeploy = current_config
                    .status
                    .and_then(|s| s.autodeploy)
                    .unwrap_or(false);

                update_deploy_config_status(
                    client,
                    &namespace,
                    name,
                    DeployConfigStatusBuilder::default().with_autodeploy(Some(!current_autodeploy)),
                )
                .await?;

                Ok(())
            }

            DeployAction::Bounce { name } => {
                log::debug!("Bounce action: name={}", name);
                let current_config = get_deploy_config(client, name)
                    .await?
                    .ok_or(AppError::NotFound("Current config not found".to_owned()))?;

                let namespace = current_config.namespace().unwrap_or_default();
                log::debug!("Bouncing deployments in namespace: {}", namespace);
                let deployments: Api<Deployment> = Api::namespaced(client.clone(), &namespace);

                let specs = current_config.resource_specs();
                let deployments_vec = specs
                    .iter()
                    .filter(|spec| spec.get("kind").and_then(|k| k.as_str()) == Some("Deployment"))
                    .map(|spec| serde_json::from_value(spec.clone()).unwrap_or_default())
                    .collect::<Vec<Deployment>>();

                log::debug!("Found {} deployments to bounce", deployments_vec.len());
                for deployment in deployments_vec {
                    let deployment_name = deployment.name_any();
                    log::info!("Restarting deployment {}", deployment_name);
                    deployments.restart(&deployment_name).await?;
                }

                Ok(())
            }

            DeployAction::ExecuteJob { name } => {
                let current_config = get_deploy_config(client, name)
                    .await?
                    .ok_or(AppError::NotFound("Current config not found".to_owned()))?;

                let deploy_config_uid = current_config.uid().ok_or(AppError::Internal(
                    "DeployConfig should have a UID".to_owned(),
                ))?;

                let namespace = current_config.namespace().unwrap_or_default();
                let jobs: Api<Job> = Api::namespaced(client.clone(), &namespace);
                let cronjobs: Api<CronJob> = Api::namespaced(client.clone(), &namespace);

                // Filter all cronjobs in the namespace to just ones owned by this deploy config
                let cronjobs_vec = cronjobs
                    .list(&Default::default())
                    .await?
                    .items
                    .into_iter()
                    .filter(|cronjob| {
                        cronjob
                            .metadata
                            .owner_references
                            .as_ref()
                            .unwrap_or(&Vec::new())
                            .iter()
                            .any(|or| or.uid == deploy_config_uid)
                    })
                    .collect::<Vec<CronJob>>();

                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| AppError::Internal(format!("Failed to get timestamp: {}", e)))?
                    .as_secs();

                for cronjob in cronjobs_vec {
                    let cronjob_name = cronjob.name_any();
                    let job_name = format!("{}-manual-{}", cronjob_name, timestamp);
                    let job = cronjob.instantiate(&job_name)?;

                    log::info!(
                        "Creating manual job {} from CronJob {}",
                        job_name,
                        cronjob_name
                    );
                    jobs.create(&PostParams::default(), &job).await?;
                }

                Ok(())
            }
        }
    }
}

trait CronJobExt {
    fn instantiate(&self, job_name: &str) -> AppResult<Job>;
}

impl CronJobExt for CronJob {
    fn instantiate(&self, job_name: &str) -> AppResult<Job> {
        let cronjob_spec = self.spec.as_ref().ok_or_else(|| {
            AppError::Internal(format!("CronJob {} missing spec", self.name_any()))
        })?;

        let job_template_spec = cronjob_spec.job_template.spec.clone().ok_or_else(|| {
            AppError::Internal(format!(
                "CronJob {} missing job_template.spec",
                self.name_any()
            ))
        })?;

        let job_template_metadata = cronjob_spec
            .job_template
            .metadata
            .clone()
            .unwrap_or_default();

        // Copy labels from job_template.metadata
        let mut labels = job_template_metadata.labels.clone().unwrap_or_default();

        // Add standard Kubernetes labels
        labels.insert("job-name".to_string(), job_name.to_string());
        if let Some(uid) = self.metadata.uid.as_ref() {
            labels.insert("controller-uid".to_string(), uid.clone());
        }

        // Copy annotations from job_template.metadata
        let mut annotations = job_template_metadata
            .annotations
            .clone()
            .unwrap_or_default();

        // Add scheduled timestamp annotation (RFC3339 format)
        annotations.insert(
            "batch.kubernetes.io/cronjob-scheduled-timestamp".to_string(),
            Utc::now().to_rfc3339(),
        );

        // Set up owner reference to the CronJob
        let mut owner_references = Vec::new();
        if let Some(uid) = self.metadata.uid.as_ref() {
            owner_references.push(OwnerReference {
                api_version: "batch/v1".to_string(),
                kind: "CronJob".to_string(),
                name: self.name_any().clone(),
                uid: uid.clone(),
                controller: Some(true),
                block_owner_deletion: Some(false), // Jobs are ephemeral, don't block CronJob deletion
            });
        }

        log::debug!(
            "Creating manual job {} from CronJob {}",
            job_name,
            self.name_any()
        );

        Ok(Job {
            metadata: ObjectMeta {
                name: Some(job_name.to_string()),
                namespace: Some(self.namespace().unwrap_or_default()),
                labels: Some(labels),
                annotations: Some(annotations),
                owner_references: Some(owner_references),
                ..Default::default()
            },
            spec: Some(job_template_spec),
            status: None,
        })
    }
}
