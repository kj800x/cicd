//! The one place a user-initiated deploy action runs.
//!
//! The web handler and the MCP tools used to carry identical copies of this
//! sequence: resolve the requested action against the config's current
//! state, turn it into a [`DeployAction`], execute it against the cluster,
//! mirror the result to GitHub, and record a revision. Keeping it here
//! means anything that must apply to every deploy (a gate, an audit record)
//! is written once.

use kube::Client;
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;

use std::collections::HashMap;

use crate::{
    crab_ext::Octocrabs,
    db::{
        blocker::Blocker,
        git_commit::GitCommit,
        git_repo::GitRepo,
        revision::{NewRevision, Revision},
    },
    error::{AppError, AppResult},
    kubernetes::{
        cr_writers,
        deploy_handlers::DeployAction,
        parameters::{ParameterValue, ParameterValues, SHA_PARAMETER},
        patches::ManifestPatch,
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

/// The selection change an action implies: which parameter, and either
/// `None` to remove its selection or `Some(s)` to set it. A branch deploy
/// of the default branch is a clear, not an override of the default with
/// itself.
pub fn selection_change(
    action: &Action,
    default_branch: Option<&str>,
    intent: &SelectionIntent,
) -> Option<(String, Option<Selection>)> {
    let sha = |change: Option<Selection>| Some((SHA_PARAMETER.to_string(), change));
    match action {
        Action::DeployBranch { branch } if Some(branch.as_str()) == default_branch => sha(None),
        Action::DeployBranch { branch } => sha(Some(
            Selection::track(branch, intent.durability.unwrap_or(Durability::Temporary))
                .with_note(intent.note.as_deref(), intent.by.as_deref()),
        )),
        Action::DeployCommit { sha: value } => sha(Some(
            Selection::pin(value, intent.durability.unwrap_or(Durability::Standing))
                .with_note(intent.note.as_deref(), intent.by.as_deref()),
        )),
        Action::ClearSelection => sha(None),
        Action::SetParameter { parameter, value } => Some((
            parameter.clone(),
            value.as_ref().map(|v| {
                Selection::pin(v, intent.durability.unwrap_or(Durability::Standing))
                    .with_note(intent.note.as_deref(), intent.by.as_deref())
            }),
        )),
        // Latest honours the selection; rollback and undeploy leave intent
        // alone on purpose; ending a temporary deployment computes its
        // changes from the config (see [`temporary_changes`]); the rest do
        // not touch versions at all.
        Action::DeployLatest
        | Action::EndTemporary
        | Action::Rollback { .. }
        | Action::Undeploy
        | Action::Bounce
        | Action::ExecuteJob
        | Action::ToggleAutodeploy => None,
    }
}

/// Everything temporary on a config: the selections to clear and, when any
/// patch is temporary, the patch list with those removed. This is what
/// "End temporary deployment" undoes in one step.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TemporaryChanges {
    /// Parameters whose selection is temporary, in name order.
    pub selections: Vec<String>,
    /// The patch list without its temporary patches, if there were any.
    pub patches: Option<Vec<ManifestPatch>>,
    removed_patches: usize,
}

impl TemporaryChanges {
    pub fn is_empty(&self) -> bool {
        self.selections.is_empty() && self.patches.is_none()
    }

    /// What ending would do, in the present tense, such as
    /// `clear SHA, GREETING; remove 2 patches`. Shown before the action.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.selections.is_empty() {
            parts.push(format!("clear {}", self.selections.join(", ")));
        }
        if self.removed_patches > 0 {
            parts.push(format!(
                "remove {} patch{}",
                self.removed_patches,
                if self.removed_patches == 1 { "" } else { "es" }
            ));
        }
        parts.join("; ")
    }

    /// A one-line summary for the revision, such as
    /// `ended temporary deployment: cleared SHA, GREETING; removed 2 patches`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.selections.is_empty() {
            parts.push(format!("cleared {}", self.selections.join(", ")));
        }
        if self.removed_patches > 0 {
            parts.push(format!(
                "removed {} patch{}",
                self.removed_patches,
                if self.removed_patches == 1 { "" } else { "es" }
            ));
        }
        format!("ended temporary deployment: {}", parts.join("; "))
    }
}

