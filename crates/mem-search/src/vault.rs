//! Local Obsidian Vault scaffold, multi-tier sync, and export helpers for Engram.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use mem_core::{MemError, MemoryMetadata, MemoryNode, MemoryTier, Result, Revision, VirtualPath};
use mem_vcs::{MemVcs, VersionControlEngine};
use serde::{Deserialize, Serialize};

use crate::hybrid::MemSearchEngine;
use crate::wiki::parse_wiki_document;

/// Summary returned after importing or syncing a local Obsidian Vault directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultImportStats {
    /// Number of Markdown files successfully written into the branch workspace.
    pub files_imported: usize,
    /// Number of files skipped (e.g. non-UTF-8, root guide files, or non-tier folders).
    pub files_skipped: usize,
    /// Distinct content blobs newly indexed into `chunk_index` / `fts_chunks` / embeddings.
    pub blobs_newly_indexed: usize,
    /// Distinct content blobs skipped because their `blob_id` was already indexed.
    pub blobs_deduplicated: usize,
    /// Total `[[Wikilink]]` and entity edges recorded for newly indexed blobs.
    pub links_extracted: usize,
}

/// Summary returned after scaffolding a collaborative team Obsidian Vault template.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultTemplateStats {
    /// Absolute or user-supplied root directory of the Obsidian Vault scaffold.
    pub vault_dir: String,
    /// Starter templates and guide files newly created on disk.
    pub files_created: usize,
    /// Existing files left untouched so re-running never overwrites user edits.
    pub files_skipped: usize,
}

/// Summary returned after exporting a revision snapshot to a local Obsidian Vault directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultExportStats {
    /// Target directory written on disk.
    pub vault_dir: String,
    /// Memory nodes exported as files under tier directories.
    pub files_exported: usize,
    /// Nodes skipped because their content could not be resolved.
    pub files_skipped: usize,
}

/// Imports every `.md` file inside `vault_dir` recursively into `branch` under `target_tier`.
///
/// Relative paths inside `vault_dir` become virtual paths such as
/// `/{target_tier}/subdir/note.md`. Frontmatter (`tags`, `importance`), heading chunks,
/// and `[[Wikilinks]]` are extracted and indexed automatically.
///
/// Hidden entries (such as `.obsidian/`, `.git/`) and `_templates/` are skipped automatically.
pub fn import_vault(
    engine: &MemSearchEngine,
    vault_dir: &Path,
    branch: &str,
    target_tier: MemoryTier,
) -> Result<VaultImportStats> {
    let md_files = validate_and_collect_markdown_files(vault_dir)?;
    let mut stats = VaultImportStats::default();
    let now_ms = Utc::now().timestamp_millis();

    for file_path in md_files {
        let Some(segments) = relative_vault_segments(vault_dir, &file_path) else {
            stats.files_skipped += 1;
            continue;
        };

        let vpath_str = format!("/{}/{}", target_tier.as_str(), segments.join("/"));
        let Ok(vpath) = VirtualPath::new(&vpath_str) else {
            stats.files_skipped += 1;
            continue;
        };
        if vpath.validate_file_path().is_err() {
            stats.files_skipped += 1;
            continue;
        }

        if write_markdown_file_to_vfs(engine, branch, &file_path, vpath, now_ms)? {
            stats.files_imported += 1;
        } else {
            stats.files_skipped += 1;
        }
    }

    let idx_stats = engine.ensure_revision_indexed(
        &Revision::BranchWorkspace(branch.to_string()),
        Some(&VirtualPath::new(format!("/{}", target_tier.as_str()))?),
    )?;
    stats.blobs_newly_indexed = idx_stats.blobs_newly_indexed;
    stats.blobs_deduplicated = idx_stats.blobs_cache_hit;
    stats.links_extracted = idx_stats.links_indexed;

    Ok(stats)
}

