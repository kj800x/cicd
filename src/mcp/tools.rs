use kube::{Client, ResourceExt};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde_json::{json, Value};

use crate::build_status::BuildStatus;
use crate::crab_ext::Octocrabs;
use crate::db::deploy_event::DeployEvent;
use crate::db::git_branch::GitBranch;
use crate::db::git_repo::GitRepo;
use crate::kubernetes::api::{get_all_deploy_configs, get_deploy_config};
use crate::kubernetes::deploy_handlers::DeployAction;
use crate::kubernetes::repo::DeploymentState;
use crate::web::Action;

use super::protocol::{Tool, ToolCallResult};

pub fn tool_definitions() -> Vec<Tool> {
    vec![
        Tool {
            name: "list_deploy_configs".to_string(),
            description: "List all deploy configs with their current state".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        Tool {
            name: "get_deploy_config".to_string(),
            description: "Get details of a single deploy config by name".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the deploy config" }
                },
                "required": ["name"]
            }),
        },
        Tool {
            name: "get_build_status".to_string(),
            description: "Get the build status for a repository's branch head commit".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Repository in owner/name format" },
                    "branch": { "type": "string", "description": "Branch name (defaults to repo's default branch)" }
                },
                "required": ["repo"]
            }),
        },
        Tool {
            name: "deploy".to_string(),
            description: "Deploy a config, optionally targeting a specific branch or SHA"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the deploy config" },
                    "branch": { "type": "string", "description": "Branch to deploy from" },
                    "sha": { "type": "string", "description": "Specific commit SHA to deploy" }
                },
                "required": ["name"]
            }),
        },
        Tool {
            name: "undeploy".to_string(),
            description: "Undeploy a deploy config, removing its resources".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the deploy config" }
                },
                "required": ["name"]
            }),
        },
        Tool {
            name: "bounce".to_string(),
            description: "Restart all deployments owned by a deploy config".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the deploy config" }
                },
                "required": ["name"]
            }),
        },
        Tool {
            name: "execute_job".to_string(),
            description: "Manually trigger CronJobs owned by a deploy config".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the deploy config" }
                },
                "required": ["name"]
            }),
        },
        Tool {
            name: "toggle_autodeploy".to_string(),
            description: "Toggle the autodeploy setting on a deploy config".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the deploy config" }
                },
                "required": ["name"]
            }),
        },
    ]
}

pub async fn dispatch(
    tool_name: &str,
    arguments: Value,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    octocrabs: &Octocrabs,
) -> ToolCallResult {
    match tool_name {
        "list_deploy_configs" => handle_list_deploy_configs(client, pool).await,
        "get_deploy_config" => handle_get_deploy_config(arguments, client).await,
        "get_build_status" => handle_get_build_status(arguments, pool).await,
        "deploy" => handle_deploy(arguments, client, pool, octocrabs).await,
        "undeploy" => handle_action("undeploy", arguments, client, pool, octocrabs).await,
        "bounce" => handle_action("bounce", arguments, client, pool, octocrabs).await,
        "execute_job" => handle_action("execute_job", arguments, client, pool, octocrabs).await,
        "toggle_autodeploy" => {
            handle_action("toggle_autodeploy", arguments, client, pool, octocrabs).await
        }
        _ => ToolCallResult::error(format!("Unknown tool: {}", tool_name)),
    }
}

async fn handle_list_deploy_configs(
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
) -> ToolCallResult {
    let configs = match get_all_deploy_configs(client).await {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("Failed to list deploy configs: {}", e)),
    };

    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("Database error: {}", e)),
    };

    let results: Vec<Value> = configs
        .iter()
        .map(|config| {
            let deployment_state = config.deployment_state();
            let (artifact_sha, artifact_branch, config_sha, config_branch, state) =
                match &deployment_state {
                    DeploymentState::DeployedWithArtifact { artifact, config } => (
                        Some(artifact.sha.as_str()),
                        artifact.branch.as_deref(),
                        Some(config.sha.as_str()),
                        config.branch.as_deref(),
                        "deployed",
                    ),
                    DeploymentState::DeployedOnlyConfig { config } => (
                        None,
                        None,
                        Some(config.sha.as_str()),
                        config.branch.as_deref(),
                        "deployed_config_only",
                    ),
                    DeploymentState::Undeployed => (None, None, None, None, "undeployed"),
                };

            let artifact_repo_name = config
                .artifact_repository()
                .map(|r| format!("{}/{}", r.owner, r.repo));
            let config_repo = config.config_repository();
            let config_repo_name = format!("{}/{}", config_repo.owner, config_repo.repo);

            json!({
                "name": config.name_any(),
                "namespace": config.namespace().unwrap_or_else(|| "default".to_string()),
                "team": config.team(),
                "kind": config.kind(),
                "state": state,
                "autodeploy": config.autodeploy(),
                "orphaned": config.is_orphaned(),
                "artifact_repo": artifact_repo_name,
                "config_repo": config_repo_name,
                "artifact_sha": artifact_sha,
                "artifact_branch": artifact_branch,
                "config_sha": config_sha,
                "config_branch": config_branch,
            })
        })
        .collect();

    // Drop conn before await points
    drop(conn);

    ToolCallResult::text(serde_json::to_string_pretty(&results).unwrap_or_default())
}

