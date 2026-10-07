//! MCP over Streamable HTTP: JSON-RPC 2.0 POSTed to /mcp, answered with a
//! single JSON response (no server-initiated streams).

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::AppState;
use crate::oauth::{bearer, challenge};
use crate::tools::{self, Caller};

const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

fn reply(id: &Value, result: Value) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}
fn rpc_error(id: &Value, code: i64, message: &str) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        .into_response()
}

pub async fn post(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(claims) = bearer(&headers).and_then(|t| s.oauth.verify_access(&s, t)) else {
        return challenge(&s);
    };
    let caller = Caller {
        agent: s
            .platform_agents
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&claims.email)),
        sub: claims.sub,
        email: claims.email,
        name: claims.name,
    };
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return rpc_error(&Value::Null, -32700, "parse error");
    };
    if msg.is_array() {
        return rpc_error(&Value::Null, -32600, "batches are not supported");
    }
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg["method"].as_str().unwrap_or_default();
    // Notifications and responses: accepted, no body.
    if msg.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    match method {
        "initialize" => {
            let asked = msg["params"]["protocolVersion"]
                .as_str()
                .unwrap_or_default();
            let version = if VERSIONS.contains(&asked) {
                asked
            } else {
                VERSIONS[0]
            };
            reply(
                &id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "traum-haft", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": format!("You build small internal apps on traum-haft. Read https://{}/llms.txt before your first change.", s.apps_domain),
                }),
            )
        }
        "ping" => reply(&id, json!({})),
        "tools/list" => reply(&id, json!({ "tools": tools::definitions() })),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or_default();
            let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
            let (text, is_error) = match tools::call(&s, &caller, name, &args).await {
                Ok(t) => (t, false),
                Err(e) => (e, true),
            };
            reply(
                &id,
                json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }),
            )
        }
        _ => rpc_error(&id, -32601, "method not found"),
    }
}

/// No server-initiated stream.
pub async fn get() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}
