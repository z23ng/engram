//! Line-delimited JSON-RPC 2.0 MCP server over stdio.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use mem_mcp::handle_jsonrpc_request;
use mem_search::{ConsolidateLlmConfig, EmbeddingProviderConfig};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};
use serde_json::{json, Value};

use crate::build_search_engine;

/// Runs a line-delimited JSON-RPC 2.0 MCP server reading from `stdin` and writing to `stdout`.
pub fn run_stdio_server(
    db_path: PathBuf,
    emb_cfg: EmbeddingProviderConfig,
    llm_cfg: ConsolidateLlmConfig,
) -> io::Result<()> {
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let store = SqliteStore::open(&db_path)
        .map_err(|e| io::Error::other(e.to_string()))?;
    let vcs = MemVcs::new(store);
    vcs.init()
        .map_err(|e| io::Error::other(e.to_string()))?;

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

        if let Some(resp) = handle_jsonrpc_request(&vcs, &engine, &llm_cfg, &req) {
            write_json_line(&mut out, &resp)?;
        }
    }

    Ok(())
}

fn write_json_line<W: Write>(out: &mut W, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value)
        .map_err(|e| io::Error::other(e.to_string()))?;
    out.write_all(b"\n")?;
    out.flush()
}

