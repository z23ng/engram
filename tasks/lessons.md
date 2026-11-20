# Lessons

## [Rust][cargo-TLS][crates-io-index] 2026-11-19 cargo fetch fails with SSL unexpected eof while curl succeeds
### Failure Mode
- Adding new deps (axum 0.8.9, getrandom 0.4.3, tower 0.5.3) to `Cargo.toml` forces an index update.
- `cargo fetch` fails, inside and outside the sandbox: `[35] SSL connect error (TLS connect error: error:0A000126:SSL routines::unexpected eof while reading)` on `index.crates.io/config.json`.
- At the same time `curl https://index.crates.io/config.json` returns 200. cargo uses vendored libcurl 8.21.0 + OpenSSL 3.6.3; system curl uses OpenSSL 3.0.13.
- No effect: `CARGO_HTTP_MULTIPLEXING=false`, `CARGO_HTTP_SSL_VERSION=tlsv1.2`, `OPENSSL_CONF` with `Groups = X25519:P-256:P-384`, `CARGO_NET_RETRY=6`.
- `cargo fetch --offline` fails: `no matching package named serde_path_to_error` (axum 0.8.9 transitive dep not in local cache).
- Root cause (confirmed): `~/.gitconfig` sets `http.proxy`/`https.proxy = http://172.24.0.1:7890`. cargo falls back to git's `http.proxy`; curl does not read git config and connects directly. `CARGO_HTTP_PROXY="" cargo fetch` succeeds. A mirror (rsproxy.cn) fails the same way, because it also goes through the proxy.
### Rules
1. Before adding a dependency, run `ls ~/.cargo/registry/cache/*/ | grep <crate>` to see if it is cached; if the closure is not cached, check `cargo fetch` works before writing code.
2. On cargo `SSL connect error ... unexpected eof`, first run `git config --show-origin --get-regexp '^(http|https)\.'`; if a proxy is set, retry with `CARGO_HTTP_PROXY=""` or a local `.cargo/config.toml` with `[http] proxy = ""`.
3. Do not retry the same cargo network command more than once without a changed hypothesis.
### Relations
- Cargo.toml (workspace.dependencies), crates/mem-server/Cargo.toml

## [Rust][Fail-Open-Gate][mem-server-mcp] 2026-11-19 MCP permission gate forwarded unmapped tools and unknown methods
### Failure Mode
- `crates/mem-server/src/mcp.rs` computed `action = tool_action(&tool)` and checked the role only when `action.is_some()`; unmapped tool names and non-tool methods were forwarded to `handle_jsonrpc_request`.
- Automated security review flagged it (MEDIUM, fail-open authorization). Reproduced with an admin `tools/call mem_future_tool`: response was `unknown tool: mem_future_tool` from the dispatcher instead of a server-side `forbidden`.
- Not exploitable at that commit (all 11 dispatcher tools were mapped), but any new tool added to `mem-mcp` without a mapping would have been callable by a `bot` key.
### Rules
1. Authorization gates deny by default: unknown resource / tool / method -> reject before reaching the handler.
2. Accept only an explicit method allowlist at the transport boundary (`initialize`, `ping`, `tools/list`, `tools/call`, `notifications/*`).
3. Keep a unit test that every entry of `mcp_tool_definitions()` has a `tool_action` mapping.
### Relations
- crates/mem-server/src/mcp.rs, crates/mem-server/tests/mcp_http.rs

## [Rust][dav-server][webdav-branch-url] 2026-11-19 dav-server rejects percent-encoded slash in a path segment
### Failure Mode
- Spec placed branch names with `/` URL-encoded in one segment: `/p/alpha/dav/draft%2Fpm-inbox/semantic/d.md`.
- dav-server 0.12.0 `normalize_path` (src/davpath.rs) decodes each segment and returns `ParseError::InvalidPath` when a decoded segment contains `/`; the PUT returned `400 Bad Request` with an empty body and nothing was written.
- Rewriting the URI before dav-server is not an option: PROPFIND hrefs would then differ from the URL the client requested.
### Rules
1. In WebDAV URLs, write `/` in branch names as `~` (git also forbids `~` in ref names); map `~` -> `/` in the handler and return 400 with a hint when the segment decodes to `/`.
2. Before designing URL schemes on top of a library router, read its path normalization code for encoded-separator handling.
### Relations
- crates/mem-server/src/dav.rs, crates/mem-server/tests/webdav.rs (branches_are_isolated_over_webdav)

