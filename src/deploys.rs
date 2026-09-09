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

use std::collections::{BTreeMap, HashMap};

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
        selections::{Choice, Durability, Mode, Selection},
        DeployConfig,
    },
    watchtower::Watchtower,
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
    /// Values typed for this one deploy, keyed by parameter name. Used for
    /// tag parameters when watchtower is unreachable: the value is deployed
    /// and recorded under the tracked channel, and the selection is left
    /// alone, so the next "latest" resolves through watchtower again.
    pub values: BTreeMap<String, String>,
}

impl SelectionIntent {
    /// From a form: `durability`, `note`, `by`, and `value_<PARAM>` fields.
    pub fn from_form(form: &HashMap<String, String>) -> Self {
        let durability = match form.get("durability").map(|d| d.trim()) {
            Some("temporary") => Some(Durability::Temporary),
            Some("standing") => Some(Durability::Standing),
            _ => None,
        };
        let values = form
            .iter()
            .filter_map(|(k, v)| {
                let name = k.strip_prefix("value_")?;
                let v = v.trim();
                (!name.is_empty() && !v.is_empty()).then(|| (name.to_string(), v.to_string()))
            })
            .collect();
        SelectionIntent {
            durability,
            note: form.get("note").cloned(),
            by: form.get("by").cloned(),
            values,
        }
    }
}

/// Resolve one tag parameter of `config` for this deploy: a pin is
/// itself, a typed one-shot value is used as is under the tracked channel,
/// and anything tracked is the highest tag watchtower knows that matches
/// the channel's range. Fails closed when watchtower is unreachable and
/// there is no typed value. `None` when `pname` is not a tag parameter.
pub async fn resolve_tag_parameter(
    watchtower: &Watchtower,
    config: &DeployConfig,
    pname: &str,
    typed: Option<&str>,
) -> AppResult<Option<ParameterValue>> {
    let name = kube::ResourceExt::name_any(config);
    let Some(source) = config.spec.spec.parameters.get(pname) else {
        return Ok(None);
    };
    let (Some(image), Some(default_pattern)) = (source.image_ref(), source.default_channel())
    else {
        return Ok(None);
    };
    let selection = config.selection(pname);
    let pattern = match selection.mode() {
        Mode::Pin(value) => {
            return Ok(Some(ParameterValue::Tag {
                value: value.to_string(),
                pattern: None,
                digest: None,
            }));
        }
        Mode::Track(pattern) => pattern.to_string(),
        Mode::Default => default_pattern.to_string(),
    };
    if let Some(value) = typed.map(str::trim).filter(|v| !v.is_empty()) {
        return Ok(Some(ParameterValue::Tag {
            value: value.to_string(),
            pattern: Some(pattern),
            digest: None,
        }));
    }
    let image_name = format!("{}/{}", image.registry, image.name);
    let repo = match watchtower.lookup(&image).await {
        Ok(Some(repo)) => repo,
        Ok(None) => {
            // Register now so the next attempt can succeed.
            watchtower.register(&image).await?;
            return Err(AppError::InvalidInput(format!(
                "watchtower had not been told about {image_name} for {name}.{pname}; it is registered now, try again in a minute"
            )));
        }
        Err(AppError::Unavailable(message)) => {
            return Err(AppError::Unavailable(format!(
                "{message}. {name} cannot resolve {pname} ({image_name}, {pattern}); type the tag to deploy for this once, or pin it"
            )));
        }
        Err(e) => return Err(e),
    };
    let candidates = repo.tag.iter().filter(|t| t.active).map(|t| t.tag.as_str());
    let chosen = crate::kubernetes::tags::highest_matching(candidates, &pattern)
        .map_err(AppError::InvalidInput)?
        .ok_or_else(|| {
            AppError::InvalidInput(format!(
                "no tag of {image_name} matches {pattern} (for {name}.{pname})"
            ))
        })?;
    let digest = repo
        .tag
        .iter()
        .find(|t| t.tag == chosen)
        .and_then(|t| t.digest())
        .map(String::from);
    Ok(Some(ParameterValue::Tag {
        value: chosen,
        pattern: Some(pattern),
        digest,
    }))
}

/// Resolve every tag parameter of `config` for this deploy (see
/// [`resolve_tag_parameter`]). The first failure is the deploy's failure.
pub async fn resolve_tag_parameters(
    watchtower: &Watchtower,
    config: &DeployConfig,
    typed: &BTreeMap<String, String>,
) -> AppResult<ParameterValues> {
    let mut values = ParameterValues::new();
    for pname in config.spec.spec.parameters.keys() {
        if let Some(value) = resolve_tag_parameter(
            watchtower,
            config,
            pname,
            typed.get(pname).map(String::as_str),
        )
        .await?
        {
            values.insert(pname.clone(), value);
        }
    }
    Ok(values)
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
        | Action::DeployAdvanced { .. }
        | Action::EndTemporary
        | Action::Rollback { .. }
        | Action::Undeploy
        | Action::Bounce
        | Action::ExecuteJob
        | Action::ToggleAutodeploy => None,
    }
}

