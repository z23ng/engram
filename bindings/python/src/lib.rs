//! Native PyO3 bindings exposing Engram (`mem`) directly to Python (`import engram`).

use std::path::PathBuf;
use std::sync::Arc;

use mem_consolidate::{ConsolidationEngine, ConsolidationRequest};
use mem_core::{
    CommitId, DiffKind, MemoryMetadata, MemoryNode, MemoryTier, MergeOutcome, MergeStrategy,
    Revision, VirtualPath,
};
use mem_search::{
    ConsolidateLlmConfig, EmbeddingProviderConfig, IngestRequest, IngestRole, MemSearchEngine,
    OpenAiCompatibleEmbedder, ProviderBackend, SearchConfig, SearchRequest,
};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use serde_json::{json, Value};

fn to_py_err(err: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

fn json_to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    let encoded = serde_json::to_string(value).map_err(to_py_err)?;
    let json_mod = PyModule::import(py, "json")?;
    json_mod.call_method1("loads", (encoded,))
}

fn parse_revision(raw: &str) -> Revision {
    if let Ok(cid) = CommitId::from_hex(raw) {
        Revision::Commit(cid)
    } else {
        Revision::BranchWorkspace(raw.to_string())
    }
}

fn build_configs(
    provider: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
    embed_model: Option<&str>,
    llm_model: Option<&str>,
) -> PyResult<(EmbeddingProviderConfig, ConsolidateLlmConfig)> {
    let backend = ProviderBackend::parse(provider).ok_or_else(|| {
        PyValueError::new_err(format!(
            "unknown provider '{provider}'; expected 'disabled', 'ollama', or 'openai'"
        ))
    })?;

    let mut emb = match backend {
        ProviderBackend::Disabled => EmbeddingProviderConfig::disabled(),
        ProviderBackend::Ollama => EmbeddingProviderConfig::ollama_default(),
        ProviderBackend::OpenAi => {
            EmbeddingProviderConfig::openai_production(api_key.unwrap_or_default())
        }
    };
    let mut llm = match backend {
        ProviderBackend::Disabled => ConsolidateLlmConfig::disabled(),
        ProviderBackend::Ollama => ConsolidateLlmConfig::ollama_default(),
        ProviderBackend::OpenAi => {
            ConsolidateLlmConfig::openai_production(api_key.unwrap_or_default())
        }
    };

    if let Some(u) = base_url.filter(|s| !s.trim().is_empty()) {
        emb.base_url = u.to_string();
        llm.base_url = u.to_string();
    }
    if let Some(k) = api_key.filter(|s| !s.trim().is_empty()) {
        emb.api_key = Some(k.to_string());
        llm.api_key = Some(k.to_string());
    }
    if let Some(m) = embed_model.filter(|s| !s.trim().is_empty()) {
        emb.model = m.to_string();
    }
    if let Some(m) = llm_model.filter(|s| !s.trim().is_empty()) {
        llm.model = m.to_string();
    }

    Ok((emb, llm))
}

fn build_engine(
    vcs: MemVcs,
    emb_cfg: &EmbeddingProviderConfig,
) -> MemSearchEngine {
    match emb_cfg.backend {
        ProviderBackend::Disabled => MemSearchEngine::new_bm25_only(vcs),
        ProviderBackend::Ollama => {
            let embedder = Arc::new(OpenAiCompatibleEmbedder::new(emb_cfg.clone()));
            MemSearchEngine::new(vcs, embedder, SearchConfig::default())
        }
        ProviderBackend::OpenAi => {
            let has_key = emb_cfg
                .api_key
                .as_deref()
                .is_some_and(|k| !k.trim().is_empty());
            if has_key {
                let embedder = Arc::new(OpenAiCompatibleEmbedder::new(emb_cfg.clone()));
                MemSearchEngine::new(vcs, embedder, SearchConfig::default())
            } else {
                MemSearchEngine::new_bm25_only(vcs)
            }
        }
    }
}

