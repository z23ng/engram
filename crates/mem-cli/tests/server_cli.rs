use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

fn mem(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mem"))
        .args(args)
        .output()
        .unwrap();
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

#[test]
fn admin_manages_projects_and_keys() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    assert!(mem(&["admin", "project", "create", "alpha", "--data", data]).0);
    assert!(!mem(&["admin", "project", "create", "../bad", "--data", data]).0);
    assert!(mem(&["admin", "project", "list", "--data", data])
        .1
        .contains("alpha"));

    let (ok, out) = mem(&[
        "admin",
        "key",
        "create",
        "--name",
        "ci",
        "--role",
        "bot",
        "--projects",
        "alpha",
        "--data",
        data,
    ]);
    assert!(ok && out.contains("egm_"), "{out}");
    assert!(!mem(&["admin", "key", "create", "--name", "x", "--role", "root", "--data", data]).0);

    let list = mem(&["admin", "key", "list", "--data", data]).1;
    assert!(
        list.contains("ci") && list.contains("bot") && list.contains("alpha"),
        "{list}"
    );
    assert!(
        !list.contains("egm_"),
        "list must not print plaintext keys: {list}"
    );
    assert!(mem(&["admin", "key", "revoke", "1", "--data", data]).0);
    assert!(mem(&["admin", "key", "list", "--data", data])
        .1
        .contains("revoked"));
}

fn http_status(addr: &str, key: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    let req = format!(
        "GET /api/v1/projects/alpha/read?path=/core/a.md HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {key}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    resp.lines().next().unwrap_or_default().to_string()
}

/// Starts `mem serve` and returns the child, the printed admin key (if any), and the address.
fn start_serve(data: &str) -> (Child, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mem"))
        .args([
            "--provider",
            "disabled",
            "serve",
            "--data",
            data,
            "--listen",
            "127.0.0.1:0",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut key = String::new();
    let mut addr = String::new();
    while addr.is_empty() {
        let line = lines.next().expect("serve exited early").unwrap();
        if let Some(k) = line.split_whitespace().find(|w| w.starts_with("egm_")) {
            key = k.to_string();
        }
        if let Some(a) = line.strip_prefix("listening on ") {
            addr = a.trim().to_string();
        }
    }
    (child, key, addr)
}

#[test]
fn serve_prints_one_time_admin_key_and_enforces_auth() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    let (mut child, key, addr) = start_serve(data);
    assert!(!key.is_empty(), "no admin key printed");
    assert!(http_status(&addr, "egm_wrong").contains("401"));
    assert!(
        http_status(&addr, &key).contains("404"),
        "admin key on missing project"
    );
    child.kill().unwrap();
    child.wait().unwrap();

    let second = mem(&["admin", "key", "list", "--data", data]).1;
    assert!(
        second.contains("admin") && !second.contains("egm_"),
        "{second}"
    );
}

#[test]
fn serve_bootstraps_admin_key_when_admin_cli_created_the_database_first() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    assert!(mem(&["admin", "project", "create", "alpha", "--data", data]).0);
    let (mut child, key, _) = start_serve(data);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        !key.is_empty(),
        "no admin key printed when no active admin key exists"
    );

    let (mut child, key, _) = start_serve(data);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        key.is_empty(),
        "second start must not print another admin key"
    );
}

#[test]
fn admin_audit_lists_rows_of_a_project() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    assert!(mem(&["admin", "project", "create", "alpha", "--data", data]).0);
    let (ok, out) = mem(&["admin", "audit", "--project", "alpha", "--data", data]);
    assert!(ok, "{out}");
    let bad_limit = [
        "admin",
        "audit",
        "--project",
        "alpha",
        "--limit",
        "x",
        "--data",
        data,
    ];
    assert!(!mem(&bad_limit).0);
}

