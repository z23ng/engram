//! Cold-path consolidation (S6): episodic notes on `main` -> LLM -> validated
//! `/semantic/` and `/procedural/` notes committed on a review branch. Never on the hot path.

use std::collections::BTreeMap;
use std::time::Duration;

use mem_core::{
    CommitId, DiffKind, MemError, MemoryNode, MemoryTier, Result, Revision, VirtualPath,
};
use mem_vcs::VersionControlEngine;
use serde::Deserialize;

use crate::config::{ConsolidateLlmConfig, ProviderBackend};
use crate::embedder::post_json_http_or_curl;
use crate::hybrid::{MemSearchEngine, SearchRequest};

const MAX_FILES_PER_BATCH: usize = 8;
/// Prompt budget in UTF-8 bytes (~15k tokens for CJK text): one source part plus 3 context notes.
const MAX_PROMPT_CHARS: usize = 48_000;
/// The two section titles of the user prompt.
const PROMPT_SECTIONS_BYTES: usize = 64;
/// Larger episodic files are sent in parts of at most this many bytes; one part plus 3 full
/// context notes and headers stays under `MAX_PROMPT_CHARS`.
const SOURCE_PART_BYTES: usize = 23_000;
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
    /// Path, plus `(part i/n)` for a split file.
    label: String,
    text: String,
    context: Vec<(VirtualPath, String)>,
}

impl Source {
    /// Prompt bytes of this source and its context, including the `### <label>` headers.
    fn chars(&self) -> usize {
        let header = |label: &str| label.len() + 8;
        header(&self.label)
            + self.text.len()
            + self
                .context
                .iter()
                .map(|(p, t)| header(p.as_str()) + t.len())
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

/// Loads a source file and its top related semantic/procedural notes (search, no LLM).
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

fn load_source(engine: &MemSearchEngine, rev: &Revision, path: VirtualPath) -> Result<Vec<Source>> {
    let text = read_text(engine, rev, &path)?.unwrap_or_default();
    let title = text
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(str::trim)
        .unwrap_or_else(|| path.as_str().rsplit('/').next().unwrap_or_default())
        .to_string();
    let hits = engine.search(&SearchRequest::new(rev.clone(), title).with_top_k(10))?;
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
            if body.len() <= CONTEXT_NOTE_BYTES {
                context.push((hit.path, body));
            }
        }
        if context.len() == CONTEXT_NOTES {
            break;
        }
    }
    let parts = split_parts(&text, SOURCE_PART_BYTES);
    let n = parts.len();
    Ok(parts
        .into_iter()
        .enumerate()
        .map(|(i, text)| Source {
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
                && chars + s.chars() <= MAX_PROMPT_CHARS - PROMPT_SECTIONS_BYTES
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
not invent anything. Reply with JSON only, in this form: \
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
    let source_commit = engine.vcs().head_commit("main")?;
    let mut out = ConsolidateOutcome {
        status: ConsolidateStatus::Skipped,
        source_commit,
        files_in: 0,
        notes_written: 0,
        notes_dropped: 0,
        branch: None,
        commit_id: None,
    };
    let paths = episodic_paths(engine, &source_commit, since)?;
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
    let rev = Revision::Commit(source_commit);
    let sources = paths
        .into_iter()
        .map(|p| load_source(engine, &rev, p))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let mut notes: BTreeMap<VirtualPath, String> = BTreeMap::new();
    for batch in batches(sources) {
        for note in ask_llm(cfg, &batch)? {
            if !note_is_valid(&note.path, &note.content) {
                out.notes_dropped += 1;
                continue;
            }
            let path = VirtualPath::new(&note.path)?;
            if read_text(engine, &rev, &path)?.as_deref() != Some(note.content.as_str()) {
                notes.insert(path, note.content);
            }
        }
    }
    if notes.is_empty() {
        out.status = ConsolidateStatus::NoChanges;
        return Ok(out);
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
