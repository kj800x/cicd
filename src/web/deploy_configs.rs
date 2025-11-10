#![allow(clippy::expect_used)]

use crate::crab_ext::Octocrabs;
use crate::db::deploy_event::DeployEvent;
use crate::db::git_branch::GitBranch;
use crate::db::git_commit::GitCommit;
use crate::db::git_repo::GitRepo;
use crate::kubernetes::api::{get_all_deploy_configs, get_deploy_config};
use crate::kubernetes::deploy_handlers::DeployAction;
use crate::kubernetes::repo::{DeploymentState, ShaMaybeBranch};
use crate::kubernetes::DeployConfig;
use crate::prelude::*;
use crate::web::{build_status, deploy_status, header, ResourceStatuses};
use kube::{Client, ResourceExt};
use maud::{html, Markup, Render};
use std::collections::HashMap;

struct PreviewArrow;

impl Render for PreviewArrow {
    fn render(&self) -> Markup {
        html!(span.preview-arrow { "⇨" })
    }
}

struct GitRef(String, String, String, bool);

impl Render for GitRef {
    fn render(&self) -> Markup {
        let owner = self.1.clone();
        let repo = self.2.clone();
        let sha = self.0.clone();
        let disable_prefixing = self.3;

        let sha_prefix = if !disable_prefixing && sha.len() >= 7 {
            &sha[..7]
        } else {
            &sha
        };

        html!(
            span {
                a.git-ref href=(format!("https://github.com/{}/{}/tree/{}", owner, repo, sha)) {
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
#[allow(dead_code)]
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
    fn get_build_time(&self) -> Option<u64> {
        match self {
            ResolvedVersion::BranchTracked { build_time, .. }
            | ResolvedVersion::TrackedSha { build_time, .. } => Some(*build_time),
            _ => None,
        }
    }

    fn from_config(
        config: &DeployConfig,
        conn: &PooledConnection<SqliteConnectionManager>,
    ) -> Self {
        match config.deployment_state() {
            DeploymentState::DeployedWithArtifact { artifact, .. } => {
                match artifact.branch {
                    Some(branch) => {
                        let build_time: Option<u64> = || -> Option<u64> {
                            let artifact_repository = config.artifact_repository()?;
                            let repo = GitRepo::get(artifact_repository, conn).ok().flatten()?;
                            let commit = GitCommit::get_by_sha(&artifact.sha, repo.id, conn)
                                .ok()
                                .flatten()?;

                            // FIXME: This isn't build time, it's commit time
                            Some(commit.timestamp as u64)

                            // let build = commit.get_build_status(conn).ok().flatten();
                            // Some(build.timestamp as u64)
                        }();

                        match build_time {
                            Some(build_time) => ResolvedVersion::BranchTracked {
                                sha: artifact.sha,
                                branch,
                                build_time,
                            },
                            None => ResolvedVersion::UnknownSha { sha: artifact.sha },
                        }
                    }
                    None => ResolvedVersion::UnknownSha { sha: artifact.sha },
                }
            }
            DeploymentState::DeployedOnlyConfig { .. } => todo!(),
            DeploymentState::Undeployed => ResolvedVersion::Undeployed,
        }
    }

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
            Action::DeployLatest => {
                let deployment_state = config.deployment_state();
                let branch_name = deployment_state.artifact_branch().unwrap_or("master");

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
                html!((GitRef(sha.clone(), owner.to_string(), repo.to_string(), false)))
            }
            ResolvedVersion::TrackedSha { sha, build_time: _ } => {
                html!((GitRef(sha.clone(), owner.to_string(), repo.to_string(), false)))
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
                        ))
                    )
                } else {
                    html!((GitRef(sha.clone(), owner.to_string(), repo.to_string(), false)))
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

    fn is_undeployed(&self) -> bool {
        matches!(self, ResolvedVersion::Undeployed)
    }
}

trait FormatStates {
    fn format_config(&self, owner: &str, repo: &str) -> Markup;
    fn format_artifact(&self, owner: &str, repo: &str) -> Markup;
}

impl FormatStates for DeploymentState {
    fn format_config(&self, owner: &str, repo: &str) -> Markup {
        match self {
            DeploymentState::DeployedWithArtifact { config, .. }
            | DeploymentState::DeployedOnlyConfig { config } => {
                html! {
                    (GitRef(config.sha.clone(), owner.to_string(), repo.to_string(), false))
                }
            }
            DeploymentState::Undeployed => html! { "Undeployed" },
        }
    }

    fn format_artifact(&self, owner: &str, repo: &str) -> Markup {
        match self {
            DeploymentState::DeployedWithArtifact { artifact, .. } => {
                html! {
                    (GitRef(artifact.sha.clone(), owner.to_string(), repo.to_string(), false))
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
    fn format_config(&self, owner: &str, repo: &str) -> Markup {
        match self {
            Ok(deployment_state) => deployment_state.format_config(owner, repo),
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
struct DeployTransition<'a> {
    from: DeploymentState,
    to: AppResult<DeploymentState>,
    current_config: DeployConfig,
    client: &'a Client,
}

impl<'a> DeployTransition<'a> {
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
        if self.to == Ok(self.from.clone()) {
            match self.from.clone() {
                DeploymentState::Undeployed => {
                    html! {
                        span { "Already undeployed"}
                        (self.current_config.format_resources(self.client).await)
                    }
                }
                DeploymentState::DeployedWithArtifact { artifact, config } => {
                    html! {
                        ul {
                            li {
                                "Artifact: "
                                (GitRef(artifact.sha.clone(), owner.to_string(), repo.to_string(), false))
                            }
                            li {
                                "Config: "
                                (GitRef(config.sha.clone(), owner.to_string(), repo.to_string(), false))
                            }
                        }
                        (self.current_config.format_resources(self.client).await)
                    }
                }
                DeploymentState::DeployedOnlyConfig { config } => {
                    html! {
                        ul {
                            li {
                                "Config: "
                                (GitRef(config.sha.clone(), owner.to_string(), repo.to_string(), false))
                            }
                        }
                        (self.current_config.format_resources(self.client).await)
                    }
                }
            }
        } else {
            html! {
                ul {
                    li {
                        "Config: "
                        // FIXME: No guarantee that config repo is the artifact repo (fix this!!!)
                        // Move repo info as well as build info into some sort of HydratedDeploymentState struct.
                        (self.from.format_config(owner, repo))
                        ( PreviewArrow {} )
                        (self.to.format_config(owner, repo))
                    }
                    li {
                        "Artifact: "
                        (self.from.format_artifact(owner, repo))
                        ( PreviewArrow {} )
                        (self.to.format_artifact(owner, repo))

                        @if let Some(compare_url) = self.compare_url(owner, repo) {
                            " "
                            a.git-ref href=(compare_url) {
                                "[compare]"
                            }
                        }
                    }
                }
                (self.current_config.format_resources(self.client).await)
            }
        }
    }
}

/// Generate the status header showing current branch and autodeploy status
fn generate_status_header(config: &DeployConfig, owner: &str, repo: &str) -> Markup {
    let default_branch = config
        .artifact_repository()
        .unwrap_or_else(|| config.config_repository().with_branch("master"))
        .branch;

    // FIXME: What about artifactless configs?
    let current_branch = config
        .status
        .clone()
        .and_then(|s| s.artifact.as_ref().and_then(|a| a.branch.clone()));

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
                            ))
                            @if branch != default_branch {
                                span class="warning-icon" title=(format!("Different from default branch ({})", default_branch)) {
                                    "⚠️"
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
            (Action::DeployLatest, Some(artifact_repository)) => {
                // In this case we are deploying the latest commit of the tracked branch and we know that the config has an artifact repository.
                // We don't know the tracked branch yet (it could be the default or a custom branch, so we need to use config.deployment_state() to see what it is.)
                // There also might not be a tracked branch if a specific commit is currently deployed, in which case we should deploy the latest commit of the default branch.
                let deployment_state = config.deployment_state();
                let branch_name = deployment_state
                    .artifact_branch()
                    .unwrap_or(&artifact_repository.branch);

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
            (Action::DeployLatest, None) => {
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
            (Action::ToggleAutodeploy, _) => Ok(config.deployment_state()),
            (Action::Undeploy, _) => Ok(DeploymentState::Undeployed),
        }
    }
}

pub async fn render_preview_content(
    selected_config: &DeployConfig,
    action: &Action,
    conn: &PooledConnection<SqliteConnectionManager>,
    client: &Client,
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

    let preview_content = match action {
        Action::DeployLatest
        | Action::DeployBranch { .. }
        | Action::DeployCommit { .. }
        | Action::Undeploy => {
            DeployTransition {
                from: selected_config.deployment_state(),
                to: DeploymentState::from_action(action, selected_config, conn),
                current_config: selected_config.clone(),
                client,
            }
            .format(&owner, &repo)
            .await
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
    // for alert in deploy_status(selected_config).await {
    //     alerts.push(alert);
    // }
    // for alert in build_status(action, selected_config, conn).await {
    //     alerts.push(alert);
    // }

    html! {
        @for alert in alerts {
            (alert)
        }
        div class="preview-transition" {
            div class="preview-transition-header" {
                (selected_config.name_any())
            }
            div class="preview-transition-content" {
                (preview_content)
            }
        }
    }
}

/// Generate the preview markup for a deploy config action
async fn generate_preview(
    selected_config: &DeployConfig,
    action: &Action,
    conn: &PooledConnection<SqliteConnectionManager>,
    client: &Client,
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

    // Wrap the preview content in the container markup
    html! {
        div class="preview-container" {
            div class="preview-content" {
                (generate_status_header(selected_config, &owner, &repo))

                div.preview-content-poll-wrapper hx-get=(format!("/fragments/deploy-preview/{}/{}?{}", selected_config.namespace().unwrap_or("default".to_string()), selected_config.name_any(), action.as_params())) hx-trigger="load, every 2s" hx-swap="morph:innerHTML" {
                    (render_preview_content(selected_config, action, conn, client).await)
                }
            }
        }
    }
}

pub enum Action {
    DeployLatest,
    DeployBranch { branch: String },
    DeployCommit { sha: String },
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
            "toggle-autodeploy" => Action::ToggleAutodeploy,
            "undeploy" => Action::Undeploy,
            _ => Action::DeployLatest,
        }
    }

    pub fn as_params(&self) -> String {
        match self {
            Action::DeployLatest => "action=deploy".to_string(),
            Action::DeployBranch { branch } => format!("action=deploy&branch={}", branch),
            Action::DeployCommit { sha } => format!("action=deploy&sha={}", sha),
            Action::ToggleAutodeploy => "action=toggle-autodeploy".to_string(),
            Action::Undeploy => "action=undeploy".to_string(),
        }
    }

    fn is_deploy(&self) -> bool {
        matches!(
            self,
            Action::DeployLatest | Action::DeployBranch { .. } | Action::DeployCommit { .. }
        )
    }

    fn is_toggle_autodeploy(&self) -> bool {
        matches!(self, Action::ToggleAutodeploy)
    }

    fn is_undeploy(&self) -> bool {
        matches!(self, Action::Undeploy)
    }
}

/// Handler for the deploy configs page
#[get("/deploy")]
pub async fn deploy_configs(
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

    let action = Action::from_query(&query);

    // Sort DeployConfigs by namespace and name for the dropdown
    let mut sorted_deploy_configs = deploy_configs.clone();
    sorted_deploy_configs.sort_by(|a, b| {
        let a_ns = a.namespace().unwrap_or_default();
        let b_ns = b.namespace().unwrap_or_default();
        let a_name = a.name_any();
        let b_name = b.name_any();

        (a_ns, a_name).cmp(&(b_ns, b_name))
    });

    // Check if we have a selected config from query parameter
    let selected_config_key = query.get("selected");

    // Find the selected deploy config or use the first one as default
    let selected_config = if let Some(selected_key) = selected_config_key {
        // Parse the selected_key which is in the format "namespace/name"
        let parts: Vec<&str> = selected_key.split('/').collect();
        if parts.len() == 2 {
            let namespace = parts[0];
            let name = parts[1];

            // Find the matching config
            sorted_deploy_configs.iter().find(|config| {
                config.namespace().unwrap_or_default() == namespace && config.name_any() == name
            })
        } else {
            sorted_deploy_configs.first()
        }
    } else {
        sorted_deploy_configs.first()
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
                                            @let namespace = config.namespace().unwrap_or_default();
                                            @let name = config.name_any();
                                            @let selected = if let Some(default) = selected_config {
                                                default.namespace().unwrap_or_default() == namespace && default.name_any() == name
                                            } else {
                                                false
                                            };

                                            option value=(format!("{}/{}", namespace, name)) selected[selected] {
                                                (format!("{}/{}", namespace, name))
                                            }
                                        }
                                    }
                                }

                                @if let Some(selected_config) = selected_config {
                                    @let deployment_state = selected_config.deployment_state();
                                    @let current_branch = deployment_state.artifact_branch();
                                    form action="/deploy" method="get" {
                                        input type="hidden" name="selected" value=(format!("{}/{}", selected_config.namespace().unwrap_or_default(), selected_config.name_any()));

                                        div class="action-radio-group" {
                                            h4 { "Action" }
                                            label class="action-radio" {
                                                input type="radio" name="action" value="deploy" checked[action.is_deploy()] onchange="this.form.submit()";
                                                "Deploy"
                                            }
                                            label class="action-radio" {
                                                input type="radio" name="action" value="toggle-autodeploy" checked[action.is_toggle_autodeploy()] onchange="this.form.submit()";
                                                @if selected_config.autodeploy() {
                                                    "Disable autodeploy"
                                                } @else {
                                                    "Enable autodeploy"
                                                }
                                            }
                                            label class="action-radio" {
                                                input type="radio" name="action" value="undeploy" checked[action.is_undeploy()] onchange="this.form.submit()";
                                                "Undeploy"
                                            }
                                        }

                                        @if action.is_deploy() {
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
                                        button.primary-action-button.danger-button[action.is_undeploy()] type="submit" {
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
                                                Action::Undeploy => {
                                                    "Undeploy"
                                                }
                                            }
                                        }
                                    }
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
                                            Action::ToggleAutodeploy => {
                                                "Option change for "
                                            }
                                            Action::Undeploy => {
                                                "Undeploy of "
                                            }
                                        }
                                        strong {
                                            (format!("{}/{}", selected_config.namespace().unwrap_or_default(), selected_config.name_any()))
                                        }
                                    }
                                    (generate_preview(selected_config, &action, &conn, &client).await)
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

    let return_url = format!(
        "/deploy?selected={}/{}&action={}&branch={}&sha={}",
        namespace,
        name,
        form.get("action").unwrap_or(&"".to_string()),
        form.get("branch").unwrap_or(&"".to_string()),
        form.get("sha").unwrap_or(&"".to_string())
    );

    let deployment_state = match DeploymentState::from_action(&action, &config, &conn) {
        Ok(deployment_state) => deployment_state,
        Err(e) => {
            log::error!("Failed to get deployment state: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to get deployment state");
        }
    };

    let deploy_action = match &action {
        Action::DeployLatest
        | Action::DeployBranch { .. }
        | Action::DeployCommit { .. }
        | Action::Undeploy => match deployment_state {
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

        Action::ToggleAutodeploy => DeployAction::ToggleAutodeploy {
            name: name.to_string(),
        },
    };

    match deploy_action
        .execute(&client, &octocrabs, config.config_repository())
        .await
    {
        Ok(()) => (),
        Err(e) => {
            log::error!("Failed to execute deploy action: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to execute deploy action");
        }
    }

    let Ok(maybe_deploy_event) = DeployEvent::from_user_deploy_action(&deploy_action) else {
        return HttpResponse::InternalServerError()
            .content_type("text/html; charset=utf-8")
            .body("Failed to create deploy event");
    };
    if let Some(deploy_event) = maybe_deploy_event {
        match deploy_event.insert(&conn) {
            Ok(_) => (),
            Err(e) => {
                log::error!("Failed to insert deploy event: {}", e);

                return HttpResponse::InternalServerError()
                    .content_type("text/html; charset=utf-8")
                    .body("Failed to insert deploy event");
            }
        }
    }

    // Redirect back to the DeployConfig page with the selected config
    HttpResponse::SeeOther()
        .append_header(("Location", return_url))
        .finish()
}
