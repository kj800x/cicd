//! The one place a user-initiated deploy action runs.
//!
//! The web handler and the MCP tools used to carry identical copies of this
//! sequence: resolve the requested action against the config's current
//! state, turn it into a [`DeployAction`], execute it against the cluster,
//! mirror the result to GitHub, and record a deploy event. Keeping it here
//! means anything that must apply to every deploy (a gate, an audit record)
//! is written once.

use kube::Client;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;

use crate::{
    crab_ext::Octocrabs,
    db::deploy_event::DeployEvent,
    error::AppResult,
    kubernetes::{deploy_handlers::DeployAction, repo::DeploymentState, DeployConfig},
    web::Action,
};

/// Turn a requested [`Action`] plus the state it resolves to into the
/// concrete [`DeployAction`] to execute.
pub fn to_deploy_action(action: &Action, name: &str, state: DeploymentState) -> DeployAction {
    match action {
        Action::DeployLatest
        | Action::DeployBranch { .. }
        | Action::DeployCommit { .. }
        | Action::Undeploy => match state {
            DeploymentState::DeployedWithArtifact { artifact, config } => DeployAction::Deploy {
                name: name.to_string(),
                artifact: Some(artifact),
                config,
            },
            DeploymentState::DeployedOnlyConfig { config } => DeployAction::Deploy {
                name: name.to_string(),
                artifact: None,
                config,
            },
            DeploymentState::Undeployed => DeployAction::Undeploy {
                name: name.to_string(),
            },
        },
        Action::Bounce => DeployAction::Bounce {
            name: name.to_string(),
        },
        Action::ExecuteJob => DeployAction::ExecuteJob {
            name: name.to_string(),
        },
        Action::ToggleAutodeploy => DeployAction::ToggleAutodeploy {
            name: name.to_string(),
        },
    }
}

/// Run `action` against `config`. On success returns the [`DeployAction`]
/// that was executed, so callers can label metrics and messages.
///
/// The GitHub deployment mirror and the deploy event are bookkeeping: their
/// failures are logged, not returned, because the cluster change has already
/// happened by then and reporting it as a failure would mislead.
pub async fn run_action(
    action: &Action,
    config: &DeployConfig,
    client: &Client,
    octocrabs: &Octocrabs,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> AppResult<DeployAction> {
    let name = kube::ResourceExt::name_any(config);
    let state = DeploymentState::from_action(action, config, conn)?;
    let deploy_action = to_deploy_action(action, &name, state);

    deploy_action
        .execute(client, octocrabs, config.config_repository())
        .await?;

    // Best-effort: mirror the new state into the GitHub Deployments API.
    crate::github_deployments::report_deploy_action(octocrabs, config, &deploy_action).await;

    match DeployEvent::from_user_deploy_action(&deploy_action, conn, config) {
        Ok(Some(event)) => {
            if let Err(e) = event.insert(conn) {
                log::error!("Failed to insert deploy event for {}: {}", name, e);
            }
        }
        Ok(None) => {}
        Err(e) => log::error!("Failed to build deploy event for {}: {}", name, e),
    }

    Ok(deploy_action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::repo::ShaMaybeBranch;

    fn sha(s: &str) -> ShaMaybeBranch {
        ShaMaybeBranch {
            sha: s.into(),
            branch: Some("master".into()),
        }
    }

    #[test]
    fn deploy_actions_follow_the_resolved_state() {
        let with = DeploymentState::DeployedWithArtifact {
            artifact: sha("a"),
            config: sha("c"),
        };
        assert!(matches!(
            to_deploy_action(&Action::DeployLatest, "x", with),
            DeployAction::Deploy {
                artifact: Some(_),
                ..
            }
        ));
        let only = DeploymentState::DeployedOnlyConfig { config: sha("c") };
        assert!(matches!(
            to_deploy_action(&Action::DeployBranch { branch: "b".into() }, "x", only),
            DeployAction::Deploy { artifact: None, .. }
        ));
        assert!(matches!(
            to_deploy_action(&Action::Undeploy, "x", DeploymentState::Undeployed),
            DeployAction::Undeploy { .. }
        ));
    }

    #[test]
    fn non_deploy_actions_ignore_state() {
        assert!(matches!(
            to_deploy_action(&Action::Bounce, "x", DeploymentState::Undeployed),
            DeployAction::Bounce { .. }
        ));
        assert!(matches!(
            to_deploy_action(&Action::ExecuteJob, "x", DeploymentState::Undeployed),
            DeployAction::ExecuteJob { .. }
        ));
        assert!(matches!(
            to_deploy_action(&Action::ToggleAutodeploy, "x", DeploymentState::Undeployed),
            DeployAction::ToggleAutodeploy { .. }
        ));
    }
}