/// Every selection change an action implies. Single-parameter actions
/// delegate to [`selection_change`]; an advanced deploy carries one choice
/// per parameter it showed, and only the choices that differ from the
/// parameter's current selection are recorded. That keeps a standing pin
/// that was merely re-submitted from being rewritten with this deploy's
/// durability. Choosing the default channel for `SHA` writes an explicit
/// empty selection when the current one is derived from what is deployed,
/// since removing a key that was never there would leave the derivation
/// in place.
pub fn selection_changes(
    action: &Action,
    config: &DeployConfig,
    intent: &SelectionIntent,
) -> Vec<(String, Option<Selection>)> {
    let Action::DeployAdvanced {
        choices,
        durability,
        ..
    } = action
    else {
        let default_branch = config.artifact_repository().map(|r| r.branch);
        return selection_change(action, default_branch.as_deref(), intent)
            .into_iter()
            .collect();
    };
    let mut changes = Vec::new();
    for (name, choice) in choices {
        let Some(source) = config.spec.spec.parameters.get(name) else {
            continue;
        };
        let current = config.selection(name);
        let explicit = config.spec.spec.selections.contains_key(name);
        let noted = |s: Selection| s.with_note(intent.note.as_deref(), intent.by.as_deref());
        let change = match choice {
            Choice::Default => {
                if !current.is_override() {
                    None
                } else if !explicit && name == SHA_PARAMETER {
                    Some(Some(Selection::default()))
                } else {
                    Some(None)
                }
            }
            Choice::Track(channel) => {
                let channel = channel.trim();
                if channel.is_empty() || source.default_value().is_some() {
                    None
                } else if Some(channel) == source.default_channel() {
                    if current.is_override() {
                        Some(None)
                    } else {
                        None
                    }
                } else if current.mode() == Mode::Track(channel) {
                    None
                } else if source.is_tag() {
                    Some(Some(noted(Selection::track_pattern(channel, *durability))))
                } else {
                    Some(Some(noted(Selection::track(channel, *durability))))
                }
            }
            Choice::Pin(value) => {
                let value = value.trim();
                if value.is_empty() || current.mode() == Mode::Pin(value) {
                    None
                } else {
                    Some(Some(noted(Selection::pin(value, *durability))))
                }
            }
        };
        if let Some(change) = change {
            changes.push((name.clone(), change));
        }
    }
    changes
}