#[test]
fn admin_reindex_rebuilds_a_project_index() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    assert!(mem(&["admin", "project", "create", "alpha", "--data", data]).0);
    let db = format!("{data}/projects/alpha.db");
    let (ok, out) = mem(&["--db", &db, "write", "/semantic/a.md", "# A\n\nrouting"]);
    assert!(ok, "{out}");
    let (ok, out) = mem(&["admin", "reindex", "--project", "alpha", "--data", data]);
    assert!(ok, "{out}");
    assert!(out.contains("reindexed alpha: 1 file(s)"), "{out}");
    assert!(!mem(&["admin", "reindex", "--project", "nope", "--data", data]).0);
    assert!(
        !mem(&["admin", "reindex", "--data", data]).0,
        "--project is required"
    );
}

#[test]
fn serve_logs_the_search_mode_of_each_project() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    assert!(mem(&["admin", "project", "create", "alpha", "--data", data]).0);
    let mut child = Command::new(env!("CARGO_BIN_EXE_mem"))
        .args([
            "--provider",
            "ollama",
            "serve",
            "--data",
            data,
            "--listen",
            "127.0.0.1:0",
        ])
        .env("MEM_EMBEDDING_BASE_URL", "http://127.0.0.1:9")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut key = String::new();
    let addr = loop {
        let line = lines.next().expect("serve exited early").unwrap();
        if let Some(k) = line.split_whitespace().find(|w| w.starts_with("egm_")) {
            key = k.to_string();
        }
        if let Some(a) = line.strip_prefix("listening on ") {
            break a.trim().to_string();
        }
    };
    http_status(&addr, &key); // opens the alpha engine
    child.kill().unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    child.wait().unwrap();
    assert!(
        err.contains("project alpha: search model disabled"),
        "{err}"
    );
    assert!(err.contains("unreachable"), "{err}");
    assert!(err.contains("consolidation LLM: ollama "), "{err}");
}

/// Mock chat endpoint returning one procedural note for every request.
fn mock_llm() -> String {
    let notes = serde_json::json!({"notes": [{"path": "/procedural/retry.md", "content": "# Retry\n\ntwice"}]});
    mock_llm_seq(vec![notes.to_string()])
}

/// Answers request `i` with message content `replies[i % len]`.
fn mock_llm_seq(replies: Vec<String>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
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
            let content = &replies[i % replies.len()];
            let resp =
                serde_json::json!({"choices": [{"message": {"content": content}}]}).to_string();
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

#[test]
fn admin_consolidate_and_schedule() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    assert!(mem(&["admin", "project", "create", "alpha", "--data", data]).0);
    let db = format!("{data}/projects/alpha.db");
    assert!(
        mem(&[
            "--db",
            &db,
            "write",
            "/episodic/a.md",
            "# Payout retry\n\nretry twice"
        ])
        .0
    );
    assert!(mem(&["--db", &db, "commit", "-m", "log"]).0);

    let out = Command::new(env!("CARGO_BIN_EXE_mem"))
        .args([
            "--provider",
            "ollama",
            "admin",
            "consolidate",
            "--project",
            "alpha",
            "--data",
            data,
        ])
        .env("MEM_LLM_BASE_URL", mock_llm())
        .env("MEM_LLM_MODEL", "test-llm")
        .env("MEM_EMBEDDING_BASE_URL", "http://127.0.0.1:9")
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("consolidate alpha: succeeded") && text.contains("branch consolidate/"),
        "{text}"
    );

    assert!(mem(&["admin", "project", "schedule", "alpha", "--every", "24", "--data", data]).0);
    assert!(mem(&["admin", "project", "schedule", "alpha", "--every", "off", "--data", data]).0);
    assert!(!mem(&["admin", "project", "schedule", "alpha", "--every", "0", "--data", data]).0);
    assert!(
        !mem(&["admin", "consolidate", "--data", data]).0,
        "--project is required"
    );
}

