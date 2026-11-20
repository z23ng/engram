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
