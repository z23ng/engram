# Engram Server (feat/mem-server)

Design: docs/SERVER_DESIGN.md. Worktree: ../ai-memory.worktree/feat-mem-server.

- [x] S1 Extract `mem-mcp` crate; `mem-server` control plane (projects, keys, authorize); minimal REST; `mem serve` / `mem admin`
- [x] S1 follow-ups: clippy 1.100 fixes, admin key bootstrap, serverInfo name `engram`, CI fmt + clippy lint job
- [x] S2 MCP Streamable HTTP endpoint, role-filtered tools, audit log
- [x] S3 WebDAV vault endpoint with ~10s idle auto-commit
- [x] S3 follow-ups: startup leftover commit, image attachments under `<tier>/_attachments/`
- [x] S4a REST API for the web client (identity, projects, keys, audit, browse, changes)
- [x] S4b Embedded React SPA (Vite + TS + Tailwind + shadcn/ui + CodeMirror, rust-embed), infrastructure-console style
- [x] S4 follow-ups: merge author = key name, lazy Files chunk (entry < 500 kB), UI tests (key secret once, bot read-only, commit dialog)
- [x] Search index rebuild: per-blob embedding model tracking, `reindex_all`, `POST .../reindex` (admin), `mem admin reindex`, Search view button
- [x] Search mode visibility: per-project stderr log line, `GET .../index`, Search view badge
- [x] Enable embeddings for the local kepay-v1 trial (user-local Ollama bge-m3)
- [x] S6 background consolidation: review branches `consolidate/*`, manual + schedule triggers, runs table, console panel
- [x] S6 review fixes (4 /run-verify rounds, PASSED at a783181)
- [x] S6 follow-ups: rewrite guards (shrink > 50%, cross-tier duplicate), `consolidate --full`, Ctrl-C marks CLI runs failed
- [x] eval_trace gate: Rust test paths, cargo compile-error Red, variable write targets (outside repo)
- [ ] S5 Dockerfile (non-root, distroless), docs sync, ROADMAP 3.4
- [ ] Gate: eval_trace.py --gate, /run-verify, push branch, open MR
