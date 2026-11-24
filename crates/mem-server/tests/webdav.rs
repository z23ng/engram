use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::Router;
use mem_core::{Revision, VirtualPath};
use mem_search::MemSearchEngine;
use mem_server::{router, ControlPlane, Role};
use mem_vcs::VersionControlEngine;
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
    let cp = cp.with_dav_commit_delay(Duration::from_millis(200));
    cp.create_project("alpha").unwrap();
    cp.create_project("beta").unwrap();
    let cp = Arc::new(cp);
    Fixture {
        _dir: dir,
        app: router(cp.clone()),
        cp,
    }
}

fn key(f: &Fixture, name: &str, role: Role) -> String {
    let scope = Some(vec!["alpha".to_string()]);
    f.cp.create_key(name, role, scope, None).unwrap().1
}

fn basic(key: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("obsidian:{key}").into_bytes();
    let mut out = String::new();
    for c in raw.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            let pad = i > c.len();
            out.push(if pad {
                '='
            } else {
                T[(n >> (18 - 6 * i) & 63) as usize] as char
            });
        }
    }
    format!("Basic {out}")
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

async fn dav(
    f: &Fixture,
    method: &str,
    uri: &str,
    key: Option<&str>,
    extra: &[(&str, &str)],
    body: Vec<u8>,
) -> Resp {
    let mut req = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri);
    if let Some(k) = key {
        req = req.header("authorization", basic(k));
    }
    for (h, v) in extra {
        req = req.header(*h, *v);
    }
    let resp = f
        .app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    Resp {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).to_string(),
    }
}

async fn put(f: &Fixture, uri: &str, key: &str, body: &str) -> StatusCode {
    dav(f, "PUT", uri, Some(key), &[], body.as_bytes().to_vec())
        .await
        .status
}

async fn get(f: &Fixture, uri: &str, key: &str) -> Resp {
    dav(f, "GET", uri, Some(key), &[], Vec::new()).await
}

fn hrefs(xml: &str) -> Vec<String> {
    xml.split("href>")
        .skip(1)
        .step_by(2)
        .filter_map(|s| s.split("</").next())
        .map(String::from)
        .collect()
}

fn engine_read(f: &Fixture, branch: &str, path: &str) -> Option<String> {
    let engine = f.cp.engine("alpha").unwrap();
    let node = engine
        .vcs()
        .read(
            &Revision::BranchWorkspace(branch.into()),
            &VirtualPath::new(path).unwrap(),
        )
        .unwrap();
    node.map(|n| n.content_str().unwrap().to_string())
}

const ROOT: &str = "/p/alpha/dav/main/";

#[tokio::test]
async fn propfind_root_lists_exactly_the_four_tiers() {
    let f = fixture();
    let k = key(&f, "dev", Role::Dev);
    let r = dav(
        &f,
        "PROPFIND",
        ROOT,
        Some(&k),
        &[("depth", "1")],
        Vec::new(),
    )
    .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS, "{}", r.body);
    let mut got = hrefs(&r.body);
    got.sort();
    let want: Vec<String> = ["", "core/", "episodic/", "procedural/", "semantic/"]
        .iter()
        .map(|s| format!("{ROOT}{s}"))
        .collect();
    assert_eq!(got, want, "{}", r.body);
}

#[tokio::test]
async fn put_then_get_and_engine_read_return_the_body() {
    let f = fixture();
    let k = key(&f, "dev", Role::Dev);
    assert!(put(&f, "/p/alpha/dav/main/semantic/a.md", &k, "# alpha")
        .await
        .is_success());
    let r = get(&f, "/p/alpha/dav/main/semantic/a.md", &k).await;
    assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, "# alpha"));
    assert_eq!(
        engine_read(&f, "main", "/semantic/a.md").as_deref(),
        Some("# alpha")
    );
}

