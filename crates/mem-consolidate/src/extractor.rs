//! Deterministic extraction, deduplication, classification, and Obsidian Markdown
//! rendering for cold-path episodic consolidation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use mem_core::{MemoryTier, VirtualPath};
use mem_search::{is_linkworthy_anchor, parse_wiki_document, EntityTriple, WikiLink};

#[derive(Debug, Clone)]
pub(crate) struct SourceEpisode {
    pub path: VirtualPath,
    #[allow(dead_code)]
    pub title: String,
    pub tags: Vec<String>,
    pub entities: Vec<EntityTriple>,
    #[allow(dead_code)]
    pub wikilinks: Vec<WikiLink>,
    pub importance: f32,
    #[allow(dead_code)]
    pub updated_at_ms: i64,
    pub raw_text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct BranchAnchor {
    pub path: VirtualPath,
    pub title: String,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DistilledCandidate {
    pub tier: MemoryTier,
    pub target_dir_override: Option<VirtualPath>,
    pub title: String,
    pub slug: String,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    pub entities: Vec<EntityTriple>,
    pub statements: Vec<String>,
    pub source_paths: Vec<VirtualPath>,
    pub importance: f32,
}

#[derive(Debug, Clone)]
struct AtomicStatement {
    text: String,
    tier: MemoryTier,
    topic_key: String,
    topic_title: String,
    tags: Vec<String>,
    entities: Vec<EntityTriple>,
    source_path: VirtualPath,
    importance: f32,
}

pub(crate) fn extract_deterministic_cards(episodes: &[SourceEpisode]) -> Vec<DistilledCandidate> {
    let mut accepted: Vec<AtomicStatement> = Vec::new();
    let mut exact_seen: HashSet<String> = HashSet::new();
    let mut accepted_tokens: Vec<HashSet<String>> = Vec::new();

    for ep in episodes {
        let parsed = parse_wiki_document(&ep.path, &ep.raw_text, 800);
        let base_topic = select_episode_topic(ep, &parsed.title);
        let lines = extract_body_lines(&ep.raw_text);

        let mut current_heading: Option<String> = None;
        let mut ep_has_statement = false;
        let mut in_fence = false;
        let mut pending_stmt: Option<(Option<String>, String)> = None;

        let mut accept_statement = |heading: Option<&str>, raw_stmt: String| {
            let cleaned = clean_statement_line(&raw_stmt);
            if cleaned.chars().count() < 8 {
                return;
            }

            let norm = normalize_statement(&cleaned);
            if norm.is_empty() || !exact_seen.insert(norm.clone()) {
                return;
            }

            let tokens = tokenize_for_similarity(&norm);
            if accepted_tokens
                .iter()
                .any(|existing| jaccard_similarity(existing, &tokens) >= 0.72)
            {
                return;
            }
            accepted_tokens.push(tokens);

            let tier = classify_statement_tier(&cleaned, ep, heading);
            let topic_title = heading
                .filter(|h| !is_generic_heading(h))
                .unwrap_or(&base_topic)
                .to_string();
            let topic_key = slugify(&topic_title);

            accepted.push(AtomicStatement {
                text: cleaned,
                tier,
                topic_key,
                topic_title,
                tags: ep.tags.clone(),
                entities: ep.entities.clone(),
                source_path: ep.path.clone(),
                importance: ep.importance,
            });
            ep_has_statement = true;
        };

        for line in lines {
            let trimmed = line.trim();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                if let Some((h, s)) = pending_stmt.take() {
                    accept_statement(h.as_deref(), s);
                }
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }
            if trimmed.is_empty() {
                if let Some((h, s)) = pending_stmt.take() {
                    accept_statement(h.as_deref(), s);
                }
                continue;
            }
            if let Some(heading) = strip_markdown_heading(trimmed) {
                if let Some((h, s)) = pending_stmt.take() {
                    accept_statement(h.as_deref(), s);
                }
                if !heading.eq_ignore_ascii_case("related")
                    && !heading.eq_ignore_ascii_case("references")
                {
                    current_heading = Some(heading.to_string());
                } else {
                    current_heading = None;
                }
                continue;
            }

            // Skip pure wikilink bullets inside a "## Related" section.
            if current_heading.is_none() && is_pure_wikilink_bullet(trimmed) {
                continue;
            }

            let is_indented_cont =
                (line.starts_with("  ") || line.starts_with('\t')) && !is_list_item_start(trimmed);
            if is_indented_cont {
                if let Some((_, ref mut prev)) = pending_stmt {
                    prev.push(' ');
                    prev.push_str(trimmed);
                    continue;
                }
            }

            if let Some((h, s)) = pending_stmt.take() {
                accept_statement(h.as_deref(), s);
            }
            pending_stmt = Some((current_heading.clone(), trimmed.to_string()));
        }
        if let Some((h, s)) = pending_stmt.take() {
            accept_statement(h.as_deref(), s);
        }

        // Fallback if episode had only short or heading-only content.
        if !ep_has_statement {
            let fallback = clean_statement_line(&ep.raw_text);
            if !fallback.is_empty() {
                let norm = normalize_statement(&fallback);
                if exact_seen.insert(norm.clone()) {
                    let tokens = tokenize_for_similarity(&norm);
                    if !accepted_tokens
                        .iter()
                        .any(|existing| jaccard_similarity(existing, &tokens) >= 0.72)
                    {
                        accepted_tokens.push(tokens);
                        let tier = classify_statement_tier(&fallback, ep, None);
                        accepted.push(AtomicStatement {
                            text: truncate_chars(&fallback, 280),
                            tier,
                            topic_key: slugify(&base_topic),
                            topic_title: base_topic,
                            tags: ep.tags.clone(),
                            entities: ep.entities.clone(),
                            source_path: ep.path.clone(),
                            importance: ep.importance,
                        });
                    }
                }
            }
        }
    }

    // Group accepted statements by (tier, topic_key).
    let mut groups: BTreeMap<(u8, String), Vec<AtomicStatement>> = BTreeMap::new();
    for stmt in accepted {
        let tier_rank = match stmt.tier {
            MemoryTier::Procedural => 1,
            MemoryTier::Semantic => 0,
            _ => 0,
        };
        groups
            .entry((tier_rank, stmt.topic_key.clone()))
            .or_default()
            .push(stmt);
    }

    let mut cards = Vec::with_capacity(groups.len());
    for ((tier_rank, slug), items) in groups {
        let tier = if tier_rank == 1 {
            MemoryTier::Procedural
        } else {
            MemoryTier::Semantic
        };
        let raw_title = items
            .first()
            .map(|i| i.topic_title.clone())
            .unwrap_or_else(|| "Consolidated Insights".to_string());
        let title = if tier == MemoryTier::Procedural
            && !raw_title.to_ascii_lowercase().contains("sop")
            && !raw_title.to_ascii_lowercase().contains("playbook")
            && !raw_title.contains("指南")
            && !raw_title.contains("手册")
        {
            format!("{raw_title} Runbook")
        } else {
            raw_title
        };

        let mut tags = BTreeSet::new();
        tags.insert("consolidated".to_string());
        tags.insert(tier.as_str().to_string());

        let mut entities: Vec<EntityTriple> = Vec::new();
        let mut ent_seen: HashSet<(String, String, String)> = HashSet::new();
        let mut sources = BTreeSet::new();
        let mut statements = Vec::new();
        let mut max_imp: f32 = if tier == MemoryTier::Procedural {
            0.80
        } else {
            0.75
        };

        for item in items {
            max_imp = max_imp.max(item.importance);
            sources.insert(item.source_path);
            for t in item.tags {
                let clean = t.trim().to_string();
                if !clean.is_empty() && clean != "episodic" && clean != "archived-episode" {
                    tags.insert(clean);
                }
            }
            for ent in item.entities {
                let key = (
                    ent.source.to_lowercase(),
                    ent.relation.to_lowercase(),
                    ent.target.to_lowercase(),
                );
                if ent_seen.insert(key) {
                    entities.push(ent);
                }
            }
            statements.push(item.text);
        }

        let mut aliases = vec![format!("consolidated-{slug}")];
        for err_alias in extract_error_aliases(&statements) {
            if !aliases.iter().any(|a| a.eq_ignore_ascii_case(&err_alias)) {
                aliases.push(err_alias);
            }
        }
        cards.push(DistilledCandidate {
            tier,
            target_dir_override: None,
            title,
            slug,
            aliases,
            tags: tags.into_iter().collect(),
            entities,
            statements,
            source_paths: sources.into_iter().collect(),
            importance: max_imp.clamp(0.1, 1.0),
        });
    }

    cards
}

pub(crate) fn render_card_markdown(
    candidate: &DistilledCandidate,
    existing_markdown: Option<&str>,
    anchors: &[BranchAnchor],
    target_path: &VirtualPath,
    consolidated_at: &str,
) -> (String, Vec<String>, Vec<VirtualPath>) {
    let mut merged_statements: Vec<String> = Vec::new();
    let mut exact_seen: HashSet<String> = HashSet::new();
    let mut token_sets: Vec<HashSet<String>> = Vec::new();

    let mut merged_aliases = candidate.aliases.clone();
    let mut merged_tags: BTreeSet<String> = candidate.tags.iter().cloned().collect();
    let mut merged_entities = candidate.entities.clone();
    let mut merged_sources: BTreeSet<String> = candidate
        .source_paths
        .iter()
        .map(|p| p.as_str().to_string())
        .collect();
    let mut final_importance = candidate.importance;

    if let Some(existing) = existing_markdown {
        let parsed = parse_wiki_document(target_path, existing, 800);
        for alias in parsed.aliases {
            if !merged_aliases
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&alias))
            {
                merged_aliases.push(alias);
            }
        }
        for tag in parsed.tags {
            merged_tags.insert(tag);
        }
        for ent in parsed.entities {
            if !merged_entities.iter().any(|e| {
                e.source.eq_ignore_ascii_case(&ent.source)
                    && e.relation.eq_ignore_ascii_case(&ent.relation)
                    && e.target.eq_ignore_ascii_case(&ent.target)
            }) {
                merged_entities.push(ent);
            }
        }
        if let Some(imp) = parsed.importance {
            final_importance = final_importance.max(imp);
        }
        for line in extract_body_lines(existing) {
            let t = line.trim();
            if t.starts_with("- /episodic/") {
                merged_sources.insert(t.trim_start_matches("- ").trim().to_string());
                continue;
            }
            if t.starts_with("- [[") && t.ends_with("]]") {
                continue;
            }
            if t.starts_with('#') {
                continue;
            }
            let clean = clean_statement_line(t);
            if clean.chars().count() < 8 {
                continue;
            }
            let norm = normalize_statement(&clean);
            if exact_seen.insert(norm.clone()) {
                let tokens = tokenize_for_similarity(&norm);
                token_sets.push(tokens);
                merged_statements.push(clean);
            }
        }
    }

    for stmt in &candidate.statements {
        let norm = normalize_statement(stmt);
        if norm.is_empty() || !exact_seen.insert(norm.clone()) {
            continue;
        }
        let tokens = tokenize_for_similarity(&norm);
        if token_sets
            .iter()
            .any(|existing| jaccard_similarity(existing, &tokens) >= 0.72)
        {
            continue;
        }
        token_sets.push(tokens);
        merged_statements.push(stmt.clone());
    }

    // Canonicalize tags & entity names against existing active branch anchors.
    let mut canonical_tags_map: HashMap<String, String> = HashMap::new();
    let mut canonical_entity_map: HashMap<String, String> = HashMap::new();
    for anchor in anchors {
        for t in &anchor.tags {
            canonical_tags_map
                .entry(t.trim().to_ascii_lowercase())
                .or_insert_with(|| t.trim().to_string());
        }
        let tl = anchor.title.trim().to_ascii_lowercase();
        if !tl.is_empty() {
            canonical_entity_map
                .entry(tl)
                .or_insert_with(|| anchor.title.trim().to_string());
        }
        for al in &anchor.aliases {
            let al_low = al.trim().to_ascii_lowercase();
            if !al_low.is_empty() {
                canonical_entity_map
                    .entry(al_low)
                    .or_insert_with(|| anchor.title.trim().to_string());
            }
        }
    }
    let normalized_tags: BTreeSet<String> = merged_tags
        .into_iter()
        .map(|t| {
            canonical_tags_map
                .get(&t.trim().to_ascii_lowercase())
                .cloned()
                .unwrap_or(t)
        })
        .collect();
    merged_tags = normalized_tags;
    for ent in &mut merged_entities {
        if let Some(canon) = canonical_entity_map.get(&ent.source.trim().to_ascii_lowercase()) {
            ent.source = canon.clone();
        }
        if let Some(canon) = canonical_entity_map.get(&ent.target.trim().to_ascii_lowercase()) {
            ent.target = canon.clone();
        }
    }

    // Discover related active notes for [[Wikilink]] weaving with IDF/anti-hub filtering.
    let total_docs = anchors.len();
    let mut term_df: HashMap<String, usize> = HashMap::new();
    for anchor in anchors {
        let mut seen = HashSet::new();
        let tl = anchor.title.trim().to_ascii_lowercase();
        if !tl.is_empty() && seen.insert(tl.clone()) {
            *term_df.entry(tl).or_insert(0) += 1;
        }
        for al in &anchor.aliases {
            let all = al.trim().to_ascii_lowercase();
            if !all.is_empty() && seen.insert(all.clone()) {
                *term_df.entry(all).or_insert(0) += 1;
            }
        }
    }

    let combined_text = format!("{} {}", candidate.title, merged_statements.join("\n"));
    let combined_lower = combined_text.to_ascii_lowercase();
    let mut linked_titles: Vec<String> = Vec::new();
    let mut linked_seen: HashSet<String> = HashSet::new();
    let mut matched_paths: Vec<VirtualPath> = Vec::new();
    let mut weave_pairs: Vec<(String, String)> = Vec::new();

    for anchor in anchors {
        if &anchor.path == target_path || anchor.title.eq_ignore_ascii_case(&candidate.title) {
            continue;
        }
        let anchor_lower = anchor.title.to_ascii_lowercase();
        let t_df = term_df.get(&anchor_lower).copied().unwrap_or(1);
        let mut matched = false;
        if is_linkworthy_anchor(&anchor.title, t_df, total_docs)
            && contains_term(&combined_lower, &anchor_lower)
        {
            weave_pairs.push((anchor.title.clone(), anchor.title.clone()));
            matched = true;
        }
        for alias in &anchor.aliases {
            let alias_lower = alias.to_ascii_lowercase();
            let a_df = term_df.get(&alias_lower).copied().unwrap_or(1);
            if is_linkworthy_anchor(alias, a_df, total_docs)
                && contains_term(&combined_lower, &alias_lower)
            {
                weave_pairs.push((alias.clone(), anchor.title.clone()));
                matched = true;
            }
        }
        if matched && linked_seen.insert(anchor_lower) {
            linked_titles.push(anchor.title.clone());
            matched_paths.push(anchor.path.clone());
        }
    }
    weave_pairs.sort_by_key(|a| std::cmp::Reverse(a.0.len()));

    // Render YAML Frontmatter.
    let escaped_title = candidate.title.replace('"', "\\\"");
    let alias_list = merged_aliases
        .iter()
        .map(|a| format!("\"{}\"", a.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let tag_list = merged_tags
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = format!(
        "---\ntitle: \"{escaped_title}\"\naliases: [{alias_list}]\ntags: [{tag_list}]\nimportance: {:.2}\nconsolidated_at: \"{consolidated_at}\"\n",
        final_importance
    );
    if !merged_entities.is_empty() {
        out.push_str("entities:\n");
        for ent in &merged_entities {
            out.push_str(&format!(
                "  - \"{} -> {} -> {}\"\n",
                ent.source.replace('"', "\\\""),
                ent.relation.replace('"', "\\\""),
                ent.target.replace('"', "\\\""),
            ));
        }
    }
    out.push_str("---\n\n");
    out.push_str(&format!("# {}\n\n", candidate.title));

    if candidate.tier == MemoryTier::Procedural {
        let mut triggers = Vec::new();
        let mut steps = Vec::new();
        let mut verifications = Vec::new();
        for stmt in &merged_statements {
            match classify_procedural_role(stmt) {
                ProceduralRole::Trigger => triggers.push(stmt.as_str()),
                ProceduralRole::Verification => verifications.push(stmt.as_str()),
                ProceduralRole::Step => steps.push(stmt.as_str()),
            }
        }
        if steps.is_empty() {
            out.push_str("## Standard Operating Procedure\n\n");
            for (idx, stmt) in merged_statements.iter().enumerate() {
                let woven = weave_line_wikilinks(stmt, &weave_pairs);
                out.push_str(&format!("{}. {}\n", idx + 1, woven));
            }
        } else {
            if !triggers.is_empty() {
                out.push_str("### Trigger & Symptoms\n\n");
                for trig in &triggers {
                    let woven = weave_line_wikilinks(trig, &weave_pairs);
                    out.push_str(&format!("- {woven}\n"));
                }
                out.push('\n');
            }
            out.push_str("## Standard Operating Procedure\n\n");
            for (idx, stmt) in steps.iter().enumerate() {
                let woven = weave_line_wikilinks(stmt, &weave_pairs);
                out.push_str(&format!("{}. {}\n", idx + 1, woven));
            }
            if !verifications.is_empty() {
                out.push_str("\n### Verification\n\n");
                for ver in &verifications {
                    let woven = weave_line_wikilinks(ver, &weave_pairs);
                    out.push_str(&format!("- {woven}\n"));
                }
            }
        }
    } else {
        out.push_str("## Distilled Facts & Decisions\n\n");
        for stmt in &merged_statements {
            let woven = weave_line_wikilinks(stmt, &weave_pairs);
            out.push_str(&format!("- {}\n", woven));
        }
    }

    if !merged_sources.is_empty() {
        out.push_str("\n## Consolidated Sources\n");
        for src in &merged_sources {
            out.push_str(&format!("- {src}\n"));
        }
    }

    if !linked_titles.is_empty() {
        out.push_str("\n## Related\n");
        for link in &linked_titles {
            out.push_str(&format!("- [[{link}]]\n"));
        }
    }

    (out, linked_titles, matched_paths)
}

enum ProceduralRole {
    Trigger,
    Step,
    Verification,
}

fn classify_procedural_role(stmt: &str) -> ProceduralRole {
    let lower = stmt.to_ascii_lowercase();
    let starts_step = [
        "sop:",
        "step ",
        "step:",
        "runbook:",
        "fix:",
        "set ",
        "run ",
        "execute ",
        "configure ",
        "always ",
        "pragma ",
    ]
    .iter()
    .any(|kw| lower.starts_with(kw));
    if starts_step {
        return ProceduralRole::Step;
    }

    if [
        "verify ",
        "confirm ",
        "assert ",
        "validate ",
        "verification:",
        "验证",
        "确认",
        "测试",
    ]
    .iter()
    .any(|kw| lower.starts_with(kw))
        || lower.contains("to verify")
        || lower.contains("verify that")
    {
        return ProceduralRole::Verification;
    }

    if [
        "symptom",
        "triggered",
        "when ",
        "timed out",
        "timeout",
        "error:",
        "error ",
        "failed with",
        "http 429",
        "http 500",
        "http 503",
        "报错",
        "异常",
        "触发",
        "失败时",
        "超时",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        return ProceduralRole::Trigger;
    }

    ProceduralRole::Step
}

fn extract_error_aliases(statements: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for stmt in statements {
        for raw in stmt.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            if raw.len() >= 5
                && raw.contains('_')
                && raw
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && raw.chars().filter(|c| c.is_ascii_uppercase()).count() >= 2
                && seen.insert(raw.to_string())
            {
                out.push(raw.to_string());
            }
        }
        let lower = stmt.to_ascii_lowercase();
        for code in ["429", "500", "502", "503", "504"] {
            if lower.contains(&format!("http {code}")) || lower.contains(&format!("{code} ")) {
                let alias = format!("HTTP {code}");
                if seen.insert(alias.clone()) {
                    out.push(alias);
                }
            }
        }
    }
    out
}

