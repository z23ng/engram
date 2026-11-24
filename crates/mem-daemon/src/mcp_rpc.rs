//! Protocol-agnostic JSON-RPC 2.0 Model Context Protocol (MCP) dispatcher (12 tools).
//!
//! Delegates the 11 core VCS/search/ingest tools and JSON-RPC framing to [`mem_mcp`]
//! while exposing the cold-path `mem_consolidate` tool for daemon & stdio sessions.

use mem_consolidate::{ConsolidationEngine, ConsolidationRequest};
use mem_core::{MergeStrategy, VirtualPath};
use mem_search::{ConsolidateLlmConfig, MemSearchEngine};
use mem_vcs::MemVcs;
use serde_json::{json, Value};

/// Processes a single JSON-RPC 2.0 MCP request and returns `None` for notifications.
pub fn handle_jsonrpc_request(
    vcs: &MemVcs,
    engine: &MemSearchEngine,
    llm_cfg: &ConsolidateLlmConfig,
    req: &Value,
) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    if method == "tools/list" {
        let resp_id = id.unwrap_or(Value::Null);
        return Some(json!({
            "jsonrpc": "2.0",
            "id": resp_id,
            "result": {
                "tools": mcp_tool_definitions()
            }
        }));
    }

    if method == "tools/call" {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        if name == "mem_consolidate" {
            let resp_id = id.unwrap_or(Value::Null);
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            return Some(match dispatch_consolidate(engine, llm_cfg, &args) {
                Ok(text) => json!({
                    "jsonrpc": "2.0",
                    "id": resp_id,
                    "result": {
                        "content": [{ "type": "text", "text": text }],
                        "isError": false
                    }
                }),
                Err(err_msg) => json!({
                    "jsonrpc": "2.0",
                    "id": resp_id,
                    "result": {
                        "content": [{ "type": "text", "text": err_msg }],
                        "isError": true
                    }
                }),
            });
        }
    }

    mem_mcp::handle_jsonrpc_request(vcs, engine, llm_cfg, req)
}

fn dispatch_consolidate(
    engine: &MemSearchEngine,
    llm_cfg: &ConsolidateLlmConfig,
    args: &Value,
) -> Result<String, String> {
    let branch = args.get("branch").and_then(Value::as_str).unwrap_or("main");
    let mut req = ConsolidationRequest::new(branch);
    if let Some(cb) = args.get("consolidation_branch").and_then(Value::as_str) {
        req = req.with_consolidation_branch(cb);
    }
    if let Some(prefix) = args.get("prefix").and_then(Value::as_str) {
        let vp = VirtualPath::new(prefix).map_err(|e| e.to_string())?;
        req = req.with_episodic_prefix(vp);
    }
    if let Some(max_notes) = args.get("max_notes").and_then(Value::as_u64) {
        req = req.with_max_notes(max_notes as usize);
    }
    if let Some(min_imp) = args.get("min_importance").and_then(Value::as_f64) {
        req = req.with_min_importance(min_imp as f32);
    }
    if let Some(use_llm) = args.get("use_llm").and_then(Value::as_bool) {
        req = req.with_use_llm(use_llm);
    }
    if let Some(auto_merge) = args.get("auto_merge").and_then(Value::as_bool) {
        req = req.with_auto_merge(auto_merge);
    }
    if let Some(strategy) = args.get("strategy").and_then(Value::as_str) {
        let parsed = match strategy {
            "ours" => MergeStrategy::Ours,
            "theirs" => MergeStrategy::Theirs,
            _ => MergeStrategy::Auto,
        };
        req = req.with_merge_strategy(parsed);
    }
    if let Some(archive) = args.get("archive_processed").and_then(Value::as_bool) {
        req = req.with_archive_processed(archive);
    }
    if let Some(del) = args.get("delete_branch").and_then(Value::as_bool) {
        req = req.with_delete_branch_on_merge(del);
    }
    if let Some(dry) = args.get("dry_run").and_then(Value::as_bool) {
        req = req.with_dry_run(dry);
    }

    let consolidator = ConsolidationEngine::new(engine.clone()).with_llm_config(llm_cfg.clone());
    let report = consolidator.consolidate(&req).map_err(|e| e.to_string())?;
    Ok(serde_json::to_string_pretty(&report).unwrap_or_default())
}

/// Returns the JSON schema definitions for all 12 Engram MCP tools.
pub fn mcp_tool_definitions() -> Vec<Value> {
    let mut defs = mem_mcp::mcp_tool_definitions();
    defs.push(json!({
        "name": "mem_consolidate",
        "description": "Run cold-path sleep consolidation on an isolated consolidate/* branch: scan /episodic/ logs, deduplicate and distill durable /semantic/ cards and /procedural/ SOPs, weave [[Wikilinks]], and optionally merge back to the source branch.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "branch": { "type": "string", "description": "Source branch to scan (default: main)" },
                "consolidation_branch": { "type": "string", "description": "Optional explicit isolated branch name" },
                "prefix": { "type": "string", "description": "Episodic prefix to scan (default: /episodic)" },
                "max_notes": { "type": "integer", "minimum": 1, "maximum": 500 },
                "min_importance": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                "use_llm": { "type": "boolean", "description": "Use configured OpenAI/Ollama LLM before deterministic fallback (default: false)" },
                "auto_merge": { "type": "boolean", "description": "Auto-merge isolated consolidate/* branch into source branch (default: false)" },
                "strategy": { "type": "string", "enum": ["auto", "ours", "theirs"] },
                "archive_processed": { "type": "boolean", "description": "Write archival summary and decay processed episodic importance" },
                "delete_branch": { "type": "boolean", "description": "Delete consolidate/* branch after a clean auto-merge" },
                "dry_run": { "type": "boolean", "description": "Preview distilled cards without creating branches or commits" }
            }
        }
    }));
    defs
}
