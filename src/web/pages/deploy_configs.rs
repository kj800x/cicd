use crate::db::Commit;
use crate::prelude::*;
use crate::web::pages::header;
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
                        --primary-blue: #0969da;
                        --text-color: #3a485a;
                        --secondary-text: #57606a;
                        --border-color: #d0d7de;
                        --bg-light: #f6f8fa;
                        --green: #2da44e;
                        --header-bg: #24292e;
                        --danger-color: #f2545b;
                        --hubspot-orange: #ff7a59;
                        --disabled-color: #e5e7eb;
                    }
                    body {
                        font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
                        background-color: white;
                        color: var(--text-color);
                        margin: 0;
                        padding: 0;
                        line-height: 1.5;
                    }
                    .commit-sha {
                        font-family: monospace;
                        font-size: 0.85rem;
                        width: 65px;
                        color: #555;
                        margin-right: 12px;
                    }
                    .header {
                        background-color: var(--header-bg);
                        color: white;
                        padding: 8px 16px;
                        display: flex;
                        align-items: center;
                    }
                    .header-logo {
                        margin-right: 12px;
                    }
                    .header-nav {
                        display: flex;
                        gap: 16px;
                        margin-left: 24px;
                    }
                    .header-nav-item {
                        color: rgba(255, 255, 255, 0.7);
                        text-decoration: none;
                        font-size: 14px;
                        font-weight: 600;
                        padding: 8px 8px;
                    }
                    .header-nav-item:hover, .header-nav-item.active {
                        color: white;
                    }
                    .subheader {
                        border-bottom: 1px solid var(--border-color);
                        display: flex;
                        padding: 0 16px;
                    }
                    .subheader-brand {
                        display: flex;
                        align-items: center;
                        padding: 12px 0;
                        margin-right: 24px;
                        color: var(--text-color);
                        font-weight: 600;
                        text-decoration: none;
                    }
                    .subheader-brand img {
                        margin-right: 8px;
                    }
                    .subheader-nav {
                        display: flex;
                    }
                    .subheader-nav-item {
                        color: var(--text-color);
                        text-decoration: none;
                        padding: 12px 16px;
                        font-size: 14px;
                        border-bottom: 2px solid transparent;
                    }
                    .subheader-nav-item:hover {
                        border-bottom-color: #d0d7de;
                    }
                    .subheader-nav-item.active {
                        border-bottom-color: var(--primary-blue);
                        font-weight: 500;
                    }
                    .content {
                        padding: 24px;
                    }
                    .content-container {
                        display: flex;
                        gap: 40px;
                        max-width: 1200px;
                        margin: 0 auto;

                        @media (max-width: 768px) {
                            flex-direction: column;
                            align-items: center;

                            .right-box {
                                width: 100%;
                            }
                        }
                    }
                    .left-box {
                        width: 340px;
                        border: 2px solid #dfe3eb;
                        background-color: #f6f8fa;
                        padding: 20px;
                        flex-shrink: 0;
                    }
                    .right-box {
                        flex-grow: 1;
                    }
                    h1 {
                        font-size: 1.4em;
                        font-weight: 400;
                        border-bottom: 2px solid #ccd5e1;
                        margin-bottom: 16px;
                        padding-bottom: 16px;
                    }
                    h3 {
                        margin: 0;
                        font-weight: 600;
                        font-size: 14px;
                        color: var(--secondary-text);
                        margin-bottom: 8px;
                    }
                    h4 {
                        font-weight: 600;
                        margin: 0;
                    }
                    select {
                        width: 100%;
                        padding: 5px 12px;
                        font-size: 14px;
                        line-height: 20px;
                        border: 1px solid var(--border-color);
                        border-radius: 6px;
                        background-color: white;
                        appearance: none;
                        margin: 0px 0 16px;
                    }
                    select:hover {
                        background-color: #f3f4f6;
                    }
                    select:focus {
                        outline: none;
                        border-color: var(--primary-blue);
                        box-shadow: 0 0 0 3px rgba(9, 105, 218, 0.3);
                    }
                    .action-radio-group {
                        display: flex;
                        flex-direction: column;
                        margin-bottom: 24px;
                    }
                    .action-radio-group {
                        margin: 0 0 8px 0;
                        font-size: 14px;
                        font-weight: 400;
                        color: var(--secondary-text);
                    }
                    .action-radio {
                        display: flex;
                        align-items: center;
                        gap: 8px;
                        cursor: pointer;
                        font-size: 14px;
                        padding: 6px 0;
                    }
                    .action-radio input[type="radio"] {
                        margin: 0;
                        appearance: none;
                        width: 16px;
                        height: 16px;
                        border: 1px solid var(--border-color);
                        border-radius: 50%;
                        position: relative;
                        cursor: pointer;
                    }
                    .action-radio input[type="radio"]:checked {
                        border-color: var(--primary-blue);
                    }
                    .action-radio input[type="radio"]:checked::after {
                        content: "";
                        position: absolute;
                        width: 8px;
                        height: 8px;
                        background: var(--primary-blue);
                        border-radius: 50%;
                        left: 50%;
                        top: 50%;
                        transform: translate(-50%, -50%);
                    }
                    .action-input {
                        margin-bottom: 16px;
                    }
                    .action-input input {
                        width: calc(100% - 24px);
                        padding: 5px 12px;
                        font-size: 14px;
                        line-height: 20px;
                        border: 1px solid var(--border-color);
                        border-radius: 6px;
                        background-color: white;
                    }
                    .action-input input:focus {
                        outline: none;
                        border-color: var(--primary-blue);
                        box-shadow: 0 0 0 3px rgba(9, 105, 218, 0.3);
                    }
                    .primary-action-button {
                        width: 100%;
                        padding: 5px 16px;
                        font-size: 14px;
                        font-weight: 600;
                        line-height: 20px;
                        background-color: var(--hubspot-orange);
                        color: white;
                        border: none;
                        border-radius: 6px;
                        cursor: pointer;
                        transition: all 300ms ease-in-out;
                    }
                    .primary-action-button:hover:not(:disabled) {
                        background-color: #f66d48;
                    }
                    .primary-action-button:disabled {
                        background-color: var(--disabled-color);
                        color: #9ca3af;
                        cursor: not-allowed;
                    }
                    .primary-action-button.danger {
                        background-color: var(--danger-color);
                        color: white;
                    }
                    .primary-action-button.danger:hover:not(:disabled) {
                        background-color: #e03e45;
                    }
                    .preview-container {
                        margin-top: 8px;
                    }
                    .preview-content {
                        font-size: 14px;
                    }
                    .preview-arrow {
                        margin: 0 8px;
                        color: var(--primary-blue);
                    }
                    "#
                    (header::styles())
                    r#"
                    .content {
                        padding: 24px;
                    }
                    "#
                }
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
            body {
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
                                    form action="/deploy" method="get" {
                                        input type="hidden" name="selected" value=(format!("{}/{}", selected_config.namespace().unwrap_or_default(), selected_config.name_any()));

                                        div class="action-radio-group" {
                                            h4 { "Action" }
                                            @let current_action = query.get("action");
                                            label class="action-radio" {
                                                input type="radio" name="action" value="deploy-latest" checked[current_action.is_none() || current_action.unwrap() == "deploy-latest"] onchange="this.form.submit()";
                                                "Deploy latest"
                                            }
                                            label class="action-radio" {
                                                input type="radio" name="action" value="track-branch" checked[current_action.map_or(false, |a| a == "track-branch")] onchange="this.form.submit()";
                                                "Deploy and switch branch"
                                            }
                                            label class="action-radio" {
                                                input type="radio" name="action" value="specific-commit" checked[current_action.map_or(false, |a| a == "specific-commit")] onchange="this.form.submit()";
                                                "Deploy specific commit"
                                            }
                                            label class="action-radio" {
                                                input type="radio" name="action" value="toggle-autodeploy" checked[current_action.map_or(false, |a| a == "toggle-autodeploy")] onchange="this.form.submit()";
                                                @if selected_config.current_autodeploy() {
                                                    "Disable autodeploy"
                                                } @else {
                                                    "Enable autodeploy"
                                                }
                                            }
                                            label class="action-radio" {
                                                input type="radio" name="action" value="undeploy" checked[current_action.map_or(false, |a| a == "undeploy")] onchange="this.form.submit()";
                                                "Undeploy"
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
                                                        "Deploy"
                                                    }
                                                }
                                            }
                                            "track-branch" => {
                                                form action=(format!("/api/override-branch/{}/{}",
                                                    selected_config.namespace().unwrap_or_default(),
                                                    selected_config.name_any()))
                                                    method="post" {
                                                    div class="action-input" {
                                                        input type="text" name="branch" placeholder="Enter branch name" required value=(query.get("branch").unwrap_or(&"".to_string()));
                                                    }
                                                    button type="submit" class="primary-action-button" {
                                                        "Deploy"
                                                    }
                                                }
                                            }
                                            "specific-commit" => {
                                                form action=(format!("/api/deploy-specific/{}/{}",
                                                    selected_config.namespace().unwrap_or_default(),
                                                    selected_config.name_any()))
                                                    method="post" {
                                                    div class="action-input" {
                                                        input type="text" name="sha" placeholder="Enter commit SHA" required pattern="[0-9a-fA-F]{5,40}" value=(query.get("sha").unwrap_or(&"".to_string()));
                                                    }
                                                    button type="submit" class="primary-action-button" {
                                                        "Deploy"
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
                                                            "Disable autodeploy"
                                                        } @else {
                                                            "Enable autodeploy"
                                                        }
                                                    }
                                                }
                                            }
                                            "undeploy" => {
                                                form data-test="hello" action=(format!("/api/undeploy/{}/{}",
                                                    selected_config.namespace().unwrap_or_default(),
                                                    selected_config.name_any()))
                                                    data-action="undeploy"
                                                    method="post" {
                                                    button type="submit" class="primary-action-button danger" {
                                                        "Undeploy"
                                                    }
                                                }
                                            }
                                            _ => {}
                                                }
                                            } @else {
                                        form action=(format!("/api/deploy/{}/{}",
                                            selected_config.namespace().unwrap_or_default(),
                                            selected_config.name_any()))
                                            method="post" {
                                            button type="submit" class="primary-action-button" {
                                                "Deploy"
                                            }
                                        }
                                    }
                                }
                            }

                            // Right side box with preview
                            @if let Some(selected_config) = selected_config {
                                div class="right-box" {
                                    h1 {
                                        @match query.get("action").unwrap_or(&"".to_string()).as_str() {
                                            "track-branch" => {
                                                "Branch deploy of "
                                            }
                                            "toggle-autodeploy" => {
                                                "Option change for "
                                            }
                                            "undeploy" => {
                                                "Undeploy of "
                                            }
                                            "deploy-latest" | "specific-commit" | _ => {
                                                "Deploy of "
                                            }
                                        }
                                        strong {
                                            (format!("{}/{}", selected_config.namespace().unwrap_or_default(), selected_config.name_any()))
                                        }
                                    }
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
                                                    _ => {}
                                                }
                                            } @else {
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
                            format!("/deploy?selected={}/{}", namespace, name),
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
                            format!("/deploy?selected={}/{}", namespace, name),
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
                            format!("/deploy?selected={}/{}", namespace, name),
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
                            format!("/deploy?selected={}/{}", namespace, name),
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
                            format!("/deploy?selected={}/{}", namespace, name),
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
