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
                        width: 400px;
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
                    .action-radio-group {
                        display: flex;
                        flex-direction: column;
                        gap: 12px;
                        margin-bottom: 20px;
                    }
                    .action-radio {
                        display: flex;
                        align-items: center;
                        gap: 8px;
                        padding: 8px;
                        border-radius: 4px;
                        border: 1px solid var(--border-color);
                        cursor: pointer;
                        transition: background-color 0.2s;
                    }
                    .action-radio:hover {
                        background-color: rgba(52, 152, 219, 0.1);
                    }
                    .action-radio input[type="radio"] {
                        margin: 0;
                    }
                    .action-input {
                        margin-top: 12px;
                        margin-bottom: 20px;
                    }
                    .action-input input {
                        width: 100%;
                        padding: 10px;
                        border-radius: 4px;
                        border: 1px solid var(--border-color);
                        font-family: monospace;
                    }
                    .primary-action-button {
                        width: 100%;
                        padding: 12px;
                        background-color: var(--accent-color);
                        color: white;
                        border: none;
                        border-radius: 4px;
                        font-size: 1rem;
                        cursor: pointer;
                        transition: background-color 0.2s;
                    }
                    .primary-action-button:hover {
                        background-color: #2980b9;
                    }
                    .primary-action-button.danger {
                        background-color: var(--danger-color);
                    }
                    .primary-action-button.danger:hover {
                        background-color: #c0392b;
                    }
                    .preview-container {
                        padding: 20px;
                        background-color: rgba(52, 152, 219, 0.1);
                        border-radius: 4px;
                        margin-top: 20px;
                    }
                    .preview-title {
                        font-weight: 600;
                        margin-bottom: 8px;
                    }
                    .preview-content {
                        font-family: monospace;
                        font-size: 1.1rem;
                    }
                    .preview-arrow {
                        margin: 0 8px;
                        color: var(--accent-color);
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

                    function submitActionForm() {
                        document.getElementById('actionForm').submit();
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
                        // Left side box with dropdown and actions
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

                            @if let Some(selected_config) = selected_config {
                                form id="actionForm" action="/deploy-configs" method="get" {
                                    input type="hidden" name="selected" value=(format!("{}/{}", selected_config.namespace().unwrap_or_default(), selected_config.name_any()));

                                    div class="action-radio-group" {
                                        @let current_action = query.get("action");
                                        label class="action-radio" {
                                            input type="radio" name="action" value="deploy-latest" checked[current_action.is_none() || current_action.unwrap() == "deploy-latest"] onchange="document.getElementById('actionForm').submit()";
                                            "Deploy Latest (Default Branch)"
                                        }
                                        label class="action-radio" {
                                            input type="radio" name="action" value="track-branch" checked[current_action.map_or(false, |a| a == "track-branch")] onchange="document.getElementById('actionForm').submit()";
                                            "Deploy and Track Branch"
                                        }
                                        label class="action-radio" {
                                            input type="radio" name="action" value="specific-commit" checked[current_action.map_or(false, |a| a == "specific-commit")] onchange="document.getElementById('actionForm').submit()";
                                            "Deploy Specific Commit"
                                        }
                                        label class="action-radio" {
                                            input type="radio" name="action" value="toggle-autodeploy" checked[current_action.map_or(false, |a| a == "toggle-autodeploy")] onchange="document.getElementById('actionForm').submit()";
                                            @if selected_config.current_autodeploy() {
                                                "Disable Autodeploy"
                                            } @else {
                                                "Enable Autodeploy"
                                            }
                                        }
                                        label class="action-radio" {
                                            input type="radio" name="action" value="undeploy" checked[current_action.map_or(false, |a| a == "undeploy")] onchange="document.getElementById('actionForm').submit()";
                                            "Undeploy"
                                        }
                                    }

                                    @if query.get("action").map_or(false, |a| a == "track-branch") {
                                        div class="action-input" {
                                            input type="text" name="branch" placeholder="Enter branch name" required;
                                        }
                                    }

                                    @if query.get("action").map_or(false, |a| a == "specific-commit") {
                                        div class="action-input" {
                                            input type="text" name="sha" placeholder="Enter commit SHA" required pattern="[0-9a-fA-F]{5,40}";
                                        }
                                    }
                                }

                                @if let Some(action) = query.get("action") {
                                    @match action.as_str() {
                                        "deploy-latest" => {
                                            form action=(format!("/api/deploy/{}/{}",
                                                selected_config.namespace().unwrap_or_default(),
                                                selected_config.name_any()))
                                                method="post" {
                                                button type="submit" class="primary-action-button" {
                                                    "Deploy Latest"
                                                }
                                            }
                                        }
                                        "track-branch" => {
                                            form action=(format!("/api/override-branch/{}/{}",
                                                selected_config.namespace().unwrap_or_default(),
                                                selected_config.name_any()))
                                                method="post" {
                                                input type="hidden" name="branch" value=(query.get("branch").unwrap_or(&"".to_string()));
                                                button type="submit" class="primary-action-button" {
                                                    "Deploy and Track Branch"
                                                }
                                            }
                                        }
                                        "specific-commit" => {
                                            form action=(format!("/api/deploy-specific/{}/{}",
                                                selected_config.namespace().unwrap_or_default(),
                                                selected_config.name_any()))
                                                method="post" {
                                                input type="hidden" name="sha" value=(query.get("sha").unwrap_or(&"".to_string()));
                                                button type="submit" class="primary-action-button" {
                                                    "Deploy Specific Commit"
                                                }
                                            }
                                        }
                                        "toggle-autodeploy" => {
                                            form action=(format!("/api/toggle-autodeploy/{}/{}",
                                                selected_config.namespace().unwrap_or_default(),
                                                selected_config.name_any()))
                                                method="post" {
                                                button type="submit" class="primary-action-button" {
                                                    @if selected_config.current_autodeploy() {
                                                        "Disable Autodeploy"
                                                    } @else {
                                                        "Enable Autodeploy"
                                                    }
                                                }
                                            }
                                        }
                                        "undeploy" => {
                                            form action=(format!("/api/undeploy/{}/{}",
                                                selected_config.namespace().unwrap_or_default(),
                                                selected_config.name_any()))
                                                method="post" {
                                                button type="submit" class="primary-action-button danger" {
                                                    "Undeploy"
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }

                        // Right side box with preview
                        @if let Some(selected_config) = selected_config {
                            div class="right-box" {
                                h3 { (format!("{}/{}", selected_config.namespace().unwrap_or_default(), selected_config.name_any())) }
                                div class="preview-container" {
                                    div class="preview-content" {
                                        @if let Some(action) = query.get("action") {
                                            @match action.as_str() {
                                                "deploy-latest" => {
                                                    @if let Some(status) = &selected_config.status {
                                                        @if let Some(wanted_sha) = &status.wanted_sha {
                                                            @if let Some(latest_sha) = &status.latest_sha {
                                                                @if wanted_sha == latest_sha {
                                                                    "unchanged, nothing to deploy"
                                                                } @else {
                                                                    (wanted_sha[..7]) span class="preview-arrow" { "→" } (latest_sha[..7])
                                                                }
                                                            } @else {
                                                                (wanted_sha[..7]) span class="preview-arrow" { "→" } "Unknown"
                                                            }
                                                        } @else {
                                                            "None" span class="preview-arrow" { "→" }
                                                            @if let Some(latest_sha) = &status.latest_sha {
                                                                (latest_sha[..7])
                                                            } @else {
                                                                "Unknown"
                                                            }
                                                        }
                                                    } @else {
                                                        "None" span class="preview-arrow" { "→" } "Unknown"
                                                    }
                                                }
                                                "track-branch" => {
                                                    @if let Some(status) = &selected_config.status {
                                                        @if let Some(wanted_sha) = &status.wanted_sha {
                                                            (wanted_sha[..7])
                                                        } @else {
                                                            "None"
                                                        }
                                                    } @else {
                                                        "None"
                                                    }
                                                    span class="preview-arrow" { "→" }
                                                    @if let Some(branch) = query.get("branch") {
                                                        (branch)
                                                    } @else {
                                                        "branch-name"
                                                    }
                                                }
                                                "specific-commit" => {
                                                    @if let Some(status) = &selected_config.status {
                                                        @if let Some(wanted_sha) = &status.wanted_sha {
                                                            (wanted_sha[..7])
                                                        } @else {
                                                            "None"
                                                        }
                                                    } @else {
                                                        "None"
                                                    }
                                                    span class="preview-arrow" { "→" }
                                                    @if let Some(sha) = query.get("sha") {
                                                        (sha[..7])
                                                    } @else {
                                                        "commit-sha"
                                                    }
                                                }
                                                "toggle-autodeploy" => {
                                                    "Autodeploy "
                                                    @if selected_config.current_autodeploy() {
                                                        "Enabled"
                                                    } @else {
                                                        "Disabled"
                                                    }
                                                    span class="preview-arrow" { "→" }
                                                    @if selected_config.current_autodeploy() {
                                                        "Disabled"
                                                    } @else {
                                                        "Enabled"
                                                    }
                                                }
                                                "undeploy" => {
                                                    @if let Some(status) = &selected_config.status {
                                                        @if let Some(wanted_sha) = &status.wanted_sha {
                                                            (wanted_sha[..7])
                                                        } @else {
                                                            "None"
                                                        }
                                                    } @else {
                                                        "None"
                                                    }
                                                    span class="preview-arrow" { "→" } "undeployed"
                                                }
                                                _ => {
                                                    "Select an action to see its effect"
                                                }
                                            }
                                        } @else {
                                            "Select an action to see its effect"
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
