//! Deterministic input sanitization, secret redaction, log denoising, 64-bit SimHash,
//! and IDF-aware anti-hub anchor filtering for memory ingestion and consolidation.

const REDACTED_MARKER: &str = "[REDACTED_SECRET]";

/// Scans text for common API keys, Bearer tokens, AWS keys, and PEM private key blocks,
/// returning `(sanitized_text, redaction_count)`.
pub fn redact_secrets(input: &str) -> (String, usize) {
    let mut count = 0usize;
    let mut lines_out: Vec<String> = Vec::new();
    let mut in_pem_key = false;

    for raw_line in input.split('\n') {
        let trimmed = raw_line.trim();
        if !in_pem_key && trimmed.starts_with("-----BEGIN ") && trimmed.contains("PRIVATE KEY-----")
        {
            in_pem_key = true;
            count += 1;
            lines_out.push(REDACTED_MARKER.to_string());
            if trimmed.contains("-----END ") && trimmed.ends_with("-----") {
                in_pem_key = false;
            }
            continue;
        }
        if in_pem_key {
            if trimmed.starts_with("-----END ") && trimmed.contains("PRIVATE KEY-----") {
                in_pem_key = false;
            }
            continue;
        }

        let (redacted_line, line_hits) = redact_inline_tokens(raw_line);
        count += line_hits;
        lines_out.push(redacted_line);
    }

    (lines_out.join("\n"), count)
}

fn redact_inline_tokens(line: &str) -> (String, usize) {
    let mut out = String::with_capacity(line.len());
    let mut hits = 0usize;
    let bytes = line.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        // Check `Bearer <token>` (case-insensitive prefix, >= 24 token chars).
        if line[i..].len() > 7 && line[i..i + 7].eq_ignore_ascii_case("bearer ") {
            let tok_start = i + 7;
            let tok_len = bytes[tok_start..]
                .iter()
                .take_while(|&&b| is_bearer_char(b))
                .count();
            if tok_len >= 24 {
                out.push_str(&line[i..tok_start]);
                out.push_str(REDACTED_MARKER);
                hits += 1;
                i = tok_start + tok_len;
                continue;
            }
        }

        // Check prefixed credentials when at a token boundary.
        let at_boundary = i == 0 || !is_ident_char(bytes[i - 1]);
        if at_boundary {
            if let Some(len) = match_prefixed_secret(&line[i..]) {
                out.push_str(REDACTED_MARKER);
                hits += 1;
                i += len;
                continue;
            }
        }

        let ch = line[i..].chars().next().expect("valid utf8 boundary");
        out.push(ch);
        i += ch.len_utf8();
    }

    (out, hits)
}

fn match_prefixed_secret(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if s.starts_with("sk-") {
        let rest_len = b[3..].iter().take_while(|&&c| is_secret_char(c)).count();
        if rest_len >= 20 {
            return Some(3 + rest_len);
        }
    } else if s.starts_with("ghp_") {
        let rest_len = b[4..].iter().take_while(|&&c| is_alnum(c)).count();
        if rest_len >= 20 {
            return Some(4 + rest_len);
        }
    } else if s.starts_with("github_pat_") {
        let rest_len = b[11..].iter().take_while(|&&c| is_secret_char(c)).count();
        if rest_len >= 20 {
            return Some(11 + rest_len);
        }
    } else if s.starts_with("egm_") {
        let rest_len = b[4..].iter().take_while(|&&c| is_alnum(c)).count();
        if rest_len >= 24 {
            return Some(4 + rest_len);
        }
    } else if s.starts_with("AKIA") && b.len() >= 20 {
        let key_ok = b[4..20]
            .iter()
            .all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if key_ok {
            let extra = b[20..].iter().take_while(|&&c| is_alnum(c)).count();
            if extra == 0 {
                return Some(20);
            }
        }
    }
    None
}

fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn is_secret_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_bearer_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
}

