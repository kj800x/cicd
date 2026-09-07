use kube::{Client, ResourceExt};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde_json::{json, Value};

use crate::build_status::BuildStatus;
use crate::crab_ext::Octocrabs;
use crate::db::blocker::Blocker;
use crate::db::git_branch::GitBranch;
use crate::db::git_repo::GitRepo;
use crate::db::revision::Revision;
use crate::kubernetes::api::{
    get_all_deploy_configs, get_deploy_config, list_namespace_objects, ListMode,
};
use crate::kubernetes::repo::DeploymentState;
use crate::web::Action;
use crate::web::ResourceStatuses;

use super::protocol::{Tool, ToolCallResult};

// NOTE: The `autodeploy` flag and the `toggle_autodeploy` action are deliberately
// not exposed over MCP. Autodeploy is not implemented yet (see
// https://github.com/kj800x/cicd/issues/20), and surfacing the flag misleads
// agents into assuming new builds roll out on their own. Re-add both once the
// feature is actually wired up. Blockers will suspend autodeploy when it exists;
// today they refuse manual deploys, which the `deploy` tool reports.
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
            description: "Get details of a single deploy config by name, including resource statuses (deployments, pods, services, ingresses, jobs)".to_string(),
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
            name: "list_blockers".to_string(),
            description: "List blockers. A blocker is a per-config hold with a reason: while one is active, deploys of that config are refused until it is cleared. Undeploy, bounce and execute_job still work.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Only this deploy config (default: every config with an active blocker)" },
                    "include_cleared": { "type": "boolean", "description": "With name: also return cleared blockers, newest first (default false)" }
                },
                "required": []
            }),
        },
        Tool {
            name: "add_blocker".to_string(),
            description: "Hold deploys of a config until the blocker is cleared. Use for incidents, freezes, or after a rollback.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the deploy config" },
                    "reason": { "type": "string", "description": "Why deploys must wait (required)" },
                    "by": { "type": "string", "description": "Who is holding it (default: mcp)" }
                },
                "required": ["name", "reason"]
            }),
        },
        Tool {
            name: "list_revisions".to_string(),
            description: "List a config's revisions, newest first. A revision is the record of one deploy or undeploy: the config commit and the value deployed for each parameter. Use a deploy revision's id with rollback.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the deploy config" },
                    "limit": { "type": "integer", "description": "How many to return (default 20)" }
                },
                "required": ["name"]
            }),
        },
        Tool {
            name: "rollback".to_string(),
            description: "Redeploy exactly what a revision deployed, then add a blocker so the config stays held until someone clears it. Not refused by existing blockers. Only deploy revisions can be rolled back to.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the deploy config" },
                    "revision": { "type": "integer", "description": "Revision id from list_revisions" }
                },
                "required": ["name", "revision"]
            }),
        },
        Tool {
            name: "clear_blocker".to_string(),
            description: "Clear one blocker by id, allowing deploys again once no active blockers remain on the config.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the deploy config the blocker belongs to" },
                    "id": { "type": "integer", "description": "Blocker id from list_blockers" },
                    "by": { "type": "string", "description": "Who is clearing it (default: mcp)" }
                },
                "required": ["name", "id"]
            }),
        },
    ]
}

fn revision_json(r: &Revision) -> Value {
    json!({
        "id": r.id,
        "config": r.config_name,
        "created_at": r.created_at,
        "actor": r.actor,
        "action": r.action,
        "reason": r.reason,
        "config_sha": r.config_sha,
        "config_branch": r.config_branch,
        "parameters": r.parameters.iter().map(|p| json!({
            "name": p.name,
            "type": p.kind,
            "value": p.value,
            "branch": p.branch,
        })).collect::<Vec<_>>(),
    })
}

