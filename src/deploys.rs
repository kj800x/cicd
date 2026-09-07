//! The one place a user-initiated deploy action runs.
//!
//! The web handler and the MCP tools used to carry identical copies of this
//! sequence: resolve the requested action against the config's current
//! state, turn it into a [`DeployAction`], execute it against the cluster,
//! mirror the result to GitHub, and record a revision. Keeping it here
//! means anything that must apply to every deploy (a gate, an audit record)
//! is written once.

use kube::Client;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;

use std::collections::HashMap;

use crate::{
    crab_ext::Octocrabs,
    db::{
        blocker::Blocker,
        revision::{NewRevision, Revision},
    },
    error::{AppError, AppResult},
    kubernetes::{
        api::patch_deploy_config_selection,
        deploy_handlers::DeployAction,
        parameters::SHA_PARAMETER,
        repo::DeploymentState,
        selections::{Durability, Selection},
        DeployConfig,
    },
    web::Action,
};

/// What the person said about an override they are making: how long it is
/// meant to last and why. Absent durability falls back to the action's
/// default (temporary for a branch, standing for a pin).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionIntent {
    pub durability: Option<Durability>,
    pub note: Option<String>,
    pub by: Option<String>,
}

impl SelectionIntent {
    pub fn from_form(form: &HashMap<String, String>) -> Self {
        let durability = match form.get("durability").map(|d| d.trim()) {
            Some("temporary") => Some(Durability::Temporary),
            Some("standing") => Some(Durability::Standing),
            _ => None,
        };
        SelectionIntent {
            durability,
            note: form.get("note").cloned(),
            by: form.get("by").cloned(),
        }
    }
}

/// The selection change an action implies for the `SHA` parameter.
/// `None` means leave it alone; `Some(None)` removes it; `Some(Some(s))`
/// sets it. A branch deploy of the default branch is a clear, not an
/// override of the default with itself.
pub fn selection_change(
    action: &Action,
    default_branch: Option<&str>,
    intent: &SelectionIntent,
) -> Option<Option<Selection>> {
    match action {
        Action::DeployBranch { branch } if Some(branch.as_str()) == default_branch => Some(None),
        Action::DeployBranch { branch } => Some(Some(
            Selection::track(branch, intent.durability.unwrap_or(Durability::Temporary))
                .with_note(intent.note.as_deref(), intent.by.as_deref()),
        )),
        Action::DeployCommit { sha } => Some(Some(
            Selection::pin(sha, intent.durability.unwrap_or(Durability::Standing))
                .with_note(intent.note.as_deref(), intent.by.as_deref()),
        )),
        Action::ClearSelection => Some(None),
        // Latest honours the selection; rollback and undeploy leave intent
        // alone on purpose; the rest do not touch versions at all.
        Action::DeployLatest
        | Action::Rollback { .. }
        | Action::Undeploy
        | Action::Bounce
        | Action::ExecuteJob
        | Action::ToggleAutodeploy => None,
    }
}

/// Refuse a deploy while the config has an active blocker.
///
/// Only deploys are gated. Undeploy stays available as the emergency exit,
/// and bounce or job execution do not change what is deployed. Rollback is
/// not gated either: it is the recovery action during an incident hold, it
/// only ever moves to a revision that was deployed before, and it adds a
/// blocker of its own. Every active blocker's reason is included, so the
/// person sees what they would have to clear.
pub fn check_blockers(
    conn: &PooledConnection<SqliteConnectionManager>,
    action: &Action,
    config_name: &str,
) -> AppResult<()> {
    if !action.is_deploy() && !action.is_clear_selection() {
        return Ok(());
    }
    let active = Blocker::active_for(conn, config_name)?;
    if active.is_empty() {
        return Ok(());
    }
    let reasons: Vec<String> = active
        .iter()
        .map(|b| format!("{} (by {})", b.reason, b.created_by))
        .collect();
    Err(AppError::Blocked(format!(
        "{} is held by {} blocker{}: {}. Clear {} before deploying.",
        config_name,
        active.len(),
        if active.len() == 1 { "" } else { "s" },
        reasons.join("; "),
        if active.len() == 1 { "it" } else { "them" },
    )))
}

