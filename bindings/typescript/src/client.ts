import { execFile } from "node:child_process";
import { promisify } from "node:util";
import type {
  BranchInfo,
  CommitInfo,
  CommitResponse,
  CompiledCoreContext,
  ConsolidateOptions,
  ConsolidationReport,
  DiffEntry,
  GcStats,
  IngestOptions,
  IngestOutcome,
  MemoryEvent,
  MemoryListEntry,
  MemoryNodeResponse,
  MergeResponse,
  MergeStrategy,
  SearchHit,
  SearchOptions,
  WriteNodeOptions,
  WriteNodeResponse,
} from "./types.ts";

const execFileAsync = promisify(execFile);

export class EngramError extends Error {
  readonly statusCode: number | undefined;
  readonly code: string | undefined;

  constructor(message: string, statusCode?: number, code?: string) {
    super(message);
    this.name = "EngramError";
    this.statusCode = statusCode;
    this.code = code;
  }
}

export interface EngramClientConfig {
  baseUrl?: string;
  token?: string;
  timeoutMs?: number;
}

/**
 * Async HTTP REST & Server-Sent Events (SSE) client for `mem-daemon`.
 */
export class EngramClient {
  private readonly baseUrl: string;
  private readonly token: string | undefined;
  private readonly timeoutMs: number;

  constructor(config: EngramClientConfig = {}) {
    this.baseUrl = (config.baseUrl ?? "http://127.0.0.1:7700").replace(/\/+$/, "");
    this.token = config.token;
    this.timeoutMs = config.timeoutMs ?? 30_000;
  }

  private async request<T>(
    method: string,
    path: string,
    query?: Record<string, string | number | undefined>,
    body?: unknown,
  ): Promise<T> {
    const url = new URL(`${this.baseUrl}${path}`);
    if (query) {
      for (const [k, v] of Object.entries(query)) {
        if (v !== undefined) {
          url.searchParams.set(k, String(v));
        }
      }
    }

    const headers: Record<string, string> = {
      Accept: "application/json",
    };
    if (this.token) {
      headers.Authorization = `Bearer ${this.token}`;
    }
    let payload: string | undefined;
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      payload = JSON.stringify(body);
    }

    const resp = await fetch(url, {
      method,
      headers,
      body: payload,
      signal: AbortSignal.timeout(this.timeoutMs),
    });

