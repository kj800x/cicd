use crate::prelude::*;
use kube::{api::Api, client::Client, config, ResourceExt};

/// Handler for the deploy configs page
pub async fn deploy_configs(
    pool: web::Data<Pool<SqliteConnectionManager>>,
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
                                        div class="detail-label" { "Branch:" }
                                        div class="detail-value" { (selected_config.spec.spec.repo.branch) }
                                    }

                                    div class="detail-row" {
                                        div class="detail-label" { "Auto-deploy:" }
                                        div class="detail-value" {
                                            @if selected_config.spec.spec.autodeploy {
                                                span class="boolean-value boolean-true" { "Enabled" }
                                            } @else {
                                                span class="boolean-value boolean-false" { "Disabled" }
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
                                }

                                h4 { "Actions" }
                                p { "Actions functionality will be added in a future update." }
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