/// What ending a temporary deployment would clear on `config`. Standing
/// overrides and standing patches are not temporary and stay.
pub fn temporary_changes(config: &DeployConfig) -> TemporaryChanges {
    let selections: Vec<String> = config
        .spec
        .spec
        .selections
        .iter()
        .filter(|(_, s)| s.is_temporary())
        .map(|(name, _)| name.clone())
        .collect();
    let all = &config.spec.spec.patches;
    let kept: Vec<ManifestPatch> = all.iter().filter(|p| !p.is_temporary()).cloned().collect();
    let removed_patches = all.len() - kept.len();
    TemporaryChanges {
        selections,
        patches: (removed_patches > 0).then_some(kept),
        removed_patches,
    }
}

/// What a person typed as a commit sha.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShaForm {
    /// Forty hex characters: usable as is.
    Full,
    /// Seven to thirty-nine hex characters: must be expanded to a known commit.
    Abbreviated,
    /// Not a sha at all.
    Invalid,
}

pub fn sha_form(sha: &str) -> ShaForm {
    let hex = sha.len() >= 7 && sha.len() <= 40 && sha.bytes().all(|b| b.is_ascii_hexdigit());
    match (hex, sha.len()) {
        (true, 40) => ShaForm::Full,
        (true, _) => ShaForm::Abbreviated,
        (false, _) => ShaForm::Invalid,
    }
}

/// The full sha to deploy for what was typed. Image tags are
/// `commit-<full sha>`, so an abbreviated sha deployed as typed would name
/// an image that does not exist; it is expanded against the commits known
/// for the artifact repo and refused unless exactly one matches.
fn full_commit_sha(
    conn: &PooledConnection<SqliteConnectionManager>,
    config: &DeployConfig,
    sha: &str,
) -> AppResult<String> {
    let sha = sha.trim();
    match sha_form(sha) {
        ShaForm::Full => Ok(sha.to_ascii_lowercase()),
        ShaForm::Invalid => Err(AppError::InvalidInput(format!(
            "'{sha}' is not a commit sha (expected 7 to 40 hex characters)"
        ))),
        ShaForm::Abbreviated => {
            let name = kube::ResourceExt::name_any(config);
            let repo = config.artifact_repository().ok_or_else(|| {
                AppError::InvalidInput(format!("{name} has no commit parameter to pin"))
            })?;
            let git_repo =
                GitRepo::get_by_name(&repo.owner, &repo.repo, conn)?.ok_or_else(|| {
                    AppError::InvalidInput(format!(
                        "{}/{} is not a known repository",
                        repo.owner, repo.repo
                    ))
                })?;
            let matches = GitCommit::find_by_prefix(&sha.to_ascii_lowercase(), git_repo.id, conn)?;
            match matches.as_slice() {
                [one] => Ok(one.sha.clone()),
                [] => Err(AppError::InvalidInput(format!(
                    "No commit of {}/{} starts with {sha}; pass the full 40-character sha",
                    repo.owner, repo.repo
                ))),
                _ => Err(AppError::InvalidInput(format!(
                    "{sha} is ambiguous in {}/{}; pass a longer or full sha",
                    repo.owner, repo.repo
                ))),
            }
        }
    }
}

/// A change to a config's patch list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchChange {
    Add(ManifestPatch),
    Remove(usize),
}

