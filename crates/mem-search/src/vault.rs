//! Local Obsidian Vault Markdown importer that populates `mem-vcs` and graph/FTS5 indices.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use mem_core::{MemError, MemoryMetadata, MemoryNode, MemoryTier, Result, Revision, VirtualPath};
use mem_vcs::VersionControlEngine;
use serde::{Deserialize, Serialize};

use crate::hybrid::MemSearchEngine;
use crate::wiki::parse_wiki_document;

/// Summary statistics returned by [`import_vault`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultImportStats {
    /// Number of Markdown files successfully written into the branch workspace.
    pub files_imported: usize,
    /// Number of skipped files (non-Markdown, invalid UTF-8, or invalid VFS paths).
    pub files_skipped: usize,
    /// Distinct blobs newly parsed and indexed.
    pub blobs_newly_indexed: usize,
    /// Distinct blobs deduplicated because their BLAKE3 content was already indexed.
    pub blobs_deduplicated: usize,
    /// Total `[[Wikilinks]]`, `#tags`, and entity edges extracted.
    pub links_extracted: usize,
}

/// Recursively imports `.md` files from a local Obsidian vault directory into `branch` under `target_tier`.
///
/// Hidden directories and files (such as `.obsidian/` and `.git/`) are skipped automatically.
pub fn import_vault(
    engine: &MemSearchEngine,
    vault_dir: &Path,
    branch: &str,
    target_tier: MemoryTier,
) -> Result<VaultImportStats> {
    if !vault_dir.exists() || !vault_dir.is_dir() {
        return Err(MemError::Storage(format!(
            "vault directory does not exist or is not a directory: {}",
            vault_dir.display()
        )));
    }

    let mut md_files = Vec::new();
    collect_markdown_files(vault_dir, &mut md_files)?;
    md_files.sort();

    let mut stats = VaultImportStats::default();
    let now_ms = Utc::now().timestamp_millis();

    for file_path in md_files {
        let Ok(rel) = file_path.strip_prefix(vault_dir) else {
            stats.files_skipped += 1;
            continue;
        };

        let mut segments: Vec<String> = Vec::new();
        let mut valid_rel = true;
        for comp in rel.components() {
            let s = comp.as_os_str().to_string_lossy();
            if s.is_empty() || s.starts_with('.') {
                valid_rel = false;
                break;
            }
            segments.push(s.replace('\\', "/"));
        }
        if !valid_rel || segments.is_empty() {
            stats.files_skipped += 1;
            continue;
        }

        let vpath_str = format!("/{}/{}", target_tier.as_str(), segments.join("/"));
        let Ok(vpath) = VirtualPath::new(&vpath_str) else {
            stats.files_skipped += 1;
            continue;
        };
        if vpath.validate_file_path().is_err() {
            stats.files_skipped += 1;
            continue;
        }

        let bytes = fs::read(&file_path).map_err(|e| {
            MemError::Storage(format!(
                "failed to read markdown file '{}': {e}",
                file_path.display()
            ))
        })?;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            stats.files_skipped += 1;
            continue;
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
        stats.files_imported += 1;
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

fn collect_markdown_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(|e| {
        MemError::Storage(format!(
            "failed to read directory '{}': {e}",
            dir.display()
        ))
    })?;

    for entry_res in entries {
        let entry = entry_res
            .map_err(|e| MemError::Storage(format!("failed to read dir entry: {e}")))?;
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        if name_str.starts_with('.') {
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