/// Strips ANSI escapes, control chars, deep stack frames (>8 lines), oversized unbroken blobs
/// (>2048 bytes), and optionally consecutive repeated log lines.
pub fn denoise_markdown(input: &str, collapse_log_repeats: bool) -> String {
    let stripped = strip_ansi_and_controls(input);
    let mut lines_stage1: Vec<String> = Vec::new();
    let mut stack_run: Vec<String> = Vec::new();

    let flush_stack = |stack: &mut Vec<String>, out: &mut Vec<String>| {
        if stack.len() > 8 {
            out.extend(stack.iter().take(4).cloned());
            out.push(format!("... [{} stack frames omitted]", stack.len() - 6));
            let tail_start = stack.len().saturating_sub(2);
            out.extend(stack[tail_start..].iter().cloned());
        } else {
            out.extend(stack.iter().cloned());
        }
        stack.clear();
    };

    for raw_line in stripped.lines() {
        let cleaned_line = truncate_oversized_tokens(raw_line);
        if is_stack_frame_line(&cleaned_line) {
            stack_run.push(cleaned_line);
        } else {
            if !stack_run.is_empty() {
                flush_stack(&mut stack_run, &mut lines_stage1);
            }
            lines_stage1.push(cleaned_line);
        }
    }
    if !stack_run.is_empty() {
        flush_stack(&mut stack_run, &mut lines_stage1);
    }

    // Collapse repeated consecutive lines (all lines when collapse_log_repeats=true, or >=3
    // timestamped/severity-prefixed log lines otherwise so normal prose repeats remain intact).
    let mut out_lines: Vec<String> = Vec::with_capacity(lines_stage1.len());
    let mut idx = 0usize;
    while idx < lines_stage1.len() {
        let cur = &lines_stage1[idx];
        let (sig, is_log_like) = log_line_signature(cur);
        let can_collapse = !sig.is_empty() && (collapse_log_repeats || is_log_like);
        if !can_collapse {
            out_lines.push(cur.clone());
            idx += 1;
            continue;
        }

        let mut run_end = idx + 1;
        while run_end < lines_stage1.len() {
            let (next_sig, _) = log_line_signature(&lines_stage1[run_end]);
            if next_sig == sig {
                run_end += 1;
            } else {
                break;
            }
        }
        let repeat_count = run_end - idx;
        let min_repeats = if collapse_log_repeats { 2 } else { 3 };
        if repeat_count >= min_repeats {
            out_lines.push(format!("{cur} [repeated {repeat_count} times]"));
        } else {
            for item in &lines_stage1[idx..run_end] {
                out_lines.push(item.clone());
            }
        }
        idx = run_end;
    }

    let mut joined = out_lines.join("\n");
    if input.ends_with('\n') && !joined.is_empty() {
        joined.push('\n');
    }
    joined
}

fn strip_ansi_and_controls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next_c in chars.by_ref() {
                    if next_c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if ch == '\r' {
            continue;
        }
        if ch.is_control() && ch != '\n' && ch != '\t' {
            continue;
        }
        out.push(ch);
    }
    out
}

fn truncate_oversized_tokens(line: &str) -> String {
    if line.len() <= 2048 {
        return line.to_string();
    }
    let mut out = String::with_capacity(512);
    let mut first = true;
    for tok in line.split(' ') {
        if !first {
            out.push(' ');
        }
        first = false;
        if tok.len() > 2048 {
            let mut cut = 256.min(tok.len());
            while cut > 0 && !tok.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push_str(&tok[..cut]);
            out.push_str("...[truncated]");
        } else {
            out.push_str(tok);
        }
    }
    out
}

fn is_stack_frame_line(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("at ") || t.starts_with("File \"") {
        return true;
    }
    // Rust backtrace frame: `12: 0x55a1...` or `   0: std::...`
    if let Some((num, rest)) = t.split_once(':') {
        if !num.is_empty() && num.len() <= 4 && num.chars().all(|c| c.is_ascii_digit()) {
            let after = rest.trim_start();
            if after.starts_with("0x") || after.contains("::") {
                return true;
            }
        }
    }
    false
}

fn log_line_signature(line: &str) -> (String, bool) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return (String::new(), false);
    }
    let mut rest = trimmed;
    let mut is_log_like = false;

    if rest.starts_with('[') {
        if let Some(close) = rest.find(']') {
            let inside = &rest[1..close];
            if inside.chars().any(|c| c.is_ascii_digit())
                && (inside.contains(':') || inside.contains('-'))
            {
                rest = rest[close + 1..].trim_start();
                is_log_like = true;
            }
        }
    }

    for prefix in ["ERROR:", "WARN:", "INFO:", "DEBUG:", "TRACE:"] {
        if let Some(after) = rest.strip_prefix(prefix) {
            rest = after.trim_start();
            is_log_like = true;
            break;
        }
    }

    (rest.to_string(), is_log_like)
}

/// Computes a 64-bit SimHash fingerprint and token count over the substantive Markdown body
/// (excluding YAML frontmatter, boilerplate section chrome, and numeric/timestamp fragments).
pub fn compute_simhash64(text: &str) -> (u64, usize) {
    let body = extract_substantive_body(strip_frontmatter_for_hash(text));
    let mut words: Vec<String> = Vec::new();
    let mut features: Vec<String> = Vec::new();
    let lower = body.to_lowercase();

    for tok in lower.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if tok.chars().count() >= 2 && !is_numeric_or_ts_fragment(tok) {
            words.push(tok.to_string());
            features.push(tok.to_string());
        }
    }
    for pair in words.windows(2) {
        features.push(format!("{}\0{}", pair[0], pair[1]));
    }

    let cjk: Vec<char> = lower
        .chars()
        .filter(|&c| {
            matches!(
                c as u32,
                0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
            )
        })
        .collect();
    for pair in cjk.windows(2) {
        let mut bg = String::with_capacity(8);
        bg.push(pair[0]);
        bg.push(pair[1]);
        features.push(bg);
    }

    let token_count = words.len() + cjk.len().saturating_sub(1);
    if features.is_empty() {
        return (0, 0);
    }

    let mut weights = [0i32; 64];
    for feat in features {
        let digest = blake3::hash(feat.as_bytes());
        let h = u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap_or([0u8; 8]));
        for (bit, slot) in weights.iter_mut().enumerate() {
            if (h >> bit) & 1 == 1 {
                *slot += 1;
            } else {
                *slot -= 1;
            }
        }
    }

    let mut simhash = 0u64;
    for (bit, &w) in weights.iter().enumerate() {
        if w > 0 {
            simhash |= 1u64 << bit;
        }
    }
    (simhash, token_count)
}