## [Web][Vite-CSP][web-dist] 2026-11-19 Vite inlined small fonts as data: URIs that the server CSP blocks
### Failure Mode
- `npm run build` with default `build.assetsInlineLimit` (4 KiB) turned small `.woff2` subsets into `data:font/woff2;base64,...` inside the CSS.
- The console page sends `font-src 'self'`; Chromium logged `Loading the font 'data:font/woff2;base64,...' violates the following Content Security Policy directive: "font-src 'self'"`. Unit tests and the Rust routing test passed; only a real browser run showed it.
### Rules
1. Keep `build.assetsInlineLimit: 0` in `web/vite.config.ts` while the CSP forbids `data:` for fonts and scripts; do not widen the CSP instead.
2. After any change to `web/` build config or `crates/mem-server/src/web.rs` CSP, load the console in a browser and check the console for CSP errors (`grep -c 'data:font' web/dist/assets/*.css` must print 0).
### Relations
- web/vite.config.ts, crates/mem-server/src/web.rs (CSP), crates/mem-server/tests/web_ui.rs

## [Ollama][fake-IP-DNS][model-pull] 2026-11-20 ollama pull refuses the blob redirect because DNS returns a 198.18.x fake IP
1. Failure Mode
   - `ollama pull bge-m3` (v0.40.16, WSL2 behind a transparent proxy in fake-IP mode) fails with `redirect target not allowed: <id>.r2.cloudflarestorage.com resolves to non-public 198.18.4.8`.
   - Setting `HTTPS_PROXY=http://172.24.0.1:7890` on `ollama serve` fails earlier: `pull model manifest: Get "https://registry.ollama.ai/v2/library/bge-m3/manifests/latest": EOF` (same proxy as the cargo-TLS entry).
2. Rules
   1. Do not route Ollama through the HTTP proxy; direct connections work through the transparent proxy.
   2. Fetch models with curl instead: GET `https://registry.ollama.ai/v2/library/<name>/manifests/<tag>` with `Accept: application/vnd.docker.distribution.manifest.v2+json`, then each `config` and `layers` digest from `.../blobs/<digest>` (`curl -L` follows the redirect), verify sha256, store as `~/.ollama/models/blobs/sha256-<hex>`, and write the manifest last to `~/.ollama/models/manifests/registry.ollama.ai/library/<name>/<tag>`.
   3. Confirm with `ollama list` and one real `/api/embed` or chat call.
3. Relations
   - Session script: scratchpad `ollama_fetch.py` (not in the repo).

## [Ollama][context-truncation][consolidate] 2026-11-20 Default 4096-token context truncated a 35,882-token prompt and the model looped
1. Failure Mode
   - First real S6 run on kepay-v1 failed after 24 s: `HTTP 500: {"error":{"message":"prediction aborted, token repeat limit reached"}}`, reported as "embedding endpoint returned HTTP 500" (wording of the shared HTTP helper).
   - Ollama log: `msg="truncating input prompt" limit=2050 prompt=35882 keep=4 new=2050`. `tasks/todo.md` (one large episodic file) was sent whole, beyond the batch budget, and the server context was the default (`OLLAMA_CONTEXT_LENGTH:0` -> 4096).
2. Rules
   1. Budget LLM prompts in bytes including headers, and split large inputs; never send a single input above the budget.
   2. Never send a truncated copy of a note the LLM may rewrite in full.
   3. Start Ollama with `OLLAMA_CONTEXT_LENGTH=32768` for consolidation with qwen2.5:7b; check the log for `truncating input prompt` after the first run.
   4. Find the failing call in the Ollama `[GIN]` lines (endpoint and status) before trusting an error message from a shared helper.
3. Relations
   - `crates/mem-search/src/consolidate.rs`, `crates/mem-search/tests/consolidate.rs`
