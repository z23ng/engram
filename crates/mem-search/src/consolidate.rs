//! Cold-path consolidation (S6): episodic notes on `main` -> LLM -> validated
//! `/semantic/` and `/procedural/` notes committed on a review branch. Never on the hot path.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use mem_core::{
    CommitId, DiffKind, MemError, MemoryNode, MemoryTier, Result, Revision, VirtualPath,
};
use mem_vcs::VersionControlEngine;
use serde::Deserialize;

use crate::config::{ConsolidateLlmConfig, ProviderBackend};
use crate::embedder::post_json_http_or_curl;
use crate::hybrid::{MemSearchEngine, SearchRequest};
use crate::ingest::weave_inline_wikilinks;
use crate::sanitize::{denoise_markdown, is_linkworthy_anchor, redact_secrets};
use crate::wiki::parse_wiki_document;

const MAX_FILES_PER_BATCH: usize = 8;
/// Prompt budget in UTF-8 bytes (~15k tokens for CJK text): one source part plus 3 context notes.
const MAX_PROMPT_CHARS: usize = 48_000;
/// The two section titles of the user prompt.
const PROMPT_SECTIONS_BYTES: usize = 64;
/// Larger episodic files are sent in parts of at most this many bytes; `load_source` keeps one
/// part plus its context notes and headers within `MAX_PROMPT_CHARS`.
const SOURCE_PART_BYTES: usize = 23_000;
/// Longest label suffix: ` (part 999/999)`.
const PART_SUFFIX_BYTES: usize = 16;
/// Larger related notes are left out: the LLM may rewrite a context note in full, so it must
/// never see a truncated copy.
const CONTEXT_NOTE_BYTES: usize = 8_000;
const MAX_NOTE_BYTES: usize = 256 * 1024;
const CONTEXT_NOTES: usize = 3;
/// Ends a runaway generation; a ~15k-token prompt plus this fits a 32k context.
const MAX_OUTPUT_TOKENS: u32 = 8192;
/// A local 7B model can need minutes for a full batch.
const LLM_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolidateStatus {
    /// Notes were committed on the review branch.
    Succeeded,
    /// The LLM output changed nothing; no branch was created.
    NoChanges,
    /// No episodic changes since the previous source commit; the LLM was not called.
    Skipped,
}

impl ConsolidateStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::NoChanges => "no_changes",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConsolidateOutcome {
    pub status: ConsolidateStatus,
    /// Head of `main` that the run read.
    pub source_commit: CommitId,
    pub files_in: usize,
    pub notes_written: usize,
    pub notes_dropped: usize,
    /// Review branch, only when `status` is `Succeeded`.
    pub branch: Option<String>,
    pub commit_id: Option<CommitId>,
    /// Batches skipped because the LLM call failed or its output was unusable.
    pub batches_failed: usize,
    /// One message per skipped batch: `batch <i> of <n> (<files>): <error>`.
    pub batch_errors: Vec<String>,
    /// Episodic files of skipped batches; pass them to the next run as `retry`.
    pub failed_paths: Vec<VirtualPath>,
}

/// Deny by default: only non-empty `.md` notes up to 256 KiB under `/semantic/` or
/// `/procedural/`, outside `_attachments/`, `_templates/`, and hidden folders.
pub fn note_is_valid(path: &str, content: &str) -> bool {
    let Ok(vp) = VirtualPath::new(path) else {
        return false;
    };
    vp.as_str() == path
        && matches!(vp.tier(), MemoryTier::Semantic | MemoryTier::Procedural)
        && path.ends_with(".md")
        && !path
            .split('/')
            .any(|s| s == "_attachments" || s == "_templates" || s.starts_with('.'))
        && !content.trim().is_empty()
        && content.len() <= MAX_NOTE_BYTES
}

struct Source {
    path: VirtualPath,
    /// Path, plus `(part i/n)` for a split file.
    label: String,
    text: String,
    context: Vec<(VirtualPath, String)>,
}

impl Source {
    /// Prompt bytes of this source and its context, including the `### <label>` headers.
    fn chars(&self) -> usize {
        header_bytes(&self.label)
            + self.text.len()
            + self
                .context
                .iter()
                .map(|(p, t)| header_bytes(p.as_str()) + t.len())
                .sum::<usize>()
    }
}

