//! Obsidian Wiki Markdown parser, frontmatter extractor, and heading-breadcrumb chunker.

use std::collections::HashSet;

use mem_core::VirtualPath;
use serde::{Deserialize, Serialize};

/// Mem0-style entity relationship triple extracted from frontmatter or Wiki metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityTriple {
    /// Subject entity name (e.g. `"SQLite"`).
    pub source: String,
    /// Relationship predicate (e.g. `"uses"`, `"depends_on"`, `"mentions"`).
    pub relation: String,
    /// Object entity name (e.g. `"WAL Mode"`).
    pub target: String,
}

/// Obsidian-style `[[Wikilink]]` reference extracted from Markdown content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiLink {
    /// Normalized note target with any `#section`, `^block`, or `|alias` stripped.
    pub target: String,
    /// Raw link target before `#section` stripping.
    pub raw_target: String,
    /// Optional display alias after `|`.
    pub alias: Option<String>,
}

/// Single heading-breadcrumb-aware chunk extracted from a Markdown document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiChunk {
    /// Zero-based sequential index of this chunk within the blob.
    pub chunk_seq: usize,
    /// Active Markdown heading hierarchy (e.g. `"# Storage > ## WAL"` or `"(root)"`).
    pub heading_path: String,
    /// Raw Markdown snippet belonging to this chunk without synthetic headers.
    pub raw_snippet: String,
    /// Breadcrumb-enriched text indexed by FTS5 and embedded by vector providers.
    pub chunk_text: String,
    /// 1-based starting line number in the original document.
    pub start_line: usize,
    /// 1-based inclusive ending line number in the original document.
    pub end_line: usize,
}

/// Structured representation of an Obsidian-style Markdown memory document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedWikiDocument {
    /// Document title (from frontmatter, first `# H1` heading, or filename stem).
    pub title: String,
    /// Alternative names/abbreviations declared in frontmatter `aliases`.
    pub aliases: Vec<String>,
    /// Merged tags from frontmatter `tags` and inline `#tags` (without leading `#`).
    pub tags: Vec<String>,
    /// Entity triples extracted from frontmatter `entities`.
    pub entities: Vec<EntityTriple>,
    /// Optional importance score declared in frontmatter.
    pub importance: Option<f32>,
    /// Extracted `[[Wikilinks]]` present in the document body.
    pub wikilinks: Vec<WikiLink>,
    /// Heading-breadcrumb semantic chunks ready for FTS5 and vector indexing.
    pub chunks: Vec<WikiChunk>,
}

#[derive(Default)]
struct FrontmatterData {
    title: Option<String>,
    aliases: Vec<String>,
    tags: Vec<String>,
    raw_entities: Vec<String>,
    importance: Option<f32>,
}

