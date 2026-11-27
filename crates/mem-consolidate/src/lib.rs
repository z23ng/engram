//! Cold-path isolated-branch episodic memory consolidation engine for Engram.
//!
//! Scans `/episodic/` trajectory logs on a source branch, deduplicates and distills
//! durable `/semantic/` knowledge cards and `/procedural/` SOP runbooks inside an
//! isolated `consolidate/*` branch, weaves `[[Wikilinks]]` against active notes,
//! and optionally merges back into the source branch after verification.

pub mod extractor;
pub mod llm;

use std::collections::HashSet;

use chrono::Utc;
use mem_core::{
    BlobId, CommitId, DiffEntry, MemError, MemoryMetadata, MemoryNode, MemoryTier, MergeConflict,
    MergeOutcome, MergeStrategy, Result, Revision, VirtualPath,
};
use mem_search::{
    denoise_markdown, mark_rewind_events_mined, mine_branch, parse_wiki_document, redact_secrets,
    ConsolidateLlmConfig, MemSearchEngine,
};
use mem_store::cas::get_blob_conn;
use mem_vcs::VersionControlEngine;
use serde::{Deserialize, Serialize};

use crate::extractor::{
    extract_deterministic_cards, render_card_markdown, BranchAnchor, DistilledCandidate,
    SourceEpisode,
};
use crate::llm::consolidate_with_llm;
pub use crate::llm::LlmChatTransport;

fn default_true() -> bool {
    true
}

/// Parameters controlling a single cold-path consolidation run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationRequest {
    /// Source branch whose episodic logs will be distilled (default `"main"`).
    pub source_branch: String,
    /// Optional explicit isolated branch name (defaults to `consolidate/<timestamp>-<hash>`).
    pub consolidation_branch: Option<String>,
    /// Virtual path prefix scanned for episodic logs (default `"/episodic"`).
    pub episodic_prefix: VirtualPath,
    /// Target directory prefix for distilled semantic cards (default `"/semantic/consolidated"`).
    pub semantic_target_dir: VirtualPath,
    /// Target directory prefix for distilled procedural SOPs (default `"/procedural/consolidated"`).
    pub procedural_target_dir: VirtualPath,
    /// Only scan episodic nodes updated at or after this Unix millisecond timestamp.
    pub since_ms: Option<i64>,
    /// Maximum number of episodic nodes inspected in this pass (default `50`).
    pub max_notes: usize,
    /// Minimum importance threshold for source episodic nodes (default `0.0`).
    pub min_importance: f32,
    /// Attempt optional OpenAI-compatible / Ollama LLM distillation before deterministic fallback.
    pub use_llm: bool,
    /// If `true`, return an error instead of falling back when LLM distillation fails.
    pub strict_llm: bool,
    /// Automatically merge the isolated `consolidate/*` branch back into `source_branch`.
    pub auto_merge: bool,
    /// Merge conflict resolution strategy when `auto_merge == true`.
    pub merge_strategy: MergeStrategy,
    /// Delete the temporary `consolidate/*` branch after a clean fast-forward or merge.
    pub delete_branch_on_merge: bool,
    /// Write an archival digest under `/episodic/archive/` and decay processed episode importance.
    pub archive_processed: bool,
    /// Multiplier applied to processed episode importance when `archive_processed == true`.
    pub decay_factor: f32,
    /// Preview distilled cards without creating branches, blobs, or commits.
    pub dry_run: bool,
    /// Author recorded on the consolidation commit.
    pub author: String,
    /// Mine unmined `rewind_events` and topology/promotion signals on `source_branch`.
    #[serde(default = "default_true")]
    pub mine_vcs_signals: bool,
    /// Sweep TTL-expired memory paths onto the isolated consolidation branch.
    #[serde(default = "default_true")]
    pub sweep_expired: bool,
}

impl Default for ConsolidationRequest {
    fn default() -> Self {
        Self {
            source_branch: "main".to_string(),
            consolidation_branch: None,
            episodic_prefix: VirtualPath::new("/episodic").expect("valid /episodic path"),
            semantic_target_dir: VirtualPath::new("/semantic/consolidated")
                .expect("valid /semantic/consolidated path"),
            procedural_target_dir: VirtualPath::new("/procedural/consolidated")
                .expect("valid /procedural/consolidated path"),
            since_ms: None,
            max_notes: 50,
            min_importance: 0.0,
            use_llm: false,
            strict_llm: false,
            auto_merge: false,
            merge_strategy: MergeStrategy::Auto,
            delete_branch_on_merge: false,
            archive_processed: false,
            decay_factor: 0.85,
            dry_run: false,
            author: "consolidator@engram".to_string(),
            mine_vcs_signals: true,
            sweep_expired: true,
        }
    }
}

