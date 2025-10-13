use crate::db::{get_latest_build, get_latest_completed_build, insert_deploy_event, DeployEvent};
use crate::prelude::*;
use crate::web::{build_status, deploy_status, header};
use kube::{
    api::{Api, Patch, PatchParams},
    client::Client,
    ResourceExt,
};
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
        match &config.status {
            None => ResolvedVersion::Undeployed,
            Some(status) => match &status.artifact.as_ref().and_then(|a| a.wanted_sha.as_ref()) {
                Some(sha) => {
                    let commit = get_commit_by_sha(sha, conn).ok().flatten();

                    // FIXME: Technically we don't know if the commit was selected as part of a tracking branch or not
                    match commit {
                        Some(commit) => ResolvedVersion::BranchTracked {
                            sha: sha.to_string(),
                            branch: config.tracking_branch().to_string(),
                            build_time: commit.timestamp as u64,
                        },
                        None => ResolvedVersion::UnknownSha {
                            sha: sha.to_string(),
                        },
                    }
                }
                None => ResolvedVersion::Undeployed,
            },
        }
    }

    pub fn from_action(
        action: &Action,
        config: &DeployConfig,
        conn: &PooledConnection<SqliteConnectionManager>,
        build_filter: BuildFilter,
    ) -> Self {
        match action {
            Action::DeployLatest => {
                let branch = config.tracking_branch();
                let commit = match build_filter {
                    BuildFilter::Any => get_latest_build(
                        config.artifact_owner(),
                        config.artifact_repo(),
                        branch,
                        conn,
                    )
                    .ok()
                    .flatten(),
                    BuildFilter::Completed => get_latest_completed_build(
                        config.artifact_owner(),
                        config.artifact_repo(),
                        branch,
                        conn,
                    )
                    .ok()
                    .flatten(),
                    BuildFilter::Successful => get_latest_successful_build(
                        config.artifact_owner(),
                        config.artifact_repo(),
                        branch,
                        conn,
                    )
                    .ok()
                    .flatten(),
                };

                match commit {
                    Some(commit) => ResolvedVersion::BranchTracked {
                        sha: commit.sha,
                        branch: branch.to_string(),
                        build_time: commit.timestamp as u64,
                    },
                    None => ResolvedVersion::ResolutionFailed,
                }
            }
            Action::DeployBranch { branch } => {
                let commit = match build_filter {
                    BuildFilter::Any => get_latest_build(
                        config.artifact_owner(),
                        config.artifact_repo(),
                        branch,
                        conn,
                    )
                    .ok()
                    .flatten(),
                    BuildFilter::Completed => get_latest_completed_build(
                        config.artifact_owner(),
                        config.artifact_repo(),
                        branch,
                        conn,
                    )
                    .ok()
                    .flatten(),
                    BuildFilter::Successful => get_latest_successful_build(
                        config.artifact_owner(),
                        config.artifact_repo(),
                        branch,
                        conn,
                    )
                    .ok()
                    .flatten(),
                };

                match commit {
                    Some(commit) => ResolvedVersion::BranchTracked {
                        sha: commit.sha,
                        branch: branch.clone(),
                        build_time: commit.timestamp as u64,
                    },
                    None => ResolvedVersion::ResolutionFailed,
                }
            }
            Action::DeployCommit { sha } => {
                let commit = get_commit_by_sha(sha, conn).ok().flatten();

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

/// Represents a transition between two resolved versions
struct DeployTransition {
    from: ResolvedVersion,
    to: ResolvedVersion,
}

impl DeployTransition {
    fn compare_url(&self, owner: &str, repo: &str) -> Option<String> {
        match (&self.from, &self.to) {
            (
                ResolvedVersion::BranchTracked {
                    branch: _,
                    sha: from_sha,
                    build_time: _,
                },
                ResolvedVersion::BranchTracked {
                    branch: _,
                    sha: to_sha,
                    build_time: _,
                },
            ) => Some(format!(
                "https://github.com/{}/{}/compare/{}...{}",
                owner, repo, from_sha, to_sha
            )),
            _ => None,
        }
    }

    /// Formats the transition for display
    fn format(&self, owner: &str, repo: &str) -> Markup {
        if self.from == self.to {
            if self.from.is_undeployed() {
                html! {
                    "Already undeployed"
                }
            } else {
                html! {
                    (self.from.format(Some(&self.to), owner, repo))
                    span {
                        " (up to date"
                        @if let Some(build_time) = self.from.get_build_time() {
                            ", built "
                            (HumanTime(build_time))
                        }
                        ")"
                    }
                }
            }
        } else {
            html! {
                (self.from.format(Some(&self.to), owner, repo))
                @if !self.from.is_undeployed() {
                    span {
                        " (last deployed"
                        @if let Some(build_time) = self.from.get_build_time() {
                            ", built "
                            (HumanTime(build_time))
                        }
                        ")"
                    }
                }
                ( PreviewArrow {} )
                (self.to.format(Some(&self.from), owner, repo))
                @if !self.to.is_undeployed() {
                    span {
                        " (latest built"
                        @if let Some(build_time) = self.to.get_build_time() {
                            ", "
                            (HumanTime(build_time))
                        }
                        ")"
                    }
                }
                @if let Some(compare_url) = self.compare_url(owner, repo) {
                    " "
                    a.git-ref href=(compare_url) {
                        "[compare]"
                    }
                }
            }
        }
    }
}

/// Generate the status header showing current branch and autodeploy status
fn generate_status_header(config: &DeployConfig, owner: &str, repo: &str) -> Markup {
    let default_branch = config.default_branch();
    let default_autodeploy = config.spec_autodeploy();
    let current_autodeploy = config.current_autodeploy();
    let current_branch = config.tracking_branch();

    html! {
        div class="status-header" {
            div class="status-item" {
                "Tracking branch: "
                strong {
                    (GitRef(
                        current_branch.to_string(),
                        owner.to_string(),
                        repo.to_string(),
                        true,
                    ))
                    @if current_branch != default_branch {
                        span class="warning-icon" title=(format!("Different from default branch ({})", default_branch)) {
                            "⚠️"
                        }
                    }
                }
            }
            div class="status-item" {
                "Autodeploy: "
                strong {
                    @if current_autodeploy {
                        (AutodeployStatus(true))
                    } @else {
                        (AutodeployStatus(false))
                    }
                    @if default_autodeploy != current_autodeploy {
                        span class="warning-icon" title="Different from default" {
                            "⚠️"
                        }
                    }
                }
            }
        }
    }
}

pub async fn render_preview_content(
    selected_config: &DeployConfig,
    action: &Action,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Markup {
    let owner = selected_config.artifact_owner().to_string();
    let repo = selected_config.artifact_repo().to_string();

    let preview_content = match action {
        Action::DeployLatest
        | Action::DeployBranch { .. }
        | Action::DeployCommit { .. }
        | Action::Undeploy => DeployTransition {
            from: ResolvedVersion::from_config(selected_config, conn),
            to: ResolvedVersion::from_action(
                action,
                selected_config,
                conn,
                BuildFilter::Successful,
            ),
        }
        .format(&owner, &repo),
        Action::ToggleAutodeploy => {
            html! {
                "Autodeploy "
                @if selected_config.current_autodeploy() {
                    (AutodeployStatus(true))
                } @else {
                    (AutodeployStatus(false))
                }
                ( PreviewArrow {} )
                @if selected_config.current_autodeploy() {
                    (AutodeployStatus(false))
                } @else {
                    (AutodeployStatus(true))
                }
            }
        }
    };

    let mut alerts = vec![];
    for alert in deploy_status(selected_config).await {
        alerts.push(alert);
    }
    for alert in build_status(action, selected_config, conn).await {
        alerts.push(alert);
    }

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
) -> Markup {
    let owner = selected_config.artifact_owner().to_string();
    let repo = selected_config.artifact_repo().to_string();

    // Wrap the preview content in the container markup
    html! {
        div class="preview-container" {
            div class="preview-content" {
                (generate_status_header(selected_config, &owner, &repo))

                div.preview-content-poll-wrapper hx-get=(format!("/fragments/deploy-preview/{}/{}?{}", selected_config.namespace().unwrap_or("default".to_string()), selected_config.name_any(), action.as_params())) hx-trigger="load, every 2s" hx-swap="morph:innerHTML" {
                    (render_preview_content(selected_config, action, conn).await)
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
    let client = match Client::try_default().await {
        Ok(client) => client,
        Err(e) => {
            log::error!("Failed to initialize Kubernetes client: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to connect to Kubernetes".to_string());
        }
    };

    // Get all DeployConfigs across all namespaces
    let deploy_configs_api: Api<DeployConfig> = Api::all(client.clone());
    let deploy_configs = match deploy_configs_api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            log::error!("Failed to list DeployConfigs: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to list DeployConfigs".to_string());
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
                                    @let current_branch = selected_config.tracking_branch();
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
                                                @if selected_config.current_autodeploy() {
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
                                                input id="branch" type="text" name="branch" placeholder="Enter branch name" value=(query.get("branch").unwrap_or(&current_branch.to_string())) onblur="this.form.submit()";
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
                                                    @if selected_config.current_autodeploy() {
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
                                    (generate_preview(selected_config, &action, &conn).await)
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

/// Handler for updating the wanted SHA of a DeployConfig
#[post("/api/deploy/{namespace}/{name}")]
pub async fn deploy_config(
    path: web::Path<(String, String)>,
    client: Option<web::Data<Client>>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    form: web::Form<HashMap<String, String>>,
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
    let deploy_configs_api: Api<DeployConfig> =
        Api::namespaced(client.get_ref().clone(), &namespace);

    let config = match deploy_configs_api.get(&name).await {
        Ok(config) => config,
        Err(e) => {
            log::error!("Failed to get DeployConfig {}/{}: {}", namespace, name, e);
            return HttpResponse::NotFound()
                .content_type("text/html; charset=utf-8")
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

    let resolved_version =
        ResolvedVersion::from_action(&action, &config, &conn, BuildFilter::Successful);

    let status = match &action {
        Action::DeployLatest | Action::DeployBranch { .. } | Action::DeployCommit { .. } => {
            match resolved_version {
                ResolvedVersion::UnknownSha { sha }
                | ResolvedVersion::TrackedSha { sha, build_time: _ } => {
                    serde_json::json!({
                        "status": {
                            "currentBranch": "",
                            "latestSha": "",
                            "wantedSha": sha
                        }
                    })
                }
                ResolvedVersion::BranchTracked {
                    sha,
                    branch,
                    build_time: _,
                } => serde_json::json!({
                    "status": {
                        "currentBranch": branch,
                        "latestSha": sha,
                        "wantedSha": sha
                    }
                }),
                ResolvedVersion::Undeployed => {
                    serde_json::json!({
                        "status": {
                            "wantedSha": null
                        }
                    })
                }
                ResolvedVersion::ResolutionFailed => {
                    return HttpResponse::BadRequest()
                        .content_type("text/html; charset=utf-8")
                        .body("No latest SHA available for deployment.");
                }
            }
        }
        Action::ToggleAutodeploy => {
            serde_json::json!({
                "status": {
                    "autodeploy": !config.current_autodeploy()
                }
            })
        }
        Action::Undeploy => {
            serde_json::json!({
                "status": {
                    "wantedSha": null
                }
            })
        }
    };

    match action {
        Action::DeployLatest | Action::DeployBranch { .. } | Action::DeployCommit { .. } => {
            let branch: Option<String> = match action {
                Action::DeployLatest => Some(config.tracking_branch().to_string()),
                Action::DeployBranch { branch } => Some(branch.clone()),
                Action::DeployCommit { .. } => None,
                Action::ToggleAutodeploy | Action::Undeploy => {
                    panic!("unreachable")
                }
            };

            let sha = status
                .get("status")
                .and_then(|s| s.get("wantedSha"))
                .map(|s| s.to_string());

            match insert_deploy_event(
                &DeployEvent {
                    deploy_config: name.to_string(),
                    team: config.team().to_string(),
                    timestamp: Utc::now().timestamp(),
                    initiator: "USER".to_string(),
                    status: "SUCCESS".to_string(),
                    branch,
                    sha,
                },
                &conn,
            ) {
                Ok(_) => {}
                Err(e) => {
                    log::error!("Failed to insert deploy event: {}", e);
                }
            }
        }
        Action::Undeploy => {
            if let Err(e) = insert_deploy_event(
                &DeployEvent {
                    deploy_config: name.to_string(),
                    team: config.team().to_string(),
                    timestamp: Utc::now().timestamp(),
                    initiator: "USER".to_string(),
                    status: "SUCCESS".to_string(),
                    branch: None,
                    sha: None,
                },
                &conn,
            ) {
                log::error!("Failed to insert deploy event: {}", e);
            }
        }
        Action::ToggleAutodeploy => {
            // No event
        }
    }

    // Apply the status update
    let patch = Patch::Merge(&status);
    let params = PatchParams::default();
    match deploy_configs_api
        .patch_status(&name, &params, &patch)
        .await
    {
        Ok(_) => (),
        Err(e) => {
            log::error!("Failed to update DeployConfig status: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body(format!("Failed to update DeployConfig status: {}", e));
        }
    };

    // Redirect back to the DeployConfig page with the selected config
    HttpResponse::SeeOther()
        .append_header(("Location", return_url))
        .finish()
}
