//! Standard JSON-RPC 2.0 Model Context Protocol (MCP) server over stdio.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use mem_core::{
    CommitId, DiffKind, MemoryMetadata, MemoryNode, MergeOutcome, MergeStrategy, Revision,
    VirtualPath,
};
use mem_search::{EmbeddingProviderConfig, MemSearchEngine, SearchRequest};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};
use serde_json::{json, Value};

use crate::build_search_engine;

/// Runs a line-delimited JSON-RPC 2.0 MCP server reading from `stdin` and writing to `stdout`.
pub fn run_stdio_server(db_path: PathBuf, emb_cfg: EmbeddingProviderConfig) -> io::Result<()> {
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let store = SqliteStore::open(&db_path)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    let vcs = MemVcs::new(store);
    vcs.init()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    let (engine, notice) = build_search_engine(vcs.clone(), &emb_cfg);
    if let Some(msg) = notice {
        eprintln!("[mem-mcp] {msg}");
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line_res in stdin.lock().lines() {
        let line = line_res?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(err) => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {
                        "code": -32700,
                        "message": format!("parse error: {err}")
                    }
                });
                write_json_line(&mut out, &resp)?;
                continue;
            }
        };

        if let Some(resp) = handle_jsonrpc_request(&vcs, &engine, &req) {
            write_json_line(&mut out, &resp)?;
        }
    }

    Ok(())
}

fn write_json_line<W: Write>(out: &mut W, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Processes a single JSON-RPC 2.0 MCP request and returns `None` for notifications.
pub fn handle_jsonrpc_request(
    vcs: &MemVcs,
    engine: &MemSearchEngine,
    req: &Value,
) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    // JSON-RPC notifications have no `id` and expect no reply.
    if id.is_none() && method.starts_with("notifications/") {
        return None;
    }

    let resp_id = id.unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let client_version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05");
            Some(json!({
                "jsonrpc": "2.0",
                "id": resp_id,
                "result": {
                    "protocolVersion": client_version,
                    "capabilities": {
                        "tools": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": "ai-memory",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }))
        }
        "notifications/initialized" => None,
        "ping" => Some(json!({
            "jsonrpc": "2.0",
            "id": resp_id,
            "result": {}
        })),
        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": resp_id,
            "result": {
                "tools": mcp_tool_definitions()
            }
        })),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(jsonrpc_error(resp_id, -32602, "missing tool 'name'"));
            };
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match dispatch_tool(vcs, engine, name, &args) {
                Ok(text) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": resp_id,
                    "result": {
                        "content": [{ "type": "text", "text": text }],
                        "isError": false
                    }
                })),
                Err(err_msg) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": resp_id,
                    "result": {
                        "content": [{ "type": "text", "text": err_msg }],
                        "isError": true
                    }
                })),
            }
        }
        other => Some(jsonrpc_error(
            resp_id,
            -32601,
            &format!("method not found: {other}"),
        )),
    }
}