    const text = await resp.text();
    if (!resp.ok) {
      let message = text || resp.statusText;
      let code: string | undefined;
      try {
        const parsed = JSON.parse(text) as { error?: string; code?: string };
        if (parsed.error) message = parsed.error;
        code = parsed.code;
      } catch {
        // non-JSON error body
      }
      throw new EngramError(message, resp.status, code);
    }
    return (text ? JSON.parse(text) : {}) as T;
  }

  async health(): Promise<Record<string, unknown>> {
    return this.request("GET", "/health");
  }

  async status(branch = "main"): Promise<Record<string, unknown>> {
    return this.request("GET", "/v1/status", { branch });
  }

  async list(branch = "main", prefix?: string): Promise<MemoryListEntry[]> {
    return this.request("GET", "/v1/nodes", { branch, prefix });
  }

  async read(path: string, branch = "main", commitId?: string): Promise<MemoryNodeResponse> {
    return this.request("GET", "/v1/node", { path, branch, commit_id: commitId });
  }

  async write(opts: WriteNodeOptions): Promise<WriteNodeResponse> {
    return this.request("PUT", "/v1/node", undefined, {
      path: opts.path,
      content: opts.content,
      branch: opts.branch ?? "main",
      tags: opts.tags,
      importance: opts.importance,
      content_type: opts.contentType,
      ttl_seconds: opts.ttlSeconds,
    });
  }

  async delete(path: string, branch = "main"): Promise<{ branch: string; path: string; removed_count: number }> {
    return this.request("DELETE", "/v1/node", { path, branch });
  }

  async commit(message: string, branch = "main", author = "ts-sdk"): Promise<CommitResponse> {
    return this.request("POST", "/v1/commits", undefined, { message, branch, author });
  }

  async log(branch = "main", limit = 20): Promise<CommitInfo[]> {
    return this.request("GET", "/v1/commits", { branch, limit });
  }

  async branches(): Promise<BranchInfo[]> {
    return this.request("GET", "/v1/branches");
  }

  async createBranch(name: string, from = "main"): Promise<{ created: string }> {
    return this.request("POST", "/v1/branches", undefined, { name, from });
  }

  async deleteBranch(name: string): Promise<{ deleted: string }> {
    return this.request("DELETE", "/v1/branches", { name });
  }

  async rewind(commitId: string, branch = "main"): Promise<{ branch: string; rewound_to: string }> {
    return this.request("POST", "/v1/rewind", undefined, { commit_id: commitId, branch });
  }

  async diff(base: string, target: string): Promise<DiffEntry[]> {
    return this.request("GET", "/v1/diff", { base, target });
  }

  async merge(source: string, target = "main", strategy: MergeStrategy = "auto"): Promise<MergeResponse> {
    return this.request("POST", "/v1/merge", undefined, { source, target, strategy });
  }

  async gc(): Promise<GcStats> {
    return this.request("POST", "/v1/gc");
  }

  async search(opts: SearchOptions): Promise<SearchHit[]> {
    return this.request("POST", "/v1/search", undefined, {
      query: opts.query,
      branch: opts.branch ?? "main",
      prefix: opts.prefix,
      top_k: opts.topK ?? 10,
    });
  }

  async compileCore(branch = "main", maxTokens = 2000): Promise<CompiledCoreContext> {
    return this.request("POST", "/v1/compile", undefined, { branch, max_tokens: maxTokens });
  }

  async ingest(opts: IngestOptions): Promise<IngestOutcome> {
    return this.request("POST", "/v1/ingest", undefined, {
      text: opts.text,
      role: opts.role ?? "dev",
      branch: opts.branch ?? "main",
      draft: opts.draft ?? false,
      title: opts.title,
      path: opts.path,
      tier: opts.tier,
      tags: opts.tags,
      author: opts.author,
      importance: opts.importance,
      commit: opts.commit ?? true,
      message: opts.message,
      use_llm: opts.useLlm ?? false,
    });
  }

  async consolidate(opts: ConsolidateOptions = {}): Promise<ConsolidationReport> {
    return this.request("POST", "/v1/consolidate", undefined, {
      branch: opts.branch ?? "main",
      consolidation_branch: opts.consolidationBranch,
      prefix: opts.prefix,
      since_ms: opts.sinceMs,
      max_notes: opts.maxNotes,
      min_importance: opts.minImportance,
      use_llm: opts.useLlm,
      auto_merge: opts.autoMerge,
      strategy: opts.strategy,
      delete_branch: opts.deleteBranch,
      archive_processed: opts.archiveProcessed,
      decay_factor: opts.decayFactor,
      dry_run: opts.dryRun,
    });
  }

  async *streamEvents(options: { branch?: string; signal?: AbortSignal } = {}): AsyncGenerator<MemoryEvent> {
    const url = new URL(`${this.baseUrl}/v1/events`);
    if (options.branch) {
      url.searchParams.set("branch", options.branch);
    }
    const headers: Record<string, string> = { Accept: "text/event-stream" };
    if (this.token) {
      headers.Authorization = `Bearer ${this.token}`;
    }

    const resp = await fetch(url, { headers, signal: options.signal });
    if (!resp.ok || !resp.body) {
      throw new EngramError(`failed to open SSE stream (${resp.status})`, resp.status);
    }

    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    let eventName: string | undefined;
    let dataLines: string[] = [];

    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split(/\r?\n/);
      buffer = lines.pop() ?? "";

      for (const line of lines) {
        if (line.startsWith(":")) continue;
        if (line === "") {
          if (dataLines.length > 0) {
            const rawData = dataLines.join("\n");
            dataLines = [];
            try {
              const parsed = JSON.parse(rawData) as MemoryEvent;
              if (!parsed.event && eventName) {
                parsed.event = eventName;
              }
              eventName = undefined;
              yield parsed;
            } catch {
              eventName = undefined;
            }
          }
        } else if (line.startsWith("event:")) {
          eventName = line.slice(6).trim();
        } else if (line.startsWith("data:")) {
          dataLines.push(line.slice(5).trim());
        }
      }
    }
  }
}

export interface LocalEngramConfig {
  dbPath: string;
  memBin?: string;
  provider?: "disabled" | "ollama" | "openai";
}

/**
 * Embedded local single-file client invoking the `mem` binary directly without starting a daemon.
 */
export class LocalEngram {
  private readonly dbPath: string;
  private readonly memBin: string;
  private readonly provider: string;

  constructor(config: LocalEngramConfig) {
    this.dbPath = config.dbPath;
    this.memBin = config.memBin ?? "mem";
    this.provider = config.provider ?? "disabled";
  }

