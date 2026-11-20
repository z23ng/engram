//! S6 consolidation core: episodic notes -> LLM -> validated notes on a review branch.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use mem_core::{MemoryNode, Revision, VirtualPath};
use mem_search::{
    consolidate, note_is_valid, ConsolidateLlmConfig, ConsolidateStatus, MemSearchEngine,
    ProviderBackend,
};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};

fn repo() -> (tempfile::TempDir, MemSearchEngine) {
    let dir = tempfile::tempdir().unwrap();
    let vcs = MemVcs::new(SqliteStore::open(dir.path().join("m.db")).unwrap());
    vcs.init().unwrap();
    (dir, MemSearchEngine::new_bm25_only(vcs))
}

fn write(e: &MemSearchEngine, path: &str, body: &str) {
    let node = MemoryNode::new(VirtualPath::new(path).unwrap(), body.as_bytes().to_vec());
    e.vcs().write_node("main", node).unwrap();
}

fn read(e: &MemSearchEngine, rev: Revision, path: &str) -> Option<String> {
    e.vcs()
        .read(&rev, &VirtualPath::new(path).unwrap())
        .unwrap()
        .map(|n| String::from_utf8(n.content).unwrap())
}

fn llm(base_url: &str) -> ConsolidateLlmConfig {
    ConsolidateLlmConfig {
        backend: ProviderBackend::Ollama,
        base_url: base_url.to_string(),
        model: "test-llm".into(),
        api_key: None,
    }
}

/// Mock OpenAI-compatible chat endpoint: answers each request with `reply` as message content
/// and records request bodies.
fn mock_llm(reply: &str) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (reply, log) = (reply.to_string(), seen.clone());
    thread::spawn(move || {
        for stream in listener.incoming() {
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
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            log.lock().unwrap().push(String::from_utf8(body).unwrap());
            let resp = serde_json::json!({"choices": [{"index": 0, "message": {"role": "assistant", "content": reply}}]})
                .to_string();
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", resp.len());
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(resp.as_bytes()).unwrap();
        }
    });
    (url, seen)
}

fn notes(items: &[(&str, &str)]) -> String {
    let notes: Vec<_> = items
        .iter()
        .map(|(p, c)| serde_json::json!({"path": p, "content": c}))
        .collect();
    serde_json::json!({ "notes": notes }).to_string()
}

fn seed(e: &MemSearchEngine) {
    write(
        e,
        "/semantic/routing.md",
        "# Channel routing\n\nRoute by amount.",
    );
    write(
        e,
        "/episodic/2026-11-20.md",
        "# Channel routing incident\n\nwingpay timed out; fallback to kbz.",
    );
    e.vcs().commit("main", "t", "seed").unwrap();
}

#[test]
fn note_validation_is_deny_by_default() {
    let ok = "# Note\n\nbody";
    for good in ["/semantic/a.md", "/procedural/ops/deploy.md"] {
        assert!(note_is_valid(good, ok), "{good}");
    }
    for bad in [
        "/core/a.md",
        "/episodic/a.md",
        "/semantic/../core/a.md",
        "/semantic/a.txt",
        "/semantic/_attachments/a.md",
        "/semantic/_templates/a.md",
        "semantic/a.md",
    ] {
        assert!(!note_is_valid(bad, ok), "{bad}");
    }
    assert!(!note_is_valid("/semantic/a.md", "  "), "empty content");
    assert!(
        !note_is_valid("/semantic/a.md", &"x".repeat(256 * 1024 + 1)),
        "oversize"
    );
}