pub(crate) fn slugify(raw: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in raw.chars() {
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
        if slug.chars().count() >= 48 {
            break;
        }
    }
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        let h = blake3::hash(raw.as_bytes()).to_hex();
        format!("topic-{}", &h[..8])
    } else {
        trimmed
    }
}

fn select_episode_topic(ep: &SourceEpisode, parsed_title: &str) -> String {
    for tag in &ep.tags {
        let lower = tag.trim().to_ascii_lowercase();
        if lower.is_empty()
            || matches!(
                lower.as_str(),
                "episodic"
                    | "log"
                    | "session"
                    | "bot"
                    | "role/bot"
                    | "role/dev"
                    | "role/pm"
                    | "consolidated"
            )
        {
            continue;
        }
        let leaf = lower.rsplit('/').next().unwrap_or(&lower);
        if leaf.len() >= 2 {
            return humanize_slug(leaf);
        }
    }

    let cleaned = parsed_title.trim();
    if !cleaned.is_empty() && !is_generic_heading(cleaned) {
        return cleaned.to_string();
    }

    "General Insights".to_string()
}

fn humanize_slug(s: &str) -> String {
    s.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => {
                    let mut part = first.to_uppercase().to_string();
                    part.push_str(chars.as_str());
                    part
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn extract_body_lines(raw: &str) -> Vec<&str> {
    let normalized = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let lines: Vec<&str> = normalized.lines().collect();
    if lines.first().is_some_and(|l| l.trim() == "---") {
        if let Some(end_idx) =
            lines.iter().enumerate().skip(1).find_map(|(idx, line)| {
                (line.trim() == "---" || line.trim() == "...").then_some(idx)
            })
        {
            return lines[end_idx + 1..].to_vec();
        }
    }
    lines
}

fn strip_markdown_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = trimmed[hashes..].trim();
        if !rest.is_empty() {
            return Some(rest);
        }
    }
    None
}

