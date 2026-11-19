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
    assert_eq!(tools.len(), 11);

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

    // Commit main so draft/pm-inbox branched from main HEAD inherits mcp_note.md.
    let commit_resp = send_and_read(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "mem_commit",
                "arguments": { "message": "commit mcp note" }
            }
        }),
    );
    assert_eq!(commit_resp["result"]["isError"], false);

    let ingest_resp = send_and_read(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "mem_ingest",
                "arguments": {
                    "role": "pm",
                    "draft": true,
                    "title": "MCP Draft Spec",
                    "text": "Business rule referencing MCP_ALIAS."
                }
            }
        }),
    );
    assert_eq!(ingest_resp["result"]["isError"], false);
    let ingest_payload: Value =
        serde_json::from_str(ingest_resp["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap();
    assert_eq!(ingest_payload["branch"], "draft/pm-inbox");
    assert_eq!(
        ingest_payload["path"],
        "/semantic/product/mcp-draft-spec.md"
    );
    assert!(ingest_payload["linked_notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "MCP Note"));

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn cli_onboard_config_switch_and_mcp_auto_wiring() {
    let dir = tempdir().unwrap();
    let home_dir = dir.path().join("fake_home");
    let mem_home = home_dir.join(".ai-memory");
    let cursor_dir = home_dir.join(".cursor");
    std::fs::create_dir_all(&cursor_dir).unwrap();

    // Pre-populate an existing Cursor MCP config to verify non-destructive merge.
    let cursor_mcp_path = cursor_dir.join("mcp.json");
    std::fs::write(
        &cursor_mcp_path,
        serde_json::to_string_pretty(&json!({
            "mcpServers": {
                "other-tool": {
                    "command": "npx",
                    "args": ["other-server"]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let run_isolated = |args: &[&str]| -> Value {
        let out = Command::new(mem_bin())
            .env("HOME", &home_dir)
            .env("MEM_HOME", &mem_home)
            .env_remove("MEM_DB_PATH")
            .env_remove("MEM_PROVIDER")
            .env_remove("MEM_CONFIG_PATH")
            .env_remove("OPENAI_API_KEY")
            .args(args)
            .output()
            .expect("spawn mem");
        assert!(
            out.status.success(),
            "mem {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("valid json output")
    };

    // 1. Run `mem onboard --mode lite --seed-demo --yes --json`.
    let onboard_res = run_isolated(&["onboard", "--mode", "lite", "--seed-demo", "--yes", "--json"]);
    assert_eq!(onboard_res["mode"], "lite");
    assert_eq!(onboard_res["seeded_notes"], 4);
    assert!(mem_home.join("config.json").exists());
    assert!(mem_home.join("memory.db").exists());

    // 2. Verify Cursor MCP config kept "other-tool" and merged "ai-memory".
    let cursor_json: Value =
        serde_json::from_str(&std::fs::read_to_string(&cursor_mcp_path).unwrap()).unwrap();
    assert_eq!(cursor_json["mcpServers"]["other-tool"]["command"], "npx");
    assert!(cursor_json["mcpServers"]["ai-memory"]["command"]
        .as_str()
        .is_some());

    // 3. Search WITHOUT passing --db or --provider flags (uses config.json automatically!).
    let hits = run_isolated(&["search", "BUSY_TIMEOUT", "--json"])
        .as_array()
        .unwrap()
        .clone();
    assert!(hits
        .iter()
        .any(|h| h["path"] == "/semantic/sqlite-wal.md"));
    assert!(hits
        .iter()
        .any(|h| h["path"] == "/procedural/sqlite-lock-sop.md"));

    // 4. Hot-switch to production OpenAI profile via `mem config use openai`.
    let switch_prod = run_isolated(&[
        "config",
        "use",
        "openai",
        "--api-key",
        "sk-prod-999",
        "--json",
    ]);
    assert_eq!(switch_prod["active_mode"], "openai");

    let shown_prod = run_isolated(&["config", "show", "--json"]);
    assert_eq!(shown_prod["embedding"]["backend"], "openai");
    assert_eq!(shown_prod["embedding"]["model"], "text-embedding-3-small");
    assert_eq!(shown_prod["consolidate_llm"]["model"], "gpt-6-luna");

    // 5. Hot-switch back to local Ollama profile.
    let switch_dev = run_isolated(&["config", "use", "ollama", "--json"]);
    assert_eq!(switch_dev["active_mode"], "ollama");

    let shown_dev = run_isolated(&["config", "show", "--json"]);
    assert_eq!(shown_dev["embedding"]["backend"], "ollama");
    assert_eq!(shown_dev["embedding"]["model"], "bge-m3");
    assert_eq!(shown_dev["consolidate_llm"]["model"], "qwen2.5:7b");
}

#[test]
fn cli_collaborative_ingest_and_vault_sync_export() {
    let dir = tempdir().unwrap();
    let home_dir = dir.path().join("fake_home");
    let mem_home = home_dir.join(".ai-memory");
    let db_path = dir.path().join("collab_cli.db");
    let vault_dir = dir.path().join("team_vault");
    let export_dir = dir.path().join("exported_vault");

    let run = |args: &[&str]| -> std::process::Output {
        Command::new(mem_bin())
            .env("HOME", &home_dir)
            .env("MEM_HOME", &mem_home)
            .args(["--db", &db_path.to_string_lossy(), "--provider", "disabled"])
            .args(args)
            .output()
            .expect("spawn mem binary")
    };

    let init_out = run(&["init", "--json"]);
    assert!(
        init_out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init_out.stderr)
    );

    let tpl_out = run(&[
        "vault",
        "init-template",
        &vault_dir.to_string_lossy(),
        "--json",
    ]);
    assert!(tpl_out.status.success());
    let tpl_json: Value = serde_json::from_slice(&tpl_out.stdout).unwrap();
    assert_eq!(tpl_json["files_created"], 6);

    std::fs::write(
        vault_dir.join("semantic/architecture/idempotency.md"),
        "---\ntitle: \"Idempotency Key\"\ntags: [\"role/dev\", \"api\"]\n---\n\n# Idempotency Key\n\nAll write APIs require an X-Idempotency-Key header.\n",
    )
    .unwrap();

    let sync_out = run(&[
        "vault",
        "sync",
        &vault_dir.to_string_lossy(),
        "--commit",
        "--json",
    ]);
    assert!(
        sync_out.status.success(),
        "sync failed: {}",
        String::from_utf8_lossy(&sync_out.stderr)
    );
    let sync_json: Value = serde_json::from_slice(&sync_out.stdout).unwrap();
    assert_eq!(sync_json["stats"]["files_imported"], 2);

    let ingest_out = run(&[
        "ingest",
        "--role",
        "pm",
        "--draft",
        "--title",
        "Payment Retry Rule",
        "--json",
        "All retries obey Idempotency Key.",
    ]);
    assert!(
        ingest_out.status.success(),
        "ingest failed: {}",
        String::from_utf8_lossy(&ingest_out.stderr)
    );
    let ingest_json: Value = serde_json::from_slice(&ingest_out.stdout).unwrap();
    assert_eq!(ingest_json["branch"], "draft/pm-inbox");
    assert_eq!(ingest_json["path"], "/semantic/product/payment-retry-rule.md");
    assert!(ingest_json["linked_notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "Idempotency Key"));

    // Must not exist on main before merge.
    let read_main = run(&["read", "/semantic/product/payment-retry-rule.md"]);
    assert!(!read_main.status.success());

    let merge_out = run(&["merge", "draft/pm-inbox", "main", "--json"]);
    assert!(merge_out.status.success());

    let read_after_merge = run(&["read", "/semantic/product/payment-retry-rule.md"]);
    assert!(read_after_merge.status.success());

    let export_out = run(&["vault", "export", &export_dir.to_string_lossy(), "--json"]);
    assert!(export_out.status.success());
    let export_json: Value = serde_json::from_slice(&export_out.stdout).unwrap();
    assert_eq!(export_json["files_exported"], 3);
    assert!(export_dir.join("core/system.md").exists());
    assert!(export_dir
        .join("semantic/architecture/idempotency.md")
        .exists());
    assert!(export_dir
        .join("semantic/product/payment-retry-rule.md")
        .exists());
}