fn jsonrpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn dispatch_tool(
    vcs: &MemVcs,
    engine: &MemSearchEngine,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    let branch = args
        .get("branch")
        .and_then(Value::as_str)
        .unwrap_or("main");

    match name {
        "mem_read" => {
            let raw_path = req_str(args, "path")?;
            let path = VirtualPath::new(raw_path).map_err(|e| e.to_string())?;
            let rev = match args.get("commit_id").and_then(Value::as_str) {
                Some(cid) => {
                    let parsed = CommitId::from_hex(cid).map_err(|e| e.to_string())?;
                    Revision::Commit(parsed)
                }
                None => Revision::BranchWorkspace(branch.to_string()),
            };
            let node = vcs.read(&rev, &path).map_err(|e| e.to_string())?;
            match node {
                Some(n) => {
                    let text = n.content_str().map_err(|e| e.to_string())?;
                    Ok(serde_json::to_string_pretty(&json!({
                        "path": n.path.as_str(),
                        "blob_id": n.blob_id.map(|b| b.to_hex()),
                        "metadata": n.metadata,
                        "content": text,
                    }))
                    .unwrap_or_default())
                }
                None => Ok(json!({ "found": false, "path": path.as_str() }).to_string()),
            }
        }
        "mem_write" => {
            let raw_path = req_str(args, "path")?;
            let content = req_str(args, "content")?;
            let path = VirtualPath::new(raw_path).map_err(|e| e.to_string())?;
            let mut meta = MemoryMetadata::default();
            if let Some(imp) = args.get("importance").and_then(Value::as_f64) {
                meta.importance = (imp as f32).clamp(0.0, 1.0);
            }
            if let Some(tags) = args.get("tags").and_then(Value::as_array) {
                meta.tags = tags
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect();
            }
            let node = MemoryNode::new(path.clone(), content.as_bytes().to_vec()).with_metadata(meta);
            let blob_id = vcs.write_node(branch, node).map_err(|e| e.to_string())?;
            engine
                .index_blob_if_needed(&path, &blob_id)
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "status": "staged",
                "branch": branch,
                "path": path.as_str(),
                "blob_id": blob_id.to_hex()
            })
            .to_string())
        }
        "mem_list" => {
            let prefix = args
                .get("prefix")
                .and_then(Value::as_str)
                .map(VirtualPath::new)
                .transpose()
                .map_err(|e| e.to_string())?;
            let entries = vcs
                .list(&Revision::BranchWorkspace(branch.to_string()), prefix.as_ref())
                .map_err(|e| e.to_string())?;
            let list: Vec<Value> = entries
                .into_iter()
                .map(|(p, b, m)| {
                    json!({
                        "path": p.as_str(),
                        "blob_id": b.to_hex(),
                        "importance": m.importance,
                        "tags": m.tags
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&list).unwrap_or_default())
        }
        "mem_search" => {
            let query = req_str(args, "query")?;
            let top_k = args
                .get("top_k")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(10);
            let prefix = args
                .get("prefix")
                .and_then(Value::as_str)
                .map(VirtualPath::new)
                .transpose()
                .map_err(|e| e.to_string())?;

            let mut req = SearchRequest::new(Revision::BranchWorkspace(branch.to_string()), query)
                .with_top_k(top_k);
            if let Some(p) = prefix {
                req = req.with_prefix(p);
            }
            let hits = engine.search(&req).map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&hits).unwrap_or_default())
        }
        "mem_compile_core" => {
            let max_tokens = args
                .get("max_tokens")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(2000);
            let compiled = engine
                .compile_core(&Revision::BranchWorkspace(branch.to_string()), max_tokens)
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_string_pretty(&compiled).unwrap_or_default())
        }
        "mem_commit" => {
            let message = req_str(args, "message")?;
            let author = args
                .get("author")
                .and_then(Value::as_str)
                .unwrap_or("mcp-agent");
            let cid = vcs
                .commit(branch, author, message)
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "branch": branch,
                "commit_id": cid.to_hex()
            })
            .to_string())
        }
        "mem_branch" => {
            let name = req_str(args, "name")?;
            let from_rev = match args.get("from").and_then(Value::as_str) {
                Some(raw) => parse_revision(raw),
                None => Revision::BranchHead("main".to_string()),
            };
            vcs.create_branch(name, &from_rev)
                .map_err(|e| e.to_string())?;
            Ok(json!({ "created": name }).to_string())
        }
        "mem_rewind" => {
            let cid_raw = req_str(args, "commit_id")?;
            let cid = CommitId::from_hex(cid_raw).map_err(|e| e.to_string())?;
            vcs.rewind(branch, &cid).map_err(|e| e.to_string())?;
            Ok(json!({ "rewound": branch, "commit_id": cid.to_hex() }).to_string())
        }
        "mem_diff" => {
            let base = parse_revision(req_str(args, "base")?);
            let target = parse_revision(req_str(args, "target")?);
            let diffs = vcs.diff(&base, &target).map_err(|e| e.to_string())?;
            let formatted: Vec<Value> = diffs
                .into_iter()
                .map(|d| {
                    let kind = match d.kind {
                        DiffKind::Added => "added",
                        DiffKind::Modified => "modified",
                        DiffKind::Deleted => "deleted",
                    };
                    json!({ "path": d.path.as_str(), "kind": kind })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&formatted).unwrap_or_default())
        }
        "mem_merge" => {
            let source = req_str(args, "source")?;
            let target = req_str(args, "target")?;
            let strategy = match args.get("strategy").and_then(Value::as_str) {
                Some("ours") => MergeStrategy::Ours,
                Some("theirs") => MergeStrategy::Theirs,
                _ => MergeStrategy::Auto,
            };
            let outcome = vcs
                .merge(source, target, strategy)
                .map_err(|e| e.to_string())?;
            let payload = match outcome {
                MergeOutcome::FastForward(cid) => {
                    json!({ "outcome": "fast_forward", "commit_id": cid.to_hex() })
                }
                MergeOutcome::Merged(cid) => {
                    json!({ "outcome": "merged", "commit_id": cid.to_hex() })
                }
                MergeOutcome::Conflict {
                    base_commit,
                    conflicts,
                } => json!({
                    "outcome": "conflict",
                    "base_commit": base_commit.to_hex(),
                    "conflicts": conflicts
                }),
            };
            Ok(serde_json::to_string_pretty(&payload).unwrap_or_default())
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required string argument '{key}'"))
}

