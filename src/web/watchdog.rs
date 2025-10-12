use crate::db::{get_latest_completed_build, BuildStatus};
use crate::prelude::*;
use crate::web::header;
use kube::{api::Api, client::Client, ResourceExt};
use maud::{html, Markup};
use std::cmp::Ordering;

#[derive(Debug, Clone)]
enum WatchdogIssueType {
    NonDefaultBranch,
    AutodeployMismatch,
    FailingBuild,
    UndeployedWithAutodeploy,
    OutOfSync,
}

impl WatchdogIssueType {
    fn severity(&self) -> u8 {
        match self {
            WatchdogIssueType::FailingBuild => 3,
            WatchdogIssueType::UndeployedWithAutodeploy => 2,
            WatchdogIssueType::OutOfSync => 2,
            WatchdogIssueType::NonDefaultBranch => 1,
            WatchdogIssueType::AutodeployMismatch => 1,
        }
    }

    fn title(&self) -> &'static str {
        match self {
            WatchdogIssueType::NonDefaultBranch => "Non-Default Branch",
            WatchdogIssueType::AutodeployMismatch => "Autodeploy Mismatch",
            WatchdogIssueType::FailingBuild => "Failing Build",
            WatchdogIssueType::UndeployedWithAutodeploy => "Undeployed with Autodeploy",
            WatchdogIssueType::OutOfSync => "Out of Sync",
        }
    }

    fn description(&self) -> &'static str {
        match self {
            WatchdogIssueType::NonDefaultBranch => {
                "Config is tracking a branch other than its default branch"
            }
            WatchdogIssueType::AutodeployMismatch => "Autodeploy state does not match spec",
            WatchdogIssueType::FailingBuild => "Current or default branch HEAD has a failing build",
            WatchdogIssueType::UndeployedWithAutodeploy => {
                "Config is undeployed but has autodeploy enabled"
            }
            WatchdogIssueType::OutOfSync => "Latest SHA differs from wanted SHA",
        }
    }

    fn css_class(&self) -> &'static str {
        match self.severity() {
            3 => "severe",
            2 => "warning",
            _ => "info",
        }
    }
}

struct WatchdogIssue {
    issue_type: WatchdogIssueType,
    config: DeployConfig,
    details: String,
}

impl WatchdogIssue {
    fn new(issue_type: WatchdogIssueType, config: DeployConfig, details: String) -> Self {
        Self {
            issue_type,
            config,
            details,
        }
    }
}

impl Ord for WatchdogIssue {
    fn cmp(&self, other: &Self) -> Ordering {
        // Sort by severity (high to low), then by issue type, then by namespace/name
        other
            .issue_type
            .severity()
            .cmp(&self.issue_type.severity())
            .then_with(|| self.issue_type.title().cmp(other.issue_type.title()))
            .then_with(|| {
                let self_key = format!(
                    "{}/{}",
                    self.config.namespace().unwrap_or_default(),
                    self.config.name_any()
                );
                let other_key = format!(
                    "{}/{}",
                    other.config.namespace().unwrap_or_default(),
                    other.config.name_any()
                );
                self_key.cmp(&other_key)
            })
    }
}

