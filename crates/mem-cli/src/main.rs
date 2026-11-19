//! Unified CLI, health diagnostics (`doctor`), and stdio MCP server (`mcp`) for AI Memory (`mem`).

mod mcp;
mod server_cmd;
mod wizard;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mem_core::{
    CommitId, DiffKind, MemError, MemoryMetadata, MemoryNode, MemoryTier, MergeOutcome,
    MergeStrategy, Result, Revision, VirtualPath,
};
use mem_search::{
    compile_core, export_vault, init_vault_template, parse_wiki_document, ConsolidateLlmConfig,
    EmbeddingProviderConfig, IngestRequest, IngestRole, MemSearchEngine, OpenAiCompatibleEmbedder,
    ProviderBackend, SearchConfig, SearchRequest,
};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};
use serde_json::json;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct GlobalOpts {
    pub db_path: PathBuf,
    pub config_path: PathBuf,
    pub persistent: Option<wizard::PersistentCliConfig>,
    pub provider_override: Option<String>,
    pub json: bool,
    pub use_color: bool,
}

impl GlobalOpts {
    pub fn embedding_config(&self) -> EmbeddingProviderConfig {
        let mut cfg = self
            .persistent
            .as_ref()
            .map(|p| p.mem.embedding.clone())
            .unwrap_or_else(EmbeddingProviderConfig::ollama_default);

        let backend_override = self
            .provider_override
            .as_deref()
            .and_then(ProviderBackend::parse)
            .or_else(|| {
                env::var("MEM_PROVIDER")
                    .ok()
                    .and_then(|v| ProviderBackend::parse(&v))
            });

        if let Some(target_backend) = backend_override {
            if target_backend != cfg.backend {
                let prev_key = cfg.api_key.clone();
                cfg = match target_backend {
                    ProviderBackend::Disabled => EmbeddingProviderConfig::disabled(),
                    ProviderBackend::Ollama => EmbeddingProviderConfig::ollama_default(),
                    ProviderBackend::OpenAi => {
                        let mut prod = EmbeddingProviderConfig::openai_production("");
                        prod.api_key = prev_key.filter(|k| !k.trim().is_empty());
                        prod
                    }
                };
            }
        }

        if let Ok(url) = env::var("MEM_EMBEDDING_BASE_URL") {
            if !url.trim().is_empty() {
                cfg.base_url = url;
            }
        }
        if let Ok(model) = env::var("MEM_EMBEDDING_MODEL") {
            if !model.trim().is_empty() {
                cfg.model = model;
            }
        }
        if let Ok(key) = env::var("OPENAI_API_KEY") {
            if !key.trim().is_empty() && cfg.backend != ProviderBackend::Disabled {
                cfg.api_key = Some(key);
            }
        }
        if let Ok(dim) = env::var("MEM_EMBEDDING_DIMENSIONS") {
            if let Ok(parsed) = dim.trim().parse::<usize>() {
                cfg.dimensions = Some(parsed);
            }
        }

        cfg
    }

