//! `< 5ms` deterministic `/core` hot-path context compiler.

use std::cmp::Ordering;
use std::collections::HashMap;

use mem_core::{BlobId, MemError, MemoryMetadata, Result, Revision, VirtualPath};
use mem_store::cas::get_blob_conn;
use mem_vcs::MemVcs;
use serde::{Deserialize, Serialize};

/// Deterministic prompt block compiled from `/core` nodes within a token budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledCoreContext {
    /// Rendered XML/Markdown context block ready to inject into a System Prompt.
    pub rendered: String,
    /// Ordered list of `/core/...` paths included within the token budget.
    pub included_paths: Vec<VirtualPath>,
    /// `/core/...` paths omitted because remaining token budget was insufficient.
    pub skipped_paths: Vec<VirtualPath>,
    /// Approximate token count of `rendered`.
    pub estimated_tokens: usize,
}

/// Estimates token count for mixed ASCII and CJK text without external tokenizers.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut ascii_chars = 0usize;
    let mut non_ascii_chars = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_chars += 1;
        } else {
            non_ascii_chars += 1;
        }
    }
    non_ascii_chars + ascii_chars.div_ceil(4)
}

/// Compiles active `/core` nodes for `rev` sorted by `(importance DESC, path ASC)` within `max_tokens`.
pub fn compile_core(
    vcs: &MemVcs,
    rev: &Revision,
    max_tokens: usize,
) -> Result<CompiledCoreContext> {
    let core_prefix = VirtualPath::new("/core")?;
    let mut entries: Vec<(VirtualPath, BlobId, MemoryMetadata)> =
        vcs.list(rev, Some(&core_prefix))?;

    entries.sort_by(|a, b| {
        b.2.importance
            .partial_cmp(&a.2.importance)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    let unique_blobs: Vec<BlobId> = entries.iter().map(|(_, b, _)| *b).collect();
    let payloads: HashMap<BlobId, String> = vcs.store().with_conn(|conn| {
        let mut map = HashMap::with_capacity(unique_blobs.len());
        for blob_id in unique_blobs {
            if map.contains_key(&blob_id) {
                continue;
            }
            let (raw, _) = get_blob_conn(conn, &blob_id)?
                .ok_or_else(|| MemError::BlobNotFound(blob_id.to_hex()))?;
            let text = String::from_utf8_lossy(&raw).into_owned();
            map.insert(blob_id, text);
        }
        Ok(map)
    })?;

    let rev_label = match rev {
        Revision::BranchWorkspace(b) => format!("workspace:{b}"),
        Revision::BranchHead(b) => format!("head:{b}"),
        Revision::Commit(c) => format!("commit:{}", c.to_hex()),
    };

    let header_open = format!("<core_memory revision=\"{rev_label}\">\n");
    let header_close = "</core_memory>";
    let wrapper_tokens = estimate_tokens(&header_open) + estimate_tokens(header_close);

    let mut body = String::new();
    let mut included_paths = Vec::new();
    let mut skipped_paths = Vec::new();
    let mut used_tokens = wrapper_tokens;

    for (path, blob_id, meta) in entries {
        let raw_content = payloads.get(&blob_id).map(String::as_str).unwrap_or("");
        let cleaned = clean_core_payload(raw_content);
        let header = format!(
            "## {} (importance: {:.2})\n",
            path.as_str(),
            meta.importance
        );
        let section = format!("{header}{}\n\n", cleaned.trim_end());
        let section_tokens = estimate_tokens(&section);

        if used_tokens + section_tokens <= max_tokens {
            body.push_str(&section);
            used_tokens += section_tokens;
            included_paths.push(path);
        } else if let Some(partial) =
            fit_partial_sections(&header, &cleaned, max_tokens.saturating_sub(used_tokens))
        {
            let partial_tokens = estimate_tokens(&partial);
            if used_tokens + partial_tokens <= max_tokens {
                body.push_str(&partial);
                used_tokens += partial_tokens;
                included_paths.push(path);
            } else {
                skipped_paths.push(path);
            }
        } else {
            skipped_paths.push(path);
        }
    }

    let mut rendered = String::with_capacity(header_open.len() + body.len() + header_close.len());
    rendered.push_str(&header_open);
    rendered.push_str(&body);
    rendered.push_str(header_close);
    let estimated_tokens = estimate_tokens(&rendered);

    Ok(CompiledCoreContext {
        rendered,
        included_paths,
        skipped_paths,
        estimated_tokens,
    })
}

/// Strips leading YAML frontmatter and excessive blank lines so `/core` injects pure rules/goals.
fn clean_core_payload(raw: &str) -> String {
    let normalized = raw
        .strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.lines().collect();
    let body_start = if lines.first().is_some_and(|l| l.trim() == "---") {
        lines
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(idx, l)| {
                let t = l.trim();
                (t == "---" || t == "...").then_some(idx + 1)
            })
            .unwrap_or(0)
    } else {
        0
    };

    let mut out = String::new();
    let mut blank_run = 0usize;
    for line in &lines[body_start..] {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
    }
    out
}

/// Fits leading `## ` / `### ` subsections when a `/core` file exceeds the remaining budget.
fn fit_partial_sections(header: &str, body: &str, budget_tokens: usize) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        let is_sub = trimmed.starts_with("## ") || trimmed.starts_with("### ");
        if is_sub && !current.trim().is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        blocks.push(current);
    }
    if blocks.len() <= 1 {
        return None;
    }

    let mut assembled = String::new();
    let mut included_blocks = 0usize;
    for block in blocks {
        let candidate_body = if assembled.is_empty() {
            block.clone()
        } else {
            format!("{assembled}\n\n{block}")
        };
        let candidate = format!("{header}{}\n...(truncated)\n\n", candidate_body.trim_end());
        if estimate_tokens(&candidate) <= budget_tokens {
            assembled = candidate_body;
            included_blocks += 1;
        } else {
            break;
        }
    }

    if included_blocks == 0 {
        None
    } else {
        Some(format!(
            "{header}{}\n...(truncated)\n\n",
            assembled.trim_end()
        ))
    }
}
