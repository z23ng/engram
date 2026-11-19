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

fn engine_vcs(f: &Fixture) -> mem_vcs::MemVcs {
    f.cp.engine("alpha").unwrap().vcs().clone()
}

async fn rest_write(f: &Fixture, key: &str, path: &str, content: &str, message: Option<&str>) {
    let mut body = json!({"path": path, "content": content});
    if let Some(m) = message {
        body["commit_message"] = json!(m);
    }
    let (status, resp) = call(
        f,
        "POST",
        "/api/v1/projects/alpha/write",
        Some(key),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
}

#[tokio::test]
async fn branches_tree_log_and_status_show_the_branch_view() {
    use mem_core::Revision;
    use mem_vcs::VersionControlEngine;
    let f = fixture();
    let dev = key(&f, "dev", Role::Dev, Some(&["alpha"]));
    rest_write(&f, &dev, "/semantic/a.md", "a", Some("add a")).await;
    engine_vcs(&f)
        .create_branch("draft/x", &Revision::BranchHead("main".into()))
        .unwrap();
    rest_write(&f, &dev, "/core/b.md", "b", None).await;

    let (_, b) = call(
        &f,
        "GET",
        "/api/v1/projects/alpha/branches",
        Some(&dev),
        None,
    )
    .await;
    let branches = b["branches"].as_array().unwrap();
    let names: Vec<&str> = branches
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["draft/x", "main"]);
    assert_eq!(
        (
            branches[0]["changes"].as_u64(),
            branches[1]["changes"].as_u64()
        ),
        (Some(0), Some(1))
    );
    assert_eq!(branches[1]["head"].as_str().unwrap().len(), 64);

    let (_, tree) = call(
        &f,
        "GET",
        "/api/v1/projects/alpha/tree?branch=main",
        Some(&dev),
        None,
    )
    .await;
    let paths: Vec<&str> = tree["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["/core/b.md", "/semantic/a.md"]);
    assert_eq!(tree["files"][0]["content_type"], "text/markdown");
    let (_, draft) = call(
        &f,
        "GET",
        "/api/v1/projects/alpha/tree?branch=draft%2Fx",
        Some(&dev),
        None,
    )
    .await;
    assert_eq!(draft["files"].as_array().unwrap().len(), 1);

    let (_, log) = call(
        &f,
        "GET",
        "/api/v1/projects/alpha/log?branch=main&limit=1",
        Some(&dev),
        None,
    )
    .await;
    let commits = log["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(
        (
            commits[0]["message"].as_str(),
            commits[0]["author"].as_str()
        ),
        (Some("add a"), Some("dev"))
    );

    let (_, st) = call(
        &f,
        "GET",
        "/api/v1/projects/alpha/status?branch=main",
        Some(&dev),
        None,
    )
    .await;
    assert_eq!(
        st,
        json!({"changes": [{"path": "/core/b.md", "op": "upsert"}]})
    );
}

#[tokio::test]
async fn diff_compares_heads_commits_and_workspace() {
    let f = fixture();
    let dev = key(&f, "dev", Role::Dev, Some(&["alpha"]));
    rest_write(&f, &dev, "/semantic/a.md", "a", Some("add a")).await;
    rest_write(&f, &dev, "/semantic/a.md", "a2", None).await;
    let uri = "/api/v1/projects/alpha/diff?base=main&target=workspace:main";
    let (status, d) = call(&f, "GET", uri, Some(&dev), None).await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["entries"][0]["path"], "/semantic/a.md");
    assert_eq!(d["entries"][0]["kind"], "modified");
    let first = engine_vcs(&f)
        .log("main", 10)
        .unwrap()
        .last()
        .unwrap()
        .commit_id
        .to_hex();
    let uri = format!("/api/v1/projects/alpha/diff?base={first}&target=main");
    let (_, d) = call(&f, "GET", &uri, Some(&dev), None).await;
    assert_eq!(d["entries"][0]["kind"], "added");
}

