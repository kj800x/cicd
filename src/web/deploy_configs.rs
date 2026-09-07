#![allow(clippy::expect_used)]

use crate::crab_ext::Octocrabs;
use crate::db::blocker::Blocker;
use crate::db::deploy_config_version::DeployConfigVersion;
use crate::db::git_branch::GitBranch;
use crate::db::git_commit::GitCommit;
use crate::db::git_repo::GitRepo;
use crate::db::revision::Revision;
use crate::kubernetes::api::{
    get_all_deploy_configs, get_deploy_config, get_namespace_uid, ListMode,
};
use crate::kubernetes::parameters::SHA_PARAMETER;
use crate::kubernetes::repo::{DeploymentState, ShaMaybeBranch};
use crate::kubernetes::selections::Mode;
use crate::kubernetes::{list_namespace_objects, DeployConfig};
use crate::prelude::*;
use crate::web::team_prefs::TeamsCookie;
use crate::web::{build_status, deploy_status, header, ResourceStatuses};
use kube::api::DynamicObject;
use kube::{Client, ResourceExt};
use maud::{html, Markup, Render};
use std::collections::HashMap;

// FIXME: Make this configurable.
const HEADLAMP_URL: &str = "https://headlamp.home.coolkev.com";

struct PreviewArrow;

impl Render for PreviewArrow {
    fn render(&self) -> Markup {
        html!(span.preview-arrow { "⇨" })
    }
}

