# Contributing to Engram (`mem`)

Thank you for contributing to **Engram**! Please review the architectural invariants and engineering workflow below before opening a Pull Request.

---

## 1. Core Architectural Invariants

Every change in the workspace must strictly uphold the five invariants defined in [AGENTS.md](./AGENTS.md):

1. **CAS Objects Are Strictly Immutable**: Never `UPDATE` rows in `blobs`, `tree_objects`, `trees`, or `commits`. All mutations create new content-addressed objects and advance `refs` or `workspace_entries`.
2. **Zero Cross-Branch Memory Leakage**: Every read, list, search, graph expansion, and `/core` compilation must be scoped to an explicit `Revision`. Never leak uncommitted, unmerged, or rewound nodes across branches.
3. **Indices Bind to `blob_id`, Not `path`**: All FTS5 chunks, embeddings, and `blob_links` attach to immutable `blob_id` digests so zero-copy branches require zero duplicate indexing.
4. **Zero LLM Calls on the Hot Path**: `mem read`, `mem write`, `mem search`, and `mem compile` must stay pure local deterministic operations (millisecond latency).
5. **Single-File Zero-External-Dependency Core**: Core workflows must work out-of-the-box with a single `.db` SQLite file and no mandatory external databases.

---

## 2. Workspace Crate Boundaries

| Crate | Responsibility |
| :--- | :--- |
| `crates/mem-core` | Pure domain types (`VirtualPath`, `MemoryNode`, BLAKE3 IDs, VCS types, `MemError`). No SQLite or network I/O. |
| `crates/mem-store` | SQLite WAL schema initialization, BLAKE3+zstd CAS, Merkle tree / commit / ref / workspace storage primitives. |
| `crates/mem-vcs` | Version control kernel (`MemVcs`): overlay workspace, Merkle tree builder, pruned diff, LCA 3-way merge, rewind, GC. |
| `crates/mem-search` | Obsidian Wiki parser, breadcrumb chunker, pluggable Ollama/OpenAI embedder, Graph-RRF hybrid search, `/core` compiler, Vault importer. |
| `crates/mem-cli` | `mem` binary: CLI commands, `onboard` TUI wizard, `config`, `doctor`, `mcp` server & client auto-wiring. |

---

## 3. Local Development & Testing

```bash
# Format code according to rustfmt.toml
cargo fmt --all

# Run the entire workspace test suite (works 100% offline)
cargo test --offline --all-targets

# Verify end-to-end showcase workflow
./examples/multi_agent_branch_merge.sh
```

### Pull Request Checklist
- [ ] Code compiles cleanly with zero compiler or Clippy warnings.
- [ ] All unit and integration tests pass via `cargo test --offline --all-targets`.
- [ ] New behavior includes focused tests (especially branch isolation, rewind, or CAS deduplication edge cases).
- [ ] Public functions/types have concise doc comments; relevant docs in `docs/` are updated when CLI or schema changes.