/// `/episodic/**/*.md` files added or changed between `since` and `source` (all on a first run).
fn episodic_paths(
    engine: &MemSearchEngine,
    source: &CommitId,
    since: Option<&CommitId>,
) -> Result<Vec<VirtualPath>> {
    let rev = Revision::Commit(*source);
    let paths: Vec<VirtualPath> = match since {
        Some(prev) => engine
            .vcs()
            .diff(&Revision::Commit(*prev), &rev)?
            .into_iter()
            .filter(|d| d.kind != DiffKind::Deleted)
            .map(|d| d.path)
            .collect(),
        None => engine
            .vcs()
            .list(&rev, None)?
            .into_iter()
            .map(|(p, _, _)| p)
            .collect(),
    };
    Ok(paths
        .into_iter()
        .filter(|p| p.tier() == MemoryTier::Episodic && p.as_str().ends_with(".md"))
        .collect())
}

fn read_text(
    engine: &MemSearchEngine,
    rev: &Revision,
    path: &VirtualPath,
) -> Result<Option<String>> {
    Ok(engine
        .vcs()
        .read(rev, path)?
        .map(|n| String::from_utf8_lossy(&n.content).into_owned()))
}

/// `/semantic/docs/x.md` -> `docs/x.md`.
fn without_tier(path: &str) -> &str {
    path.trim_start_matches('/')
        .split_once('/')
        .map_or(path, |(_, rest)| rest)
}

/// Prompt bytes of a `\n### <label>\n` section header plus the trailing newline.
fn header_bytes(label: &str) -> usize {
    label.len() + 8
}

/// Splits `text` at line ends into parts of at most `max` bytes (a longer line is cut at a
/// char boundary).
fn split_parts(text: &str, max: usize) -> Vec<String> {
    let mut parts = vec![String::new()];
    for line in text.split_inclusive('\n') {
        let mut rest = line;
        while !rest.is_empty() {
            let last = parts.last_mut().expect("parts is never empty");
            let room = max - last.len();
            if rest.len() <= room {
                last.push_str(rest);
                break;
            }
            if !last.is_empty() && rest.len() <= max {
                parts.push(String::new());
                continue;
            }
            let mut cut = room;
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            last.push_str(&rest[..cut]);
            rest = &rest[cut..];
            parts.push(String::new());
        }
    }
    parts.retain(|p| !p.is_empty());
    parts
}

/// Loads a source file (in parts) and its top related semantic/procedural notes (search, no LLM).
fn load_source(engine: &MemSearchEngine, rev: &Revision, path: VirtualPath) -> Result<Vec<Source>> {
    let raw_text = read_text(engine, rev, &path)?.unwrap_or_default();
    let (redacted, _) = redact_secrets(&raw_text);
    let text = denoise_markdown(&redacted, false);
    let title = text
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(str::trim)
        .unwrap_or_else(|| path.as_str().rsplit('/').next().unwrap_or_default())
        .to_string();
    let hits = engine.search(&SearchRequest::new(rev.clone(), title).with_top_k(10))?;
    let parts = split_parts(&text, SOURCE_PART_BYTES);
    let largest = parts.iter().map(String::len).max().unwrap_or(0);
    // Bytes left for context next to the largest part and its longest label (`(part n/n)`), so
    // a batch of one source never exceeds MAX_PROMPT_CHARS, whatever the path lengths.
    let mut room = (MAX_PROMPT_CHARS - PROMPT_SECTIONS_BYTES - SYSTEM_PROMPT.len())
        .saturating_sub(largest + header_bytes(path.as_str()) + PART_SUFFIX_BYTES);
    let mut context: Vec<(VirtualPath, String)> = Vec::new();
    for hit in hits {
        let related = matches!(
            hit.path.tier(),
            MemoryTier::Semantic | MemoryTier::Procedural
        );
        if !related || context.iter().any(|(p, _)| *p == hit.path) {
            continue;
        }
        if let Some(body) = read_text(engine, rev, &hit.path)? {
            let need = header_bytes(hit.path.as_str()) + body.len();
            if body.len() <= CONTEXT_NOTE_BYTES && need <= room {
                room -= need;
                context.push((hit.path, body));
            }
        }
        if context.len() == CONTEXT_NOTES {
            break;
        }
    }
    let n = parts.len();
    Ok(parts
        .into_iter()
        .enumerate()
        .map(|(i, text)| Source {
            path: path.clone(),
            label: match n {
                1 => path.as_str().to_string(),
                _ => format!("{} (part {}/{n})", path.as_str(), i + 1),
            },
            text,
            context: context.clone(),
        })
        .collect())
}