/// Turn a requested [`Action`] plus the state it resolves to into the
/// concrete [`DeployAction`] to execute.
pub fn to_deploy_action(action: &Action, name: &str, state: DeploymentState) -> DeployAction {
    match action {
        Action::DeployLatest
        | Action::DeployBranch { .. }
        | Action::DeployCommit { .. }
        | Action::Rollback { .. }
        | Action::ClearSelection
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
/// that was executed, so callers can label metrics and messages. `actor`
/// says where the action came from (`web`, `mcp`) and is recorded on the
/// revision.
///
/// The GitHub deployment mirror and the revision are bookkeeping: their
/// failures are logged, not returned, because the cluster change has already
/// happened by then and reporting it as a failure would mislead.
pub async fn run_action(
    action: &Action,
    config: &DeployConfig,
    client: &Client,
    octocrabs: &Octocrabs,
    conn: &PooledConnection<SqliteConnectionManager>,
    actor: &str,
    intent: &SelectionIntent,
) -> AppResult<DeployAction> {
    let name = kube::ResourceExt::name_any(config);
    check_blockers(conn, action, &name)?;
    let state = DeploymentState::from_action(action, config, conn)?;
    let deploy_action = to_deploy_action(action, &name, state);

    deploy_action
        .execute(client, octocrabs, config.config_repository())
        .await?;

    // Record the intent behind the deploy. Bookkeeping like the rest: the
    // deploy has happened, and a missing selection only means the next
    // "latest" derives it from what is deployed.
    let default_branch = config.artifact_repository().map(|r| r.branch);
    if let Some(change) = selection_change(action, default_branch.as_deref(), intent) {
        let ns = kube::ResourceExt::namespace(config).unwrap_or_else(|| "default".to_string());
        if let Err(e) =
            patch_deploy_config_selection(client, &ns, &name, SHA_PARAMETER, change.as_ref()).await
        {
            log::error!("Failed to record selection for {}: {}", name, e);
        }
    }

    // Best-effort: mirror the new state into the GitHub Deployments API.
    crate::github_deployments::report_deploy_action(octocrabs, config, &deploy_action).await;

    if let Some(mut new) = NewRevision::from_deploy_action(&deploy_action, config, conn, actor) {
        if let Action::Rollback { revision } = action {
            new.reason = Some(format!("rollback to revision {revision}"));
        }
        match Revision::record(conn, new) {
            Ok(rev) => log::info!("Recorded revision {} for {} ({})", rev.id, name, rev.action),
            Err(e) => log::error!("Failed to record revision for {}: {}", name, e),
        }
    }

    // A rollback holds the config until someone decides the incident is over.
    // Selections are untouched, so clearing the blocker and deploying latest
    // resumes exactly what was being tracked before.
    if let Action::Rollback { revision } = action {
        match Blocker::create(
            conn,
            &name,
            &format!(
                "Rolled back to revision {revision}; clear when it is safe to move forward again"
            ),
            actor,
        ) {
            Ok(b) => log::info!("Blocker {} added on {} after rollback", b.id, name),
            Err(e) => log::error!("Failed to add rollback blocker on {}: {}", name, e),
        }
    }

    Ok(deploy_action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::migrated_memory_pool;
    use crate::kubernetes::repo::ShaMaybeBranch;

    #[test]
    fn blockers_refuse_deploys_only() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        assert!(check_blockers(&conn, &Action::DeployLatest, "site").is_ok());

        let b = Blocker::create(&conn, "site", "incident 42", "kevin")?;
        match check_blockers(&conn, &Action::DeployLatest, "site") {
            Err(AppError::Blocked(message)) => assert!(message.contains("incident 42")),
            other => panic!("expected Blocked, got {other:?}"),
        }
        assert!(
            check_blockers(&conn, &Action::DeployBranch { branch: "b".into() }, "site").is_err()
        );
        assert!(check_blockers(&conn, &Action::DeployCommit { sha: "s".into() }, "site").is_err());

        // Not gated: the emergency exit and the non-version actions.
        assert!(check_blockers(&conn, &Action::Undeploy, "site").is_ok());
        assert!(check_blockers(&conn, &Action::Bounce, "site").is_ok());
        assert!(check_blockers(&conn, &Action::ExecuteJob, "site").is_ok());
        assert!(check_blockers(&conn, &Action::ToggleAutodeploy, "site").is_ok());
        // Other configs are unaffected.
        assert!(check_blockers(&conn, &Action::DeployLatest, "other").is_ok());

        Blocker::clear(&conn, b.id, "kevin")?;
        assert!(check_blockers(&conn, &Action::DeployLatest, "site").is_ok());
        Ok(())
    }

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
    fn rollback_maps_like_a_deploy_and_is_not_gated() -> AppResult<()> {
        let with = DeploymentState::DeployedWithArtifact {
            artifact: sha("a"),
            config: sha("c"),
        };
        assert!(matches!(
            to_deploy_action(&Action::Rollback { revision: 7 }, "x", with),
            DeployAction::Deploy {
                artifact: Some(_),
                ..
            }
        ));
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        Blocker::create(&conn, "site", "incident", "kevin")?;
        assert!(check_blockers(&conn, &Action::Rollback { revision: 7 }, "site").is_ok());
        Ok(())
    }

    #[test]
    fn selection_changes_follow_the_action() {
        let intent = SelectionIntent::default();
        let branch = Action::DeployBranch {
            branch: "feature".into(),
        };
        match selection_change(&branch, Some("master"), &intent) {
            Some(Some(s)) => {
                assert_eq!(s.track.as_ref().map(|t| t.branch.as_str()), Some("feature"));
                assert_eq!(
                    s.durability,
                    Durability::Temporary,
                    "branches default to temporary"
                );
            }
            other => panic!("expected a track selection, got {other:?}"),
        }
        let default_branch = Action::DeployBranch {
            branch: "master".into(),
        };
        assert_eq!(
            selection_change(&default_branch, Some("master"), &intent),
            Some(None),
            "deploying the default branch clears the override"
        );
        let pin = Action::DeployCommit { sha: "abc".into() };
        let noted = SelectionIntent {
            durability: Some(Durability::Temporary),
            note: Some("testing".into()),
            by: Some("kevin".into()),
        };
        match selection_change(&pin, Some("master"), &noted) {
            Some(Some(s)) => {
                assert_eq!(s.pin.as_ref().map(|p| p.value.as_str()), Some("abc"));
                assert_eq!(
                    s.durability,
                    Durability::Temporary,
                    "explicit durability wins"
                );
                assert_eq!(s.note.as_deref(), Some("testing"));
                assert_eq!(s.by.as_deref(), Some("kevin"));
            }
            other => panic!("expected a pin selection, got {other:?}"),
        }
        match selection_change(&pin, Some("master"), &intent) {
            Some(Some(s)) => assert_eq!(
                s.durability,
                Durability::Standing,
                "pins default to standing"
            ),
            other => panic!("expected a pin selection, got {other:?}"),
        }
        assert_eq!(
            selection_change(&Action::ClearSelection, Some("master"), &intent),
            Some(None)
        );
        for untouched in [
            Action::DeployLatest,
            Action::Rollback { revision: 1 },
            Action::Undeploy,
            Action::Bounce,
        ] {
            assert_eq!(selection_change(&untouched, Some("master"), &intent), None);
        }
    }

    #[test]
    fn clear_selection_is_gated_and_maps_like_a_deploy() -> AppResult<()> {
        let only = DeploymentState::DeployedOnlyConfig { config: sha("c") };
        assert!(matches!(
            to_deploy_action(&Action::ClearSelection, "x", only),
            DeployAction::Deploy { artifact: None, .. }
        ));
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        Blocker::create(&conn, "site", "incident", "kevin")?;
        assert!(check_blockers(&conn, &Action::ClearSelection, "site").is_err());
        Ok(())
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
