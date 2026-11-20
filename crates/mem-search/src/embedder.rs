//! Pluggable embedding providers supporting zero-model fallback, local Ollama, and OpenAI API.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use mem_core::{MemError, Result};
use serde::Deserialize;
use serde_json::json;

use crate::config::{EmbeddingProviderConfig, ProviderBackend};

/// Abstraction over local or remote text embedding providers.
/// `model_id` of a provider that produces no vectors (BM25-only search).
pub const DISABLED_MODEL: &str = "disabled";

pub trait EmbeddingProvider: Send + Sync {
    /// Human-readable identifier for this provider.
    fn provider_name(&self) -> &str;

    /// Identity of the vectors this provider produces (provider plus model). The index stores it
    /// per blob and re-embeds a blob when it changes. [`DISABLED_MODEL`] means "no vectors".
    fn model_id(&self) -> String {
        self.provider_name().to_string()
    }

    /// Embeds a batch of document chunks into unit-length or raw float32 vectors.
    ///
    /// Returns `Ok(None)` when running in Level-1 zero-model (BM25 + Wiki Graph) mode.
    fn embed_batch(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>>;

    /// Embeds a single search query string.
    ///
    /// Returns `Ok(None)` when running in Level-1 zero-model (BM25 + Wiki Graph) mode.
    fn embed_query(&self, query: &str) -> Result<Option<Vec<f32>>> {
        let Some(mut batch) = self.embed_batch(&[query.to_string()])? else {
            return Ok(None);
        };
        Ok(batch.pop())
    }
}

/// Level-1 zero-model embedder disabling vector generation in favor of pure FTS5 + Wiki Graph.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEmbedder;

impl EmbeddingProvider for NoopEmbedder {
    fn provider_name(&self) -> &str {
        "disabled"
    }

    fn embed_batch(&self, _texts: &[String]) -> Result<Option<Vec<Vec<f32>>>> {
        Ok(None)
    }
}

/// Pure-Rust deterministic feature-hashing embedder for offline vector similarity without models.
#[derive(Debug, Clone, Copy)]
pub struct DeterministicHashEmbedder {
    dim: usize,
}

impl Default for DeterministicHashEmbedder {
    fn default() -> Self {
        Self::new(256)
    }
}

impl DeterministicHashEmbedder {
    /// Creates a deterministic hash embedder projecting tokens and CJK n-grams into `dim` dimensions.
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(32) }
    }

    fn embed_single(&self, text: &str) -> Vec<f32> {
        let mut vec = vec![0.0f32; self.dim];
        let lower = text.to_lowercase();

        for token in lower
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
        {
            self.accumulate_feature(&mut vec, token.as_bytes(), 1.0);
        }

        // Add character bigrams so CJK phrases without spaces remain semantically matchable.
        let non_space: Vec<char> = lower.chars().filter(|c| !c.is_whitespace()).collect();
        for pair in non_space.windows(2) {
            if !pair[0].is_ascii_alphanumeric() || !pair[1].is_ascii_alphanumeric() {
                let mut buf = [0u8; 8];
                let n0 = pair[0].encode_utf8(&mut buf[..4]).len();
                let n1 = pair[1].encode_utf8(&mut buf[n0..]).len();
                self.accumulate_feature(&mut vec, &buf[..n0 + n1], 0.75);
            }
        }

        l2_normalize(&mut vec);
        vec
    }

    fn accumulate_feature(&self, vec: &mut [f32], feature: &[u8], weight: f32) {
        let digest = blake3::hash(feature);
        let bytes = digest.as_bytes();
        let idx1 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize % self.dim;
        let sign1 = if bytes[4] & 1 == 0 { 1.0 } else { -1.0 };
        vec[idx1] += sign1 * weight;
    }
}

impl EmbeddingProvider for DeterministicHashEmbedder {
    fn provider_name(&self) -> &str {
        "deterministic-hash"
    }

    fn model_id(&self) -> String {
        format!("deterministic-hash:{}", self.dim)
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>> {
        if texts.is_empty() {
            return Ok(Some(Vec::new()));
        }
        Ok(Some(texts.iter().map(|t| self.embed_single(t)).collect()))
    }
}

/// Custom HTTP POST transport callback `(url, bearer_api_key, json_body) -> response_json_body`.
pub type HttpPostTransport = Arc<dyn Fn(&str, Option<&str>, &str) -> Result<String> + Send + Sync>;

/// OpenAI-compatible `/v1/embeddings` client supporting local Ollama and production OpenAI API.
#[derive(Clone)]
pub struct OpenAiCompatibleEmbedder {
    config: EmbeddingProviderConfig,
    timeout: Duration,
    transport: Option<HttpPostTransport>,
}

impl std::fmt::Debug for OpenAiCompatibleEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleEmbedder")
            .field("config", &self.config)
            .field("timeout", &self.timeout)
            .field("has_custom_transport", &self.transport.is_some())
            .finish()
    }
}