#[tokio::test]
async fn auth_rules_bot_write_missing_key_and_wrong_project() {
    let f = fixture();
    let bot = key(&f, "bot", Role::Bot);
    assert_eq!(
        put(&f, "/p/alpha/dav/main/semantic/b.md", &bot, "x").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(engine_read(&f, "main", "/semantic/b.md"), None);
    let bot_read = dav(
        &f,
        "PROPFIND",
        ROOT,
        Some(&bot),
        &[("depth", "0")],
        Vec::new(),
    )
    .await;
    assert_eq!(bot_read.status, StatusCode::MULTI_STATUS, "bot may read");

    let anon = dav(&f, "PROPFIND", ROOT, None, &[("depth", "0")], Vec::new()).await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);
    assert!(anon.headers["www-authenticate"]
        .to_str()
        .unwrap()
        .starts_with("Basic"));

    let dev = key(&f, "dev", Role::Dev);
    let other = dav(
        &f,
        "PROPFIND",
        "/p/beta/dav/main/",
        Some(&dev),
        &[("depth", "0")],
        Vec::new(),
    )
    .await;
    assert_eq!(other.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn names_outside_tier_markdown_are_forbidden() {
    let f = fixture();
    let k = key(&f, "dev", Role::Dev);
    for bad in [
        ".obsidian/app.json",
        "notes/x.md",
        "semantic/img.png",
        "semantic/.hidden.md",
        "_templates/t.md",
    ] {
        let status = put(&f, &format!("{ROOT}{bad}"), &k, "x").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{bad}");
    }
    let engine = f.cp.engine("alpha").unwrap();
    assert!(engine
        .vcs()
        .list(&Revision::BranchWorkspace("main".into()), None)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn propfind_tier_lists_existing_files_and_move_delete_work() {
    let f = fixture();
    let k = key(&f, "dev", Role::Dev);
    let engine = f.cp.engine("alpha").unwrap();
    let node = mem_core::MemoryNode::new(
        VirtualPath::new("/semantic/rest.md").unwrap(),
        b"r".to_vec(),
    );
    engine.vcs().write_node("main", node).unwrap();
    let r = dav(
        &f,
        "PROPFIND",
        "/p/alpha/dav/main/semantic/",
        Some(&k),
        &[("depth", "1")],
        Vec::new(),
    )
    .await;
    assert!(
        hrefs(&r.body)
            .iter()
            .any(|h| h.ends_with("/semantic/rest.md")),
        "{}",
        r.body
    );

    let dest = "http://localhost/p/alpha/dav/main/semantic/moved.md";
    let mv = dav(
        &f,
        "MOVE",
        "/p/alpha/dav/main/semantic/rest.md",
        Some(&k),
        &[("destination", dest)],
        Vec::new(),
    )
    .await;
    assert!(mv.status.is_success(), "{} {}", mv.status, mv.body);
    assert_eq!(engine_read(&f, "main", "/semantic/rest.md"), None);
    assert_eq!(
        engine_read(&f, "main", "/semantic/moved.md").as_deref(),
        Some("r")
    );

    let del = dav(
        &f,
        "DELETE",
        "/p/alpha/dav/main/semantic/moved.md",
        Some(&k),
        &[],
        Vec::new(),
    )
    .await;
    assert!(del.status.is_success(), "{}", del.status);
    assert_eq!(
        get(&f, "/p/alpha/dav/main/semantic/moved.md", &k)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn idle_auto_commit_with_one_key_creates_one_commit_and_one_audit_row() {
    let f = fixture();
    let k = key(&f, "pm-alice", Role::Pm);
    put(&f, "/p/alpha/dav/main/semantic/p1.md", &k, "one").await;
    put(&f, "/p/alpha/dav/main/semantic/p2.md", &k, "two").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let log = f.cp.engine("alpha").unwrap().vcs().log("main", 10).unwrap();
    assert_eq!(log.len(), 2, "init + one webdav commit");
    assert_eq!(
        (log[0].author.as_str(), log[0].message.as_str()),
        ("pm-alice", "webdav sync: 2 change(s)")
    );
    let audit = f.cp.list_audit("alpha", 10).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].operation, "dav.commit");
    assert_eq!(
        audit[0].commit_id.as_deref(),
        Some(log[0].commit_id.to_hex().as_str())
    );
}

#[tokio::test]
async fn idle_auto_commit_with_two_keys_lists_both_authors() {
    let f = fixture();
    let a = key(&f, "b-dev", Role::Dev);
    let b = key(&f, "a-pm", Role::Pm);
    put(&f, "/p/alpha/dav/main/semantic/x.md", &a, "x").await;
    put(&f, "/p/alpha/dav/main/semantic/y.md", &b, "y").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let log = f.cp.engine("alpha").unwrap().vcs().log("main", 10).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].author, "webdav:a-pm,b-dev");
    assert_eq!(f.cp.list_audit("alpha", 10).unwrap().len(), 2);
}

#[tokio::test]
async fn branches_are_isolated_over_webdav() {
    let f = fixture();
    let k = key(&f, "dev", Role::Dev);
    let engine = f.cp.engine("alpha").unwrap();
    engine
        .vcs()
        .create_branch("draft/pm-inbox", &Revision::BranchHead("main".into()))
        .unwrap();
    put(&f, "/p/alpha/dav/main/semantic/m.md", &k, "main").await;
    put(&f, "/p/alpha/dav/draft~pm-inbox/semantic/d.md", &k, "draft").await;
    assert_eq!(
        engine_read(&f, "draft/pm-inbox", "/semantic/d.md").as_deref(),
        Some("draft")
    );
    assert_eq!(
        get(&f, "/p/alpha/dav/draft~pm-inbox/semantic/m.md", &k)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&f, "/p/alpha/dav/main/semantic/d.md", &k).await.status,
        StatusCode::NOT_FOUND
    );
    let encoded = get(&f, "/p/alpha/dav/draft%2Fpm-inbox/semantic/d.md", &k).await;
    assert_eq!(
        encoded.status,
        StatusCode::BAD_REQUEST,
        "use ~ for / in branch names"
    );
}

#[tokio::test]
async fn unknown_method_is_405_and_oversized_put_is_413() {
    let f = fixture();
    let k = key(&f, "dev", Role::Dev);
    let patch = dav(
        &f,
        "PATCH",
        "/p/alpha/dav/main/semantic/a.md",
        Some(&k),
        &[],
        b"x".to_vec(),
    )
    .await;
    assert_eq!(patch.status, StatusCode::METHOD_NOT_ALLOWED);
    let big = vec![b'a'; 8 * 1024 * 1024 + 1];
    let r = dav(
        &f,
        "PUT",
        "/p/alpha/dav/main/semantic/big.md",
        Some(&k),
        &[],
        big,
    )
    .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(engine_read(&f, "main", "/semantic/big.md"), None);
}

#[tokio::test]
async fn idle_timer_skips_commit_when_workspace_has_no_changes() {
    let f = fixture();
    let k = key(&f, "dev", Role::Dev);
    put(&f, "/p/alpha/dav/main/semantic/tmp.md", &k, "t").await;
    let del = dav(
        &f,
        "DELETE",
        "/p/alpha/dav/main/semantic/tmp.md",
        Some(&k),
        &[],
        Vec::new(),
    )
    .await;
    assert!(del.status.is_success());
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        f.cp.engine("alpha")
            .unwrap()
            .vcs()
            .log("main", 10)
            .unwrap()
            .len(),
        1
    );
    assert!(f.cp.list_audit("alpha", 10).unwrap().is_empty());
}

