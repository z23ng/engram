# Changelog

All notable changes to **Engram (`mem`)** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- **`mem-consolidate`**: Cold-path sleep consolidation engine (`ConsolidationEngine`) scanning `/episodic/` logs inside isolated `consolidate/*` branches, deduplicating (exact + Jaccard) and classifying observations into `/semantic/consolidated/*` cards and `/procedural/consolidated/*` SOPs, weaving active `[[Wikilinks]]`, optionally refining via OpenAI-compatible/Ollama LLM with deterministic fallback, archiving/decaying processed episodes, and auto-merging back to source branches.
- **`mem-daemon`**: Async Tokio + Axum HTTP REST (`/v1/*`), filtered Server-Sent Events (`/v1/events`), Streamable HTTP MCP (`POST /mcp`), and HTTP+SSE MCP (`GET /mcp/sse` + `POST /mcp/messages`) server with Bearer token auth, background consolidation timer, and 12 MCP tools (adding `mem_consolidate`).
- **`mem-fuse`**: LayerFS-inspired POSIX FUSE virtual filesystem adapter (`MemoryPosixFs` + Linux `fuser 0.19` pure-Rust mount) exposing branches read-write or historical commits read-only (`mem mount` / `mem-fuse`), automatically indexing written Markdown notes back into FTS5 + Wiki graph.
- **`bindings/python` (`engram`) & `bindings/typescript` (`@engram-memory/sdk`)**:
  - Native PyO3 Python bindings (`engram.open`, `engram.open_in_memory`) enabling 5-line embedded usage plus zero-dependency `EngramClient` HTTP/SSE client.
  - Zero-dependency TypeScript SDK (`EngramClient` with `streamEvents()` SSE generator + `LocalEngram`).
- **`mem-vcs` & `mem-cli`**: Added chronological append-only auto-merge for `/episodic/` paths under `MergeStrategy::Auto`, plus `mem consolidate`, `mem daemon`, `mem mount` CLI subcommands, updated `Dockerfile`/`docker-compose.yml`/`Makefile`, and CI SDK tests.

## [0.1.0] - 2026-11-19

### Added
- **`mem-core`**: Strongly-typed `VirtualPath` (traversal-safe `/core`, `/episodic`, `/semantic`, `/procedural`), `BlobId`/`TreeId`/`CommitId` (BLAKE3), canonical `MemoryMetadata`, and VCS domain types.
- **`mem-store`**: Single-file SQLite WAL persistence with BLAKE3 + zstd content-addressed blob deduplication (`blobs`), Merkle tree nodes (`tree_objects`, `trees`), immutable commit DAG (`commits`, `refs`), per-branch staging overlay (`workspace_entries`), and content-addressed search/graph tables (`chunk_index`, `fts_chunks`, `blob_links`).
- **`mem-vcs`**: `MemVcs` engine implementing `init`, `write`, `delete`, `read`, `list`, `commit`, zero-copy `create_branch` (`<1ms` at 10,000 nodes), atomic `rewind`, Merkle subtree-pruned `diff`, BFS LCA 3-way `merge` (`auto`/`ours`/`theirs`), and mark-and-sweep `gc` (including `fts_chunks` orphan cleanup).
- **`mem-search`**:
  - Obsidian Wiki parser (`wiki.rs`) supporting YAML Frontmatter (`title`, `aliases`, `tags`, `entities`, `importance`), `[[Wikilinks]]`, inline `#tags`, and heading-breadcrumb chunking.
  - Branch-isolated 4-way Graph-RRF hybrid search (`hybrid.rs`) with multi-column weighted FTS5 BM25 (including CJK bigram support), zero-allocation little-endian cosine vector scoring, and 1-hop `[[Wikilink]]` outlink/backlink/entity graph expansion.
  - Pluggable OpenAI-compatible embedding provider (`embedder.rs` & `config.rs`) supporting local Ollama (`bge-m3` / `qwen2.5:7b`), production OpenAI (`text-embedding-3-small` / `gpt-6-luna`), and Level-1 zero-model fallback (`NoopEmbedder`).
  - `<5ms` deterministic `/core` hot compiler (`compiler.rs`), collaborative Obsidian Vault bridge (`vault.rs`: `init_vault_template`, multi-tier `sync_vault`, snapshot `export_vault`, and single-tier `import_vault`, automatically skipping `.obsidian/`, `.git/`, and `_templates/`).
  - Collaborative natural-language ingest weaver (`ingest.rs`): `IngestRole::{Pm, Dev, Bot}` role-based VFS routing, `draft/{role}-inbox` branch isolation, active-branch `[[Wikilink]]` weaving, and optional OpenAI-compatible (`gpt-6-luna`) or Ollama (`qwen2.5:7b`) structuring with deterministic local fallback.
- **`mem-cli`**:
  - Unified `mem` binary with Git-like VCS/search commands, `mem ingest`, `mem vault init-template|import|sync|export`, `mem doctor` diagnostics, and stdio JSON-RPC 2.0 MCP server (`mem mcp`) exposing 11 tools (including `mem_ingest`).
  - Modern Clack-style interactive onboarding wizard (`mem onboard`), persistent config (`~/.ai-memory/config.json`), 1-command profile switching (`mem config use ollama|openai|lite`), zero-touch MCP client wiring (`mem mcp install`), and shell completions (`mem completions`).
- **Installer, Examples & CI/CD**:
  - One-line installer (`install.sh`), `Dockerfile` + `docker-compose.yml`, curated `examples/obsidian-vault/` + `examples/multi_agent_branch_merge.sh`, complete user guide (`docs/USER_GUIDE.md`), and GitHub Actions CI + multi-platform Release workflows.
