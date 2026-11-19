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
    let (cp, _) = ControlPlane::open(dir.path(), Box::new(MemSearchEngine::new_bm25_only)).unwrap();
    cp.create_project("alpha").unwrap();
    cp.create_project("beta").unwrap();
    let cp = Arc::new(cp);
    Fixture { _dir: dir, app: router(cp.clone()), cp }
}

fn key(f: &Fixture, role: Role, projects: &[&str]) -> String {
    let scope = Some(projects.iter().map(|p| p.to_string()).collect());
    f.cp.create_key("t", role, scope, None).unwrap().1
}

async fn send(
    app: &Router,
    key: Option<&str>,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().uri(uri).method(if body.is_some() { "POST" } else { "GET" });
    if let Some(k) = key {
        req = req.header("authorization", format!("Bearer {k}"));
    }
    let req = match body {
        Some(b) => req.header("content-type", "application/json").body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    };
    let resp = app.clone().oneshot(req.unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

const READ: &str = "/api/v1/projects/alpha/read?path=/semantic/a.md";
const WRITE: &str = "/api/v1/projects/alpha/write";

#[tokio::test]
async fn missing_or_unknown_or_revoked_key_is_401() {
    let f = fixture();
    assert_eq!(send(&f.app, None, READ, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&f.app, Some("egm_nope"), READ, None).await.0, StatusCode::UNAUTHORIZED);
    let (info, plain) = f.cp.create_key("t", Role::Dev, None, None).unwrap();
    f.cp.revoke_key(info.id).unwrap();
    assert_eq!(send(&f.app, Some(&plain), READ, None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn key_for_project_a_is_403_on_project_b() {
    let f = fixture();
    let k = key(&f, Role::Dev, &["beta"]);
    assert_eq!(send(&f.app, Some(&k), READ, None).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn bot_can_read_but_write_is_403() {
    let f = fixture();
    let k = key(&f, Role::Bot, &["alpha"]);
    let body = json!({"path": "/semantic/a.md", "content": "x"});
    assert_eq!(send(&f.app, Some(&k), WRITE, Some(body)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(send(&f.app, Some(&k), READ, None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dev_write_then_read_returns_content_without_cross_project_leak() {
    let f = fixture();
    let k = key(&f, Role::Dev, &["alpha", "beta"]);
    let body = json!({"path": "/semantic/a.md", "content": "alpha rule", "commit_message": "add"});
    let (status, written) = send(&f.app, Some(&k), WRITE, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert!(written["commit_id"].is_string());

    let (status, read) = send(&f.app, Some(&k), READ, None).await;
    assert_eq!((status, read["content"].as_str()), (StatusCode::OK, Some("alpha rule")));
    let other = "/api/v1/projects/beta/read?path=/semantic/a.md";
    assert_eq!(send(&f.app, Some(&k), other, None).await.0, StatusCode::NOT_FOUND);
}
