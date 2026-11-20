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

WebDAV vault behavior (S3):

- URL: `/p/{project}/dav/{branch}/`. In the branch segment, write `/` as `~` (`draft/pm-inbox` -> `draft~pm-inbox`), because dav-server rejects a decoded `/` inside a path segment; git also forbids `~` in ref names. A segment that decodes to `/` returns 400.
- Auth: HTTP Basic (any username, password = API key) or `Bearer`. Missing or invalid key -> 401 with `WWW-Authenticate: Basic realm="engram"`. Read methods: OPTIONS, PROPFIND, GET, HEAD. Write methods: PUT, DELETE, MKCOL, MOVE, COPY, PROPPATCH, LOCK, UNLOCK (403 for `bot`). Any other method -> 405.
- Namespace: the root lists only `core/`, `semantic/`, `procedural/`, `episodic/`. Writable: `.md` files inside a tier, and images (`png`, `jpg`, `jpeg`, `gif`, `webp`; no SVG because it can carry scripts) below `<tier>/_attachments/` for every tier except `core` (so `compile_core` never sees binary data). Images keep their content type and are not indexed. Other targets, hidden names, and `_templates` -> 403. All writes, including every file of a folder MOVE or COPY, pass the same check before anything is written. In Obsidian, set "Default location for new attachments" to a `_attachments` subfolder of a tier, for example `semantic/_attachments`. PUT body limit 8 MiB -> 413. File mtime = node `updated_at_ms`; ETag = blob id.
- Folders exist only through files. MKCOL inside a tier succeeds and keeps the empty folder in process memory only (lost on restart).
- Idle auto-commit: each mutation writes the branch workspace at once; after `dav_commit_delay` (default 10s) without a new DAV mutation on that branch, the server commits once. Author = key name, or `webdav:<a>,<b>` (sorted) for several keys; message `webdav sync: <N> change(s)`. No commit when the workspace has no changes. One audit row `dav.commit` per writer key; single PUTs are not audited. The commit includes any other staged (uncommitted) workspace changes of that branch, for example staged REST or MCP writes.
- Startup: `mem serve` commits every branch whose workspace still holds changes (for example after a restart inside the idle window), with author `system:startup`, message `startup: commit <N> leftover change(s)`, and one audit row `startup.commit` per commit.
- Obsidian (Remotely Save plugin, WebDAV): server address `https://<host>/p/<project>/dav/main/`, any username, password = API key (a `dev` or `pm` key to push, a `bot` key for read-only).

REST API behavior (S4a). All routes need `Authorization: Bearer <key>`. Project routes check the project scope first (403), then the role permission. `branch` defaults to `main`; branch names go URL-encoded in the query (`draft%2Fx`).

