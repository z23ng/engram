//! S6 consolidation core: episodic notes -> LLM -> validated notes on a review branch.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use mem_core::{MemoryNode, Revision, VirtualPath};
use mem_search::{
    consolidate, consolidate_retry, note_is_valid, ConsolidateLlmConfig, ConsolidateStatus,
    MemSearchEngine, ProviderBackend,
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
    mock_llm_seq(&[reply])
}

/// Like `mock_llm`, but answers request `i` with `replies[i % replies.len()]`.
fn mock_llm_seq(replies: &[&str]) -> (String, Arc<Mutex<Vec<String>>>) {
    let replies: Vec<String> = replies.iter().map(|r| r.to_string()).collect();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let reply = &replies[i % replies.len()];
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

/// Prompt text a mock request carried (system + user content).
fn user_text(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    v["messages"][1]["content"].as_str().unwrap().to_string()
}

#[test]
fn a_large_episodic_file_is_split_and_large_context_notes_are_not_sent() {
    let (_d, e) = repo();
    let mut log = String::from("# Payout journal\n\nSTART-MARKER\n");
    while log.len() < 60_000 {
        log.push_str("payout to wingpay retried after timeout; settled next cycle.\n");
    }
    log.push_str("END-MARKER\n");
    write(&e, "/episodic/journal.md", &log);
    write(
        &e,
        "/semantic/payout-big.md",
        &format!(
            "# Payout journal\n\nBIG-NOTE\n{}",
            "payout settlement rules. ".repeat(400)
        ),
    );
    e.vcs().commit("main", "t", "seed").unwrap();
    let (url, seen) = mock_llm(&notes(&[("/procedural/payout.md", "# Payout\n\nretry")]));

    let out = consolidate(&e, &llm(&url), None, "consolidate/big").unwrap();
    assert_eq!(
        (out.status, out.files_in),
        (ConsolidateStatus::Succeeded, 1)
    );
    let prompts: Vec<String> = seen.lock().unwrap().iter().map(|b| user_text(b)).collect();
    assert!(
        prompts.len() >= 2,
        "a 60 kB source cannot fit one 48 kB prompt: {}",
        prompts.len()
    );
    assert!(
        prompts.iter().all(|p| p.len() <= 48_000),
        "{:?}",
        prompts.iter().map(String::len).collect::<Vec<_>>()
    );
    let all = prompts.join("\n");
    assert!(
        all.contains("START-MARKER") && all.contains("END-MARKER"),
        "every part is sent"
    );
    assert!(
        !all.contains("BIG-NOTE"),
        "context notes over 8 kB are never sent truncated"
    );
}

#[test]
fn llm_errors_name_the_consolidation_llm() {
    let (_d, e) = repo();
    seed(&e);
    let err = consolidate(&e, &llm("http://127.0.0.1:9/v1"), None, "consolidate/x").unwrap_err();
    assert!(err.to_string().contains("consolidation LLM"), "{err}");
}

#[test]
fn llm_requests_cap_the_output_length() {
    let (_d, e) = repo();
    seed(&e);
    let (url, seen) = mock_llm(&notes(&[("/procedural/x.md", "# X\n\ny")]));
    consolidate(&e, &llm(&url), None, "consolidate/cap").unwrap();
    let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0]).unwrap();
    assert_eq!(
        body["max_tokens"], 8192,
        "a runaway generation must end: {body}"
    );
}

#[test]
fn a_bad_batch_is_skipped_and_counted_and_good_batches_are_committed() {
    let (_d, e) = repo();
    for name in ["a", "b"] {
        let mut log = format!("# Journal {name}\n\n");
        while log.len() < 30_000 {
            log.push_str("payout retried after timeout.\n");
        }
        write(&e, &format!("/episodic/{name}.md"), &log);
    }
    e.vcs().commit("main", "t", "seed").unwrap();
    let good = notes(&[("/procedural/retry.md", "# Retry\n\ntwice")]);
    let (url, seen) = mock_llm_seq(&[
        &good,
        "{\"notes\": [{\"path\": \"/semantic/x.md\", \"content\": \"cut",
    ]);

    let out = consolidate(&e, &llm(&url), None, "consolidate/partial").unwrap();
    assert_eq!(seen.lock().unwrap().len(), 2, "two batches");
    assert_eq!(
        (out.status, out.notes_written, out.batches_failed),
        (ConsolidateStatus::Succeeded, 1, 1)
    );
    assert!(
        out.batch_errors[0].contains("batch 2 of 2"),
        "{:?}",
        out.batch_errors
    );
    assert!(e.vcs().head_commit("consolidate/partial").is_ok());
}