impl OpenAiCompatibleEmbedder {
    /// Creates an embedder using `config` with direct HTTP/1.1 transport over `TcpStream`.
    pub fn new(config: EmbeddingProviderConfig) -> Self {
        Self {
            config,
            timeout: Duration::from_secs(30),
            transport: None,
        }
    }

    /// Creates an embedder with an explicit transport callback (useful for TLS clients or tests).
    pub fn with_transport(config: EmbeddingProviderConfig, transport: HttpPostTransport) -> Self {
        Self {
            config,
            timeout: Duration::from_secs(30),
            transport: Some(transport),
        }
    }

    /// Sets the network socket timeout for HTTP requests.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Returns the underlying [`EmbeddingProviderConfig`].
    pub fn config(&self) -> &EmbeddingProviderConfig {
        &self.config
    }

    /// Returns the full `/embeddings` endpoint URL.
    pub fn embeddings_url(&self) -> String {
        format!("{}/embeddings", self.config.base_url.trim_end_matches('/'))
    }

    /// Builds the OpenAI-compatible JSON request body for `texts`.
    pub fn build_embeddings_request_json(&self, texts: &[String]) -> serde_json::Value {
        let mut payload = json!({
            "model": self.config.model,
            "input": texts,
        });
        if let Some(dim) = self.config.dimensions {
            payload["dimensions"] = json!(dim);
        }
        payload
    }
}

#[derive(Deserialize)]
struct OpenAiEmbeddingsResponse {
    data: Vec<OpenAiEmbeddingDatum>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingDatum {
    index: usize,
    embedding: Vec<f32>,
}

impl EmbeddingProvider for OpenAiCompatibleEmbedder {
    fn provider_name(&self) -> &str {
        match self.config.backend {
            ProviderBackend::Disabled => "disabled",
            ProviderBackend::Ollama => "ollama",
            ProviderBackend::OpenAi => "openai",
        }
    }

    fn model_id(&self) -> String {
        match (self.config.backend, self.config.dimensions) {
            (ProviderBackend::Disabled, _) => DISABLED_MODEL.to_string(),
            (_, Some(d)) => format!("{}:{}:{d}", self.provider_name(), self.config.model),
            (_, None) => format!("{}:{}", self.provider_name(), self.config.model),
        }
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>> {
        if self.config.backend == ProviderBackend::Disabled {
            return Ok(None);
        }
        if texts.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let url = self.embeddings_url();
        let body = serde_json::to_string(&self.build_embeddings_request_json(texts))?;
        let api_key = self
            .config
            .api_key
            .as_deref()
            .filter(|k| !k.trim().is_empty());

        let raw_response = match &self.transport {
            Some(transport) => transport(&url, api_key, &body)?,
            None => http_post_json(&url, api_key, &body, self.timeout)?,
        };

        let mut parsed: OpenAiEmbeddingsResponse =
            serde_json::from_str(&raw_response).map_err(|e| {
                MemError::Serialization(format!(
                    "invalid OpenAI-compatible embedding response: {e}"
                ))
            })?;
        parsed.data.sort_by_key(|d| d.index);

        if parsed.data.len() != texts.len() {
            return Err(MemError::Storage(format!(
                "embedding count mismatch: sent {}, received {}",
                texts.len(),
                parsed.data.len()
            )));
        }

        Ok(Some(parsed.data.into_iter().map(|d| d.embedding).collect()))
    }
}

pub(crate) fn post_json_http_or_curl(
    url: &str,
    api_key: Option<&str>,
    body: &str,
    timeout: Duration,
) -> Result<String> {
    http_post_json(url, api_key, body, timeout)
}

fn http_post_json(
    url: &str,
    api_key: Option<&str>,
    body: &str,
    timeout: Duration,
) -> Result<String> {
    if url.starts_with("https://") {
        return https_post_via_curl(url, api_key, body, timeout);
    }

    let without_scheme = url
        .strip_prefix("http://")
        .ok_or_else(|| MemError::Storage(format!("unsupported embedding URL scheme: {url}")))?;
    let (host_port, path) = match without_scheme.find('/') {
        Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
        None => (without_scheme, "/"),
    };
    let addr_target = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:80")
    };