fn blocker_json(b: &Blocker) -> Value {
    json!({
        "id": b.id,
        "config": b.config_name,
        "reason": b.reason,
        "active": b.is_active(),
        "created_by": b.created_by,
        "created_at": b.created_at,
        "cleared_by": b.cleared_by,
        "cleared_at": b.cleared_at,
    })
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
        "get_deploy_config" => handle_get_deploy_config(arguments, client, pool).await,
        "get_build_status" => handle_get_build_status(arguments, pool).await,
        "deploy" => handle_deploy(arguments, client, pool, octocrabs).await,
        "undeploy" => handle_action("undeploy", arguments, client, pool, octocrabs).await,
        "bounce" => handle_action("bounce", arguments, client, pool, octocrabs).await,
        "execute_job" => handle_action("execute_job", arguments, client, pool, octocrabs).await,
        "list_revisions" => handle_list_revisions(arguments, pool),
        "rollback" => handle_rollback(arguments, client, pool, octocrabs).await,
        "list_blockers" => handle_list_blockers(arguments, pool),
        "add_blocker" => handle_add_blocker(arguments, pool),
        "clear_blocker" => handle_clear_blocker(arguments, pool),
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
            let blockers = Blocker::active_for(&conn, &config.name_any()).unwrap_or_default();

            json!({
                "name": config.name_any(),
                "namespace": config.namespace().unwrap_or_else(|| "default".to_string()),
                "team": config.team(),
                "kind": config.kind(),
                "state": state,
                "orphaned": config.is_orphaned(),
                "blocked": !blockers.is_empty(),
                "blockers": blockers.iter().map(blocker_json).collect::<Vec<_>>(),
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
    pool: &Pool<SqliteConnectionManager>,
) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: name".to_string()),
    };
    let (blockers, latest_revision) = match pool.get() {
        Ok(conn) => (
            Blocker::active_for(&conn, name).unwrap_or_default(),
            Revision::latest_for(&conn, name).ok().flatten(),
        ),
        Err(e) => return ToolCallResult::error(format!("Database error: {}", e)),
    };

    let config = match get_deploy_config(client, name).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return ToolCallResult::error(format!("Deploy config '{}' not found", name));
        }
        Err(e) => return ToolCallResult::error(format!("Failed to get deploy config: {}", e)),
    };

    let deployment_state = config.deployment_state();
    let (artifact_sha, artifact_branch, config_sha, config_branch, state) = match &deployment_state
    {
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
    let namespace = config.namespace().unwrap_or_else(|| "default".to_string());

    let namespaced_objs = match list_namespace_objects(client, &namespace, ListMode::All).await {
        Ok(objs) => objs,
        Err(e) => {
            log::warn!("Failed to list namespace objects for {}: {}", namespace, e);
            vec![]
        }
    };
    let resources = config.format_resources_json(&namespaced_objs);

    let result = json!({
        "name": config.name_any(),
        "namespace": namespace,
        "team": config.team(),
        "kind": config.kind(),
        "state": state,
        "orphaned": config.is_orphaned(),
        "blocked": !blockers.is_empty(),
        "blockers": blockers.iter().map(blocker_json).collect::<Vec<_>>(),
        "latest_revision": latest_revision.as_ref().map(revision_json),
        "supports_bounce": config.supports_bounce(),
        "supports_execute_job": config.supports_execute_job(),
        "artifact_repo": artifact_repo.as_ref().map(|r| format!("{}/{}", r.owner, r.repo)),
        "artifact_default_branch": artifact_repo.as_ref().map(|r| &r.branch),
        "config_repo": format!("{}/{}", config_repo.owner, config_repo.repo),
        "artifact_sha": artifact_sha,
        "artifact_branch": artifact_branch,
        "config_sha": config_sha,
        "config_branch": config_branch,
        "resources": resources,
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

    let build_status: BuildStatus = head_commit.get_build_status(&conn).ok().flatten().into();

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
                return ToolCallResult::error("Cannot bounce an orphaned config.".to_string());
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
        _ => return ToolCallResult::error(format!("Unknown action: {}", action_type)),
    };

    execute_deploy_action(&action, name, &config, client, pool, octocrabs).await
}

fn handle_list_revisions(arguments: Value, pool: &Pool<SqliteConnectionManager>) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: name".to_string()),
    };
    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|l| l.clamp(1, 200) as usize)
        .unwrap_or(20);
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("Database error: {}", e)),
    };
    match Revision::list_for(&conn, name, limit) {
        Ok(revs) => ToolCallResult::text(
            serde_json::to_string_pretty(&revs.iter().map(revision_json).collect::<Vec<_>>())
                .unwrap_or_default(),
        ),
        Err(e) => ToolCallResult::error(format!("Failed to list revisions: {}", e)),
    }
}

