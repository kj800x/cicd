//! The deploy page: pick a config, pick an action, watch the preview.
//!
//! The left column is one form. In simple mode it holds the config picker,
//! the action chooser and the button. "Deploy advanced" is an action like
//! the others; choosing it unfolds one selector per parameter, the patch
//! list and a durability toggle, and the button becomes "Deploy advanced".
//! The right column previews what the chosen action would do, parameter by
//! parameter, and polls the cluster for the resources it owns.
//!
//! Every choice lives in the query string, so the page is its own state:
//! the GET form re-submits on change and the POST form mirrors the same
//! fields as hidden inputs.
#![allow(clippy::expect_used)]

use crate::crab_ext::Octocrabs;
use crate::db::blocker::Blocker;
use crate::db::git_branch::GitBranch;
use crate::db::git_commit::GitCommit;
use crate::db::git_repo::GitRepo;
use crate::db::revision::Revision;
use crate::deploys::SelectionIntent;
use crate::kubernetes::api::{get_all_deploy_configs, get_deploy_config, ListMode};
use crate::kubernetes::parameters::SHA_PARAMETER;
use crate::kubernetes::patches::PatchChanges;
use crate::kubernetes::repo::{DeploymentState, ShaMaybeBranch};
use crate::kubernetes::selections::{Choice, Durability, Mode, Selection};
use crate::kubernetes::{list_namespace_objects, DeployConfig};
use crate::prelude::*;
use crate::web::team_prefs::TeamsCookie;
use crate::web::{deploy_form, header, preview};
use kube::{Client, ResourceExt};
use maud::{html, Markup, Render};
use std::collections::{BTreeMap, HashMap};

/// A commit sha (or branch name) linked to GitHub. The sha is shortened to
/// seven characters unless `disable_prefixing` is set; `file_path` narrows
/// the link to a path in the tree.
pub struct GitRef(
    pub String,
    pub String,
    pub String,
    pub bool,
    pub Option<String>,
);

impl Render for GitRef {
    fn render(&self) -> Markup {
        let owner = self.1.clone();
        let repo = self.2.clone();
        let sha = self.0.clone();
        let disable_prefixing = self.3;
        let file_path = self.4.clone();

        let sha_prefix = if !disable_prefixing && sha.len() >= 7 {
            &sha[..7]
        } else {
            &sha
        };

        html!(
            span {
                a.git-ref href=(format!("https://github.com/{}/{}/tree/{}{}", owner, repo, sha, file_path.map(|path| format!("/{}", path)).unwrap_or_default())) target="_blank" title=(sha) {
                    (sha_prefix)
                }
            }
        )
    }
}

pub struct HumanTime(pub u64);