#[test]
fn a_partial_consolidation_exits_zero_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    assert!(mem(&["admin", "project", "create", "alpha", "--data", data]).0);
    let db = format!("{data}/projects/alpha.db");
    for name in ["a", "b"] {
        let mut log = format!("# Journal {name}\n\n");
        while log.len() < 30_000 {
            log.push_str("payout retried after timeout.\n");
        }
        assert!(mem(&["--db", &db, "write", &format!("/episodic/{name}.md"), &log]).0);
    }
    assert!(mem(&["--db", &db, "commit", "-m", "log"]).0);
    let good = serde_json::json!({"notes": [{"path": "/procedural/retry.md", "content": "# Retry\n\ntwice"}]});
    let out = Command::new(env!("CARGO_BIN_EXE_mem"))
        .args([
            "--provider",
            "ollama",
            "admin",
            "consolidate",
            "--project",
            "alpha",
            "--data",
            data,
        ])
        .env(
            "MEM_LLM_BASE_URL",
            mock_llm_seq(vec![good.to_string(), "not json".into()]),
        )
        .env("MEM_LLM_MODEL", "test-llm")
        .env("MEM_EMBEDDING_BASE_URL", "http://127.0.0.1:9")
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notes were committed: {text}");
    assert!(
        text.contains("warning: 1 batch(es) skipped") && text.contains("/episodic/b.md"),
        "{text}"
    );
}

#[test]
fn a_schedule_without_a_consolidation_llm_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    assert!(mem(&["admin", "project", "create", "alpha", "--data", data]).0);
    let (ok, text) = mem(&[
        "--provider",
        "disabled",
        "admin",
        "project",
        "schedule",
        "alpha",
        "--every",
        "24",
        "--data",
        data,
    ]);
    assert!(!ok && text.contains("no consolidation LLM"), "{text}");
    assert!(
        mem(&[
            "--provider",
            "disabled",
            "admin",
            "project",
            "schedule",
            "alpha",
            "--every",
            "off",
            "--data",
            data
        ])
        .0
    );
}

fn project_with_log(data: &str) {
    assert!(mem(&["admin", "project", "create", "alpha", "--data", data]).0);
    let db = format!("{data}/projects/alpha.db");
    assert!(
        mem(&[
            "--db",
            &db,
            "write",
            "/episodic/a.md",
            "# Payout retry\n\nretry twice"
        ])
        .0
    );
    assert!(mem(&["--db", &db, "commit", "-m", "log"]).0);
}

fn consolidate_cmd(data: &str, llm: &str, extra: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mem"));
    cmd.args([
        "--provider",
        "ollama",
        "admin",
        "consolidate",
        "--project",
        "alpha",
        "--data",
        data,
    ])
    .args(extra)
    .env("MEM_LLM_BASE_URL", llm)
    .env("MEM_LLM_MODEL", "test-llm")
    .env("MEM_EMBEDDING_BASE_URL", "http://127.0.0.1:9");
    cmd
}

#[test]
fn consolidate_full_reads_every_episodic_file_again() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    project_with_log(data);
    let llm = mock_llm();
    let run = |extra: &[&str]| {
        let out = consolidate_cmd(data, &llm, extra).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    assert!(run(&[]).contains("succeeded"));
    assert!(run(&[]).contains("skipped"));
    let full = run(&["--full"]);
    assert!(
        full.contains("succeeded") && full.contains("files 1"),
        "{full}"
    );
}

#[test]
fn ctrl_c_marks_a_cli_consolidation_run_failed() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    project_with_log(data);
    // An LLM that accepts the request and never answers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let llm = format!("http://{}/v1", listener.local_addr().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(120));
        drop(stream);
    });
    let child = consolidate_cmd(data, &llm, &[])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    rx.recv_timeout(std::time::Duration::from_secs(30))
        .expect("the run reached the LLM");
    assert!(Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success());
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success() && text.contains("interrupted"),
        "{text}"
    );
    let db = rusqlite::Connection::open(format!("{data}/server.db")).unwrap();
    let (status, error): (String, String) = db
        .query_row(
            "SELECT status, error FROM consolidation_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((status.as_str(), error.as_str()), ("failed", "interrupted"));
}