#[test]
fn the_prompt_limits_notes_per_batch() {
    let (_d, e) = repo();
    seed(&e);
    let (url, seen) = mock_llm(&notes(&[("/procedural/x.md", "# X\n\ny")]));
    consolidate(&e, &llm(&url), None, "consolidate/limits").unwrap();
    let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0]).unwrap();
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("at most 5 notes") && system.contains("4,000 characters"),
        "{system}"
    );
}

fn two_large_sources(e: &MemSearchEngine) {
    for name in ["a", "b"] {
        let mut log = format!("# Journal {name}\n\n");
        while log.len() < 30_000 {
            log.push_str("payout retried after timeout.\n");
        }
        write(e, &format!("/episodic/{name}.md"), &log);
    }
    e.vcs().commit("main", "t", "seed").unwrap();
}

#[test]
fn an_empty_episodic_file_is_no_changes_without_an_llm_call() {
    let (_d, e) = repo();
    write(&e, "/episodic/empty.md", "");
    e.vcs().commit("main", "t", "empty").unwrap();
    let out = consolidate(&e, &llm("http://127.0.0.1:9/v1"), None, "consolidate/empty").unwrap();
    assert_eq!(
        (out.status, out.files_in, out.branch),
        (ConsolidateStatus::NoChanges, 1, None)
    );
}

#[test]
fn files_of_failed_batches_are_named_and_retried_by_the_next_run() {
    let (_d, e) = repo();
    two_large_sources(&e);
    let good = notes(&[("/procedural/retry.md", "# Retry\n\ntwice")]);
    let (url, _) = mock_llm_seq(&[&good, "not json"]);
    let out = consolidate(&e, &llm(&url), None, "consolidate/one").unwrap();
    assert_eq!(
        out.failed_paths,
        vec![VirtualPath::new("/episodic/b.md").unwrap()]
    );
    assert!(
        out.batch_errors[0].contains("/episodic/b.md"),
        "{:?}",
        out.batch_errors
    );

    let (url, seen) = mock_llm(&notes(&[("/procedural/b.md", "# B\n\nnote")]));
    let out2 = consolidate_retry(
        &e,
        &llm(&url),
        Some(&out.source_commit),
        &out.failed_paths,
        "consolidate/two",
    )
    .unwrap();
    assert_eq!(
        (out2.status, out2.files_in),
        (ConsolidateStatus::Succeeded, 1)
    );
    let prompt = seen.lock().unwrap().join("\n");
    assert!(
        prompt.contains("Journal b") && !prompt.contains("Journal a"),
        "only the failed file is retried"
    );
}

#[test]
fn a_repeated_path_and_a_rewrite_of_an_unseen_note_are_dropped() {
    let (_d, e) = repo();
    two_large_sources(&e);
    write(
        &e,
        "/semantic/unrelated.md",
        "# Office plants\n\nwater weekly",
    );
    e.vcs().commit("main", "t", "more").unwrap();
    let first = notes(&[
        ("/procedural/retry.md", "# Retry\n\nfirst"),
        ("/semantic/unrelated.md", "# Office plants\n\nnever"),
    ]);
    let second = notes(&[("/procedural/retry.md", "# Retry\n\nsecond")]);
    let (url, _) = mock_llm_seq(&[&first, &second]);
    let out = consolidate(&e, &llm(&url), None, "consolidate/dup").unwrap();
    assert_eq!((out.notes_written, out.notes_dropped), (1, 2), "{out:?}");
    let head = Revision::BranchHead("consolidate/dup".into());
    assert_eq!(
        read(&e, head.clone(), "/procedural/retry.md").unwrap(),
        "# Retry\n\nfirst"
    );
    assert_eq!(
        read(&e, head, "/semantic/unrelated.md").unwrap(),
        "# Office plants\n\nwater weekly"
    );
}

#[test]
fn the_prompt_budget_includes_the_system_prompt() {
    // 22,000 + 22,000 + 3,500 bytes of text fit 48,000 bytes only without the system prompt.
    let (_d, e) = repo();
    for (name, len) in [("a", 22_000), ("b", 22_000), ("c", 3_500)] {
        write(
            &e,
            &format!("/episodic/{name}.md"),
            &sized(&format!("Journal {name}"), "entry", len),
        );
    }
    e.vcs().commit("main", "t", "seed").unwrap();
    let (url, seen) = mock_llm(&notes(&[("/procedural/x.md", "# X\n\ny")]));
    consolidate(&e, &llm(&url), None, "consolidate/budget").unwrap();
    assert_request_sizes(&seen.lock().unwrap(), 2);
}