/// Scaffolds a collaborative Obsidian Vault directory layout for PM + Developer + Bot teams.
///
/// Creates `/core`, `/semantic/product`, `/semantic/architecture`, `/procedural/runbooks`,
/// `/episodic/bot-insights`, and `_templates/` along with role-specific Markdown templates.
/// Existing files on disk are preserved untouched (`files_skipped`).
pub fn init_vault_template(vault_dir: &Path) -> Result<VaultTemplateStats> {
    let dirs = [
        vault_dir.to_path_buf(),
        vault_dir.join("core"),
        vault_dir.join("semantic").join("product"),
        vault_dir.join("semantic").join("architecture"),
        vault_dir.join("procedural").join("runbooks"),
        vault_dir.join("episodic").join("bot-insights"),
        vault_dir.join("_templates"),
    ];
    for dir in &dirs {
        fs::create_dir_all(dir).map_err(|e| {
            MemError::Storage(format!(
                "failed to create vault directory '{}': {e}",
                dir.display()
            ))
        })?;
    }

    let starter_files: [(&str, &str); 6] = [
        (
            "README.md",
            "# Engram Collaborative Memory Vault\n\n\
This folder mirrors Engram's four memory tiers so PMs, Developers, and Bots share one versioned knowledge graph.\n\n\
## Directory Ownership\n\n\
- `core/` — Always-injected team identity, hard system invariants, and active priorities (`< 2k tokens`).\n\
- `semantic/product/` — **PM workspace**: PRDs, user stories, business rules, and acceptance criteria.\n\
- `semantic/architecture/` — **Developer workspace**: ADRs, domain models, and API/DB constraints.\n\
- `procedural/runbooks/` — **Developer/Ops workspace**: deploy checklists, incident playbooks, and verification scripts.\n\
- `episodic/bot-insights/` — **Bot workspace**: session takeaways and edge cases captured during AI runs.\n\
- `_templates/` — Obsidian starter templates (`sync` skips this folder automatically).\n\n\
## Daily Workflow\n\n\
1. Edit or author notes in Obsidian (use `[[Note Title]]` links across product and architecture docs).\n\
2. Sync all four tiers into Engram:\n\n\
```bash\n\
mem vault sync . --branch main --commit\n\
```\n\n\
3. Or capture quick notes from CLI into an isolated review inbox:\n\n\
```bash\n\
mem ingest --role pm --draft \"Checkout retries must cap at 3 attempts.\"\n\
mem diff main..draft/pm-inbox\n\
mem merge draft/pm-inbox --into main\n\
```\n",
        ),
        (
            "core/system.md",
            "---\ntitle: \"Project Core Memory\"\nrole: \"dev\"\ntier: \"core\"\ntags: [\"core\", \"system\"]\nimportance: 0.95\n---\n\n\
# Project Core Memory\n\n\
- Keep `/core` concise so every AI session loads essential business and architecture truths cleanly.\n\
- Link deep specs in `[[Project Architecture]]` or product rules under `semantic/product/`.\n",
        ),
        (
            "_templates/pm-prd.md",
            "---\ntitle: \"{{title}}\"\nrole: \"pm\"\ntier: \"semantic\"\naliases: []\ntags: [\"role/pm\", \"prd\"]\nimportance: 0.80\n---\n\n\
# {{title}}\n\n\
## Summary & User Value\n\n\
- Problem statement and target user cohort.\n\n\
## Business Rules & Acceptance Criteria\n\n\
- [ ] Rule 1:\n\
- [ ] Edge case handling:\n\n\
## Related Architecture & Links\n\n\
- Link related technical specs with `[[Architecture Note]]`.\n",
        ),
        (
            "_templates/dev-adr.md",
            "---\ntitle: \"{{title}}\"\nrole: \"dev\"\ntier: \"semantic\"\naliases: []\ntags: [\"role/dev\", \"adr\"]\nimportance: 0.85\n---\n\n\
# {{title}}\n\n\
## Context\n\n\
- Why this decision is needed and constraints observed.\n\n\
## Decision & Invariants\n\n\
- Chosen design and non-negotiable invariants.\n\n\
## Consequences & Related\n\n\
- Link driving product rules with `[[PRD Title]]` or operational steps with `[[Runbook]]`.\n",
        ),
        (
            "_templates/dev-runbook.md",
            "---\ntitle: \"{{title}}\"\nrole: \"dev\"\ntier: \"procedural\"\ntags: [\"role/dev\", \"runbook\"]\nimportance: 0.80\n---\n\n\
# {{title}}\n\n\
## Trigger & Preconditions\n\n\
- When to run this procedure.\n\n\
## Step-by-Step Verification\n\n\
1. Check current state.\n\
2. Execute mitigation.\n\
3. Verify recovery metrics.\n\n\
## Related\n\n\
- [[Architecture Overview]]\n",
        ),
        (
            "_templates/bot-insight.md",
            "---\ntitle: \"{{title}}\"\nrole: \"bot\"\ntier: \"episodic\"\ntags: [\"role/bot\", \"observation\"]\nimportance: 0.55\n---\n\n\
# {{title}}\n\n\
## Observation\n\n\
- What happened during task execution.\n\n\
## Recommended Follow-up\n\n\
- Candidate rule or FAQ to promote after PM/Dev review.\n\n\
## Related\n\n\
- [[Related Spec]]\n",
        ),
    ];

    let mut stats = VaultTemplateStats {
        vault_dir: vault_dir.display().to_string(),
        files_created: 0,
        files_skipped: 0,
    };

    for (rel_path, content) in starter_files {
        let dest = vault_dir.join(rel_path);
        if dest.exists() {
            stats.files_skipped += 1;
            continue;
        }
        fs::write(&dest, content.as_bytes()).map_err(|e| {
            MemError::Storage(format!(
                "failed to write vault template file '{}': {e}",
                dest.display()
            ))
        })?;
        stats.files_created += 1;
    }

    Ok(stats)
}