/// Greedy batches of at most 8 sources and 48,000 characters (a larger single source runs alone).
fn batches(sources: Vec<Source>) -> Vec<Vec<Source>> {
    let mut out: Vec<Vec<Source>> = Vec::new();
    let mut chars = 0;
    for s in sources {
        let fits = out.last().is_some_and(|b| {
            b.len() < MAX_FILES_PER_BATCH
                && chars + s.chars()
                    <= MAX_PROMPT_CHARS - PROMPT_SECTIONS_BYTES - SYSTEM_PROMPT.len()
        });
        if !fits {
            out.push(Vec::new());
            chars = 0;
        }
        chars += s.chars();
        out.last_mut().expect("batch pushed above").push(s);
    }
    out
}

const SYSTEM_PROMPT: &str =
    "You consolidate an Engram memory vault. Episodic notes are raw logs of \
what happened. Extract durable facts into notes under /semantic/ and repeatable procedures into \
notes under /procedural/. To change a related existing note, return its path with the full new \
content; otherwise create a new note with a short kebab-case file name. Keep facts verbatim and do \
not invent anything. Return at most 5 notes, each under 4,000 characters; prefer fewer, \
shorter notes. Reply with JSON only, in this form: \
{\"notes\":[{\"path\":\"/semantic/<name>.md\",\"content\":\"<full Markdown>\"}]}. \
Return {\"notes\":[]} when nothing durable is found.";

#[derive(Deserialize)]
struct Reply {
    notes: Vec<ReplyNote>,
}

#[derive(Deserialize)]
struct ReplyNote {
    path: String,
    content: String,
}

fn user_prompt(batch: &[Source]) -> String {
    let mut out = String::from("## Episodic notes\n");
    for s in batch {
        out.push_str(&format!("\n### {}\n{}\n", s.label, s.text));
    }
    out.push_str("\n## Related existing notes\n");
    let mut shown: Vec<&VirtualPath> = Vec::new();
    for (path, text) in batch.iter().flat_map(|s| &s.context) {
        if !shown.contains(&path) {
            shown.push(path);
            out.push_str(&format!("\n### {}\n{}\n", path.as_str(), text));
        }
    }
    out
}

/// One chat completion; any transport or format problem is an error (the run fails).
fn ask_llm(cfg: &ConsolidateLlmConfig, batch: &[Source]) -> Result<Vec<ReplyNote>> {
    let body = serde_json::json!({
        "model": cfg.model,
        "temperature": 0.1,
        "max_tokens": MAX_OUTPUT_TOKENS,
        "response_format": {"type": "json_object"},
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_prompt(batch)},
        ],
    });
    let endpoint = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let raw = post_json_http_or_curl(
        &endpoint,
        cfg.api_key.as_deref(),
        &body.to_string(),
        LLM_TIMEOUT,
    )
    .map_err(|e| MemError::Storage(format!("consolidation LLM call failed: {e}")))?;
    let resp: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| MemError::Serialization(format!("consolidation LLM response: {e}")))?;
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    // Tolerate ```json fences and prose around the object.
    let json = match (content.find('{'), content.rfind('}')) {
        (Some(a), Some(b)) if a < b => &content[a..=b],
        _ => {
            return Err(MemError::Serialization(
                "consolidation LLM returned no JSON object".into(),
            ))
        }
    };
    let reply: Reply = serde_json::from_str(json)
        .map_err(|e| MemError::Serialization(format!("consolidation LLM notes: {e}")))?;
    Ok(reply.notes)
}

/// Consolidates episodic changes of `main` since `since` into a new `branch`. Reads only the
/// head commit of `main`; writes only `branch`; deletes nothing.
pub fn consolidate(
    engine: &MemSearchEngine,
    cfg: &ConsolidateLlmConfig,
    since: Option<&CommitId>,
    branch: &str,
) -> Result<ConsolidateOutcome> {
    consolidate_retry(engine, cfg, since, &[], branch)
}

