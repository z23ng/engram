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