impl PartialOrd for WatchdogIssue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for WatchdogIssue {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for WatchdogIssue {}

fn render_issue(issue: &WatchdogIssue) -> Markup {
    let base_url = format!(
        "/deploy?selected={}/{}",
        issue.config.namespace().unwrap_or_default(),
        issue.config.name_any()
    );

    let action_url = match &issue.issue_type {
        WatchdogIssueType::NonDefaultBranch => {
            // Link to deploy with default branch to fix the non-default branch issue
            format!(
                "{}&action=deploy&branch={}",
                base_url, issue.config.spec.spec.artifact.branch
            )
        }
        WatchdogIssueType::AutodeployMismatch => {
            format!("{}&action=toggle-autodeploy", base_url)
        }
        WatchdogIssueType::UndeployedWithAutodeploy => {
            // Link to deploy with default branch since we want to deploy the standard version
            format!(
                "{}&action=deploy&branch={}",
                base_url, issue.config.spec.spec.artifact.branch
            )
        }
        // For out of sync and failing builds, just link to the deploy page
        WatchdogIssueType::OutOfSync | WatchdogIssueType::FailingBuild => base_url,
    };

    html! {
        div class=(format!("watchdog-card {}", issue.issue_type.css_class())) {
            div class="watchdog-card-header" {
                h3 class="watchdog-card-title" { (issue.issue_type.title()) }
                span class="watchdog-card-config" {
                    a href=(action_url) {
                        (format!("{}/{}",
                            issue.config.namespace().unwrap_or_default(),
                            issue.config.name_any()))
                    }
                }
            }
            div class="watchdog-card-description" {
                p { (issue.issue_type.description()) }
            }
            div class="watchdog-card-details" {
                p { (issue.details) }
            }
        }
    }
}

#[get("/watchdog")]
pub async fn watchdog(
    pool: web::Data<Pool<SqliteConnectionManager>>,
    client: Option<web::Data<Client>>,
) -> impl Responder {
    let conn = pool.get().unwrap();

    // Get all DeployConfigs
    let client = match client {
        Some(client) => client,
        None => {
            return HttpResponse::ServiceUnavailable()
                .content_type("text/html; charset=utf-8")
                .body("Kubernetes client is not available.");
        }
    };

    let deploy_configs_api: Api<DeployConfig> = Api::all(client.get_ref().clone());
    let deploy_configs = match deploy_configs_api.list(&Default::default()).await {
        Ok(list) => list.items,
        Err(e) => {
            log::error!("Failed to list DeployConfigs: {}", e);
            return HttpResponse::InternalServerError()
                .content_type("text/html; charset=utf-8")
                .body("Failed to list DeployConfigs");
        }
    };

    let mut issues = Vec::new();

    // Check each deploy config for issues
    for config in deploy_configs {
        // Check for non-default branch
        let tracking_branch = &config.tracking_branch();
        if tracking_branch != &config.spec.spec.artifact.branch {
            issues.push(WatchdogIssue::new(
                WatchdogIssueType::NonDefaultBranch,
                config.clone(),
                format!(
                    "Tracking '{}' instead of default branch '{}'",
                    tracking_branch, config.spec.spec.artifact.branch
                ),
            ));
        }

        // Check for autodeploy mismatch
        let current_autodeploy = config.current_autodeploy();
        if current_autodeploy != config.spec.spec.autodeploy {
            issues.push(WatchdogIssue::new(
                WatchdogIssueType::AutodeployMismatch,
                config.clone(),
                format!(
                    "State: {}, Spec: {}",
                    current_autodeploy, config.spec.spec.autodeploy
                ),
            ));
        }

        // Check for failing builds
        let tracking_branch = config.tracking_branch();

        let commit = get_latest_completed_build(
            &config.spec.spec.artifact.owner,
            &config.spec.spec.artifact.repo,
            tracking_branch,
            &conn,
        );

        if let Some(commit) = commit {
            if commit.build_status == BuildStatus::Failure {
                issues.push(WatchdogIssue::new(
                    WatchdogIssueType::FailingBuild,
                    config.clone(),
                    format!(
                        "Branch '{}' HEAD ({}) has a failing build",
                        tracking_branch,
                        if commit.sha.len() >= 7 {
                            &commit.sha[..7]
                        } else {
                            &commit.sha
                        }
                    ),
                ));
            }
        }

        // Check for undeployed with autodeploy
        if config.current_autodeploy() == true && config.wanted_sha().is_none() {
            issues.push(WatchdogIssue::new(
                WatchdogIssueType::UndeployedWithAutodeploy,
                config.clone(),
                "Config is undeployed but has autodeploy enabled".to_string(),
            ));
        }

        // Check for out of sync
        if let (Some(latest), Some(wanted)) = (&config.latest_sha(), &config.wanted_sha()) {
            if latest != wanted {
                issues.push(WatchdogIssue::new(
                    WatchdogIssueType::OutOfSync,
                    config.clone(),
                    format!(
                        "Latest: {}, Wanted: {}",
                        if latest.len() >= 7 {
                            &latest[..7]
                        } else {
                            latest
                        },
                        if wanted.len() >= 7 {
                            &wanted[..7]
                        } else {
                            wanted
                        }
                    ),
                ));
            }
        }
    }

    // Sort issues by severity and type
    issues.sort();

    let markup = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width, initial-scale=1.0";
                title { "Watchdog Dashboard" }
                style {
                    r#"
                    :root {
                        --primary-blue: #0969da;
                        --text-color: #3a485a;
                        --secondary-text: #57606a;
                        --border-color: #d0d7de;
                        --bg-light: #f6f8fa;
                        --green: #2da44e;
                        --header-bg: #24292e;
                        --danger-color: #cf222e;
                        --warning-color: #9a6700;
                        --info-color: #0969da;
                    }
                    body {
                        font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
                        background-color: white;
                        color: var(--text-color);
                        margin: 0;
                        padding: 0;
                        line-height: 1.5;
                    }
                    .content {
                        max-width: 1200px;
                        margin: 0 auto;
                        padding: 24px;
                    }
                    .watchdog-header {
                        margin-bottom: 24px;
                        padding-bottom: 16px;
                        border-bottom: 1px solid var(--border-color);
                    }
                    .watchdog-header h1 {
                        font-size: 24px;
                        font-weight: 600;
                        margin: 0;
                        color: var(--text-color);
                    }
                    .watchdog-header p {
                        margin: 8px 0 0;
                        color: var(--secondary-text);
                        font-size: 14px;
                    }
                    .watchdog-card {
                        border: 1px solid var(--border-color);
                        border-radius: 6px;
                        margin-bottom: 16px;
                        background-color: white;
                        box-shadow: 0 1px 3px rgba(0, 0, 0, 0.04);
                    }
                    .watchdog-card.severe {
                        border-left: 4px solid var(--danger-color);
                    }
                    .watchdog-card.severe .watchdog-card-header {
                        background-color: #ffebe9;
                    }
                    .watchdog-card.warning {
                        border-left: 4px solid var(--warning-color);
                    }
                    .watchdog-card.warning .watchdog-card-header {
                        background-color: #fff8c5;
                    }
                    .watchdog-card.info {
                        border-left: 4px solid var(--info-color);
                    }
                    .watchdog-card.info .watchdog-card-header {
                        background-color: var(--bg-light);
                    }
                    .watchdog-card-header {
                        display: flex;
                        justify-content: space-between;
                        align-items: center;
                        padding: 12px 16px;
                        border-bottom: 1px solid var(--border-color);
                        border-top-left-radius: 6px;
                        border-top-right-radius: 6px;
                    }
                    .watchdog-card-title {
                        margin: 0;
                        font-size: 14px;
                        font-weight: 600;
                        color: var(--text-color);
                    }
                    .watchdog-card-config {
                        font-size: 12px;
                    }
                    .watchdog-card-config a {
                        color: var(--primary-blue);
                        text-decoration: none;
                        font-weight: 500;
                    }
                    .watchdog-card-config a:hover {
                        text-decoration: underline;
                    }
                    .watchdog-card-description {
                        padding: 12px 16px;
                        border-bottom: 1px solid var(--border-color);
                        background-color: white;
                    }
                    .watchdog-card-description p {
                        margin: 0;
                        color: var(--secondary-text);
                        font-size: 13px;
                    }
                    .watchdog-card-details {
                        padding: 12px 16px;
                        background-color: var(--bg-light);
                        border-bottom-left-radius: 6px;
                        border-bottom-right-radius: 6px;
                    }
                    .watchdog-card-details p {
                        margin: 0;
                        font-family: ui-monospace, SFMono-Regular, SF Mono, Menlo, Consolas, Liberation Mono, monospace;
                        font-size: 12px;
                        color: var(--text-color);
                    }
                    .no-issues {
                        text-align: center;
                        padding: 48px 0;
                        background-color: var(--bg-light);
                        border-radius: 6px;
                        border: 1px solid var(--border-color);
                    }
                    .no-issues h2 {
                        margin: 0 0 8px;
                        font-size: 20px;
                        font-weight: 600;
                        color: var(--text-color);
                    }
                    .no-issues p {
                        margin: 0;
                        font-size: 14px;
                        color: var(--secondary-text);
                    }
                    "#
                    (header::styles())
                }
                (header::scripts())
            }
            body {
                (header::render("watchdog"))
                div class="content" {
                    div class="watchdog-header" {
                        h1 { "Watchdog" }
                        p { "Monitoring deploy configs for potential issues" }
                    }
                    @if issues.is_empty() {
                        div class="no-issues" {
                            h2 { "All Clear" }
                            p { "No issues detected in any deploy configs" }
                        }
                    } @else {
                        @for issue in &issues {
                            (render_issue(issue))
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
