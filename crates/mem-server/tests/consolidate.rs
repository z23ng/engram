//! S6 consolidation runs: REST trigger, run bookkeeping, permissions, schedule.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use mem_search::{ConsolidateLlmConfig, MemSearchEngine, ProviderBackend};
use mem_server::{router, ControlPlane, Role};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Fixture {
    _dir: tempfile::TempDir,
    cp: Arc<ControlPlane>,
    app: Router,
    admin: String,
}

fn llm(base_url: &str) -> ConsolidateLlmConfig {
    ConsolidateLlmConfig {
        backend: ProviderBackend::Ollama,
        base_url: base_url.into(),
        model: "test-llm".into(),
        api_key: None,
    }
}

/// Mock chat endpoint that waits `delay` and then returns one new procedural note.
fn mock_llm(delay: Duration) -> String {
    let notes = json!({"notes": [{"path": "/procedural/retry.md", "content": "# Retry\n\ntwice"}]});
    mock_llm_seq(delay, vec![notes.to_string()])
}

/// Answers request `i` with message content `replies[i % len]`.
fn mock_llm_seq(delay: Duration, replies: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            reader.read_exact(&mut vec![0; len]).unwrap();
            std::thread::sleep(delay);
            let content = &replies[i % replies.len()];
            let resp = json!({"choices": [{"message": {"content": content}}]}).to_string();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                resp.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    url
}

fn fixture(llm_url: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let (cp, _) = ControlPlane::open(dir.path(), Box::new(MemSearchEngine::new_bm25_only)).unwrap();
    let cp = cp.with_llm_config(llm(llm_url));
    cp.create_project("alpha").unwrap();
    cp.create_project("beta").unwrap();
    let admin = cp.create_key("root", Role::Admin, None, None).unwrap().1;
    let cp = Arc::new(cp);
    Fixture {
        _dir: dir,
        app: router(cp.clone()),
        cp,
        admin,
    }
}