impl ConsolidationRequest {
    /// Creates a default consolidation request targeting `source_branch`.
    pub fn new(source_branch: impl Into<String>) -> Self {
        Self {
            source_branch: source_branch.into(),
            ..Self::default()
        }
    }

    pub fn with_consolidation_branch(mut self, branch: impl Into<String>) -> Self {
        self.consolidation_branch = Some(branch.into());
        self
    }

    pub fn with_episodic_prefix(mut self, prefix: VirtualPath) -> Self {
        self.episodic_prefix = prefix;
        self
    }

    pub fn with_since_ms(mut self, since_ms: i64) -> Self {
        self.since_ms = Some(since_ms);
        self
    }

    pub fn with_max_notes(mut self, max_notes: usize) -> Self {
        self.max_notes = max_notes;
        self
    }

    pub fn with_min_importance(mut self, min_importance: f32) -> Self {
        self.min_importance = min_importance;
        self
    }

    pub fn with_use_llm(mut self, use_llm: bool) -> Self {
        self.use_llm = use_llm;
        self
    }

    pub fn with_strict_llm(mut self, strict_llm: bool) -> Self {
        self.strict_llm = strict_llm;
        self
    }

    pub fn with_auto_merge(mut self, auto_merge: bool) -> Self {
        self.auto_merge = auto_merge;
        self
    }

    pub fn with_merge_strategy(mut self, strategy: MergeStrategy) -> Self {
        self.merge_strategy = strategy;
        self
    }

    pub fn with_delete_branch_on_merge(mut self, delete_branch: bool) -> Self {
        self.delete_branch_on_merge = delete_branch;
        self
    }

    pub fn with_archive_processed(mut self, archive: bool) -> Self {
        self.archive_processed = archive;
        self
    }

    pub fn with_decay_factor(mut self, decay_factor: f32) -> Self {
        self.decay_factor = decay_factor;
        self
    }

    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    pub fn with_mine_vcs_signals(mut self, mine: bool) -> Self {
        self.mine_vcs_signals = mine;
        self
    }

    pub fn with_sweep_expired(mut self, sweep: bool) -> Self {
        self.sweep_expired = sweep;
        self
    }
}

/// Summary of a single semantic or procedural card produced during consolidation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DistilledCard {
    /// Target memory tier (`Semantic` or `Procedural`).
    pub tier: MemoryTier,
    /// Target VFS path where the card was written (or would be written in `dry_run`).
    pub path: VirtualPath,
    /// Note title.
    pub title: String,
    /// BLAKE3 blob hash of the rendered card (`None` during `dry_run`).
    pub blob_id: Option<BlobId>,
    /// Source episodic paths distilled into this card.
    pub source_paths: Vec<VirtualPath>,
    /// Active branch note titles woven via `[[Wikilinks]]`.
    pub linked_notes: Vec<String>,
    /// Rendered Obsidian Markdown content.
    pub markdown: String,
}

/// Merge outcome reported after committing the isolated `consolidate/*` branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ConsolidationMergeStatus {
    /// Consolidation remained on its isolated branch (`auto_merge == false` or `dry_run == true`).
    Skipped,
    /// Source branch fast-forwarded cleanly to the consolidation commit.
    FastForward { commit_id: CommitId },
    /// Three-way merge created a merge commit on `source_branch`.
    Merged { commit_id: CommitId },
    /// Auto-merge encountered conflicts; `consolidate_branch` is preserved intact for review.
    Conflict {
        base_commit: CommitId,
        conflicts: Vec<MergeConflict>,
    },
}

