use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};
use tempfile::tempdir;

fn mem_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mem")
}

#[test]
fn cli_full_workflow_and_doctor() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("cli_test.db");
    let db_str = db_path.to_string_lossy().to_string();

    let run = |args: &[&str]| -> Value {
        let mut full = vec!["--db", &db_str, "--provider", "disabled", "--json"];
        full.extend_from_slice(args);
        let out = Command::new(mem_bin())
            .args(&full)
            .output()
            .expect("spawn mem binary");
        assert!(
            out.status.success(),
            "mem {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("valid json output")
    };

    let init_res = run(&["init"]);
    assert_eq!(init_res["branch"], "main");
    let c0 = init_res["root_commit_id"].as_str().unwrap().to_string();

    run(&[
        "write",
        "/core/rules.md",
        "Never leak cross-branch memories.",
        "--importance",
        "1.0",
    ]);
    run(&[
        "write",
        "/semantic/sqlite-wal.md",
        "---\ntitle: SQLite WAL\naliases: [\"BUSY_TIMEOUT\"]\n---\n# WAL\nSee [[/procedural/lock-sop.md]].",
        "--importance",
        "0.9",
    ]);
    run(&[
        "write",
        "/procedural/lock-sop.md",
        "# Lock SOP\nCheck long transactions.",
        "--importance",
        "0.8",
    ]);

    let c1 = run(&["commit", "-m", "initial wiki"])["commit_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(c1, c0);

    let search_hits = run(&["search", "BUSY_TIMEOUT"])
        .as_array()
        .unwrap()
        .clone();
    assert!(search_hits
        .iter()
        .any(|h| h["path"] == "/semantic/sqlite-wal.md"));
    assert!(search_hits
        .iter()
        .any(|h| h["path"] == "/procedural/lock-sop.md"));

    let compiled = run(&["compile", "--max-tokens", "500"]);
    assert!(compiled["rendered"]
        .as_str()
        .unwrap()
        .contains("Never leak cross-branch memories."));

    let doctor = run(&["doctor"]);
    assert_eq!(doctor["sqlite_ok"], true);
    assert_eq!(doctor["fts5_ok"], true);
    assert_eq!(doctor["wiki_graph_ok"], true);

    run(&["rewind", &c0]);
    let after_rewind = run(&["ls"]).as_array().unwrap().clone();
    assert!(after_rewind.is_empty());
}

#[test]
fn mcp_stdio_server_initialize_list_and_call_tools() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("mcp_test.db");

    let mut child = Command::new(mem_bin())
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--provider",
            "disabled",
            "mcp",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mem mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    fn send_and_read(
        stdin: &mut std::process::ChildStdin,
        reader: &mut BufReader<std::process::ChildStdout>,
        msg: Value,
    ) -> Value {
        writeln!(stdin, "{}", serde_json::to_string(&msg).unwrap()).unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("valid jsonrpc response")
    }

    let init_resp = send_and_read(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.1" }
            }
        }),
    );
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "ai-memory");

    // Notification expects no response.
    writeln!(
        stdin,
        "{}",
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .unwrap();
    stdin.flush().unwrap();

    let tools_resp = send_and_read(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );
    let tools = tools_resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 10);

    let write_resp = send_and_read(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "mem_write",
                "arguments": {
                    "path": "/semantic/mcp_note.md",
                    "content": "---\naliases: [\"MCP_ALIAS\"]\n---\n# MCP Note\nStored via MCP stdio.",
                    "importance": 0.9
                }
            }
        }),
    );
    assert_eq!(write_resp["result"]["isError"], false);

    let search_resp = send_and_read(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "mem_search",
                "arguments": { "query": "MCP_ALIAS" }
            }
        }),
    );
    let text = search_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("/semantic/mcp_note.md"));

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success());
}