/// Like [`consolidate`], and also reads the `retry` files (the `failed_paths` of the previous
/// run) when they still exist on `main`.
pub fn consolidate_retry(
    engine: &MemSearchEngine,
    cfg: &ConsolidateLlmConfig,
    since: Option<&CommitId>,
    retry: &[VirtualPath],
    branch: &str,
) -> Result<ConsolidateOutcome> {
    let source_commit = engine.vcs().head_commit("main")?;
    let mut out = ConsolidateOutcome {
        status: ConsolidateStatus::Skipped,
        source_commit,
        files_in: 0,
        notes_written: 0,
        notes_dropped: 0,
        branch: None,
        commit_id: None,
        batches_failed: 0,
        batch_errors: Vec::new(),
        failed_paths: Vec::new(),
    };
    let rev = Revision::Commit(source_commit);
    let mut paths: BTreeSet<VirtualPath> = episodic_paths(engine, &source_commit, since)?
        .into_iter()
        .collect();
    for path in retry {
        if path.tier() == MemoryTier::Episodic && engine.vcs().read(&rev, path)?.is_some() {
            paths.insert(path.clone());
        }
    }
    if paths.is_empty() {
        return Ok(out);
    }
    let usable_key = cfg.api_key.as_deref().is_some_and(|k| !k.trim().is_empty());
    match cfg.backend {
        ProviderBackend::Disabled => {
            return Err(MemError::Storage("consolidation LLM is disabled".into()))
        }
        ProviderBackend::OpenAi if !usable_key => {
            return Err(MemError::Storage(
                "consolidation LLM needs an OpenAI API key".into(),
            ))
        }
        _ => {}
    }
    out.files_in = paths.len();
    let sources = paths
        .into_iter()
        .map(|p| load_source(engine, &rev, p))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let mut notes: BTreeMap<VirtualPath, String> = BTreeMap::new();
    // Paths already answered by an earlier batch; a repeat would silently replace its facts.
    let mut answered: BTreeSet<VirtualPath> = BTreeSet::new();
    // `docs/x.md` for every /semantic/docs/x.md and /procedural/docs/x.md on the source commit.
    // Notes accepted by this run are added too.
    let mut tier_twins: BTreeSet<String> = BTreeSet::new();
    for tier in ["/semantic", "/procedural"] {
        for (p, _, _) in engine.vcs().list(&rev, Some(&VirtualPath::new(tier)?))? {
            tier_twins.insert(without_tier(p.as_str()).to_string());
        }
    }
    let batches = batches(sources);
    let total = batches.len();
    if total == 0 {
        // Only empty files changed: nothing to send.
        out.status = ConsolidateStatus::NoChanges;
        return Ok(out);
    }
    for (i, batch) in batches.iter().enumerate() {
        // A bad batch is skipped and counted; the run fails only when every batch fails.
        let reply = match ask_llm(cfg, batch) {
            Ok(reply) => reply,
            Err(e) => {
                let labels: Vec<&str> = batch.iter().map(|s| s.label.as_str()).collect();
                out.batches_failed += 1;
                out.batch_errors.push(format!(
                    "batch {} of {total} ({}): {e}",
                    i + 1,
                    labels.join(", ")
                ));
                for s in batch {
                    if !out.failed_paths.contains(&s.path) {
                        out.failed_paths.push(s.path.clone());
                    }
                }
                continue;
            }
        };
        let seen: BTreeSet<&VirtualPath> = batch
            .iter()
            .flat_map(|s| s.context.iter().map(|(p, _)| p))
            .collect();
        for note in reply {
            if !note_is_valid(&note.path, &note.content) {
                out.notes_dropped += 1;
                continue;
            }
            let path = VirtualPath::new(&note.path)?;
            let current = read_text(engine, &rev, &path)?;
            // Deny a repeated path, a full rewrite of an existing note the batch never saw, a
            // rewrite that removes more than half of a note or strips all [[Wikilinks]] without
            // supersession, and a new note that already exists in the other tier.
            let denied = match &current {
                Some(old) => {
                    let old_links = parse_wiki_document(&path, old, 512).wikilinks;
                    let new_links = parse_wiki_document(&path, &note.content, 512).wikilinks;
                    let declares_supersede =
                        note.content.to_ascii_lowercase().contains("supersedes")
                            || note.content.contains("Deprecated");
                    let dropped_all_links =
                        !old_links.is_empty() && new_links.is_empty() && !declares_supersede;
                    !seen.contains(&path) || note.content.len() * 2 < old.len() || dropped_all_links
                }
                None => tier_twins.contains(without_tier(path.as_str())),
            };
            if answered.contains(&path) || denied {
                out.notes_dropped += 1;
                continue;
            }
            answered.insert(path.clone());
            if current.is_none() {
                tier_twins.insert(without_tier(path.as_str()).to_string());
            }
            if current.as_deref() != Some(note.content.as_str()) {
                notes.insert(path, note.content);
            }
        }
    }
    if out.batches_failed == total {
        return Err(MemError::Storage(out.batch_errors.join("; ")));
    }
    if notes.is_empty() {
        out.status = ConsolidateStatus::NoChanges;
        return Ok(out);
    }

    // Weave non-self active [[Wikilinks]] deterministically into newly generated or updated notes.
    let anchors = load_non_episodic_anchors(engine, &rev)?;
    if !anchors.is_empty() {
        let total_docs = anchors.len();
        let mut doc_freq: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for (a_path, a_title, a_aliases) in &anchors {
            let _ = a_path;
            let mut seen_t = std::collections::HashSet::new();
            let tl = a_title.trim().to_ascii_lowercase();
            if !tl.is_empty() && seen_t.insert(tl.clone()) {
                *doc_freq.entry(tl).or_insert(0) += 1;
            }
            for al in a_aliases {
                let all = al.trim().to_ascii_lowercase();
                if !all.is_empty() && seen_t.insert(all.clone()) {
                    *doc_freq.entry(all).or_insert(0) += 1;
                }
            }
        }

        for (note_path, content) in notes.iter_mut() {
            let parsed_note = parse_wiki_document(note_path, content, 512);
            let mut pairs: Vec<(String, String)> = Vec::new();
            for (a_path, a_title, a_aliases) in &anchors {
                if a_path == note_path || a_title.eq_ignore_ascii_case(&parsed_note.title) {
                    continue;
                }
                let t_df = doc_freq
                    .get(&a_title.trim().to_ascii_lowercase())
                    .copied()
                    .unwrap_or(1);
                if is_linkworthy_anchor(a_title, t_df, total_docs) {
                    pairs.push((a_title.clone(), a_title.clone()));
                }
                for al in a_aliases {
                    let a_df = doc_freq
                        .get(&al.trim().to_ascii_lowercase())
                        .copied()
                        .unwrap_or(1);
                    if is_linkworthy_anchor(al, a_df, total_docs) {
                        pairs.push((al.clone(), a_title.clone()));
                    }
                }
            }
            pairs.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
            pairs.dedup();
            if !pairs.is_empty() {
                *content = weave_body_preserving_frontmatter(content, &pairs);
            }
        }
    }
    engine.vcs().create_branch(branch, &rev)?;
    let message = format!(
        "consolidate: {} note(s) from {} episodic file(s) @ {}",
        notes.len(),
        out.files_in,
        &source_commit.to_hex()[..10]
    );
    let written = (|| {
        for (path, content) in &notes {
            let blob = engine.vcs().write_node(
                branch,
                MemoryNode::new(path.clone(), content.as_bytes().to_vec()),
            )?;
            engine.index_blob_if_needed(path, &blob)?;
        }
        engine
            .vcs()
            .commit(branch, &format!("consolidator:{}", cfg.model), &message)
    })();
    match written {
        Ok(id) => {
            out.status = ConsolidateStatus::Succeeded;
            out.notes_written = notes.len();
            out.branch = Some(branch.to_string());
            out.commit_id = Some(id);
            Ok(out)
        }
        Err(e) => {
            let _ = engine.vcs().delete_branch(branch);
            Err(e)
        }
    }
}