  private async runJson<T>(args: string[]): Promise<T> {
    const fullArgs = ["--db", this.dbPath, "--provider", this.provider, "--json", ...args];
    try {
      const { stdout } = await execFileAsync(this.memBin, fullArgs, {
        maxBuffer: 16 * 1024 * 1024,
      });
      return JSON.parse(stdout) as T;
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : String(err);
      throw new EngramError(`mem CLI execution failed: ${msg}`);
    }
  }

  async init(): Promise<{ db_path: string; branch: string; root_commit_id: string }> {
    return this.runJson(["init"]);
  }

  async write(opts: WriteNodeOptions): Promise<WriteNodeResponse> {
    const args = ["write", opts.path, opts.content, "--branch", opts.branch ?? "main"];
    if (opts.importance !== undefined) {
      args.push("--importance", String(opts.importance));
    }
    if (opts.tags && opts.tags.length > 0) {
      args.push("--tags", opts.tags.join(","));
    }
    return this.runJson(args);
  }

  async read(path: string, branch = "main", commitId?: string): Promise<MemoryNodeResponse> {
    const args = ["read", path, "--branch", branch];
    if (commitId) args.push("--commit", commitId);
    return this.runJson(args);
  }

  async list(branch = "main", prefix?: string): Promise<MemoryListEntry[]> {
    const args = ["ls", "--branch", branch];
    if (prefix) args.push(prefix);
    return this.runJson(args);
  }

  async commit(message: string, branch = "main", author?: string): Promise<CommitResponse> {
    const args = ["commit", "-m", message, "--branch", branch];
    if (author) args.push("--author", author);
    return this.runJson(args);
  }

  async createBranch(name: string, from = "main"): Promise<{ created: string }> {
    return this.runJson(["branch", name, "--from", from]);
  }

  async branches(): Promise<BranchInfo[]> {
    return this.runJson(["branch"]);
  }

  async deleteBranch(name: string): Promise<{ deleted: string }> {
    return this.runJson(["branch", "-d", name]);
  }

  async rewind(commitId: string, branch = "main"): Promise<{ branch: string; rewound_to: string }> {
    return this.runJson(["rewind", commitId, "--branch", branch]);
  }

  async diff(base: string, target: string): Promise<DiffEntry[]> {
    return this.runJson(["diff", base, target]);
  }

  async merge(source: string, target = "main", strategy: MergeStrategy = "auto"): Promise<MergeResponse> {
    return this.runJson(["merge", source, target, "--strategy", strategy]);
  }

  async search(opts: SearchOptions): Promise<SearchHit[]> {
    const args = [
      "search",
      opts.query,
      "--branch",
      opts.branch ?? "main",
      "-k",
      String(opts.topK ?? 10),
    ];
    if (opts.prefix) args.push("--prefix", opts.prefix);
    return this.runJson(args);
  }

  async compileCore(branch = "main", maxTokens = 2000): Promise<CompiledCoreContext> {
    return this.runJson(["compile", "--branch", branch, "--max-tokens", String(maxTokens)]);
  }

  async ingest(opts: IngestOptions): Promise<IngestOutcome> {
    const args = [
      "ingest",
      "--role",
      opts.role ?? "dev",
      "--branch",
      opts.branch ?? "main",
    ];
    if (opts.draft) args.push("--draft");
    if (opts.title) args.push("--title", opts.title);
    if (opts.path) args.push("--path", opts.path);
    if (opts.tier) args.push("--tier", opts.tier);
    if (opts.tags && opts.tags.length > 0) args.push("--tag", opts.tags.join(","));
    if (opts.importance !== undefined) args.push("--importance", String(opts.importance));
    if (opts.commit === false) args.push("--no-commit");
    if (opts.useLlm) args.push("--llm");
    args.push(opts.text);
    return this.runJson(args);
  }

  async consolidate(opts: ConsolidateOptions = {}): Promise<ConsolidationReport> {
    const args = ["consolidate", "--branch", opts.branch ?? "main"];
    if (opts.consolidationBranch) args.push("--branch-name", opts.consolidationBranch);
    if (opts.prefix) args.push("--prefix", opts.prefix);
    if (opts.maxNotes !== undefined) args.push("--max-notes", String(opts.maxNotes));
    if (opts.minImportance !== undefined) args.push("--min-importance", String(opts.minImportance));
    if (opts.useLlm) args.push("--llm");
    if (opts.autoMerge) args.push("--auto-merge");
    if (opts.strategy) args.push("--strategy", opts.strategy);
    if (opts.deleteBranch) args.push("--delete-branch");
    if (opts.archiveProcessed) args.push("--archive");
    if (opts.dryRun) args.push("--dry-run");
    return this.runJson(args);
  }

  async gc(): Promise<GcStats> {
    return this.runJson(["gc"]);
  }
}
