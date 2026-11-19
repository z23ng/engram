//! REST API used by the web client (S4a).

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
    Fixture {
        _dir: dir,
        app: router(cp.clone()),
        cp,
    }
}

/// `projects = None` creates a key for all projects.
fn key(f: &Fixture, name: &str, role: Role, projects: Option<&[&str]>) -> String {
    let scope = projects.map(|p| p.iter().map(|s| s.to_string()).collect());
    f.cp.create_key(name, role, scope, None).unwrap().1
}

async fn call(
    f: &Fixture,
    method: &str,
    uri: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(k) = key {
        req = req.header("authorization", format!("Bearer {k}"));
    }
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = f
        .app
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn me_returns_the_key_identity() {
    let f = fixture();
    let dev = key(&f, "dev-ann", Role::Dev, Some(&["alpha"]));
    let (status, me) = call(&f, "GET", "/api/v1/me", Some(&dev), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        me,
        json!({"name": "dev-ann", "role": "dev", "projects": ["alpha"]})
    );
    let (status, _) = call(&f, "GET", "/api/v1/me", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn project_list_follows_key_scope_and_create_needs_global_admin() {
    let f = fixture();
    let dev = key(&f, "dev", Role::Dev, Some(&["alpha"]));
    let admin = key(&f, "admin", Role::Admin, None);
    let scoped_admin = key(&f, "a-admin", Role::Admin, Some(&["alpha"]));
    let (_, list) = call(&f, "GET", "/api/v1/projects", Some(&dev), None).await;
    assert_eq!(list, json!({"projects": ["alpha"]}));

    let body = json!({"slug": "gamma"});
    for denied in [&dev, &scoped_admin] {
        let (status, _) = call(
            &f,
            "POST",
            "/api/v1/projects",
            Some(denied),
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    let (status, _) = call(&f, "POST", "/api/v1/projects", Some(&admin), Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, list) = call(&f, "GET", "/api/v1/projects", Some(&admin), None).await;
    assert_eq!(list, json!({"projects": ["alpha", "beta", "gamma"]}));
}

#[tokio::test]
async fn key_endpoints_create_list_and_revoke_for_global_admin_only() {
    let f = fixture();
    let admin = key(&f, "admin", Role::Admin, None);
    let dev = key(&f, "dev", Role::Dev, None);
    let body = json!({"name": "bot1", "role": "bot", "projects": ["alpha"], "expires_days": 30});
    let (status, created) = call(&f, "POST", "/api/v1/keys", Some(&admin), Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let secret = created["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("egm_"));
    assert_eq!(created["key"]["role"], "bot");
    assert!(created["key"]["expires_at_ms"].as_i64().is_some());

    let (_, list) = call(&f, "GET", "/api/v1/keys", Some(&admin), None).await;
    let names: Vec<&str> = list["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["admin", "dev", "bot1"]);
    assert!(!list.to_string().contains(&secret) && !list.to_string().contains("hash"));
    assert_eq!(
        call(&f, "GET", "/api/v1/me", Some(&secret), None).await.0,
        StatusCode::OK
    );

    let id = created["key"]["id"].as_i64().unwrap();
    let (status, _) = call(
        &f,
        "DELETE",
        &format!("/api/v1/keys/{id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        call(&f, "GET", "/api/v1/me", Some(&secret), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&f, "DELETE", "/api/v1/keys/999", Some(&admin), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        call(&f, "GET", "/api/v1/keys", Some(&dev), None).await.0,
        StatusCode::FORBIDDEN
    );
    let bad = json!({"name": "x", "role": "root"});
    assert_eq!(
        call(&f, "POST", "/api/v1/keys", Some(&admin), Some(bad))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn audit_endpoint_needs_manage_on_the_project() {
    let f = fixture();
    let admin = key(&f, "admin", Role::Admin, Some(&["alpha"]));
    let dev = key(&f, "dev", Role::Dev, Some(&["alpha"]));
    let write = json!({"path": "/semantic/a.md", "content": "a", "commit_message": "add a"});
    let (status, _) = call(
        &f,
        "POST",
        "/api/v1/projects/alpha/write",
        Some(&dev),
        Some(write),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, audit) = call(
        &f,
        "GET",
        "/api/v1/projects/alpha/audit?limit=5",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(audit["entries"][0]["operation"], "rest.write");
    assert_eq!(audit["entries"][0]["key_name"], "dev");
    assert!(audit["entries"][0]["commit_id"].is_string());
    let (status, _) = call(&f, "GET", "/api/v1/projects/alpha/audit", Some(&dev), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
