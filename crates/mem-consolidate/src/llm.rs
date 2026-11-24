//! Optional OpenAI-compatible cold-path LLM distiller for episodic consolidation.

use std::sync::Arc;
use std::time::Duration;

use mem_core::{MemError, MemoryTier, Result};
use mem_search::{post_json_http_or_curl, ConsolidateLlmConfig, EntityTriple, ProviderBackend};
use serde::Deserialize;
use serde_json::json;

use crate::extractor::{slugify, DistilledCandidate, SourceEpisode};

const CONSOLIDATE_LLM_TIMEOUT: Duration = Duration::from_secs(30);

/// Custom chat completion transport callback `(url, bearer_api_key, json_body) -> response_json`.
pub type LlmChatTransport = Arc<dyn Fn(&str, Option<&str>, &str) -> Result<String> + Send + Sync>;

#[derive(Debug, Deserialize)]
struct LlmConsolidationOutput {
    #[serde(default)]
    semantic_cards: Vec<LlmCardSpec>,
    #[serde(default)]
    procedural_cards: Vec<LlmCardSpec>,
}

#[derive(Debug, Deserialize)]
struct LlmCardSpec {
    title: String,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    importance: Option<f32>,
    #[serde(default)]
    facts: Vec<String>,
    #[serde(default)]
    steps: Vec<String>,
}

pub(crate) fn consolidate_with_llm(
    cfg: &ConsolidateLlmConfig,
    transport: Option<&LlmChatTransport>,
    episodes: &[SourceEpisode],
) -> Result<Vec<DistilledCandidate>> {
    if transport.is_none() {
        match cfg.backend {
            ProviderBackend::Disabled => {
                return Err(MemError::Storage(
                    "consolidate LLM backend is disabled".to_string(),
                ));
            }
            ProviderBackend::OpenAi
                if cfg
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty() =>
            {
                return Err(MemError::Storage(
                    "OPENAI_API_KEY is not configured for LLM consolidation".to_string(),
                ));
            }
            _ => {}
        }
    }

    let endpoint = if cfg.base_url.trim().is_empty() {
        "http://127.0.0.1:11434/v1/chat/completions".to_string()
    } else {
        format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'))
    };

    let mut prompt_episodes = String::new();
    for ep in episodes {
        prompt_episodes.push_str(&format!(
            "--- SOURCE: {} (importance: {:.2}, tags: {}) ---\n{}\n\n",
            ep.path,
            ep.importance,
            ep.tags.join(", "),
            ep.raw_text.trim()
        ));
    }

    let system_prompt = concat!(
        "You are Engram's cold-path memory consolidation engine. ",
        "Given episodic session logs, deduplicate overlapping observations and distill durable ",
        "semantic facts and procedural SOPs. Respond strictly with valid JSON matching: ",
        "{\"semantic_cards\":[{\"title\":string,\"slug\":string?,\"aliases\":[string],\"tags\":[string],",
        "\"entities\":[\"Subject -> relation -> Object\"],\"importance\":number,\"facts\":[string]}],",
        "\"procedural_cards\":[{\"title\":string,\"slug\":string?,\"aliases\":[string],\"tags\":[string],",
        "\"entities\":[string],\"importance\":number,\"steps\":[string]}]}. ",
        "Do not invent facts absent from the logs."
    );

    let payload = json!({
        "model": if cfg.model.is_empty() { "qwen2.5:7b" } else { cfg.model.as_str() },
        "temperature": 0.1,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": prompt_episodes }
        ]
    });

    let body = serde_json::to_string(&payload)?;
    let api_key = cfg.api_key.as_deref().filter(|k| !k.trim().is_empty());

    let raw_response = match transport {
        Some(cb) => cb(&endpoint, api_key, &body)?,
        None => post_json_http_or_curl(&endpoint, api_key, &body, CONSOLIDATE_LLM_TIMEOUT)?,
    };

    let outer: serde_json::Value = serde_json::from_str(&raw_response).map_err(|e| {
        MemError::Serialization(format!("invalid LLM consolidation HTTP JSON response: {e}"))
    })?;
    let content = outer
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .map(unwrap_json_fence)
        .ok_or_else(|| {
            MemError::Serialization("LLM response missing choices[0].message.content".to_string())
        })?;

    let parsed: LlmConsolidationOutput = serde_json::from_str(content).map_err(|e| {
        MemError::Serialization(format!("LLM output is not valid consolidation JSON: {e}"))
    })?;

    let all_sources: Vec<_> = episodes.iter().map(|e| e.path.clone()).collect();
    let mut cards = Vec::new();

    for spec in parsed.semantic_cards {
        if let Some(card) = build_candidate_from_spec(MemoryTier::Semantic, spec, &all_sources) {
            cards.push(card);
        }
    }
    for spec in parsed.procedural_cards {
        if let Some(card) = build_candidate_from_spec(MemoryTier::Procedural, spec, &all_sources) {
            cards.push(card);
        }
    }

    if cards.is_empty() {
        return Err(MemError::Serialization(
            "LLM consolidation returned zero non-empty cards".to_string(),
        ));
    }

    Ok(cards)
}

fn build_candidate_from_spec(
    tier: MemoryTier,
    spec: LlmCardSpec,
    all_sources: &[mem_core::VirtualPath],
) -> Option<DistilledCandidate> {
    let title = spec.title.trim().to_string();
    if title.is_empty() {
        return None;
    }

    let statements: Vec<String> = if tier == MemoryTier::Procedural {
        spec.steps
            .into_iter()
            .chain(spec.facts)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        spec.facts
            .into_iter()
            .chain(spec.steps)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    if statements.is_empty() {
        return None;
    }

    let slug = spec
        .slug
        .as_deref()
        .map(slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| slugify(&title));

    let mut tags = spec.tags;
    if !tags.iter().any(|t| t == "consolidated") {
        tags.push("consolidated".to_string());
    }
    if !tags.iter().any(|t| t == tier.as_str()) {
        tags.push(tier.as_str().to_string());
    }

    let entities = spec
        .entities
        .into_iter()
        .filter_map(|raw| parse_entity_triple(&raw))
        .collect();

    let default_imp = if tier == MemoryTier::Procedural {
        0.85
    } else {
        0.80
    };

    Some(DistilledCandidate {
        tier,
        title,
        slug,
        aliases: spec.aliases,
        tags,
        entities,
        statements,
        source_paths: all_sources.to_vec(),
        importance: spec.importance.unwrap_or(default_imp).clamp(0.1, 1.0),
    })
}

fn parse_entity_triple(raw: &str) -> Option<EntityTriple> {
    let parts: Vec<&str> = raw.split("->").map(str::trim).collect();
    if parts.len() == 3 && parts.iter().all(|p| !p.is_empty()) {
        Some(EntityTriple {
            source: parts[0].to_string(),
            relation: parts[1].to_string(),
            target: parts[2].to_string(),
        })
    } else {
        None
    }
}

fn unwrap_json_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
    {
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
    }
    trimmed
}
