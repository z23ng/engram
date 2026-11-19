//! Natural-language and rough-note collaborative knowledge weaver (`mem ingest`).
//!
//! Converts unstructured PM, Developer, or Bot inputs into Obsidian-style Wiki
//! Markdown notes, automatically discovering active branch notes to weave
//! `[[Wikilinks]]`, routing to role-appropriate VFS namespaces, and supporting
//! isolated `draft/{role}-inbox` branches for safe team review before merge.

use std::collections::HashSet;
use std::time::Duration;

use chrono::Utc;
use mem_core::{BlobId, CommitId, MemError, MemoryNode, MemoryTier, Result, Revision, VirtualPath};
use mem_vcs::VersionControlEngine;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::config::{ConsolidateLlmConfig, ProviderBackend};
use crate::embedder::post_json_http_or_curl;
use crate::hybrid::{MemSearchEngine, SearchRequest};
use crate::wiki::parse_wiki_document;

const LLM_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RELATED_LINKS: usize = 6;
const MAX_SLUG_CHARS: usize = 48;

/// Collaborator persona feeding knowledge into Engram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestRole {
    /// Product Manager: PRDs, business constraints, user stories, FAQs.
    Pm,
    /// Developer: ADRs, architectural invariants, API contracts, runbooks, postmortems.
    Dev,
    /// Bot / AI Agent: session discoveries, edge cases, incident observations.
    Bot,
}

impl IngestRole {
    /// Parses a role label case-insensitively (`pm`, `dev`, `bot` and common aliases).
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pm" | "product" | "product-manager" => Ok(Self::Pm),
            "dev" | "developer" | "eng" | "engineer" => Ok(Self::Dev),
            "bot" | "agent" | "assistant" => Ok(Self::Bot),
            other => Err(MemError::Serialization(format!(
                "unknown ingest role '{other}'; expected one of: pm, dev, bot"
            ))),
        }
    }

    /// Canonical lowercase identifier for this role.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pm => "pm",
            Self::Dev => "dev",
            Self::Bot => "bot",
        }
    }

    /// Default memory tier used when callers omit `--tier` and `--path`.
    pub fn default_tier(&self) -> MemoryTier {
        match self {
            Self::Pm => MemoryTier::Semantic,
            Self::Dev => MemoryTier::Semantic,
            Self::Bot => MemoryTier::Episodic,
        }
    }

    /// Default importance score assigned to notes contributed by this role.
    pub fn default_importance(&self) -> f32 {
        match self {
            Self::Pm => 0.75,
            Self::Dev => 0.85,
            Self::Bot => 0.55,
        }
    }

    /// Default VFS directory prefix for `(role, tier)`.
    pub fn default_dir(&self, tier: MemoryTier) -> &'static str {
        match (self, tier) {
            (Self::Pm, MemoryTier::Core) => "/core",
            (Self::Pm, MemoryTier::Semantic) => "/semantic/product",
            (Self::Pm, MemoryTier::Procedural) => "/procedural/product-playbooks",
            (Self::Pm, MemoryTier::Episodic) => "/episodic/product-logs",
            (Self::Dev, MemoryTier::Core) => "/core",
            (Self::Dev, MemoryTier::Semantic) => "/semantic/architecture",
            (Self::Dev, MemoryTier::Procedural) => "/procedural/runbooks",
            (Self::Dev, MemoryTier::Episodic) => "/episodic/incidents",
            (Self::Bot, MemoryTier::Core) => "/core",
            (Self::Bot, MemoryTier::Semantic) => "/semantic/bot-notes",
            (Self::Bot, MemoryTier::Procedural) => "/procedural/bot-learnings",
            (Self::Bot, MemoryTier::Episodic) => "/episodic/bot-insights",
        }
    }

    /// Default isolated inbox branch name when `--draft` is requested from `main`.
    pub fn draft_branch(&self) -> String {
        format!("draft/{}-inbox", self.as_str())
    }
}