| Route | Permission | Result |
|---|---|---|
| `GET /api/v1/me` | any key | `{name, role, projects}` (`projects: null` = all) |
| `GET /api/v1/projects` | any key | projects in the key scope |
| `POST /api/v1/projects` `{slug}` | global admin | 201; audit `project.create` |
| `GET/POST /api/v1/keys`, `DELETE /api/v1/keys/{id}` | global admin | list (never hashes or secrets) / 201 `{key, secret}` (secret shown once) / 204, 404 if not active |
| `GET .../projects/{p}/audit?limit=` | Manage | newest first, max 1000 |
| `GET .../branches` | Read | `{name, head, changes}` sorted by name; `changes` = staged workspace entries |
| `GET .../tree?branch=&prefix=` | Read | workspace view: `{path, blob_id, content_type, updated_at_ms}` |
| `GET .../log?branch=&limit=` | Read | commits, max 500 |
| `GET .../status?branch=` | Read | staged changes `{path, op: upsert\|delete}` |
| `GET .../diff?base=&target=` | Read | revision = 64-hex commit, `workspace:<branch>`, or a branch name (its head) |
| `GET .../search?q=&branch=&prefix=&limit=` | Read | hybrid search over the branch workspace, max 50 |
| `GET .../raw?path=&branch=` | Read | bytes; only png/jpeg/gif/webp inline, `text/*` as `text/plain`, all else `application/octet-stream` + `Content-Disposition: attachment`; always `nosniff`, `no-store`, `CSP: sandbox; default-src 'none'` (stored types can come from MCP metadata, and the web client shares the origin) |
| `GET .../read`, `POST .../write` | Read / Write | S1 (unchanged) |
| `POST .../commit` `{message, branch?}` | Write | author = key name; 400 if nothing is staged; audit `rest.commit` |
| `DELETE .../files?path=&branch=` | Write | `{deleted}` (file or folder); 404 if nothing matched; audit `rest.delete` |
| `PUT .../raw?path=&branch=` (body = bytes) | Write | images only, same rule as WebDAV attachments; 8 MiB -> 413; not indexed; audit `rest.upload` |
| `POST .../branches` `{name, from?}` | Branch | 201; `~` is rejected (WebDAV uses it for `/`); audit `rest.branch` |
| `POST .../merge` `{source, target?, strategy?}` | Branch | `{outcome: fast_forward\|merged, commit_id}`; conflict -> 409 `{conflicts}` and no change; audit `rest.merge`. A real merge commit has author = key name (MCP `mem_merge` also overrides `author`). |
| `GET .../index` | Read | `{model, files, files_with_vectors, chunks}`: the active embedding `model_id` and index coverage, counted per indexed content version (all branches and history). The Search view shows it as a badge: "BM25 only" or "Hybrid · <model> · N/M files embedded". |
| `POST .../reindex` | Manage | Indexes every branch view (head plus staged changes) again with the server's embedder, replacing rows blob by blob in groups of at most 128 chunks (no clear-first, so a failure keeps the old rows of the other blobs). Refused when no embedding model is active and the index holds vectors. Returns `{files_indexed, chunks, model}`; audit `rest.reindex`. `mem admin reindex --project <slug>` does the same from the CLI, and the Search view shows a "Rebuild index" button to admin keys. |

Search index behavior:

- Every write path (REST, WebDAV, MCP, CLI) indexes the new blob at once; each search first indexes any blob of the searched view that has no index rows. Index rows are keyed by `blob_id`, so branches and merges reuse them.
- Table `blob_index_model(blob_id, model)` records which embedding model made a blob's vectors (`model_id`: `ollama:bge-m3`, `openai:text-embedding-3-small[:dims]`, or `disabled` for BM25 only). When the active model differs from the stored one, an index pass drops that blob's chunk, full-text, and link rows and indexes it again, so enabling or changing a model re-embeds old files without a manual rebuild. One search re-embeds at most 32 such blobs; later searches (or a rebuild) catch up. Vectors that have no model row (written before the table existed) count as stale. Vector scoring uses only vectors of the active model, so not-yet-re-embedded vectors never mix into results. Embedding requests carry at most 128 texts. With no model active (Ollama unreachable, or `disabled`), existing rows are kept, so an embedder outage never drops vectors.
- `mem serve` logs to stderr when it opens a project engine: `project <slug>: search model <model_id>`, preceded by the fallback notice when the configured Ollama or OpenAI endpoint is not usable. The embedder is chosen when the engine opens and stays until the server restarts.

Web console behavior (S4b):

- Source in `web/` (React 19, TypeScript 7, Vite 8, Tailwind CSS 4, shadcn-style components copied into `web/src/components/ui`, CodeMirror 6, TanStack Query 5). `npm --prefix web ci && npm --prefix web run build` writes `web/dist`; `rust-embed` 8.14.0 embeds it into `mem-server` at compile time in release builds (debug builds read `web/dist` from disk). Without a build, client routes return 503 with the build command; the API keeps working.
- Routing: the axum fallback serves `/assets/*` files (immutable cache) and `index.html` for every other GET (`no-cache`). Paths under `/api/` and `/p/`, missing assets, and any path with a `..` segment return a plain 404, never HTML.
- `index.html` headers: `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'` (inline styles are needed by CodeMirror), `nosniff`, `Referrer-Policy: no-referrer`. Vite asset inlining is off (`assetsInlineLimit: 0`) so fonts never become `data:` URIs. The Files view (CodeMirror, Markdown renderer) loads as a separate chunk; `npm run build` fails when the entry script is larger than 500 kB (`web/scripts/check-bundle.mjs`).
- Session: the user pastes an API key; it is checked with `GET /api/v1/me` and kept in `sessionStorage` (this tab only). A 401 from any call signs the user out. Theme (light/dark) is kept in `localStorage`.
- Layout: left navigation (Files, Changes, History, Branches, Search, Audit log for admin; Projects, API keys for global admin), top bar with project and branch switchers (staged count), Ctrl/Cmd+K palette (views, files, projects), and a Connect dialog with the MCP, WebDAV (`~` branch form), and REST URLs.
- Files: tier tree, CodeMirror editor with Edit / Split / Preview, Ctrl/Cmd+S stages the file (`POST .../write`), Commit dialog commits all staged changes with author = key name. Pasting a png/jpg/gif/webp image uploads it to `<tier>/_attachments/<UTC stamp>-<name>.<ext>` (`semantic` for `core` documents) and inserts a relative Markdown link, which also works in Obsidian. The preview renders Markdown without raw HTML; vault images load through `GET .../raw` as `blob:` URLs, external images are not loaded.
- The UI hides write actions for `bot` keys; the server still enforces every permission.