#[tokio::test]
async fn mkcol_rules_and_lock_permissions() {
    let f = fixture();
    let k = key(&f, "dev", Role::Dev);
    let bot = key(&f, "bot", Role::Bot);
    let mk = |uri: &'static str| dav(&f, "MKCOL", uri, Some(&k), &[], Vec::new());
    assert_eq!(
        mk("/p/alpha/dav/main/semantic/newdir").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        mk("/p/alpha/dav/main/semantic").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        mk("/p/alpha/dav/main/notes").await.status,
        StatusCode::FORBIDDEN
    );
    let probe = dav(
        &f,
        "PROPFIND",
        "/p/alpha/dav/main/semantic/newdir/",
        Some(&k),
        &[("depth", "0")],
        Vec::new(),
    )
    .await;
    assert_eq!(probe.status, StatusCode::MULTI_STATUS);

    let dest = "http://localhost/p/alpha/dav/main/semantic/newdir/n.md";
    put(&f, "/p/alpha/dav/main/semantic/n.md", &k, "n").await;
    let mv = dav(
        &f,
        "MOVE",
        "/p/alpha/dav/main/semantic/n.md",
        Some(&k),
        &[("destination", dest)],
        Vec::new(),
    )
    .await;
    assert!(mv.status.is_success(), "{}", mv.status);
    assert_eq!(
        engine_read(&f, "main", "/semantic/newdir/n.md").as_deref(),
        Some("n")
    );

    let lock_body = br#"<?xml version="1.0"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockinfo>"#.to_vec();
    let lock = dav(
        &f,
        "LOCK",
        "/p/alpha/dav/main/semantic/newdir/n.md",
        Some(&k),
        &[],
        lock_body.clone(),
    )
    .await;
    assert_eq!(lock.status, StatusCode::OK, "{}", lock.body);
    let bot_lock = dav(
        &f,
        "LOCK",
        "/p/alpha/dav/main/semantic/newdir/n.md",
        Some(&bot),
        &[],
        lock_body,
    )
    .await;
    assert_eq!(bot_lock.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn image_attachments_are_stored_raw_outside_core() {
    let f = fixture();
    let k = key(&f, "pm", Role::Pm);
    let png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00];
    let ok = dav(
        &f,
        "PUT",
        "/p/alpha/dav/main/semantic/_attachments/a.png",
        Some(&k),
        &[],
        png.clone(),
    )
    .await;
    assert_eq!(ok.status, StatusCode::CREATED);
    let vp = VirtualPath::new("/semantic/_attachments/a.png").unwrap();
    let rev = Revision::BranchWorkspace("main".into());
    let node =
        f.cp.engine("alpha")
            .unwrap()
            .vcs()
            .read(&rev, &vp)
            .unwrap()
            .unwrap();
    assert_eq!(node.content, png);
    assert_eq!(node.metadata.content_type, "image/png");
    let got = get(&f, "/p/alpha/dav/main/semantic/_attachments/a.png", &k).await;
    assert_eq!(got.status, StatusCode::OK);
    for denied in [
        "/p/alpha/dav/main/core/_attachments/a.png",
        "/p/alpha/dav/main/semantic/_attachments/a.svg",
        "/p/alpha/dav/main/semantic/_attachments/a.md.exe",
        "/p/alpha/dav/main/semantic/sub/_attachments/a.png",
    ] {
        let r = dav(&f, "PUT", denied, Some(&k), &[], png.clone()).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{denied}");
    }
    let dest = [(
        "destination",
        "http://localhost/p/alpha/dav/main/semantic/pics/",
    )];
    let mv = dav(
        &f,
        "MOVE",
        "/p/alpha/dav/main/semantic/_attachments/",
        Some(&k),
        &dest,
        Vec::new(),
    )
    .await;
    assert_eq!(
        mv.status,
        StatusCode::FORBIDDEN,
        "folder MOVE must not place images outside _attachments"
    );
    let moved = VirtualPath::new("/semantic/pics/a.png").unwrap();
    assert!(f
        .cp
        .engine("alpha")
        .unwrap()
        .vcs()
        .read(&rev, &moved)
        .unwrap()
        .is_none());
}
