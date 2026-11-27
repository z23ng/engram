//! Deterministic VCS history, rewind anti-pattern, co-change, topology gap, and utility mining.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::Utc;
use mem_core::{BlobId, CommitId, DiffKind, MemError, MemoryTier, Result, Revision, VirtualPath};
use mem_store::cas::get_blob_conn;
use mem_store::SqliteStore;
use mem_vcs::tree::{parse_metadata, resolve_effective_tree};
use mem_vcs::{MemVcs, VersionControlEngine};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::hybrid::MemSearchEngine;
use crate::sanitize::simhash_hamming;
use crate::telemetry::{load_branch_telemetry, TelemetryRow};

const MAX_COMMITS_FOR_MINING: usize = 100;
const MAX_COCHANGE_FILES_PER_COMMIT: usize = 12;

/// Co-modification edge mined from commit history on a branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoChangeEdge {
    pub path_a: VirtualPath,
    pub path_b: VirtualPath,
    pub co_commits: usize,
    pub weight: f32,
}

/// Anti-pattern pitfall mined from a `mem_rewind` event and its abandoned commit diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindPitfall {
    pub event_id: i64,
    pub branch: String,
    pub abandoned_commit: CommitId,
    pub target_commit: CommitId,
    pub reverted_paths: Vec<VirtualPath>,
    pub summary: String,
    pub rules: Vec<String>,
}

/// Unresolved `[[Wikilink]]` target referenced by active notes inside a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DanglingLink {
    pub target_ref: String,
    pub referenced_by: Vec<VirtualPath>,
}

/// Candidate missing link discovered via SimHash proximity or shared entity triples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuggestedLink {
    pub path_a: VirtualPath,
    pub path_b: VirtualPath,
    pub similarity: f32,
    pub reason: String,
}

/// High-utility memory node recommended for promotion to a higher memory tier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionCandidate {
    pub path: VirtualPath,
    pub from_tier: MemoryTier,
    pub suggested_tier: MemoryTier,
    pub utility_score: f32,
    pub reason: String,
}

/// Structured branch-level mining report covering VCS evolution, topology gaps, and TTL hygiene.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MiningReport {
    pub branch: String,
    pub cochange_edges: Vec<CoChangeEdge>,
    pub rewind_pitfalls: Vec<RewindPitfall>,
    pub dangling_links: Vec<DanglingLink>,
    pub missing_links: Vec<SuggestedLink>,
    pub high_churn_paths: Vec<(VirtualPath, usize)>,
    pub promotion_candidates: Vec<PromotionCandidate>,
    pub expired_paths: Vec<VirtualPath>,
}

/// Co-modification edges and high-churn paths extracted from commit history.
pub type CommitHistoryMining = (Vec<CoChangeEdge>, Vec<(VirtualPath, usize)>);

/// Mines co-modified file pairs and semantic churn counts from commit history on `branch`.
pub fn mine_commit_history(
    vcs: &MemVcs,
    branch: &str,
    max_commits: usize,
) -> Result<CommitHistoryMining> {
    let commits = vcs.log(branch, max_commits.max(1))?;
    if commits.len() < 2 {
        return Ok((Vec::new(), Vec::new()));
    }

    let active_paths: HashSet<VirtualPath> = vcs
        .list(&Revision::BranchWorkspace(branch.to_string()), None)?
        .into_iter()
        .map(|(p, _, _)| p)
        .collect();

    let mut pair_counts: BTreeMap<(VirtualPath, VirtualPath), usize> = BTreeMap::new();
    let mut churn_counts: HashMap<VirtualPath, usize> = HashMap::new();

    for info in &commits {
        let Some(&parent_id) = info.parent_ids.first() else {
            continue;
        };
        let Ok(diffs) = vcs.diff(
            &Revision::Commit(parent_id),
            &Revision::Commit(info.commit_id),
        ) else {
            continue;
        };

        let mut touched: Vec<VirtualPath> = diffs
            .into_iter()
            .filter(|d| d.kind != DiffKind::Deleted && active_paths.contains(&d.path))
            .map(|d| d.path)
            .collect();
        touched.sort();
        touched.dedup();

        for p in &touched {
            if p.tier() == MemoryTier::Semantic {
                *churn_counts.entry(p.clone()).or_insert(0) += 1;
            }
        }

        if (2..=MAX_COCHANGE_FILES_PER_COMMIT).contains(&touched.len()) {
            for i in 0..touched.len() {
                for j in (i + 1)..touched.len() {
                    *pair_counts
                        .entry((touched[i].clone(), touched[j].clone()))
                        .or_insert(0) += 1;
                }
            }
        }
    }

    let mut edges: Vec<CoChangeEdge> = pair_counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .map(|((path_a, path_b), co_commits)| CoChangeEdge {
            path_a,
            path_b,
            co_commits,
            weight: (co_commits as f32 / 5.0).clamp(0.25, 1.0),
        })
        .collect();
    edges.sort_by(|a, b| {
        b.co_commits
            .cmp(&a.co_commits)
            .then_with(|| a.path_a.cmp(&b.path_a))
            .then_with(|| a.path_b.cmp(&b.path_b))
    });

    let mut high_churn: Vec<(VirtualPath, usize)> = churn_counts
        .into_iter()
        .filter(|(_, count)| *count >= 4)
        .collect();
    high_churn.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    Ok((edges, high_churn))
}