async fn handle_get_deploy_config(
    arguments: Value,
    client: &Client,
) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: name".to_string()),
    };

    let config = match get_deploy_config(client, name).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return ToolCallResult::error(format!("Deploy config '{}' not found", name));
        }
        Err(e) => return ToolCallResult::error(format!("Failed to get deploy config: {}", e)),
    };

    let deployment_state = config.deployment_state();
    let (artifact_sha, artifact_branch, config_sha, config_branch, state) =
        match &deployment_state {
            DeploymentState::DeployedWithArtifact { artifact, config } => (
                Some(artifact.sha.as_str()),
                artifact.branch.as_deref(),
                Some(config.sha.as_str()),
                config.branch.as_deref(),
                "deployed",
            ),
            DeploymentState::DeployedOnlyConfig { config } => (
                None,
                None,
                Some(config.sha.as_str()),
                config.branch.as_deref(),
                "deployed_config_only",
            ),
            DeploymentState::Undeployed => (None, None, None, None, "undeployed"),
        };

    let artifact_repo = config.artifact_repository();
    let config_repo = config.config_repository();

    let result = json!({
        "name": config.name_any(),
        "namespace": config.namespace().unwrap_or_else(|| "default".to_string()),
        "team": config.team(),
        "kind": config.kind(),
        "state": state,
        "autodeploy": config.autodeploy(),
        "orphaned": config.is_orphaned(),
        "supports_bounce": config.supports_bounce(),
        "supports_execute_job": config.supports_execute_job(),
        "artifact_repo": artifact_repo.as_ref().map(|r| format!("{}/{}", r.owner, r.repo)),
        "artifact_default_branch": artifact_repo.as_ref().map(|r| &r.branch),
        "config_repo": format!("{}/{}", config_repo.owner, config_repo.repo),
        "artifact_sha": artifact_sha,
        "artifact_branch": artifact_branch,
        "config_sha": config_sha,
        "config_branch": config_branch,
    });

    ToolCallResult::text(serde_json::to_string_pretty(&result).unwrap_or_default())
}

async fn handle_get_build_status(
    arguments: Value,
    pool: &Pool<SqliteConnectionManager>,
) -> ToolCallResult {
    let repo_str = match arguments.get("repo").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => return ToolCallResult::error("Missing required parameter: repo".to_string()),
    };

    let parts: Vec<&str> = repo_str.splitn(2, '/').collect();
    if parts.len() != 2 {
        return ToolCallResult::error("repo must be in owner/name format".to_string());
    }
    let (owner, name) = (parts[0], parts[1]);

    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("Database error: {}", e)),
    };

    let repo = match GitRepo::get_by_name(owner, name, &conn) {
        Ok(Some(r)) => r,
        Ok(None) => return ToolCallResult::error(format!("Repository '{}' not found", repo_str)),
        Err(e) => return ToolCallResult::error(format!("Failed to look up repo: {}", e)),
    };

    let branch_name = arguments
        .get("branch")
        .and_then(|v| v.as_str())
        .unwrap_or(&repo.default_branch);

    let branch = match GitBranch::get_by_name(branch_name, repo.id, &conn) {
        Ok(Some(b)) => b,
        Ok(None) => {
            return ToolCallResult::error(format!("Branch '{}' not found", branch_name));
        }
        Err(e) => return ToolCallResult::error(format!("Failed to look up branch: {}", e)),
    };

    let head_commit =
        match crate::db::git_commit::GitCommit::get_by_sha(&branch.head_commit_sha, repo.id, &conn)
        {
            Ok(Some(c)) => c,
            Ok(None) => {
                return ToolCallResult::error(format!(
                    "Head commit {} not found",
                    branch.head_commit_sha
                ));
            }
            Err(e) => return ToolCallResult::error(format!("Failed to get head commit: {}", e)),
        };

    let build_status: BuildStatus = head_commit
        .get_build_status(&conn)
        .ok()
        .flatten()
        .into();

    let status_str: String = build_status.into();

    let result = json!({
        "repo": repo_str,
        "branch": branch_name,
        "head_sha": branch.head_commit_sha,
        "build_status": status_str,
    });

    ToolCallResult::text(serde_json::to_string_pretty(&result).unwrap_or_default())
}