/// Request payload describing a rough note or structured insight to weave into Engram.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestRequest {
    /// Contributor role driving default VFS namespace, tone, and tags.
    pub role: IngestRole,
    /// Unstructured note, decision blurb, meeting note, or bot observation.
    pub raw_text: String,
    /// Base target branch (defaults to `"main"`).
    pub branch: String,
    /// Optional explicit note title (otherwise derived from `# Heading` or leading line).
    pub title: Option<String>,
    /// Optional explicit VFS path overriding role/tier directory routing.
    pub path: Option<VirtualPath>,
    /// Optional target tier overriding the role's default tier.
    pub tier: Option<MemoryTier>,
    /// Additional user/team tags attached alongside the role tag.
    pub tags: Vec<String>,
    /// Route write into an isolated draft inbox branch (`draft/{role}-inbox`) instead of `branch`.
    pub draft: bool,
    /// Optional explicit commit author (defaults to `"{role}@engram"`).
    pub author: Option<String>,
    /// Optional importance score in `[0.0, 1.0]`.
    pub importance: Option<f32>,
    /// Optional commit message when `commit == true`.
    pub commit_message: Option<String>,
    /// Whether to commit immediately after writing and indexing (defaults to `true`).
    pub commit: bool,
    /// Whether to attempt optional OpenAI-compatible/Ollama LLM formatting before deterministic fallback.
    pub use_llm: bool,
}

impl IngestRequest {
    /// Creates a new ingest request targeting `main` with immediate commit and deterministic weaving.
    pub fn new(role: IngestRole, raw_text: impl Into<String>) -> Self {
        Self {
            role,
            raw_text: raw_text.into(),
            branch: "main".to_string(),
            title: None,
            path: None,
            tier: None,
            tags: Vec::new(),
            draft: false,
            author: None,
            importance: None,
            commit_message: None,
            commit: true,
            use_llm: false,
        }
    }

    /// Sets the target or base branch.
    pub fn with_branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = branch.into();
        self
    }

    /// Sets an explicit note title.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Sets an explicit VFS path overriding automatic role routing.
    pub fn with_path(mut self, path: VirtualPath) -> Self {
        self.path = Some(path);
        self
    }

    /// Sets an explicit target memory tier.
    pub fn with_tier(mut self, tier: MemoryTier) -> Self {
        self.tier = Some(tier);
        self
    }

    /// Appends additional tags to the generated note.
    pub fn with_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags.extend(tags.into_iter().map(Into::into));
        self
    }

    /// Enables or disables isolated `draft/{role}-inbox` branch routing.
    pub fn with_draft(mut self, draft: bool) -> Self {
        self.draft = draft;
        self
    }

    /// Sets an explicit commit author.
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    /// Sets an explicit importance weight in `[0.0, 1.0]`.
    pub fn with_importance(mut self, importance: f32) -> Self {
        self.importance = Some(importance);
        self
    }

    /// Sets whether to immediately create a commit after writing and indexing.
    pub fn with_commit(mut self, commit: bool) -> Self {
        self.commit = commit;
        self
    }

    /// Sets an explicit commit message.
    pub fn with_commit_message(mut self, msg: impl Into<String>) -> Self {
        self.commit_message = Some(msg.into());
        self
    }

    /// Enables or disables optional LLM structuring (`gpt-6-luna` / `qwen2.5:7b`).
    pub fn with_use_llm(mut self, use_llm: bool) -> Self {
        self.use_llm = use_llm;
        self
    }
}

/// Result metadata and rendered Markdown returned by [`ingest_knowledge`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestOutcome {
    /// Actual branch written to (e.g. `"main"` or `"draft/pm-inbox"`).
    pub branch: String,
    /// Target normalized VFS path written in the revision.
    pub path: VirtualPath,
    /// Final note title.
    pub title: String,
    /// Contributor role used for formatting and routing.
    pub role: IngestRole,
    /// Immutable BLAKE3 content hash of the ingested Markdown note.
    pub blob_id: BlobId,
    /// Commit ID created when `commit == true`.
    pub commit_id: Option<CommitId>,
    /// Note titles woven or linked via `[[Wikilinks]]`.
    pub linked_notes: Vec<String>,
    /// Active VFS paths corresponding to discovered related notes.
    pub matched_paths: Vec<VirtualPath>,
    /// Final normalized tag list attached to the memory node.
    pub tags: Vec<String>,
    /// Whether the remote/local LLM formatter produced the body (`false` = deterministic template).
    pub used_llm: bool,
    /// Rendered Obsidian-compatible Markdown written into Engram.
    pub markdown: String,
}

#[derive(Debug, Clone)]
struct ActiveAnchor {
    path: VirtualPath,
    title: String,
    aliases: Vec<String>,
}