/// Complete structured report produced by [`ConsolidationEngine::consolidate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationReport {
    /// Source branch scanned for episodic logs.
    pub source_branch: String,
    /// Isolated consolidation branch created/updated (`None` if `dry_run` or no episodes found).
    pub consolidation_branch: Option<String>,
    /// Whether `dry_run` prevented repository mutations.
    pub dry_run: bool,
    /// Whether an LLM backend succeeded for distillation.
    pub used_llm: bool,
    /// Non-fatal fallback notice when LLM was requested but unavailable.
    pub llm_fallback_reason: Option<String>,
    /// Number of episodic logs scanned.
    pub episodes_scanned: usize,
    /// Virtual paths of scanned episodic logs.
    pub source_episode_paths: Vec<VirtualPath>,
    /// Distilled `/semantic/` and `/procedural/` cards generated.
    pub distilled_cards: Vec<DistilledCard>,
    /// Archival digest path written on `consolidate_branch` when `archive_processed == true`.
    pub archive_path: Option<VirtualPath>,
    /// Commit created on `consolidate_branch`.
    pub commit_id: Option<CommitId>,
    /// Merkle diff between `source_branch` and `consolidate_branch`.
    pub diff_from_source: Vec<DiffEntry>,
    /// Merge outcome back into `source_branch`.
    pub merge_status: ConsolidationMergeStatus,
    /// Whether the temporary `consolidate_branch` was deleted after merge.
    pub consolidation_branch_deleted: bool,
    /// Number of rewind-mined pitfall cards distilled onto the consolidation branch.
    #[serde(default)]
    pub pitfalls_distilled: usize,
    /// Number of TTL-expired nodes deleted on the consolidation branch.
    #[serde(default)]
    pub expired_swept: usize,
    /// Unresolved `[[Wikilink]]` targets discovered during branch topology mining.
    #[serde(default)]
    pub dangling_links: Vec<String>,
    /// High-utility active paths recommended for tier promotion.
    #[serde(default)]
    pub promotion_candidates: Vec<VirtualPath>,
}

/// Cold-path background memory consolidator running exclusively on isolated branches.
#[derive(Clone)]
pub struct ConsolidationEngine {
    search: MemSearchEngine,
    llm_config: ConsolidateLlmConfig,
    llm_transport: Option<LlmChatTransport>,
}

impl std::fmt::Debug for ConsolidationEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsolidationEngine")
            .field("llm_config", &self.llm_config)
            .field("has_custom_transport", &self.llm_transport.is_some())
            .finish()
    }
}

impl ConsolidationEngine {
    /// Creates a new consolidation engine with deterministic local extraction (`Disabled` LLM).
    pub fn new(search: MemSearchEngine) -> Self {
        Self {
            search,
            llm_config: ConsolidateLlmConfig::disabled(),
            llm_transport: None,
        }
    }

    /// Configures an OpenAI-compatible or Ollama cold-path LLM endpoint.
    pub fn with_llm_config(mut self, cfg: ConsolidateLlmConfig) -> Self {
        self.llm_config = cfg;
        self
    }

    /// Injects a custom HTTP transport callback for LLM completions (useful in tests).
    pub fn with_llm_transport(mut self, transport: LlmChatTransport) -> Self {
        self.llm_transport = Some(transport);
        self
    }

    /// Returns a reference to the underlying [`MemSearchEngine`].
    pub fn search(&self) -> &MemSearchEngine {
        &self.search
    }

