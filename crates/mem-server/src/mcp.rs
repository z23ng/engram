use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use mem_mcp::{handle_jsonrpc_request, mcp_tool_definitions};
use serde_json::{json, Value};

use crate::http::guarded;
use crate::{Action, ControlPlane, ServerError};

/// Permission class of an MCP tool; `None` for unknown tools.
pub(crate) fn tool_action(name: &str) -> Option<Action> {
    match name {
        "mem_read" | "mem_list" | "mem_search" | "mem_compile_core" | "mem_diff" => {
            Some(Action::Read)
        }
        "mem_write" | "mem_commit" | "mem_ingest" => Some(Action::Write),
        "mem_branch" | "mem_rewind" | "mem_merge" => Some(Action::Branch),
        _ => None,
    }
}

/// MCP Streamable HTTP endpoint: stateless, one JSON-RPC message per POST.
pub(crate) async fn mcp(
    State(cp): State<Arc<ControlPlane>>,
    Path(project): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ServerError> {
    let scope = project.clone();
    guarded(cp, headers, scope, Action::Read, move |cp, key, engine| {
        let Ok(mut req) = serde_json::from_slice::<Value>(&body) else {
            let err = json!({"jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": "parse error"}});
            return Ok((StatusCode::BAD_REQUEST, Json(err)).into_response());
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req["method"].as_str().unwrap_or_default().to_string();
        let allowed = |name: &str| tool_action(name).is_some_and(|a| key.role.allows(a));
        if method == "tools/list" {
            let tools: Vec<Value> = mcp_tool_definitions()
                .into_iter()
                .filter(|t| allowed(t["name"].as_str().unwrap_or_default()))
                .collect();
            let resp = json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools}});
            return Ok(Json(resp).into_response());
        }
        let tool = req["params"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let action = if method == "tools/call" {
            tool_action(&tool)
        } else {
            None
        };
        if action.is_some() && !allowed(&tool) {
            let msg = format!(
                "forbidden: role '{}' may not call {tool}",
                key.role.as_str()
            );
            let resp = json!({"jsonrpc": "2.0", "id": id,
                "result": {"content": [{"type": "text", "text": msg}], "isError": true}});
            return Ok(Json(resp).into_response());
        }
        if matches!(tool.as_str(), "mem_commit" | "mem_ingest") {
            set_author(&mut req, &key.name);
        }
        let Some(resp) = handle_jsonrpc_request(engine.vcs(), &engine, cp.llm_config(), &req)
        else {
            return Ok(StatusCode::ACCEPTED.into_response());
        };
        if action.is_some_and(|a| a != Action::Read) && resp["result"]["isError"] == false {
            let text = resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default();
            let parsed = serde_json::from_str::<Value>(text).unwrap_or_default();
            cp.record_audit(key, &project, &tool, parsed["commit_id"].as_str())?;
        }
        Ok(Json(resp).into_response())
    })
    .await
}

/// Replaces `params.arguments.author` so clients cannot choose the commit author.
fn set_author(req: &mut Value, name: &str) {
    let Some(params) = req.get_mut("params").and_then(Value::as_object_mut) else {
        return;
    };
    let args = params.entry("arguments").or_insert_with(|| json!({}));
    if let Some(args) = args.as_object_mut() {
        args.insert("author".into(), json!(name));
    }
}
