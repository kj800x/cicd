//! Backfill and guard for the artifact -> parameters migration.
//!
//! While configs are migrated, a DeployConfig may carry only the legacy
//! `artifact` fields, only the `parameters` maps, or both. [`backfill_parameters`]
//! copies legacy into new where new is missing, so every config converges
//! without a manual script. [`assert_parameters_present`] is the fail-closed
//! guard: a config that still has legacy fields without the matching
//! parameters entry is refused rather than reconciled, because reconciling it
//! with the wrong reading could prune its running workloads.
//!
//! Both are removed once every config is migrated and the legacy fields are
//! dropped from the CRD.

use kube::{
    api::{Api, Patch, PatchParams},
    Client, ResourceExt,
};

use crate::{
    error::{AppError, AppResult},
    kubernetes::{
        api::{get_deploy_config, update_deploy_config_status},
        deploy_config::DeployConfig,
        parameters::{ParameterSource, SHA_PARAMETER},
        DeployConfigStatusBuilder,
    },
};

/// Which parts of a config still have legacy fields without a parameters entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LegacyOnly {
    pub spec: bool,
    pub status: bool,
}

impl LegacyOnly {
    pub fn any(self) -> bool {
        self.spec || self.status
    }
}

/// Inspect a config for legacy fields that have no parameters counterpart.
pub fn legacy_only(dc: &DeployConfig) -> LegacyOnly {
    let spec = &dc.spec.spec;
    let status = dc.status.as_ref();
    LegacyOnly {
        spec: spec.artifact.is_some() && !spec.parameters.contains_key(SHA_PARAMETER),
        status: status
            .is_some_and(|s| s.artifact.is_some() && !s.parameters.contains_key(SHA_PARAMETER)),
    }
}

/// Copy legacy fields into the parameters maps where they are missing.
///
/// Returns `true` if a patch was sent, in which case the caller should
/// requeue and read the object again rather than continue with the stale
/// copy. Verifies that the patched fields actually persisted: if the CRD
/// does not declare `parameters` yet, the API server prunes them on write,
/// and this would otherwise loop forever while the config stays unmigrated.
pub async fn backfill_parameters(client: &Client, dc: &DeployConfig) -> AppResult<bool> {
    let needs = legacy_only(dc);
    if !needs.any() {
        return Ok(false);
    }

    let ns = dc.namespace().unwrap_or_else(|| "default".to_string());
    let name = dc.name_any();
    log::info!(
        "Backfilling parameters for DeployConfig {}/{} (spec: {}, status: {})",
        ns,
        name,
        needs.spec,
        needs.status
    );

    if needs.spec {
        if let Some(artifact) = dc.spec.spec.artifact.clone() {
            let api: Api<DeployConfig> = Api::namespaced(client.clone(), &ns);
            let patch = serde_json::json!({
                "spec": { "parameters": ParameterSource::sha_map(Some(artifact)) }
            });
            api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await?;
        }
    }

    if needs.status {
        if let Some(artifact) = dc.status.as_ref().and_then(|s| s.artifact.clone()) {
            // The builder now writes only the parameters entry; the legacy
            // field is left in place until the CRD drops it.
            update_deploy_config_status(
                client,
                &ns,
                &name,
                DeployConfigStatusBuilder::default().with_artifact(Some(artifact)),
            )
            .await?;
        }
    }

    let after = get_deploy_config(client, &name).await?.ok_or_else(|| {
        AppError::NotFound(format!("DeployConfig {name} vanished during backfill"))
    })?;
    let still = legacy_only(&after);
    if still.any() {
        return Err(AppError::Internal(format!(
            "Backfilled parameters for DeployConfig {ns}/{name} did not persist \
             (spec: {}, status: {}). The CRD probably does not declare `parameters` yet; \
             apply kubernetes/deploy-config-crd.yaml and the next reconcile will retry.",
            still.spec, still.status
        )));
    }

    Ok(true)
}

/// Fail closed: refuse to reconcile a config whose legacy fields have no
/// parameters counterpart. Returning an error requeues the config without
/// applying or pruning anything.
pub fn assert_parameters_present(dc: &DeployConfig) -> AppResult<()> {
    let needs = legacy_only(dc);
    if needs.any() {
        return Err(AppError::Internal(format!(
            "DeployConfig {} has legacy artifact fields without parameters (spec: {}, status: {}); \
             refusing to reconcile until it is backfilled. \
             See kubernetes/migration/migration-status.sh.",
            dc.name_any(),
            needs.spec,
            needs.status
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::deploy_config::{
        DeployConfigSpec, DeployConfigSpecFields, DeployConfigStatus,
    };
    use crate::kubernetes::{
        parameters::ParameterValue,
        repo::{RepositoryBranch, ShaMaybeBranch},
        Repository,
    };

    fn dc(
        spec_legacy: bool,
        spec_new: bool,
        status_legacy: bool,
        status_new: bool,
    ) -> DeployConfig {
        let rb = RepositoryBranch {
            owner: "o".into(),
            repo: "r".into(),
            branch: "master".into(),
        };
        let sha = ShaMaybeBranch {
            sha: "abc".into(),
            branch: Some("master".into()),
        };
        let mut dc = DeployConfig::new(
            "test",
            DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    team: "t".into(),
                    kind: "service".into(),
                    artifact: spec_legacy.then(|| rb.clone()),
                    parameters: if spec_new {
                        ParameterSource::sha_map(Some(rb))
                    } else {
                        Default::default()
                    },
                    config: Repository {
                        owner: "o".into(),
                        repo: "cfg".into(),
                    },
                    specs: vec![],
                },
            },
        );
        let mut status = DeployConfigStatus::default();
        if status_legacy {
            status.artifact = Some(sha.clone());
        }
        if status_new {
            status
                .parameters
                .insert(SHA_PARAMETER.into(), ParameterValue::from(sha));
        }
        dc.status = Some(status);
        dc
    }

    #[test]
    fn legacy_only_detects_each_side() {
        assert_eq!(
            legacy_only(&dc(true, false, true, false)),
            LegacyOnly {
                spec: true,
                status: true
            }
        );
        assert_eq!(
            legacy_only(&dc(true, true, true, false)),
            LegacyOnly {
                spec: false,
                status: true
            }
        );
        assert_eq!(
            legacy_only(&dc(true, true, true, true)),
            LegacyOnly::default()
        );
        // New-only and config-only configs have nothing to migrate.
        assert_eq!(
            legacy_only(&dc(false, true, false, true)),
            LegacyOnly::default()
        );
        assert_eq!(
            legacy_only(&dc(false, false, false, false)),
            LegacyOnly::default()
        );
    }

    #[test]
    fn guard_refuses_legacy_only_and_passes_otherwise() {
        assert!(assert_parameters_present(&dc(true, false, true, false)).is_err());
        assert!(assert_parameters_present(&dc(true, true, true, false)).is_err());
        assert!(assert_parameters_present(&dc(true, true, true, true)).is_ok());
        assert!(assert_parameters_present(&dc(false, true, false, true)).is_ok());
        assert!(assert_parameters_present(&dc(false, false, false, false)).is_ok());
    }
}