    /// Scans `/episodic/` on `req.source_branch`, distills durable cards inside an isolated
    /// `consolidate/*` branch, and optionally merges back into `req.source_branch`.
    pub fn consolidate(&self, req: &ConsolidationRequest) -> Result<ConsolidationReport> {
        let vcs = self.search.vcs();
        let source_head = vcs.head_commit(&req.source_branch)?;
        let source_rev = Revision::BranchWorkspace(req.source_branch.clone());

        let episodes = self.collect_episodes(&source_rev, req)?;
        let source_episode_paths: Vec<VirtualPath> =
            episodes.iter().map(|e| e.path.clone()).collect();

        let mining_report = if req.mine_vcs_signals || req.sweep_expired {
            Some(mine_branch(&self.search, &req.source_branch)?)
        } else {
            None
        };
        let rewind_pitfalls = if req.mine_vcs_signals {
            mining_report
                .as_ref()
                .map(|r| r.rewind_pitfalls.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let expired_paths = if req.sweep_expired {
            let mut raw_expired = mining_report
                .as_ref()
                .map(|r| r.expired_paths.clone())
                .unwrap_or_default();
            if !raw_expired.is_empty() {
                let committed_rev = Revision::BranchHead(req.source_branch.clone());
                let committed_tree = vcs.store().with_conn(|conn| {
                    mem_vcs::tree::resolve_effective_tree(conn, &committed_rev, None)
                })?;
                raw_expired.retain(|p| committed_tree.contains_key(p));
            }
            raw_expired
        } else {
            Vec::new()
        };
        let dangling_links: Vec<String> = mining_report
            .as_ref()
            .map(|r| {
                r.dangling_links
                    .iter()
                    .map(|d| d.target_ref.clone())
                    .collect()
            })
            .unwrap_or_default();
        let promotion_candidates: Vec<VirtualPath> = mining_report
            .as_ref()
            .map(|r| {
                r.promotion_candidates
                    .iter()
                    .map(|c| c.path.clone())
                    .collect()
            })
            .unwrap_or_default();

        if episodes.is_empty() && rewind_pitfalls.is_empty() && expired_paths.is_empty() {
            return Ok(ConsolidationReport {
                source_branch: req.source_branch.clone(),
                consolidation_branch: None,
                dry_run: req.dry_run,
                used_llm: false,
                llm_fallback_reason: None,
                episodes_scanned: 0,
                source_episode_paths,
                distilled_cards: Vec::new(),
                archive_path: None,
                commit_id: None,
                diff_from_source: Vec::new(),
                merge_status: ConsolidationMergeStatus::Skipped,
                consolidation_branch_deleted: false,
                pitfalls_distilled: 0,
                expired_swept: 0,
                dangling_links,
                promotion_candidates,
            });
        }

        let (mut candidates, used_llm, llm_fallback_reason) = if episodes.is_empty() {
            (Vec::new(), false, None)
        } else if req.use_llm {
            match consolidate_with_llm(&self.llm_config, self.llm_transport.as_ref(), &episodes) {
                Ok(llm_cards) => (llm_cards, true, None),
                Err(err) => {
                    if req.strict_llm {
                        return Err(err);
                    }
                    (
                        extract_deterministic_cards(&episodes),
                        false,
                        Some(err.to_string()),
                    )
                }
            }
        } else {
            (extract_deterministic_cards(&episodes), false, None)
        };

        let pitfall_dir = VirtualPath::new("/procedural/pitfalls")?;
        let mut pitfall_event_ids = Vec::with_capacity(rewind_pitfalls.len());
        let pitfalls_distilled = rewind_pitfalls.len();
        for pitfall in rewind_pitfalls {
            pitfall_event_ids.push(pitfall.event_id);
            let short_hex = &pitfall.abandoned_commit.to_hex()[..8];
            let mut stmts = vec![pitfall.summary];
            stmts.extend(pitfall.rules);
            candidates.push(DistilledCandidate {
                tier: MemoryTier::Procedural,
                target_dir_override: Some(pitfall_dir.clone()),
                title: format!("Rewind Pitfall {short_hex}"),
                slug: format!("rewind-{short_hex}"),
                aliases: vec![format!("pitfall-{short_hex}")],
                tags: vec![
                    "procedural".to_string(),
                    "pitfall".to_string(),
                    "rewind-mined".to_string(),
                    "consolidated".to_string(),
                ],
                entities: Vec::new(),
                statements: stmts,
                source_paths: pitfall.reverted_paths,
                importance: 0.85,
            });
        }

        let anchors = self.load_active_anchors(&source_rev)?;
        let consolidated_at = Utc::now().to_rfc3339();

        let mut prepared_cards: Vec<(DistilledCandidate, VirtualPath, String, Vec<String>)> =
            Vec::new();
        let mut used_paths: HashSet<VirtualPath> = HashSet::new();

        for cand in candidates {
            let target_path = resolve_card_path(
                &cand,
                &req.semantic_target_dir,
                &req.procedural_target_dir,
                &mut used_paths,
            )?;
            let existing_md = vcs
                .read(&source_rev, &target_path)?
                .and_then(|node| String::from_utf8(node.content).ok());
            let (markdown, linked_notes, _) = render_card_markdown(
                &cand,
                existing_md.as_deref(),
                &anchors,
                &target_path,
                &consolidated_at,
            );
            prepared_cards.push((cand, target_path, markdown, linked_notes));
        }

        if req.dry_run {
            let distilled_cards = prepared_cards
                .into_iter()
                .map(|(cand, path, markdown, linked_notes)| DistilledCard {
                    tier: cand.tier,
                    path,
                    title: cand.title,
                    blob_id: None,
                    source_paths: cand.source_paths,
                    linked_notes,
                    markdown,
                })
                .collect();

            return Ok(ConsolidationReport {
                source_branch: req.source_branch.clone(),
                consolidation_branch: None,
                dry_run: true,
                used_llm,
                llm_fallback_reason,
                episodes_scanned: episodes.len(),
                source_episode_paths,
                distilled_cards,
                archive_path: None,
                commit_id: None,
                diff_from_source: Vec::new(),
                merge_status: ConsolidationMergeStatus::Skipped,
                consolidation_branch_deleted: false,
                pitfalls_distilled,
                expired_swept: expired_paths.len(),
                dangling_links,
                promotion_candidates,
            });
        }

        let run_id = build_run_id(&source_head, &episodes);
        let consolidate_branch = self.ensure_consolidation_branch(req, &run_id)?;

        let mut distilled_cards = Vec::with_capacity(prepared_cards.len());
        for (cand, path, markdown, linked_notes) in prepared_cards {
            let mut tags = cand.tags.clone();
            if !tags.iter().any(|t| t == "consolidated") {
                tags.push("consolidated".to_string());
            }
            let node = MemoryNode::new(path.clone(), markdown.as_bytes().to_vec())
                .with_tags(tags)
                .with_importance(cand.importance);
            let blob_id = vcs.write_node(&consolidate_branch, node)?;
            self.search.index_blob_if_needed(&path, &blob_id)?;

            distilled_cards.push(DistilledCard {
                tier: cand.tier,
                path,
                title: cand.title,
                blob_id: Some(blob_id),
                source_paths: cand.source_paths,
                linked_notes,
                markdown,
            });
        }

        let mut expired_swept = 0usize;
        for exp_path in &expired_paths {
            expired_swept += vcs.delete_path(&consolidate_branch, exp_path)?;
        }

        let archive_path = if req.archive_processed && !episodes.is_empty() {
            Some(self.archive_processed_episodes(
                &consolidate_branch,
                &run_id,
                &episodes,
                &distilled_cards,
                req.decay_factor,
                &consolidated_at,
            )?)
        } else {
            None
        };

        let commit_msg = format!(
            "consolidate({}): distilled {} card(s) from {} episode(s)",
            req.source_branch,
            distilled_cards.len(),
            episodes.len()
        );
        let commit_id = vcs.commit(&consolidate_branch, &req.author, &commit_msg)?;
        if !pitfall_event_ids.is_empty() {
            mark_rewind_events_mined(vcs.store(), &pitfall_event_ids)?;
        }

        let diff_from_source = vcs.diff(
            &Revision::BranchHead(req.source_branch.clone()),
            &Revision::BranchHead(consolidate_branch.clone()),
        )?;

        let mut branch_deleted = false;
        let merge_status = if req.auto_merge {
            match vcs.merge(&consolidate_branch, &req.source_branch, req.merge_strategy)? {
                MergeOutcome::FastForward(cid) => {
                    if req.delete_branch_on_merge && consolidate_branch != "main" {
                        vcs.delete_branch(&consolidate_branch)?;
                        branch_deleted = true;
                    }
                    ConsolidationMergeStatus::FastForward { commit_id: cid }
                }
                MergeOutcome::Merged(cid) => {
                    if req.delete_branch_on_merge && consolidate_branch != "main" {
                        vcs.delete_branch(&consolidate_branch)?;
                        branch_deleted = true;
                    }
                    ConsolidationMergeStatus::Merged { commit_id: cid }
                }
                MergeOutcome::Conflict {
                    base_commit,
                    conflicts,
                } => ConsolidationMergeStatus::Conflict {
                    base_commit,
                    conflicts,
                },
            }
        } else {
            ConsolidationMergeStatus::Skipped
        };

        Ok(ConsolidationReport {
            source_branch: req.source_branch.clone(),
            consolidation_branch: Some(consolidate_branch),
            dry_run: false,
            used_llm,
            llm_fallback_reason,
            episodes_scanned: episodes.len(),
            source_episode_paths,
            distilled_cards,
            archive_path,
            commit_id: Some(commit_id),
            diff_from_source,
            merge_status,
            consolidation_branch_deleted: branch_deleted,
            pitfalls_distilled,
            expired_swept,
            dangling_links,
            promotion_candidates,
        })
    }

    fn collect_episodes(
        &self,
        rev: &Revision,
        req: &ConsolidationRequest,
    ) -> Result<Vec<SourceEpisode>> {
        let vcs = self.search.vcs();
        let entries = vcs.list(rev, Some(&req.episodic_prefix))?;
        let mut episodes = Vec::new();

        for (path, blob_id, meta) in entries {
            let p_str = path.as_str();
            if p_str.starts_with("/episodic/archive/")
                || p_str == "/episodic/archive"
                || p_str.starts_with("/episodic/consolidation-reports/")
            {
                continue;
            }
            if meta
                .tags
                .iter()
                .any(|t| t.eq_ignore_ascii_case("archived-episode"))
            {
                continue;
            }
            if let Some(since) = req.since_ms {
                if meta.updated_at_ms < since {
                    continue;
                }
            }
            if meta.importance < req.min_importance {
                continue;
            }

            let raw_bytes = vcs.store().with_conn(|conn| {
                get_blob_conn(conn, &blob_id)?
                    .map(|(b, _)| b)
                    .ok_or_else(|| MemError::BlobNotFound(blob_id.to_hex()))
            })?;
            let Ok(raw_raw_text) = String::from_utf8(raw_bytes) else {
                continue;
            };
            let (redacted, _) = redact_secrets(&raw_raw_text);
            let raw_text = denoise_markdown(&redacted, true);
            let parsed = parse_wiki_document(&path, &raw_text, 800);
            let mut merged_tags = meta.tags.clone();
            for t in parsed.tags {
                if !merged_tags.iter().any(|existing| existing == &t) {
                    merged_tags.push(t);
                }
            }

            episodes.push(SourceEpisode {
                path,
                title: parsed.title,
                tags: merged_tags,
                entities: parsed.entities,
                wikilinks: parsed.wikilinks,
                importance: meta.importance,
                updated_at_ms: meta.updated_at_ms,
                raw_text,
            });
        }

        episodes.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| a.path.cmp(&b.path))
        });
        if req.max_notes > 0 && episodes.len() > req.max_notes {
            episodes.truncate(req.max_notes);
        }