/// Computes the Hamming distance between two 64-bit SimHash fingerprints.
pub fn simhash_hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

fn is_numeric_or_ts_fragment(tok: &str) -> bool {
    if tok.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    tok.chars().any(|c| c.is_ascii_digit())
        && tok
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 't' | 'z'))
}

fn extract_substantive_body(body: &str) -> String {
    let mut content_lines: Vec<&str> = Vec::new();
    let mut skipped_first_h1 = false;
    let mut all_lines: Vec<&str> = Vec::new();

    for raw_line in body.lines() {
        let trimmed = raw_line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower == "## related" {
            break;
        }
        all_lines.push(raw_line);
        if matches!(
            lower.as_str(),
            "## observation & context"
                | "## context / background"
                | "## key decisions & facts"
                | "## standard operating procedure"
                | "## distilled facts & decisions"
                | "## consolidated sources"
        ) {
            continue;
        }
        if !skipped_first_h1 && trimmed.starts_with("# ") && content_lines.is_empty() {
            skipped_first_h1 = true;
            continue;
        }
        if !trimmed.is_empty() {
            content_lines.push(raw_line);
        }
    }

    if content_lines.is_empty() {
        all_lines.join("\n")
    } else {
        content_lines.join("\n")
    }
}

fn strip_frontmatter_for_hash(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    if let Some(after_start) = trimmed.strip_prefix("---") {
        let after_nl = after_start
            .strip_prefix("\r\n")
            .or_else(|| after_start.strip_prefix('\n'));
        if let Some(rest) = after_nl {
            if let Some(end_idx) = rest.find("\n---") {
                return &rest[end_idx + 4..];
            }
        }
    }
    trimmed
}

/// Anti-hub filter for automatic `[[Wikilink]]` weaving: blocks generic stopwords,
/// overly short ASCII tokens (`< 3` chars), and ultra-frequent hub anchors when a branch
/// has `>= 12` notes.
pub fn is_linkworthy_anchor(term: &str, doc_freq: usize, total_docs: usize) -> bool {
    let trimmed = term.trim();
    let char_count = trimmed.chars().count();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.is_ascii() {
        if char_count < 3 {
            return false;
        }
    } else if char_count < 2 {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "summary"
            | "notes"
            | "note"
            | "context"
            | "related"
            | "config"
            | "readme"
            | "index"
            | "general"
            | "overview"
            | "system"
            | "default"
            | "配置"
            | "总结"
            | "说明"
            | "问题"
            | "记录"
            | "文档"
            | "详情"
            | "概述"
    ) {
        return false;
    }

    if total_docs >= 12 && doc_freq * 100 > total_docs * 35 {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_realistic_credentials_without_touching_normal_text() {
        let sample = "OpenAI sk-proj1234567890abcdefghij\nGitHub ghp_0123456789abcdefghij012345\nHeader Authorization: Bearer tok_0123456789abcdefghijklmnop\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAK\n-----END RSA PRIVATE KEY-----\nNormal short key sk-test remains.";
        let (clean, count) = redact_secrets(sample);
        assert_eq!(count, 4);
        assert!(!clean.contains("sk-proj1234567890"));
        assert!(!clean.contains("ghp_0123456789"));
        assert!(!clean.contains("MIIEowIBAAK"));
        assert!(clean.contains("sk-test"));
    }

    #[test]
    fn denoises_ansi_stack_traces_and_repeated_logs() {
        let mut raw = String::from("\x1b[31mERROR: timeout\x1b[0m\n");
        raw.push_str("[2026-11-20 10:00:01] ERROR: socket busy\n");
        raw.push_str("[2026-11-20 10:00:02] ERROR: socket busy\n");
        raw.push_str("[2026-11-20 10:00:03] ERROR: socket busy\n");
        for i in 0..12 {
            raw.push_str(&format!("    at frame_{i} (file.js:{i}:1)\n"));
        }
        let cleaned = denoise_markdown(&raw, true);
        assert!(!cleaned.contains("\x1b[31m"));
        assert!(cleaned.contains("[repeated 3 times]"));
        assert!(cleaned.contains("stack frames omitted"));
    }

    #[test]
    fn simhash_detects_near_duplicates() {
        let a = "Observed checkout retries succeeding on attempt 2 during WAL load test at 10:01.";
        let b = "Observed checkout retries succeeding on attempt 2 during WAL load test at 10:05.";
        let c = "PostgreSQL replication lag alert triggered on analytics read replica cluster.";
        let (ha, _) = compute_simhash64(a);
        let (hb, _) = compute_simhash64(b);
        let (hc, _) = compute_simhash64(c);
        assert!(simhash_hamming(ha, hb) <= 6);
        assert!(simhash_hamming(ha, hc) > 8);
    }
}