#[test]
fn a_run_writes_valid_notes_to_a_review_branch_and_leaves_main_alone() {
    let (_d, e) = repo();
    seed(&e);
    let main_before = e.vcs().head_commit("main").unwrap();
    let reply = notes(&[
        (
            "/semantic/routing.md",
            "# Channel routing\n\nRoute by amount. Fall back to kbz when wingpay times out.",
        ),
        (
            "/procedural/fallback.md",
            "# Fallback SOP\n\n1. Switch to kbz.",
        ),
        ("/core/agent.md", "# never"),
    ]);
    let (url, seen) = mock_llm(&reply);

    let out = consolidate(&e, &llm(&url), None, "consolidate/test").unwrap();
    assert_eq!(out.status, ConsolidateStatus::Succeeded);
    assert_eq!(
        (out.files_in, out.notes_written, out.notes_dropped),
        (1, 2, 1)
    );
    assert_eq!(out.source_commit, main_before);
    assert_eq!(out.branch.as_deref(), Some("consolidate/test"));

    let prompt = seen.lock().unwrap().join("\n");
    assert!(
        prompt.contains("wingpay timed out") && prompt.contains("Route by amount"),
        "episodic and context notes are sent"
    );

    let head = Revision::BranchHead("consolidate/test".into());
    assert!(read(&e, head.clone(), "/semantic/routing.md")
        .unwrap()
        .contains("Fall back to kbz"));
    assert!(read(&e, head.clone(), "/procedural/fallback.md").is_some());
    assert!(read(&e, head, "/core/agent.md").is_none());
    let log = e.vcs().log("consolidate/test", 1).unwrap();
    assert_eq!(log[0].author, "consolidator:test-llm");
    assert_eq!(Some(log[0].commit_id), out.commit_id);
    assert_eq!(
        e.vcs().head_commit("main").unwrap(),
        main_before,
        "main is never changed"
    );
    assert_eq!(
        read(
            &e,
            Revision::BranchHead("main".into()),
            "/semantic/routing.md"
        )
        .unwrap(),
        "# Channel routing\n\nRoute by amount."
    );
}

#[test]
fn only_episodic_changes_since_the_last_source_commit_are_read() {
    let (_d, e) = repo();
    seed(&e);
    let first = e.vcs().head_commit("main").unwrap();
    // Unreachable LLM: a skipped run must not call it.
    let out = consolidate(
        &e,
        &llm("http://127.0.0.1:9/v1"),
        Some(&first),
        "consolidate/none",
    )
    .unwrap();
    assert_eq!(
        (out.status, out.files_in, out.branch),
        (ConsolidateStatus::Skipped, 0, None)
    );

    write(
        &e,
        "/episodic/2026-11-21.md",
        "# Payout retry\n\nretry twice",
    );
    write(&e, "/semantic/other.md", "# Other\n\nnot episodic");
    e.vcs().commit("main", "t", "more").unwrap();
    let (url, seen) = mock_llm(&notes(&[("/procedural/retry.md", "# Retry\n\ntwice")]));
    let out = consolidate(&e, &llm(&url), Some(&first), "consolidate/two").unwrap();
    assert_eq!(
        (out.status, out.files_in),
        (ConsolidateStatus::Succeeded, 1)
    );
    let prompt = seen.lock().unwrap().join("\n");
    assert!(
        prompt.contains("retry twice") && !prompt.contains("wingpay timed out"),
        "{prompt}"
    );
}

#[test]
fn an_unreachable_llm_fails_and_creates_no_branch() {
    let (_d, e) = repo();
    seed(&e);
    assert!(consolidate(&e, &llm("http://127.0.0.1:9/v1"), None, "consolidate/fail").is_err());
    assert!(e.vcs().head_commit("consolidate/fail").is_err());
}

#[test]
fn unparsable_output_fails_and_identical_output_is_no_changes() {
    let (_d, e) = repo();
    seed(&e);
    let (url, _) = mock_llm("I could not do it.");
    assert!(consolidate(&e, &llm(&url), None, "consolidate/bad").is_err());
    assert!(e.vcs().head_commit("consolidate/bad").is_err());

    let (url, _) = mock_llm(&format!(
        "```json\n{}\n```",
        notes(&[(
            "/semantic/routing.md",
            "# Channel routing\n\nRoute by amount."
        )])
    ));
    let out = consolidate(&e, &llm(&url), None, "consolidate/same").unwrap();
    assert_eq!(
        (out.status, out.notes_written, out.branch),
        (ConsolidateStatus::NoChanges, 0, None)
    );
    assert!(e.vcs().head_commit("consolidate/same").is_err());
}
