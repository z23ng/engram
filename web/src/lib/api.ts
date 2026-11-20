// Typed client for the Engram REST API (`/api/v1`). See docs/SERVER_DESIGN.md section 5.

export type Role = "admin" | "dev" | "pm" | "bot";
export type Me = { name: string; role: Role; projects: string[] | null };
export type Branch = { name: string; head: string; changes: number };
export type ConsolidationRun = {
  id: number;
  project: string;
  trigger: "manual" | "schedule";
  requested_by: string;
  status: "running" | "succeeded" | "no_changes" | "skipped" | "failed";
  source_commit: string | null;
  branch: string | null;
  files_in: number;
  notes_written: number;
  notes_dropped: number;
  batches_failed: number;
  /** Episodic files of skipped batches; the next run reads them again. */
  failed_paths: string[];
  error: string | null;
  started_at_ms: number;
  finished_at_ms: number | null;
};
export type FileEntry = { path: string; blob_id: string; content_type: string; updated_at_ms: number };
export type Commit = {
  commit_id: string;
  parent_ids: string[];
  author: string;
  message: string;
  created_at_ms: number;
};
export type Change = { path: string; op: "upsert" | "delete" };
export type DiffEntry = { path: string; kind: "added" | "modified" | "deleted" };
export type Hit = { path: string; title: string; heading_path: string; snippet: string };
export type Key = {
  id: number;
  name: string;
  role: Role;
  projects: string[] | null;
  expires_at_ms: number | null;
  revoked: boolean;
};
export type AuditEntry = {
  at_ms: number;
  key_id: number | null;
  key_name: string | null;
  role: string | null;
  project: string;
  operation: string;
  commit_id: string | null;
};
export type MergeResult =
  | { outcome: "fast_forward" | "merged"; commit_id: string; target: string }
  | { outcome: "conflict"; base_commit: string; conflicts: { path: string }[] };

export class ApiError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

type Query = Record<string, string | number | undefined>;
type Options = { query?: Query; json?: unknown; body?: BodyInit; contentType?: string; accept?: number[] };

export function createClient(getKey: () => string | null, onUnauthorized: () => void) {
  async function send(method: string, path: string, o: Options = {}): Promise<Response> {
    const qs = new URLSearchParams();
    for (const [k, v] of Object.entries(o.query ?? {})) if (v !== undefined) qs.set(k, String(v));
    const query = qs.toString();
    const headers = new Headers();
    const key = getKey();
    if (key) headers.set("authorization", `Bearer ${key}`);
    let body = o.body;
    if (o.json !== undefined) {
      headers.set("content-type", "application/json");
      body = JSON.stringify(o.json);
    } else if (o.contentType) {
      headers.set("content-type", o.contentType);
    }
    const resp = await fetch(`/api/v1${path}${query ? `?${query}` : ""}`, { method, headers, body });
    if (resp.ok || o.accept?.includes(resp.status)) return resp;
    if (resp.status === 401) onUnauthorized();
    const text = await resp.text();
    let message = text || `HTTP ${resp.status}`;
    try {
      const parsed = JSON.parse(text);
      if (typeof parsed?.error === "string") message = parsed.error;
    } catch {
      // Not JSON: keep the raw text.
    }
    throw new ApiError(resp.status, message);
  }

  const json = async <T>(method: string, path: string, o?: Options) =>
    (await (await send(method, path, o)).json()) as T;
  const p = (project: string) => `/projects/${encodeURIComponent(project)}`;

  return {
    me: () => json<Me>("GET", "/me"),
    projects: () => json<{ projects: string[] }>("GET", "/projects").then((r) => r.projects),
    createProject: (slug: string) => json("POST", "/projects", { json: { slug } }),
    keys: () => json<{ keys: Key[] }>("GET", "/keys").then((r) => r.keys),
    createKey: (input: { name: string; role: Role; projects: string[] | null; expires_days?: number }) =>
      json<{ key: Key; secret: string }>("POST", "/keys", { json: input }),
    revokeKey: (id: number) => send("DELETE", `/keys/${id}`).then(() => undefined),
    audit: (project: string, limit = 200) =>
      json<{ entries: AuditEntry[] }>("GET", `${p(project)}/audit`, { query: { limit } }).then((r) => r.entries),
    branches: (project: string) =>
      json<{ branches: Branch[] }>("GET", `${p(project)}/branches`).then((r) => r.branches),
    tree: (project: string, branch: string) =>
      json<{ files: FileEntry[] }>("GET", `${p(project)}/tree`, { query: { branch } }).then((r) => r.files),
    log: (project: string, branch: string, limit = 100) =>
      json<{ commits: Commit[] }>("GET", `${p(project)}/log`, { query: { branch, limit } }).then((r) => r.commits),
    status: (project: string, branch: string) =>
      json<{ changes: Change[] }>("GET", `${p(project)}/status`, { query: { branch } }).then((r) => r.changes),
    diff: (project: string, base: string, target: string) =>
      json<{ entries: DiffEntry[] }>("GET", `${p(project)}/diff`, { query: { base, target } }).then((r) => r.entries),
    search: (project: string, branch: string, q: string, limit = 20) =>
      json<{ hits: Hit[] }>("GET", `${p(project)}/search`, { query: { q, branch, limit } }).then((r) => r.hits),
    read: (project: string, branch: string, path: string) =>
      json<{ path: string; blob_id: string; content: string }>("GET", `${p(project)}/read`, { query: { path, branch } }),
    write: (project: string, branch: string, path: string, content: string) =>
      json("POST", `${p(project)}/write`, { json: { path, content, branch } }),
    commit: (project: string, branch: string, message: string) =>
      json<{ commit_id: string }>("POST", `${p(project)}/commit`, { json: { branch, message } }),
    deleteFile: (project: string, branch: string, path: string) =>
      json<{ deleted: number }>("DELETE", `${p(project)}/files`, { query: { path, branch } }),
    upload: (project: string, branch: string, path: string, file: Blob) =>
      json("PUT", `${p(project)}/raw`, { query: { path, branch }, body: file, contentType: file.type }),
    rawBlob: (project: string, branch: string, path: string) =>
      send("GET", `${p(project)}/raw`, { query: { path, branch } }).then((r) => r.blob()),
    createBranch: (project: string, name: string, from: string) =>
      json("POST", `${p(project)}/branches`, { json: { name, from } }),
    merge: (project: string, source: string, target: string) =>
      json<MergeResult>("POST", `${p(project)}/merge`, { json: { source, target }, accept: [409] }),
    indexInfo: (project: string) =>
      json<{ model: string; files: number; files_with_vectors: number; chunks: number }>("GET", `${p(project)}/index`),
    consolidate: (project: string) => json<{ run_id: number }>("POST", `${p(project)}/consolidate`),
    consolidationRuns: (project: string, limit = 10) =>
      json<{ runs: ConsolidationRun[]; every_hours: number | null }>("GET", `${p(project)}/consolidate/runs`, { query: { limit } }),
    setConsolidateSchedule: (project: string, every_hours: number | null) =>
      json<{ every_hours: number | null }>("PUT", `${p(project)}/consolidate/schedule`, { json: { every_hours } }),
    reindex: (project: string) =>
      json<{ files_indexed: number; chunks: number; model: string }>("POST", `${p(project)}/reindex`),
  };
}

export type Client = ReturnType<typeof createClient>;