#[tokio::test]
async fn search_uses_the_branch_workspace_only() {
    use mem_core::Revision;
    use mem_vcs::VersionControlEngine;
    let f = fixture();
    let bot = key(&f, "bot", Role::Bot, Some(&["alpha"]));
    let dev = key(&f, "dev", Role::Dev, Some(&["alpha"]));
    engine_vcs(&f)
        .create_branch("other", &Revision::BranchHead("main".into()))
        .unwrap();
    rest_write(
        &f,
        &dev,
        "/semantic/rust.md",
        "# Rust\n\ntokio runtime notes",
        None,
    )
    .await;
    let (status, hits) = call(
        &f,
        "GET",
        "/api/v1/projects/alpha/search?q=tokio&branch=main",
        Some(&bot),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    assert_eq!(hits["hits"][0]["path"], "/semantic/rust.md");
    let (_, none) = call(
        &f,
        "GET",
        "/api/v1/projects/alpha/search?q=tokio&branch=other",
        Some(&bot),
        None,
    )
    .await;
    assert_eq!(none["hits"], json!([]));
}

#[tokio::test]
async fn raw_returns_bytes_with_stored_content_type() {
    use mem_core::{MemoryNode, VirtualPath};
    let f = fixture();
    let bot = key(&f, "bot", Role::Bot, Some(&["alpha"]));
    let png = vec![0x89, b'P', b'N', b'G', 0xff];
    let mut node = MemoryNode::new(
        VirtualPath::new("/semantic/_attachments/p.png").unwrap(),
        png.clone(),
    );
    node.metadata.content_type = "image/png".into();
    engine_vcs(&f).write_node("main", node).unwrap();
    let req = Request::get("/api/v1/projects/alpha/raw?path=/semantic/_attachments/p.png")
        .header("authorization", format!("Bearer {bot}"))
        .body(Body::empty())
        .unwrap();
    let resp = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "image/png");
    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
        png
    );
}