async fn handle_rollback(
    arguments: Value,
    client: &Client,
    pool: &Pool<SqliteConnectionManager>,
    octocrabs: &Octocrabs,
) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: name".to_string()),
    };
    let revision = match arguments.get("revision").and_then(|v| v.as_i64()) {
        Some(r) => r,
        None => return ToolCallResult::error("Missing required parameter: revision".to_string()),
    };
    let config = match get_deploy_config(client, name).await {
        Ok(Some(c)) => c,
        Ok(None) => return ToolCallResult::error(format!("Deploy config '{}' not found", name)),
        Err(e) => return ToolCallResult::error(format!("Failed to get deploy config: {}", e)),
    };
    if config.is_orphaned() {
        return ToolCallResult::error(
            "Cannot roll back an orphaned config; its config repository is gone.".to_string(),
        );
    }
    execute_deploy_action(
        &Action::Rollback { revision },
        name,
        &config,
        client,
        pool,
        octocrabs,
    )
    .await
}

fn handle_list_blockers(arguments: Value, pool: &Pool<SqliteConnectionManager>) -> ToolCallResult {
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("Database error: {}", e)),
    };
    let include_cleared = arguments
        .get("include_cleared")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let result = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(name) if include_cleared => Blocker::history_for(&conn, name, 50),
        Some(name) => Blocker::active_for(&conn, name),
        None => Blocker::all_active(&conn),
    };
    match result {
        Ok(blockers) => ToolCallResult::text(
            serde_json::to_string_pretty(&blockers.iter().map(blocker_json).collect::<Vec<_>>())
                .unwrap_or_default(),
        ),
        Err(e) => ToolCallResult::error(format!("Failed to list blockers: {}", e)),
    }
}

fn handle_add_blocker(arguments: Value, pool: &Pool<SqliteConnectionManager>) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: name".to_string()),
    };
    let reason = arguments
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let by = arguments
        .get("by")
        .and_then(|v| v.as_str())
        .unwrap_or("mcp");
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("Database error: {}", e)),
    };
    match Blocker::create(&conn, name, reason, by) {
        Ok(b) => ToolCallResult::text(
            serde_json::to_string_pretty(&blocker_json(&b)).unwrap_or_default(),
        ),
        Err(e) => ToolCallResult::error(format!("Failed to add blocker: {}", e)),
    }
}

fn handle_clear_blocker(arguments: Value, pool: &Pool<SqliteConnectionManager>) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return ToolCallResult::error("Missing required parameter: name".to_string()),
    };
    let id = match arguments.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return ToolCallResult::error("Missing required parameter: id".to_string()),
    };
    let by = arguments
        .get("by")
        .and_then(|v| v.as_str())
        .unwrap_or("mcp");
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => return ToolCallResult::error(format!("Database error: {}", e)),
    };
    match Blocker::get(&conn, id) {
        Ok(Some(b)) if b.config_name == name => {}
        Ok(_) => return ToolCallResult::error(format!("No blocker {} on {}", id, name)),
        Err(e) => return ToolCallResult::error(format!("Failed to look up blocker: {}", e)),
    }
    match Blocker::clear(&conn, id, by) {
        Ok(true) => ToolCallResult::text(format!("Cleared blocker {} on {}", id, name)),
        Ok(false) => {
            ToolCallResult::text(format!("Blocker {} on {} was already cleared", id, name))
        }
        Err(e) => ToolCallResult::error(format!("Failed to clear blocker: {}", e)),
    }
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

    match crate::deploys::run_action(action, config, client, octocrabs, &conn, "mcp").await {
        Ok(_) => {}
        Err(crate::error::AppError::Blocked(message)) => {
            return ToolCallResult::error(format!(
                "Deploy refused: {} Use list_blockers to see them.",
                message
            ));
        }
        Err(crate::error::AppError::InvalidInput(message)) => {
            return ToolCallResult::error(message);
        }
        Err(e) => return ToolCallResult::error(format!("Failed to execute action: {}", e)),
    }

    let action_desc = match action {
        Action::DeployLatest => "Deploy (latest)".to_string(),
        Action::DeployBranch { branch } => format!("Deploy (branch: {})", branch),
        Action::DeployCommit { sha } => format!("Deploy (sha: {})", sha),
        Action::Rollback { revision } => format!("Rollback (revision: {})", revision),
        Action::Undeploy => "Undeploy".to_string(),
        Action::Bounce => "Bounce".to_string(),
        Action::ExecuteJob => "Execute job".to_string(),
        Action::ToggleAutodeploy => "Toggle autodeploy".to_string(),
    };

    ToolCallResult::text(format!(
        "Successfully executed: {} on {}",
        action_desc, name
    ))
}