/// Mines unmined `rewind_events` on `branch` whose abandoned commits remain in storage.
pub fn mine_rewind_pitfalls(vcs: &MemVcs, branch: &str) -> Result<Vec<RewindPitfall>> {
    let raw_events: Vec<(i64, String, String)> = vcs.store().with_conn(|conn| {
        let mut stmt = conn
            .prepare_cached(
                "SELECT id, abandoned_commit_id, target_commit_id
                 FROM rewind_events
                 WHERE branch = ?1 AND mined = 0
                 ORDER BY id ASC",
            )
            .map_err(|e| {
                MemError::Storage(format!("failed to prepare rewind_events query: {e}"))
            })?;
        let rows = stmt
            .query_map(params![branch], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| MemError::Storage(format!("failed to query rewind_events: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(
                row.map_err(|e| MemError::Storage(format!("failed to decode rewind event: {e}")))?,
            );
        }
        Ok(out)
    })?;

    let mut pitfalls = Vec::new();
    for (event_id, abandoned_hex, target_hex) in raw_events {
        let Ok(abandoned_commit) = CommitId::from_hex(&abandoned_hex) else {
            continue;
        };
        let Ok(target_commit) = CommitId::from_hex(&target_hex) else {
            continue;
        };
        if vcs.store().get_commit(&abandoned_commit)?.is_none()
            || vcs.store().get_commit(&target_commit)?.is_none()
        {
            continue;
        }
        let Ok(diffs) = vcs.diff(
            &Revision::Commit(target_commit),
            &Revision::Commit(abandoned_commit),
        ) else {
            continue;
        };
        if diffs.is_empty() {
            continue;
        }

        let reverted_paths: Vec<VirtualPath> = diffs.iter().map(|d| d.path.clone()).collect();
        let mut rules = Vec::new();
        for diff in diffs.iter().take(4) {
            let kind_label = match diff.kind {
                DiffKind::Added => "added",
                DiffKind::Modified => "modified",
                DiffKind::Deleted => "deleted",
            };
            let abandoned_preview = diff
                .new_blob_id
                .and_then(|bid| load_blob_preview(vcs.store(), &bid))
                .unwrap_or_default();
            let baseline_preview = diff
                .old_blob_id
                .and_then(|bid| load_blob_preview(vcs.store(), &bid))
                .unwrap_or_default();

            let mut detail = format!(
                "Avoid repeating reverted {kind_label} change at `{}` (abandoned commit {}).",
                diff.path,
                &abandoned_hex[..8.min(abandoned_hex.len())]
            );
            if !abandoned_preview.is_empty() {
                detail.push_str(&format!(" Reverted snippet: \"{abandoned_preview}\"."));
            }
            if !baseline_preview.is_empty() {
                detail.push_str(&format!(" Safe baseline: \"{baseline_preview}\"."));
            }
            rules.push(detail);
        }

        let summary = format!(
            "Rewound branch '{}' from {} back to {} across {} path(s).",
            branch,
            &abandoned_hex[..8.min(abandoned_hex.len())],
            &target_hex[..8.min(target_hex.len())],
            reverted_paths.len()
        );
        pitfalls.push(RewindPitfall {
            event_id,
            branch: branch.to_string(),
            abandoned_commit,
            target_commit,
            reverted_paths,
            summary,
            rules,
        });
    }

    Ok(pitfalls)
}

/// Marks the specified `rewind_events` IDs as mined so they are not distilled repeatedly.
pub fn mark_rewind_events_mined(store: &SqliteStore, event_ids: &[i64]) -> Result<()> {
    if event_ids.is_empty() {
        return Ok(());
    }
    store.with_tx(|tx| {
        let mut stmt = tx
            .prepare_cached("UPDATE rewind_events SET mined = 1 WHERE id = ?1")
            .map_err(|e| MemError::Storage(format!("failed to prepare rewind mark update: {e}")))?;
        for &id in event_ids {
            stmt.execute(params![id]).map_err(|e| {
                MemError::Storage(format!("failed to mark rewind event mined: {e}"))
            })?;
        }
        Ok(())
    })
}

/// Runs all deterministic miners for `branch`, returning a comprehensive [`MiningReport`].
pub fn mine_branch(engine: &MemSearchEngine, branch: &str) -> Result<MiningReport> {
    let vcs = engine.vcs();
    let rev = Revision::BranchWorkspace(branch.to_string());

    let (cochange_edges, high_churn_paths) =
        mine_commit_history(vcs, branch, MAX_COMMITS_FOR_MINING)?;
    let rewind_pitfalls = mine_rewind_pitfalls(vcs, branch)?;

    let graph = engine.branch_graph(&rev)?;
    let active_entries = vcs.list(&rev, None)?;
    let now_ms = Utc::now().timestamp_millis();

    let mut path_by_blob: HashMap<BlobId, Vec<VirtualPath>> = HashMap::new();
    for (p, b, _) in &active_entries {
        path_by_blob.entry(*b).or_default().push(p.clone());
    }

    let existing_edge_pairs: HashSet<(String, String)> = graph
        .edges
        .iter()
        .flat_map(|e| {
            [
                (e.source.clone(), e.target.clone()),
                (e.target.clone(), e.source.clone()),
            ]
        })
        .collect();

    let mut node_in_degree: HashMap<String, usize> = HashMap::new();
    let mut resolver_keys: HashSet<String> = HashSet::new();
    for n in &graph.nodes {
        node_in_degree.insert(n.path.clone(), n.in_degree);
        let p_low = n.path.to_lowercase();
        resolver_keys.insert(p_low.clone());
        if let Some(no_ext) = p_low.strip_suffix(".md") {
            resolver_keys.insert(no_ext.to_string());
        }
        if let Some(fname) = n.path.rsplit('/').next() {
            let fn_low = fname.to_lowercase();
            resolver_keys.insert(fn_low.clone());
            if let Some(stem) = fn_low.strip_suffix(".md") {
                resolver_keys.insert(stem.to_string());
            }
        }
        let t_low = n.title.trim().to_lowercase();
        if !t_low.is_empty() {
            resolver_keys.insert(t_low);
        }
    }

    let (
        expired_paths,
        telemetry,
        dangling_links,
        missing_links,
    ) = vcs.store().with_conn(|conn| {
        // 1. Expired paths in raw workspace/committed tree (bypassing TTL read filter).
        let raw_flat = resolve_effective_tree(conn, &rev, None)?;
        let mut expired = Vec::new();
        for (path, (_, meta_json)) in raw_flat {
            let meta = parse_metadata(&meta_json)?;
            if meta.is_expired_at(now_ms) {
                expired.push(path);
            }
        }
        expired.sort();

        // 2. Branch telemetry.
        let telemetry = load_branch_telemetry(conn, branch)?;

        // 3. Collect headers aliases + wikilinks + entities + simhash for active blobs.
        let mut dangling_map: BTreeMap<String, BTreeSet<VirtualPath>> = BTreeMap::new();
        let mut sigs_by_path: Vec<(VirtualPath, u64, usize)> = Vec::new();
        let mut entity_groups: HashMap<String, BTreeSet<VirtualPath>> = HashMap::new();

        let mut alias_stmt = conn
            .prepare_cached("SELECT aliases FROM chunk_index WHERE blob_id = ?1 AND chunk_seq = 0 LIMIT 1")
            .map_err(|e| MemError::Storage(format!("failed to prepare alias query: {e}")))?;
        let mut link_stmt = conn
            .prepare_cached(
                "SELECT link_kind, source_entity, target_ref FROM blob_links WHERE source_blob_id = ?1",
            )
            .map_err(|e| MemError::Storage(format!("failed to prepare mining link query: {e}")))?;
        let mut sig_stmt = conn
            .prepare_cached("SELECT simhash64, token_count FROM blob_signatures WHERE blob_id = ?1")
            .map_err(|e| MemError::Storage(format!("failed to prepare mining sig query: {e}")))?;

        // Pass 1: collect all frontmatter aliases and SimHash signatures across active blobs first,
        // so wikilink resolution sees all aliases regardless of HashMap iteration order.
        for (blob_id, paths) in &path_by_blob {
            let hex = blob_id.to_hex();
            if let Ok(aliases_joined) = alias_stmt.query_row(params![hex], |r| r.get::<_, String>(0)) {
                for al in aliases_joined.split(" | ").map(str::trim).filter(|s| !s.is_empty()) {
                    resolver_keys.insert(al.to_lowercase());
                }
            }
            if let Ok((raw_sig, tok_count)) = sig_stmt.query_row(params![hex], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            }) {
                let sig = u64::from_ne_bytes(raw_sig.to_ne_bytes());
                let tc = usize::try_from(tok_count.max(0)).unwrap_or(0);
                if tc >= 6 {
                    for p in paths {
                        if matches!(p.tier(), MemoryTier::Semantic | MemoryTier::Procedural) {
                            sigs_by_path.push((p.clone(), sig, tc));
                        }
                    }
                }
            }
        }

        // Pass 2: collect dangling wikilinks and shared-entity candidate edges.
        for (blob_id, paths) in &path_by_blob {
            let hex = blob_id.to_hex();
            if let Ok(mut rows) = link_stmt.query(params![hex]) {
                while let Ok(Some(row)) = rows.next() {
                    let kind: String = row.get(0).map_err(|e| {
                        MemError::Storage(format!("failed to decode mining link kind: {e}"))
                    })?;
                    let src_ent: String = row.get(1).map_err(|e| {
                        MemError::Storage(format!("failed to decode mining link source: {e}"))
                    })?;
                    let target_ref: String = row.get(2).map_err(|e| {
                        MemError::Storage(format!("failed to decode mining link target: {e}"))
                    })?;
                    match kind.as_str() {
                        "wikilink" => {
                            let clean = target_ref.trim().to_lowercase();
                            let no_ext = clean.strip_suffix(".md").unwrap_or(&clean);
                            let no_lead = no_ext.trim_start_matches("./").trim_start_matches('/');
                            if !clean.is_empty()
                                && !resolver_keys.contains(&clean)
                                && !resolver_keys.contains(no_ext)
                                && !resolver_keys.contains(no_lead)
                            {
                                let entry = dangling_map.entry(target_ref.trim().to_string()).or_default();
                                for p in paths {
                                    entry.insert(p.clone());
                                }
                            }
                        }
                        "entity_rel" => {
                            for p in paths {
                                if !src_ent.trim().is_empty() {
                                    entity_groups
                                        .entry(src_ent.trim().to_lowercase())
                                        .or_default()
                                        .insert(p.clone());
                                }
                                if !target_ref.trim().is_empty() {
                                    entity_groups
                                        .entry(target_ref.trim().to_lowercase())
                                        .or_default()
                                        .insert(p.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        let mut dangling: Vec<DanglingLink> = dangling_map
            .into_iter()
            .map(|(target_ref, refs)| DanglingLink {
                target_ref,
                referenced_by: refs.into_iter().collect(),
            })
            .collect();
        dangling.sort_by(|a, b| {
            b.referenced_by
                .len()
                .cmp(&a.referenced_by.len())
                .then_with(|| a.target_ref.cmp(&b.target_ref))
        });

        let mut missing: Vec<SuggestedLink> = Vec::new();
        let mut seen_missing: HashSet<(VirtualPath, VirtualPath)> = HashSet::new();
        sigs_by_path.sort_by(|a, b| a.0.cmp(&b.0));
        for i in 0..sigs_by_path.len() {
            for j in (i + 1)..sigs_by_path.len() {
                let (ref pa, sa, _) = sigs_by_path[i];
                let (ref pb, sb, _) = sigs_by_path[j];
                if existing_edge_pairs.contains(&(pa.as_str().to_string(), pb.as_str().to_string()))
                {
                    continue;
                }
                let dist = simhash_hamming(sa, sb);
                if dist <= 10 {
                    let sim = 1.0 - (dist as f32 / 64.0);
                    if seen_missing.insert((pa.clone(), pb.clone())) {
                        missing.push(SuggestedLink {
                            path_a: pa.clone(),
                            path_b: pb.clone(),
                            similarity: sim,
                            reason: format!("SimHash proximity (hamming={dist})"),
                        });
                    }
                }
            }
        }
        for (entity, group) in entity_groups {
            let paths: Vec<VirtualPath> = group.into_iter().collect();
            if paths.len() < 2 || paths.len() > 8 {
                continue;
            }
            for i in 0..paths.len() {
                for j in (i + 1)..paths.len() {
                    let (pa, pb) = (&paths[i], &paths[j]);
                    if existing_edge_pairs
                        .contains(&(pa.as_str().to_string(), pb.as_str().to_string()))
                    {
                        continue;
                    }
                    if seen_missing.insert((pa.clone(), pb.clone())) {
                        missing.push(SuggestedLink {
                            path_a: pa.clone(),
                            path_b: pb.clone(),
                            similarity: 0.85,
                            reason: format!("Shared entity '{entity}'"),
                        });
                    }
                }
            }
        }
        missing.sort_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.path_a.cmp(&b.path_a))
        });
        missing.truncate(20);

        Ok((expired, telemetry, dangling, missing))
    })?;

    // 4. Promotion candidates from telemetry + graph topology.
    let mut promotion_candidates = Vec::new();
    for (path, blob_id, _) in &active_entries {
        let Some(row) = telemetry.get(path) else {
            continue;
        };
        if row.blob_id != blob_id.to_hex() {
            continue;
        }
        let in_deg = node_in_degree.get(path.as_str()).copied().unwrap_or(0);
        if let Some(cand) = evaluate_promotion(path, row, in_deg) {
            promotion_candidates.push(cand);
        }
    }
    promotion_candidates.sort_by(|a, b| {
        b.utility_score
            .partial_cmp(&a.utility_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });

    Ok(MiningReport {
        branch: branch.to_string(),
        cochange_edges,
        rewind_pitfalls,
        dangling_links,
        missing_links,
        high_churn_paths,
        promotion_candidates,
        expired_paths,
    })
}

fn evaluate_promotion(
    path: &VirtualPath,
    row: &TelemetryRow,
    in_degree: usize,
) -> Option<PromotionCandidate> {
    let utility = row.read_hits.max(0) as f32 * 0.5 + row.post_hit_commits.max(0) as f32 * 1.5
        - row.post_hit_rewinds.max(0) as f32 * 2.0;
    match path.tier() {
        MemoryTier::Episodic if row.post_hit_commits >= 2 || row.read_hits >= 3 => {
            let low = path.as_str().to_ascii_lowercase();
            let suggested_tier = if low.contains("sop")
                || low.contains("runbook")
                || low.contains("fix")
                || low.contains("pitfall")
                || low.contains("error")
            {
                MemoryTier::Procedural
            } else {
                MemoryTier::Semantic
            };
            Some(PromotionCandidate {
                path: path.clone(),
                from_tier: MemoryTier::Episodic,
                suggested_tier,
                utility_score: utility,
                reason: format!(
                    "read_hits={}, post_hit_commits={}",
                    row.read_hits, row.post_hit_commits
                ),
            })
        }
        MemoryTier::Semantic | MemoryTier::Procedural
            if row.post_hit_commits >= 5 && in_degree >= 2 =>
        {
            Some(PromotionCandidate {
                path: path.clone(),
                from_tier: path.tier(),
                suggested_tier: MemoryTier::Core,
                utility_score: utility,
                reason: format!(
                    "post_hit_commits={}, in_degree={in_degree}",
                    row.post_hit_commits
                ),
            })
        }
        _ => None,
    }
}

fn load_blob_preview(store: &SqliteStore, blob_id: &BlobId) -> Option<String> {
    let (bytes, _) = store
        .with_conn(|conn| get_blob_conn(conn, blob_id))
        .ok()??;
    let text = String::from_utf8(bytes).ok()?;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && *l != "---" && !l.starts_with("title:"))?;
    let mut out = String::new();
    for ch in line.chars().take(120) {
        if ch == '"' {
            out.push('\'');
        } else {
            out.push(ch);
        }
    }
    (!out.is_empty()).then_some(out)
}
