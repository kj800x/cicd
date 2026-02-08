use actix_web::{web, HttpResponse};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::crab_ext::Octocrabs;

use super::protocol::{InitializeResult, JsonRpcRequest, JsonRpcResponse};
use super::tools;

pub async fn handle_mcp(
    body: web::Json<JsonRpcRequest>,
    client: web::Data<kube::Client>,
    pool: web::Data<Pool<SqliteConnectionManager>>,
    octocrabs: web::Data<Octocrabs>,
) -> HttpResponse {
    let request = body.into_inner();

    // Notifications (no id) don't get responses
    if request.id.is_none() {
        return HttpResponse::Accepted().finish();
    }

    let is_initialize = request.method == "initialize";

    let response = match request.method.as_str() {
        "initialize" => handle_initialize(request.id),
        "ping" => JsonRpcResponse::success(request.id, json!({})),
        "tools/list" => handle_tools_list(request.id),
        "tools/call" => {
            handle_tools_call(request.id, request.params, &client, &pool, &octocrabs).await
        }
        _ => JsonRpcResponse::method_not_found(request.id),
    };

    let mut http_response = HttpResponse::Ok();
    http_response.content_type("application/json");

    if is_initialize {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let session_id = format!("{:x}", md5::compute(nanos.to_le_bytes()));
        http_response.insert_header(("Mcp-Session-Id", session_id));
    }

    http_response.json(response)
}

fn handle_initialize(id: Option<serde_json::Value>) -> JsonRpcResponse {
    let result = InitializeResult {
        protocol_version: "2025-03-26".to_string(),
        capabilities: super::protocol::Capabilities {
            tools: super::protocol::ToolsCapability {},
        },
        server_info: super::protocol::ServerInfo {
            name: "cicd-mcp".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    };

    JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default())
}

fn handle_tools_list(id: Option<serde_json::Value>) -> JsonRpcResponse {
    let tool_defs = tools::tool_definitions();
    let result = json!({ "tools": tool_defs });
    JsonRpcResponse::success(id, result)
}

async fn handle_tools_call(
    id: Option<serde_json::Value>,
    params: Option<serde_json::Value>,
    client: &kube::Client,
    pool: &Pool<SqliteConnectionManager>,
    octocrabs: &Octocrabs,
) -> JsonRpcResponse {
    let params = match params {
        Some(p) => p,
        None => {
            return JsonRpcResponse::invalid_params(id, "Missing params".to_string());
        }
    };

    let tool_name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return JsonRpcResponse::invalid_params(id, "Missing tool name".to_string());
        }
    };

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    let result = tools::dispatch(&tool_name, arguments, client, pool, octocrabs).await;

    JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default())
}