async fn handle_deploy(
    arguments: Value,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    octocrabs: &Octocrabs,
) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: name".to_string()),
    };

    let config = match get_deploy_config(client, name).await {
        Ok(Some(c)) => c,
        Ok(None) => return ToolCallResult::error(format!("Deploy config '{}' not found", name)),
        Err(e) => return ToolCallResult::error(format!("Failed to get deploy config: {}", e)),
    };

    if config.is_orphaned() {
        return ToolCallResult::error(
            "Cannot deploy an orphaned config. Only undeploy is allowed.".to_string(),
        );
    }

    let action = if let Some(sha) = arguments.get("sha").and_then(|v| v.as_str()) {
        if !sha.is_empty() {
            Action::DeployCommit {
                sha: sha.to_string(),
            }
        } else if let Some(branch) = arguments.get("branch").and_then(|v| v.as_str()) {
            if !branch.is_empty() {
                Action::DeployBranch {
                    branch: branch.to_string(),
                }
            } else {
                Action::DeployLatest
            }
        } else {
            Action::DeployLatest
        }
    } else if let Some(branch) = arguments.get("branch").and_then(|v| v.as_str()) {
        if !branch.is_empty() {
            Action::DeployBranch {
                branch: branch.to_string(),
            }
        } else {
            Action::DeployLatest
        }
    } else {
        Action::DeployLatest
    };

    execute_deploy_action(&action, name, &config, client, pool, octocrabs).await
}

async fn handle_action(
    action_type: &str,
    arguments: Value,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    octocrabs: &Octocrabs,
) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: name".to_string()),
    };

    let config = match get_deploy_config(client, name).await {
        Ok(Some(c)) => c,
        Ok(None) => return ToolCallResult::error(format!("Deploy config '{}' not found", name)),
        Err(e) => return ToolCallResult::error(format!("Failed to get deploy config: {}", e)),
    };

    let action = match action_type {
        "undeploy" => Action::Undeploy,
        "bounce" => {
            if config.is_orphaned() {
                return ToolCallResult::error(
                    "Cannot bounce an orphaned config.".to_string(),
                );
            }
            Action::Bounce
        }
        "execute_job" => {
            if config.is_orphaned() {
                return ToolCallResult::error(
                    "Cannot execute job on an orphaned config.".to_string(),
                );
            }
            Action::ExecuteJob
        }
        "toggle_autodeploy" => {
            if config.is_orphaned() {
                return ToolCallResult::error(
                    "Cannot toggle autodeploy on an orphaned config.".to_string(),
                );
            }
            Action::ToggleAutodeploy
        }
        _ => return ToolCallResult::error(format!("Unknown action: {}", action_type)),
    };

    execute_deploy_action(&action, name, &config, client, pool, octocrabs).await
}

async fn execute_deploy_action(
    action: &Action,
    name: &str,
    config: &crate::kubernetes::DeployConfig,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    octocrabs: &Octocrabs,
) -> ToolCallResult {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("Database error: {}", e)),
    };

    let deployment_state = match DeploymentState::from_action(action, config, &conn) {
        Ok(s) => s,
        Err(e) => {
            return ToolCallResult::error(format!("Failed to resolve deployment state: {}", e));
        }
    };

    let deploy_action = match action {
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
        Action::Bounce => DeployAction::Bounce {
            name: name.to_string(),
        },
        Action::ExecuteJob => DeployAction::ExecuteJob {
            name: name.to_string(),
        },
        Action::ToggleAutodeploy => DeployAction::ToggleAutodeploy {
            name: name.to_string(),
        },
    };

    if let Err(e) = deploy_action
        .execute(client, octocrabs, config.config_repository())
        .await
    {
        return ToolCallResult::error(format!("Failed to execute action: {}", e));
    }

    // Log deploy event
    match DeployEvent::from_user_deploy_action(&deploy_action, &conn, config) {
        Ok(Some(event)) => {
            if let Err(e) = event.insert(&conn) {
                log::error!("Failed to insert MCP deploy event: {}", e);
            }
        }
        Ok(None) => {}
        Err(e) => {
            log::error!("Failed to create MCP deploy event: {}", e);
        }
    }

    let action_desc = match action {
        Action::DeployLatest => "Deploy (latest)".to_string(),
        Action::DeployBranch { branch } => format!("Deploy (branch: {})", branch),
        Action::DeployCommit { sha } => format!("Deploy (sha: {})", sha),
        Action::Undeploy => "Undeploy".to_string(),
        Action::Bounce => "Bounce".to_string(),
        Action::ExecuteJob => "Execute job".to_string(),
        Action::ToggleAutodeploy => "Toggle autodeploy".to_string(),
    };

    ToolCallResult::text(format!("Successfully executed: {} on {}", action_desc, name))
}