fn is_generic_heading(heading: &str) -> bool {
    let h = heading.trim().to_ascii_lowercase();
    matches!(
        h.as_str(),
        "observation & context"
            | "summary"
            | "context"
            | "notes"
            | "log"
            | "session log"
            | "timeline"
            | "distilled facts & decisions"
            | "standard operating procedure"
            | "consolidated sources"
            | "related"
    )
}

fn is_pure_wikilink_bullet(line: &str) -> bool {
    let inner = line.trim_start_matches(['-', '*', ' ']).trim();
    inner.starts_with("[[") && inner.ends_with("]]")
}

fn is_list_item_start(trimmed: &str) -> bool {
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("> ") {
        return true;
    }
    if let Some(idx) = trimmed.find(['.', ')']) {
        if idx > 0 && idx <= 3 && trimmed[..idx].chars().all(|c| c.is_ascii_digit()) {
            return trimmed[idx + 1..].starts_with(' ');
        }
    }
    false
}

fn clean_statement_line(line: &str) -> String {
    let mut s = line.trim();
    s = s.trim_start_matches(['-', '*', '>', ' ']).trim();
    // Strip leading numbered list marker like "1. " or "2) "
    if let Some(idx) = s.find(['.', ')']) {
        if idx > 0 && idx <= 3 && s[..idx].chars().all(|c| c.is_ascii_digit()) {
            let after = &s[idx + 1..];
            if after.starts_with(' ') {
                s = after.trim();
            }
        }
    }
    // Strip leading timestamp like "[09:05] " or "09:05 "
    if s.len() >= 6 && s.chars().take(2).all(|c| c.is_ascii_digit()) && s.as_bytes()[2] == b':' {
        if let Some(pos) = s.find(' ') {
            s = s[pos + 1..].trim();
        }
    }
    s.to_string()
}