    pub fn consolidate_config(&self) -> ConsolidateLlmConfig {
        let mut cfg = self
            .persistent
            .as_ref()
            .map(|p| p.mem.consolidate_llm.clone())
            .unwrap_or_else(ConsolidateLlmConfig::ollama_default);

        let backend_override = self
            .provider_override
            .as_deref()
            .and_then(ProviderBackend::parse)
            .or_else(|| {
                env::var("MEM_PROVIDER")
                    .ok()
                    .and_then(|v| ProviderBackend::parse(&v))
            });

        if let Some(target_backend) = backend_override {
            if target_backend != cfg.backend {
                let prev_key = cfg.api_key.clone();
                cfg = match target_backend {
                    ProviderBackend::Disabled => ConsolidateLlmConfig::disabled(),
                    ProviderBackend::Ollama => ConsolidateLlmConfig::ollama_default(),
                    ProviderBackend::OpenAi => {
                        let mut prod = ConsolidateLlmConfig::openai_production("");
                        prod.api_key = prev_key.filter(|k| !k.trim().is_empty());
                        prod
                    }
                };
            }
        }

        if let Ok(url) = env::var("MEM_LLM_BASE_URL") {
            if !url.trim().is_empty() {
                cfg.base_url = url;
            }
        }
        if let Ok(model) = env::var("MEM_LLM_MODEL") {
            if !model.trim().is_empty() {
                cfg.model = model;
            }
        }
        if let Ok(key) = env::var("OPENAI_API_KEY") {
            if !key.trim().is_empty() && cfg.backend != ProviderBackend::Disabled {
                cfg.api_key = Some(key);
            }
        }

        cfg
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> std::result::Result<ExitCode, String> {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    if raw_args.is_empty() {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }

    let (opts, rest) = parse_global_opts(&raw_args)?;
    if rest.is_empty() {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }

    let subcmd = rest[0].as_str();
    let args = &rest[1..];

    match subcmd {
        "help" => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        "version" => {
            println!("mem {VERSION}");
            Ok(ExitCode::SUCCESS)
        }
        "onboard" | "setup" => wizard::cmd_onboard(&opts, args),
        "config" => wizard::cmd_config(&opts, args),
        "completions" => wizard::cmd_completions(args),
        "init" => cmd_init(&opts),
        "status" => cmd_status(&opts, args),
        "write" => cmd_write(&opts, args),
        "read" => cmd_read(&opts, args),
        "ls" | "list" => cmd_ls(&opts, args),
        "commit" => cmd_commit(&opts, args),
        "log" => cmd_log(&opts, args),
        "branch" => cmd_branch(&opts, args),
        "rewind" => cmd_rewind(&opts, args),
        "diff" => cmd_diff(&opts, args),
        "merge" => cmd_merge(&opts, args),
        "search" => cmd_search(&opts, args),
        "compile" => cmd_compile(&opts, args),
        "ingest" => cmd_ingest(&opts, args),
        "vault" => cmd_vault(&opts, args),
        "gc" => cmd_gc(&opts),
        "doctor" => cmd_doctor(&opts),
        "serve" => server_cmd::cmd_serve(&opts, args),
        "admin" => server_cmd::cmd_admin(args),
        "mcp" => match args.first().map(String::as_str) {
            Some("install") => wizard::cmd_mcp_install(&opts, &args[1..]),
            None | Some("serve") => {
                let emb_cfg = opts.embedding_config();
                let llm_cfg = opts.consolidate_config();
                mcp::run_stdio_server(opts.db_path.clone(), emb_cfg, llm_cfg)
                    .map_err(|e| e.to_string())?;
                Ok(ExitCode::SUCCESS)
            }
            Some(other) => Err(format!(
                "unknown mcp subcommand '{other}'; expected 'serve' or 'install'"
            )),
        },
        other => Err(format!(
            "unknown subcommand '{other}'. Run 'mem --help' for usage."
        )),
    }
}

fn parse_global_opts(raw: &[String]) -> std::result::Result<(GlobalOpts, Vec<String>), String> {
    let mut explicit_db: Option<PathBuf> = None;
    let mut explicit_config: Option<PathBuf> = None;
    let mut provider_override: Option<String> = None;
    let mut json = false;
    let mut rest = Vec::new();
    let mut i = 0usize;

    while i < raw.len() {
        let arg = &raw[i];
        if arg == "--" {
            rest.extend(raw[i + 1..].iter().cloned());
            break;
        }
        if arg == "-h" || arg == "--help" {
            if rest.is_empty() {
                rest.push("help".to_string());
                break;
            }
            rest.push(arg.clone());
        } else if arg == "-V" || arg == "--version" {
            rest.push("version".to_string());
            break;
        } else if arg == "--json" {
            json = true;
        } else if arg == "--db" {
            i += 1;
            let val = raw.get(i).ok_or("--db requires a path argument")?;
            explicit_db = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--db=") {
            explicit_db = Some(PathBuf::from(val));
        } else if arg == "--config" {
            i += 1;
            let val = raw.get(i).ok_or("--config requires a path argument")?;
            explicit_config = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--config=") {
            explicit_config = Some(PathBuf::from(val));
        } else if arg == "--provider" {
            i += 1;
            let val = raw
                .get(i)
                .ok_or("--provider requires ollama|openai|disabled")?;
            provider_override = Some(val.clone());
        } else if let Some(val) = arg.strip_prefix("--provider=") {
            provider_override = Some(val.to_string());
        } else {
            rest.push(arg.clone());
        }
        i += 1;
    }

    let config_path = wizard::resolve_config_path(explicit_config.as_deref());
    let persistent = wizard::load_persistent_config(&config_path);

    let db_path = explicit_db
        .or_else(|| {
            env::var("MEM_DB_PATH")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| {
            persistent
                .as_ref()
                .and_then(|p| p.db_path.as_ref())
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| wizard::mem_home_dir().join("memory.db"));

    let use_color = should_use_color(json);
    Ok((
        GlobalOpts {
            db_path,
            config_path,
            persistent,
            provider_override,
            json,
            use_color,
        },
        rest,
    ))
}

fn should_use_color(json: bool) -> bool {
    !json && env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal()
}

pub(crate) fn paint(enabled: bool, code: &str, text: &str) -> String {
    if enabled {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn open_existing_vcs(db_path: &Path) -> std::result::Result<(SqliteStore, MemVcs), String> {
    if !db_path.exists() {
        return Err(format!(
            "memory database does not exist at '{}'. Run 'mem --db {} init' first.",
            db_path.display(),
            db_path.display()
        ));
    }
    let store = SqliteStore::open(db_path).map_err(|e| e.to_string())?;
    let vcs = MemVcs::new(store.clone());
    Ok((store, vcs))
}

/// Builds a [`MemSearchEngine`] that cleanly degrades to Level-1 BM25 + Wiki Graph if local Ollama is offline.
pub(crate) fn build_search_engine(
    vcs: MemVcs,
    emb_cfg: &EmbeddingProviderConfig,
) -> (MemSearchEngine, Option<String>) {
    match emb_cfg.backend {
        ProviderBackend::Disabled => (MemSearchEngine::new_bm25_only(vcs), None),
        ProviderBackend::Ollama => {
            if is_http_endpoint_reachable(&emb_cfg.base_url, Duration::from_millis(400)) {
                let embedder = Arc::new(OpenAiCompatibleEmbedder::new(emb_cfg.clone()));
                (
                    MemSearchEngine::new(vcs, embedder, SearchConfig::default()),
                    None,
                )
            } else {
                let notice = format!(
                    "Ollama endpoint '{}' is unreachable; falling back to Level-1 FTS5 BM25 + Obsidian Wiki Graph.",
                    emb_cfg.base_url
                );
                (MemSearchEngine::new_bm25_only(vcs), Some(notice))
            }
        }
        ProviderBackend::OpenAi => {
            let has_key = emb_cfg
                .api_key
                .as_deref()
                .is_some_and(|k| !k.trim().is_empty());
            if has_key {
                let embedder = Arc::new(OpenAiCompatibleEmbedder::new(emb_cfg.clone()));
                (
                    MemSearchEngine::new(vcs, embedder, SearchConfig::default()),
                    None,
                )
            } else {
                let notice = "OPENAI_API_KEY is not set; falling back to Level-1 FTS5 BM25 + Obsidian Wiki Graph.".to_string();
                (MemSearchEngine::new_bm25_only(vcs), Some(notice))
            }
        }
    }
}

pub(crate) fn is_http_endpoint_reachable(base_url: &str, timeout: Duration) -> bool {
    let without_scheme = base_url
        .trim()
        .strip_prefix("http://")
        .or_else(|| base_url.trim().strip_prefix("https://"))
        .unwrap_or(base_url.trim());
    let host_port = without_scheme.split('/').next().unwrap_or("");
    if host_port.is_empty() {
        return false;
    }
    let addr = if host_port.contains(':') {
        host_port.to_string()
    } else if base_url.trim().starts_with("https://") {
        format!("{host_port}:443")
    } else {
        format!("{host_port}:80")
    };

    let Ok(mut addrs) = addr.to_socket_addrs() else {
        return false;
    };
    let Some(sock) = addrs.next() else {
        return false;
    };
    TcpStream::connect_timeout(&sock, timeout).is_ok()
}

fn parse_revision(raw: &str) -> Revision {
    if let Ok(cid) = CommitId::from_hex(raw) {
        Revision::Commit(cid)
    } else {
        Revision::BranchWorkspace(raw.to_string())
    }
}

fn cmd_init(opts: &GlobalOpts) -> std::result::Result<ExitCode, String> {
    if let Some(parent) = opts.db_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "failed to create database directory '{}': {e}",
                    parent.display()
                )
            })?;
        }
    }

    let store = SqliteStore::open(&opts.db_path).map_err(|e| e.to_string())?;
    let vcs = MemVcs::new(store);
    let root_commit = vcs.init().map_err(|e| e.to_string())?;

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "db_path": opts.db_path.display().to_string(),
                "branch": "main",
                "root_commit_id": root_commit.to_hex()
            }))
            .unwrap()
        );
    } else {
        let ok = paint(opts.use_color, "32;1", "Initialized");
        println!(
            "{ok} AI Memory repository at {}\n  branch: main\n  commit: {}",
            opts.db_path.display(),
            root_commit.to_hex()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_status(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut i = 0usize;
    while i < args.len() {
        if args[i] == "--branch" || args[i] == "-b" {
            i += 1;
            branch = args.get(i).ok_or("--branch requires a branch name")?.clone();
        } else {
            return Err(format!("unexpected argument '{}' in status", args[i]));
        }
        i += 1;
    }

    let (store, vcs) = open_existing_vcs(&opts.db_path)?;
    let head = vcs.head_commit(&branch).map_err(|e| e.to_string())?;
    let ws = store.workspace_list(&branch).map_err(|e| e.to_string())?;
    let active = vcs
        .list(&Revision::BranchWorkspace(branch.clone()), None)
        .map_err(|e| e.to_string())?;
    let refs = store.list_refs().map_err(|e| e.to_string())?;

    let mut tier_counts: HashMap<&'static str, usize> = HashMap::new();
    for (p, _, _) in &active {
        *tier_counts.entry(p.tier().as_str()).or_insert(0) += 1;
    }

    let emb_cfg = opts.embedding_config();
    let llm_cfg = opts.consolidate_config();
    let blob_count = store.blob_count().map_err(|e| e.to_string())?;
    let tree_count = store.tree_count().map_err(|e| e.to_string())?;
    let commit_count = store.commit_count().map_err(|e| e.to_string())?;

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "db_path": opts.db_path.display().to_string(),
                "branch": branch,
                "head_commit": head.to_hex(),
                "branches": refs.len(),
                "staged_changes": ws.len(),
                "active_nodes": active.len(),
                "tiers": tier_counts,
                "storage": {
                    "blobs": blob_count,
                    "trees": tree_count,
                    "commits": commit_count,
                },
                "embedding_provider": emb_cfg,
                "consolidate_llm": llm_cfg,
            }))
            .unwrap()
        );
    } else {
        println!(
            "{} ({})",
            paint(opts.use_color, "1", "AI Memory Status"),
            opts.db_path.display()
        );
        println!("  Branch      : {} (HEAD {})", paint(opts.use_color, "36", &branch), &head.to_hex()[..12]);
        println!("  Total Refs  : {}", refs.len());
        println!(
            "  Active Docs : {}  (/core: {}, /semantic: {}, /procedural: {}, /episodic: {})",
            active.len(),
            tier_counts.get("core").copied().unwrap_or(0),
            tier_counts.get("semantic").copied().unwrap_or(0),
            tier_counts.get("procedural").copied().unwrap_or(0),
            tier_counts.get("episodic").copied().unwrap_or(0),
        );
        println!("  CAS Objects : {blob_count} blobs, {tree_count} trees, {commit_count} commits");
        println!(
            "  Providers   : embedding={:?} ({}), llm={:?} ({})",
            emb_cfg.backend,
            if emb_cfg.model.is_empty() { "none" } else { &emb_cfg.model },
            llm_cfg.backend,
            if llm_cfg.model.is_empty() { "none" } else { &llm_cfg.model },
        );
        if ws.is_empty() {
            println!("  Workspace   : clean (no uncommitted edits)");
        } else {
            println!("  Workspace   : {} staged change(s)", ws.len());
            for entry in ws {
                println!("    [{:?}] {}", entry.op, entry.path);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_write(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut explicit_tags: Option<Vec<String>> = None;
    let mut explicit_importance: Option<f32> = None;
    let mut from_file: Option<PathBuf> = None;
    let mut positionals = Vec::new();
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            "--tags" => {
                i += 1;
                let raw = args.get(i).ok_or("--tags requires a comma-separated list")?;
                let list = raw
                    .split(',')
                    .map(|s| s.trim().trim_start_matches('#').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                explicit_tags = Some(list);
            }
            "--importance" => {
                i += 1;
                let raw = args.get(i).ok_or("--importance requires a float in [0,1]")?;
                let val: f32 = raw.parse().map_err(|_| format!("invalid importance '{raw}'"))?;
                explicit_importance = Some(val.clamp(0.0, 1.0));
            }
            "--file" | "-f" => {
                i += 1;
                let p = args.get(i).ok_or("--file requires a local file path")?;
                from_file = Some(PathBuf::from(p));
            }
            other => positionals.push(other.to_string()),
        }
        i += 1;
    }

    let raw_vpath = positionals
        .first()
        .ok_or("usage: mem write <virtual_path> <content|-> [--file <local>] [--branch <b>]")?;
    let vpath = VirtualPath::new(raw_vpath).map_err(|e| e.to_string())?;
    vpath.validate_file_path().map_err(|e| e.to_string())?;

    let content_bytes = if let Some(local_path) = from_file {
        fs::read(&local_path)
            .map_err(|e| format!("failed to read '{}': {e}", local_path.display()))?
    } else if let Some(content_arg) = positionals.get(1) {
        if content_arg == "-" {
            let mut buf = Vec::new();
            io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| format!("failed to read stdin: {e}"))?;
            buf
        } else {
            content_arg.as_bytes().to_vec()
        }
    } else {
        return Err("missing content argument (or pass '-' / --file <path>)".to_string());
    };

    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let mut meta = MemoryMetadata::default();
    if let Ok(text) = std::str::from_utf8(&content_bytes) {
        let parsed = parse_wiki_document(&vpath, text, 800);
        if !parsed.tags.is_empty() {
            meta.tags = parsed.tags;
        }
        if let Some(imp) = parsed.importance {
            meta.importance = imp;
        }
    }
    if let Some(tags) = explicit_tags {
        meta.tags = tags;
    }
    if let Some(imp) = explicit_importance {
        meta.importance = imp;
    }

    let node = MemoryNode::new(vpath.clone(), content_bytes).with_metadata(meta);
    let blob_id = vcs.write_node(&branch, node).map_err(|e| e.to_string())?;

    let emb_cfg = opts.embedding_config();
    let (engine, notice) = build_search_engine(vcs, &emb_cfg);
    if let Some(msg) = notice {
        if !opts.json {
            eprintln!("note: {msg}");
        }
    }
    let newly_indexed = engine
        .index_blob_if_needed(&vpath, &blob_id)
        .map_err(|e| e.to_string())?;

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "branch": branch,
                "path": vpath.as_str(),
                "blob_id": blob_id.to_hex(),
                "newly_indexed": newly_indexed,
            }))
            .unwrap()
        );
    } else {
        println!(
            "{} {} -> {} (blob {}, {})",
            paint(opts.use_color, "32", "Staged"),
            vpath,
            branch,
            &blob_id.to_hex()[..12],
            if newly_indexed { "indexed" } else { "deduplicated" }
        );
    }

    Ok(ExitCode::SUCCESS)
}

