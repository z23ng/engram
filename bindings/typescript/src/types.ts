export type MemoryTier = "core" | "episodic" | "semantic" | "procedural";
export type MergeStrategy = "auto" | "ours" | "theirs";
export type IngestRole = "pm" | "dev" | "bot";

export interface MemoryMetadata {
  content_type: string;
  importance: number;
  tags: string[];
  created_at_ms: number;
  updated_at_ms: number;
  ttl_seconds: number | null;
}

export interface MemoryNodeResponse {
  path: string;
  blob_id: string | null;
  metadata: MemoryMetadata;
  content: string;
}

export interface MemoryListEntry {
  path: string;
  blob_id: string;
  importance: number;
  tags: string[];
  updated_at_ms?: number;
}

export interface WriteNodeOptions {
  path: string;
  content: string;
  branch?: string;
  tags?: string[];
  importance?: number;
  contentType?: string;
  ttlSeconds?: number;
}

export interface WriteNodeResponse {
  status: string;
  branch: string;
  path: string;
  blob_id: string;
}

export interface CommitResponse {
  branch: string;
  commit_id: string;
  author: string;
  message: string;
}

export interface CommitInfo {
  commit_id: string;
  tree_id: string;
  parent_ids: string[];
  author: string;
  message: string;
  created_at_ms: number;
}

export interface BranchInfo {
  branch: string;
  commit_id: string;
}

export interface DiffEntry {
  path: string;
  kind: "added" | "modified" | "deleted";
  old_blob_id: string | null;
  new_blob_id: string | null;
}

export interface MergeConflict {
  path: string;
  base_blob_id: string | null;
  ours_blob_id: string | null;
  theirs_blob_id: string | null;
}

export interface MergeResponse {
  outcome: "fast_forward" | "merged" | "conflict";
  commit_id?: string;
  base_commit?: string;
  conflicts?: MergeConflict[];
}

export interface SearchOptions {
  query: string;
  branch?: string;
  prefix?: string;
  topK?: number;
}

export interface SearchHit {
  path: string;
  blob_id: string;
  title: string;
  heading_path: string;
  snippet: string;
  start_line: number;
  end_line: number;
  score: number;
  bm25_rank: number | null;
  vector_rank: number | null;
  graph_score: number;
  outlinks: string[];
  backlinks: string[];
}

export interface CompiledCoreContext {
  rendered: string;
  included_paths: string[];
  skipped_paths: string[];
  estimated_tokens: number;
}

export interface IngestOptions {
  text: string;
  role?: IngestRole;
  branch?: string;
  draft?: boolean;
  title?: string;
  path?: string;
  tier?: MemoryTier;
  tags?: string[];
  author?: string;
  importance?: number;
  commit?: boolean;
  message?: string;
  useLlm?: boolean;
}

export interface IngestOutcome {
  branch: string;
  path: string;
  title: string;
  role: IngestRole;
  blob_id: string;
  commit_id: string | null;
  linked_notes: string[];
  matched_paths: string[];
  tags: string[];
  used_llm: boolean;
  markdown: string;
}

export interface ConsolidateOptions {
  branch?: string;
  consolidationBranch?: string;
  prefix?: string;
  sinceMs?: number;
  maxNotes?: number;
  minImportance?: number;
  useLlm?: boolean;
  autoMerge?: boolean;
  strategy?: MergeStrategy;
  deleteBranch?: boolean;
  archiveProcessed?: boolean;
  decayFactor?: number;
  dryRun?: boolean;
}

export interface DistilledCard {
  tier: MemoryTier;
  path: string;
  title: string;
  blob_id: string | null;
  source_paths: string[];
  linked_notes: string[];
  markdown: string;
}

export interface ConsolidationReport {
  source_branch: string;
  consolidation_branch: string | null;
  dry_run: boolean;
  used_llm: boolean;
  llm_fallback_reason: string | null;
  episodes_scanned: number;
  source_episode_paths: string[];
  distilled_cards: DistilledCard[];
  archive_path: string | null;
  commit_id: string | null;
  diff_from_source: DiffEntry[];
  merge_status: {
    status: "skipped" | "fast_forward" | "merged" | "conflict";
    commit_id?: string;
    base_commit?: string;
    conflicts?: MergeConflict[];
  };
  consolidation_branch_deleted: boolean;
}

export interface GcStats {
  removed_commits: number;
  removed_trees: number;
  removed_blobs: number;
  reclaimed_bytes: number;
}

export interface MemoryEvent {
  event: string;
  branch: string;
  timestamp_ms: number;
  data: Record<string, unknown>;
}