/// Structures a raw note, weaves `[[Wikilinks]]` against active notes on the target branch,
/// writes and indexes the Markdown node, and optionally commits the snapshot.
pub fn ingest_knowledge(
    engine: &MemSearchEngine,
    req: &IngestRequest,
    llm_config: Option<&ConsolidateLlmConfig>,
) -> Result<IngestOutcome> {
    let raw_trimmed = req.raw_text.trim();
    if raw_trimmed.is_empty() {
        return Err(MemError::Serialization(
            "ingest text cannot be empty".to_string(),
        ));
    }

    let target_branch = resolve_target_branch(engine, req)?;
    let target_rev = Revision::BranchWorkspace(target_branch.clone());

    let resolved_title = derive_title(req.title.as_deref(), raw_trimmed);
    let resolved_tier = req
        .path
        .as_ref()
        .map(VirtualPath::tier)
        .or(req.tier)
        .unwrap_or_else(|| req.role.default_tier());
    let resolved_path = resolve_target_path(req, resolved_tier, &resolved_title)?;

    let (linked_notes, matched_paths, weave_pairs) = discover_related_notes(
        engine,
        &target_rev,
        &resolved_path,
        &resolved_title,
        raw_trimmed,
    )?;

    let resolved_tags = normalize_tags(req.role, resolved_tier, &req.tags);
    let resolved_importance = req
        .importance
        .unwrap_or_else(|| req.role.default_importance())
        .clamp(0.0, 1.0);
    let resolved_author = req
        .author
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{}@engram", req.role.as_str()));

    let ingested_at = Utc::now().to_rfc3339();

    let (markdown, used_llm) = match (req.use_llm, llm_config) {
        (true, Some(cfg)) => {
            if let Some(llm_md) = try_format_with_llm(
                cfg,
                req.role,
                resolved_tier,
                &resolved_title,
                raw_trimmed,
                &linked_notes,
                &resolved_tags,
                resolved_importance,
                &ingested_at,
            ) {
                (llm_md, true)
            } else {
                (
                    render_deterministic_markdown(
                        req.role,
                        resolved_tier,
                        &resolved_title,
                        raw_trimmed,
                        &weave_pairs,
                        &linked_notes,
                        &resolved_tags,
                        resolved_importance,
                        &ingested_at,
                    ),
                    false,
                )
            }
        }
        _ => (
            render_deterministic_markdown(
                req.role,
                resolved_tier,
                &resolved_title,
                raw_trimmed,
                &weave_pairs,
                &linked_notes,
                &resolved_tags,
                resolved_importance,
                &ingested_at,
            ),
            false,
        ),
    };

    let node = MemoryNode::new(resolved_path.clone(), markdown.as_bytes().to_vec())
        .with_tags(resolved_tags.clone())
        .with_importance(resolved_importance);

    let blob_id = engine.vcs().write_node(&target_branch, node)?;
    engine.index_blob_if_needed(&resolved_path, &blob_id)?;

    let commit_id = if req.commit {
        let msg = req
            .commit_message
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("ingest({}): {}", req.role.as_str(), resolved_title));
        Some(
            engine
                .vcs()
                .commit(&target_branch, &resolved_author, &msg)?,
        )
    } else {
        None
    };

    Ok(IngestOutcome {
        branch: target_branch,
        path: resolved_path,
        title: resolved_title,
        role: req.role,
        blob_id,
        commit_id,
        linked_notes,
        matched_paths,
        tags: resolved_tags,
        used_llm,
        markdown,
    })
}

fn resolve_target_branch(engine: &MemSearchEngine, req: &IngestRequest) -> Result<String> {
    let base_branch = if req.branch.trim().is_empty() {
        "main"
    } else {
        req.branch.trim()
    };

    if !req.draft {
        engine.vcs().head_commit(base_branch)?;
        return Ok(base_branch.to_string());
    }

    let draft_branch = if base_branch == "main" {
        req.role.draft_branch()
    } else if base_branch.starts_with("draft/") {
        base_branch.to_string()
    } else {
        format!("draft/{}-{}", req.role.as_str(), base_branch)
    };

    match engine.vcs().head_commit(&draft_branch) {
        Ok(_) => Ok(draft_branch),
        Err(MemError::BranchNotFound(_)) => {
            engine.vcs().create_branch(
                &draft_branch,
                &Revision::BranchHead(base_branch.to_string()),
            )?;
            Ok(draft_branch)
        }
        Err(err) => Err(err),
    }
}

fn resolve_target_path(
    req: &IngestRequest,
    resolved_tier: MemoryTier,
    resolved_title: &str,
) -> Result<VirtualPath> {
    if let Some(ref explicit) = req.path {
        explicit.validate_file_path()?;
        return Ok(explicit.clone());
    }
    let slug = slugify_title(resolved_title);
    let raw = format!("{}/{}.md", req.role.default_dir(resolved_tier), slug);
    let vpath = VirtualPath::new(&raw)?;
    vpath.validate_file_path()?;
    Ok(vpath)
}