#[tokio::test]
async fn browse_endpoints_check_scope_and_branch() {
    let f = fixture();
    let dev = key(&f, "dev", Role::Dev, Some(&["alpha"]));
    for uri in [
        "/api/v1/projects/beta/tree",
        "/api/v1/projects/beta/branches",
        "/api/v1/projects/beta/log",
    ] {
        assert_eq!(
            call(&f, "GET", uri, Some(&dev), None).await.0,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
    let (status, _) = call(
        &f,
        "GET",
        "/api/v1/projects/alpha/log?branch=nope",
        Some(&dev),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn upload(f: &Fixture, key: &str, path: &str, body: Vec<u8>) -> StatusCode {
    let req = Request::put(format!("/api/v1/projects/alpha/raw?path={path}"))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "image/png")
        .body(Body::from(body))
        .unwrap();
    f.app.clone().oneshot(req).await.unwrap().status()
}

fn audit_ops(f: &Fixture) -> Vec<String> {
    f.cp.list_audit("alpha", 50)
        .unwrap()
        .into_iter()
        .map(|a| a.operation)
        .collect()
}

#[tokio::test]
async fn commit_and_delete_use_the_key_author_and_are_audited() {
    let f = fixture();
    let dev = key(&f, "dev", Role::Dev, Some(&["alpha"]));
    rest_write(&f, &dev, "/semantic/a.md", "a", None).await;
    let (status, c) = call(
        &f,
        "POST",
        "/api/v1/projects/alpha/commit",
        Some(&dev),
        Some(json!({"message": "save"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{c}");
    let log = engine_vcs(&f).log("main", 1).unwrap();
    assert_eq!(
        (log[0].author.as_str(), log[0].commit_id.to_hex()),
        ("dev", c["commit_id"].as_str().unwrap().to_string())
    );
    let (status, _) = call(
        &f,
        "POST",
        "/api/v1/projects/alpha/commit",
        Some(&dev),
        Some(json!({"message": "empty"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "nothing to commit");

    let (status, d) = call(
        &f,
        "DELETE",
        "/api/v1/projects/alpha/files?path=/semantic/a.md",
        Some(&dev),
        None,
    )
    .await;
    assert_eq!((status, d), (StatusCode::OK, json!({"deleted": 1})));
    let (_, st) = call(&f, "GET", "/api/v1/projects/alpha/status", Some(&dev), None).await;
    assert_eq!(st["changes"][0]["op"], "delete");
    let (status, _) = call(
        &f,
        "DELETE",
        "/api/v1/projects/alpha/files?path=/semantic/zz.md",
        Some(&dev),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(audit_ops(&f), ["rest.delete", "rest.commit", "rest.write"]);
}

#[tokio::test]
async fn upload_accepts_images_only_below_attachments() {
    use mem_core::{Revision, VirtualPath};
    use mem_vcs::VersionControlEngine;
    let f = fixture();
    let pm = key(&f, "pm", Role::Pm, Some(&["alpha"]));
    let png = vec![0x89, b'P', b'N', b'G', 0xff];
    assert_eq!(
        upload(&f, &pm, "/semantic/_attachments/x.png", png.clone()).await,
        StatusCode::OK
    );
    let vp = VirtualPath::new("/semantic/_attachments/x.png").unwrap();
    let node = engine_vcs(&f)
        .read(&Revision::BranchWorkspace("main".into()), &vp)
        .unwrap()
        .unwrap();
    assert_eq!(
        (node.content, node.metadata.content_type.as_str()),
        (png.clone(), "image/png")
    );
    for denied in [
        "/semantic/x.png",
        "/core/_attachments/x.png",
        "/semantic/_attachments/x.svg",
    ] {
        assert_eq!(
            upload(&f, &pm, denied, png.clone()).await,
            StatusCode::FORBIDDEN,
            "{denied}"
        );
    }
    let big = vec![0u8; 8 * 1024 * 1024 + 1];
    assert_eq!(
        upload(&f, &pm, "/semantic/_attachments/big.png", big).await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(audit_ops(&f), ["rest.upload"]);
}

#[tokio::test]
async fn branch_create_and_merge_need_branch_permission() {
    let f = fixture();
    let pm = key(&f, "pm", Role::Pm, Some(&["alpha"]));
    rest_write(&f, &pm, "/semantic/shared.md", "base", Some("base")).await;
    let body = json!({"name": "draft/pm", "from": "main"});
    let (status, _) = call(
        &f,
        "POST",
        "/api/v1/projects/alpha/branches",
        Some(&pm),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        call(
            &f,
            "POST",
            "/api/v1/projects/alpha/branches",
            Some(&pm),
            Some(body)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    let draft = json!({"path": "/semantic/new.md", "content": "n", "branch": "draft/pm", "commit_message": "draft"});
    call(
        &f,
        "POST",
        "/api/v1/projects/alpha/write",
        Some(&pm),
        Some(draft),
    )
    .await;
    let merge = json!({"source": "draft/pm", "target": "main"});
    let (status, m) = call(
        &f,
        "POST",
        "/api/v1/projects/alpha/merge",
        Some(&pm),
        Some(merge.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{m}");
    assert!(m["commit_id"].is_string(), "{m}");
    let (_, tree) = call(&f, "GET", "/api/v1/projects/alpha/tree", Some(&pm), None).await;
    assert!(tree.to_string().contains("/semantic/new.md"));

    for (branch, text) in [("main", "ours"), ("draft/pm", "theirs")] {
        let w = json!({"path": "/semantic/shared.md", "content": text, "branch": branch, "commit_message": text});
        call(
            &f,
            "POST",
            "/api/v1/projects/alpha/write",
            Some(&pm),
            Some(w),
        )
        .await;
    }
    let (status, conflict) = call(
        &f,
        "POST",
        "/api/v1/projects/alpha/merge",
        Some(&pm),
        Some(merge),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["conflicts"][0]["path"], "/semantic/shared.md");
    let ops = audit_ops(&f);
    assert!(
        ops.contains(&"rest.branch".to_string()) && ops.contains(&"rest.merge".to_string()),
        "{ops:?}"
    );
}

#[tokio::test]
async fn bot_cannot_change_anything() {
    let f = fixture();
    let bot = key(&f, "bot", Role::Bot, Some(&["alpha"]));
    let p = "/api/v1/projects/alpha";
    let cases = [
        ("POST", format!("{p}/commit"), Some(json!({"message": "m"}))),
        ("DELETE", format!("{p}/files?path=/semantic/a.md"), None),
        ("POST", format!("{p}/branches"), Some(json!({"name": "x"}))),
        (
            "POST",
            format!("{p}/merge"),
            Some(json!({"source": "main"})),
        ),
    ];
    for (method, uri, body) in cases {
        assert_eq!(
            call(&f, method, &uri, Some(&bot), body).await.0,
            StatusCode::FORBIDDEN,
            "{method} {uri}"
        );
    }
    assert_eq!(
        upload(&f, &bot, "/semantic/_attachments/x.png", vec![1]).await,
        StatusCode::FORBIDDEN
    );
    assert!(audit_ops(&f).is_empty());
}

#[tokio::test]
async fn raw_never_serves_active_content_inline() {
    use mem_core::{MemoryNode, VirtualPath};
    let f = fixture();
    let dev = key(&f, "dev", Role::Dev, Some(&["alpha"]));
    let cases = [
        (
            "/semantic/x.md",
            "text/html",
            "text/plain; charset=utf-8",
            None,
        ),
        (
            "/semantic/y.md",
            "application/xhtml+xml",
            "application/octet-stream",
            Some("attachment"),
        ),
        (
            "/semantic/z.md",
            "image/svg+xml",
            "application/octet-stream",
            Some("attachment"),
        ),
        (
            "/semantic/w.md",
            "text/markdown",
            "text/plain; charset=utf-8",
            None,
        ),
    ];
    for (path, stored, served, disposition) in cases {
        let mut node = MemoryNode::new(
            VirtualPath::new(path).unwrap(),
            b"<script>alert(1)</script>".to_vec(),
        );
        node.metadata.content_type = stored.into();
        engine_vcs(&f).write_node("main", node).unwrap();
        let req = Request::get(format!("/api/v1/projects/alpha/raw?path={path}"))
            .header("authorization", format!("Bearer {dev}"))
            .body(Body::empty())
            .unwrap();
        let resp = f.app.clone().oneshot(req).await.unwrap();
        let h = resp.headers();
        assert_eq!(h["content-type"], served, "{stored}");
        assert_eq!(h["content-security-policy"], "sandbox; default-src 'none'");
        assert_eq!(
            h.get("content-disposition").map(|v| v.to_str().unwrap()),
            disposition,
            "{stored}"
        );
    }
}

#[tokio::test]
async fn merge_commit_uses_the_key_name_as_author() {
    let f = fixture();
    let pm = key(&f, "pm-alice", Role::Pm, Some(&["alpha"]));
    rest_write(&f, &pm, "/semantic/base.md", "base", Some("base")).await;
    call(
        &f,
        "POST",
        "/api/v1/projects/alpha/branches",
        Some(&pm),
        Some(json!({"name": "draft"})),
    )
    .await;
    for (branch, path) in [
        ("main", "/semantic/main.md"),
        ("draft", "/semantic/draft.md"),
    ] {
        let w = json!({"path": path, "content": "x", "branch": branch, "commit_message": path});
        call(
            &f,
            "POST",
            "/api/v1/projects/alpha/write",
            Some(&pm),
            Some(w),
        )
        .await;
    }
    let body = json!({"source": "draft", "target": "main"});
    let (status, m) = call(
        &f,
        "POST",
        "/api/v1/projects/alpha/merge",
        Some(&pm),
        Some(body),
    )
    .await;
    assert_eq!(
        (status, m["outcome"].as_str()),
        (StatusCode::OK, Some("merged")),
        "{m}"
    );
    assert_eq!(engine_vcs(&f).log("main", 1).unwrap()[0].author, "pm-alice");
}
