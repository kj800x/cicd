use crate::db::Commit;
use crate::prelude::*;
use kube::{
    api::{Api, Patch, PatchParams},
    client::Client,
    ResourceExt,
};
use regex::Regex;
use std::collections::HashMap;

/// Get the latest successful build for a branch
fn get_latest_successful_build(
    owner: String,
    repo: String,
    branch: String,
    conn: &PooledConnection<SqliteConnectionManager>,
) -> Result<Option<Commit>, rusqlite::Error> {
    // Get the repository ID
    let repo_id = match get_repo(conn, owner.clone(), repo.clone())? {
        Some(repo) => repo.id,
        None => return Ok(None),
    };

    // Get the branch ID
    let branch_id = match get_branch_by_name(&branch, repo_id as u64, conn)? {
        Some(branch) => branch.id,
        None => return Ok(None),
    };

    // Get the latest successful build for this branch
    let mut stmt = conn.prepare(
        r#"
        SELECT c.id, c.sha, c.message, c.timestamp, c.build_status, c.build_url
        FROM git_commit c
        JOIN git_commit_branch cb ON c.sha = cb.commit_sha
        WHERE cb.branch_id = ?1
        AND c.build_status = 'Success'
        ORDER BY c.timestamp DESC
        LIMIT 1
        "#,
    )?;

    let commit = stmt.query_row([branch_id], |row| {
        Ok(Commit {
            id: row.get(0)?,
            sha: row.get(1)?,
            message: row.get(2)?,
            timestamp: row.get(3)?,
            build_status: row.get::<_, Option<String>>(4)?.into(),
            build_url: row.get(5)?,
        })
    });

    match commit {
        Ok(commit) => Ok(Some(commit)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Handler for the deploy configs page
pub async fn deploy_configs(
    _pool: web::Data<Pool<SqliteConnectionManager>>,
    query: web::Query<std::collections::HashMap<String, String>>,
) -> impl Responder {
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
                style {
                    r#"
                    :root {
                        --success-color: #2ecc71;
                        --failure-color: #e74c3c;
                        --pending-color: #f39c12;
                        --none-color: #7f8c8d;
                        --bg-color: #f7f9fc;
                        --card-bg: #ffffff;
                        --text-color: #333333;
                        --accent-color: #3498db;
                        --border-color: #e0e0e0;
                        --danger-color: #e74c3c;
                    }
                    body {
                        font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
                        background-color: var(--bg-color);
                        color: var(--text-color);
                        margin: 0;
                        padding: 20px;
                    }
                    header {
                        text-align: center;
                        margin-bottom: 30px;
                    }
                    h1 {
                        color: var(--accent-color);
                        margin-bottom: 5px;
                    }
                    .subtitle {
                        color: #666;
                        font-size: 1.1rem;
                    }
                    .nav-links {
                        display: flex;
                        justify-content: center;
                        margin-bottom: 20px;
                    }
                    .nav-links a {
                        margin: 0 10px;
                        padding: 8px 16px;
                        color: var(--accent-color);
                        text-decoration: none;
                        border-radius: 4px;
                        transition: background-color 0.2s;
                    }
                    .nav-links a:hover {
                        background-color: rgba(52, 152, 219, 0.1);
                    }
                    .nav-links a.active {
                        background-color: var(--accent-color);
                        color: white;
                    }
                    .content-container {
                        display: flex;
                        max-width: 1200px;
                        margin: 0 auto;
                        gap: 24px;
                    }
                    .left-box {
                        background-color: var(--card-bg);
                        border-radius: 8px;
                        box-shadow: 0 2px 10px rgba(0, 0, 0, 0.08);
                        width: 300px;
                        padding: 20px;
                        border: 2px solid var(--border-color);
                    }
                    .right-box {
                        background-color: var(--card-bg);
                        border-radius: 8px;
                        box-shadow: 0 2px 10px rgba(0, 0, 0, 0.08);
                        flex-grow: 1;
                        padding: 20px;
                        border: 2px solid var(--border-color);
                    }
                    select {
                        width: 100%;
                        padding: 10px;
                        border-radius: 4px;
                        border: 1px solid var(--border-color);
                        background-color: white;
                        font-size: 1rem;
                        margin-bottom: 20px;
                    }
                    .detail-row {
                        display: flex;
                        margin-bottom: 12px;
                    }
                    .detail-label {
                        font-weight: 600;
                        width: 150px;
                        margin-right: 16px;
                    }
                    .detail-value {
                        flex-grow: 1;
                        word-break: break-all;
                    }
                    .sha-value {
                        font-family: monospace;
                    }
                    .boolean-value {
                        padding: 4px 8px;
                        border-radius: 4px;
                        font-size: 0.9rem;
                        font-weight: 500;
                    }
                    .boolean-true {
                        background-color: rgba(46, 204, 113, 0.2);
                        color: #27ae60;
                    }
                    .boolean-false {
                        background-color: rgba(231, 76, 60, 0.2);
                        color: #c0392b;
                    }
                    .repo-info {
                        border-bottom: 1px solid var(--border-color);
                        margin-bottom: 20px;
                        padding-bottom: 20px;
                    }
                    .sha-info {
                        border-bottom: 1px solid var(--border-color);
                        margin-bottom: 20px;
                        padding-bottom: 20px;
                    }
                    .deploy-button {
                        padding: 10px 16px;
                        background-color: var(--accent-color);
                        color: white;
                        border: none;
                        border-radius: 4px;
                        font-size: 0.9rem;
                        cursor: pointer;
                        transition: background-color 0.2s;
                    }
                    .deploy-button:hover {
                        background-color: #2980b9;
                    }
                    .deploy-button.disabled {
                        background-color: #ccc;
                        cursor: not-allowed;
                    }
                    .actions-container {
                        display: flex;
                        flex-direction: column;
                        gap: 16px;
                    }
                    .action-form {
                        display: flex;
                        align-items: center;
                        gap: 16px;
                    }
                    .action-button {
                        padding: 10px 16px;
                        background-color: var(--accent-color);
                        color: white;
                        border: none;
                        border-radius: 4px;
                        font-size: 0.9rem;
                        cursor: pointer;
                        transition: background-color 0.2s;
                        min-width: 180px;
                    }
                    .action-button.primary {
                        background-color: var(--success-color);
                    }
                    .action-button.primary:hover {
                        background-color: #27ae60;
                    }
                    .action-button.danger {
                        background-color: var(--danger-color);
                    }
                    .action-button.danger:hover {
                        background-color: #c0392b;
                    }
                    .action-button:hover {
                        background-color: #2980b9;
                    }
                    .action-button:disabled {
                        background-color: #ccc;
                        cursor: not-allowed;
                    }
                    .action-description {
                        color: #666;
                        font-size: 0.9rem;
                    }
                    .input-group {
                        display: flex;
                        gap: 8px;
                    }
                    .sha-input {
                        padding: 10px;
                        border: 1px solid var(--border-color);
                        border-radius: 4px;
                        font-family: monospace;
                        min-width: 300px;
                    }
                    .info-message {
                        padding: 12px 16px;
                        border-radius: 4px;
                        margin-bottom: 16px;
                        font-size: 0.9rem;
                    }
                    .info-message.warning {
                        background-color: rgba(243, 156, 18, 0.2);
                        border-left: 4px solid var(--pending-color);
                        color: #7d5a00;
                    }
                    .warning-icon {
                        color: var(--pending-color);
                        margin-left: 8px;
                        display: inline-flex;
                        align-items: center;
                    }
                    "#
                }
                script {
                    r#"
                    function updateSelection() {
                        const selectElement = document.getElementById('deployConfigSelect');
                        const selectedValue = selectElement.value;
                        window.location.href = '/deploy-configs?selected=' + encodeURIComponent(selectedValue);
                    }
                    "#
                }
            }
            body {
                header {
                    h1 { "Kubernetes DeployConfig Dashboard" }
                    div class="subtitle" { "View and manage deployment configurations" }
                }

                div class="nav-links" {
                    a href="/" { "Recent Branches" }
                    a href="/all-recent-builds" { "All Recent Builds" }
                    a href="/deploy-configs" class="active" { "Deploy Configs" }
                }

                @if sorted_deploy_configs.is_empty() {
                    div style="text-align:center; margin-top:40px;" {
                        h2 { "No DeployConfigs Found" }
                        p { "There are no DeployConfigs in the Kubernetes cluster." }
                    }
                } @else {
                    div class="content-container" {
                        // Left side box with dropdown
                        div class="left-box" {
                            h3 { "Select DeployConfig" }
                            select id="deployConfigSelect" onchange="updateSelection()" {
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

                        // Right side box with details
                        @if let Some(selected_config) = selected_config {
                            div class="right-box" {
                                h3 { "DeployConfig Details" }

                                div class="repo-info" {
                                    h4 { "Repository Information" }

                                    div class="detail-row" {
                                        div class="detail-label" { "Repository:" }
                                        div class="detail-value" {
                                            (format!("{}/{}",
                                                selected_config.spec.spec.repo.owner,
                                                selected_config.spec.spec.repo.repo))
                                        }
                                    }

                                    div class="detail-row" {
                                        div class="detail-label" { "Default Branch:" }
                                        div class="detail-value" { (selected_config.spec.spec.repo.default_branch) }
                                    }

                                    div class="detail-row" {
                                        div class="detail-label" { "Current Branch:" }
                                        div class="detail-value" {
                                            @if let Some(status) = &selected_config.status {
                                                @if let Some(current_branch) = &status.current_branch {
                                                    (current_branch)
                                                    @if current_branch != &selected_config.spec.spec.repo.default_branch {
                                                        span class="warning-icon" title="Current branch is different from default branch" {
                                                            "⚠️"
                                                        }
                                                    }
                                                } @else {
                                                    "Not set"
                                                }
                                            } @else {
                                                "Not set"
                                            }
                                        }
                                    }

                                    div class="detail-row" {
                                        div class="detail-label" { "Auto-deploy:" }
                                        div class="detail-value" {
                                            @let current_autodeploy = selected_config.current_autodeploy();
                                            @let default_autodeploy = selected_config.spec.spec.autodeploy;

                                            div style="display: flex; align-items: center; gap: 8px;" {
                                                span class=(format!("boolean-value {}", if current_autodeploy { "boolean-true" } else { "boolean-false" })) {
                                                    (if current_autodeploy { "Enabled" } else { "Disabled" })
                                                }
                                                @if current_autodeploy != default_autodeploy {
                                                    span class="warning-icon" title=(format!("Auto-deploy is {} (default is {})",
                                                        if current_autodeploy { "enabled" } else { "disabled" },
                                                        if default_autodeploy { "enabled" } else { "disabled" })) {
                                                        "⚠️"
                                                    }
                                                }
                                                form action=(format!("/api/toggle-autodeploy/{}/{}",
                                                    selected_config.namespace().unwrap_or_default(),
                                                    selected_config.name_any()))
                                                    method="post" style="margin-left: 8px;" {
                                                    button type="submit" class="action-button" {
                                                        (if current_autodeploy { "Disable Auto-deploy" } else { "Enable Auto-deploy" })
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                div class="sha-info" {
                                    h4 { "Deployment Status" }

                                    div class="detail-row" {
                                        div class="detail-label" { "Current SHA:" }
                                        div class="detail-value sha-value" {
                                            @if let Some(status) = &selected_config.status {
                                                @if let Some(sha) = &status.current_sha {
                                                    (sha)
                                                } @else {
                                                    "Not deployed"
                                                }
                                            } @else {
                                                "Not deployed"
                                            }
                                        }
                                    }

                                    div class="detail-row" {
                                        div class="detail-label" { "Latest SHA:" }
                                        div class="detail-value sha-value" {
                                            @if let Some(status) = &selected_config.status {
                                                @if let Some(sha) = &status.latest_sha {
                                                    (sha)
                                                } @else {
                                                    "Unknown"
                                                }
                                            } @else {
                                                "Unknown"
                                            }
                                        }
                                    }

                                    div class="detail-row" {
                                        div class="detail-label" { "Wanted SHA:" }
                                        div class="detail-value sha-value" {
                                            @if let Some(status) = &selected_config.status {
                                                @if let Some(sha) = &status.wanted_sha {
                                                    (sha)
                                                } @else {
                                                    "None"
                                                }
                                            } @else {
                                                "None"
                                            }
                                        }
                                    }

                                    // Add the deploy button if autodeploy is disabled
                                    @if !selected_config.current_autodeploy() {
                                        div class="detail-row" style="margin-top: 20px;" {
                                            // Check if we have a latest SHA and it's different from wanted SHA
                                            @let can_deploy = if let Some(status) = &selected_config.status {
                                                if let Some(latest_sha) = &status.latest_sha {
                                                    if let Some(wanted_sha) = &status.wanted_sha {
                                                        latest_sha != wanted_sha
                                                    } else {
                                                        true // No wanted SHA, so we can deploy
                                                    }
                                                } else {
                                                    false // No latest SHA, can't deploy
                                                }
                                            } else {
                                                false // No status, can't deploy
                                            };

                                            @if can_deploy {
                                                // Enable button if we can deploy
                                                form action=(format!("/api/deploy/{}/{}",
                                                    selected_config.namespace().unwrap_or_default(),
                                                    selected_config.name_any()))
                                                    method="post" {
                                                    button type="submit" class="deploy-button" {
                                                        "Deploy Latest"
                                                    }
                                                }
                                            } @else {
                                                // Disable button if we can't deploy
                                                button type="button" class="deploy-button disabled" disabled {
                                                    "No Update Available"
                                                }
                                            }
                                        }
                                    }
                                }

                                h4 { "Actions" }
                                div class="actions-container" {
                                    @if selected_config.current_autodeploy() {
                                        // If autodeploy is enabled, show message that manual actions are disabled
                                        div class="info-message warning" {
                                            "Automatic deployment is enabled for this configuration. Manual deployment actions are disabled."
                                        }
                                    } @else {
                                        // 1. Deploy Latest - only show if there's a latest SHA and it's different from wanted SHA
                                        @let can_deploy_latest = if let Some(status) = &selected_config.status {
                                            if let Some(latest_sha) = &status.latest_sha {
                                                if let Some(wanted_sha) = &status.wanted_sha {
                                                    latest_sha != wanted_sha // Only if different
                                                } else {
                                                    true // No wanted SHA, can deploy
                                                }
                                            } else {
                                                false // No latest SHA
                                            }
                                        } else {
                                            false // No status
                                        };

                                        @if can_deploy_latest {
                                            form action=(format!("/api/deploy/{}/{}",
                                                selected_config.namespace().unwrap_or_default(),
                                                selected_config.name_any()))
                                                method="post" class="action-form" {
                                                button type="submit" class="action-button primary" {
                                                    "Deploy Latest Version"
                                                }
                                                span class="action-description" {
                                                    "Update deployment to use the latest successful build."
                                                }
                                            }
                                        }

                                        // 2. Undeploy - only show if there's a current deployment (wanted SHA is set)
                                        @let can_undeploy = if let Some(status) = &selected_config.status {
                                            status.wanted_sha.is_some()
                                        } else {
                                            false
                                        };

                                        @if can_undeploy {
                                            form action=(format!("/api/undeploy/{}/{}",
                                                selected_config.namespace().unwrap_or_default(),
                                                selected_config.name_any()))
                                                method="post" class="action-form" {
                                                button type="submit" class="action-button danger" {
                                                    "Undeploy"
                                                }
                                                span class="action-description" {
                                                    "Remove the deployment from the cluster."
                                                }
                                            }
                                        }

                                        // 3. Deploy Specific Version - always show for manual deployments
                                        form action=(format!("/api/deploy-specific/{}/{}",
                                            selected_config.namespace().unwrap_or_default(),
                                            selected_config.name_any()))
                                            method="post" class="action-form" {
                                            div class="input-group" {
                                                input type="text" name="sha" placeholder="Enter commit SHA" required
                                                    class="sha-input" pattern="[0-9a-fA-F]{5,40}"
                                                    title="Enter a valid git SHA (at least 5 hex characters)";
                                                button type="submit" class="action-button" {
                                                    "Deploy Specific Version"
                                                }
                                            }
                                            span class="action-description" {
                                                "Deploy a specific version by entering its commit SHA."
                                            }
                                        }

                                        // Add branch change form
                                        form action=(format!("/api/override-branch/{}/{}",
                                            selected_config.namespace().unwrap_or_default(),
                                            selected_config.name_any()))
                                            method="post" class="action-form" {
                                            div class="input-group" {
                                                input type="text" name="branch" placeholder="Enter branch name" required
                                                    class="sha-input";
                                                button type="submit" class="action-button" {
                                                    "Change Branch"
                                                }
                                            }
                                            span class="action-description" {
                                                "Change the current branch and deploy the latest successful build from that branch."
                                            }
                                        }
                                    }
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
) -> impl Responder {
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

    match deploy_configs_api.get(&name).await {
        Ok(config) => {
            // Check if it has autodeploy enabled (shouldn't happen due to UI, but just to be sure)
            if config.current_autodeploy() {
                return HttpResponse::BadRequest()
                    .content_type("text/html; charset=utf-8")
                    .body("Cannot manually deploy when autodeploy is enabled.");
            }

            // Get the latest SHA
            let latest_sha = if let Some(status) = &config.status {
                if let Some(sha) = &status.latest_sha {
                    sha.clone()
                } else {
                    return HttpResponse::BadRequest()
                        .content_type("text/html; charset=utf-8")
                        .body("No latest SHA available for deployment.");
                }
            } else {
                return HttpResponse::BadRequest()
                    .content_type("text/html; charset=utf-8")
                    .body("No status available for the DeployConfig.");
            };

            // Update the wanted SHA
            let status = serde_json::json!({
                "status": {
                    "wantedSha": latest_sha
                }
            });

            // Apply the status update
            let patch = Patch::Merge(&status);
            let params = PatchParams::default();

            match deploy_configs_api
                .patch_status(&name, &params, &patch)
                .await
            {
                Ok(_) => {
                    // Redirect back to the DeployConfig page with the selected config
                    HttpResponse::SeeOther()
                        .append_header((
                            "Location",
                            format!("/deploy-configs?selected={}/{}", namespace, name),
                        ))
                        .finish()
                }
                Err(e) => {
                    log::error!("Failed to update DeployConfig status: {}", e);
                    HttpResponse::InternalServerError()
                        .content_type("text/html; charset=utf-8")
                        .body(format!("Failed to update DeployConfig status: {}", e))
                }
            }
        }
        Err(e) => {
            log::error!("Failed to get DeployConfig {}/{}: {}", namespace, name, e);
            HttpResponse::NotFound()
                .content_type("text/html; charset=utf-8")
                .body(format!("DeployConfig {}/{} not found.", namespace, name))
        }
    }
}

/// Handler for undeploying (setting wantedSha to null)
#[post("/api/undeploy/{namespace}/{name}")]
pub async fn undeploy_config(
    path: web::Path<(String, String)>,
    client: Option<web::Data<Client>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();

    // Check if Kubernetes client is available
    let client = match client {
        Some(client) => client,
        None => {
            return HttpResponse::ServiceUnavailable()
                .content_type("text/html; charset=utf-8")
                .body("Kubernetes client is not available. Undeploy functionality is disabled.");
        }
    };

    // Get the DeployConfig
    let deploy_configs_api: Api<DeployConfig> =
        Api::namespaced(client.get_ref().clone(), &namespace);

    match deploy_configs_api.get(&name).await {
        Ok(config) => {
            // Check if it has autodeploy enabled
            if config.current_autodeploy() {
                return HttpResponse::BadRequest()
                    .content_type("text/html; charset=utf-8")
                    .body("Cannot manually undeploy when autodeploy is enabled.");
            }

            // Set wantedSha to null
            let status = serde_json::json!({
                "status": {
                    "wantedSha": null
                }
            });

            // Apply the status update
            let patch = Patch::Merge(&status);
            let params = PatchParams::default();

            match deploy_configs_api
                .patch_status(&name, &params, &patch)
                .await
            {
                Ok(_) => {
                    // Redirect back to the DeployConfig page with the selected config
                    HttpResponse::SeeOther()
                        .append_header((
                            "Location",
                            format!("/deploy-configs?selected={}/{}", namespace, name),
                        ))
                        .finish()
                }
                Err(e) => {
                    log::error!("Failed to update DeployConfig status: {}", e);
                    HttpResponse::InternalServerError()
                        .content_type("text/html; charset=utf-8")
                        .body(format!("Failed to update DeployConfig status: {}", e))
                }
            }
        }
        Err(e) => {
            log::error!("Failed to get DeployConfig {}/{}: {}", namespace, name, e);
            HttpResponse::NotFound()
                .content_type("text/html; charset=utf-8")
                .body(format!("DeployConfig {}/{} not found.", namespace, name))
        }
    }
}

/// Handler for deploying a specific SHA
#[post("/api/deploy-specific/{namespace}/{name}")]
pub async fn deploy_specific_config(
    path: web::Path<(String, String)>,
    form: web::Form<HashMap<String, String>>,
    client: Option<web::Data<Client>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();

    // Get the SHA from the form
    let sha = match form.get("sha") {
        Some(sha) => sha,
        None => {
            return HttpResponse::BadRequest()
                .content_type("text/html; charset=utf-8")
                .body("No SHA provided.");
        }
    };

    // Validate SHA format (simple validation, at least 5 hex characters)
    let sha_regex = Regex::new(r"^[0-9a-fA-F]{5,40}$").unwrap();
    if !sha_regex.is_match(sha) {
        return HttpResponse::BadRequest()
            .content_type("text/html; charset=utf-8")
            .body("Invalid SHA format. SHA must be 5-40 hex characters.");
    }

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

    match deploy_configs_api.get(&name).await {
        Ok(config) => {
            // Check if it has autodeploy enabled
            if config.current_autodeploy() {
                return HttpResponse::BadRequest()
                    .content_type("text/html; charset=utf-8")
                    .body("Cannot manually deploy when autodeploy is enabled.");
            }

            // Update the wanted SHA to the specified value
            let status = serde_json::json!({
                "status": {
                    "wantedSha": sha
                }
            });

            // Apply the status update
            let patch = Patch::Merge(&status);
            let params = PatchParams::default();

            match deploy_configs_api
                .patch_status(&name, &params, &patch)
                .await
            {
                Ok(_) => {
                    // Redirect back to the DeployConfig page with the selected config
                    HttpResponse::SeeOther()
                        .append_header((
                            "Location",
                            format!("/deploy-configs?selected={}/{}", namespace, name),
                        ))
                        .finish()
                }
                Err(e) => {
                    log::error!("Failed to update DeployConfig status: {}", e);
                    HttpResponse::InternalServerError()
                        .content_type("text/html; charset=utf-8")
                        .body(format!("Failed to update DeployConfig status: {}", e))
                }
            }
        }
        Err(e) => {
            log::error!("Failed to get DeployConfig {}/{}: {}", namespace, name, e);
            HttpResponse::NotFound()
                .content_type("text/html; charset=utf-8")
                .body(format!("DeployConfig {}/{} not found.", namespace, name))
        }
    }
}

/// Handler for overriding the branch of a DeployConfig
#[post("/api/override-branch/{namespace}/{name}")]
pub async fn override_branch(
    path: web::Path<(String, String)>,
    form: web::Form<HashMap<String, String>>,
    client: Option<web::Data<Client>>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();
    log::debug!(
        "Received branch override request for {}/{}",
        namespace,
        name
    );

    // Get the branch from the form
    let branch = match form.get("branch") {
        Some(branch) => {
            log::debug!("Branch override value: {}", branch);
            branch.clone()
        }
        None => {
            log::error!("No branch provided in form");
            return HttpResponse::BadRequest()
                .content_type("text/html; charset=utf-8")
                .body("No branch provided.");
        }
    };

    // Check if Kubernetes client is available
    let client = match client {
        Some(client) => {
            log::debug!("Kubernetes client is available");
            client
        }
        None => {
            log::error!("Kubernetes client is not available");
            return HttpResponse::ServiceUnavailable()
                .content_type("text/html; charset=utf-8")
                .body("Kubernetes client is not available");
        }
    };

    // Get the DeployConfig
    let deploy_configs_api: Api<DeployConfig> =
        Api::namespaced(client.get_ref().clone(), &namespace);

    match deploy_configs_api.get(&name).await {
        Ok(config) => {
            log::debug!("Found DeployConfig {}/{}", namespace, name);
            log::debug!(
                "Current branch: {:?}",
                config
                    .status
                    .as_ref()
                    .and_then(|s| s.current_branch.clone())
            );

            // Get the latest successful build for the new branch
            let conn = match pool.get() {
                Ok(conn) => conn,
                Err(e) => {
                    log::error!("Failed to get database connection: {}", e);
                    return HttpResponse::InternalServerError()
                        .content_type("text/html; charset=utf-8")
                        .body("Failed to get database connection");
                }
            };

            // Get the latest successful build for the new branch
            let latest_sha = match get_latest_successful_build(
                config.spec.spec.repo.owner.clone(),
                config.spec.spec.repo.repo.clone(),
                branch.clone(),
                &conn,
            ) {
                Ok(Some(commit)) => {
                    log::debug!(
                        "Found latest successful build for branch {}: {}",
                        branch,
                        commit.sha
                    );
                    Some(commit.sha)
                }
                Ok(None) => {
                    log::debug!("No successful builds found for branch {}", branch);
                    return HttpResponse::BadRequest()
                        .content_type("text/html; charset=utf-8")
                        .body(format!("No successful builds found for branch {}", branch));
                }
                Err(e) => {
                    log::error!("Error getting latest successful build: {}", e);
                    return HttpResponse::InternalServerError()
                        .content_type("text/html; charset=utf-8")
                        .body(format!("Error getting latest successful build: {}", e));
                }
            };

            // Update the status with the new branch and SHA
            let status_patch = serde_json::json!({
                "status": {
                    "currentBranch": branch,
                    "latestSha": latest_sha,
                    "wantedSha": latest_sha
                }
            });

            log::debug!(
                "Status patch payload: {}",
                serde_json::to_string_pretty(&status_patch).unwrap()
            );

            let patch = Patch::Merge(&status_patch);
            let params = PatchParams::default();

            match deploy_configs_api
                .patch_status(&name, &params, &patch)
                .await
            {
                Ok(updated_config) => {
                    log::debug!(
                        "Successfully updated DeployConfig status {}/{}",
                        namespace,
                        name
                    );
                    log::debug!("Updated DeployConfig status: {:?}", updated_config.status);
                    // Redirect back to the DeployConfig page with the selected config
                    HttpResponse::SeeOther()
                        .append_header((
                            "Location",
                            format!("/deploy-configs?selected={}/{}", namespace, name),
                        ))
                        .finish()
                }
                Err(e) => {
                    log::error!("Failed to update DeployConfig status: {}", e);
                    HttpResponse::InternalServerError()
                        .content_type("text/html; charset=utf-8")
                        .body(format!("Failed to update DeployConfig status: {}", e))
                }
            }
        }
        Err(e) => {
            log::error!("Failed to get DeployConfig {}/{}: {}", namespace, name, e);
            HttpResponse::NotFound()
                .content_type("text/html; charset=utf-8")
                .body(format!("DeployConfig {}/{} not found.", namespace, name))
        }
    }
}

/// Handler for toggling autodeploy
#[post("/api/toggle-autodeploy/{namespace}/{name}")]
pub async fn toggle_autodeploy(
    path: web::Path<(String, String)>,
    client: Option<web::Data<Client>>,
) -> impl Responder {
    let (namespace, name) = path.into_inner();

    // Check if Kubernetes client is available
    let client = match client {
        Some(client) => client,
        None => {
            return HttpResponse::ServiceUnavailable()
                .content_type("text/html; charset=utf-8")
                .body("Kubernetes client is not available. Autodeploy toggle functionality is disabled.");
        }
    };

    // Get the DeployConfig
    let deploy_configs_api: Api<DeployConfig> =
        Api::namespaced(client.get_ref().clone(), &namespace);

    match deploy_configs_api.get(&name).await {
        Ok(config) => {
            // Get current autodeploy state
            let current_autodeploy = config.current_autodeploy();

            // Toggle the autodeploy state
            let status = serde_json::json!({
                "status": {
                    "autodeploy": !current_autodeploy
                }
            });

            // Apply the status update
            let patch = Patch::Merge(&status);
            let params = PatchParams::default();

            match deploy_configs_api
                .patch_status(&name, &params, &patch)
                .await
            {
                Ok(_) => {
                    // Redirect back to the DeployConfig page with the selected config
                    HttpResponse::SeeOther()
                        .append_header((
                            "Location",
                            format!("/deploy-configs?selected={}/{}", namespace, name),
                        ))
                        .finish()
                }
                Err(e) => {
                    log::error!("Failed to update DeployConfig status: {}", e);
                    HttpResponse::InternalServerError()
                        .content_type("text/html; charset=utf-8")
                        .body(format!("Failed to update DeployConfig status: {}", e))
                }
            }
        }
        Err(e) => {
            log::error!("Failed to get DeployConfig {}/{}: {}", namespace, name, e);
            HttpResponse::NotFound()
                .content_type("text/html; charset=utf-8")
                .body(format!("DeployConfig {}/{} not found.", namespace, name))
        }
    }
}