fn derive_title(explicit: Option<&str>, raw_text: &str) -> String {
    if let Some(title) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return title.to_string();
    }

    let mut in_frontmatter = false;
    for (idx, line) in raw_text.lines().enumerate() {
        let trimmed = line.trim();
        if idx == 0 && trimmed == "---" {
            in_frontmatter = true;
            continue;
        }
        if in_frontmatter {
            if let Some(rest) = trimmed.strip_prefix("title:") {
                let unquoted = rest.trim().trim_matches(|c| c == '"' || c == '\'');
                if !unquoted.is_empty() {
                    return unquoted.to_string();
                }
            }
            if trimmed == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let cleaned = trimmed
            .trim_start_matches('#')
            .trim_start_matches(['-', '*', '>', ' '])
            .trim();
        if cleaned.is_empty() {
            continue;
        }
        let sentence = cleaned
            .split(['\n', '。', '；', ';'])
            .next()
            .unwrap_or(cleaned)
            .trim();
        return truncate_chars(sentence, 64);
    }

    "Untitled Note".to_string()
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    s.chars()
        .take(max_chars)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn slugify_title(title: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !ch.is_ascii() && ch.is_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
        if slug.chars().count() >= MAX_SLUG_CHARS {
            break;
        }
    }
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        let digest = blake3::hash(title.as_bytes()).to_hex();
        format!("note-{}", &digest[..8])
    } else {
        trimmed
    }
}

fn normalize_tags(role: IngestRole, tier: MemoryTier, user_tags: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let defaults = [format!("role/{}", role.as_str()), tier.as_str().to_string()];
    for tag in defaults.into_iter().chain(user_tags.iter().cloned()) {
        let clean = tag.trim().trim_start_matches('#').to_string();
        if !clean.is_empty() && seen.insert(clean.to_ascii_lowercase()) {
            out.push(clean);
        }
    }
    out
}

/// Linked titles, matched paths, and (title, alias) pairs to weave inline.
type RelatedNotes = (Vec<String>, Vec<VirtualPath>, Vec<(String, String)>);

fn discover_related_notes(
    engine: &MemSearchEngine,
    rev: &Revision,
    target_path: &VirtualPath,
    resolved_title: &str,
    raw_text: &str,
) -> Result<RelatedNotes> {
    let active_anchors = load_active_anchors(engine, rev)?;
    let raw_lower = raw_text.to_ascii_lowercase();
    let title_lower = resolved_title.to_ascii_lowercase();

    let mut linked_titles: Vec<String> = Vec::new();
    let mut linked_seen: HashSet<String> = HashSet::new();
    let mut matched_paths: Vec<VirtualPath> = Vec::new();
    let mut path_seen: HashSet<VirtualPath> = HashSet::new();
    let mut weave_pairs: Vec<(String, String)> = Vec::new();

    // 1. Preserve any existing [[Wikilink]] already authored in raw_text.
    let parsed_existing = parse_wiki_document(target_path, raw_text, 512);
    for wl in &parsed_existing.wikilinks {
        let key = wl.target.trim().to_ascii_lowercase();
        if !key.is_empty() && linked_seen.insert(key) {
            linked_titles.push(wl.target.trim().to_string());
        }
    }

    // 2. Direct title/alias matching against active branch notes (for natural inline weaving).
    for anchor in &active_anchors {
        if &anchor.path == target_path || anchor.title.eq_ignore_ascii_case(resolved_title) {
            continue;
        }
        let mut matched_anchor = false;
        let anchor_title_lower = anchor.title.to_ascii_lowercase();
        if anchor.title.chars().count() >= 2
            && contains_term_boundary(&raw_lower, &anchor_title_lower)
        {
            weave_pairs.push((anchor.title.clone(), anchor.title.clone()));
            matched_anchor = true;
        } else if anchor.title.chars().count() >= 2
            && contains_term_boundary(&title_lower, &anchor_title_lower)
        {
            matched_anchor = true;
        }

        for alias in &anchor.aliases {
            if alias.chars().count() >= 2 {
                let alias_lower = alias.to_ascii_lowercase();
                if contains_term_boundary(&raw_lower, &alias_lower) {
                    weave_pairs.push((alias.clone(), anchor.title.clone()));
                    matched_anchor = true;
                } else if contains_term_boundary(&title_lower, &alias_lower) {
                    matched_anchor = true;
                }
            }
        }

        if matched_anchor {
            if linked_seen.insert(anchor_title_lower) && linked_titles.len() < MAX_RELATED_LINKS {
                linked_titles.push(anchor.title.clone());
            }
            if path_seen.insert(anchor.path.clone()) && matched_paths.len() < MAX_RELATED_LINKS {
                matched_paths.push(anchor.path.clone());
            }
        }
    }

    // 3. Branch-scoped hybrid search to pick up conceptually related notes even without exact title match.
    let query_seed = format!("{} {}", resolved_title, truncate_chars(raw_text, 240));
    if !active_anchors.is_empty() && linked_titles.len() < MAX_RELATED_LINKS {
        let mut req = SearchRequest::new(rev.clone(), query_seed).with_top_k(6);
        req.top_k = 6;
        if let Ok(hits) = engine.search(&req) {
            for hit in hits {
                if &hit.path == target_path || hit.title.eq_ignore_ascii_case(resolved_title) {
                    continue;
                }
                let t_key = hit.title.to_ascii_lowercase();
                if linked_seen.insert(t_key) && linked_titles.len() < MAX_RELATED_LINKS {
                    linked_titles.push(hit.title.clone());
                }
                if path_seen.insert(hit.path.clone()) && matched_paths.len() < MAX_RELATED_LINKS {
                    matched_paths.push(hit.path);
                }
            }
        }
    }

    // Prefer longest match first when weaving inline links so multi-word titles win over sub-terms.
    weave_pairs.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
    weave_pairs.dedup();

    Ok((linked_titles, matched_paths, weave_pairs))
}