fn cmd_read(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut commit_hex: Option<String> = None;
    let mut path_arg: Option<String> = None;
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            "--commit" | "-c" => {
                i += 1;
                commit_hex = Some(args.get(i).ok_or("--commit requires a commit hash")?.clone());
            }
            other => {
                if path_arg.is_none() {
                    path_arg = Some(other.to_string());
                } else {
                    return Err(format!("unexpected argument '{other}'"));
                }
            }
        }
        i += 1;
    }

    let raw_path = path_arg.ok_or("usage: mem read <virtual_path> [--branch <b>] [--commit <cid>]")?;
    let vpath = VirtualPath::new(raw_path).map_err(|e| e.to_string())?;
    let rev = match commit_hex {
        Some(c) => Revision::Commit(CommitId::from_hex(&c).map_err(|e| e.to_string())?),
        None => Revision::BranchWorkspace(branch),
    };

    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let Some(node) = vcs.read(&rev, &vpath).map_err(|e| e.to_string())? else {
        return Err(format!("memory node not found at {vpath}"));
    };
    let text = node.content_str().map_err(|e| e.to_string())?;

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "path": node.path.as_str(),
                "blob_id": node.blob_id.map(|b| b.to_hex()),
                "metadata": node.metadata,
                "content": text,
            }))
            .unwrap()
        );
    } else {
        println!("{text}");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_ls(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut prefix: Option<VirtualPath> = None;
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            other => {
                prefix = Some(VirtualPath::new(other).map_err(|e| e.to_string())?);
            }
        }
        i += 1;
    }

    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let entries = vcs
        .list(&Revision::BranchWorkspace(branch.clone()), prefix.as_ref())
        .map_err(|e| e.to_string())?;

    if opts.json {
        let arr: Vec<_> = entries
            .iter()
            .map(|(p, b, m)| {
                json!({
                    "path": p.as_str(),
                    "blob_id": b.to_hex(),
                    "importance": m.importance,
                    "tags": m.tags,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap());
    } else if entries.is_empty() {
        println!("(empty)");
    } else {
        for (p, b, m) in entries {
            let tags_str = if m.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", m.tags.join(", "))
            };
            println!(
                "{}  imp={:.2}  {}{}",
                p.as_str(),
                m.importance,
                &b.to_hex()[..10],
                tags_str
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_commit(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut message: Option<String> = None;
    let mut author = env::var("USER").unwrap_or_else(|_| "user".to_string());
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            "-m" | "--message" => {
                i += 1;
                message = Some(args.get(i).ok_or("-m requires a commit message")?.clone());
            }
            "--author" => {
                i += 1;
                author = args.get(i).ok_or("--author requires a name")?.clone();
            }
            other => return Err(format!("unexpected argument '{other}' in commit")),
        }
        i += 1;
    }

    let msg = message.ok_or("usage: mem commit -m <message> [--branch <b>] [--author <name>]")?;
    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let cid = vcs.commit(&branch, &author, &msg).map_err(|e| e.to_string())?;

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "branch": branch,
                "commit_id": cid.to_hex(),
                "author": author,
                "message": msg,
            }))
            .unwrap()
        );
    } else {
        println!(
            "[{} {}] {}",
            paint(opts.use_color, "36", &branch),
            &cid.to_hex()[..12],
            msg
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_log(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut limit = 20usize;
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            "-n" | "--limit" => {
                i += 1;
                let raw = args.get(i).ok_or("-n requires an integer")?;
                limit = raw.parse().map_err(|_| format!("invalid limit '{raw}'"))?;
            }
            other => return Err(format!("unexpected argument '{other}' in log")),
        }
        i += 1;
    }

    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let commits = vcs.log(&branch, limit).map_err(|e| e.to_string())?;

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&commits).unwrap());
    } else {
        for c in commits {
            println!(
                "{}  {}  ({})  {}",
                paint(opts.use_color, "33", &c.commit_id.to_hex()[..12]),
                c.created_at_ms,
                c.author,
                c.message
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_branch(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut from_raw: Option<String> = None;
    let mut delete_name: Option<String> = None;
    let mut positionals = Vec::new();
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--from" => {
                i += 1;
                from_raw = Some(args.get(i).ok_or("--from requires a branch or commit")?.clone());
            }
            "-d" | "--delete" => {
                i += 1;
                delete_name = Some(args.get(i).ok_or("-d requires a branch name")?.clone());
            }
            other => positionals.push(other.to_string()),
        }
        i += 1;
    }

    let (store, vcs) = open_existing_vcs(&opts.db_path)?;

    if let Some(del) = delete_name {
        vcs.delete_branch(&del).map_err(|e| e.to_string())?;
        if opts.json {
            println!("{}", json!({ "deleted": del }));
        } else {
            println!("Deleted branch '{del}'");
        }
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(new_branch) = positionals.first() {
        let rev = match from_raw {
            Some(r) => parse_revision(&r),
            None => Revision::BranchHead("main".to_string()),
        };
        vcs.create_branch(new_branch, &rev)
            .map_err(|e| e.to_string())?;
        if opts.json {
            println!("{}", json!({ "created": new_branch }));
        } else {
            println!("Created zero-copy branch '{new_branch}'");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let refs = store.list_refs().map_err(|e| e.to_string())?;
    if opts.json {
        let out: Vec<_> = refs
            .iter()
            .map(|(n, c)| json!({ "branch": n, "commit_id": c.to_hex() }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        for (name, cid) in refs {
            println!("{name:<24} {}", &cid.to_hex()[..12]);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_rewind(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut commit_arg: Option<String> = None;
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            other => {
                if commit_arg.is_none() {
                    commit_arg = Some(other.to_string());
                } else {
                    return Err(format!("unexpected argument '{other}'"));
                }
            }
        }
        i += 1;
    }

    let cid_raw = commit_arg.ok_or("usage: mem rewind <commit_id> [--branch <b>]")?;
    let cid = CommitId::from_hex(&cid_raw).map_err(|e| e.to_string())?;
    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    vcs.rewind(&branch, &cid).map_err(|e| e.to_string())?;

    if opts.json {
        println!("{}", json!({ "branch": branch, "rewound_to": cid.to_hex() }));
    } else {
        println!(
            "{} '{}' -> {}",
            paint(opts.use_color, "33;1", "Rewound"),
            branch,
            &cid.to_hex()[..12]
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_diff(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    if args.len() != 2 {
        return Err("usage: mem diff <base_branch_or_commit> <target_branch_or_commit>".to_string());
    }
    let base = parse_revision(&args[0]);
    let target = parse_revision(&args[1]);

    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let diffs = vcs.diff(&base, &target).map_err(|e| e.to_string())?;

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&diffs).unwrap());
    } else if diffs.is_empty() {
        println!("(no differences)");
    } else {
        for d in diffs {
            let line = match d.kind {
                DiffKind::Added => paint(opts.use_color, "32", &format!("+ {}", d.path)),
                DiffKind::Modified => paint(opts.use_color, "33", &format!("~ {}", d.path)),
                DiffKind::Deleted => paint(opts.use_color, "31", &format!("- {}", d.path)),
            };
            println!("{line}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_merge(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut strategy = MergeStrategy::Auto;
    let mut positionals = Vec::new();
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--strategy" | "-s" => {
                i += 1;
                let s = args.get(i).ok_or("--strategy requires auto|ours|theirs")?;
                strategy = match s.as_str() {
                    "auto" => MergeStrategy::Auto,
                    "ours" => MergeStrategy::Ours,
                    "theirs" => MergeStrategy::Theirs,
                    _ => return Err(format!("invalid merge strategy '{s}'")),
                };
            }
            other => positionals.push(other.to_string()),
        }
        i += 1;
    }

    if positionals.len() != 2 {
        return Err("usage: mem merge <source_branch> <target_branch> [--strategy auto|ours|theirs]".to_string());
    }

    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let outcome = vcs
        .merge(&positionals[0], &positionals[1], strategy)
        .map_err(|e| e.to_string())?;

    match outcome {
        MergeOutcome::FastForward(cid) => {
            if opts.json {
                println!("{}", json!({ "outcome": "fast_forward", "commit_id": cid.to_hex() }));
            } else {
                println!("Fast-forward merged into '{}' ({})", positionals[1], &cid.to_hex()[..12]);
            }
            Ok(ExitCode::SUCCESS)
        }
        MergeOutcome::Merged(cid) => {
            if opts.json {
                println!("{}", json!({ "outcome": "merged", "commit_id": cid.to_hex() }));
            } else {
                println!("Merged '{}' into '{}' ({})", positionals[0], positionals[1], &cid.to_hex()[..12]);
            }
            Ok(ExitCode::SUCCESS)
        }
        MergeOutcome::Conflict {
            base_commit,
            conflicts,
        } => {
            if opts.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "outcome": "conflict",
                        "base_commit": base_commit.to_hex(),
                        "conflicts": conflicts
                    }))
                    .unwrap()
                );
            } else {
                println!(
                    "{} Merge aborted with {} conflict(s) (LCA {}):",
                    paint(opts.use_color, "31;1", "CONFLICT"),
                    conflicts.len(),
                    &base_commit.to_hex()[..12]
                );
                for c in conflicts {
                    println!("  ! {}", c.path);
                }
                println!("Re-run with '--strategy ours' or '--strategy theirs' to resolve.");
            }
            Ok(ExitCode::from(2))
        }
    }
}

fn cmd_search(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut prefix: Option<VirtualPath> = None;
    let mut top_k = 10usize;
    let mut query_parts = Vec::new();
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            "--prefix" | "-p" => {
                i += 1;
                let raw = args.get(i).ok_or("--prefix requires a virtual path")?;
                prefix = Some(VirtualPath::new(raw).map_err(|e| e.to_string())?);
            }
            "-k" | "--top-k" => {
                i += 1;
                let raw = args.get(i).ok_or("-k requires an integer")?;
                top_k = raw.parse().map_err(|_| format!("invalid top_k '{raw}'"))?;
            }
            other => query_parts.push(other.to_string()),
        }
        i += 1;
    }

    let query = query_parts.join(" ");
    if query.trim().is_empty() {
        return Err("usage: mem search <query> [--branch <b>] [--prefix <p>] [-k <top_k>]".to_string());
    }

    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let emb_cfg = opts.embedding_config();
    let (engine, notice) = build_search_engine(vcs.clone(), &emb_cfg);
    if let Some(msg) = notice {
        if !opts.json {
            eprintln!("note: {msg}");
        }
    }

    let mut req = SearchRequest::new(Revision::BranchWorkspace(branch), query.clone()).with_top_k(top_k);
    if let Some(p) = prefix {
        req = req.with_prefix(p);
    }

    let t0 = Instant::now();
    let hits = match engine.search(&req) {
        Ok(h) => h,
        Err(err) if emb_cfg.backend == ProviderBackend::Ollama => {
            if !opts.json {
                eprintln!("note: Ollama embedding call failed ({err}); falling back to Level-1 BM25 + Wiki Graph.");
            }
            let fallback = MemSearchEngine::new_bm25_only(vcs);
            fallback.search(&req).map_err(|e| e.to_string())?
        }
        Err(err) => return Err(err.to_string()),
    };
    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&hits).unwrap());
    } else if hits.is_empty() {
        println!("No results for \"{query}\" ({elapsed_ms:.2}ms)");
    } else {
        println!(
            "{} result(s) for \"{query}\" ({elapsed_ms:.2}ms)\n",
            hits.len()
        );
        for (idx, h) in hits.iter().enumerate() {
            println!(
                "{}. {}  {}",
                idx + 1,
                paint(opts.use_color, "36;1", h.path.as_str()),
                paint(opts.use_color, "90", &format!("(score={:.4}, lines {}-{})", h.score, h.start_line, h.end_line))
            );
            println!("   Title   : {}  |  Section: {}", h.title, h.heading_path);
            let preview: String = h
                .snippet
                .lines()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(140)
                .collect();
            if !preview.is_empty() {
                println!("   Snippet : {preview}");
            }
            if !h.outlinks.is_empty() {
                let outs: Vec<&str> = h.outlinks.iter().map(VirtualPath::as_str).collect();
                println!("   Outlinks: {}", outs.join(", "));
            }
            if !h.backlinks.is_empty() {
                let backs: Vec<&str> = h.backlinks.iter().map(VirtualPath::as_str).collect();
                println!("   Backlinks: {}", backs.join(", "));
            }
            println!();
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_compile(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut max_tokens = 2000usize;
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            "--max-tokens" => {
                i += 1;
                let raw = args.get(i).ok_or("--max-tokens requires an integer")?;
                max_tokens = raw.parse().map_err(|_| format!("invalid max_tokens '{raw}'"))?;
            }
            other => return Err(format!("unexpected argument '{other}' in compile")),
        }
        i += 1;
    }

    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let t0 = Instant::now();
    let compiled =
        compile_core(&vcs, &Revision::BranchWorkspace(branch), max_tokens).map_err(|e| e.to_string())?;
    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "elapsed_ms": elapsed_ms,
                "estimated_tokens": compiled.estimated_tokens,
                "included_paths": compiled.included_paths,
                "skipped_paths": compiled.skipped_paths,
                "rendered": compiled.rendered,
            }))
            .unwrap()
        );
    } else {
        eprintln!(
            "[compile_core] {:.2}ms | ~{} tokens | {} included, {} skipped",
            elapsed_ms,
            compiled.estimated_tokens,
            compiled.included_paths.len(),
            compiled.skipped_paths.len()
        );
        println!("{}", compiled.rendered);
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_ingest(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut role = IngestRole::Dev;
    let mut branch = "main".to_string();
    let mut draft = false;
    let mut title: Option<String> = None;
    let mut path_arg: Option<VirtualPath> = None;
    let mut tier: Option<MemoryTier> = None;
    let mut tags: Vec<String> = Vec::new();
    let mut file_arg: Option<String> = None;
    let mut author: Option<String> = None;
    let mut importance: Option<f32> = None;
    let mut commit = true;
    let mut commit_message: Option<String> = None;
    let mut use_llm = false;
    let mut text_parts: Vec<String> = Vec::new();
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--role" | "-r" => {
                i += 1;
                let raw = args.get(i).ok_or("--role requires pm | dev | bot")?;
                role = IngestRole::parse(raw).map_err(|e| e.to_string())?;
            }
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            "--draft" => {
                draft = true;
            }
            "--title" => {
                i += 1;
                title = Some(args.get(i).ok_or("--title requires a string")?.clone());
            }
            "--path" | "-p" => {
                i += 1;
                let raw = args.get(i).ok_or("--path requires a VFS path")?;
                let vp = VirtualPath::new(raw).map_err(|e| e.to_string())?;
                vp.validate_file_path().map_err(|e| e.to_string())?;
                path_arg = Some(vp);
            }
            "--tier" | "-t" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or("--tier requires core|episodic|semantic|procedural")?;
                tier = Some(
                    MemoryTier::from_segment(raw)
                        .ok_or_else(|| format!("unknown tier '{raw}'"))?,
                );
            }
            "--tag" => {
                i += 1;
                let raw = args.get(i).ok_or("--tag requires a tag name")?;
                for t in raw.split(',') {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        tags.push(trimmed.to_string());
                    }
                }
            }
            "--file" | "-f" => {
                i += 1;
                file_arg = Some(args.get(i).ok_or("--file requires a local path")?.clone());
            }
            "--author" => {
                i += 1;
                author = Some(args.get(i).ok_or("--author requires a name")?.clone());
            }
            "--importance" => {
                i += 1;
                let raw = args.get(i).ok_or("--importance requires a float 0.0..1.0")?;
                importance = Some(
                    raw.parse::<f32>()
                        .map_err(|_| format!("invalid --importance '{raw}'"))?,
                );
            }
            "--no-commit" => {
                commit = false;
            }
            "-m" | "--message" => {
                i += 1;
                commit_message = Some(args.get(i).ok_or("-m requires a commit message")?.clone());
            }
            "--llm" => {
                use_llm = true;
            }
            other => {
                text_parts.push(other.to_string());
            }
        }
        i += 1;
    }

    let raw_text = if let Some(local_path) = file_arg {
        fs::read_to_string(&local_path)
            .map_err(|e| format!("failed to read '{local_path}': {e}"))?
    } else if text_parts.len() == 1 && text_parts[0] == "-" {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("failed to read stdin: {e}"))?;
        buf
    } else if !text_parts.is_empty() {
        text_parts.join(" ")
    } else if !io::stdin().is_terminal() {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("failed to read stdin: {e}"))?;
        buf
    } else {
        return Err(
            "usage: mem ingest [--role pm|dev|bot] [--draft] [--title <T>] [--path <VFS>] [--tier <T>] [--tag <TAG>] [--file <PATH>] [--llm] <TEXT | ->"
                .to_string(),
        );
    };

    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let emb_cfg = opts.embedding_config();
    let llm_cfg = opts.consolidate_config();
    let (engine, notice) = build_search_engine(vcs, &emb_cfg);
    if let Some(msg) = notice {
        if !opts.json {
            eprintln!("note: {msg}");
        }
    }

    let mut req = IngestRequest::new(role, raw_text)
        .with_branch(branch)
        .with_draft(draft)
        .with_commit(commit)
        .with_use_llm(use_llm);
    if let Some(t) = title {
        req = req.with_title(t);
    }
    if let Some(p) = path_arg {
        req = req.with_path(p);
    }
    if let Some(tr) = tier {
        req = req.with_tier(tr);
    }
    if !tags.is_empty() {
        req = req.with_tags(tags);
    }
    if let Some(a) = author {
        req = req.with_author(a);
    }
    if let Some(imp) = importance {
        req = req.with_importance(imp);
    }
    if let Some(msg) = commit_message {
        req = req.with_commit_message(msg);
    }

    let outcome = engine
        .ingest(&req, Some(&llm_cfg))
        .map_err(|e| e.to_string())?;

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&outcome).unwrap());
    } else {
        println!(
            "{} [{}] {} -> {} (branch: {})",
            paint(opts.use_color, "32;1", "Ingested"),
            outcome.role.as_str(),
            outcome.title,
            outcome.path,
            outcome.branch
        );
        if let Some(cid) = outcome.commit_id {
            println!("  commit : {}", &cid.to_hex()[..12]);
        }
        println!(
            "  mode   : {}",
            if outcome.used_llm {
                format!("LLM ({})", llm_cfg.model)
            } else {
                "deterministic Obsidian weaver".to_string()
            }
        );
        if !outcome.linked_notes.is_empty() {
            let links = outcome
                .linked_notes
                .iter()
                .map(|l| format!("[[{l}]]"))
                .collect::<Vec<_>>()
                .join(", ");
            println!("  links  : {links}");
        }
        if draft {
            println!(
                "  review : mem diff main {} && mem merge {} main",
                outcome.branch, outcome.branch
            );
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn cmd_vault(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "init-template" => cmd_vault_init_template(opts, &args[1..]),
        "import" => cmd_vault_import(opts, &args[1..]),
        "sync" => cmd_vault_sync(opts, &args[1..]),
        "export" => cmd_vault_export(opts, &args[1..]),
        _ => Err(
            "usage: mem vault <init-template|import|sync|export> <local_dir> [options]".to_string(),
        ),
    }
}

fn cmd_vault_init_template(
    opts: &GlobalOpts,
    args: &[String],
) -> std::result::Result<ExitCode, String> {
    let vault_dir = args
        .first()
        .ok_or("usage: mem vault init-template <local_dir>")?;
    let stats = init_vault_template(Path::new(vault_dir)).map_err(|e| e.to_string())?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&stats).unwrap());
    } else {
        println!(
            "{} collaborative Obsidian Vault template at {}\n  created: {} file(s), preserved existing: {} file(s)\n  next   : edit notes in Obsidian, then run 'mem vault sync {} --commit'",
            paint(opts.use_color, "32;1", "Scaffolded"),
            stats.vault_dir,
            stats.files_created,
            stats.files_skipped,
            vault_dir
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_vault_import(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut tier = MemoryTier::Semantic;
    let mut commit = false;
    let mut message: Option<String> = None;
    let mut dir_arg: Option<String> = None;
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            "--tier" | "-t" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or("--tier requires core|episodic|semantic|procedural")?;
                tier = MemoryTier::from_segment(raw)
                    .ok_or_else(|| format!("unknown tier '{raw}'"))?;
            }
            "--commit" => {
                commit = true;
            }
            "-m" | "--message" => {
                i += 1;
                message = Some(args.get(i).ok_or("-m requires a commit message")?.clone());
            }
            other => {
                if dir_arg.is_none() {
                    dir_arg = Some(other.to_string());
                } else {
                    return Err(format!("unexpected argument '{other}'"));
                }
            }
        }
        i += 1;
    }

    let vault_dir = dir_arg.ok_or("missing <local_dir> for 'mem vault import'")?;
    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let emb_cfg = opts.embedding_config();
    let (engine, notice) = build_search_engine(vcs, &emb_cfg);
    if let Some(msg) = notice {
        if !opts.json {
            eprintln!("note: {msg}");
        }
    }

    let stats = engine
        .import_vault(Path::new(&vault_dir), &branch, tier)
        .map_err(|e| e.to_string())?;
    let commit_id = if commit {
        let msg = message.unwrap_or_else(|| format!("import vault '{vault_dir}' into /{}", tier.as_str()));
        Some(
            engine
                .vcs()
                .commit(&branch, "vault-import@engram", &msg)
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "stats": stats,
                "commit_id": commit_id.map(|c| c.to_hex()),
            }))
            .unwrap()
        );
    } else {
        println!(
            "{} {} markdown file(s) into '{}' (/{})\n  indexed: {} new blob(s), {} deduplicated, {} wiki/tag/entity links extracted",
            paint(opts.use_color, "32;1", "Imported"),
            stats.files_imported,
            branch,
            tier.as_str(),
            stats.blobs_newly_indexed,
            stats.blobs_deduplicated,
            stats.links_extracted
        );
        if let Some(cid) = commit_id {
            println!("  commit : {}", &cid.to_hex()[..12]);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_vault_sync(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut commit = false;
    let mut message: Option<String> = None;
    let mut dir_arg: Option<String> = None;
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            "--commit" => {
                commit = true;
            }
            "-m" | "--message" => {
                i += 1;
                message = Some(args.get(i).ok_or("-m requires a commit message")?.clone());
            }
            other => {
                if dir_arg.is_none() {
                    dir_arg = Some(other.to_string());
                } else {
                    return Err(format!("unexpected argument '{other}'"));
                }
            }
        }
        i += 1;
    }

    let vault_dir = dir_arg.ok_or("usage: mem vault sync <local_dir> [--branch <b>] [--commit]")?;
    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let emb_cfg = opts.embedding_config();
    let (engine, notice) = build_search_engine(vcs, &emb_cfg);
    if let Some(msg) = notice {
        if !opts.json {
            eprintln!("note: {msg}");
        }
    }

    let stats = engine
        .sync_vault(Path::new(&vault_dir), &branch)
        .map_err(|e| e.to_string())?;
    let commit_id = if commit {
        let msg = message.unwrap_or_else(|| format!("sync multi-tier vault '{vault_dir}'"));
        Some(
            engine
                .vcs()
                .commit(&branch, "vault-sync@engram", &msg)
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "stats": stats,
                "commit_id": commit_id.map(|c| c.to_hex()),
            }))
            .unwrap()
        );
    } else {
        println!(
            "{} {} markdown file(s) across all tiers into '{}' ({} skipped)\n  indexed: {} new blob(s), {} deduplicated, {} wiki/tag/entity links extracted",
            paint(opts.use_color, "32;1", "Synced"),
            stats.files_imported,
            branch,
            stats.files_skipped,
            stats.blobs_newly_indexed,
            stats.blobs_deduplicated,
            stats.links_extracted
        );
        if let Some(cid) = commit_id {
            println!("  commit : {}", &cid.to_hex()[..12]);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_vault_export(opts: &GlobalOpts, args: &[String]) -> std::result::Result<ExitCode, String> {
    let mut branch = "main".to_string();
    let mut commit_override: Option<String> = None;
    let mut dir_arg: Option<String> = None;
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--branch" | "-b" => {
                i += 1;
                branch = args.get(i).ok_or("--branch requires a name")?.clone();
            }
            "--commit" => {
                i += 1;
                commit_override = Some(args.get(i).ok_or("--commit requires a hash")?.clone());
            }
            other => {
                if dir_arg.is_none() {
                    dir_arg = Some(other.to_string());
                } else {
                    return Err(format!("unexpected argument '{other}'"));
                }
            }
        }
        i += 1;
    }

    let vault_dir =
        dir_arg.ok_or("usage: mem vault export <local_dir> [--branch <b> | --commit <hash>]")?;
    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let rev = if let Some(cid_raw) = commit_override {
        let cid = CommitId::from_hex(&cid_raw).map_err(|e| e.to_string())?;
        Revision::Commit(cid)
    } else {
        Revision::BranchWorkspace(branch.clone())
    };

    let stats = export_vault(&vcs, &rev, Path::new(&vault_dir)).map_err(|e| e.to_string())?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&stats).unwrap());
    } else {
        println!(
            "{} {} memory node(s) to {}",
            paint(opts.use_color, "32;1", "Exported"),
            stats.files_exported,
            stats.vault_dir
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_gc(opts: &GlobalOpts) -> std::result::Result<ExitCode, String> {
    let (_, vcs) = open_existing_vcs(&opts.db_path)?;
    let stats = vcs.gc().map_err(|e| e.to_string())?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&stats).unwrap());
    } else {
        println!(
            "GC complete: removed {} commits, {} trees, {} blobs ({} bytes reclaimed)",
            stats.removed_commits, stats.removed_trees, stats.removed_blobs, stats.reclaimed_bytes
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_doctor(opts: &GlobalOpts) -> std::result::Result<ExitCode, String> {
    let emb_cfg = opts.embedding_config();
    let llm_cfg = opts.consolidate_config();

    let db_exists = opts.db_path.exists();
    let store_res: Result<SqliteStore> = if db_exists {
        SqliteStore::open(&opts.db_path)
    } else {
        SqliteStore::open_in_memory()
    };

    let (sqlite_ok, fts5_ok, links_ok, core_ms) = match store_res {
        Ok(store) => {
            let wal_check = store.with_conn(|c| {
                let fts: i64 = c
                    .query_row("SELECT COUNT(*) FROM fts_chunks", [], |r| r.get(0))
                    .map_err(|e| MemError::Storage(e.to_string()))?;
                let links: i64 = c
                    .query_row("SELECT COUNT(*) FROM blob_links", [], |r| r.get(0))
                    .map_err(|e| MemError::Storage(e.to_string()))?;
                Ok((fts >= 0, links >= 0))
            });
            let vcs = MemVcs::new(store);
            let _ = vcs.init();
            let t0 = Instant::now();
            let compile_ok = compile_core(&vcs, &Revision::BranchHead("main".into()), 2000).is_ok();
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
            match wal_check {
                Ok((f, l)) => (true, f, l, if compile_ok { Some(elapsed_ms) } else { None }),
                Err(_) => (true, false, false, None),
            }
        }
        Err(_) => (false, false, false, None),
    };

    let (provider_status, provider_detail) = match emb_cfg.backend {
        ProviderBackend::Disabled => (
            "PASS",
            "Level-1 zero-model mode (SQLite FTS5 BM25 + Obsidian Wiki Graph)".to_string(),
        ),
        ProviderBackend::Ollama => {
            if is_http_endpoint_reachable(&emb_cfg.base_url, Duration::from_millis(500)) {
                (
                    "PASS",
                    format!(
                        "Ollama reachable at {} (embed={}, llm={})",
                        emb_cfg.base_url, emb_cfg.model, llm_cfg.model
                    ),
                )
            } else {
                (
                    "WARN",
                    format!(
                        "Ollama not listening at {} (runtime will auto-fallback to Level-1 BM25 + Wiki Graph; start with 'ollama serve && ollama pull {}')",
                        emb_cfg.base_url, emb_cfg.model
                    ),
                )
            }
        }
        ProviderBackend::OpenAi => {
            if emb_cfg
                .api_key
                .as_deref()
                .is_some_and(|k| !k.trim().is_empty())
            {
                (
                    "PASS",
                    format!(
                        "OpenAI API key configured (embed={}, llm={})",
                        emb_cfg.model, llm_cfg.model
                    ),
                )
            } else {
                (
                    "WARN",
                    "MEM_PROVIDER=openai but OPENAI_API_KEY is unset; runtime will fallback to Level-1 BM25 + Wiki Graph".to_string(),
                )
            }
        }
    };

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "db_path": opts.db_path.display().to_string(),
                "db_exists": db_exists,
                "sqlite_ok": sqlite_ok,
                "fts5_ok": fts5_ok,
                "wiki_graph_ok": links_ok,
                "core_compile_ms": core_ms,
                "provider_status": provider_status,
                "provider_detail": provider_detail,
            }))
            .unwrap()
        );
    } else {
        println!("{}", paint(opts.use_color, "1", "AI Memory Doctor"));
        println!(
            "  [{}] Database      : {} ({})",
            if sqlite_ok { paint(opts.use_color, "32", "PASS") } else { paint(opts.use_color, "31", "FAIL") },
            opts.db_path.display(),
            if db_exists { "initialized on disk" } else { "not yet initialized (verified schema in-memory)" }
        );
        println!(
            "  [{}] FTS5 + Graph  : fts_chunks={}, blob_links={}",
            if fts5_ok && links_ok { paint(opts.use_color, "32", "PASS") } else { paint(opts.use_color, "31", "FAIL") },
            fts5_ok,
            links_ok
        );
        println!(
            "  [{}] Provider      : {}",
            match provider_status {
                "PASS" => paint(opts.use_color, "32", "PASS"),
                "WARN" => paint(opts.use_color, "33", "WARN"),
                _ => paint(opts.use_color, "31", "FAIL"),
            },
            provider_detail
        );
        match core_ms {
            Some(ms) => println!(
                "  [{}] /core Compile : {:.2}ms (target < 5.00ms)",
                paint(opts.use_color, "32", "PASS"),
                ms
            ),
            None => println!(
                "  [{}] /core Compile : failed",
                paint(opts.use_color, "31", "FAIL")
            ),
        }
    }

    if sqlite_ok && fts5_ok && links_ok {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

fn print_help() {
    println!(
        r#"mem {VERSION} — Engram: Versioned Structured Memory Engine for AI Agents

USAGE:
  mem [GLOBAL OPTIONS] <COMMAND> [ARGS]

GLOBAL OPTIONS:
  --db <PATH>                 SQLite database path (default: config.json or ~/.ai-memory/memory.db)
  --config <PATH>             Config file path (default: $MEM_CONFIG_PATH or ~/.ai-memory/config.json)
  --provider <MODE>           Override provider: ollama | openai | disabled
  --json                      Output machine-readable JSON
  -h, --help                  Print help information
  -V, --version               Print version

ONBOARDING & CONFIG:
  onboard | setup             Launch Clack-style setup wizard (hardware discovery + MCP wiring)
  config show                 Show persisted config, active providers, and database path
  config use <MODE>           Hot-switch provider: ollama | openai | lite
  mcp install [--client C]    Auto-wire MCP server into Cursor, Claude Code, Claude Desktop, Windsurf
  completions <SHELL>         Print shell tab completions (bash | zsh | fish)

MEMORY & VCS COMMANDS:
  init                        Initialize a new single-file memory repository
  status                      Show branch, active tier counts, staged changes, and providers
  write <PATH> <CONTENT>      Write a memory node (use '-' for stdin or --file <LOCAL>)
  read <PATH>                 Read a memory node from a branch or --commit <HASH>
  ls [PREFIX]                 List active memory paths in a branch
  commit -m <MSG>             Snapshot working workspace into an immutable Merkle commit
  log                         Show commit history DAG
  branch [NAME]               List branches, create a zero-copy branch, or delete (-d)
  rewind <COMMIT_ID>          Atomically reset branch pointer and discard dirty workspace
  diff <BASE> <TARGET>        Compare two branches or commit hashes
  merge <SRC> <DST>           Three-way merge source branch into target branch
  search <QUERY>              Branch-isolated BM25 + Vector + 1-hop Obsidian Wiki search
  compile                     Compile /core memories into <core_memory> block (<5ms)
  ingest [OPTS] <TEXT | ->    Structure PM/Dev/Bot rough notes with auto-[[Wikilinks]] (--draft)
  vault <SUBCMD> <DIR>        Obsidian Vault bridge: init-template | import | sync | export
  gc                          Run mark-and-sweep garbage collection
  doctor                      Verify SQLite FTS5/Wiki Graph, provider connectivity, and latency
  mcp [serve]                 Start stdio JSON-RPC 2.0 MCP Server for Coding Agents
  serve --data D [--listen A]  Start the multi-project HTTP server (prints admin key on first run)
  admin --data D project|key   Manage server projects and API keys (admin, dev, pm, bot)
"#
    );
}