/// Syncs a multi-tier Obsidian Vault directory (`core/`, `semantic/`, `procedural/`, `episodic/`)
/// directly into `branch`, indexing all active blobs once.
///
/// Files inside hidden folders (`.obsidian/`, `.git/`), `_templates/`, root `README.md`,
/// or non-tier folders are skipped gracefully without error.
pub fn sync_vault(
    engine: &MemSearchEngine,
    vault_dir: &Path,
    branch: &str,
) -> Result<VaultImportStats> {
    let md_files = validate_and_collect_markdown_files(vault_dir)?;
    let mut stats = VaultImportStats::default();
    let now_ms = Utc::now().timestamp_millis();

    for file_path in md_files {
        let Some(segments) = relative_vault_segments(vault_dir, &file_path) else {
            stats.files_skipped += 1;
            continue;
        };
        if segments.len() < 2 {
            // Root-level files like README.md are vault documentation, not memory nodes.
            stats.files_skipped += 1;
            continue;
        }
        if MemoryTier::from_segment(&segments[0]).is_none() {
            stats.files_skipped += 1;
            continue;
        }

        let vpath_str = format!("/{}", segments.join("/"));
        let Ok(vpath) = VirtualPath::new(&vpath_str) else {
            stats.files_skipped += 1;
            continue;
        };
        if vpath.validate_file_path().is_err() {
            stats.files_skipped += 1;
            continue;
        }

        if write_markdown_file_to_vfs(engine, branch, &file_path, vpath, now_ms)? {
            stats.files_imported += 1;
        } else {
            stats.files_skipped += 1;
        }
    }

    let idx_stats =
        engine.ensure_revision_indexed(&Revision::BranchWorkspace(branch.to_string()), None)?;
    stats.blobs_newly_indexed = idx_stats.blobs_newly_indexed;
    stats.blobs_deduplicated = idx_stats.blobs_cache_hit;
    stats.links_extracted = idx_stats.links_indexed;

    Ok(stats)
}

/// Exports every active node in `rev` into a local tier-structured Obsidian Vault directory.
///
/// The resulting directory mirrors `/core`, `/semantic`, `/procedural`, `/episodic` cleanly
/// and can be edited in Obsidian and re-synced via [`sync_vault`].
pub fn export_vault(vcs: &MemVcs, rev: &Revision, vault_dir: &Path) -> Result<VaultExportStats> {
    fs::create_dir_all(vault_dir).map_err(|e| {
        MemError::Storage(format!(
            "failed to create export directory '{}': {e}",
            vault_dir.display()
        ))
    })?;

    let entries = vcs.list(rev, None)?;
    let mut stats = VaultExportStats {
        vault_dir: vault_dir.display().to_string(),
        files_exported: 0,
        files_skipped: 0,
    };

    for (vpath, _blob_id, _meta) in entries {
        let Some(node) = vcs.read(rev, &vpath)? else {
            stats.files_skipped += 1;
            continue;
        };

        let mut dest = vault_dir.to_path_buf();
        for seg in vpath.segments() {
            dest.push(seg);
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                MemError::Storage(format!(
                    "failed to create export subdirectory '{}': {e}",
                    parent.display()
                ))
            })?;
        }
        fs::write(&dest, &node.content).map_err(|e| {
            MemError::Storage(format!(
                "failed to write exported memory file '{}': {e}",
                dest.display()
            ))
        })?;
        stats.files_exported += 1;
    }

    Ok(stats)
}

fn validate_and_collect_markdown_files(vault_dir: &Path) -> Result<Vec<PathBuf>> {
    if !vault_dir.exists() || !vault_dir.is_dir() {
        return Err(MemError::Storage(format!(
            "vault directory does not exist or is not a directory: {}",
            vault_dir.display()
        )));
    }

    let mut md_files = Vec::new();
    collect_markdown_files(vault_dir, &mut md_files)?;
    md_files.sort();
    Ok(md_files)
}

fn is_skipped_vault_entry(name: &str) -> bool {
    name.starts_with('.') || name == "_templates"
}

