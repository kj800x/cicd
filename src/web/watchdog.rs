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
    let base_url = format!("/deploy?selected={}", issue.config.qualified_name());

    let action_url = match &issue.issue_type {
        WatchdogIssueType::NonDefaultBranch => {
            // Link to deploy with default branch to fix the non-default branch issue
            format!(
                "{}&action=deploy&branch={}",
                base_url,
                issue.config.default_branch()
            )
        }
        WatchdogIssueType::AutodeployMismatch => {
            format!("{}&action=toggle-autodeploy", base_url)
        }
        WatchdogIssueType::UndeployedWithAutodeploy => {
            // Link to deploy with default branch since we want to deploy the standard version
            format!(
                "{}&action=deploy&branch={}",
                base_url,
                issue.config.default_branch()
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
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to get database connection: {}", e);
            return HttpResponse::InternalServerError().body("Failed to connect to database");
        }
    };

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
        if !config.is_tracking_default_branch() {
            issues.push(WatchdogIssue::new(
                WatchdogIssueType::NonDefaultBranch,
                config.clone(),
                format!(
                    "Tracking '{}' instead of default branch '{}'",
                    config.tracking_branch(),
                    config.default_branch()
                ),
            ));
        }

        // Check for autodeploy mismatch
        if !config.autodeploy_matches_spec() {
            issues.push(WatchdogIssue::new(
                WatchdogIssueType::AutodeployMismatch,
                config.clone(),
                format!(
                    "State: {}, Spec: {}",
                    config.current_autodeploy(),
                    config.spec_autodeploy()
                ),
            ));
        }

        // Check for failing builds
        let tracking_branch = config.tracking_branch();

        let commit = get_latest_completed_build(
            config.artifact_owner(),
            config.artifact_repo(),
            tracking_branch,
            &conn,
        )
        .ok()
        .flatten();

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
        if config.current_autodeploy() && config.is_undeployed() {
            issues.push(WatchdogIssue::new(
                WatchdogIssueType::UndeployedWithAutodeploy,
                config.clone(),
                "Config is undeployed but has autodeploy enabled".to_string(),
            ));
        }

        // Check for out of sync
        if !config.is_in_sync() {
            if let (Some(latest), Some(wanted)) = (config.latest_sha(), config.wanted_sha()) {
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
                (header::stylesheet_link())
                (header::scripts())
            }
            body.watchdog-page {
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