    let sock_addr = addr_target
        .to_socket_addrs()
        .map_err(|e| MemError::Storage(format!("failed to resolve '{addr_target}': {e}")))?
        .next()
        .ok_or_else(|| {
            MemError::Storage(format!("no socket addresses resolved for '{addr_target}'"))
        })?;

    let mut stream = TcpStream::connect_timeout(&sock_addr, timeout)
        .map_err(|e| MemError::Storage(format!("failed to connect to '{addr_target}': {e}")))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| MemError::Storage(format!("failed to set read timeout: {e}")))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| MemError::Storage(format!("failed to set write timeout: {e}")))?;

    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(key) = api_key {
        request.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);

    stream
        .write_all(request.as_bytes())
        .map_err(|e| MemError::Storage(format!("failed to write HTTP request: {e}")))?;
    stream
        .flush()
        .map_err(|e| MemError::Storage(format!("failed to flush HTTP request: {e}")))?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| MemError::Storage(format!("failed to read HTTP status line: {e}")))?;

    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| MemError::Storage(format!("malformed HTTP status line: {status_line:?}")))?;

    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    loop {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .map_err(|e| MemError::Storage(format!("failed to read HTTP header: {e}")))?;
        let trimmed = header.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, val)) = trimmed.split_once(':') {
            let lname = name.trim().to_ascii_lowercase();
            let lval = val.trim();
            if lname == "content-length" {
                content_length = lval.parse::<usize>().ok();
            } else if lname == "transfer-encoding" && lval.to_ascii_lowercase().contains("chunked")
            {
                is_chunked = true;
            }
        }
    }

    let raw_body = if is_chunked {
        read_chunked_body(&mut reader)?
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| MemError::Storage(format!("failed to read HTTP body: {e}")))?;
        buf
    } else {
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .map_err(|e| MemError::Storage(format!("failed to read HTTP body to EOF: {e}")))?;
        buf
    };

    let body_str = String::from_utf8(raw_body)?;
    if !(200..300).contains(&status_code) {
        return Err(MemError::Storage(format!(
            "HTTP {status_code} from {url}: {body_str}"
        )));
    }
    Ok(body_str)
}

fn https_post_via_curl(
    url: &str,
    api_key: Option<&str>,
    body: &str,
    timeout: Duration,
) -> Result<String> {
    curl_post(url, api_key, body, timeout)
}

/// curl arguments and stdin. Headers and body go through `--config -` on stdin, so the API key
/// never appears in the process arguments (readable by any local user through `ps`).
fn curl_invocation(
    url: &str,
    api_key: Option<&str>,
    body: &str,
    timeout: Duration,
) -> (Vec<String>, String) {
    let quote = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
    let mut config = format!("header = {}\n", quote("Content-Type: application/json"));
    if let Some(key) = api_key {
        config.push_str(&format!(
            "header = {}\n",
            quote(&format!("Authorization: Bearer {key}"))
        ));
    }
    config.push_str(&format!("data-binary = {}\n", quote(body)));
    let args = [
        "-sS",
        "-X",
        "POST",
        "--max-time",
        &timeout.as_secs().max(1).to_string(),
        "--config",
        "-",
        "-w",
        "\n%{http_code}",
        url,
    ]
    .map(String::from)
    .to_vec();
    (args, config)
}

fn curl_post(url: &str, api_key: Option<&str>, body: &str, timeout: Duration) -> Result<String> {
    let (args, config) = curl_invocation(url, api_key, body, timeout);
    let mut child = Command::new("curl")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            MemError::Storage(format!(
                "failed to spawn system 'curl' for HTTPS request to '{url}': {e}"
            ))
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(config.as_bytes()).map_err(|e| {
            MemError::Storage(format!("failed to pipe request config to curl: {e}"))
        })?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| MemError::Storage(format!("failed waiting for curl HTTPS response: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(MemError::Storage(format!(
            "curl HTTPS request failed (exit {:?}): {}",
            output.status.code(),
            stderr.trim()
        )));
    }

    let full_out = String::from_utf8(output.stdout)?;
    let (resp_body, code_str) = full_out.rsplit_once('\n').unwrap_or(("", full_out.trim()));
    let status_code = code_str.trim().parse::<u16>().map_err(|e| {
        MemError::Storage(format!(
            "failed to parse HTTP status code from curl output '{code_str}': {e}"
        ))
    })?;

    if !(200..300).contains(&status_code) {
        return Err(MemError::Storage(format!(
            "HTTP {status_code} from {url}: {resp_body}"
        )));
    }

    Ok(resp_body.to_string())
}