/// Parses an Obsidian-flavored Markdown document into frontmatter metadata, links, and chunks.
pub fn parse_wiki_document(
    path: &VirtualPath,
    markdown: &str,
    max_chunk_chars: usize,
) -> ParsedWikiDocument {
    let normalized = markdown
        .strip_prefix('\u{feff}')
        .unwrap_or(markdown)
        .replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.lines().collect();

    let (fm, body_start_idx) = parse_frontmatter(&lines);

    let default_title = path
        .file_name()
        .map(|name| name.strip_suffix(".md").unwrap_or(name))
        .filter(|s| !s.is_empty())
        .unwrap_or("untitled")
        .to_string();

    let mut first_h1: Option<String> = None;
    let mut inline_tags: Vec<String> = Vec::new();
    let mut wikilinks: Vec<WikiLink> = Vec::new();
    let mut seen_links: HashSet<(String, Option<String>)> = HashSet::new();

    let mut fence_marker: Option<char> = None;
    for line in lines.iter().skip(body_start_idx) {
        let trimmed = line.trim_start();
        if let Some(marker) = fence_marker {
            if is_closing_fence(trimmed, marker) {
                fence_marker = None;
            }
            continue;
        }
        if let Some(marker) = opening_fence(trimmed) {
            fence_marker = Some(marker);
            continue;
        }

        if let Some((level, heading_text)) = parse_atx_heading(trimmed) {
            if level == 1 && first_h1.is_none() && !heading_text.is_empty() {
                first_h1 = Some(heading_text.to_string());
            }
        }

        let stripped = strip_inline_code(line);
        for link in extract_wikilinks_from_line(&stripped) {
            if seen_links.insert((link.target.clone(), link.alias.clone())) {
                wikilinks.push(link);
            }
        }
        for link in extract_markdown_links_from_line(&stripped) {
            if seen_links.insert((link.target.clone(), link.alias.clone())) {
                wikilinks.push(link);
            }
        }
        for tag in extract_inline_tags_from_line(&stripped) {
            inline_tags.push(tag);
        }
    }

    let title = fm
        .title
        .filter(|t| !t.trim().is_empty())
        .or(first_h1)
        .unwrap_or(default_title);

    let aliases = dedup_preserve_order(fm.aliases);
    let mut all_tags = fm.tags;
    all_tags.extend(inline_tags);
    let tags = dedup_preserve_order(all_tags);

    let entities = parse_entities(&title, &fm.raw_entities);

    let chunks = build_breadcrumb_chunks(
        &lines,
        body_start_idx,
        &title,
        &aliases,
        &tags,
        max_chunk_chars.max(120),
    );

    ParsedWikiDocument {
        title,
        aliases,
        tags,
        entities,
        importance: fm.importance,
        wikilinks,
        chunks,
    }
}

fn parse_frontmatter(lines: &[&str]) -> (FrontmatterData, usize) {
    let mut data = FrontmatterData::default();
    if lines.is_empty() || lines[0].trim() != "---" {
        return (data, 0);
    }

    let Some(end_idx) = lines.iter().enumerate().skip(1).find_map(|(idx, line)| {
        let t = line.trim();
        (t == "---" || t == "...").then_some(idx)
    }) else {
        return (data, 0);
    };

    let mut active_list_key: Option<String> = None;
    for line in &lines[1..end_idx] {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(item) = trimmed.strip_prefix("- ") {
            if let Some(key) = active_list_key.as_deref() {
                push_frontmatter_list_item(&mut data, key, item);
            }
            continue;
        }

        if !line.starts_with(' ') && !line.starts_with('\t') {
            active_list_key = None;
        }

        let Some((raw_key, raw_val)) = trimmed.split_once(':') else {
            continue;
        };
        let key = raw_key.trim().to_ascii_lowercase();
        let val = raw_val.trim();

        match key.as_str() {
            "title" => {
                if !val.is_empty() {
                    data.title = Some(unquote_yaml(val));
                }
            }
            "importance" => {
                if let Ok(score) = unquote_yaml(val).parse::<f32>() {
                    if score.is_finite() {
                        data.importance = Some(score.clamp(0.0, 1.0));
                    }
                }
            }
            "aliases" | "alias" => {
                if val.is_empty() {
                    active_list_key = Some("aliases".to_string());
                } else {
                    for item in parse_yaml_list_value(val) {
                        data.aliases.push(item);
                    }
                }
            }
            "tags" | "tag" => {
                if val.is_empty() {
                    active_list_key = Some("tags".to_string());
                } else {
                    for item in parse_yaml_list_value(val) {
                        let cleaned = item.trim_start_matches('#').trim().to_string();
                        if !cleaned.is_empty() {
                            data.tags.push(cleaned);
                        }
                    }
                }
            }
            "entities" | "entity" => {
                if val.is_empty() {
                    active_list_key = Some("entities".to_string());
                } else {
                    for item in parse_yaml_list_value(val) {
                        data.raw_entities.push(item);
                    }
                }
            }
            _ => {}
        }
    }

    (data, end_idx + 1)
}