        Ok(episodes)
    }

    fn load_active_anchors(&self, rev: &Revision) -> Result<Vec<BranchAnchor>> {
        let vcs = self.search.vcs();
        let entries = vcs.list(rev, None)?;
        let mut anchors = Vec::new();

        for (path, blob_id, _) in entries {
            if path.tier() == MemoryTier::Episodic {
                continue;
            }
            let raw_bytes = vcs.store().with_conn(|conn| {
                get_blob_conn(conn, &blob_id)?
                    .map(|(b, _)| b)
                    .ok_or_else(|| MemError::BlobNotFound(blob_id.to_hex()))
            })?;
            let Ok(raw_raw_text) = String::from_utf8(raw_bytes) else {
                continue;
            };
            let (redacted, _) = redact_secrets(&raw_raw_text);
            let raw_text = denoise_markdown(&redacted, true);
            let parsed = parse_wiki_document(&path, &raw_text, 800);
            anchors.push(BranchAnchor {
                path,
                title: parsed.title,
                aliases: parsed.aliases,
                tags: parsed.tags,
            });
        }
        Ok(anchors)
    }

    fn ensure_consolidation_branch(
        &self,
        req: &ConsolidationRequest,
        run_id: &str,
    ) -> Result<String> {
        let vcs = self.search.vcs();
        if let Some(ref explicit) = req.consolidation_branch {
            let trimmed = explicit.trim();
            if trimmed.is_empty() {
                return Err(MemError::InvalidPath(
                    "consolidation branch name cannot be empty".to_string(),
                ));
            }
            if vcs.head_commit(trimmed).is_err() {
                vcs.create_branch(trimmed, &Revision::BranchHead(req.source_branch.clone()))?;
            }
            return Ok(trimmed.to_string());
        }

        let base_name = format!("consolidate/{run_id}");
        if vcs.head_commit(&base_name).is_err() {
            vcs.create_branch(&base_name, &Revision::BranchHead(req.source_branch.clone()))?;
            return Ok(base_name);
        }

        for idx in 1..=64 {
            let candidate = format!("{base_name}-{idx}");
            if vcs.head_commit(&candidate).is_err() {
                vcs.create_branch(&candidate, &Revision::BranchHead(req.source_branch.clone()))?;
                return Ok(candidate);
            }
        }

        Err(MemError::BranchAlreadyExists(base_name))
    }

    fn archive_processed_episodes(
        &self,
        consolidate_branch: &str,
        run_id: &str,
        episodes: &[SourceEpisode],
        cards: &[DistilledCard],
        decay_factor: f32,
        consolidated_at: &str,
    ) -> Result<VirtualPath> {
        let vcs = self.search.vcs();
        let factor = decay_factor.clamp(0.05, 1.0);

        for ep in episodes {
            let decayed_imp = (ep.importance * factor).clamp(0.05, 1.0);
            let mut tags = ep.tags.clone();
            if !tags.iter().any(|t| t == "archived-episode") {
                tags.push("archived-episode".to_string());
            }
            let meta = MemoryMetadata {
                content_type: "text/markdown".to_string(),
                importance: decayed_imp,
                tags,
                created_at_ms: ep.updated_at_ms,
                updated_at_ms: Utc::now().timestamp_millis(),
                ttl_seconds: None,
            };
            let node = MemoryNode::new(ep.path.clone(), ep.raw_text.as_bytes().to_vec())
                .with_metadata(meta);
            let blob_id = vcs.write_node(consolidate_branch, node)?;
            self.search.index_blob_if_needed(&ep.path, &blob_id)?;
        }

        let archive_path = VirtualPath::new(format!("/episodic/archive/consolidated-{run_id}.md"))?;
        let mut digest = format!(
            "---\ntitle: \"Consolidation Archive {run_id}\"\ntags: [\"episodic\", \"archive\", \"consolidated\"]\nimportance: 0.25\nconsolidated_at: \"{consolidated_at}\"\n---\n\n# Consolidation Archive ({run_id})\n\n## Distilled Output Cards\n"
        );
        for card in cards {
            digest.push_str(&format!("- [[{}]] (`{}`)\n", card.title, card.path));
        }
        digest.push_str("\n## Archived Episodic Sources\n");
        for ep in episodes {
            digest.push_str(&format!(
                "- {} (importance -> {:.2})\n",
                ep.path,
                (ep.importance * factor).clamp(0.05, 1.0)
            ));
        }

        let archive_node = MemoryNode::new(archive_path.clone(), digest.as_bytes().to_vec())
            .with_tags(vec![
                "episodic".to_string(),
                "archive".to_string(),
                "consolidated".to_string(),
            ])
            .with_importance(0.25);
        let blob_id = vcs.write_node(consolidate_branch, archive_node)?;
        self.search.index_blob_if_needed(&archive_path, &blob_id)?;

        Ok(archive_path)
    }
}

