use std::net::SocketAddr;
use std::time::Duration;

use mem_daemon::{serve_on_listener, DaemonConfig, DaemonState};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

struct HttpResp {
    status: u16,
    body: String,
}

async fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    json_body: Option<Value>,
) -> HttpResp {
    let mut stream = TcpStream::connect(addr).await.expect("connect daemon");
    let body_str = json_body
        .map(|v| serde_json::to_string(&v).unwrap())
        .unwrap_or_default();

    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(tok) = token {
        req.push_str(&format!("Authorization: Bearer {tok}\r\n"));
    }
    if !body_str.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body_str.len()));
    }
    req.push_str("\r\n");
    req.push_str(&body_str);

    stream.write_all(req.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    HttpResp {
        status,
        body: body.to_string(),
    }
}

async fn spawn_test_daemon(auth_token: Option<&str>) -> (SocketAddr, oneshot::Sender<()>) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("daemon.db");
    // Keep dir alive for the lifetime of the test server task.
    let cfg = DaemonConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        db_path,
        auth_token: auth_token.map(String::from),
        ..DaemonConfig::default()
    };
    let state = DaemonState::from_config(&cfg).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let _keep_dir = dir;
        serve_on_listener(listener, state, async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });

    (addr, tx)
}

#[tokio::test]
async fn daemon_rest_lifecycle_branch_isolation_and_auth() {
    let (addr, shutdown) = spawn_test_daemon(Some("secret-token-42")).await;

    // Health works without token.
    let health = http_request(addr, "GET", "/health", None, None).await;
    assert_eq!(health.status, 200);
    let health_json: Value = serde_json::from_str(&health.body).unwrap();
    assert_eq!(health_json["status"], "ok");
    assert_eq!(health_json["auth_required"], true);

    // Unauthenticated /v1/* is rejected.
    let unauth = http_request(addr, "GET", "/v1/status", None, None).await;
    assert_eq!(unauth.status, 401);

    let tok = Some("secret-token-42");

    // Write + index on main, then commit.
    let write_resp = http_request(
        addr,
        "PUT",
        "/v1/node",
        tok,
        Some(json!({
            "path": "/core/rules.md",
            "content": "Always isolate branch searches.",
            "importance": 0.95,
            "tags": ["core", "safety"]
        })),
    )
    .await;
    assert_eq!(write_resp.status, 200);

    let seed_sem = http_request(
        addr,
        "PUT",
        "/v1/node",
        tok,
        Some(json!({
            "path": "/semantic/wal.md",
            "content": "---\ntitle: \"SQLite WAL\"\naliases: [\"BUSY_TIMEOUT\"]\n---\n# WAL\nSet busy_timeout=5000.",
            "importance": 0.9
        })),
    )
    .await;
    assert_eq!(seed_sem.status, 200);

    let commit_resp = http_request(
        addr,
        "POST",
        "/v1/commits",
        tok,
        Some(json!({ "branch": "main", "message": "seed core and wal" })),
    )
    .await;
    assert_eq!(commit_resp.status, 200);
    let c1 = serde_json::from_str::<Value>(&commit_resp.body).unwrap()["commit_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Create isolated branch and write a secret episodic log.
    let br_resp = http_request(
        addr,
        "POST",
        "/v1/branches",
        tok,
        Some(json!({ "name": "exp/isolated", "from": "main" })),
    )
    .await;
    assert_eq!(br_resp.status, 200);

    let ep_resp = http_request(
        addr,
        "PUT",
        "/v1/node",
        tok,
        Some(json!({
            "branch": "exp/isolated",
            "path": "/episodic/secret-session.md",
            "content": "- Observed ISOLATED_TOKEN_XYZ during SQLite WAL retry test.",
            "importance": 0.8
        })),
    )
    .await;
    assert_eq!(ep_resp.status, 200);
    http_request(
        addr,
        "POST",
        "/v1/commits",
        tok,
        Some(json!({ "branch": "exp/isolated", "message": "isolated log" })),
    )
    .await;

    // Main search must NOT see exp/isolated content.
    let main_search = http_request(
        addr,
        "POST",
        "/v1/search",
        tok,
        Some(json!({ "branch": "main", "query": "ISOLATED_TOKEN_XYZ" })),
    )
    .await;
    assert_eq!(main_search.status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&main_search.body)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // exp/isolated search finds it immediately.
    let br_search = http_request(
        addr,
        "POST",
        "/v1/search",
        tok,
        Some(json!({ "branch": "exp/isolated", "query": "ISOLATED_TOKEN_XYZ" })),
    )
    .await;
    assert_eq!(br_search.status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&br_search.body)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Compile /core on main.
    let compiled = http_request(
        addr,
        "POST",
        "/v1/compile",
        tok,
        Some(json!({ "branch": "main", "max_tokens": 500 })),
    )
    .await;
    assert_eq!(compiled.status, 200);
    assert!(compiled.body.contains("Always isolate branch searches."));

    // Collaborative ingest & consolidate over REST.
    let ingest = http_request(
        addr,
        "POST",
        "/v1/ingest",
        tok,
        Some(json!({
            "role": "pm",
            "draft": true,
            "title": "Retry Policy",
            "text": "Retries respect BUSY_TIMEOUT rules."
        })),
    )
    .await;
    assert_eq!(ingest.status, 200);
    let ingest_json: Value = serde_json::from_str(&ingest.body).unwrap();
    assert_eq!(ingest_json["branch"], "draft/pm-inbox");

    let consolidate = http_request(
        addr,
        "POST",
        "/v1/consolidate",
        tok,
        Some(json!({
            "branch": "exp/isolated",
            "auto_merge": true,
            "delete_branch": true
        })),
    )
    .await;
    assert_eq!(consolidate.status, 200);
    let cons_json: Value = serde_json::from_str(&consolidate.body).unwrap();
    assert_eq!(cons_json["episodes_scanned"], 1);
    assert!(!cons_json["distilled_cards"].as_array().unwrap().is_empty());

    // Diff & merge & rewind & delete branch & gc.
    let diff = http_request(
        addr,
        "GET",
        "/v1/diff?base=main&target=draft/pm-inbox",
        tok,
        None,
    )
    .await;
    assert_eq!(diff.status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&diff.body)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let merge = http_request(
        addr,
        "POST",
        "/v1/merge",
        tok,
        Some(json!({ "source": "draft/pm-inbox", "target": "main" })),
    )
    .await;
    assert_eq!(merge.status, 200);

    let rewind = http_request(
        addr,
        "POST",
        "/v1/rewind",
        tok,
        Some(json!({ "branch": "main", "commit_id": c1 })),
    )
    .await;
    assert_eq!(rewind.status, 200);

    let del_br = http_request(addr, "DELETE", "/v1/branches/exp/isolated", tok, None).await;
    assert_eq!(del_br.status, 200);

    let gc = http_request(addr, "POST", "/v1/gc", tok, None).await;
    assert_eq!(gc.status, 200);

    let _ = shutdown.send(());
}

#[tokio::test]
async fn daemon_sse_events_and_mcp_http_and_sse_transports() {
    let (addr, shutdown) = spawn_test_daemon(None).await;

    // 1. Connect to /v1/events?branch=main and verify filtered real-time SSE events.
    let stream = TcpStream::connect(addr).await.unwrap();
    let (read_half, mut write_half) = stream.into_split();
    write_half
        .write_all(
            format!("GET /v1/events?branch=main HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut sse_reader = BufReader::new(read_half);

    // Read HTTP response headers.
    let mut line = String::new();
    loop {
        line.clear();
        sse_reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }

    // Write to another branch (filtered out) and to main (delivered).
    http_request(
        addr,
        "POST",
        "/v1/branches",
        None,
        Some(json!({ "name": "other", "from": "main" })),
    )
    .await;
    http_request(
        addr,
        "PUT",
        "/v1/node",
        None,
        Some(json!({
            "branch": "other",
            "path": "/core/other.md",
            "content": "ignored event"
        })),
    )
    .await;
    http_request(
        addr,
        "PUT",
        "/v1/node",
        None,
        Some(json!({
            "branch": "main",
            "path": "/core/live.md",
            "content": "live sse node"
        })),
    )
    .await;

    let mut received_main_write = false;
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut buf = String::new();
        while !received_main_write {
            buf.clear();
            let n = sse_reader.read_line(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            assert!(!buf.contains("ignored event"));
            if buf.contains("live.md") {
                received_main_write = true;
            }
        }
    })
    .await
    .expect("timed out waiting for main SSE event");
    assert!(received_main_write);

    // 2. Test Streamable HTTP MCP POST /mcp (12 tools + mem_consolidate).
    let tools_resp = http_request(
        addr,
        "POST",
        "/mcp",
        None,
        Some(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        })),
    )
    .await;
    assert_eq!(tools_resp.status, 200);
    let tools_json: Value = serde_json::from_str(&tools_resp.body).unwrap();
    let tools = tools_json["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 12);
    assert!(tools.iter().any(|t| t["name"] == "mem_consolidate"));

    // Invoke mem_consolidate over Streamable HTTP POST /mcp.
    let mcp_ep = http_request(
        addr,
        "POST",
        "/mcp",
        None,
        Some(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "mem_write",
                "arguments": {
                    "path": "/episodic/http-mcp.md",
                    "content": "- Always verify BLAKE3 digest via Streamable HTTP MCP.",
                    "tags": ["episodic", "cas"]
                }
            }
        })),
    )
    .await;
    assert_eq!(mcp_ep.status, 200);

    let mcp_cons = http_request(
        addr,
        "POST",
        "/mcp",
        None,
        Some(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "mem_consolidate",
                "arguments": { "branch": "main", "dry_run": true }
            }
        })),
    )
    .await;
    assert_eq!(mcp_cons.status, 200);
    let cons_json: Value = serde_json::from_str(&mcp_cons.body).unwrap();
    assert_eq!(cons_json["result"]["isError"], false);
    assert!(cons_json["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("episodes_scanned"));

    // 3. Test Classic HTTP+SSE MCP (/mcp/sse -> endpoint -> POST /mcp/messages).
    let mcp_stream = TcpStream::connect(addr).await.unwrap();
    let (mcp_read, mut mcp_write) = mcp_stream.into_split();
    mcp_write
        .write_all(format!("GET /mcp/sse HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut mcp_reader = BufReader::new(mcp_read);
    loop {
        line.clear();
        mcp_reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }

    let endpoint_path = tokio::time::timeout(Duration::from_secs(3), async {
        let mut l = String::new();
        loop {
            l.clear();
            mcp_reader.read_line(&mut l).await.unwrap();
            if let Some(rest) = l.strip_prefix("data:") {
                let uri = rest.trim().to_string();
                if uri.starts_with("/mcp/messages?session_id=") {
                    return uri;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for MCP SSE endpoint event");

    let post_msg = http_request(
        addr,
        "POST",
        &endpoint_path,
        None,
        Some(json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": {
                "name": "mem_read",
                "arguments": { "path": "/core/live.md" }
            }
        })),
    )
    .await;
    assert_eq!(post_msg.status, 202);

    let sse_msg = tokio::time::timeout(Duration::from_secs(3), async {
        let mut l = String::new();
        loop {
            l.clear();
            mcp_reader.read_line(&mut l).await.unwrap();
            if let Some(rest) = l.strip_prefix("data:") {
                let data = rest.trim();
                if data.contains("live sse node") {
                    return data.to_string();
                }
            }
        }
    })
    .await
    .expect("timed out waiting for MCP SSE message reply");
    assert!(sse_msg.contains("\"id\":99"));

    let _ = shutdown.send(());
}

#[tokio::test]
async fn daemon_concurrent_multi_branch_writes_and_searches_stay_isolated() {
    let (addr, shutdown) = spawn_test_daemon(None).await;

    let mut handles = Vec::new();
    for worker_id in 0..4usize {
        handles.push(tokio::spawn(async move {
            let branch = format!("worker/w{worker_id}");
            let br_resp = http_request(
                addr,
                "POST",
                "/v1/branches",
                None,
                Some(json!({ "name": branch, "from": "main" })),
            )
            .await;
            assert_eq!(br_resp.status, 200);

            let token = format!("UNIQUE_WORKER_TOKEN_{worker_id}");
            for step in 0..5usize {
                let path = format!("/semantic/w{worker_id}_step{step}.md");
                let content = format!(
                    "---\naliases: [\"{token}\"]\n---\n# Step {step}\nWorker {worker_id} private insight {token}."
                );
                let wr = http_request(
                    addr,
                    "PUT",
                    "/v1/node",
                    None,
                    Some(json!({
                        "branch": branch,
                        "path": path,
                        "content": content,
                    })),
                )
                .await;
                assert_eq!(wr.status, 200);
            }

            let commit = http_request(
                addr,
                "POST",
                "/v1/commits",
                None,
                Some(json!({
                    "branch": branch,
                    "message": format!("commit worker {worker_id}")
                })),
            )
            .await;
            assert_eq!(commit.status, 200);

            let own_hits = http_request(
                addr,
                "POST",
                "/v1/search",
                None,
                Some(json!({ "branch": branch, "query": token })),
            )
            .await;
            assert_eq!(own_hits.status, 200);
            assert_eq!(
                serde_json::from_str::<Value>(&own_hits.body)
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .len(),
                5
            );

            let other_token = format!("UNIQUE_WORKER_TOKEN_{}", (worker_id + 1) % 4);
            let leaked_hits = http_request(
                addr,
                "POST",
                "/v1/search",
                None,
                Some(json!({ "branch": branch, "query": other_token })),
            )
            .await;
            assert_eq!(leaked_hits.status, 200);
            assert_eq!(
                serde_json::from_str::<Value>(&leaked_hits.body)
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .len(),
                0
            );
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // Main branch must still have 0 hits for every worker token.
    for worker_id in 0..4usize {
        let token = format!("UNIQUE_WORKER_TOKEN_{worker_id}");
        let main_hits = http_request(
            addr,
            "POST",
            "/v1/search",
            None,
            Some(json!({ "branch": "main", "query": token })),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&main_hits.body)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    let _ = shutdown.send(());
}

#[tokio::test]
async fn daemon_web_ui_and_branch_graph_isolation() {
    let (addr, shutdown) = spawn_test_daemon(Some("ui-token-7")).await;
    let tok = Some("ui-token-7");

    // Web UI loads cleanly without an Authorization header so users can enter bearer tokens in-app.
    for path in ["/", "/ui"] {
        let resp = http_request(addr, "GET", path, None, None).await;
        assert_eq!(resp.status, 200, "expected 200 for {path}");
        assert!(
            resp.body.contains("Engram"),
            "expected Engram title in {path}"
        );
        assert!(
            resp.body.contains("/v1/graph"),
            "expected graph endpoint in {path}"
        );
    }

    // /v1/graph still enforces bearer authentication.
    let unauth_graph = http_request(addr, "GET", "/v1/graph?branch=main", None, None).await;
    assert_eq!(unauth_graph.status, 401);

    // Populate main branch with linked notes.
    let w1 = http_request(
        addr,
        "PUT",
        "/v1/node",
        tok,
        Some(json!({
            "branch": "main",
            "path": "/semantic/architecture.md",
            "content": "---\ntitle: Engram Architecture\ntags: [core, design]\n---\n\n# Engram Architecture\n\nUses [[sqlite-wal]] for ACID snapshots.\n",
            "tags": ["core", "design"],
            "importance": 0.9
        })),
    )
    .await;
    assert_eq!(w1.status, 200);

    let w2 = http_request(
        addr,
        "PUT",
        "/v1/node",
        tok,
        Some(json!({
            "branch": "main",
            "path": "/procedural/sqlite-wal.md",
            "content": "# SQLite WAL\n\nReferenced by [[Engram Architecture]].\n",
            "tags": ["sqlite"],
            "importance": 0.8
        })),
    )
    .await;
    assert_eq!(w2.status, 200);

    let commit_main = http_request(
        addr,
        "POST",
        "/v1/commits",
        tok,
        Some(json!({ "branch": "main", "message": "seed graph notes", "author": "tester" })),
    )
    .await;
    assert_eq!(commit_main.status, 200);

    // Fork exp/isolated branch with additional notes & links.
    let mk_branch = http_request(
        addr,
        "POST",
        "/v1/branches",
        tok,
        Some(json!({ "name": "exp/isolated", "from": "main" })),
    )
    .await;
    assert_eq!(mk_branch.status, 200);

    let w3 = http_request(
        addr,
        "PUT",
        "/v1/node",
        tok,
        Some(json!({
            "branch": "exp/isolated",
            "path": "/semantic/secret-idea.md",
            "content": "# Secret Idea\n\nConnects [[sqlite-wal]] and [[unmerged-sibling]].\n"
        })),
    )
    .await;
    assert_eq!(w3.status, 200);

    let w4 = http_request(
        addr,
        "PUT",
        "/v1/node",
        tok,
        Some(json!({
            "branch": "exp/isolated",
            "path": "/semantic/unmerged-sibling.md",
            "content": "# Unmerged Sibling\n\nExperimental branch only.\n"
        })),
    )
    .await;
    assert_eq!(w4.status, 200);

    // Graph for main must strictly omit exp/isolated nodes and edges.
    let main_graph_resp = http_request(addr, "GET", "/v1/graph?branch=main", tok, None).await;
    assert_eq!(main_graph_resp.status, 200);
    let main_graph: Value = serde_json::from_str(&main_graph_resp.body).unwrap();
    let main_nodes = main_graph["nodes"].as_array().unwrap();
    let main_edges = main_graph["edges"].as_array().unwrap();
    assert_eq!(main_nodes.len(), 2);
    assert!(main_nodes
        .iter()
        .all(|n| n["path"] != "/semantic/secret-idea.md"));
    assert!(main_edges.iter().any(|e| {
        e["source"] == "/semantic/architecture.md" && e["target"] == "/procedural/sqlite-wal.md"
    }));
    assert!(main_edges.iter().any(|e| {
        e["source"] == "/procedural/sqlite-wal.md" && e["target"] == "/semantic/architecture.md"
    }));

    // Graph for exp/isolated includes all 4 nodes and its branch-specific edges.
    let exp_graph_resp =
        http_request(addr, "GET", "/v1/graph?branch=exp%2Fisolated", tok, None).await;
    assert_eq!(exp_graph_resp.status, 200);
    let exp_graph: Value = serde_json::from_str(&exp_graph_resp.body).unwrap();
    let exp_nodes = exp_graph["nodes"].as_array().unwrap();
    let exp_edges = exp_graph["edges"].as_array().unwrap();
    assert_eq!(exp_nodes.len(), 4);
    assert!(exp_edges.iter().any(|e| {
        e["source"] == "/semantic/secret-idea.md" && e["target"] == "/semantic/unmerged-sibling.md"
    }));

    let _ = shutdown.send(());
}
