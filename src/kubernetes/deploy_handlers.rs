use chrono::Utc;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{ObjectMeta, PostParams};
use kube::{Api, Client, ResourceExt};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::crab_ext::Octocrabs;
use crate::kubernetes::api::{delete_deploy_config, get_deploy_config};
use crate::kubernetes::bounce;
use crate::kubernetes::cr_writers::{self, DeployedStatus};
use crate::kubernetes::parameters::{ParameterValue, SHA_PARAMETER};
use crate::webhooks::config_sync::fetch_deploy_config_by_sha;
use crate::{
    crab_ext::IRepo,
    error::{AppError, AppResult},
    kubernetes::{parameters::ParameterValues, repo::ShaMaybeBranch},
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
        /// Resolved values for every parameter other than `SHA`.
        values: ParameterValues,
    },
    Undeploy {
        name: String,
    },
    ToggleAutodeploy {
        name: String,
    },
}

impl DeployAction {
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
                values,
            } => {
                log::debug!(
                    "Deploy action: name={}, artifact={:?}, config={:?}, values={:?}",
                    name,
                    artifact,
                    config,
                    values
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

                let namespace = desired_config.namespace().unwrap_or_default();
                cr_writers::set_specs(client, &namespace, name, &desired_config.spec.spec.specs)
                    .await?;

                let mut parameters = values.clone();
                if let Some(artifact) = artifact {
                    parameters.insert(
                        SHA_PARAMETER.to_string(),
                        ParameterValue::from(artifact.clone()),
                    );
                }
                cr_writers::set_deployed_status(
                    client,
                    &namespace,
                    name,
                    &DeployedStatus {
                        parameters,
                        config: Some(config.clone()),
                    },
                )
                .await?;

                Ok(())
            }

            DeployAction::Undeploy { name } => {
                let current_config = get_deploy_config(client, name)
                    .await?
                    .ok_or(AppError::NotFound("Current config not found".to_owned()))?;

                let namespace = current_config.namespace().unwrap_or_default();
                cr_writers::set_specs(client, &namespace, name, &[]).await?;
                cr_writers::set_deployed_status(
                    client,
                    &namespace,
                    name,
                    &DeployedStatus::undeployed(),
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

                cr_writers::set_autodeploy(client, &namespace, name, !current_autodeploy).await?;

                Ok(())
            }

            DeployAction::Bounce { name } => {
                let current_config = get_deploy_config(client, name)
                    .await?
                    .ok_or(AppError::NotFound("Current config not found".to_owned()))?;
                let namespace = current_config.namespace().unwrap_or_default();

                let mut restarted = 0;
                let mut skipped = Vec::new();
                for (kind, workload) in current_config.bounce_targets() {
                    let live = match kind.as_str() {
                        "Deployment" => {
                            live_json::<Deployment>(client, &namespace, &workload).await?
                        }
                        "StatefulSet" => {
                            live_json::<StatefulSet>(client, &namespace, &workload).await?
                        }
                        "DaemonSet" => {
                            live_json::<DaemonSet>(client, &namespace, &workload).await?
                        }
                        _ => continue,
                    };
                    match bounce::plan(&kind, live.as_ref()) {
                        bounce::Outcome::Skip(why) => {
                            log::info!("Not restarting {kind} {namespace}/{workload}: {why}");
                            skipped.push(format!("{kind} {workload}: {why}"));
                        }
                        bounce::Outcome::Restart { note } => {
                            log::info!(
                                "Restarting {kind} {namespace}/{workload}{}",
                                note.map(|n| format!(" ({n})")).unwrap_or_default()
                            );
                            match kind.as_str() {
                                "Deployment" => {
                                    restart::<Deployment>(client, &namespace, &workload).await?
                                }
                                "StatefulSet" => {
                                    restart::<StatefulSet>(client, &namespace, &workload).await?
                                }
                                _ => restart::<DaemonSet>(client, &namespace, &workload).await?,
                            }
                            restarted += 1;
                        }
                    }
                }

                // Reporting success when nothing restarted would hide an
                // OnDelete StatefulSet or a scaled-down config.
                if restarted == 0 {
                    return Err(AppError::InvalidInput(if skipped.is_empty() {
                        format!("{name} has no workloads to restart")
                    } else {
                        format!("Nothing restarted: {}", skipped.join("; "))
                    }));
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

/// The live workload `name` as JSON, for [`bounce::plan`].
async fn live_json<K>(
    client: &Client,
    namespace: &str,
    name: &str,
) -> AppResult<Option<serde_json::Value>>
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope, DynamicType = ()>
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + serde::Serialize,
{
    let api: Api<K> = Api::namespaced(client.clone(), namespace);
    let live = api.get_opt(name).await?;
    Ok(live.and_then(|obj| serde_json::to_value(obj).ok()))
}

/// `kubectl rollout restart` for one workload.
async fn restart<K>(client: &Client, namespace: &str, name: &str) -> AppResult<()>
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope, DynamicType = ()>
        + kube::core::util::Restart
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned,
{
    let api: Api<K> = Api::namespaced(client.clone(), namespace);
    api.restart(name).await?;
    Ok(())
}