fn push_frontmatter_list_item(data: &mut FrontmatterData, key: &str, raw_item: &str) {
    let cleaned = unquote_yaml(raw_item.trim());
    if cleaned.is_empty() {
        return;
    }
    match key {
        "aliases" => data.aliases.push(cleaned),
        "tags" => {
            let tag = cleaned.trim_start_matches('#').trim().to_string();
            if !tag.is_empty() {
                data.tags.push(tag);
            }
        }
        "entities" => data.raw_entities.push(cleaned),
        _ => {}
    }
}

fn parse_yaml_list_value(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let inner = if trimmed.starts_with('[') && trimmed.ends_with(']') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };

    split_respecting_quotes(inner)
        .into_iter()
        .map(|s| unquote_yaml(s.trim()))
        .filter(|s| !s.is_empty())
        .collect()
}

fn split_respecting_quotes(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut quote: Option<char> = None;

    for (idx, ch) in input.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == ',' => {
                parts.push(&input[start..idx]);
                start = idx + 1;
            }
            None => {}
        }
    }
    parts.push(&input[start..]);
    parts
}

fn unquote_yaml(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        trimmed[1..trimmed.len() - 1].trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_entities(title: &str, raw_entities: &[String]) -> Vec<EntityTriple> {
    let mut out = Vec::new();
    for raw in raw_entities {
        let parts: Vec<&str> = raw.split("->").map(str::trim).collect();
        match parts.as_slice() {
            [source, relation, target]
                if !source.is_empty() && !relation.is_empty() && !target.is_empty() =>
            {
                out.push(EntityTriple {
                    source: (*source).to_string(),
                    relation: (*relation).to_string(),
                    target: (*target).to_string(),
                });
            }
            [source, target] if !source.is_empty() && !target.is_empty() => {
                out.push(EntityTriple {
                    source: (*source).to_string(),
                    relation: "related_to".to_string(),
                    target: (*target).to_string(),
                });
            }
            _ if !raw.trim().is_empty() => {
                out.push(EntityTriple {
                    source: title.to_string(),
                    relation: "mentions".to_string(),
                    target: raw.trim().to_string(),
                });
            }
            _ => {}
        }
    }
    out
}

fn opening_fence(trimmed_line: &str) -> Option<char> {
    if trimmed_line.starts_with("```") {
        Some('`')
    } else if trimmed_line.starts_with("~~~") {
        Some('~')
    } else {
        None
    }
}

fn is_closing_fence(trimmed_line: &str, marker: char) -> bool {
    let count = trimmed_line.chars().take_while(|&c| c == marker).count();
    count >= 3 && trimmed_line[count..].trim().is_empty()
}

fn parse_atx_heading(trimmed_line: &str) -> Option<(usize, &str)> {
    let level = trimmed_line.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed_line[level..];
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    Some((level, title))
}

fn strip_inline_code(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_code = false;
    for ch in line.chars() {
        if ch == '`' {
            in_code = !in_code;
            out.push(' ');
        } else if in_code {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

fn extract_wikilinks_from_line(line: &str) -> Vec<WikiLink> {
    let mut links = Vec::new();
    let mut rest = line;

    while let Some(open_idx) = rest.find("[[") {
        let after_open = &rest[open_idx + 2..];
        let Some(close_idx) = after_open.find("]]") else {
            break;
        };
        let inside = &after_open[..close_idx];
        let (target_part, alias_part) = match inside.split_once('|') {
            Some((t, a)) => (
                t.trim(),
                Some(a.trim().to_string()).filter(|s| !s.is_empty()),
            ),
            None => (inside.trim(), None),
        };
        let clean_target = target_part
            .split(['#', '^'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string();

        if !clean_target.is_empty() {
            links.push(WikiLink {
                target: clean_target,
                raw_target: target_part.to_string(),
                alias: alias_part,
            });
        }

        rest = &after_open[close_idx + 2..];
    }

    links
}

/// Extracts standard Markdown links `[label](target.md)` or `[label](/tier/path)` from a single line
/// whose inline code spans have already been stripped. Skips images (`![...]`) and external URLs.
pub fn extract_markdown_links_from_line(line: &str) -> Vec<WikiLink> {
    let mut links = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        // Skip [[wikilinks]] and ![images].
        if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            if let Some(rel_end) = line[i + 2..].find("]]") {
                i += 2 + rel_end + 2;
                continue;
            }
            break;
        }
        let is_image = i > 0 && bytes[i - 1] == b'!';

        // Find matching closing `]` on the same line without crossing another unescaped `[`.
        let after_bracket = &line[i + 1..];
        let Some(close_rel) = after_bracket.find(']') else {
            break;
        };
        let label = after_bracket[..close_rel].trim();
        let after_close_idx = i + 1 + close_rel + 1;

        if is_image || !line[after_close_idx..].starts_with('(') {
            i = after_close_idx;
            continue;
        }

        let after_paren = &line[after_close_idx + 1..];
        let Some(end_paren_rel) = after_paren.find(')') else {
            break;
        };
        let raw_href = after_paren[..end_paren_rel].trim();
        i = after_close_idx + 1 + end_paren_rel + 1;

        if raw_href.is_empty() {
            continue;
        }
        let target_token = if raw_href.starts_with('<') {
            raw_href
                .strip_prefix('<')
                .and_then(|s| s.split('>').next())
                .unwrap_or("")
                .trim()
        } else {
            raw_href.split_whitespace().next().unwrap_or("").trim()
        };

        if target_token.is_empty()
            || target_token.starts_with('#')
            || target_token.contains("://")
            || target_token.starts_with("mailto:")
            || target_token.starts_with("tel:")
            || target_token.starts_with("data:")
            || target_token.starts_with("javascript:")
        {
            continue;
        }

        let clean_target = target_token.split(['#', '^']).next().unwrap_or("").trim();
        let clean_lower = clean_target.to_ascii_lowercase();
        let is_note_target = clean_lower.ends_with(".md")
            || clean_lower.starts_with("/core/")
            || clean_lower.starts_with("/semantic/")
            || clean_lower.starts_with("/procedural/")
            || clean_lower.starts_with("/episodic/");

        if !is_note_target || clean_target.is_empty() {
            continue;
        }

        links.push(WikiLink {
            target: clean_target.to_string(),
            raw_target: target_token.to_string(),
            alias: (!label.is_empty()).then(|| label.to_string()),
        });
    }

    links
}

/// Extracts standard Markdown note links across a full Markdown snippet/document,
/// automatically ignoring fenced code blocks and inline backtick code spans.
pub fn extract_markdown_links_from_markdown(markdown: &str) -> Vec<WikiLink> {
    let mut links = Vec::new();
    let mut seen: HashSet<(String, Option<String>)> = HashSet::new();
    let mut fence_marker: Option<char> = None;

    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if let Some(marker) = fence_marker {
            if is_closing_fence(trimmed, marker) {
                fence_marker = None;
            }
            continue;
        }
        if let Some(marker) = opening_fence(trimmed) {
            fence_marker = Some(marker);
            continue;
        }
        let stripped = strip_inline_code(line);
        for link in extract_markdown_links_from_line(&stripped) {
            if seen.insert((link.target.clone(), link.alias.clone())) {
                links.push(link);
            }
        }
    }

    links
}

fn extract_inline_tags_from_line(line: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let chars: Vec<(usize, char)> = line.char_indices().collect();

    for (pos, &(byte_idx, ch)) in chars.iter().enumerate() {
        if ch != '#' {
            continue;
        }
        let prev_ok = if pos == 0 {
            true
        } else {
            let prev = chars[pos - 1].1;
            prev.is_whitespace() || matches!(prev, '(' | '[' | ',' | ';' | ':')
        };
        if !prev_ok {
            continue;
        }

        let after = &line[byte_idx + ch.len_utf8()..];
        let mut tag_len = 0usize;
        for c in after.chars() {
            if is_tag_char(c) {
                tag_len += c.len_utf8();
            } else {
                break;
            }
        }
        if tag_len == 0 {
            continue;
        }
        let candidate = &after[..tag_len];
        // Skip pure numbers (e.g. `#123`) and hex colors if not containing any letter/slash.
        if candidate.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if !candidate.chars().any(|c| c.is_alphabetic()) {
            continue;
        }
        let trimmed = candidate.trim_matches('/');
        if !trimmed.is_empty() {
            tags.push(trimmed.to_string());
        }
    }

    tags
}

fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/')
}

fn dedup_preserve_order(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let trimmed = item.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_ascii_lowercase();
        if seen.insert(key) {
            out.push(trimmed);
        }
    }
    out
}

fn build_breadcrumb_chunks(
    lines: &[&str],
    body_start_idx: usize,
    title: &str,
    aliases: &[String],
    tags: &[String],
    max_chunk_chars: usize,
) -> Vec<WikiChunk> {
    let mut chunks = Vec::new();
    let mut heading_stack: Vec<(usize, String)> = Vec::new();
    let mut current_heading_path = "(root)".to_string();
    let mut buf: Vec<(usize, &str)> = Vec::new();
    let mut buf_chars = 0usize;
    let mut fence_marker: Option<char> = None;

    let aliases_str = if aliases.is_empty() {
        "none".to_string()
    } else {
        aliases.join(", ")
    };
    let tags_str = if tags.is_empty() {
        "none".to_string()
    } else {
        tags.join(", ")
    };

    let flush = |chunks: &mut Vec<WikiChunk>,
                 buf: &mut Vec<(usize, &str)>,
                 buf_chars: &mut usize,
                 heading_path: &str| {
        if buf.is_empty() {
            return;
        }
        let raw_snippet: String = buf
            .iter()
            .map(|(_, line)| *line)
            .collect::<Vec<_>>()
            .join("\n");
        if raw_snippet.trim().is_empty() {
            buf.clear();
            *buf_chars = 0;
            return;
        }

        let start_line = buf.first().map(|(ln, _)| *ln).unwrap_or(1);
        let end_line = buf.last().map(|(ln, _)| *ln).unwrap_or(start_line);
        let chunk_seq = chunks.len();
        let chunk_text = format!(
            "[Title: {title} | Aliases: {aliases_str} | Tags: {tags_str} | Section: {heading_path}]\n{raw_snippet}"
        );
        chunks.push(WikiChunk {
            chunk_seq,
            heading_path: heading_path.to_string(),
            raw_snippet,
            chunk_text,
            start_line,
            end_line,
        });
        buf.clear();
        *buf_chars = 0;
    };

    for (offset, &line) in lines.iter().skip(body_start_idx).enumerate() {
        let line_no = body_start_idx + offset + 1;
        let trimmed = line.trim_start();

        if let Some(marker) = fence_marker {
            if is_closing_fence(trimmed, marker) {
                fence_marker = None;
            }
            if buf_chars + line.len() + 1 > max_chunk_chars && !buf.is_empty() {
                flush(&mut chunks, &mut buf, &mut buf_chars, &current_heading_path);
            }
            buf_chars += line.len() + 1;
            buf.push((line_no, line));
            continue;
        }

        if let Some(marker) = opening_fence(trimmed) {
            fence_marker = Some(marker);
            if buf_chars + line.len() + 1 > max_chunk_chars && !buf.is_empty() {
                flush(&mut chunks, &mut buf, &mut buf_chars, &current_heading_path);
            }
            buf_chars += line.len() + 1;
            buf.push((line_no, line));
            continue;
        }

        if let Some((level, heading_text)) = parse_atx_heading(trimmed) {
            flush(&mut chunks, &mut buf, &mut buf_chars, &current_heading_path);
            while let Some((top_level, _)) = heading_stack.last() {
                if *top_level >= level {
                    heading_stack.pop();
                } else {
                    break;
                }
            }
            let prefix = "#".repeat(level);
            heading_stack.push((level, format!("{prefix} {heading_text}")));
            current_heading_path = heading_stack
                .iter()
                .map(|(_, h)| h.as_str())
                .collect::<Vec<_>>()
                .join(" > ");

            buf_chars += line.len() + 1;
            buf.push((line_no, line));
            continue;
        }

        if buf_chars + line.len() + 1 > max_chunk_chars && !buf.is_empty() {
            flush(&mut chunks, &mut buf, &mut buf_chars, &current_heading_path);
        }
        buf_chars += line.len() + 1;
        buf.push((line_no, line));
    }

    flush(&mut chunks, &mut buf, &mut buf_chars, &current_heading_path);

    if chunks.is_empty() {
        let heading_path = "(frontmatter)".to_string();
        let raw_snippet = title.to_string();
        let chunk_text = format!(
            "[Title: {title} | Aliases: {aliases_str} | Tags: {tags_str} | Section: {heading_path}]\n{raw_snippet}"
        );
        chunks.push(WikiChunk {
            chunk_seq: 0,
            heading_path,
            raw_snippet,
            chunk_text,
            start_line: 1,
            end_line: lines.len().max(1),
        });
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_wikilinks_tags_and_breadcrumbs() {
        let path = VirtualPath::new("/semantic/sqlite-wal.md").unwrap();
        let md = r##"---
title: SQLite 并发与锁配置
aliases: ["BUSY_TIMEOUT", "SQLite 锁"]
tags:
  - db/sqlite
  - "#architecture/storage"
entities:
  - "SQLite -> uses -> WAL Mode"
importance: 0.92
---
# 存储引擎

## 并发锁处理
必须设置 `PRAGMA busy_timeout = 5000`，详见 [[/procedural/sqlite-lock-sop.md|锁处理SOP]] 与 [[adr-001#WAL|ADR-001]]。
标签示例 #perf/low-latency，但跳过代码里的 `[[ignored]]` 和 `#ignored-tag`。

```markdown
# Not A Heading
[[ignored-in-fence]]
```
"##;

        let parsed = parse_wiki_document(&path, md, 800);
        assert_eq!(parsed.title, "SQLite 并发与锁配置");
        assert_eq!(parsed.aliases, vec!["BUSY_TIMEOUT", "SQLite 锁"]);
        assert_eq!(
            parsed.tags,
            vec!["db/sqlite", "architecture/storage", "perf/low-latency"]
        );
        assert_eq!(parsed.importance, Some(0.92));
        assert_eq!(parsed.entities.len(), 1);
        assert_eq!(parsed.entities[0].source, "SQLite");
        assert_eq!(parsed.entities[0].relation, "uses");
        assert_eq!(parsed.entities[0].target, "WAL Mode");

        assert_eq!(parsed.wikilinks.len(), 2);
        assert_eq!(parsed.wikilinks[0].target, "/procedural/sqlite-lock-sop.md");
        assert_eq!(parsed.wikilinks[0].alias.as_deref(), Some("锁处理SOP"));
        assert_eq!(parsed.wikilinks[1].target, "adr-001");
        assert_eq!(parsed.wikilinks[1].raw_target, "adr-001#WAL");

        assert!(parsed
            .chunks
            .iter()
            .any(|c| c.heading_path == "# 存储引擎 > ## 并发锁处理"
                && c.chunk_text.contains("Aliases: BUSY_TIMEOUT, SQLite 锁")));
    }

    #[test]
    fn extracts_standard_markdown_note_links_only() {
        let path = VirtualPath::new("/semantic/docs/overview.md").unwrap();
        let md = r##"# Overview

See [Order Research](./order/research.md) and [SOP](../sop.md#step-1) plus [Engine](/semantic/order-matching-engine.md).
Ignore [External](https://example.com/guide.md), ![Diagram](./arch.png), `[Ignored](./code.md)`, and [Section](#overview).
"##;
        let parsed = parse_wiki_document(&path, md, 800);
        let targets: Vec<&str> = parsed.wikilinks.iter().map(|w| w.target.as_str()).collect();
        assert_eq!(
            targets,
            vec![
                "./order/research.md",
                "../sop.md",
                "/semantic/order-matching-engine.md",
            ]
        );
        assert_eq!(parsed.wikilinks[0].alias.as_deref(), Some("Order Research"));
    }
}