fn load_active_anchors(engine: &MemSearchEngine, rev: &Revision) -> Result<Vec<ActiveAnchor>> {
    let active = engine.vcs().list(rev, None)?;
    if active.is_empty() {
        return Ok(Vec::new());
    }

    let mut anchors = Vec::with_capacity(active.len());
    engine.vcs().store().with_conn(|conn| {
        let mut stmt = conn
            .prepare_cached(
                "SELECT title, aliases FROM chunk_index WHERE blob_id = ?1 AND chunk_seq = 0 LIMIT 1",
            )
            .map_err(|e| MemError::Storage(format!("failed to prepare anchor query: {e}")))?;

        for (path, blob_id, _) in &active {
            let row: Option<(String, String)> = match stmt
                .query_row(params![blob_id.to_hex()], |r| Ok((r.get(0)?, r.get(1)?)))
            {
                Ok(v) => Some(v),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => {
                    return Err(MemError::Storage(format!(
                        "failed to query active anchor metadata: {e}"
                    )));
                }
            };
            let (title, aliases) = if let Some((t, aliases_joined)) = row {
                let parsed_aliases = if aliases_joined.is_empty() {
                    Vec::new()
                } else {
                    aliases_joined
                        .split(" | ")
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                };
                (t, parsed_aliases)
            } else {
                let stem = path
                    .file_name()
                    .unwrap_or("note")
                    .strip_suffix(".md")
                    .unwrap_or_else(|| path.file_name().unwrap_or("note"))
                    .to_string();
                (stem, Vec::new())
            };
            anchors.push(ActiveAnchor {
                path: path.clone(),
                title,
                aliases,
            });
        }
        Ok(())
    })?;

    Ok(anchors)
}

fn contains_term_boundary(haystack_lower: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return false;
    }
    let ascii_bounded = needle_lower.is_ascii();
    let mut start = 0usize;
    while let Some(rel) = haystack_lower[start..].find(needle_lower) {
        let abs = start + rel;
        let end = abs + needle_lower.len();
        if !ascii_bounded || has_ascii_word_boundaries(haystack_lower, abs, end) {
            return true;
        }
        start = abs + needle_lower.chars().next().map(char::len_utf8).unwrap_or(1);
    }
    false
}

fn has_ascii_word_boundaries(text: &str, start: usize, end: usize) -> bool {
    let left_ok = text[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
    let right_ok = text[end..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
    left_ok && right_ok
}

/// Inserts inline `[[Wikilinks]]` for matched active note titles/aliases while respecting
/// existing `[[...]]` links and backtick code spans/fences.
pub(crate) fn weave_inline_wikilinks(body: &str, anchors: &[(String, String)]) -> String {
    if anchors.is_empty() {
        return body.to_string();
    }

    let mut out_lines = Vec::new();
    let mut in_code_fence = false;
    let mut linked_targets = HashSet::new();

    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code_fence = !in_code_fence;
            out_lines.push(line.to_string());
            continue;
        }
        if in_code_fence || trimmed.starts_with('#') {
            out_lines.push(line.to_string());
            continue;
        }

        let mut current = line.to_string();
        for (needle, target_title) in anchors {
            if linked_targets.contains(&target_title.to_ascii_lowercase()) {
                continue;
            }
            if let Some(updated) = replace_first_unprotected(&current, needle, target_title) {
                current = updated;
                linked_targets.insert(target_title.to_ascii_lowercase());
            }
        }
        out_lines.push(current);
    }

    out_lines.join("\n")
}