async fn call(
    f: &Fixture,
    method: &str,
    uri: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"));
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

async fn seed(f: &Fixture) {
    let body = json!({"path": "/episodic/2026-11-20.md", "content": "# Payout retry\n\nretry twice", "commit_message": "log"});
    let (s, r) = call(
        f,
        "POST",
        "/api/v1/projects/alpha/write",
        &f.admin,
        Some(body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
}

/// Waits until the newest run is no longer `running` and returns it.
async fn finished_run(f: &Fixture) -> Value {
    for _ in 0..100 {
        let (s, r) = call(
            f,
            "GET",
            "/api/v1/projects/alpha/consolidate/runs",
            &f.admin,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{r}");
        if r["runs"][0]["status"] != "running" {
            return r["runs"][0].clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run did not finish");
}

const RUN: &str = "/api/v1/projects/alpha/consolidate";

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_run_commits_a_review_branch_and_records_the_run() {
    let f = fixture(&mock_llm(Duration::ZERO));
    seed(&f).await;
    let (s, r) = call(&f, "POST", RUN, &f.admin, None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    assert!(r["run_id"].as_i64().is_some(), "{r}");

    let run = finished_run(&f).await;
    assert_eq!(
        (
            run["status"].as_str(),
            run["trigger"].as_str(),
            run["requested_by"].as_str()
        ),
        (Some("succeeded"), Some("manual"), Some("root")),
        "{run}"
    );
    assert_eq!(
        (run["files_in"].as_u64(), run["notes_written"].as_u64()),
        (Some(1), Some(1)),
        "{run}"
    );
    assert_eq!(run["batches_failed"], 0, "{run}");
    let branch = run["branch"].as_str().unwrap();
    assert!(branch.starts_with("consolidate/"), "{run}");
    let engine = f.cp.engine("alpha").unwrap();
    assert_eq!(
        engine.vcs().log(branch, 1).unwrap()[0].author,
        "consolidator:test-llm"
    );
    let audit: Vec<String> =
        f.cp.list_audit("alpha", 10)
            .unwrap()
            .into_iter()
            .map(|a| a.operation)
            .collect();
    assert_eq!(audit[0], "consolidate.run");

    // Nothing new on /episodic/ since the last successful source commit.
    let (s, _) = call(&f, "POST", RUN, &f.admin, None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let run = finished_run(&f).await;
    assert_eq!(
        (run["status"].as_str(), run["branch"].as_str()),
        (Some("skipped"), None),
        "{run}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_run_records_the_error_and_creates_no_branch() {
    let f = fixture("http://127.0.0.1:9/v1");
    seed(&f).await;
    assert_eq!(
        call(&f, "POST", RUN, &f.admin, None).await.0,
        StatusCode::ACCEPTED
    );
    let run = finished_run(&f).await;
    assert_eq!(run["status"], "failed", "{run}");
    assert!(
        !run["error"].as_str().unwrap_or_default().is_empty(),
        "{run}"
    );
    assert!(run["branch"].is_null(), "{run}");
    let audit: Vec<String> =
        f.cp.list_audit("alpha", 10)
            .unwrap()
            .into_iter()
            .map(|a| a.operation)
            .collect();
    assert_eq!(audit[0], "consolidate.run", "failed runs are audited too");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_run_per_project_at_a_time() {
    let f = fixture(&mock_llm(Duration::from_millis(600)));
    seed(&f).await;
    assert_eq!(
        call(&f, "POST", RUN, &f.admin, None).await.0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        call(&f, "POST", RUN, &f.admin, None).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(finished_run(&f).await["status"], "succeeded");
    assert_eq!(
        call(
            &f,
            "POST",
            "/api/v1/projects/beta/consolidate",
            &f.admin,
            None
        )
        .await
        .0,
        StatusCode::ACCEPTED,
        "other projects are not blocked"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_admin_keys_manage_consolidation() {
    let f = fixture("http://127.0.0.1:9/v1");
    for role in [Role::Dev, Role::Pm, Role::Bot] {
        let k =
            f.cp.create_key("k", role, Some(vec!["alpha".into()]), None)
                .unwrap()
                .1;
        assert_eq!(
            call(&f, "POST", RUN, &k, None).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &f,
                "GET",
                "/api/v1/projects/alpha/consolidate/runs",
                &k,
                None
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let body = Some(json!({"every_hours": 24}));
        assert_eq!(
            call(
                &f,
                "PUT",
                "/api/v1/projects/alpha/consolidate/schedule",
                &k,
                body
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_schedule_picks_only_due_projects() {
    let f = fixture(&mock_llm(Duration::ZERO));
    let uri = "/api/v1/projects/alpha/consolidate/schedule";
    let (s, r) = call(&f, "PUT", uri, &f.admin, Some(json!({"every_hours": 24}))).await;
    assert_eq!(
        (s, r["every_hours"].as_u64()),
        (StatusCode::OK, Some(24)),
        "{r}"
    );
    assert_eq!(
        call(&f, "PUT", uri, &f.admin, Some(json!({"every_hours": 0})))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (_, runs) = call(
        &f,
        "GET",
        "/api/v1/projects/alpha/consolidate/runs",
        &f.admin,
        None,
    )
    .await;
    assert_eq!(runs["every_hours"], 24, "{runs}");

    let now = chrono::Utc::now().timestamp_millis();
    assert_eq!(
        f.cp.due_consolidations(now).unwrap(),
        vec!["alpha".to_string()],
        "never ran: due"
    );
    seed(&f).await;
    call(&f, "POST", RUN, &f.admin, None).await;
    let finished = finished_run(&f).await["finished_at_ms"].as_i64().unwrap();
    const HOUR: i64 = 3_600_000;
    assert!(f.cp.due_consolidations(finished + HOUR).unwrap().is_empty());
    assert_eq!(
        f.cp.due_consolidations(finished + 24 * HOUR).unwrap(),
        vec!["alpha".to_string()]
    );

    call(&f, "PUT", uri, &f.admin, Some(json!({"every_hours": null}))).await;
    assert!(
        f.cp.due_consolidations(finished + 48 * HOUR)
            .unwrap()
            .is_empty(),
        "off"
    );
}

#[test]
fn an_older_server_db_is_migrated() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = rusqlite::Connection::open(dir.path().join("server.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE projects (slug TEXT PRIMARY KEY, created_at INTEGER NOT NULL);
             CREATE TABLE consolidation_runs (
                 id INTEGER PRIMARY KEY, project TEXT NOT NULL, trigger TEXT NOT NULL,
                 requested_by TEXT NOT NULL, status TEXT NOT NULL, source_commit TEXT, branch TEXT,
                 files_in INTEGER NOT NULL DEFAULT 0, notes_written INTEGER NOT NULL DEFAULT 0,
                 notes_dropped INTEGER NOT NULL DEFAULT 0, error TEXT, started_at_ms INTEGER NOT NULL,
                 finished_at_ms INTEGER);
             INSERT INTO projects VALUES ('alpha', 1);
             INSERT INTO consolidation_runs (project, trigger, requested_by, status, started_at_ms, finished_at_ms)
             VALUES ('alpha', 'manual', 'root', 'failed', 1, 2);",
        )
        .unwrap();
    }
    for _ in 0..2 {
        let (cp, _) =
            ControlPlane::open(dir.path(), Box::new(MemSearchEngine::new_bm25_only)).unwrap();
        let cp = cp.with_llm_config(llm("http://127.0.0.1:9/v1"));
        let runs = cp.list_consolidations("alpha", 10).unwrap();
        assert_eq!((runs.len(), runs[0].batches_failed), (1, 0));
        cp.set_consolidate_schedule("alpha", Some(6)).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn files_of_skipped_batches_are_retried_by_the_next_run() {
    let good = json!({"notes": [{"path": "/procedural/retry.md", "content": "# Retry\n\ntwice"}]})
        .to_string();
    let f = fixture(&mock_llm_seq(
        Duration::ZERO,
        vec![good.clone(), "not json".into(), good],
    ));
    for name in ["a", "b"] {
        let mut log = format!("# Journal {name}\n\n");
        while log.len() < 30_000 {
            log.push_str("payout retried after timeout.\n");
        }
        let body =
            json!({"path": format!("/episodic/{name}.md"), "content": log, "commit_message": name});
        assert_eq!(
            call(
                &f,
                "POST",
                "/api/v1/projects/alpha/write",
                &f.admin,
                Some(body)
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    call(&f, "POST", RUN, &f.admin, None).await;
    let run = finished_run(&f).await;
    assert_eq!(
        (run["status"].as_str(), run["batches_failed"].as_i64()),
        (Some("succeeded"), Some(1)),
        "{run}"
    );
    assert_eq!(run["failed_paths"], json!(["/episodic/b.md"]), "{run}");

    call(&f, "POST", RUN, &f.admin, None).await;
    let run = finished_run(&f).await;
    assert_eq!(
        (run["status"].as_str(), run["files_in"].as_i64()),
        (Some("succeeded"), Some(1)),
        "{run}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_schedule_needs_an_llm_and_an_explicit_value() {
    let f = fixture(&mock_llm(Duration::ZERO));
    let uri = "/api/v1/projects/alpha/consolidate/schedule";
    assert_eq!(
        call(&f, "PUT", uri, &f.admin, Some(json!({}))).await.0,
        StatusCode::BAD_REQUEST,
        "missing every_hours"
    );

    let dir = tempfile::tempdir().unwrap();
    let (cp, _) = ControlPlane::open(dir.path(), Box::new(MemSearchEngine::new_bm25_only)).unwrap();
    cp.create_project("alpha").unwrap();
    let admin = cp.create_key("root", Role::Admin, None, None).unwrap().1;
    let cp = Arc::new(cp);
    let off = Fixture {
        _dir: dir,
        app: router(cp.clone()),
        cp,
        admin,
    };
    let (s, r) = call(
        &off,
        "PUT",
        uri,
        &off.admin,
        Some(json!({"every_hours": 24})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "no LLM configured: {r}");
    assert_eq!(
        call(
            &off,
            "PUT",
            uri,
            &off.admin,
            Some(json!({"every_hours": null}))
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_left_running_by_a_stopped_server_are_marked_failed() {
    let f = fixture(&mock_llm(Duration::ZERO));
    let id = f.cp.start_consolidation("alpha", "manual", "root").unwrap();
    assert_eq!(f.cp.fail_interrupted_runs().unwrap(), 1);
    let run = f.cp.consolidation_run(id).unwrap();
    assert_eq!(
        (run.status.as_str(), run.error.as_deref()),
        ("failed", Some("server restarted"))
    );
}

#[test]
fn mem_serve_clears_interrupted_runs_at_startup_without_delaying_the_schedule() {
    let f = fixture(&mock_llm(Duration::ZERO));
    f.cp.set_consolidate_schedule("alpha", Some(168)).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let started = now - 200 * 3_600_000; // a stopped server left this run 200 h ago
    let db = rusqlite::Connection::open(f._dir.path().join("server.db")).unwrap();
    db.execute(
        "INSERT INTO consolidation_runs (project, trigger, requested_by, status, started_at_ms)
         VALUES ('alpha', 'schedule', 'root', 'running', ?1)",
        [started],
    )
    .unwrap();
    let id = db.last_insert_rowid();

    let cp = f.cp.clone();
    let server = std::thread::spawn(move || mem_server::run_server(cp, "127.0.0.1:0"));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while f.cp.consolidation_run(id).unwrap().status == "running" {
        assert!(
            !server.is_finished(),
            "run_server stopped: {:?}",
            server.join()
        );
        assert!(
            std::time::Instant::now() < deadline,
            "run_server did not clear the row"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let run = f.cp.consolidation_run(id).unwrap();
    assert_eq!(
        (run.status.as_str(), run.finished_at_ms),
        ("failed", Some(started))
    );
    // Still due: the scheduler (first tick at startup) starts a new run at once.
    while f.cp.list_consolidations("alpha", 10).unwrap().len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the schedule was delayed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Let the scheduled run finish before the fixture deletes its directory.
    while f.cp.list_consolidations("alpha", 1).unwrap()[0].status == "running" {
        assert!(
            std::time::Instant::now() < deadline,
            "the scheduled run did not finish"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        f.cp.list_consolidations("alpha", 10).unwrap()[0].trigger,
        "schedule"
    );
}

#[test]
fn a_run_that_panics_inside_finish_consolidation_is_marked_aborted() {
    let dir = tempfile::tempdir().unwrap();
    ControlPlane::open(dir.path(), Box::new(MemSearchEngine::new_bm25_only))
        .unwrap()
        .0
        .create_project("alpha")
        .unwrap();
    // The run opens the project engine first; this factory panics there.
    let (cp, _) =
        ControlPlane::open(dir.path(), Box::new(|_| panic!("engine open failed"))).unwrap();
    let cp = cp.with_llm_config(llm("http://127.0.0.1:9/v1"));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cp.consolidate_now("alpha", "root")
    }));
    assert!(result.is_err(), "the run panicked");
    let run = &cp.list_consolidations("alpha", 1).unwrap()[0];
    assert_eq!(
        (run.status.as_str(), run.error.as_deref()),
        ("failed", Some("run aborted"))
    );
}