/// Embedded single-file Engram engine backed by Rust `MemVcs`, `MemSearchEngine`, and `ConsolidationEngine`.
#[pyclass(name = "NativeEngram")]
pub struct NativeEngram {
    vcs: MemVcs,
    search: MemSearchEngine,
    llm_cfg: ConsolidateLlmConfig,
}

#[pymethods]
impl NativeEngram {
    /// Opens or creates a SQLite-backed Engram memory repository at `db_path`.
    #[staticmethod]
    #[pyo3(signature = (db_path, provider="disabled", base_url=None, api_key=None, embed_model=None, llm_model=None))]
    pub fn open(
        db_path: &str,
        provider: &str,
        base_url: Option<&str>,
        api_key: Option<&str>,
        embed_model: Option<&str>,
        llm_model: Option<&str>,
    ) -> PyResult<Self> {
        let path = PathBuf::from(db_path);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(to_py_err)?;
            }
        }
        let store = SqliteStore::open(&path).map_err(to_py_err)?;
        let vcs = MemVcs::new(store);
        vcs.init().map_err(to_py_err)?;
        let (emb_cfg, llm_cfg) = build_configs(provider, base_url, api_key, embed_model, llm_model)?;
        let search = build_engine(vcs.clone(), &emb_cfg);
        Ok(Self {
            vcs,
            search,
            llm_cfg,
        })
    }

    /// Opens an ephemeral in-memory Engram repository (ideal for tests).
    #[staticmethod]
    #[pyo3(signature = (provider="disabled", base_url=None, api_key=None, embed_model=None, llm_model=None))]
    pub fn open_in_memory(
        provider: &str,
        base_url: Option<&str>,
        api_key: Option<&str>,
        embed_model: Option<&str>,
        llm_model: Option<&str>,
    ) -> PyResult<Self> {
        let store = SqliteStore::open_in_memory().map_err(to_py_err)?;
        let vcs = MemVcs::new(store);
        vcs.init().map_err(to_py_err)?;
        let (emb_cfg, llm_cfg) = build_configs(provider, base_url, api_key, embed_model, llm_model)?;
        let search = build_engine(vcs.clone(), &emb_cfg);
        Ok(Self {
            vcs,
            search,
            llm_cfg,
        })
    }

    /// Writes or updates a Markdown memory node in `branch`'s working workspace and indexes it.
    #[pyo3(signature = (path, content, branch="main", tags=None, importance=0.5))]
    pub fn write<'py>(
        &self,
        py: Python<'py>,
        path: &str,
        content: &str,
        branch: &str,
        tags: Option<Vec<String>>,
        importance: f32,
    ) -> PyResult<Bound<'py, PyAny>> {
        let vpath = VirtualPath::new(path).map_err(to_py_err)?;
        vpath.validate_file_path().map_err(to_py_err)?;
        let mut meta = MemoryMetadata::default();
        meta.importance = importance.clamp(0.0, 1.0);
        if let Some(t) = tags {
            meta.tags = t;
        }
        let node = MemoryNode::new(vpath.clone(), content.as_bytes().to_vec()).with_metadata(meta);
        let blob_id = self.vcs.write_node(branch, node).map_err(to_py_err)?;
        self.search
            .index_blob_if_needed(&vpath, &blob_id)
            .map_err(to_py_err)?;
        json_to_py(
            py,
            &json!({
                "status": "staged",
                "branch": branch,
                "path": vpath.as_str(),
                "blob_id": blob_id.to_hex(),
            }),
        )
    }

    /// Reads a memory node from a branch workspace or specific commit ID; returns `None` if absent.
    #[pyo3(signature = (path, branch="main", commit_id=None))]
    pub fn read<'py>(
        &self,
        py: Python<'py>,
        path: &str,
        branch: &str,
        commit_id: Option<&str>,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        let vpath = VirtualPath::new(path).map_err(to_py_err)?;
        let rev = match commit_id {
            Some(cid) => Revision::Commit(CommitId::from_hex(cid).map_err(to_py_err)?),
            None => Revision::BranchWorkspace(branch.to_string()),
        };
        let node = self.vcs.read(&rev, &vpath).map_err(to_py_err)?;
        match node {
            Some(n) => {
                let text = n.content_str().map_err(to_py_err)?;
                let val = json!({
                    "path": n.path.as_str(),
                    "blob_id": n.blob_id.map(|b| b.to_hex()),
                    "metadata": n.metadata,
                    "content": text,
                });
                Ok(Some(json_to_py(py, &val)?))
            }
            None => Ok(None),
        }
    }

    /// Deletes a file or directory prefix inside `branch`'s working workspace.
    #[pyo3(signature = (path, branch="main"))]
    pub fn delete(&self, path: &str, branch: &str) -> PyResult<usize> {
        let vpath = VirtualPath::new(path).map_err(to_py_err)?;
        self.vcs.delete_path(branch, &vpath).map_err(to_py_err)
    }

    /// Lists active memory nodes in `branch` optionally filtered by `prefix`.
    #[pyo3(signature = (branch="main", prefix=None))]
    pub fn list<'py>(
        &self,
        py: Python<'py>,
        branch: &str,
        prefix: Option<&str>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let p = prefix
            .filter(|s| !s.trim().is_empty())
            .map(VirtualPath::new)
            .transpose()
            .map_err(to_py_err)?;
        let entries = self
            .vcs
            .list(&Revision::BranchWorkspace(branch.to_string()), p.as_ref())
            .map_err(to_py_err)?;
        let arr: Vec<Value> = entries
            .into_iter()
            .map(|(vp, bid, meta)| {
                json!({
                    "path": vp.as_str(),
                    "blob_id": bid.to_hex(),
                    "importance": meta.importance,
                    "tags": meta.tags,
                    "updated_at_ms": meta.updated_at_ms,
                })
            })
            .collect();
        json_to_py(py, &json!(arr))
    }

    /// Commits staged workspace changes into an immutable Merkle Tree snapshot.
    #[pyo3(signature = (message, branch="main", author="python-sdk"))]
    pub fn commit(&self, message: &str, branch: &str, author: &str) -> PyResult<String> {
        let cid = self.vcs.commit(branch, author, message).map_err(to_py_err)?;
        Ok(cid.to_hex())
    }

    /// Returns up to `limit` commits in reverse-chronological order for `branch`.
    #[pyo3(signature = (branch="main", limit=20))]
    pub fn log<'py>(
        &self,
        py: Python<'py>,
        branch: &str,
        limit: usize,
    ) -> PyResult<Bound<'py, PyAny>> {
        let commits = self.vcs.log(branch, limit).map_err(to_py_err)?;
        let val = serde_json::to_value(commits).map_err(to_py_err)?;
        json_to_py(py, &val)
    }

    /// Creates a zero-copy branch pointing to `from_rev` (branch name or commit hex).
    #[pyo3(signature = (name, from_rev="main"))]
    pub fn create_branch(&self, name: &str, from_rev: &str) -> PyResult<()> {
        let rev = if let Ok(cid) = CommitId::from_hex(from_rev) {
            Revision::Commit(cid)
        } else {
            Revision::BranchHead(from_rev.to_string())
        };
        self.vcs.create_branch(name, &rev).map_err(to_py_err)
    }

    /// Lists all branches and their head commit hashes.
    pub fn branches<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let refs = self.vcs.store().list_refs().map_err(to_py_err)?;
        let arr: Vec<Value> = refs
            .into_iter()
            .map(|(branch, cid)| json!({ "branch": branch, "commit_id": cid.to_hex() }))
            .collect();
        json_to_py(py, &json!(arr))
    }

    /// Deletes a non-main branch reference and clears its workspace.
    pub fn delete_branch(&self, name: &str) -> PyResult<()> {
        self.vcs.delete_branch(name).map_err(to_py_err)
    }

    /// Atomically rewinds `branch` to `commit_id` and discards uncommitted workspace changes.
    #[pyo3(signature = (commit_id, branch="main"))]
    pub fn rewind(&self, commit_id: &str, branch: &str) -> PyResult<()> {
        let cid = CommitId::from_hex(commit_id).map_err(to_py_err)?;
        self.vcs.rewind(branch, &cid).map_err(to_py_err)
    }

    /// Computes structured Merkle diffs between `base` and `target` (branches or commit hashes).
    pub fn diff<'py>(
        &self,
        py: Python<'py>,
        base: &str,
        target: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        let base_rev = parse_revision(base);
        let target_rev = parse_revision(target);
        let diffs = self.vcs.diff(&base_rev, &target_rev).map_err(to_py_err)?;
        let arr: Vec<Value> = diffs
            .into_iter()
            .map(|d| {
                let kind = match d.kind {
                    DiffKind::Added => "added",
                    DiffKind::Modified => "modified",
                    DiffKind::Deleted => "deleted",
                };
                json!({
                    "path": d.path.as_str(),
                    "kind": kind,
                    "old_blob_id": d.old_blob_id.map(|b| b.to_hex()),
                    "new_blob_id": d.new_blob_id.map(|b| b.to_hex()),
                })
            })
            .collect();
        json_to_py(py, &json!(arr))
    }

    /// Three-way merges `source` into `target` using `strategy` (`auto` | `ours` | `theirs`).
    #[pyo3(signature = (source, target="main", strategy="auto"))]
    pub fn merge<'py>(
        &self,
        py: Python<'py>,
        source: &str,
        target: &str,
        strategy: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        let strat = match strategy {
            "ours" => MergeStrategy::Ours,
            "theirs" => MergeStrategy::Theirs,
            "auto" => MergeStrategy::Auto,
            other => {
                return Err(PyValueError::new_err(format!(
                    "invalid merge strategy '{other}'"
                )))
            }
        };
        let outcome = self.vcs.merge(source, target, strat).map_err(to_py_err)?;
        let val = match outcome {
            MergeOutcome::FastForward(cid) => {
                json!({ "outcome": "fast_forward", "commit_id": cid.to_hex() })
            }
            MergeOutcome::Merged(cid) => {
                json!({ "outcome": "merged", "commit_id": cid.to_hex() })
            }
            MergeOutcome::Conflict {
                base_commit,
                conflicts,
            } => json!({
                "outcome": "conflict",
                "base_commit": base_commit.to_hex(),
                "conflicts": conflicts,
            }),
        };
        json_to_py(py, &val)
    }

    /// Executes branch-isolated Graph-RRF hybrid search (BM25 + vector + 1-hop Wiki graph).
    #[pyo3(signature = (query, branch="main", prefix=None, top_k=10))]
    pub fn search<'py>(
        &self,
        py: Python<'py>,
        query: &str,
        branch: &str,
        prefix: Option<&str>,
        top_k: usize,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mut req =
            SearchRequest::new(Revision::BranchWorkspace(branch.to_string()), query).with_top_k(top_k);
        if let Some(raw_prefix) = prefix.filter(|s| !s.trim().is_empty()) {
            req = req.with_prefix(VirtualPath::new(raw_prefix).map_err(to_py_err)?);
        }
        let hits = self.search.search(&req).map_err(to_py_err)?;
        let val = serde_json::to_value(hits).map_err(to_py_err)?;
        json_to_py(py, &val)
    }

    /// Compiles pinned `/core` memories into a `<core_memory>` system prompt block in `< 5ms`.
    #[pyo3(signature = (branch="main", max_tokens=2000))]
    pub fn compile_core<'py>(
        &self,
        py: Python<'py>,
        branch: &str,
        max_tokens: usize,
    ) -> PyResult<Bound<'py, PyAny>> {
        let compiled = self
            .search
            .compile_core(&Revision::BranchWorkspace(branch.to_string()), max_tokens)
            .map_err(to_py_err)?;
        let val = serde_json::to_value(compiled).map_err(to_py_err)?;
        json_to_py(py, &val)
    }

    /// Ingests unstructured PM/Dev/Bot text, weaves `[[Wikilinks]]`, and optionally isolates into `draft/*`.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (text, role="dev", branch="main", draft=false, title=None, path=None, tier=None, tags=None, importance=None, commit=true, use_llm=false))]
    pub fn ingest<'py>(
        &self,
        py: Python<'py>,
        text: &str,
        role: &str,
        branch: &str,
        draft: bool,
        title: Option<&str>,
        path: Option<&str>,
        tier: Option<&str>,
        tags: Option<Vec<String>>,
        importance: Option<f32>,
        commit: bool,
        use_llm: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let parsed_role = IngestRole::parse(role).map_err(to_py_err)?;
        let mut req = IngestRequest::new(parsed_role, text)
            .with_branch(branch)
            .with_draft(draft)
            .with_commit(commit)
            .with_use_llm(use_llm);
        if let Some(t) = title {
            req = req.with_title(t);
        }
        if let Some(p) = path {
            let vp = VirtualPath::new(p).map_err(to_py_err)?;
            vp.validate_file_path().map_err(to_py_err)?;
            req = req.with_path(vp);
        }
        if let Some(tr) = tier {
            let parsed_tier = MemoryTier::from_segment(tr)
                .ok_or_else(|| PyValueError::new_err(format!("unknown tier '{tr}'")))?;
            req = req.with_tier(parsed_tier);
        }
        if let Some(tg) = tags {
            req = req.with_tags(tg);
        }
        if let Some(imp) = importance {
            req = req.with_importance(imp);
        }
        let outcome = self
            .search
            .ingest(&req, Some(&self.llm_cfg))
            .map_err(to_py_err)?;
        let val = serde_json::to_value(outcome).map_err(to_py_err)?;
        json_to_py(py, &val)
    }

    /// Runs cold-path sleep consolidation on an isolated `consolidate/*` branch.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (branch="main", consolidation_branch=None, prefix="/episodic", max_notes=50, min_importance=0.0, use_llm=false, auto_merge=false, strategy="auto", archive_processed=false, delete_branch=false, dry_run=false))]
    pub fn consolidate<'py>(
        &self,
        py: Python<'py>,
        branch: &str,
        consolidation_branch: Option<&str>,
        prefix: &str,
        max_notes: usize,
        min_importance: f32,
        use_llm: bool,
        auto_merge: bool,
        strategy: &str,
        archive_processed: bool,
        delete_branch: bool,
        dry_run: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let strat = match strategy {
            "ours" => MergeStrategy::Ours,
            "theirs" => MergeStrategy::Theirs,
            _ => MergeStrategy::Auto,
        };
        let mut req = ConsolidationRequest::new(branch)
            .with_episodic_prefix(VirtualPath::new(prefix).map_err(to_py_err)?)
            .with_max_notes(max_notes)
            .with_min_importance(min_importance)
            .with_use_llm(use_llm)
            .with_auto_merge(auto_merge)
            .with_merge_strategy(strat)
            .with_archive_processed(archive_processed)
            .with_delete_branch_on_merge(delete_branch)
            .with_dry_run(dry_run);
        if let Some(cb) = consolidation_branch {
            req = req.with_consolidation_branch(cb);
        }

        let consolidator =
            ConsolidationEngine::new(self.search.clone()).with_llm_config(self.llm_cfg.clone());
        let report = consolidator.consolidate(&req).map_err(to_py_err)?;
        let val = serde_json::to_value(report).map_err(to_py_err)?;
        json_to_py(py, &val)
    }

    /// Reclaims unreachable commits, trees, blobs, and orphaned FTS5 chunks.
    pub fn gc<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let stats = self.vcs.gc().map_err(to_py_err)?;
        let val = serde_json::to_value(stats).map_err(to_py_err)?;
        json_to_py(py, &val)
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<NativeEngram>()?;
    Ok(())
}
