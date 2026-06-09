//! Mirrors our deployment state into the GitHub Deployments API.
//!
//! We perform deployments ourselves; GitHub's Deployments API is used purely as a
//! status mirror. GitHub has no first-class "deployment name" — the grouping key it
//! actually exposes is the `environment` string, so each DeployConfig maps to one or
//! two environments:
//!
//! * the artifact ref under environment `{name}` on the artifact repo, and
//! * the config ref under environment `{name}-config` on the config repo.
//!
//! The distinct environment names are load-bearing: when the artifact and config
//! repos are the same, a shared environment would make each `success` status
//! auto-inactivate the other (GitHub keys auto-inactivation on repo + environment).
//!
//! All reporting is best-effort: failures are logged and never abort a deploy that
//! has already succeeded on our side.

use serde_json::json;

use crate::crab_ext::{IRepo, OctocrabExt, Octocrabs};
use crate::error::{AppError, AppResult};
use crate::kubernetes::deploy_handlers::DeployAction;
use crate::kubernetes::DeployConfig;

const CONFIG_ENV_SUFFIX: &str = "-config";
const DESCRIPTION: &str = "Deployed by cicd-controller";

/// Reflect the outcome of a deploy action into the GitHub Deployments API.
///
/// Best-effort: any failure is logged and swallowed.
pub async fn report_deploy_action(
    octocrabs: &Octocrabs,
    config: &DeployConfig,
    action: &DeployAction,
) {
    match action {
        DeployAction::Deploy {
            name,
            artifact,
            config: config_ref,
        } => {
            // Artifact deployment: environment `{name}` on the artifact repo.
            if let (Some(artifact_ref), Some(artifact_repo)) =
                (artifact, config.artifact_repository())
            {
                let repo = artifact_repo.into_repo();
                report_success(octocrabs, &repo, &artifact_ref.sha, name).await;
            }

            // Config deployment: environment `{name}-config` on the config repo.
            let config_repo = config.config_repository();
            let config_env = format!("{name}{CONFIG_ENV_SUFFIX}");
            report_success(octocrabs, &config_repo, &config_ref.sha, &config_env).await;
        }
        DeployAction::Undeploy { name } => {
            // Tear down both environments by marking their latest deployment inactive.
            if let Some(artifact_repo) = config.artifact_repository() {
                let repo = artifact_repo.into_repo();
                report_inactive(octocrabs, &repo, name).await;
            }
            let config_repo = config.config_repository();
            report_inactive(
                octocrabs,
                &config_repo,
                &format!("{name}{CONFIG_ENV_SUFFIX}"),
            )
            .await;
        }
        // Bounce / ExecuteJob / ToggleAutodeploy don't change which SHA is live.
        DeployAction::Bounce { .. }
        | DeployAction::ExecuteJob { .. }
        | DeployAction::ToggleAutodeploy { .. } => {}
    }
}

/// Create a deployment for `sha` in `environment` and mark it `success`.
async fn report_success(octocrabs: &Octocrabs, repo: &impl IRepo, sha: &str, environment: &str) {
    let Some(crab) = octocrabs.crab_for(repo).await else {
        log::warn!(
            "GitHub deployment skipped: no token can access {}/{}",
            repo.owner(),
            repo.repo()
        );
        return;
    };

    if let Err(e) = create_success(crab, repo, sha, environment).await {
        log::warn!(
            "GitHub deployment report failed for {}/{} env={}: {}",
            repo.owner(),
            repo.repo(),
            environment,
            e
        );
    }
}

async fn create_success(
    crab: &octocrab::Octocrab,
    repo: &impl IRepo,
    sha: &str,
    environment: &str,
) -> AppResult<()> {
    let deployment: serde_json::Value = crab
        .post(
            format!("/repos/{}/{}/deployments", repo.owner(), repo.repo()),
            Some(&json!({
                "ref": sha,
                "environment": environment,
                "auto_merge": false,
                // We resolve and gate SHAs ourselves, so don't let GitHub re-gate on
                // its own status checks (which would 409 on a not-yet-green commit).
                "required_contexts": [],
                "description": DESCRIPTION,
                "production_environment": true,
            })),
        )
        .await?;

    let id = deployment
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            AppError::Internal("GitHub deployment response missing numeric id".to_string())
        })?;

    let _: serde_json::Value = crab
        .post(
            format!(
                "/repos/{}/{}/deployments/{}/statuses",
                repo.owner(),
                repo.repo(),
                id
            ),
            Some(&json!({
                "state": "success",
                "environment": environment,
                "description": DESCRIPTION,
            })),
        )
        .await?;

    Ok(())
}

/// Mark the most recent deployment in `environment` as `inactive`.
async fn report_inactive(octocrabs: &Octocrabs, repo: &impl IRepo, environment: &str) {
    let Some(crab) = octocrabs.crab_for(repo).await else {
        log::warn!(
            "GitHub deployment deactivate skipped: no token can access {}/{}",
            repo.owner(),
            repo.repo()
        );
        return;
    };

    if let Err(e) = set_inactive(crab, repo, environment).await {
        log::warn!(
            "GitHub deployment deactivate failed for {}/{} env={}: {}",
            repo.owner(),
            repo.repo(),
            environment,
            e
        );
    }
}

async fn set_inactive(
    crab: &octocrab::Octocrab,
    repo: &impl IRepo,
    environment: &str,
) -> AppResult<()> {
    // List deployments for the environment; GitHub returns most-recent first.
    let deployments: serde_json::Value = crab
        .get(
            format!(
                "/repos/{}/{}/deployments?environment={}",
                repo.owner(),
                repo.repo(),
                environment
            ),
            None::<&()>,
        )
        .await?;

    let Some(id) = deployments
        .as_array()
        .and_then(|d| d.first())
        .and_then(|d| d.get("id"))
        .and_then(serde_json::Value::as_u64)
    else {
        // Nothing to deactivate — the environment never had a deployment.
        return Ok(());
    };

    let _: serde_json::Value = crab
        .post(
            format!(
                "/repos/{}/{}/deployments/{}/statuses",
                repo.owner(),
                repo.repo(),
                id
            ),
            Some(&json!({
                "state": "inactive",
                "environment": environment,
            })),
        )
        .await?;

    Ok(())
}