/// The action with any abbreviated commit sha expanded to the full one:
/// a `DeployCommit`, or the `SHA` pin of an advanced deploy. Every other
/// action is returned as is. Both the deploy and its preview go through
/// here so they agree on what is being deployed.
pub fn normalize_action(
    conn: &PooledConnection<SqliteConnectionManager>,
    config: &DeployConfig,
    action: &Action,
) -> AppResult<Action> {
    match action {
        Action::DeployCommit { sha } => Ok(Action::DeployCommit {
            sha: full_commit_sha(conn, config, sha)?,
        }),
        Action::DeployAdvanced {
            choices,
            durability,
            patches,
        } => {
            let mut choices = choices.clone();
            if let Some(Choice::Pin(sha)) = choices.get(SHA_PARAMETER) {
                if !sha.trim().is_empty() {
                    let full = full_commit_sha(conn, config, sha)?;
                    choices.insert(SHA_PARAMETER.to_string(), Choice::Pin(full));
                }
            }
            Ok(Action::DeployAdvanced {
                choices,
                durability: *durability,
                patches: patches.clone(),
            })
        }
        other => Ok(other.clone()),
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

    let mut effective = config.clone();
    effective.spec.spec.patches = patches.clone();
    validate_patches(&effective, client).await?;

    let ns = kube::ResourceExt::namespace(config).unwrap_or_else(|| "default".to_string());
    cr_writers::set_patches(client, &ns, &name, &patches).await?;

    let conn = pool.get()?;
    match Revision::record(&conn, NewRevision::patch_change(&effective, actor, &reason)) {
        Ok(rev) => log::info!("Recorded revision {} for {} ({})", rev.id, name, reason),
        Err(e) => log::error!("Failed to record patch revision for {}: {}", name, e),
    }
    Ok(())
}

/// Check that a config's patch list fits: render (or, for an undeployed
/// config, apply the patches to the bare templates) and dry-run the exact
/// objects a reconcile would apply, so the API server's schema check
/// happens now with the error shown to the person, instead of failing
/// quietly in the controller afterwards.
pub async fn validate_patches(config: &DeployConfig, client: &Client) -> AppResult<()> {
    let patches = &config.spec.spec.patches;
    let rendered = if config.status.as_ref().is_some_and(|s| s.config.is_some()) {
        config.render_manifests()?
    } else {
        let templates = config.resource_templates();
        let manifests = templates.iter().map(|t| t.manifest.clone()).collect();
        crate::kubernetes::patches::apply_patches(&templates, manifests, patches)?
    };
    let ns = kube::ResourceExt::namespace(config).unwrap_or_else(|| "default".to_string());
    for obj in config.child_objects(rendered)? {
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
        .filter_map(|p| match p.kind.as_str() {
            "value" => Some((p.name, ParameterValue::Value { value: p.value })),
            // A replayed tag is exactly that tag, whatever the range was.
            "tag" => Some((
                p.name,
                ParameterValue::Tag {
                    value: p.value,
                    pattern: None,
                    digest: None,
                },
            )),
            _ => None,
        })
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
        | Action::DeployAdvanced { .. }
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
    let normalized = normalize_action(&conn, config, action)?;
    let action = &normalized;
    if let Action::DeployAdvanced { choices, .. } = action {
        for (parameter, choice) in choices {
            if choice.typed().is_some_and(|v| v.trim().is_empty()) {
                return Err(AppError::InvalidInput(format!(
                    "type a {} for ${parameter}, or choose its default",
                    if choice.kind() == "pin" {
                        "value"
                    } else {
                        "channel"
                    }
                )));
            }
        }
    }
    if let Action::SetParameter { parameter, .. } = action {
        let settable = config
            .spec
            .spec
            .parameters
            .get(parameter)
            .is_some_and(|s| s.default_value().is_some() || s.is_tag());
        if !settable {
            return Err(AppError::InvalidInput(format!(
                "{parameter} is not a value or tag parameter of {name}; commit parameters are set with the deploy form"
            )));
        }
    }

    // The selection changes this action implies are applied to an in-memory
    // copy first, so static parameters resolve against the new intent, and
    // are persisted after the deploy succeeds. Ending a temporary deployment
    // is the one action that changes several selections and the patch list
    // at once.
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
    } else {
        changes.extend(selection_changes(action, config, intent));
        if let Action::DeployAdvanced {
            patches,
            durability,
            ..
        } = action
        {
            if !patches.is_empty() {
                new_patches = Some(patches.apply_to(&config.spec.spec.patches, *durability));
            }
        }
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
    // Patches this deploy adds are checked against the cluster before
    // anything is written, as an add through the patch tools would be.
    if action.is_deploy_advanced() && new_patches.is_some() {
        validate_patches(&effective, client).await?;
    }

    let state = DeploymentState::from_action(action, &effective, &conn)?;
    let mut values = match action {
        Action::Rollback { revision } => values_from_revision(&conn, *revision)?,
        _ => effective.resolve_value_parameters(),
    };
    drop(conn);
    // Tag parameters resolve through watchtower, after the connection is
    // released (the lookup is an await). A rollback already carries them.
    let deploys = !matches!(
        action,
        Action::Rollback { .. }
            | Action::Undeploy
            | Action::Bounce
            | Action::ExecuteJob
            | Action::ToggleAutodeploy
    );
    if deploys && effective.spec.spec.parameters.values().any(|s| s.is_tag()) {
        values.extend(
            resolve_tag_parameters(Watchtower::global(), &effective, &intent.values).await?,
        );
    }
    let deploy_action = to_deploy_action(action, &name, state, values);

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

    // Best-effort: mirror the new state into the GitHub Deployments API, in
    // the background. Two GitHub round trips were on the path between the
    // click and the response, and nothing here depends on them.
    {
        let octocrabs = octocrabs.clone();
        let config = config.clone();
        let deploy_action = deploy_action.clone();
        tokio::spawn(async move {
            crate::github_deployments::report_deploy_action(&octocrabs, &config, &deploy_action)
                .await;
        });
    }

    let conn = pool.get()?;
    if let Some(mut new) = NewRevision::from_deploy_action(&deploy_action, &effective, &conn, actor)
    {
        if let Action::Rollback { revision } = action {
            new.reason = Some(match intent.note.as_deref().map(str::trim) {
                Some(note) if !note.is_empty() => {
                    format!("rollback to revision {revision}: {note}")
                }
                _ => format!("rollback to revision {revision}"),
            });
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
        let reason = match intent.note.as_deref().map(str::trim) {
            Some(note) if !note.is_empty() => format!("Rolled back to revision {revision}: {note}"),
            _ => format!(
                "Rolled back to revision {revision}; clear when it is safe to move forward again"
            ),
        };
        match Blocker::create(&conn, &name, &reason, actor) {
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
                assert_eq!(s.track.as_ref().map(|t| t.channel()), Some("feature"));
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
            values: Default::default(),
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
    fn advanced_deploy_records_only_what_changed() {
        use crate::kubernetes::parameters::ParameterSource;
        let mut config = bare_config();
        config.spec.spec.parameters.insert(
            "NGINX".into(),
            ParameterSource::Tag {
                image: "nginx".into(),
                pattern: "1.27.*".into(),
            },
        );
        config.spec.spec.parameters.insert(
            "REPLICAS".into(),
            ParameterSource::Value {
                default: "2".into(),
            },
        );
        config.spec.spec.selections.insert(
            "NGINX".into(),
            Selection::pin("1.27.4", Durability::Standing),
        );
        let intent = SelectionIntent {
            note: Some("advanced deploy".into()),
            by: Some("web".into()),
            ..Default::default()
        };
        let advanced = |pairs: &[(&str, Choice)]| Action::DeployAdvanced {
            choices: pairs
                .iter()
                .map(|(n, c)| (n.to_string(), c.clone()))
                .collect(),
            durability: Durability::Temporary,
            patches: Default::default(),
        };

        // Re-submitting the standing pin leaves it alone; the durability of
        // this deploy does not rewrite it.
        let same = advanced(&[
            ("NGINX", Choice::Pin("1.27.4".into())),
            ("REPLICAS", Choice::Default),
            (SHA_PARAMETER, Choice::Default),
        ]);
        assert!(selection_changes(&same, &config, &intent).is_empty());

        let changed = advanced(&[
            (SHA_PARAMETER, Choice::Track("fix/upload".into())),
            ("NGINX", Choice::Default),
            ("REPLICAS", Choice::Pin("3".into())),
        ]);
        let changes = selection_changes(&changed, &config, &intent);
        assert_eq!(changes.len(), 3);
        let by_name: BTreeMap<String, Option<Selection>> = changes.into_iter().collect();
        let sha = by_name[SHA_PARAMETER].clone().unwrap_or_default();
        assert_eq!(sha.mode(), Mode::Track("fix/upload"));
        assert_eq!(sha.durability, Durability::Temporary);
        assert_eq!(sha.note.as_deref(), Some("advanced deploy"));
        assert_eq!(by_name["NGINX"], None, "back to the default range");
        assert_eq!(
            by_name["REPLICAS"].clone().unwrap_or_default().mode(),
            Mode::Pin("3")
        );

        // The default branch typed as a channel is the default, not a track.
        let default_branch = advanced(&[(SHA_PARAMETER, Choice::Track("master".into()))]);
        assert!(selection_changes(&default_branch, &config, &intent).is_empty());
        // Tag ranges are tracked as patterns.
        let range = advanced(&[("NGINX", Choice::Track("1.28.*".into()))]);
        let changes = selection_changes(&range, &config, &intent);
        let sel = changes[0].1.clone().unwrap_or_default();
        assert_eq!(
            sel.track.as_ref().and_then(|t| t.pattern.as_deref()),
            Some("1.28.*")
        );
        // Nothing typed yet is not a change.
        let empty = advanced(&[(SHA_PARAMETER, Choice::Pin("  ".into()))]);
        assert!(selection_changes(&empty, &config, &intent).is_empty());
        // Unknown parameters are ignored.
        let unknown = advanced(&[("NOPE", Choice::Pin("x".into()))]);
        assert!(selection_changes(&unknown, &config, &intent).is_empty());
    }

    #[test]
    fn advanced_default_overrides_a_derived_sha_pin() {
        use crate::kubernetes::deploy_config::DeployConfigStatus;
        use crate::kubernetes::parameters::ParameterValue;
        let mut config = bare_config();
        let mut status = DeployConfigStatus::default();
        status.parameters.insert(
            SHA_PARAMETER.into(),
            ParameterValue::Commit {
                value: "abc".into(),
                branch: None,
            },
        );
        config.status = Some(status);
        assert_eq!(config.selection(SHA_PARAMETER).mode(), Mode::Pin("abc"));
        let action = Action::DeployAdvanced {
            choices: BTreeMap::from([(SHA_PARAMETER.to_string(), Choice::Default)]),
            durability: Durability::Standing,
            patches: Default::default(),
        };
        let changes = selection_changes(&action, &config, &SelectionIntent::default());
        assert_eq!(changes.len(), 1);
        let written = changes[0].1.clone();
        assert!(
            written.as_ref().is_some_and(|s| !s.is_override()),
            "an explicit empty selection beats the derivation"
        );
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