/// Add or remove a manifest patch. The new patch list is validated by
/// rendering the config with it before anything is written, so a patch
/// that does not fit is refused here rather than failing reconcile later.
/// Refused while a blocker is active, like a deploy: patches change what
/// runs. Records a revision with action `patch`.
pub async fn change_patches(
    config: &DeployConfig,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    actor: &str,
    change: PatchChange,
    _octocrabs: &Octocrabs,
) -> AppResult<()> {
    let name = kube::ResourceExt::name_any(config);
    check_blockers(&pool.get()?, &Action::DeployLatest, &name)?;

    let mut patches = config.spec.spec.patches.clone();
    let reason = match change {
        PatchChange::Add(patch) => {
            let reason = format!("added patch: {}", patch.describe());
            patches.push(patch);
            reason
        }
        PatchChange::Remove(index) => {
            if index >= patches.len() {
                return Err(AppError::InvalidInput(format!(
                    "{name} has no patch at position {index}"
                )));
            }
            let removed = patches.remove(index);
            format!("removed patch: {}", removed.describe())
        }
    };

    // Validate against what is deployed now. An undeployed config has no
    // values, so only check the patches themselves fit the templates.
    let mut effective = config.clone();
    effective.spec.spec.patches = patches.clone();
    let rendered = if config.status.as_ref().is_some_and(|s| s.config.is_some()) {
        effective.render_manifests()?
    } else {
        let templates = effective.resource_templates();
        let manifests = templates.iter().map(|t| t.manifest.clone()).collect();
        crate::kubernetes::patches::apply_patches(&templates, manifests, &patches)?
    };

    // A patch can fit the JSON and still produce a manifest Kubernetes will
    // not take (an `add` on `/env` instead of `/env/-` turns the array into
    // an object). Dry-run the exact objects a reconcile would apply so the
    // API server's schema check happens now, with the error shown to the
    // person, instead of failing quietly in the controller afterwards.
    let ns = kube::ResourceExt::namespace(config).unwrap_or_else(|| "default".to_string());
    for obj in effective.child_objects(rendered)? {
        let kind = obj
            .types
            .as_ref()
            .map(|t| t.kind.clone())
            .unwrap_or_default();
        let obj_name = kube::ResourceExt::name_any(&obj);
        if let Err(e) = crate::kubernetes::api::apply_dry_run(client, &ns, obj).await {
            let detail = match &e {
                AppError::Kubernetes(kube::Error::Api(resp)) => resp.message.clone(),
                other => other.to_string(),
            };
            return Err(AppError::InvalidInput(format!(
                "Kubernetes rejected {kind}/{obj_name} with these patches: {detail}"
            )));
        }
    }

    cr_writers::set_patches(client, &ns, &name, &patches).await?;

    let conn = pool.get()?;
    match Revision::record(&conn, NewRevision::patch_change(&effective, actor, &reason)) {
        Ok(rev) => log::info!("Recorded revision {} for {} ({})", rev.id, name, reason),
        Err(e) => log::error!("Failed to record patch revision for {}: {}", name, e),
    }
    Ok(())
}