fn resolve_card_path(
    cand: &DistilledCandidate,
    semantic_dir: &VirtualPath,
    procedural_dir: &VirtualPath,
    used_paths: &mut HashSet<VirtualPath>,
) -> Result<VirtualPath> {
    let dir = if let Some(ref custom) = cand.target_dir_override {
        custom.as_str()
    } else if cand.tier == MemoryTier::Procedural {
        procedural_dir.as_str()
    } else {
        semantic_dir.as_str()
    };

    let base_slug = if cand.slug.is_empty() {
        "consolidated-note"
    } else {
        cand.slug.as_str()
    };

    let candidate = VirtualPath::new(format!("{dir}/{base_slug}.md"))?;
    if used_paths.insert(candidate.clone()) {
        candidate.validate_file_path()?;
        return Ok(candidate);
    }

    for suffix in 2..=64 {
        let alt = VirtualPath::new(format!("{dir}/{base_slug}-{suffix}.md"))?;
        if used_paths.insert(alt.clone()) {
            alt.validate_file_path()?;
            return Ok(alt);
        }
    }

    Err(MemError::InvalidPath(format!(
        "exhausted path disambiguation for '{base_slug}'"
    )))
}

fn build_run_id(source_head: &CommitId, episodes: &[SourceEpisode]) -> String {
    let ts = Utc::now().format("%Y%m%d-%H%M%S");
    let mut hasher = blake3::Hasher::new();
    hasher.update(source_head.as_bytes());
    for ep in episodes {
        hasher.update(ep.path.as_str().as_bytes());
        hasher.update(&ep.updated_at_ms.to_le_bytes());
    }
    let hex = hasher.finalize().to_hex();
    format!("{ts}-{}", &hex[..6])
}