impl Render for HumanTime {
    fn render(&self) -> Markup {
        let time = match Utc.timestamp_millis_opt(self.0 as i64).single() {
            Some(t) => t,
            None => return html! { "Invalid timestamp" },
        };
        let eastern = chrono_tz::America::New_York;
        let local = time.with_timezone(&eastern);

        html! {
            time datetime=(time.to_rfc3339()) {
                (local.format("%B %d at %I:%M %p ET"))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BuildFilter {
    Any,
    /// Not requested by any current caller; kept so the resolver reads as
    /// the full set of choices.
    #[allow(dead_code)]
    Completed,
    Successful,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedVersion {
    UnknownSha {
        sha: String,
    },
    TrackedSha {
        sha: String,
        build_time: u64,
    },
    BranchTracked {
        sha: String,
        branch: String,
        build_time: u64,
    },
    Undeployed,
    ResolutionFailed,
}

impl ResolvedVersion {
    pub fn from_action(
        action: &Action,
        config: &DeployConfig,
        conn: &PooledConnection<SqliteConnectionManager>,
        build_filter: BuildFilter,
    ) -> Self {
        let artifact_repository = config
            .artifact_repository()
            .expect("Failed to get artifact repository");
        let repo =
            GitRepo::get_by_name(&artifact_repository.owner, &artifact_repository.repo, conn)
                .ok()
                .flatten()
                .expect("Failed to get git repo");

        match action {
            Action::DeployLatest
            | Action::DeployAdvanced { .. }
            | Action::SetParameter { .. }
            | Action::ClearSelection
            | Action::EndTemporary => {
                let selection = action.effective_sha_selection(config);
                let branch_name: &str = match (action.deploys_latest(), selection.mode()) {
                    (true, Mode::Pin(value)) => {
                        // Latest of a pinned parameter is the pin itself.
                        return match GitCommit::get_by_sha(value, repo.id, conn).ok().flatten() {
                            Some(commit) => ResolvedVersion::TrackedSha {
                                sha: commit.sha,
                                build_time: commit.timestamp as u64,
                            },
                            None => ResolvedVersion::UnknownSha {
                                sha: value.to_string(),
                            },
                        };
                    }
                    (true, Mode::Track(branch)) => branch,
                    _ => &artifact_repository.branch,
                };

                let branch = GitBranch::get_by_name(branch_name, repo.id, conn)
                    .ok()
                    .flatten();

                let Some(branch) = branch else {
                    return ResolvedVersion::ResolutionFailed;
                };

                let commit = match build_filter {
                    BuildFilter::Any => branch.latest_build(conn).ok().flatten(),
                    BuildFilter::Completed => branch.latest_completed_build(conn).ok().flatten(),
                    BuildFilter::Successful => branch.latest_successful_build(conn).ok().flatten(),
                };

                match commit {
                    Some(commit) => ResolvedVersion::BranchTracked {
                        sha: commit.sha,
                        branch: branch.name.clone(),
                        // FIXME: This isn't build time, it's commit time
                        build_time: commit.timestamp as u64,
                    },
                    None => ResolvedVersion::ResolutionFailed,
                }
            }
            Action::DeployBranch { branch } => {
                let branch = GitBranch::get_by_name(branch, repo.id, conn).ok().flatten();

                let Some(branch) = branch else {
                    return ResolvedVersion::ResolutionFailed;
                };

                let commit = match build_filter {
                    BuildFilter::Any => branch.latest_build(conn).ok().flatten(),
                    BuildFilter::Completed => branch.latest_completed_build(conn).ok().flatten(),
                    BuildFilter::Successful => branch.latest_successful_build(conn).ok().flatten(),
                };

                match commit {
                    Some(commit) => ResolvedVersion::BranchTracked {
                        sha: commit.sha,
                        branch: branch.name.clone(),
                        build_time: commit.timestamp as u64,
                    },
                    None => ResolvedVersion::ResolutionFailed,
                }
            }
            Action::DeployCommit { sha } => {
                let commit = GitCommit::get_by_sha(sha, repo.id, conn).ok().flatten();

                match commit {
                    Some(commit) => ResolvedVersion::TrackedSha {
                        sha: commit.sha,
                        build_time: commit.timestamp as u64,
                    },
                    None => ResolvedVersion::UnknownSha { sha: sha.clone() },
                }
            }
            Action::Rollback { revision } => {
                let sha = Revision::get(conn, *revision)
                    .ok()
                    .flatten()
                    .and_then(|rev| rev.parameter(SHA_PARAMETER).map(|p| p.value.clone()));
                let Some(sha) = sha else {
                    return ResolvedVersion::ResolutionFailed;
                };
                match GitCommit::get_by_sha(&sha, repo.id, conn).ok().flatten() {
                    Some(commit) => ResolvedVersion::TrackedSha {
                        sha: commit.sha,
                        build_time: commit.timestamp as u64,
                    },
                    None => ResolvedVersion::UnknownSha { sha },
                }
            }
            Action::Bounce => ResolvedVersion::ResolutionFailed,
            Action::ExecuteJob => ResolvedVersion::ResolutionFailed,
            Action::ToggleAutodeploy => ResolvedVersion::ResolutionFailed,
            Action::Undeploy => ResolvedVersion::Undeployed,
        }
    }

    fn matches_branch(&self, other: Option<&ResolvedVersion>) -> bool {
        match (self, other) {
            (
                ResolvedVersion::BranchTracked { branch, .. },
                Some(ResolvedVersion::BranchTracked {
                    branch: other_branch,
                    ..
                }),
            ) => branch == other_branch,
            _ => false,
        }
    }

    /// Formats the version for display, showing branch:sha if branch differs from comparison
    pub fn format(&self, other: Option<&ResolvedVersion>, owner: &str, repo: &str) -> Markup {
        match self {
            ResolvedVersion::UnknownSha { sha } => {
                html!(
                    (GitRef(
                        sha.clone(),
                        owner.to_string(),
                        repo.to_string(),
                        false,
                        None
                    ))
                )
            }
            ResolvedVersion::TrackedSha { sha, build_time: _ } => {
                html!(
                    (GitRef(
                        sha.clone(),
                        owner.to_string(),
                        repo.to_string(),
                        false,
                        None
                    ))
                )
            }
            ResolvedVersion::BranchTracked {
                sha,
                branch,
                build_time: _,
            } => {
                // If we have a branch and it differs from the other version's branch, show it
                let show_branch = !self.matches_branch(other);

                if show_branch {
                    html!(
                        (branch)
                        ":"
                        (GitRef(
                            sha.clone(),
                            owner.to_string(),
                            repo.to_string(),
                            false,
                            None,
                        ))
                    )
                } else {
                    html!(
                        (GitRef(
                            sha.clone(),
                            owner.to_string(),
                            repo.to_string(),
                            false,
                            None
                        ))
                    )
                }
            }
            ResolvedVersion::Undeployed => {
                html!("Undeployed")
            }
            ResolvedVersion::ResolutionFailed => {
                html!("ERROR: Resolution failed")
            }
        }
    }
}

impl DeploymentState {
    pub fn from_action(
        action: &Action,
        config: &DeployConfig,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        let artifact_repository = config.artifact_repository();

        match (action, artifact_repository) {
            (Action::DeployLatest, Some(artifact_repository))
            | (Action::DeployAdvanced { .. }, Some(artifact_repository))
            | (Action::SetParameter { .. }, Some(artifact_repository))
            | (Action::ClearSelection, Some(artifact_repository))
            | (Action::EndTemporary, Some(artifact_repository)) => {
                // "Latest" means latest according to the parameter's selection:
                // the default channel, an override branch, or, for a pin, the
                // pin itself. Clearing the selection always means the default.
                let selection = action.effective_sha_selection(config);
                let branch_name: &str = match (action.deploys_latest(), selection.mode()) {
                    (true, Mode::Pin(value)) => {
                        let pinned = ShaMaybeBranch {
                            sha: value.to_string(),
                            branch: None,
                        };
                        return Ok(DeploymentState::DeployedWithArtifact {
                            config: if artifact_repository.clone().into_repo()
                                == config.config_repository()
                            {
                                pinned.clone()
                            } else {
                                ShaMaybeBranch::latest_for_branch(
                                    config.config_repository(),
                                    "master",
                                    BuildFilter::Any,
                                    conn,
                                )?
                            },
                            artifact: pinned,
                        });
                    }
                    (true, Mode::Track(branch)) => branch,
                    _ => &artifact_repository.branch,
                };

                Ok(DeploymentState::DeployedWithArtifact {
                    artifact: ShaMaybeBranch::latest_for_branch(
                        artifact_repository.clone().into_repo(),
                        branch_name,
                        BuildFilter::Successful,
                        conn,
                    )?,
                    config: if artifact_repository.clone().into_repo() == config.config_repository()
                    {
                        ShaMaybeBranch::latest_for_branch(
                            config.config_repository(),
                            branch_name,
                            BuildFilter::Successful,
                            conn,
                        )?
                    } else {
                        ShaMaybeBranch::latest_for_branch(
                            config.config_repository(),
                            "master",
                            BuildFilter::Any,
                            conn,
                        )?
                    },
                })
            }
            (Action::DeployLatest, None)
            | (Action::DeployAdvanced { .. }, None)
            | (Action::SetParameter { .. }, None)
            | (Action::ClearSelection, None)
            | (Action::EndTemporary, None) => {
                let deployment_state = config.deployment_state();
                // FIXME: Misleading: artifact_branch is just the tracking branch.
                let branch_name = deployment_state.artifact_branch().unwrap_or("master");

                Ok(DeploymentState::DeployedOnlyConfig {
                    config: ShaMaybeBranch::latest_for_branch(
                        config.config_repository(),
                        branch_name,
                        BuildFilter::Any,
                        conn,
                    )?,
                })
            }
            (Action::DeployBranch { branch }, Some(artifact_repository)) => {
                Ok(DeploymentState::DeployedWithArtifact {
                    artifact: ShaMaybeBranch::latest_for_branch(
                        artifact_repository.clone().into_repo(),
                        branch,
                        BuildFilter::Successful,
                        conn,
                    )?,
                    config: if artifact_repository.into_repo() == config.config_repository() {
                        ShaMaybeBranch::latest_for_branch(
                            config.config_repository(),
                            branch,
                            BuildFilter::Successful,
                            conn,
                        )?
                    } else {
                        ShaMaybeBranch::latest_for_branch(
                            config.config_repository(),
                            "master",
                            BuildFilter::Any,
                            conn,
                        )?
                    },
                })
            }
            (Action::DeployBranch { branch }, None) => Ok(DeploymentState::DeployedOnlyConfig {
                config: ShaMaybeBranch::latest_for_branch(
                    config.config_repository(),
                    branch,
                    BuildFilter::Any,
                    conn,
                )?,
            }),
            (Action::DeployCommit { sha }, Some(artifact_repository)) => {
                Ok(DeploymentState::DeployedWithArtifact {
                    artifact: ShaMaybeBranch {
                        sha: sha.clone(),
                        branch: None,
                    },
                    config: if artifact_repository.into_repo() == config.config_repository() {
                        ShaMaybeBranch {
                            sha: sha.clone(),
                            branch: None,
                        }
                    } else {
                        ShaMaybeBranch::latest_for_branch(
                            config.config_repository(),
                            "master",
                            BuildFilter::Any,
                            conn,
                        )?
                    },
                })
            }
            (Action::DeployCommit { sha }, None) => Ok(DeploymentState::DeployedOnlyConfig {
                config: ShaMaybeBranch {
                    sha: sha.clone(),
                    branch: None,
                },
            }),
            (Action::Rollback { revision }, artifact_repository) => {
                let rev = Revision::get(conn, *revision)?.ok_or_else(|| {
                    AppError::InvalidInput(format!("Revision {revision} does not exist"))
                })?;
                if rev.config_name != config.name_any() {
                    return Err(AppError::InvalidInput(format!(
                        "Revision {revision} belongs to {}, not {}",
                        rev.config_name,
                        config.name_any()
                    )));
                }
                if rev.action != "deploy" {
                    return Err(AppError::InvalidInput(format!(
                        "Revision {revision} is an undeploy; use undeploy instead"
                    )));
                }
                let config_state = ShaMaybeBranch {
                    sha: rev.config_sha.clone().ok_or_else(|| {
                        AppError::InvalidInput(format!("Revision {revision} has no config commit"))
                    })?,
                    branch: rev.config_branch.clone(),
                };
                match (rev.parameter(SHA_PARAMETER), artifact_repository) {
                    (Some(param), Some(_)) => Ok(DeploymentState::DeployedWithArtifact {
                        artifact: ShaMaybeBranch {
                            sha: param.value.clone(),
                            branch: param.branch.clone(),
                        },
                        config: config_state,
                    }),
                    (Some(_), None) => Err(AppError::InvalidInput(format!(
                        "Revision {revision} deployed an artifact but the config no longer has one"
                    ))),
                    (None, _) => Ok(DeploymentState::DeployedOnlyConfig {
                        config: config_state,
                    }),
                }
            }
            (Action::Bounce, _) | (Action::ExecuteJob, _) | (Action::ToggleAutodeploy, _) => {
                Ok(config.deployment_state())
            }
            (Action::Undeploy, _) => Ok(DeploymentState::Undeployed),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    DeployLatest,
    DeployBranch {
        branch: String,
    },
    DeployCommit {
        sha: String,
    },
    /// The advanced form: one choice per parameter it showed and the
    /// pending patch edits, recorded together with one durability, then a
    /// deploy of latest of the result.
    DeployAdvanced {
        choices: BTreeMap<String, Choice>,
        durability: Durability,
        patches: PatchChanges,
    },
    /// Replay an earlier revision's deployed values, then hold the config
    /// with a blocker.
    Rollback {
        revision: i64,
    },
    /// Drop the SHA parameter's override or pin and deploy the latest of
    /// its default channel.
    ClearSelection,
    /// Pin a static parameter to `value` (or reset it to its default with
    /// `None`) and redeploy with the SHA parameter's current selection.
    SetParameter {
        parameter: String,
        value: Option<String>,
    },
    /// Clear every temporary selection and remove every temporary patch,
    /// then deploy latest of what remains. The one-step way out of a
    /// temporary deployment; standing overrides are left alone.
    EndTemporary,
    Bounce,
    ExecuteJob,
    ToggleAutodeploy,
    Undeploy,
}

fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

impl Action {
    pub fn from_query(query: &HashMap<String, String>) -> Self {
        match query
            .get("action")
            .unwrap_or(&"deploy".to_string())
            .as_str()
        {
            "deploy" => {
                if let Some(sha) = query.get("sha").filter(|s| !s.is_empty()) {
                    Action::DeployCommit { sha: sha.clone() }
                } else if let Some(branch) = query.get("branch").filter(|s| !s.is_empty()) {
                    Action::DeployBranch {
                        branch: branch.clone(),
                    }
                } else {
                    Action::DeployLatest
                }
            }
            "deploy-advanced" => Action::advanced_from_query(query),
            "rollback" => match query.get("revision").and_then(|r| r.parse::<i64>().ok()) {
                Some(revision) => Action::Rollback { revision },
                None => Action::DeployLatest,
            },
            "clear-selection" => Action::ClearSelection,
            "end-temporary" => Action::EndTemporary,
            "set-parameter" => match query.get("parameter").filter(|p| !p.is_empty()) {
                Some(parameter) => Action::SetParameter {
                    parameter: parameter.clone(),
                    value: query.get("value").filter(|v| !v.is_empty()).cloned(),
                },
                None => Action::DeployLatest,
            },
            "toggle-autodeploy" => Action::ToggleAutodeploy,
            "undeploy" => Action::Undeploy,
            "bounce" => Action::Bounce,
            "execute-job" => Action::ExecuteJob,
            _ => Action::DeployLatest,
        }
    }

    /// `sel_<P>=default|track|pin` with the channel in `track_<P>` and the
    /// value in `pin_<P>`; `durability` applies to every override made;
    /// `patches` is the pending patch edits as JSON. A parameter without a
    /// `sel_` field is left as it is; unreadable patches are dropped.
    fn advanced_from_query(query: &HashMap<String, String>) -> Self {
        let mut choices = BTreeMap::new();
        for (key, value) in query {
            let Some(parameter) = key.strip_prefix("sel_") else {
                continue;
            };
            if parameter.is_empty() {
                continue;
            }
            let typed = |prefix: &str| {
                query
                    .get(&format!("{prefix}_{parameter}"))
                    .map(|v| v.trim().to_string())
                    .unwrap_or_default()
            };
            let choice = match value.trim() {
                "track" => Choice::Track(typed("track")),
                "pin" => Choice::Pin(typed("pin")),
                _ => Choice::Default,
            };
            choices.insert(parameter.to_string(), choice);
        }
        let patches = query
            .get("patches")
            .map(|raw| PatchChanges::from_json(raw))
            .unwrap_or_else(|| Ok(PatchChanges::default()))
            .unwrap_or_else(|e| {
                log::warn!("ignoring pending patches from the query string: {e}");
                PatchChanges::default()
            });
        Action::DeployAdvanced {
            choices,
            durability: durability_from_query(query),
            patches,
        }
    }

    pub fn as_params(&self) -> String {
        match self {
            Action::DeployLatest => "action=deploy".to_string(),
            Action::DeployBranch { branch } => {
                format!("action=deploy&branch={}", url_encode(branch))
            }
            Action::DeployCommit { sha } => format!("action=deploy&sha={}", url_encode(sha)),
            Action::DeployAdvanced {
                choices,
                durability,
                patches,
            } => {
                let mut params =
                    format!("action=deploy-advanced&durability={}", durability.as_str());
                for (parameter, choice) in choices {
                    let p = url_encode(parameter);
                    params.push_str(&format!("&sel_{p}={}", choice.kind()));
                    if let Some(typed) = choice.typed() {
                        params.push_str(&format!("&{}_{p}={}", choice.kind(), url_encode(typed)));
                    }
                }
                if !patches.is_empty() {
                    params.push_str(&format!("&patches={}", url_encode(&patches.to_json())));
                }
                params
            }
            Action::Rollback { revision } => format!("action=rollback&revision={}", revision),
            Action::ClearSelection => "action=clear-selection".to_string(),
            Action::EndTemporary => "action=end-temporary".to_string(),
            Action::SetParameter { parameter, value } => format!(
                "action=set-parameter&parameter={}&value={}",
                url_encode(parameter),
                url_encode(&value.clone().unwrap_or_default())
            ),
            Action::Bounce => "action=bounce".to_string(),
            Action::ExecuteJob => "action=execute-job".to_string(),
            Action::ToggleAutodeploy => "action=toggle-autodeploy".to_string(),
            Action::Undeploy => "action=undeploy".to_string(),
        }
    }

    /// The `action=` value the forms send for this action.
    pub fn form_value(&self) -> &'static str {
        match self {
            Action::DeployLatest | Action::DeployBranch { .. } | Action::DeployCommit { .. } => {
                "deploy"
            }
            Action::DeployAdvanced { .. } => "deploy-advanced",
            Action::Rollback { .. } => "rollback",
            Action::ClearSelection => "clear-selection",
            Action::EndTemporary => "end-temporary",
            Action::SetParameter { .. } => "set-parameter",
            Action::Bounce => "bounce",
            Action::ExecuteJob => "execute-job",
            Action::ToggleAutodeploy => "toggle-autodeploy",
            Action::Undeploy => "undeploy",
        }
    }

    pub fn is_deploy(&self) -> bool {
        matches!(
            self,
            Action::DeployLatest
                | Action::DeployBranch { .. }
                | Action::DeployCommit { .. }
                | Action::DeployAdvanced { .. }
        )
    }

    pub fn is_deploy_advanced(&self) -> bool {
        matches!(self, Action::DeployAdvanced { .. })
    }

    /// The choices of an advanced deploy, empty for anything else.
    pub fn choices(&self) -> Option<&BTreeMap<String, Choice>> {
        match self {
            Action::DeployAdvanced { choices, .. } => Some(choices),
            _ => None,
        }
    }

    pub fn durability(&self) -> Option<Durability> {
        match self {
            Action::DeployAdvanced { durability, .. } => Some(*durability),
            _ => None,
        }
    }

    /// The pending patch edits of an advanced deploy; empty for anything else.
    pub fn patch_changes(&self) -> Option<&PatchChanges> {
        match self {
            Action::DeployAdvanced { patches, .. } => Some(patches),
            _ => None,
        }
    }

    /// This action with its pending patch edits replaced.
    pub fn with_patch_changes(&self, patches: PatchChanges) -> Action {
        match self {
            Action::DeployAdvanced {
                choices,
                durability,
                ..
            } => Action::DeployAdvanced {
                choices: choices.clone(),
                durability: *durability,
                patches,
            },
            other => other.clone(),
        }
    }

    /// Actions that change what is deployed: the preview shows every
    /// parameter's transition and the resources that will change.
    pub fn changes_deployment(&self) -> bool {
        !matches!(
            self,
            Action::Bounce | Action::ExecuteJob | Action::ToggleAutodeploy
        )
    }

    /// Actions a blocker refuses: everything that deploys something new.
    pub fn is_gated_by_blockers(&self) -> bool {
        self.is_deploy()
            || self.is_clear_selection()
            || self.is_set_parameter()
            || self.is_end_temporary()
    }

    /// Metrics label for the action as requested, before it is resolved
    /// against the config's state (a requested deploy of an undeployable
    /// config resolves to an undeploy, for example).
    pub fn action_type(&self) -> &'static str {
        match self {
            Action::DeployLatest | Action::DeployBranch { .. } | Action::DeployCommit { .. } => {
                "deploy"
            }
            Action::DeployAdvanced { .. } => "deploy_advanced",
            Action::Rollback { .. } => "rollback",
            Action::ClearSelection => "clear_selection",
            Action::EndTemporary => "end_temporary",
            Action::SetParameter { .. } => "set_parameter",
            Action::Undeploy => "undeploy",
            Action::Bounce => "bounce",
            Action::ExecuteJob => "execute_job",
            Action::ToggleAutodeploy => "toggle_autodeploy",
        }
    }

    pub fn is_clear_selection(&self) -> bool {
        matches!(self, Action::ClearSelection)
    }

    pub fn is_set_parameter(&self) -> bool {
        matches!(self, Action::SetParameter { .. })
    }

    pub fn is_end_temporary(&self) -> bool {
        matches!(self, Action::EndTemporary)
    }

    pub fn is_toggle_autodeploy(&self) -> bool {
        matches!(self, Action::ToggleAutodeploy)
    }

    pub fn is_undeploy(&self) -> bool {
        matches!(self, Action::Undeploy)
    }

    pub fn is_bounce(&self) -> bool {
        matches!(self, Action::Bounce)
    }

    pub fn is_execute_job(&self) -> bool {
        matches!(self, Action::ExecuteJob)
    }

    /// Actions that resolve the SHA parameter through its current selection.
    pub fn deploys_latest(&self) -> bool {
        matches!(
            self,
            Action::DeployLatest
                | Action::DeployAdvanced { .. }
                | Action::SetParameter { .. }
                | Action::EndTemporary
        )
    }

    /// The SHA selection this action resolves against. Ending a temporary
    /// deployment drops a temporary selection before resolving, so a
    /// preview of it shows the default channel rather than the override.
    pub fn effective_sha_selection(&self, config: &DeployConfig) -> Selection {
        let selection = config.selection(SHA_PARAMETER);
        if self.is_end_temporary() && selection.is_temporary() {
            Selection::default()
        } else {
            selection
        }
    }

    /// The config as this action would leave its selections and patches,
    /// before anything is deployed: what the preview resolves against. The
    /// same changes [`crate::deploys::run_action`] persists after the deploy.
    pub fn effective_config(&self, config: &DeployConfig) -> DeployConfig {
        let mut effective = config.clone();
        if self.is_end_temporary() {
            let temporary = crate::deploys::temporary_changes(config);
            for parameter in &temporary.selections {
                effective.spec.spec.selections.remove(parameter);
            }
            if let Some(patches) = temporary.patches {
                effective.spec.spec.patches = patches;
            }
            return effective;
        }
        for (parameter, selection) in
            crate::deploys::selection_changes(self, config, &SelectionIntent::default())
        {
            match selection {
                Some(s) => {
                    effective.spec.spec.selections.insert(parameter, s);
                }
                None => {
                    effective.spec.spec.selections.remove(&parameter);
                }
            }
        }
        if let Action::DeployAdvanced {
            patches,
            durability,
            ..
        } = self
        {
            if !patches.is_empty() {
                effective.spec.spec.patches =
                    patches.apply_to(&config.spec.spec.patches, *durability);
            }
        }
        effective
    }

    /// The preview heading, up to the config name.
    pub fn title(&self) -> String {
        match self {
            Action::DeployLatest | Action::DeployAdvanced { .. } => "Deploy of ".to_string(),
            Action::DeployBranch { .. } => "Branch deploy of ".to_string(),
            Action::DeployCommit { .. } => "Commit deploy of ".to_string(),
            Action::Bounce => "Bounce of ".to_string(),
            Action::ExecuteJob => "Manual execution of ".to_string(),
            Action::ToggleAutodeploy => "Option change for ".to_string(),
            Action::Undeploy => "Undeploy of ".to_string(),
            Action::Rollback { revision } => format!("Rollback to revision {} of ", revision),
            Action::ClearSelection => "Back to the default branch for ".to_string(),
            Action::EndTemporary => "End of the temporary deployment of ".to_string(),
            Action::SetParameter { parameter, .. } => format!("Set ${} on ", parameter),
        }
    }

    /// The submit button's label.
    pub fn button_label(&self, config: &DeployConfig) -> String {
        match self {
            Action::DeployLatest | Action::DeployBranch { .. } | Action::DeployCommit { .. } => {
                "Deploy".to_string()
            }
            Action::DeployAdvanced { .. } => "Deploy advanced".to_string(),
            Action::ToggleAutodeploy => {
                if config.autodeploy() {
                    "Disable autodeploy".to_string()
                } else {
                    "Enable autodeploy".to_string()
                }
            }
            Action::Bounce => "Bounce".to_string(),
            Action::ExecuteJob => "Execute job".to_string(),
            Action::Undeploy => "Undeploy".to_string(),
            Action::Rollback { .. } => "Roll back".to_string(),
            Action::ClearSelection => "Clear and deploy latest".to_string(),
            Action::EndTemporary => "End temporary deployment".to_string(),
            Action::SetParameter { .. } => "Set and deploy".to_string(),
        }
    }
}

/// The durability the form sent; temporary unless it said standing.
pub fn durability_from_query(query: &HashMap<String, String>) -> Durability {
    match query.get("durability").map(|d| d.trim()) {
        Some("standing") => Durability::Standing,
        _ => Durability::Temporary,
    }
}

/// `value_<P>` fields: tags typed for this one deploy.
pub fn typed_values(query: &HashMap<String, String>) -> BTreeMap<String, String> {
    query
        .iter()
        .filter_map(|(k, v)| {
            let name = k.strip_prefix("value_")?;
            let v = v.trim();
            (!name.is_empty() && !v.is_empty()).then(|| (name.to_string(), v.to_string()))
        })
        .collect()
}

/// Handler for the deploy configs page
#[get("/deploy")]
pub async fn deploy_configs(
    req: actix_web::HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    query: web::Query<std::collections::HashMap<String, String>>,
    octocrabs: web::Data<Octocrabs>,
) -> impl Responder {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Failed to connect to database");
        }
    };

    // Initialize Kubernetes client
    // FIXME: Should this come from web::Data?
    let client = match Client::try_default().await {
        Ok(client) => client,
        Err(e) => {
            log::error!("Failed to initialize Kubernetes client: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to connect to Kubernetes".to_string());
        }
    };

    let deploy_configs = match get_all_deploy_configs(&client).await {
        Ok(deploy_configs) => deploy_configs,
        Err(e) => {
            log::error!("Failed to get all deploy configs: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to get all deploy configs".to_string());
        }
    };

    let teams_cookie = TeamsCookie::from_request(&req);
    let deploy_configs = teams_cookie.filter_configs(&deploy_configs);

    let action = Action::from_query(&query);
    let typed = typed_values(&query);

    // Sort DeployConfigs by namespace and name for the dropdown
    let mut sorted_deploy_configs = deploy_configs.clone();
    sorted_deploy_configs.sort_by(|a, b| {
        let a_name = a.name_any();
        let b_name = b.name_any();

        a_name.cmp(&b_name)
    });

    // Check if we have a selected config from query parameter
    let selected_config_key = query.get("selected");

    // Find the selected deploy config or use the first one as default
    let selected_config = if let Some(name) = selected_config_key {
        // Find the matching config
        sorted_deploy_configs
            .iter()
            .find(|config| &config.name_any() == name)
    } else {
        sorted_deploy_configs.first()
    };

    let namespaced_objs = if let Some(selected_config) = selected_config {
        match list_namespace_objects(
            &client,
            &selected_config.namespace().unwrap_or("default".to_string()),
            ListMode::All,
        )
        .await
        {
            Ok(objs) => objs,
            Err(e) => {
                log::error!("Failed to get namespaced objects: {}", e);
                vec![]
            }
        }
    } else {
        vec![]
    };

    let temporary_strip = crate::web::selections::render_temporary_strip(&deploy_configs);
    let held_strip = match Blocker::all_active(&conn) {
        Ok(blockers) => crate::web::blockers::render_held_strip(&blockers),
        Err(e) => {
            log::warn!("Failed to load active blockers: {}", e);
            html! {}
        }
    };

    let mut left_column = html! {};
    let mut right_column = html! {};
    if let Some(config) = selected_config {
        let blockers = Blocker::active_for(&conn, &config.name_any()).unwrap_or_default();
        let prepared = preview::prepare(&conn, config, &action);
        let resolved = preview::resolve_tags(&prepared, &typed).await;
        left_column = deploy_form::render(
            config,
            &sorted_deploy_configs,
            &prepared,
            &query,
            !blockers.is_empty(),
            &resolved,
            &conn,
        );
        right_column = preview::render_page_preview(
            &prepared,
            &conn,
            &client,
            Some(&octocrabs),
            &namespaced_objs,
            &typed,
            &resolved,
        )
        .await;
    }

    // Render the HTML template using Maud
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { "Deploy" }
                (header::stylesheet_link())
                (header::scripts())
            }
            body.deploy-page hx-ext="morph" {
                (header::render("deploy"))
                (held_strip)
                (temporary_strip)
                div class="content" {
                    @if sorted_deploy_configs.is_empty() {
                        div class="empty-state" {
                            h2 { "No deploy configs" }
                            p { "There are no deploy configs in the cluster." }
                        }
                    } @else {
                        div class="content-container" {
                            (left_column)
                            (right_column)
                        }
                    }
                }
            }
        }
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(markup.into_string())
}

/// Handler for updating a DeployConfig
#[post("/api/deploy/{namespace}/{name}")]
pub async fn deploy_config(
    path: web::Path<(String, String)>,
    client: Option<web::Data<Client>>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    form: web::Form<HashMap<String, String>>,
    octocrabs: web::Data<Octocrabs>,
) -> impl Responder {
    let action = Action::from_query(&form);
    let (namespace, name) = path.into_inner();

    // Check if Kubernetes client is available
    let client = match client {
        Some(client) => client,
        None => {
            return HttpResponse::ServiceUnavailable()
                .content_type("text/html; charset=utf-8")
                .body("Kubernetes client is not available. Deploy functionality is disabled.");
        }
    };

    // Get the DeployConfig
    let config = match get_deploy_config(&client, &name).await {
        Ok(Some(config)) => config,
        Ok(None) => {
            return HttpResponse::NotFound()
                .body(format!("DeployConfig {}/{} not found.", namespace, name));
        }
        Err(e) => {
            log::error!("Failed to get DeployConfig {}/{}: {}", namespace, name, e);
            return HttpResponse::NotFound()
                .body(format!("DeployConfig {}/{} not found.", namespace, name));
        }
    };

    // Reject non-undeploy actions for orphaned configs
    if config.is_orphaned() && !matches!(&action, Action::Undeploy) {
        return HttpResponse::BadRequest()
            .content_type("text/html; charset=utf-8")
            .body("Cannot perform this action on an orphaned deploy config. Only undeploy is allowed.");
    }

    // An advanced deploy lands back in simple mode: its choices are now the
    // config's selections and the plain preview shows them.
    let return_url = if action.is_deploy_advanced() {
        format!("/deploy?selected={}", url_encode(&name))
    } else {
        format!(
            "/deploy?selected={}&action={}&branch={}&sha={}",
            url_encode(&name),
            form.get("action").unwrap_or(&"".to_string()),
            url_encode(form.get("branch").unwrap_or(&"".to_string())),
            url_encode(form.get("sha").unwrap_or(&"".to_string()))
        )
    };

    let mut intent = crate::deploys::SelectionIntent::from_form(&form);
    if action.is_deploy_advanced() {
        // The form asks neither why nor who; the durability is the action's.
        intent.durability = action.durability();
        intent.note = Some("advanced deploy".to_string());
        intent.by = Some("web".to_string());
    }
    let result =
        crate::deploys::run_action(&action, &config, &client, &octocrabs, &pool, "web", &intent)
            .await;
    let (action_type, outcome) = match &result {
        Ok(deploy_action) => (deploy_action.action_type(), "success"),
        Err(_) => (action.action_type(), "error"),
    };
    crate::metrics::get().deploy_actions.add(
        1,
        &[
            opentelemetry::KeyValue::new("name", name.clone()),
            opentelemetry::KeyValue::new("action", action_type),
            opentelemetry::KeyValue::new("result", outcome),
        ],
    );
    match result {
        Ok(_) => {}
        Err(AppError::Blocked(message)) => {
            return HttpResponse::Conflict()
                .content_type("text/html; charset=utf-8")
                .body(message);
        }
        Err(AppError::InvalidInput(message)) => {
            return HttpResponse::BadRequest()
                .content_type("text/html; charset=utf-8")
                .body(message);
        }
        Err(AppError::Unavailable(message)) => {
            return HttpResponse::ServiceUnavailable()
                .content_type("text/html; charset=utf-8")
                .body(message);
        }
        Err(e) => {
            log::error!("Failed to execute deploy action on {}: {}", name, e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body(format!("Failed to execute deploy action: {}", e));
        }
    }

    // Redirect back to the DeployConfig page with the selected config
    HttpResponse::SeeOther()
        .append_header(("Location", return_url))
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn advanced_choices_round_trip_through_the_query_string() {
        let q = query(&[
            ("action", "deploy-advanced"),
            ("durability", "standing"),
            ("sel_SHA", "track"),
            ("track_SHA", "fix/upload timeout"),
            ("sel_NGINX", "pin"),
            ("pin_NGINX", "1.27.4"),
            ("sel_REPLICAS", "default"),
            ("sel_", "pin"),
        ]);
        let action = Action::from_query(&q);
        let Action::DeployAdvanced {
            choices,
            durability,
            patches,
        } = &action
        else {
            panic!("expected an advanced deploy, got {action:?}");
        };
        assert!(patches.is_empty());
        assert_eq!(*durability, Durability::Standing);
        assert_eq!(choices.len(), 3, "the empty name is ignored");
        assert_eq!(
            choices["SHA"],
            Choice::Track("fix/upload timeout".into()),
            "typed channels are trimmed, not mangled"
        );
        assert_eq!(choices["NGINX"], Choice::Pin("1.27.4".into()));
        assert_eq!(choices["REPLICAS"], Choice::Default);

        let params = action.as_params();
        let reparsed: HashMap<String, String> = url::form_urlencoded::parse(params.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(Action::from_query(&reparsed), action);
        assert!(action.is_deploy() && action.is_deploy_advanced());
        assert!(action.deploys_latest());
        assert_eq!(action.form_value(), "deploy-advanced");
    }

    #[test]
    fn missing_typed_values_are_empty_choices() {
        let q = query(&[("action", "deploy-advanced"), ("sel_SHA", "pin")]);
        match Action::from_query(&q) {
            Action::DeployAdvanced {
                choices,
                durability,
                ..
            } => {
                assert_eq!(choices["SHA"], Choice::Pin(String::new()));
                assert_eq!(durability, Durability::Temporary, "temporary by default");
            }
            other => panic!("expected an advanced deploy, got {other:?}"),
        }
    }

    #[test]
    fn pending_patches_ride_along_in_the_query_string() {
        use crate::kubernetes::patches::{ManifestPatch, PatchOp, PatchTarget};
        let changes = PatchChanges {
            remove: vec![1],
            add: vec![ManifestPatch {
                target: PatchTarget {
                    file: None,
                    kind: "Deployment".into(),
                    name: "web".into(),
                },
                op: PatchOp::Replace,
                path: "/spec/replicas".into(),
                value: Some(serde_json::json!(3)),
                durability: Durability::Temporary,
                note: None,
                by: None,
                since: None,
            }],
        };
        let action = Action::DeployAdvanced {
            choices: BTreeMap::new(),
            durability: Durability::Standing,
            patches: changes.clone(),
        };
        let reparsed: HashMap<String, String> =
            url::form_urlencoded::parse(action.as_params().as_bytes())
                .into_owned()
                .collect();
        assert_eq!(Action::from_query(&reparsed), action);
        assert_eq!(action.patch_changes(), Some(&changes));

        let broken = query(&[("action", "deploy-advanced"), ("patches", "{nope")]);
        assert!(Action::from_query(&broken)
            .patch_changes()
            .is_some_and(PatchChanges::is_empty));
        let cleared = action.with_patch_changes(PatchChanges::default());
        assert!(cleared.patch_changes().is_some_and(PatchChanges::is_empty));
    }

    #[test]
    fn typed_values_come_from_value_fields() {
        let q = query(&[
            ("value_NGINX", " 1.27.5 "),
            ("value_", "x"),
            ("value_EMPTY", " "),
        ]);
        let typed = typed_values(&q);
        assert_eq!(typed.len(), 1);
        assert_eq!(typed["NGINX"], "1.27.5");
    }

    #[test]
    fn blockers_gate_everything_that_deploys() {
        assert!(Action::DeployLatest.is_gated_by_blockers());
        assert!(Action::DeployAdvanced {
            choices: BTreeMap::new(),
            durability: Durability::Temporary,
            patches: Default::default(),
        }
        .is_gated_by_blockers());
        assert!(Action::EndTemporary.is_gated_by_blockers());
        assert!(!Action::Undeploy.is_gated_by_blockers());
        assert!(!Action::Bounce.is_gated_by_blockers());
        assert!(!Action::ToggleAutodeploy.is_gated_by_blockers());
        assert!(!Action::Rollback { revision: 1 }.is_gated_by_blockers());
    }
}