/// The static parameter values a rollback replays, from the revision's
/// recorded rows. Commit parameters are replayed through the resolver.
fn values_from_revision(
    conn: &PooledConnection<SqliteConnectionManager>,
    revision: i64,
) -> AppResult<ParameterValues> {
    let rev = Revision::get(conn, revision)?
        .ok_or_else(|| AppError::InvalidInput(format!("Revision {revision} does not exist")))?;
    Ok(rev
        .parameters
        .into_iter()
        .filter(|p| p.kind == "value")
        .map(|p| (p.name, ParameterValue::Value { value: p.value }))
        .collect())
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
    if !action.is_deploy()
        && !action.is_clear_selection()
        && !action.is_set_parameter()
        && !action.is_end_temporary()
    {
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
/// concrete [`DeployAction`] to execute. `values` are the resolved static
/// parameters, carried along for deploys.
pub fn to_deploy_action(
    action: &Action,
    name: &str,
    state: DeploymentState,
    values: ParameterValues,
) -> DeployAction {
    match action {
        Action::DeployLatest
        | Action::DeployBranch { .. }
        | Action::DeployCommit { .. }
        | Action::Rollback { .. }
        | Action::ClearSelection
        | Action::SetParameter { .. }
        | Action::EndTemporary
        | Action::Undeploy => match state {
            DeploymentState::DeployedWithArtifact { artifact, config } => DeployAction::Deploy {
                name: name.to_string(),
                artifact: Some(artifact),
                config,
                values,
            },
            DeploymentState::DeployedOnlyConfig { config } => DeployAction::Deploy {
                name: name.to_string(),
                artifact: None,
                config,
                values,
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
    pool: &Pool<SqliteConnectionManager>,
    actor: &str,
    intent: &SelectionIntent,
) -> AppResult<DeployAction> {
    let name = kube::ResourceExt::name_any(config);
    // Connections are never held across an await: sqlite connections are
    // not Sync, and the webhook handlers need this future to be Send.
    let conn = pool.get()?;
    check_blockers(&conn, action, &name)?;
    // An abbreviated sha is expanded before anything is recorded or applied.
    let normalized;
    let action = match action {
        Action::DeployCommit { sha } => {
            normalized = Action::DeployCommit {
                sha: full_commit_sha(&conn, config, sha)?,
            };
            &normalized
        }
        other => other,
    };
    if let Action::SetParameter { parameter, .. } = action {
        let is_static = config
            .spec
            .spec
            .parameters
            .get(parameter)
            .is_some_and(|s| s.default_value().is_some());
        if !is_static {
            return Err(AppError::InvalidInput(format!(
                "{parameter} is not a value parameter of {name}; only value parameters can be set"
            )));
        }
    }

    // The selection changes this action implies are applied to an in-memory
    // copy first, so static parameters resolve against the new intent, and
    // are persisted after the deploy succeeds. Ending a temporary deployment
    // is the one action that changes several selections and the patch list
    // at once.
    let default_branch = config.artifact_repository().map(|r| r.branch);
    let mut changes: Vec<(String, Option<Selection>)> = Vec::new();
    let mut new_patches: Option<Vec<ManifestPatch>> = None;
    let mut reason: Option<String> = None;
    if action.is_end_temporary() {
        let temporary = temporary_changes(config);
        if temporary.is_empty() {
            return Err(AppError::InvalidInput(format!(
                "{name} has nothing temporary to end"
            )));
        }
        reason = Some(temporary.describe());
        changes.extend(temporary.selections.iter().map(|p| (p.clone(), None)));
        new_patches = temporary.patches;
    } else if let Some(change) = selection_change(action, default_branch.as_deref(), intent) {
        changes.push(change);
    }
    let mut effective = config.clone();
    for (parameter, selection) in &changes {
        match selection {
            Some(s) => {
                effective
                    .spec
                    .spec
                    .selections
                    .insert(parameter.clone(), s.clone());
            }
            None => {
                effective.spec.spec.selections.remove(parameter);
            }
        }
    }
    if let Some(patches) = &new_patches {
        effective.spec.spec.patches = patches.clone();
    }

    let state = DeploymentState::from_action(action, &effective, &conn)?;
    let values = match action {
        Action::Rollback { revision } => values_from_revision(&conn, *revision)?,
        _ => effective.resolve_value_parameters(),
    };
    let deploy_action = to_deploy_action(action, &name, state, values);
    drop(conn);

    deploy_action
        .execute(client, octocrabs, config.config_repository())
        .await?;

    // Record the intent behind the deploy. Bookkeeping like the rest: the
    // deploy has happened, and a missing selection only means the next
    // "latest" derives it from what is deployed.
    // The selections manager owns the whole map, so the effective map is
    // written in one apply rather than one key at a time.
    let ns = kube::ResourceExt::namespace(config).unwrap_or_else(|| "default".to_string());
    if !changes.is_empty() {
        if let Err(e) =
            cr_writers::set_selections(client, &ns, &name, &effective.spec.spec.selections).await
        {
            log::error!("Failed to record selections for {}: {}", name, e);
        }
    }
    if let Some(patches) = &new_patches {
        if let Err(e) = cr_writers::set_patches(client, &ns, &name, patches).await {
            log::error!("Failed to record patch list for {}: {}", name, e);
        }
    }

    // Best-effort: mirror the new state into the GitHub Deployments API.
    crate::github_deployments::report_deploy_action(octocrabs, config, &deploy_action).await;

    let conn = pool.get()?;
    if let Some(mut new) = NewRevision::from_deploy_action(&deploy_action, &effective, &conn, actor)
    {
        if let Action::Rollback { revision } = action {
            new.reason = Some(format!("rollback to revision {revision}"));
        }
        if reason.is_some() {
            new.reason = reason.clone();
        }
        match Revision::record(&conn, new) {
            Ok(rev) => log::info!("Recorded revision {} for {} ({})", rev.id, name, rev.action),
            Err(e) => log::error!("Failed to record revision for {}: {}", name, e),
        }
    }

    // A rollback holds the config until someone decides the incident is over.
    // Selections are untouched, so clearing the blocker and deploying latest
    // resumes exactly what was being tracked before.
    if let Action::Rollback { revision } = action {
        match Blocker::create(
            &conn,
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
            to_deploy_action(&Action::DeployLatest, "x", with, ParameterValues::default()),
            DeployAction::Deploy {
                artifact: Some(_),
                ..
            }
        ));
        let only = DeploymentState::DeployedOnlyConfig { config: sha("c") };
        assert!(matches!(
            to_deploy_action(
                &Action::DeployBranch { branch: "b".into() },
                "x",
                only,
                ParameterValues::default()
            ),
            DeployAction::Deploy { artifact: None, .. }
        ));
        assert!(matches!(
            to_deploy_action(
                &Action::Undeploy,
                "x",
                DeploymentState::Undeployed,
                ParameterValues::default()
            ),
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
            to_deploy_action(
                &Action::Rollback { revision: 7 },
                "x",
                with,
                ParameterValues::default()
            ),
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
            Some((_, Some(s))) => {
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
            Some((SHA_PARAMETER.to_string(), None)),
            "deploying the default branch clears the override"
        );
        let pin = Action::DeployCommit { sha: "abc".into() };
        let noted = SelectionIntent {
            durability: Some(Durability::Temporary),
            note: Some("testing".into()),
            by: Some("kevin".into()),
        };
        match selection_change(&pin, Some("master"), &noted) {
            Some((_, Some(s))) => {
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
            Some((_, Some(s))) => assert_eq!(
                s.durability,
                Durability::Standing,
                "pins default to standing"
            ),
            other => panic!("expected a pin selection, got {other:?}"),
        }
        assert_eq!(
            selection_change(&Action::ClearSelection, Some("master"), &intent),
            Some((SHA_PARAMETER.to_string(), None))
        );
        for untouched in [
            Action::DeployLatest,
            Action::Rollback { revision: 1 },
            Action::Undeploy,
            Action::Bounce,
        ] {
            assert_eq!(selection_change(&untouched, Some("master"), &intent), None);
        }

        let set = Action::SetParameter {
            parameter: "REPLICAS".into(),
            value: Some("5".into()),
        };
        match selection_change(&set, Some("master"), &intent) {
            Some((param, Some(s))) => {
                assert_eq!(param, "REPLICAS");
                assert_eq!(s.pin.as_ref().map(|p| p.value.as_str()), Some("5"));
                assert_eq!(s.durability, Durability::Standing);
            }
            other => panic!("expected a pin on REPLICAS, got {other:?}"),
        }
        let reset = Action::SetParameter {
            parameter: "REPLICAS".into(),
            value: None,
        };
        assert_eq!(
            selection_change(&reset, Some("master"), &intent),
            Some(("REPLICAS".to_string(), None))
        );
    }

    #[test]
    fn clear_selection_is_gated_and_maps_like_a_deploy() -> AppResult<()> {
        let only = DeploymentState::DeployedOnlyConfig { config: sha("c") };
        assert!(matches!(
            to_deploy_action(
                &Action::ClearSelection,
                "x",
                only,
                ParameterValues::default()
            ),
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
            to_deploy_action(
                &Action::Bounce,
                "x",
                DeploymentState::Undeployed,
                ParameterValues::default()
            ),
            DeployAction::Bounce { .. }
        ));
        assert!(matches!(
            to_deploy_action(
                &Action::ExecuteJob,
                "x",
                DeploymentState::Undeployed,
                ParameterValues::default()
            ),
            DeployAction::ExecuteJob { .. }
        ));
        assert!(matches!(
            to_deploy_action(
                &Action::ToggleAutodeploy,
                "x",
                DeploymentState::Undeployed,
                ParameterValues::default()
            ),
            DeployAction::ToggleAutodeploy { .. }
        ));
    }
    fn bare_config() -> DeployConfig {
        use crate::kubernetes::deploy_config::{DeployConfigSpec, DeployConfigSpecFields};
        use crate::kubernetes::parameters::ParameterSource;
        use crate::kubernetes::repo::Repository;
        DeployConfig::new(
            "site",
            DeployConfigSpec {
                spec: DeployConfigSpecFields {
                    team: "t".into(),
                    kind: "service".into(),
                    parameters: ParameterSource::sha_map(Some(
                        Repository {
                            owner: "o".into(),
                            repo: "r".into(),
                        }
                        .with_branch("master"),
                    )),
                    selections: Default::default(),
                    patches: vec![],
                    config: Repository {
                        owner: "o".into(),
                        repo: "c".into(),
                    },
                    specs: vec![],
                },
            },
        )
    }

    fn patch(durability: Durability) -> ManifestPatch {
        use crate::kubernetes::patches::{PatchOp, PatchTarget};
        ManifestPatch {
            target: PatchTarget {
                file: None,
                kind: "Deployment".into(),
                name: "web".into(),
            },
            op: PatchOp::Replace,
            path: "/spec/replicas".into(),
            value: Some(serde_json::json!(3)),
            durability,
            note: None,
            by: None,
            since: None,
        }
    }

    #[test]
    fn ending_a_temporary_deployment_clears_only_what_is_temporary() {
        let mut config = bare_config();
        assert!(temporary_changes(&config).is_empty());

        let selections = &mut config.spec.spec.selections;
        selections.insert(
            SHA_PARAMETER.into(),
            Selection::track("feature", Durability::Temporary),
        );
        selections.insert(
            "GREETING".into(),
            Selection::pin("howdy", Durability::Temporary),
        );
        selections.insert("REPLICAS".into(), Selection::pin("5", Durability::Standing));
        config.spec.spec.patches = vec![
            patch(Durability::Standing),
            patch(Durability::Temporary),
            patch(Durability::Temporary),
        ];

        let changes = temporary_changes(&config);
        assert_eq!(changes.selections, vec!["GREETING", SHA_PARAMETER]);
        assert_eq!(changes.patches.as_ref().map(Vec::len), Some(1));
        assert_eq!(
            changes.describe(),
            "ended temporary deployment: cleared GREETING, SHA; removed 2 patches"
        );

        // Standing patches alone leave the patch list untouched.
        config.spec.spec.patches = vec![patch(Durability::Standing)];
        assert!(temporary_changes(&config).patches.is_none());
    }

    #[test]
    fn ending_a_temporary_deployment_is_gated_like_a_deploy() -> AppResult<()> {
        let pool = migrated_memory_pool();
        let conn = pool.get()?;
        assert!(check_blockers(&conn, &Action::EndTemporary, "site").is_ok());
        Blocker::create(&conn, "site", "incident", "kevin")?;
        assert!(check_blockers(&conn, &Action::EndTemporary, "site").is_err());
        assert!(selection_change(
            &Action::EndTemporary,
            Some("master"),
            &SelectionIntent::default()
        )
        .is_none());
        Ok(())
    }
    #[test]
    fn sha_forms_are_classified() {
        assert_eq!(sha_form(&"a".repeat(40)), ShaForm::Full);
        assert_eq!(sha_form("911fcbb"), ShaForm::Abbreviated);
        assert_eq!(sha_form(&"b".repeat(39)), ShaForm::Abbreviated);
        assert_eq!(sha_form("911fcb"), ShaForm::Invalid, "too short");
        assert_eq!(sha_form("master"), ShaForm::Invalid);
        assert_eq!(sha_form(&"c".repeat(41)), ShaForm::Invalid);
    }
}