fn classify_statement_tier(
    statement: &str,
    ep: &SourceEpisode,
    heading: Option<&str>,
) -> MemoryTier {
    let lower = statement.to_ascii_lowercase();
    let heading_lower = heading.unwrap_or("").to_ascii_lowercase();
    let path_lower = ep.path.as_str().to_ascii_lowercase();

    let procedural_keywords = [
        "sop",
        "runbook",
        "playbook",
        "checklist",
        "troubleshoot",
        "debug",
        "workaround",
        "remediation",
        "mitigation",
        "step ",
        "steps:",
        "must set",
        "always run",
        "execute ",
        "pragma ",
        "retry ",
        "restart ",
        "rollback",
        "rewind",
        "how to",
        "fix:",
        "排查",
        "步骤",
        "修复",
        "解决",
        "回滚",
        "重试",
        "配置",
        "命令",
        "注意",
        "操作",
        "指南",
        "手册",
    ];

    if procedural_keywords
        .iter()
        .any(|kw| lower.contains(kw) || heading_lower.contains(kw))
        || path_lower.contains("sop")
        || path_lower.contains("runbook")
        || ep.tags.iter().any(|t| {
            matches!(
                t.to_ascii_lowercase().as_str(),
                "sop" | "runbook" | "procedural" | "ops"
            )
        })
    {
        MemoryTier::Procedural
    } else {
        MemoryTier::Semantic
    }
}