fn replace_first_unprotected(line: &str, needle: &str, target_title: &str) -> Option<String> {
    let line_lower = line.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    let ascii_bounded = needle.is_ascii();

    let protected = compute_protected_byte_mask(line);
    let mut cursor = 0usize;

    while let Some(rel) = line_lower[cursor..].find(&needle_lower) {
        let abs = cursor + rel;
        let end = abs + needle_lower.len();
        if end <= line.len()
            && line.is_char_boundary(abs)
            && line.is_char_boundary(end)
            && !protected[abs..end].iter().any(|&p| p)
            && (!ascii_bounded || has_ascii_word_boundaries(&line_lower, abs, end))
        {
            let matched_slice = &line[abs..end];
            let replacement = if matched_slice == target_title {
                format!("[[{target_title}]]")
            } else {
                format!("[[{target_title}|{matched_slice}]]")
            };
            let mut result = String::with_capacity(line.len() + replacement.len());
            result.push_str(&line[..abs]);
            result.push_str(&replacement);
            result.push_str(&line[end..]);
            return Some(result);
        }
        cursor = abs + needle_lower.chars().next().map(char::len_utf8).unwrap_or(1);
    }

    None
}

fn compute_protected_byte_mask(line: &str) -> Vec<bool> {
    let mut mask = vec![false; line.len()];
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Protect [[...]] wikilinks
        if i + 1 < bytes.len() && bytes[i] == b'[' && bytes[i + 1] == b'[' {
            let start = i;
            if let Some(rel_end) = line[start + 2..].find("]]") {
                let end = start + 2 + rel_end + 2;
                mask[start..end].fill(true);
                i = end;
                continue;
            }
        }
        // Protect `inline code`
        if bytes[i] == b'`' {
            let start = i;
            if let Some(rel_end) = line[start + 1..].find('`') {
                let end = start + 1 + rel_end + 1;
                mask[start..end].fill(true);
                i = end;
                continue;
            }
        }
        // Protect [text](url) markdown links
        if bytes[i] == b'[' {
            if let Some(rel_close) = line[i..].find("](") {
                let after = i + rel_close + 2;
                if let Some(rel_paren) = line[after..].find(')') {
                    let end = after + rel_paren + 1;
                    mask[i..end].fill(true);
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    mask
}

#[allow(clippy::too_many_arguments)]
fn render_deterministic_markdown(
    role: IngestRole,
    tier: MemoryTier,
    title: &str,
    raw_text: &str,
    weave_pairs: &[(String, String)],
    linked_notes: &[String],
    tags: &[String],
    importance: f32,
    ingested_at: &str,
) -> String {
    let stripped_body = strip_leading_frontmatter(raw_text);
    let woven_body = weave_inline_wikilinks(stripped_body, weave_pairs);
    let section_heading = match role {
        IngestRole::Pm => "## Summary & Product Context",
        IngestRole::Dev => "## Summary & Technical Notes",
        IngestRole::Bot => "## Observation & Context",
    };

    let mut out = render_frontmatter(title, role, tier, tags, importance, ingested_at);
    out.push_str(&format!("# {title}\n\n"));

    if woven_body.trim_start().starts_with('#') {
        out.push_str(woven_body.trim());
        out.push('\n');
    } else {
        out.push_str(section_heading);
        out.push_str("\n\n");
        out.push_str(woven_body.trim());
        out.push('\n');
    }

    if !linked_notes.is_empty() {
        out.push_str("\n## Related\n");
        for link_title in linked_notes {
            out.push_str(&format!("- [[{link_title}]]\n"));
        }
    }

    out
}

fn strip_leading_frontmatter(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    if let Some(after_start) = trimmed.strip_prefix("---") {
        let after_nl = after_start
            .strip_prefix("\r\n")
            .or_else(|| after_start.strip_prefix('\n'));
        if let Some(rest) = after_nl {
            if let Some(end_idx) = rest.find("\n---") {
                let after_end = &rest[end_idx + 4..];
                return after_end.trim_start_matches(['\r', '\n']);
            }
        }
    }
    trimmed
}

fn render_frontmatter(
    title: &str,
    role: IngestRole,
    tier: MemoryTier,
    tags: &[String],
    importance: f32,
    ingested_at: &str,
) -> String {
    let escaped_title = title.replace('"', "\\\"");
    let tag_list = tags
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "---\ntitle: \"{escaped_title}\"\nrole: \"{}\"\ntier: \"{}\"\ntags: [{tag_list}]\nimportance: {importance:.2}\ningested_at: \"{ingested_at}\"\n---\n\n",
        role.as_str(),
        tier.as_str(),
    )
}

#[allow(clippy::too_many_arguments)]
fn try_format_with_llm(
    cfg: &ConsolidateLlmConfig,
    role: IngestRole,
    tier: MemoryTier,
    title: &str,
    raw_text: &str,
    linked_notes: &[String],
    tags: &[String],
    importance: f32,
    ingested_at: &str,
) -> Option<String> {
    match cfg.backend {
        ProviderBackend::Disabled => return None,
        ProviderBackend::OpenAi
            if cfg
                .api_key
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty() =>
        {
            return None;
        }
        _ => {}
    }

    let endpoint = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let related_hint = if linked_notes.is_empty() {
        "(none)".to_string()
    } else {
        linked_notes
            .iter()
            .map(|t| format!("[[{t}]]"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let role_instruction = match role {
        IngestRole::Pm => {
            "Role: Product Manager. Organize into concise sections: Summary, Business Rules / Acceptance Criteria, Open Questions, Related."
        }
        IngestRole::Dev => {
            "Role: Software Engineer. Organize into concise sections: Summary, Architecture & Invariants, Operational Notes, Related."
        }
        IngestRole::Bot => {
            "Role: AI Agent. Organize into concise sections: Observation, Context & Trigger, Recommended Follow-up, Related."
        }
    };

    let system_prompt = format!(
        "You are an Obsidian knowledge curator for Engram (`mem`). Rewrite the raw input into clean Markdown (WITHOUT wrapping in ```markdown fences). Keep all facts verbatim. {role_instruction} Reuse and weave these active Wikilinks where relevant: {related_hint}."
    );
    let user_prompt = format!(
        "Suggested Title: {title}\nTarget Tier: {}\nRaw Note:\n{raw_text}",
        tier.as_str()
    );

    let payload = serde_json::json!({
        "model": cfg.model,
        "temperature": 0.1,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": user_prompt }
        ]
    });
    let body = serde_json::to_string(&payload).ok()?;
    let resp_raw =
        post_json_http_or_curl(&endpoint, cfg.api_key.as_deref(), &body, LLM_TIMEOUT).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&resp_raw).ok()?;
    let content = parsed
        .pointer("/choices/0/message/content")?
        .as_str()?
        .trim();
    if content.is_empty() {
        return None;
    }

    let clean_content = unwrap_markdown_fence(content);
    let mut final_md = if clean_content.starts_with("---") {
        clean_content.to_string()
    } else {
        let mut fm = render_frontmatter(title, role, tier, tags, importance, ingested_at);
        fm.push_str(clean_content);
        if !fm.ends_with('\n') {
            fm.push('\n');
        }
        fm
    };

    if !linked_notes.is_empty() {
        let missing_links: Vec<&String> = linked_notes
            .iter()
            .filter(|t| !final_md.contains(&format!("[[{t}")))
            .collect();
        if !missing_links.is_empty() {
            if !final_md.ends_with('\n') {
                final_md.push('\n');
            }
            final_md.push_str("\n## Related\n");
            for link in missing_links {
                final_md.push_str(&format!("- [[{link}]]\n"));
            }
        }
    }

    Some(final_md)
}

fn unwrap_markdown_fence(s: &str) -> &str {
    let trimmed = s.trim();
    if let Some(rest) = trimmed
        .strip_prefix("```markdown")
        .or_else(|| trimmed.strip_prefix("```md"))
    {
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use mem_core::{MemoryNode, VirtualPath};
    use mem_store::SqliteStore;
    use mem_vcs::{MemVcs, VersionControlEngine};

    use super::*;

    fn test_engine() -> MemSearchEngine {
        let store = SqliteStore::open_in_memory().expect("open memory store");
        let vcs = MemVcs::new(store);
        vcs.init().expect("init main branch");
        MemSearchEngine::new_bm25_only(vcs)
    }

    #[test]
    fn role_defaults_and_slugs_route_cleanly() {
        assert_eq!(IngestRole::parse("PM").unwrap(), IngestRole::Pm);
        assert_eq!(IngestRole::parse("developer").unwrap(), IngestRole::Dev);
        assert_eq!(IngestRole::parse("agent").unwrap(), IngestRole::Bot);

        assert_eq!(IngestRole::Pm.default_tier(), MemoryTier::Semantic);
        assert_eq!(IngestRole::Dev.default_tier(), MemoryTier::Semantic);
        assert_eq!(IngestRole::Bot.default_tier(), MemoryTier::Episodic);

        assert_eq!(
            IngestRole::Pm.default_dir(MemoryTier::Semantic),
            "/semantic/product"
        );
        assert_eq!(
            IngestRole::Dev.default_dir(MemoryTier::Procedural),
            "/procedural/runbooks"
        );
        assert_eq!(IngestRole::Pm.draft_branch(), "draft/pm-inbox");

        assert_eq!(
            slugify_title("Checkout Rate Limit v2!"),
            "checkout-rate-limit-v2"
        );
        assert_eq!(slugify_title("支付超时 重试策略"), "支付超时-重试策略");
    }

    #[test]
    fn inline_wikilink_weaving_respects_existing_links_and_code_spans() {
        let body = "Check `WAL Mode` and [[WAL Mode]] before changing WAL Mode retries.";
        let anchors = vec![("WAL Mode".to_string(), "WAL Mode".to_string())];
        let woven = weave_inline_wikilinks(body, &anchors);
        assert_eq!(
            woven,
            "Check `WAL Mode` and [[WAL Mode]] before changing [[WAL Mode]] retries."
        );
    }

    #[test]
    fn ingest_weaves_active_links_and_isolates_draft_branches() {
        let engine = test_engine();
        let existing_path = VirtualPath::new("/semantic/architecture/wal-mode.md").unwrap();
        let existing_doc = "---\ntitle: \"SQLite WAL Mode\"\naliases: [\"WAL\"]\n---\n\n# SQLite WAL Mode\n\nConcurrent readers never block writers.\n";
        let blob = engine
            .vcs()
            .write_node(
                "main",
                MemoryNode::new(existing_path.clone(), existing_doc.as_bytes().to_vec()),
            )
            .unwrap();
        engine.index_blob_if_needed(&existing_path, &blob).unwrap();
        engine
            .vcs()
            .commit("main", "dev@engram", "seed architecture note")
            .unwrap();

        let draft_req = IngestRequest::new(
            IngestRole::Pm,
            "Checkout retry policy must respect SQLite WAL concurrency limits during traffic spikes.",
        )
        .with_title("Checkout Retry Policy")
        .with_tags(["checkout", "pm-prd"])
        .with_draft(true);

        let outcome = ingest_knowledge(&engine, &draft_req, None).unwrap();
        assert_eq!(outcome.branch, "draft/pm-inbox");
        assert_eq!(
            outcome.path.as_str(),
            "/semantic/product/checkout-retry-policy.md"
        );
        assert!(outcome.commit_id.is_some());
        assert!(!outcome.used_llm);
        assert!(outcome.markdown.contains("[[SQLite WAL Mode"));
        assert!(outcome.tags.iter().any(|t| t == "role/pm"));
        assert!(outcome.tags.iter().any(|t| t == "checkout"));

        // Must NOT leak onto main before explicit merge
        assert!(engine
            .vcs()
            .read(
                &Revision::BranchWorkspace("main".to_string()),
                &outcome.path
            )
            .unwrap()
            .is_none());
        assert!(engine
            .vcs()
            .read(
                &Revision::BranchWorkspace("draft/pm-inbox".to_string()),
                &outcome.path
            )
            .unwrap()
            .is_some());
    }

    #[test]
    fn ingest_uses_openai_compatible_llm_when_enabled_and_reachable() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock llm");
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let req_text = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(req_text.contains("POST /v1/chat/completions"));
            assert!(req_text.contains("gpt-6-luna"));

            let body = serde_json::json!({
                "id": "chatcmpl-test",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "# Payment Timeout Policy\n\n## Summary\n\nRetry idempotent payments up to 3 times.\n"
                    }
                }]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });

        let engine = test_engine();
        let llm_cfg = ConsolidateLlmConfig {
            backend: ProviderBackend::OpenAi,
            base_url: format!("http://{addr}/v1"),
            model: "gpt-6-luna".to_string(),
            api_key: Some("sk-test-123".to_string()),
        };

        let req = IngestRequest::new(
            IngestRole::Dev,
            "payment retries should cap at 3 attempts with idempotency key",
        )
        .with_title("Payment Timeout Policy")
        .with_use_llm(true);

        let outcome = ingest_knowledge(&engine, &req, Some(&llm_cfg)).unwrap();
        assert!(outcome.used_llm);
        assert!(outcome
            .markdown
            .contains("Retry idempotent payments up to 3 times."));
        assert!(outcome.markdown.contains("role: \"dev\""));
        server.join().unwrap();
    }
}
