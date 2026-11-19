# Engram Server (`mem serve`) Design

Status: approved 2026-11-19. Implements ROADMAP 3.4 (`mem-daemon`) as a multi-project team server.

## 1. Goal

Run Engram as a network service with many projects. API keys control access, and each key has a role. Clients connect through 4 transports:

- MCP over HTTP (coding agents and other agents).
- WebDAV (Obsidian vault sync; PM submits markdown here).
- REST API.
- Embedded web client for markdown editing.

The local single-file `mem` CLI keeps working without changes (AGENTS.md invariant 5).

## 2. Decisions

| Topic | Decision |
|---|---|
| Tenancy | One organization, many projects. No sign-up flow. An `admin` key creates projects and keys. |
| Write flow | `dev` and `pm` keys write, commit, branch, rewind, and merge on any branch, including `main`. |
| Web client | React + TypeScript + Vite + CodeMirror, built to static files and embedded in the `mem` binary with `rust-embed`. |
| Web client style | Modern infrastructure-console style (TL, 2026-11-19), in the manner of large cloud/devtools consoles: neutral grayscale with one accent color, light and dark themes, left navigation + top project/branch switcher, dense data tables, monospace for commit IDs/paths/keys, command palette (Ctrl/Cmd+K). Proposed kit: Tailwind CSS + shadcn/ui (Radix) + lucide icons. |
| WebDAV commit | Each PUT writes the branch workspace. The server commits after ~10s without a new WebDAV write on that project/branch. Commit author = key name. |
| MCP transport | Streamable HTTP, one JSON response per POST, no SSE. Reuse the existing hand-written JSON-RPC dispatcher (no `rmcp`). |
| TLS | Terminated by a reverse proxy (Caddy/nginx). |

## 3. Storage

- Control plane: `<data>/server.db` with tables `projects`, `api_keys`, `audit_log`.
- Engine: one file per project, `<data>/projects/<slug>.db`, with the unchanged `mem-store` schema.
- One database per project gives cross-project isolation by construction, and no engine schema change.
- The server is the only writer of each file. It caches one `MemSearchEngine` (with its `MemVcs`) per project. Engine calls run in `tokio::task::spawn_blocking`; `SqliteStore` serializes access with `Arc<Mutex<Connection>>`.

## 4. API keys and permissions

- Key format: `egm_` + 32 random bytes in base62. The server stores the BLAKE3 hash only and shows the plaintext once.
- Key fields: `id`, `name`, `role`, `projects` (list or `*`), `created_at`, `expires_at`, `revoked_at`, `last_used_at`. (`created_by` is deferred: in S1 only the local `mem admin` CLI creates keys, and it has no caller identity.)

| Role | Read / search / compile / log / diff | Write / delete / commit / ingest | Branch / rewind / merge | Manage projects and keys |
|---|---|---|---|---|
| `admin` | yes | yes | yes | yes |
| `dev` | yes | yes | yes | no |
| `pm` | yes | yes | yes | no |
| `bot` | yes | no | no | no |

- One function, `authorize(key, project, Action) -> Result<(), AuthError>`, guards all transports.
- MCP `tools/list` returns only the tools that the role allows.
- WebDAV `PUT` / `DELETE` / `MOVE` / `COPY` / `MKCOL` return 403 for `bot`.
- Each write appends to `audit_log`: key name, role, project, operation, resulting commit.
- Behavior change: `mem_ingest` with a `bot` key is denied (local CLI `--role bot` is unchanged).

## 5. Endpoints (crate `mem-server`: axum 0.8.9, tokio 1.54.1, tower-http 0.7.1, dav-server 0.12.0)

| Endpoint | Auth | Purpose |
|---|---|---|
| `POST /p/{project}/mcp` | `Authorization: Bearer <key>` | MCP Streamable HTTP |
| `/p/{project}/dav/{branch}/...` | HTTP Basic, password = key | WebDAV vault; folders map to `/core`, `/semantic`, `/procedural`, `/episodic` like `sync_vault` |
| `/api/v1/...` | Bearer | REST for the web client. S1: `GET .../projects/{p}/read?path=&branch=`, `POST .../projects/{p}/write` (stages the node; commits with author = key name only when `commit_message` is given) |
| `/` | Bearer kept in browser session | Embedded web client |

MCP endpoint behavior (S2):

- Key check (401) and project scope (403) run before the body is parsed. Malformed JSON returns HTTP 400 with JSON-RPC error `-32700`. Notifications return 202 with an empty body. `GET` returns 405.
- Tool permissions: Read = `mem_read`, `mem_list`, `mem_search`, `mem_compile_core`, `mem_diff`; Write = `mem_write`, `mem_commit`, `mem_ingest`; Branch = `mem_branch`, `mem_rewind`, `mem_merge`. A denied `tools/call` returns `result.isError = true` with a `forbidden` message and does not reach the engine. Deny by default: a tool without a mapping is denied for every role, and only the methods `initialize`, `ping`, `tools/list`, `tools/call`, `notifications/*` are accepted (others get `-32601`). A unit test checks that every tool in `mcp_tool_definitions()` has a mapping.
- The server overwrites `author` in `mem_commit` / `mem_ingest` with the key name.
- Audit rows: operation = tool name for successful Write/Branch tool calls, `rest.write` for REST writes; `commit_id` is taken from the tool result when present. `mem admin audit --project <slug> [--limit N]` lists them, newest first.

CLI additions: `mem serve --data <dir> --listen <addr>`, `mem admin project create|list`, `mem admin key create|list|revoke`. First start on an empty data directory prints a one-time `admin` key.

## 6. Crate changes

- New `crates/mem-mcp` (library): `dispatch_tool`, tool definitions, `handle_jsonrpc_request`, moved from `mem-cli/src/mcp.rs`. Used by stdio (`mem mcp serve`) and HTTP.
- New `crates/mem-server` (library): control plane, auth, axum router, WebDAV filesystem, embedded UI.
- `mem-cli`: adds `serve` and `admin` subcommands.
- New `web/`: React SPA sources.

## 7. Milestones

| # | Scope | Red tests first |
|---|---|---|
| S1 | Extract `mem-mcp`. `mem-server` skeleton, control-plane DB, key create/hash/verify/revoke, `authorize`, minimal REST read/write, `mem serve`, `mem admin`. | Wrong key -> 401. Revoked key -> 401. Key for A -> 403 on B. `bot` write -> 403. `dev` write then read returns content. |
| S2 | MCP over HTTP, role-filtered tool list, audit log. | `bot` `tools/list` has no `mem_write`. `bot` `tools/call mem_write` fails. Cross-project isolation via MCP. |
| S3 | WebDAV vault with idle auto-commit. | PROPFIND lists tier folders. PUT `.md` -> `mem_read` returns it. `bot` PUT -> 403. Idle commit creates one commit for many PUTs. |
| S4 | Web client (full REST + embedded SPA). | REST contract tests; build smoke test. |
| S5 | Docker (non-root, distroless), docs, ROADMAP 3.4 done. | - |

## 8. Out of scope (v1)

Path-prefix permissions, OAuth/SSO, billing, multi-org tenancy, multi-process sharding.