fn relative_vault_segments(vault_dir: &Path, file_path: &Path) -> Option<Vec<String>> {
    let rel = file_path.strip_prefix(vault_dir).ok()?;
    let mut segments = Vec::new();
    for comp in rel.components() {
        let s = comp.as_os_str().to_string_lossy();
        if s.is_empty() || is_skipped_vault_entry(&s) {
            return None;
        }
        segments.push(s.replace('\\', "/"));
    }
    if segments.is_empty() {
        None
    } else {
        Some(segments)
    }
}

fn write_markdown_file_to_vfs(
    engine: &MemSearchEngine,
    branch: &str,
    file_path: &Path,
    vpath: VirtualPath,
    now_ms: i64,
) -> Result<bool> {
    let bytes = fs::read(file_path).map_err(|e| {
        MemError::Storage(format!(
            "failed to read markdown file '{}': {e}",
            file_path.display()
        ))
    })?;
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(false);
    };

    let parsed = parse_wiki_document(&vpath, text, engine.config().chunk_max_chars);
    let metadata = MemoryMetadata {
        content_type: "text/markdown".to_string(),
        importance: parsed.importance.unwrap_or(0.6),
        tags: parsed.tags,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
        ttl_seconds: None,
    };

    let node = MemoryNode::new(vpath, bytes).with_metadata(metadata);
    engine.vcs().write(branch, node)?;
    Ok(true)
}

fn collect_markdown_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(|e| {
        MemError::Storage(format!("failed to read directory '{}': {e}", dir.display()))
    })?;

    for entry_res in entries {
        let entry =
            entry_res.map_err(|e| MemError::Storage(format!("failed to read dir entry: {e}")))?;
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        if is_skipped_vault_entry(&name_str) {
            continue;
        }

        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| MemError::Storage(format!("failed to inspect file type: {e}")))?;
        if file_type.is_dir() {
            collect_markdown_files(&path, out)?;
        } else if file_type.is_file() {
            let is_md = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
            if is_md {
                out.push(path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use mem_store::SqliteStore;
    use mem_vcs::MemVcs;
    use tempfile::tempdir;

    use super::*;

    fn test_engine() -> MemSearchEngine {
        let store = SqliteStore::open_in_memory().expect("open memory store");
        let vcs = MemVcs::new(store);
        vcs.init().expect("init main branch");
        MemSearchEngine::new_bm25_only(vcs)
    }

    #[test]
    fn template_sync_and_export_round_trip() {
        let tmp = tempdir().unwrap();
        let vault = tmp.path().join("team-vault");

        let init1 = init_vault_template(&vault).unwrap();
        assert_eq!(init1.files_created, 6);
        assert_eq!(init1.files_skipped, 0);

        // Idempotent second run leaves existing files intact
        let init2 = init_vault_template(&vault).unwrap();
        assert_eq!(init2.files_created, 0);
        assert_eq!(init2.files_skipped, 6);

        // Add PM & Dev notes alongside ignored folders
        fs::write(
            vault.join("semantic/product/checkout-prd.md"),
            "---\ntitle: \"Checkout PRD\"\ntags: [\"role/pm\"]\n---\n\n# Checkout PRD\n\nRequires [[Retry Architecture]].\n",
        )
        .unwrap();
        fs::write(
            vault.join("semantic/architecture/retry-arch.md"),
            "---\ntitle: \"Retry Architecture\"\ntags: [\"role/dev\"]\n---\n\n# Retry Architecture\n\nUses idempotency keys.\n",
        )
        .unwrap();
        fs::create_dir_all(vault.join(".obsidian")).unwrap();
        fs::write(vault.join(".obsidian/config.md"), "# Ignored").unwrap();
        fs::write(
            vault.join("_templates/custom-ignored.md"),
            "# Ignored template",
        )
        .unwrap();

        let engine = test_engine();
        let sync_stats = sync_vault(&engine, &vault, "main").unwrap();
        // core/system.md + checkout-prd.md + retry-arch.md = 3; README.md at root is skipped (1)
        assert_eq!(sync_stats.files_imported, 3);
        assert_eq!(sync_stats.files_skipped, 1);

        let export_dir = tmp.path().join("exported-vault");
        let export_stats = export_vault(
            engine.vcs(),
            &Revision::BranchWorkspace("main".to_string()),
            &export_dir,
        )
        .unwrap();
        assert_eq!(export_stats.files_exported, 3);
        assert!(export_dir.join("core/system.md").exists());
        assert!(export_dir.join("semantic/product/checkout-prd.md").exists());
        assert!(export_dir
            .join("semantic/architecture/retry-arch.md")
            .exists());
    }
}