fn parse_revision(raw: &str) -> Revision {
    if let Ok(cid) = CommitId::from_hex(raw) {
        Revision::Commit(cid)
    } else {
        Revision::BranchWorkspace(raw.to_string())
    }
}

fn mcp_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "mem_read",
            "description": "Read a versioned memory node from a branch workspace or specific commit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Virtual path e.g. /core/rules.md" },
                    "branch": { "type": "string", "description": "Branch name (default: main)" },
                    "commit_id": { "type": "string", "description": "Optional commit hash override" }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "mem_write",
            "description": "Write or update a Markdown memory node in a branch workspace and update Wiki/FTS5 index.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Virtual path under /core, /episodic, /semantic, or /procedural" },
                    "content": { "type": "string", "description": "Markdown content (supports YAML frontmatter and [[Wikilinks]])" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "importance": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                    "branch": { "type": "string" }
                },
                "required": ["path", "content"]
            }
        }),
        json!({
            "name": "mem_list",
            "description": "List active memory paths in a branch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prefix": { "type": "string" },
                    "branch": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "mem_search",
            "description": "Branch-isolated Obsidian Wiki-Graph + BM25 + Vector hybrid search with 1-hop [[Wikilink]] outlink/backlink expansion.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "prefix": { "type": "string" },
                    "branch": { "type": "string" },
                    "top_k": { "type": "integer", "minimum": 1, "maximum": 50 }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "mem_compile_core",
            "description": "Compile pinned /core memories into a deterministic prompt block within max_tokens (<5ms).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "branch": { "type": "string" },
                    "max_tokens": { "type": "integer", "minimum": 50 }
                }
            }
        }),
        json!({
            "name": "mem_commit",
            "description": "Commit staged workspace modifications into an immutable Merkle Tree snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": { "type": "string" },
                    "branch": { "type": "string" },
                    "author": { "type": "string" }
                },
                "required": ["message"]
            }
        }),
        json!({
            "name": "mem_branch",
            "description": "Create a zero-copy isolated branch from an existing branch or commit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "from": { "type": "string" }
                },
                "required": ["name"]
            }
        }),
        json!({
            "name": "mem_rewind",
            "description": "Atomically rewind a branch back to a healthy commit and discard uncommitted workspace changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commit_id": { "type": "string" },
                    "branch": { "type": "string" }
                },
                "required": ["commit_id"]
            }
        }),
        json!({
            "name": "mem_diff",
            "description": "Compare two branches or commits and return structured Added/Modified/Deleted differences.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "base": { "type": "string" },
                    "target": { "type": "string" }
                },
                "required": ["base", "target"]
            }
        }),
        json!({
            "name": "mem_merge",
            "description": "Three-way merge source branch into target branch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": { "type": "string" },
                    "target": { "type": "string" },
                    "strategy": { "type": "string", "enum": ["auto", "ours", "theirs"] }
                },
                "required": ["source", "target"]
            }
        })
    ]
}