fn read_chunked_body<R: BufRead>(reader: &mut R) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        reader
            .read_line(&mut size_line)
            .map_err(|e| MemError::Storage(format!("failed to read HTTP chunk size: {e}")))?;
        let hex_size = size_line.trim().split(';').next().unwrap_or("0");
        let chunk_len = usize::from_str_radix(hex_size, 16).map_err(|e| {
            MemError::Storage(format!("invalid HTTP chunk length '{hex_size}': {e}"))
        })?;
        if chunk_len == 0 {
            break;
        }
        let mut chunk = vec![0u8; chunk_len];
        reader
            .read_exact(&mut chunk)
            .map_err(|e| MemError::Storage(format!("failed to read HTTP chunk data: {e}")))?;
        out.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        let _ = reader.read_exact(&mut crlf);
    }
    Ok(out)
}

/// Encodes a `f32` embedding slice as compact little-endian bytes for SQLite BLOB storage.
pub fn encode_f32_vec(v: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for &val in v {
        bytes.extend_from_slice(&val.to_le_bytes());
    }
    bytes
}

/// Decodes little-endian bytes from SQLite `chunk_index.embedding` back into `Vec<f32>`.
pub fn decode_f32_vec(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return Err(MemError::Storage(format!(
            "invalid float32 embedding byte length: {}",
            bytes.len()
        )));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.as_chunks::<4>().0 {
        out.push(f32::from_le_bytes(*chunk));
    }
    Ok(out)
}

/// Computes cosine similarity between `a` and raw little-endian `f32` bytes without heap allocation.
pub fn cosine_similarity_le_bytes(a: &[f32], le_bytes: &[u8]) -> Result<f32> {
    if !le_bytes.len().is_multiple_of(4) {
        return Err(MemError::Storage(format!(
            "invalid float32 embedding byte length: {}",
            le_bytes.len()
        )));
    }
    if a.is_empty() || le_bytes.len() != a.len() * 4 {
        return Ok(0.0);
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (&x, chunk) in a.iter().zip(le_bytes.as_chunks::<4>().0) {
        let y = f32::from_le_bytes(*chunk);
        let xf = f64::from(x);
        let yf = f64::from(y);
        dot += xf * yf;
        norm_a += xf * xf;
        norm_b += yf * yf;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return Ok(0.0);
    }
    Ok((dot / (norm_a.sqrt() * norm_b.sqrt())) as f32)
}

/// Computes cosine similarity between two float32 vectors (`0.0` for empty/mismatched/zero vectors).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let xf = f64::from(x);
        let yf = f64::from(y);
        dot += xf * yf;
        norm_a += xf * xf;
        norm_b += yf * yf;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a.sqrt() * norm_b.sqrt())) as f32
}

fn l2_normalize(vec: &mut [f32]) {
    let norm_sq: f64 = vec.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
    if norm_sq > 0.0 {
        let inv = (1.0 / norm_sq.sqrt()) as f32;
        for v in vec.iter_mut() {
            *v *= inv;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::{curl_invocation, curl_post, http_post_json};

    /// One-shot HTTP server: returns the raw request and answers with `status`.
    fn serve_once(status: &str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/x", listener.local_addr().unwrap());
        let status = status.to_string();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut head, mut len) = (String::new(), 0usize);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let resp =
                format!("HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}");
            stream.write_all(resp.as_bytes()).unwrap();
            head + &String::from_utf8(body).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn the_api_key_never_appears_in_curl_arguments() {
        let (args, stdin) = curl_invocation(
            "https://h/v1",
            Some("sk-SECRET\"x"),
            "{}",
            Duration::from_secs(5),
        );
        assert!(args.iter().all(|a| !a.contains("SECRET")), "{args:?}");
        assert!(
            stdin.contains(r#"header = "Authorization: Bearer sk-SECRET\"x""#),
            "{stdin}"
        );
    }

    #[test]
    fn curl_sends_the_config_headers_and_the_exact_body() {
        let (url, server) = serve_once("200 OK");
        let body = r#"{"a":"quote \" and backslash \\ and 中文"}"#;
        assert_eq!(
            curl_post(&url, Some("sk-test"), body, Duration::from_secs(10)).unwrap(),
            "{}"
        );
        let req = server.join().unwrap();
        assert!(req.contains("Authorization: Bearer sk-test"), "{req}");
        assert!(req.ends_with(body), "{req}");
    }

    #[test]
    fn http_errors_name_the_endpoint_url() {
        let (url, server) = serve_once("500 Internal Server Error");
        let err = http_post_json(&url, None, "{}", Duration::from_secs(5))
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert!(
            err.contains(&format!("HTTP 500 from {url}")) && !err.contains("embedding endpoint"),
            "{err}"
        );
    }
}