fn normalize_statement(s: &str) -> String {
    s.to_lowercase()
        .replace("[[", "")
        .replace("]]", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn tokenize_for_similarity(norm: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for tok in norm.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if tok.chars().count() >= 2 {
            set.insert(tok.to_string());
        }
    }
    let non_ascii: Vec<char> = norm
        .chars()
        .filter(|c| !c.is_ascii() && c.is_alphanumeric())
        .collect();
    for pair in non_ascii.windows(2) {
        let mut bg = String::with_capacity(8);
        bg.push(pair[0]);
        bg.push(pair[1]);
        set.insert(bg);
    }
    set
}

fn jaccard_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

fn contains_term(haystack_lower: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return false;
    }
    let ascii = needle_lower.is_ascii();
    let mut start = 0usize;
    while let Some(rel) = haystack_lower[start..].find(needle_lower) {
        let abs = start + rel;
        let end = abs + needle_lower.len();
        if !ascii || ascii_word_bounded(haystack_lower, abs, end) {
            return true;
        }
        start = abs + needle_lower.chars().next().map(char::len_utf8).unwrap_or(1);
    }
    false
}

fn ascii_word_bounded(text: &str, start: usize, end: usize) -> bool {
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

fn weave_line_wikilinks(line: &str, pairs: &[(String, String)]) -> String {
    let mut current = line.to_string();
    let mut linked = HashSet::new();
    for (needle, target) in pairs {
        if linked.contains(&target.to_ascii_lowercase()) {
            continue;
        }
        if current.contains(&format!("[[{target}")) {
            linked.insert(target.to_ascii_lowercase());
            continue;
        }
        let lower = current.to_ascii_lowercase();
        let needle_lower = needle.to_ascii_lowercase();
        let ascii = needle.is_ascii();
        if let Some(pos) = lower.find(&needle_lower) {
            let end = pos + needle_lower.len();
            if current.is_char_boundary(pos)
                && current.is_char_boundary(end)
                && (!ascii || ascii_word_bounded(&lower, pos, end))
                && !inside_brackets_or_backticks(&current, pos)
            {
                let slice = &current[pos..end];
                let repl = if slice == target {
                    format!("[[{target}]]")
                } else {
                    format!("[[{target}|{slice}]]")
                };
                let mut next = String::with_capacity(current.len() + repl.len());
                next.push_str(&current[..pos]);
                next.push_str(&repl);
                next.push_str(&current[end..]);
                current = next;
                linked.insert(target.to_ascii_lowercase());
            }
        }
    }
    current
}

fn inside_brackets_or_backticks(line: &str, byte_pos: usize) -> bool {
    let prefix = &line[..byte_pos];
    let open_wiki = prefix.matches("[[").count();
    let close_wiki = prefix.matches("]]").count();
    if open_wiki > close_wiki {
        return true;
    }
    let backticks = prefix.matches('`').count();
    backticks % 2 == 1
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}