CLI additions: `mem serve --data <dir> --listen <addr>`, `mem admin project create|list`, `mem admin key create|list|revoke`, `mem admin reindex --project <slug>` (uses the same embedder settings as `mem serve`). First start on an empty data directory prints a one-time `admin` key.

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
| S6 | Background consolidation (section 7.1). | See 7.1 tests. |

### 7.1 S6 background consolidation (ROADMAP 4.3)

Decisions (TL, 2026-11-20): manual trigger plus an optional per-project schedule; the result is a review branch that a person merges. The hot path stays free of LLM calls.

Input:

- Source revision: the head commit of `main` when the run starts. Staged changes and other branches are never read.
- Source files: `/episodic/**/*.md` files added or changed since the `source_commit` of the last successful run (all of `/episodic/` on the first run). No new episodic changes -> the run ends as `skipped` and makes no branch.
- Context per source file: the top 3 search hits for its title in `/semantic/` and `/procedural/` on the same revision (deterministic search, no extra LLM call), with full content.
- Batches: at most 8 source files and 48,000 bytes of prompt text (headers included) per LLM call, about 15k tokens for CJK text. An episodic file larger than 23,000 bytes is sent in parts (`<path> (part i/n)`), split at line ends. A related note larger than 8,000 bytes is left out of the context, because the LLM may rewrite a context note in full and must never see a truncated copy.
- Ollama must run with a context window that holds a batch: start it with `OLLAMA_CONTEXT_LENGTH=32768` (the default of 4096 tokens truncates the prompt; the model then loops and Ollama aborts with `token repeat limit reached`).

LLM call (reuses `ConsolidateLlmConfig` and the OpenAI-compatible `chat/completions` client of `ingest.rs`; temperature 0.1):

- The prompt asks for JSON only: `{"notes":[{"path": "...", "content": "..."}]}`. `content` is the full new Markdown of a note (create, or a full rewrite of one of the context notes).
- Validation, deny by default: `path` must be a valid `VirtualPath` under `/semantic/` or `/procedural/`, end in `.md`, not be under `_attachments/` or `_templates/`, and `content` must be non-empty and at most 256 KiB. Invalid notes are dropped and counted. The run never deletes, and never writes `/core/` or `/episodic/`.
- The prompt asks for at most 5 notes per batch, each under 4,000 characters.
- A batch whose LLM call fails or whose output is unparsable is skipped and counted (`batches_failed`; the run `error` lists `batch <i> of <n> (<files>): <reason>`; `failed_paths` lists the episodic files), and the run still commits the notes of the good batches. The run fails only when every batch fails. A failed run creates no branch. (TL decision 2026-11-20, after a 7B batch on kepay-v1 exceeded the output cap.) The next run reads the `failed_paths` of the previous run again, in addition to the new changes. It reads the whole file again, also the parts whose batch succeeded, so its review branch can repeat notes of the previous, unmerged review branch (review or delete one before merging the other). Only empty episodic files changed: `no_changes`, with no LLM call.
- A path that an earlier batch of the same run already returned is dropped, and so is a rewrite of an existing note that was not in that batch's context. The prompt budget of 48,000 bytes includes the system prompt; a source keeps only the context notes that fit next to its largest part, so no request exceeds the budget, whatever the path lengths.

Output:

- New branch `consolidate/<UTC yyyymmdd-HHMM>` from the source commit; the valid notes are written and committed once, author `consolidator:<model>`, message `consolidate: <N> note(s) from <M> episodic file(s) @ <source_commit[..10]>`. Notes whose content equals the current blob are skipped; if no note changes anything the run ends as `no_changes` and the branch is removed.
- Nothing merges automatically. A person reviews the diff (Branches / History) and uses the existing Merge action; merge author = key name.
- Control-plane table `consolidation_runs(id, project, trigger 'manual'|'schedule', requested_by, status 'running'|'succeeded'|'no_changes'|'skipped'|'failed', source_commit, branch, files_in, notes_written, notes_dropped, batches_failed, failed_paths (JSON array), error, started_at_ms, finished_at_ms)`; `batches_failed` and `failed_paths` are added by an idempotent migration (a "duplicate column name" from a concurrent migration counts as done). Every finished run, also a `failed` one, is audited as `consolidate.run` with the commit id; an audit failure is logged and does not change the run status.
- `mem admin consolidate` exits non-zero only for a `failed` run; a run with skipped batches prints a warning.
- One run per project at a time: a second request while one is running returns 409. A drop guard releases the project also when the run panics.

Triggers:

- `POST /api/v1/projects/{p}/consolidate` (Manage) -> 202 `{run_id}`; the run executes on a blocking thread. `GET .../consolidate/runs?limit=` (Manage) lists runs, newest first.
- `mem admin --data <dir> consolidate --project <slug>` runs synchronously and prints the result (same embedder and LLM settings as `mem serve`).
- Schedule: `projects.consolidate_every_hours` (NULL = off, default). Set with `PUT .../consolidate/schedule {"every_hours": N|null}` (Manage) or `mem admin project schedule <slug> --every <hours|off>`. The field is required; a schedule on a server with no consolidation LLM is refused (REST 400, CLI non-zero exit). `mem serve` runs one tokio task that wakes every 60 s and starts a `schedule` run for each project whose last finished run (any status) is at least `every_hours` old.
- Console: on Branches, admin keys see "Consolidate now", the schedule selector (off / 6 h / 24 h / 7 d), and the last 10 runs with status, branch link, and error. The panel polls every 2 s while a run is `running`. At startup `mem serve` marks runs left `running` by a stopped server as `failed` (`server restarted`). `consolidate/*` branches carry an "LLM" badge.

Implementation notes (S6 as built):

- A `no_changes` run never creates the branch (same result as creating and removing it).
- When `consolidate/<UTC yyyymmdd-HHMM>` already exists (two runs in one minute), the branch gets the suffix `-<run_id>`.
- "Last successful run" = the newest run with status `succeeded`, `no_changes`, or `skipped`; each of them consumed the episodic files up to its `source_commit`.
- `GET .../consolidate/runs` also returns `every_hours`, so the console needs one call. Schedule changes are audited as `consolidate.schedule`; every finished run (also `skipped` and `no_changes`) is audited as `consolidate.run`.
- Each LLM call has a 600 s timeout, `max_tokens: 8192` (a runaway generation then ends as unparsable output instead of hitting the timeout), and asks for `response_format: json_object`; the reply may wrap the JSON in a fence or prose.
- The one-run guard is per process: `mem admin consolidate` and a running `mem serve` do not see each other's runs. A run interrupted by a server stop stays `running` in the table.

Privacy: with the OpenAI backend, episodic and context note text leaves the machine. `mem serve` logs the LLM backend and model at startup.

Tests (mock LLM on a local `TcpListener`, as in `ingest.rs`): path validation table (core, episodic, `..`, non-md, attachments, oversize); a manual run writes the expected notes on a `consolidate/*` branch with author `consolidator:<model>` and leaves `main` unchanged; a second run with no new episodic changes is `skipped`; an unreachable LLM gives `failed` and no branch; output identical to existing notes gives `no_changes`; a concurrent trigger gets 409; dev and bot keys get 403; the schedule picks only due projects (injected clock); CLI command prints the run result; the UI shows the button only for admin and lists runs.

## 8. Out of scope (v1)

Path-prefix permissions, OAuth/SSO, billing, multi-org tenancy, multi-process sharding.