#[test]
fn long_paths_never_push_a_request_over_the_budget() {
    let (_d, e) = repo();
    let long = "payout-journal-".repeat(8);
    write(
        &e,
        &format!("/episodic/{long}.md"),
        &sized("Payout journal", "payout journal entry", 23_000),
    );
    for i in 0..3 {
        write(
            &e,
            &format!("/semantic/{long}{i}.md"),
            &sized(&format!("Payout journal {i}"), "payout journal rule", 7_990),
        );
    }
    e.vcs().commit("main", "t", "seed").unwrap();
    let (url, seen) = mock_llm(&notes(&[("/procedural/x.md", "# X\n\ny")]));
    consolidate(&e, &llm(&url), None, "consolidate/long").unwrap();
    assert_request_sizes(&seen.lock().unwrap(), 1);
}

/// `# title` plus `word` lines, cut to exactly `len` bytes (ASCII).
fn sized(title: &str, word: &str, len: usize) -> String {
    let mut s = format!("# {title}\n\n");
    while s.len() < len {
        s.push_str(word);
        s.push('\n');
    }
    s.truncate(len);
    s
}

/// At least `min` requests, each with system + user content of at most 48,000 bytes.
fn assert_request_sizes(bodies: &[String], min: usize) {
    assert!(bodies.len() >= min, "{} request(s)", bodies.len());
    for body in bodies {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let total: usize = (0..2)
            .map(|i| v["messages"][i]["content"].as_str().unwrap().len())
            .sum();
        assert!(total <= 48_000, "system + user = {total} bytes");
    }
}

#[test]
fn a_short_source_keeps_all_three_context_notes() {
    let (_d, e) = repo();
    // 3 x (header + 7,990) = 24,306 bytes: fits next to 500 bytes, not next to 23,000. With the
    // long source path the old reservation leaves 24,187 bytes (SYSTEM_PROMPT is 622 bytes), so
    // the test still detects the old formula unless SYSTEM_PROMPT shrinks by 119 bytes or more.
    let long = "payout-journal-".repeat(6);
    write(
        &e,
        &format!("/episodic/{long}.md"),
        &sized("Payout journal", "payout journal entry", 500),
    );
    for i in 0..3 {
        write(
            &e,
            &format!("/semantic/{long}{i}.md"),
            &sized(&format!("Payout journal {i}"), "payout journal rule", 7_990),
        );
    }
    e.vcs().commit("main", "t", "seed").unwrap();
    let (url, seen) = mock_llm(&notes(&[("/procedural/x.md", "# X\n\ny")]));
    consolidate(&e, &llm(&url), None, "consolidate/short").unwrap();
    let prompt = seen.lock().unwrap().join("\n");
    for i in 0..3 {
        assert!(
            prompt.contains(&format!("{long}{i}.md")),
            "context note {i} missing"
        );
    }
}

#[test]
fn a_shrinking_rewrite_and_a_note_duplicated_across_tiers_are_dropped() {
    let (_d, e) = repo();
    seed(&e); // /semantic/routing.md (36 bytes) is a context note of the episodic file
    write(
        &e,
        "/procedural/docs/channels.md",
        "# Channels\n\nkbz, wingpay",
    );
    e.vcs().commit("main", "t", "channels").unwrap();
    let reply = notes(&[
        ("/semantic/routing.md", "# Routing\n\nx"), // 13 bytes: removes more than half
        ("/semantic/docs/channels.md", "# Channels\n\ncopy"), // same note in the other tier
        (
            "/procedural/fallback.md",
            "# Fallback SOP\n\n1. Switch to kbz.",
        ),
    ]);
    let (url, _) = mock_llm(&reply);
    let out = consolidate(&e, &llm(&url), None, "consolidate/guards").unwrap();
    assert_eq!((out.notes_written, out.notes_dropped), (1, 2), "{out:?}");
    let head = Revision::BranchHead("consolidate/guards".into());
    assert_eq!(
        read(&e, head.clone(), "/semantic/routing.md").unwrap(),
        "# Channel routing\n\nRoute by amount."
    );
    assert!(read(&e, head, "/semantic/docs/channels.md").is_none());
}

#[test]
fn the_same_new_note_in_both_tiers_of_one_run_is_written_once() {
    let (_d, e) = repo();
    seed(&e);
    let reply = notes(&[
        (
            "/procedural/fallback.md",
            "# Fallback SOP\n\n1. Switch to kbz.",
        ),
        ("/semantic/fallback.md", "# Fallback\n\nSwitch to kbz."),
    ]);
    let (url, _) = mock_llm(&reply);
    let out = consolidate(&e, &llm(&url), None, "consolidate/twins").unwrap();
    assert_eq!((out.notes_written, out.notes_dropped), (1, 1), "{out:?}");
}
