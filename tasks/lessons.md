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