fn load_non_episodic_anchors(
    engine: &MemSearchEngine,
    rev: &Revision,
) -> Result<Vec<(VirtualPath, String, Vec<String>)>> {
    let entries = engine.vcs().list(rev, None)?;
    let mut out = Vec::new();
    for (path, _, _) in entries {
        if path.tier() == MemoryTier::Episodic {
            continue;
        }
        if let Some(text) = read_text(engine, rev, &path)? {
            let parsed = parse_wiki_document(&path, &text, 512);
            out.push((path, parsed.title, parsed.aliases));
        }
    }
    Ok(out)
}

fn weave_body_preserving_frontmatter(markdown: &str, pairs: &[(String, String)]) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    if lines.first().is_some_and(|l| l.trim() == "---") {
        if let Some(end_idx) = lines
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(idx, l)| (l.trim() == "---" || l.trim() == "...").then_some(idx))
        {
            let fm = lines[..=end_idx].join("\n");
            let body = lines[end_idx + 1..].join("\n");
            let woven = weave_inline_wikilinks(&body, pairs);
            let mut result = format!("{fm}\n{woven}");
            if markdown.ends_with('\n') && !result.ends_with('\n') {
                result.push('\n');
            }
            return result;
        }
    }
    let mut woven = weave_inline_wikilinks(markdown, pairs);
    if markdown.ends_with('\n') && !woven.ends_with('\n') {
        woven.push('\n');
    }
    woven
}
