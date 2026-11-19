use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use mem_search::MemSearchEngine;
use mem_server::{router, ControlPlane, Role};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Fixture {
    _dir: tempfile::TempDir,
    cp: Arc<ControlPlane>,
    app: Router,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let factory = Box::new(MemSearchEngine::new_bm25_only);
    let (cp, _) = ControlPlane::open(dir.path(), factory).unwrap();
    cp.create_project("alpha").unwrap();
    cp.create_project("beta").unwrap();
    let cp = Arc::new(cp);
    Fixture {
        _dir: dir,
        app: router(cp.clone()),
        cp,
    }
}

fn key(f: &Fixture, name: &str, role: Role, projects: &[&str]) -> String {
    let scope = Some(projects.iter().map(|p| p.to_string()).collect());
    f.cp.create_key(name, role, scope, None).unwrap().1
}

async fn post(f: &Fixture, project: &str, key: &str, body: &str) -> (StatusCode, String) {
    let req = Request::post(format!("/p/{project}/mcp"))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = f.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn rpc(f: &Fixture, project: &str, key: &str, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let (status, text) = post(f, project, key, &body.to_string()).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(&text).unwrap()
}

async fn call(f: &Fixture, project: &str, key: &str, tool: &str, args: Value) -> Value {
    let params = json!({"name": tool, "arguments": args});
    rpc(f, project, key, "tools/call", params).await["result"].clone()
}

async fn tool_names(f: &Fixture, key: &str) -> Vec<String> {
    let list = rpc(f, "alpha", key, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap().iter();
    tools
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

fn text(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn tools_list_is_filtered_by_role() {
    let f = fixture();
    let bot = tool_names(&f, &key(&f, "bot", Role::Bot, &["alpha"])).await;
    assert!(bot.contains(&"mem_read".into()) && bot.contains(&"mem_search".into()));
    for denied in [
        "mem_write",
        "mem_commit",
        "mem_merge",
        "mem_ingest",
        "mem_rewind",
    ] {
        assert!(!bot.contains(&denied.to_string()), "bot sees {denied}");
    }
    assert_eq!(
        tool_names(&f, &key(&f, "dev", Role::Dev, &["alpha"]))
            .await
            .len(),
        11
    );
}

#[tokio::test]
async fn bot_write_is_forbidden_and_touches_nothing() {
    let f = fixture();
    let bot = key(&f, "bot", Role::Bot, &["alpha"]);
    let dev = key(&f, "dev", Role::Dev, &["alpha"]);
    let args = json!({"path": "/semantic/x.md", "content": "nope"});
    let denied = call(&f, "alpha", &bot, "mem_write", args).await;
    assert_eq!(denied["isError"], true);
    assert!(text(&denied).contains("forbidden"), "{denied}");
    let read = call(
        &f,
        "alpha",
        &dev,
        "mem_read",
        json!({"path": "/semantic/x.md"}),
    )
    .await;
    assert!(text(&read).contains("\"found\":false"), "{read}");
    call(&f, "alpha", &bot, "mem_list", json!({})).await;
    assert!(f.cp.list_audit("alpha", 10).unwrap().is_empty());
}

#[tokio::test]
async fn dev_commit_via_mcp_is_visible_via_rest_with_key_author_and_audit() {
    let f = fixture();
    let dev = key(&f, "dev-agent", Role::Dev, &["alpha"]);
    let args = json!({"path": "/semantic/a.md", "content": "alpha rule"});
    assert_eq!(
        call(&f, "alpha", &dev, "mem_write", args).await["isError"],
        false
    );
    let args = json!({"message": "add rule", "author": "spoofed"});
    let committed = call(&f, "alpha", &dev, "mem_commit", args).await;
    assert_eq!(committed["isError"], false, "{committed}");

    let req = Request::get("/api/v1/projects/alpha/read?path=/semantic/a.md")
        .header("authorization", format!("Bearer {dev}"))
        .body(Body::empty())
        .unwrap();
    let resp = f.app.clone().oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("alpha rule"));

    let log = f.cp.engine("alpha").unwrap().vcs().log("main", 1).unwrap();
    assert_eq!(log[0].author, "dev-agent");
    let audit = f.cp.list_audit("alpha", 10).unwrap();
    let ops: Vec<&str> = audit.iter().map(|a| a.operation.as_str()).collect();
    assert_eq!(ops, ["mem_commit", "mem_write"]);
    assert_eq!(
        audit[0].commit_id.as_deref(),
        Some(log[0].commit_id.to_hex().as_str())
    );
    assert_eq!(
        (audit[0].key_name.as_str(), audit[0].role.as_str()),
        ("dev-agent", "dev")
    );
}

#[tokio::test]
async fn projects_are_isolated_over_mcp() {
    let f = fixture();
    let a_only = key(&f, "a", Role::Dev, &["alpha"]);
    let (status, _) = post(
        &f,
        "beta",
        &a_only,
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let both = key(&f, "both", Role::Dev, &["alpha", "beta"]);
    let args = json!({"path": "/semantic/secret.md", "content": "alpha only"});
    call(&f, "alpha", &both, "mem_write", args).await;
    let read = call(
        &f,
        "beta",
        &both,
        "mem_read",
        json!({"path": "/semantic/secret.md"}),
    )
    .await;
    assert!(text(&read).contains("\"found\":false"), "{read}");
}

#[tokio::test]
async fn transport_rules_notification_get_and_parse_error() {
    let f = fixture();
    let k = key(&f, "dev", Role::Dev, &["alpha"]);
    let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    assert_eq!(
        post(&f, "alpha", &k, note).await,
        (StatusCode::ACCEPTED, String::new())
    );

    let (_, bad) = post(&f, "alpha", &k, "{not json").await;
    let bad: Value = serde_json::from_str(&bad).unwrap();
    assert_eq!(bad["error"]["code"], -32700);

    let get = Request::get("/p/alpha/mcp")
        .header("authorization", format!("Bearer {k}"))
        .body(Body::empty())
        .unwrap();
    let resp = f.app.clone().oneshot(get).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