struct GitRef(String, String, String, bool, Option<String>);

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
                a.git-ref href=(format!("https://github.com/{}/{}/tree/{}{}", owner, repo, sha, file_path.map(|path| format!("/{}", path)).unwrap_or_default())) target="_blank" {
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

struct AutodeployStatus(bool);
impl Render for AutodeployStatus {
    fn render(&self) -> Markup {
        if self.0 {
            html!(
                span.autodeploy-status.autodeploy-enabled {
                    "Enabled"
                }
            )
        } else {
            html!(
                span.autodeploy-status.autodeploy-disabled {
                    "Disabled"
                }
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BuildFilter {
    Any,
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
            Action::DeployLatest | Action::ClearSelection => {
                let selection = config.selection(SHA_PARAMETER);
                let branch_name: &str = match (action, selection.mode()) {
                    (Action::DeployLatest, Mode::Pin(value)) => {
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
                    (Action::DeployLatest, Mode::Track(branch)) => branch,
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

trait FormatStates {
    fn format_config(&self, owner: &str, repo: &str, config_name: &str) -> Markup;
    fn format_artifact(&self, owner: &str, repo: &str) -> Markup;
}

impl FormatStates for DeploymentState {
    fn format_config(&self, owner: &str, repo: &str, config_name: &str) -> Markup {
        match self {
            DeploymentState::DeployedWithArtifact { config, .. }
            | DeploymentState::DeployedOnlyConfig { config } => {
                html! {
                    (GitRef(config.sha.clone(), owner.to_string(), repo.to_string(), false, Some(format!(".deploy/{}", config_name))))
                }
            }
            DeploymentState::Undeployed => html! { "Undeployed" },
        }
    }

    fn format_artifact(&self, owner: &str, repo: &str) -> Markup {
        match self {
            DeploymentState::DeployedWithArtifact { artifact, .. } => {
                html! {
                    (GitRef(artifact.sha.clone(), owner.to_string(), repo.to_string(), false, None))
                }
            }
            DeploymentState::DeployedOnlyConfig { .. } => {
                html! {
                    span { "No artifact" }
                }
            }
            DeploymentState::Undeployed => html! { "Undeployed" },
        }
    }
}

impl FormatStates for AppResult<DeploymentState> {
    fn format_config(&self, owner: &str, repo: &str, config_name: &str) -> Markup {
        match self {
            Ok(deployment_state) => deployment_state.format_config(owner, repo, config_name),
            Err(_) => {
                html! {
                    span { "[resolution failed]" }
                }
            }
        }
    }

    fn format_artifact(&self, owner: &str, repo: &str) -> Markup {
        match self {
            Ok(deployment_state) => deployment_state.format_artifact(owner, repo),
            Err(_) => {
                html! {
                    span { "[resolution failed]" }
                }
            }
        }
    }
}

/// Represents a transition between two resolved versions
struct DeployTransition {
    from: DeploymentState,
    to: AppResult<DeploymentState>,
    current_config: DeployConfig,
    /// Whether the kube manifests actually changed between from and to config SHAs.
    /// None means we couldn't determine (e.g. hash not yet synced to DB).
    config_manifest_changed: Option<bool>,
}

impl DeployTransition {
    fn compare_url(&self, owner: &str, repo: &str) -> Option<String> {
        match (&self.from, &self.to) {
            (
                DeploymentState::DeployedWithArtifact { artifact, .. },
                Ok(DeploymentState::DeployedWithArtifact {
                    artifact: other_artifact,
                    ..
                }),
            ) => Some(format!(
                "https://github.com/{}/{}/compare/{}...{}",
                owner, repo, artifact.sha, other_artifact.sha
            )),
            _ => None,
        }
    }

    /// Formats the transition for display
    async fn format(&self, owner: &str, repo: &str) -> Markup {
        let config_owner = self.current_config.config_repository().owner.clone();
        let config_repo = self.current_config.config_repository().repo.clone();
        if self.to == Ok(self.from.clone()) {
            match self.from.clone() {
                DeploymentState::Undeployed => {
                    html! {
                        div { "Already undeployed"}
                    }
                }
                DeploymentState::DeployedWithArtifact { artifact, config } => {
                    html! {
                        div {
                            .icon {
                                span.deploy-config__icon.m-right-1 {}
                            }
                            (GitRef(config.sha.clone(), config_owner.clone(), config_repo.clone(), false, Some(format!(".deploy/{}", &self.current_config.name_any()))))
                        }
                        div {
                            .icon {
                                i.octicon.octicon-git-commit {}
                            }
                            (GitRef(artifact.sha.clone(), owner.to_string(), repo.to_string(), false, None))
                        }
                    }
                }
                DeploymentState::DeployedOnlyConfig { config } => {
                    html! {
                        div {
                            .icon {
                                span.deploy-config__icon.m-right-1 {}
                            }
                            (GitRef(config.sha.clone(), config_owner.clone(), config_repo.clone(), false, Some(format!(".deploy/{}", &self.current_config.name_any()))))
                        }
                    }
                }
            }
        } else {
            html! {
                div {
                    .icon {
                        span.deploy-config__icon.m-right-1 {}
                    }
                    (self.from.format_config(&config_owner, &config_repo, &self.current_config.name_any()))
                    ( PreviewArrow {} )
                    (self.to.format_config(&config_owner, &config_repo, &self.current_config.name_any()))
                    @if self.config_manifest_changed == Some(true) {
                        " "
                        span style="color: var(--warning-color); font-weight: 600;" { "[CONFIG CHANGED]" }
                    }
                }
                div {
                    .icon {
                        i.octicon.octicon-git-commit {}
                    }
                    (self.from.format_artifact(owner, repo))
                    ( PreviewArrow {} )
                    (self.to.format_artifact(owner, repo))

                    @if let Some(compare_url) = self.compare_url(owner, repo) {
                        " "
                        a.git-ref href=(compare_url) target="_blank" {
                            "[compare]"
                        }
                    }
                }
            }
        }
    }
}

/// Generate the status header showing current branch and autodeploy status
async fn generate_status_header(
    config: &DeployConfig,
    owner: &str,
    repo: &str,
    client: &Client,
    active_blockers: usize,
) -> Markup {
    let default_branch = config
        .artifact_repository()
        .unwrap_or_else(|| config.config_repository().with_branch("master"))
        .branch;

    // FIXME: What about artifactless configs?
    let current_branch = match config.deployment_state() {
        DeploymentState::DeployedWithArtifact { artifact, .. } => artifact.branch.clone(),
        DeploymentState::DeployedOnlyConfig { config } => config.branch.clone(),
        DeploymentState::Undeployed => None,
    };

    let namespace = config.namespace().unwrap_or("default".to_string());
    let namespace_uid = get_namespace_uid(client, &namespace)
        .await
        .unwrap_or_default();

    html! {
        div class="status-header" {
            div class="status-item" {
                "Tracking branch: "
                strong {
                    @match current_branch {
                        Some(branch) => {
                            (GitRef(
                                branch.to_string(),
                                owner.to_string(),
                                repo.to_string(),
                                true,
                                None,
                            ))
                            @if branch != default_branch {
                                span class="warning-icon" title=(format!("Different from default branch ({})", default_branch)) {
                                    i class="fa fa-exclamation-triangle" {}
                                }
                            }
                        }
                        None => {
                            "None"
                        }
                    }

                }
            }
            div class="status-item" {
                "Autodeploy: "
                strong {
                    @if config.autodeploy() {
                        (AutodeployStatus(true))
                    } @else {
                        (AutodeployStatus(false))
                    }
                }
            }
            div class="status-item" {
                "Held: "
                strong {
                    @if active_blockers == 0 {
                        "No"
                    } @else if active_blockers == 1 {
                        "Yes, 1 blocker"
                    } @else {
                        (format!("Yes, {} blockers", active_blockers))
                    }
                }
            }
            div class="status-item" {
                "Namespace: "
                strong {
                    a href=(format!("{}/c/main/map?group=namespace&node={}", HEADLAMP_URL, namespace_uid)) target="_blank" {
                        (namespace)
                    }
                }
            }
        }
    }
}

impl ShaMaybeBranch {}

impl DeploymentState {
    pub fn from_action(
        action: &Action,
        config: &DeployConfig,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> AppResult<Self> {
        let artifact_repository = config.artifact_repository();

        match (action, artifact_repository) {
            (Action::DeployLatest, Some(artifact_repository))
            | (Action::ClearSelection, Some(artifact_repository)) => {
                // "Latest" means latest according to the parameter's selection:
                // the default channel, an override branch, or, for a pin, the
                // pin itself. Clearing the selection always means the default.
                let selection = config.selection(SHA_PARAMETER);
                let branch_name: &str = match (action, selection.mode()) {
                    (Action::DeployLatest, Mode::Pin(value)) => {
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
                    (Action::DeployLatest, Mode::Track(branch)) => branch,
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
            (Action::DeployLatest, None) | (Action::ClearSelection, None) => {
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

pub async fn render_preview_content(
    selected_config: &DeployConfig,
    action: &Action,
    conn: &PooledConnection<SqliteConnectionManager>,
    namespaced_objs: &[DynamicObject],
) -> Markup {
    let owner = selected_config
        .artifact_repository()
        .unwrap_or_else(|| selected_config.config_repository().with_branch("master"))
        .owner
        .to_string();
    let repo = selected_config
        .artifact_repository()
        .unwrap_or_else(|| selected_config.config_repository().with_branch("master"))
        .repo
        .to_string();

    let from = selected_config.deployment_state();
    let to = DeploymentState::from_action(action, selected_config, conn);

    let config_manifest_changed = (|| -> Option<bool> {
        let cfg_repo = selected_config.config_repository();
        let repo_id = GitRepo::get_by_name(&cfg_repo.owner, &cfg_repo.repo, conn)
            .ok()??
            .id;
        let name = selected_config.name_any();
        let from_sha = match &from {
            DeploymentState::DeployedWithArtifact { config, .. } => Some(config.sha.as_str()),
            DeploymentState::DeployedOnlyConfig { config } => Some(config.sha.as_str()),
            DeploymentState::Undeployed => None,
        };
        let to_sha = match to.as_ref().ok()? {
            DeploymentState::DeployedWithArtifact { config, .. } => Some(config.sha.as_str()),
            DeploymentState::DeployedOnlyConfig { config } => Some(config.sha.as_str()),
            DeploymentState::Undeployed => None,
        };
        let from_hash = from_sha.and_then(|sha| {
            DeployConfigVersion::get_hash(&name, repo_id, sha, conn)
                .ok()
                .flatten()
        });
        let to_hash = to_sha.and_then(|sha| {
            DeployConfigVersion::get_hash(&name, repo_id, sha, conn)
                .ok()
                .flatten()
        });
        match (from_hash, to_hash) {
            (Some(fh), Some(th)) => Some(fh != th),
            _ => None,
        }
    })();

    let deploy_transition = DeployTransition {
        from,
        to,
        current_config: selected_config.clone(),
        config_manifest_changed,
    };

    let preview_content = match action {
        Action::DeployLatest
        | Action::DeployBranch { .. }
        | Action::DeployCommit { .. }
        | Action::Rollback { .. }
        | Action::ClearSelection
        | Action::Undeploy => deploy_transition.format(&owner, &repo).await,
        Action::Bounce => {
            html! {
                // TODO:
                "Bounce deployments in "
                (selected_config.name_any())
            }
        }
        Action::ExecuteJob => {
            html! {
                // TODO:
                "Manual execution of "
                (selected_config.name_any())
            }
        }
        Action::ToggleAutodeploy => {
            html! {
                "Autodeploy "
                @if selected_config.autodeploy() {
                    (AutodeployStatus(true))
                    ( PreviewArrow {} )
                    (AutodeployStatus(false))
                } @else {
                    (AutodeployStatus(false))
                    ( PreviewArrow {} )
                    (AutodeployStatus(true))
                }
            }
        }
    };

    let mut alerts: Vec<Markup> = vec![];
    for alert in deploy_status(selected_config, namespaced_objs).await {
        alerts.push(alert);
    }
    for alert in build_status(action, selected_config, conn).await {
        alerts.push(alert);
    }
    match Blocker::active_for(conn, &selected_config.name_any()) {
        Ok(blockers) if !blockers.is_empty() => {
            alerts.push(crate::web::blockers::render_blocker_alert(&blockers))
        }
        Ok(_) => {}
        Err(e) => log::warn!("Failed to load blockers for preview: {}", e),
    }

    html! {
        @for alert in alerts {
            (alert)
        }
        div class="preview-transition" {
            div class="deployable-item__content" {
                (selected_config.name_any())
                .deployable-item-info {
                    (preview_content)
                }
            }
            (selected_config.format_resources(namespaced_objs).await)
        }
    }
}

/// Generate the preview markup for a deploy config action
async fn generate_preview(
    selected_config: &DeployConfig,
    action: &Action,
    conn: &PooledConnection<SqliteConnectionManager>,
    client: &Client,
    namespaced_objs: &[DynamicObject],
) -> Markup {
    let owner = selected_config
        .artifact_repository()
        .unwrap_or_else(|| selected_config.config_repository().with_branch("master"))
        .owner
        .to_string();
    let repo = selected_config
        .artifact_repository()
        .unwrap_or_else(|| selected_config.config_repository().with_branch("master"))
        .repo
        .to_string();

    let active_blockers = Blocker::active_for(conn, &selected_config.name_any())
        .map(|b| b.len())
        .unwrap_or(0);

    // Wrap the preview content in the container markup
    html! {
        div class="preview-container" {
            div class="preview-content" {
                (generate_status_header(selected_config, &owner, &repo, client, active_blockers).await)

                div.preview-content-poll-wrapper hx-get=(format!("/fragments/deploy-preview/{}/{}?{}", selected_config.namespace().unwrap_or("default".to_string()), selected_config.name_any(), action.as_params())) hx-trigger="load, every 2s" hx-swap="morph:innerHTML" {
                    (render_preview_content(selected_config, action, conn, namespaced_objs).await)
                }
            }
        }
    }
}

pub enum Action {
    DeployLatest,
    DeployBranch {
        branch: String,
    },
    DeployCommit {
        sha: String,
    },
    /// Replay an earlier revision's deployed values, then hold the config
    /// with a blocker.
    Rollback {
        revision: i64,
    },
    /// Drop the SHA parameter's override or pin and deploy the latest of
    /// its default channel.
    ClearSelection,
    Bounce,
    ExecuteJob,
    ToggleAutodeploy,
    Undeploy,
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
            "rollback" => match query.get("revision").and_then(|r| r.parse::<i64>().ok()) {
                Some(revision) => Action::Rollback { revision },
                None => Action::DeployLatest,
            },
            "clear-selection" => Action::ClearSelection,
            "toggle-autodeploy" => Action::ToggleAutodeploy,
            "undeploy" => Action::Undeploy,
            "bounce" => Action::Bounce,
            "execute-job" => Action::ExecuteJob,
            _ => Action::DeployLatest,
        }
    }

    pub fn as_params(&self) -> String {
        match self {
            Action::DeployLatest => "action=deploy".to_string(),
            Action::DeployBranch { branch } => format!("action=deploy&branch={}", branch),
            Action::DeployCommit { sha } => format!("action=deploy&sha={}", sha),
            Action::Rollback { revision } => format!("action=rollback&revision={}", revision),
            Action::ClearSelection => "action=clear-selection".to_string(),
            Action::Bounce => "action=bounce".to_string(),
            Action::ExecuteJob => "action=execute-job".to_string(),
            Action::ToggleAutodeploy => "action=toggle-autodeploy".to_string(),
            Action::Undeploy => "action=undeploy".to_string(),
        }
    }

    pub fn is_deploy(&self) -> bool {
        matches!(
            self,
            Action::DeployLatest | Action::DeployBranch { .. } | Action::DeployCommit { .. }
        )
    }

    /// Metrics label for the action as requested, before it is resolved
    /// against the config's state (a requested deploy of an undeployable
    /// config resolves to an undeploy, for example).
    pub fn action_type(&self) -> &'static str {
        match self {
            Action::DeployLatest | Action::DeployBranch { .. } | Action::DeployCommit { .. } => {
                "deploy"
            }
            Action::Rollback { .. } => "rollback",
            Action::ClearSelection => "clear_selection",
            Action::Undeploy => "undeploy",
            Action::Bounce => "bounce",
            Action::ExecuteJob => "execute_job",
            Action::ToggleAutodeploy => "toggle_autodeploy",
        }
    }

    pub fn is_clear_selection(&self) -> bool {
        matches!(self, Action::ClearSelection)
    }

    fn is_toggle_autodeploy(&self) -> bool {
        matches!(self, Action::ToggleAutodeploy)
    }

    fn is_undeploy(&self) -> bool {
        matches!(self, Action::Undeploy)
    }

    fn is_bounce(&self) -> bool {
        matches!(self, Action::Bounce)
    }

    fn is_execute_job(&self) -> bool {
        matches!(self, Action::ExecuteJob)
    }
}

/// Handler for the deploy configs page
#[get("/deploy")]
pub async fn deploy_configs(
    req: actix_web::HttpRequest,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    query: web::Query<std::collections::HashMap<String, String>>,
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

    let held_strip = match Blocker::all_active(&conn) {
        Ok(blockers) => crate::web::blockers::render_held_strip(&blockers),
        Err(e) => {
            log::warn!("Failed to load active blockers: {}", e);
            html! {}
        }
    };
    let blocker_panel = match selected_config {
        Some(config) => {
            let name = config.name_any();
            let blockers = Blocker::active_for(&conn, &name).unwrap_or_default();
            let return_url = format!(
                "/deploy?selected={}&{}",
                name,
                Action::from_query(&query).as_params()
            );
            crate::web::blockers::render_blocker_panel(&blockers, &name, &return_url)
        }
        None => html! {},
    };

    // Render the HTML template using Maud
    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { "DeployConfig Dashboard" }
                (header::stylesheet_link())
                (header::scripts())
                script {
                    r#"
                    function updateSelection() {
                        const selectElement = document.getElementById('deployConfigSelect');
                        const selectedValue = selectElement.value;
                        window.location.href = '/deploy?selected=' + encodeURIComponent(selectedValue);
                    }

                    function submitActionForm() {
                        document.getElementById('actionForm').submit();
                    }
                    "#
                }
            }
            body.deploy-page hx-ext="morph" {
                (header::render("deploy"))
                div class="content" {
                (held_strip)
                @if sorted_deploy_configs.is_empty() {
                    div style="text-align:center; margin-top:40px;" {
                        h2 { "No DeployConfigs Found" }
                        p { "There are no DeployConfigs in the Kubernetes cluster." }
                    }
                } @else {
                    div class="content-container" {
                        // Left side box with dropdown and actions
                        div class="left-box" {
                                h3 { "Deploy config" }
                                form action="/deploy" method="get" {
                                    select name="selected" onchange="this.form.submit()" {
                                        @for config in &sorted_deploy_configs {
                                            @let name = config.name_any();
                                            @let selected = if let Some(default) = selected_config {
                                                default.name_any() == name
                                            } else {
                                                false
                                            };

                                            option value=(name) selected[selected] {
                                                (name)
                                            }
                                        }
                                    }
                                }

                                @if let Some(selected_config) = selected_config {
                                    @let deployment_state = selected_config.deployment_state();
                                    @let current_branch = deployment_state.artifact_branch();
                                    form action="/deploy" method="get" {
                                        input type="hidden" name="selected" value=(selected_config.name_any());

                                        div class="action-radio-group" {
                                            h4 { "Action" }
                                            @let is_orphaned = selected_config.is_orphaned();
                                            label class="action-radio" {
                                                input type="radio" name="action" value="deploy" checked[action.is_deploy()] disabled[is_orphaned] onchange="this.form.submit()";
                                                "Deploy"
                                            }
                                            label class="action-radio" {
                                                input type="radio" name="action" value="toggle-autodeploy" checked[action.is_toggle_autodeploy()] disabled[is_orphaned] onchange="this.form.submit()";
                                                @if selected_config.autodeploy() {
                                                    "Disable autodeploy"
                                                } @else {
                                                    "Enable autodeploy"
                                                }
                                            }
                                            @if selected_config.supports_bounce() {
                                                label class="action-radio" {
                                                    input type="radio" name="action" value="bounce" checked[action.is_bounce()] disabled[is_orphaned] onchange="this.form.submit()";
                                                    "Bounce"
                                                }
                                            }
                                            @if selected_config.supports_execute_job() {
                                                label class="action-radio" {
                                                    input type="radio" name="action" value="execute-job" checked[action.is_execute_job()] disabled[is_orphaned] onchange="this.form.submit()";
                                                    "Execute job"
                                                }
                                            }
                                            label class="action-radio" {
                                                input type="radio" name="action" value="undeploy" checked[action.is_undeploy()] onchange="this.form.submit()";
                                                "Undeploy"
                                            }
                                        }

                                        @if action.is_deploy() && !selected_config.is_orphaned() {
                                            div class="action-input" {
                                                label for="branch" { "Branch" }
                                                input id="branch" type="text" name="branch" placeholder="Enter branch name" value=(query.get("branch").unwrap_or(&current_branch.unwrap_or_default().to_string())) onblur="this.form.submit()";
                                            }
                                            div class="action-input" {
                                                label for="sha" { "SHA override" }
                                                input id="sha" type="text" name="sha" placeholder="Enter commit SHA" pattern="[0-9a-fA-F]{5,40}" value=(query.get("sha").unwrap_or(&"".to_string())) onblur="this.form.submit()";
                                            }
                                        }
                                    }
                                    form action=(format!("/api/deploy/{}/{}",
                                        selected_config.namespace().unwrap_or_default(),
                                        selected_config.name_any()))
                                        method="post"
                                    {
                                        input type="hidden" name="branch" value=(query.get("branch").unwrap_or(&"".to_string()));
                                        input type="hidden" name="sha" value=(query.get("sha").unwrap_or(&"".to_string()));
                                        input type="hidden" name="action" value=(query.get("action").unwrap_or(&"".to_string()));
                                        @let is_orphaned = selected_config.is_orphaned();
                                        button.primary-action-button.danger-button[action.is_undeploy()] type="submit" disabled[is_orphaned && !action.is_undeploy()] {
                                            @match action {
                                                Action::DeployLatest | Action::DeployBranch { .. } | Action::DeployCommit { .. } => {
                                                    "Deploy"
                                                }
                                                Action::ToggleAutodeploy => {
                                                    @if selected_config.autodeploy() {
                                                        "Disable autodeploy"
                                                    } @else {
                                                        "Enable autodeploy"
                                                    }
                                                }
                                                Action::Bounce => {
                                                    "Bounce"
                                                }
                                                Action::ExecuteJob => {
                                                    "Execute job"
                                                }
                                                Action::Undeploy => {
                                                    "Undeploy"
                                                }
                                                Action::Rollback { .. } => {
                                                    "Roll back"
                                                }
                                                Action::ClearSelection => {
                                                    "Clear and deploy latest"
                                                }
                                            }
                                        }
                                    }
                                    (blocker_panel)
                                }
                            }

                            // Right side box with preview
                            @if let Some(selected_config) = selected_config {
                                div class="right-box" {
                                    h1 {
                                        @match action {
                                            Action::DeployLatest => {
                                                "Deploy of "
                                            }
                                            Action::DeployBranch { .. } => {
                                                "Branch deploy of "
                                            }
                                            Action::DeployCommit { .. } => {
                                                "Commit deploy of "
                                            }
                                            Action::Bounce => {
                                                "Bounce deployments in "
                                            }
                                            Action::ExecuteJob => {
                                                "Manual execution of "
                                            }
                                            Action::ToggleAutodeploy => {
                                                "Option change for "
                                            }
                                            Action::Undeploy => {
                                                "Undeploy of "
                                            }
                                            Action::Rollback { revision } => {
                                                (format!("Rollback to revision {} of ", revision))
                                            }
                                            Action::ClearSelection => {
                                                "Back to the default branch for "
                                            }
                                        }
                                        strong {
                                            (format!("{}", selected_config.name_any()))
                                        }
                                    }
                                    @if selected_config.is_orphaned() {
                                        div.alert.alert-warning {
                                            div class="alert-header" {
                                                i class="fa fa-exclamation-triangle" {}
                                                " Orphaned Deploy Config"
                                            }
                                            div class="alert-content" {
                                                div class="details" {
                                                    "This deploy config has been deleted from the config repository but is still deployed. Only undeploy is available."
                                                }
                                            }
                                        }
                                    }
                                    (generate_preview(selected_config, &action, &conn, &client, &namespaced_objs).await)
                                }
                            }
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
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Failed to connect to database");
        }
    };
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

    let return_url = format!(
        "/deploy?selected={}&action={}&branch={}&sha={}",
        name,
        form.get("action").unwrap_or(&"".to_string()),
        form.get("branch").unwrap_or(&"".to_string()),
        form.get("sha").unwrap_or(&"".to_string())
    );

    let intent = crate::deploys::SelectionIntent::from_form(&form);
    let result =
        crate::deploys::run_action(&action, &config, &client, &octocrabs, &conn, "web", &intent)
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
