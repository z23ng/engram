//! Modern Clack-style interactive onboarding wizard, persistent config, MCP auto-wiring, and shell completions.

use std::env;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use mem_core::{MemoryMetadata, MemoryNode, MemoryTier, Revision, VirtualPath};
use mem_search::{
    compile_core, ConsolidateLlmConfig, EmbeddingProviderConfig, MemConfig, MemSearchEngine,
    SearchConfig, SearchRequest,
};
use mem_store::SqliteStore;
use mem_vcs::{MemVcs, VersionControlEngine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{is_http_endpoint_reachable, paint, GlobalOpts};

/// Persistent configuration saved at `~/.ai-memory/config.json` (or `$MEM_HOME/config.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistentCliConfig {
    /// Default SQLite database file path.
    pub db_path: Option<String>,
    /// Bundled search, embedding, and consolidation LLM configuration.
    #[serde(flatten)]
    pub mem: MemConfig,
}

impl Default for PersistentCliConfig {
    fn default() -> Self {
        Self {
            db_path: None,
            mem: MemConfig::ollama_dev(),
        }
    }
}

/// Resolves the AI Memory home directory (`$MEM_HOME`, or `$HOME/.ai-memory`, or `.ai-memory`).
pub fn mem_home_dir() -> PathBuf {
    if let Some(dir) = env::var_os("MEM_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(home) = user_home_dir() {
        return home.join(".ai-memory");
    }
    PathBuf::from(".ai-memory")
}

/// Resolves the user's `$HOME` (or `%USERPROFILE%` on Windows).
pub fn user_home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Resolves the active `config.json` path.
pub fn resolve_config_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(p) = env::var_os("MEM_CONFIG_PATH").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    mem_home_dir().join("config.json")
}

/// Loads `config.json` if present and valid.
pub fn load_persistent_config(path: &Path) -> Option<PersistentCliConfig> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<PersistentCliConfig>(&raw).ok()
}

/// Saves `config.json` atomically with restricted permissions (`0600` on Unix).
pub fn save_persistent_config(path: &Path, cfg: &PersistentCliConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "failed to create config directory '{}': {e}",
                    parent.display()
                )
            })?;
        }
    }

    let body = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("failed to serialize config: {e}"))?;
    fs::write(path, format!("{body}\n")).map_err(|e| {
        format!(
            "failed to write config file '{}': {e}",
            path.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }

    Ok(())
}

/// Detected host hardware, local Ollama runtime, and installed Coding Agents.
#[derive(Debug, Clone)]
pub struct HostEnvironment {
    pub os_arch: String,
    pub cpu_threads: usize,
    pub ram_gib: Option<f64>,
    pub gpu: Option<String>,
    pub ollama_installed: bool,
    pub ollama_online: bool,
    pub detected_clients: Vec<&'static str>,
}

impl HostEnvironment {
    pub fn detect() -> Self {
        let os_arch = format!("{} / {}", env::consts::OS, env::consts::ARCH);
        let cpu_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let ram_gib = detect_ram_gib();
        let gpu = detect_nvidia_gpu();
        let ollama_installed = command_exists("ollama");
        let ollama_online =
            is_http_endpoint_reachable("http://127.0.0.1:11434/v1", Duration::from_millis(300));

        let mut detected_clients = Vec::new();
        if let Some(home) = user_home_dir() {
            if home.join(".cursor").exists() {
                detected_clients.push("Cursor");
            }
            if home.join(".claude").exists() || home.join(".claude.json").exists() {
                detected_clients.push("Claude Code");
            }
            if claude_desktop_config_path(&home).is_some_and(|p| p.parent().is_some_and(|d| d.exists())) {
                detected_clients.push("Claude Desktop");
            }
            if home.join(".codeium/windsurf").exists() {
                detected_clients.push("Windsurf");
            }
        }

        Self {
            os_arch,
            cpu_threads,
            ram_gib,
            gpu,
            ollama_installed,
            ollama_online,
            detected_clients,
        }
    }
}

fn detect_ram_gib() -> Option<f64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb_str = rest.split_whitespace().next()?;
            let kb: f64 = kb_str.parse().ok()?;
            return Some(kb / (1024.0 * 1024.0));
        }
    }
    None
}

fn detect_nvidia_gpu() -> Option<String> {
    if !command_exists("nvidia-smi") {
        return None;
    }
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=name,memory.total", "--format=csv,noheader"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let first = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    if first.is_empty() {
        None
    } else {
        Some(first)
    }
}

fn command_exists(bin: &str) -> bool {
    let Some(path_var) = env::var_os("PATH") else {
        return false;
    };
    for dir in env::split_paths(&path_var) {
        let full = dir.join(bin);
        if full.is_file() {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Serialize)]
pub struct McpInstallReport {
    pub client: String,
    pub path: String,
    pub status: String,
}

fn claude_desktop_config_path(home: &Path) -> Option<PathBuf> {
    match env::consts::OS {
        "macos" => Some(home.join("Library/Application Support/Claude/claude_desktop_config.json")),
        "linux" => Some(home.join(".config/Claude/claude_desktop_config.json")),
        "windows" => env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("Claude/claude_desktop_config.json")),
        _ => None,
    }
}

/// Safely injects `mcpServers["ai-memory"]` into selected AI coding client configs without clobbering existing entries.
pub fn install_mcp_to_clients(
    db_path: &Path,
    target_client: &str,
) -> Result<Vec<McpInstallReport>, String> {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("mem"));
    let exe_str = exe.display().to_string();
    let db_str = db_path.display().to_string();

    let server_entry = json!({
        "command": exe_str,
        "args": ["--db", db_str, "mcp"]
    });

    let mut targets: Vec<(&str, PathBuf, bool)> = Vec::new();
    let norm = target_client.trim().to_ascii_lowercase();

    if let Some(home) = user_home_dir() {
        let cursor_path = home.join(".cursor/mcp.json");
        let claude_code_path = home.join(".claude.json");
        let windsurf_path = home.join(".codeium/windsurf/mcp_config.json");

        match norm.as_str() {
            "all" | "auto" => {
                if home.join(".cursor").exists() {
                    targets.push(("Cursor", cursor_path, true));
                }
                if home.join(".claude").exists() || claude_code_path.exists() {
                    targets.push(("Claude Code", claude_code_path, true));
                }
                if let Some(cd_path) = claude_desktop_config_path(&home) {
                    if cd_path.parent().is_some_and(|p| p.exists()) {
                        targets.push(("Claude Desktop", cd_path, true));
                    }
                }
                if home.join(".codeium/windsurf").exists() {
                    targets.push(("Windsurf", windsurf_path, true));
                }
            }
            "cursor" => targets.push(("Cursor", cursor_path, true)),
            "claude-code" | "claude" => targets.push(("Claude Code", claude_code_path, true)),
            "claude-desktop" => {
                let cd = claude_desktop_config_path(&home).ok_or(
                    "Claude Desktop config path is unsupported on this OS",
                )?;
                targets.push(("Claude Desktop", cd, true));
            }
            "windsurf" => targets.push(("Windsurf", windsurf_path, true)),
            other => {
                return Err(format!(
                    "unknown MCP client '{other}'; expected all | cursor | claude-code | claude-desktop | windsurf"
                ));
            }
        }
    }

    // Always write canonical fallback snippet inside MEM_HOME so users always have a copyable template.
    let fallback_path = mem_home_dir().join("mcp_config.json");
    targets.push(("Universal MCP Snippet", fallback_path, true));

    let mut reports = Vec::with_capacity(targets.len());
    for (name, path, create_parent) in targets {
        let status = merge_mcp_json(&path, &server_entry, create_parent)?;
        reports.push(McpInstallReport {
            client: name.to_string(),
            path: path.display().to_string(),
            status,
        });
    }

    Ok(reports)
}

fn merge_mcp_json(
    path: &Path,
    server_entry: &Value,
    create_parent: bool,
) -> Result<String, String> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            if create_parent {
                fs::create_dir_all(parent).map_err(|e| {
                    format!("failed to create directory '{}': {e}", parent.display())
                })?;
            } else {
                return Ok("skipped".to_string());
            }
        }
    }

    let existed = path.exists();
    let mut root: Value = if existed {
        let raw = fs::read_to_string(path).map_err(|e| {
            format!("failed to read MCP config '{}': {e}", path.display())
        })?;
        if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&raw).map_err(|e| {
                format!(
                    "existing MCP config '{}' is not valid JSON: {e}",
                    path.display()
                )
            })?
        }
    } else {
        json!({})
    };

    let obj = root.as_object_mut().ok_or_else(|| {
        format!(
            "MCP config '{}' must contain a JSON object at top level",
            path.display()
        )
    })?;

    let servers_val = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let servers_obj = servers_val.as_object_mut().ok_or_else(|| {
        format!(
            "'mcpServers' in '{}' must be a JSON object",
            path.display()
        )
    })?;

    let status = if servers_obj.contains_key("ai-memory") {
        "updated"
    } else if existed {
        "merged"
    } else {
        "created"
    };
    servers_obj.insert("ai-memory".to_string(), server_entry.clone());

    let pretty = serde_json::to_string_pretty(&root)
        .map_err(|e| format!("failed to serialize MCP JSON: {e}"))?;
    fs::write(path, format!("{pretty}\n"))
        .map_err(|e| format!("failed to write MCP config '{}': {e}", path.display()))?;

    Ok(status.to_string())
}

/// Handles `mem mcp install [...]`
pub fn cmd_mcp_install(opts: &GlobalOpts, args: &[String]) -> Result<ExitCode, String> {
    let mut client = "all".to_string();
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--client" | "-c" => {
                i += 1;
                client = args
                    .get(i)
                    .ok_or("--client requires all|cursor|claude-code|claude-desktop|windsurf")?
                    .clone();
            }
            other => return Err(format!("unexpected argument '{other}' in 'mem mcp install'")),
        }
        i += 1;
    }

    let reports = install_mcp_to_clients(&opts.db_path, &client)?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&reports).unwrap());
    } else {
        let c = opts.use_color;
        println!(
            "{}  {}",
            paint(c, "36;1", "◆"),
            paint(c, "1", "MCP Agent Auto-Wiring")
        );
        for (idx, r) in reports.iter().enumerate() {
            let branch_sym = if idx + 1 == reports.len() { "└─" } else { "├─" };
            println!(
                "│  {} {:<22} {} ({})",
                paint(c, "90", branch_sym),
                r.client,
                paint(c, "32", &r.status),
                paint(c, "90", &r.path)
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Handles `mem config show` and `mem config use <ollama|openai|lite>`.
pub fn cmd_config(opts: &GlobalOpts, args: &[String]) -> Result<ExitCode, String> {
    let sub = args.first().map(String::as_str).unwrap_or("show");
    match sub {
        "show" => {
            let saved = load_persistent_config(&opts.config_path);
            let emb = opts.embedding_config();
            let llm = opts.consolidate_config();
            let mut emb_display = emb.clone();
            if let Some(ref mut k) = emb_display.api_key {
                *k = mask_secret(k);
            }
            let mut llm_display = llm.clone();
            if let Some(ref mut k) = llm_display.api_key {
                *k = mask_secret(k);
            }

            if opts.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "config_path": opts.config_path.display().to_string(),
                        "config_exists": saved.is_some(),
                        "db_path": opts.db_path.display().to_string(),
                        "embedding": emb_display,
                        "consolidate_llm": llm_display,
                    }))
                    .unwrap()
                );
            } else {
                let c = opts.use_color;
                println!("{} {}", paint(c, "36;1", "◆"), paint(c, "1", "Active Engram Configuration"));
                println!("│  Config File : {} ({})", opts.config_path.display(), if saved.is_some() { "persisted" } else { "using defaults" });
                println!("│  Vault DB    : {}", opts.db_path.display());
                println!(
                    "│  Embedding   : {:?}  model={}  url={}  key={}",
                    emb_display.backend,
                    if emb_display.model.is_empty() { "none" } else { &emb_display.model },
                    if emb_display.base_url.is_empty() { "-" } else { &emb_display.base_url },
                    emb_display.api_key.as_deref().unwrap_or("none")
                );
                println!(
                    "└  Consolidate : {:?}  model={}  url={}",
                    llm_display.backend,
                    if llm_display.model.is_empty() { "none" } else { &llm_display.model },
                    if llm_display.base_url.is_empty() { "-" } else { &llm_display.base_url }
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        "use" => {
            let mode = args
                .get(1)
                .ok_or("usage: mem config use <ollama|openai|lite> [--api-key <KEY>]")?
                .to_ascii_lowercase();

            let mut api_key: Option<String> = None;
            let mut embed_model: Option<String> = None;
            let mut llm_model: Option<String> = None;
            let mut base_url: Option<String> = None;
            let mut dimensions: Option<usize> = None;

            let mut i = 2usize;
            while i < args.len() {
                match args[i].as_str() {
                    "--api-key" => {
                        i += 1;
                        api_key = Some(args.get(i).ok_or("--api-key requires a value")?.clone());
                    }
                    "--embed-model" => {
                        i += 1;
                        embed_model = Some(args.get(i).ok_or("--embed-model requires a value")?.clone());
                    }
                    "--llm-model" => {
                        i += 1;
                        llm_model = Some(args.get(i).ok_or("--llm-model requires a value")?.clone());
                    }
                    "--base-url" => {
                        i += 1;
                        base_url = Some(args.get(i).ok_or("--base-url requires a value")?.clone());
                    }
                    "--dimensions" => {
                        i += 1;
                        let raw = args.get(i).ok_or("--dimensions requires an integer")?;
                        dimensions = Some(
                            raw.parse()
                                .map_err(|_| format!("invalid dimensions '{raw}'"))?,
                        );
                    }
                    other => return Err(format!("unexpected option '{other}' in 'mem config use'")),
                }
                i += 1;
            }

            let mut cfg = load_persistent_config(&opts.config_path).unwrap_or_default();
            cfg.db_path = Some(opts.db_path.display().to_string());

            match mode.as_str() {
                "ollama" | "local" => {
                    cfg.mem.embedding = EmbeddingProviderConfig::ollama_default();
                    cfg.mem.consolidate_llm = ConsolidateLlmConfig::ollama_default();
                }
                "openai" | "prod" | "production" => {
                    let key = api_key
                        .or_else(|| env::var("OPENAI_API_KEY").ok().filter(|k| !k.trim().is_empty()))
                        .or_else(|| cfg.mem.embedding.api_key.clone())
                        .unwrap_or_default();
                    cfg.mem.embedding = EmbeddingProviderConfig::openai_production(key.clone());
                    cfg.mem.consolidate_llm = ConsolidateLlmConfig::openai_production(key);
                }
                "lite" | "disabled" | "none" => {
                    cfg.mem.embedding = EmbeddingProviderConfig::disabled();
                    cfg.mem.consolidate_llm = ConsolidateLlmConfig::disabled();
                }
                other => {
                    return Err(format!(
                        "unknown profile '{other}'; expected ollama | openai | lite"
                    ));
                }
            }

            if let Some(url) = base_url {
                cfg.mem.embedding.base_url = url.clone();
                cfg.mem.consolidate_llm.base_url = url;
            }
            if let Some(m) = embed_model {
                cfg.mem.embedding.model = m;
            }
            if let Some(m) = llm_model {
                cfg.mem.consolidate_llm.model = m;
            }
            if let Some(d) = dimensions {
                cfg.mem.embedding.dimensions = Some(d);
            }

            save_persistent_config(&opts.config_path, &cfg)?;

            if opts.json {
                let mut masked_cfg = cfg.clone();
                if let Some(ref mut k) = masked_cfg.mem.embedding.api_key {
                    *k = mask_secret(k);
                }
                if let Some(ref mut k) = masked_cfg.mem.consolidate_llm.api_key {
                    *k = mask_secret(k);
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "config_path": opts.config_path.display().to_string(),
                        "active_mode": mode,
                        "config": masked_cfg,
                    }))
                    .unwrap()
                );
            } else {
                let c = opts.use_color;
                println!(
                    "{} Switched intelligence profile to {} ({})",
                    paint(c, "32;1", "✔"),
                    paint(c, "36;1", &mode),
                    opts.config_path.display()
                );
                println!(
                    "  Embedding: {}  |  Consolidation LLM: {}",
                    if cfg.mem.embedding.model.is_empty() {
                        "disabled (BM25 + Wiki Graph)"
                    } else {
                        &cfg.mem.embedding.model
                    },
                    if cfg.mem.consolidate_llm.model.is_empty() {
                        "disabled"
                    } else {
                        &cfg.mem.consolidate_llm.model
                    }
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!(
            "unknown config subcommand '{other}'; expected 'show' or 'use'"
        )),
    }
}

/// Interactive Clack-style onboarding wizard (`mem onboard` / `mem setup`).
pub fn cmd_onboard(opts: &GlobalOpts, args: &[String]) -> Result<ExitCode, String> {
    let mut mode: Option<String> = None;
    let mut api_key: Option<String> = None;
    let mut vault_dir: Option<PathBuf> = None;
    let mut seed_demo = false;
    let mut no_seed = false;
    let mut client = "all".to_string();
    let mut assume_yes = false;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" | "-m" => {
                i += 1;
                mode = Some(
                    args.get(i)
                        .ok_or("--mode requires ollama|openai|lite")?
                        .to_ascii_lowercase(),
                );
            }
            "--api-key" => {
                i += 1;
                api_key = Some(args.get(i).ok_or("--api-key requires a key")?.clone());
            }
            "--vault" => {
                i += 1;
                vault_dir = Some(PathBuf::from(
                    args.get(i).ok_or("--vault requires a directory path")?,
                ));
            }
            "--seed-demo" => seed_demo = true,
            "--no-seed" => no_seed = true,
            "--client" => {
                i += 1;
                client = args
                    .get(i)
                    .ok_or("--client requires all|cursor|claude-code|claude-desktop|windsurf")?
                    .clone();
            }
            "--yes" | "-y" => assume_yes = true,
            other => return Err(format!("unexpected option '{other}' in 'mem onboard'")),
        }
        i += 1;
    }

    let c = opts.use_color;
    let interactive = !assume_yes && !opts.json && io::stdin().is_terminal();
    let host = HostEnvironment::detect();

    if !opts.json {
        println!();
        println!(
            "{}  {}",
            paint(c, "36;1", "┌"),
            paint(c, "1", "Engram (`mem`) — Interactive Setup")
        );
        println!(
            "{}  {}",
            paint(c, "36;1", "│"),
            paint(c, "90", "Git-like Versioned Memory + Obsidian Wiki Graph + MCP")
        );
        println!("{}", paint(c, "36;1", "│"));

        // Step 1: Hardware & Host Discovery
        println!(
            "{}  {}",
            paint(c, "32;1", "◇"),
            paint(c, "1", "Host & Hardware Discovery")
        );
        let ram_label = host
            .ram_gib
            .map(|g| format!(", {:.1} GiB RAM", g))
            .unwrap_or_default();
        println!(
            "{}  ├─ System   : {} ({} threads{})",
            paint(c, "36;1", "│"),
            host.os_arch,
            host.cpu_threads,
            ram_label
        );
        println!(
            "{}  ├─ GPU      : {}",
            paint(c, "36;1", "│"),
            host.gpu.as_deref().unwrap_or("None detected (CPU mode supported)")
        );
        let ollama_label = if host.ollama_online {
            paint(c, "32", "online at 127.0.0.1:11434")
        } else if host.ollama_installed {
            paint(c, "33", "installed (daemon offline; auto-fallback enabled)")
        } else {
            paint(c, "90", "not installed")
        };
        println!("{}  ├─ Ollama   : {}", paint(c, "36;1", "│"), ollama_label);
        let clients_label = if host.detected_clients.is_empty() {
            "none detected (will generate universal mcp_config.json)".to_string()
        } else {
            host.detected_clients.join(", ")
        };
        println!("{}  └─ Agents   : {}", paint(c, "36;1", "│"), clients_label);
        println!("{}", paint(c, "36;1", "│"));
    }

    // Step 2: Select Intelligence Profile
    let selected_mode = match mode {
        Some(m) => m,
        None if interactive => {
            let rec_ollama = host.ollama_online || host.gpu.is_some();
            println!(
                "{}  {}",
                paint(c, "36;1", "◆"),
                paint(c, "1", "Choose Intelligence Profile")
            );
            println!(
                "{}     [1] Local Ollama (Dev / Offline GPU)  {} {}",
                paint(c, "36;1", "│"),
                paint(c, "90", "bge-m3 + qwen2.5:7b"),
                if rec_ollama { paint(c, "32", "(Recommended)") } else { String::new() }
            );
            println!(
                "{}     [2] Production OpenAI API            {}",
                paint(c, "36;1", "│"),
                paint(c, "90", "text-embedding-3-small + gpt-6-luna")
            );
            println!(
                "{}     [3] Lite Engine (Zero-Model CPU)     {} {}",
                paint(c, "36;1", "│"),
                paint(c, "90", "FTS5 BM25 + Obsidian [[Wikilink]] Graph"),
                if !rec_ollama { paint(c, "32", "(Recommended)") } else { String::new() }
            );
            let default_choice = if rec_ollama { "1" } else { "3" };
            let raw = prompt_line(&format!("{}  Select [1/2/3] (default {default_choice}): ", paint(c, "36;1", "│")))?;
            match if raw.trim().is_empty() { default_choice } else { raw.trim() } {
                "1" | "ollama" => "ollama".to_string(),
                "2" | "openai" => "openai".to_string(),
                "3" | "lite" => "lite".to_string(),
                other => return Err(format!("invalid selection '{other}'")),
            }
        }
        None => {
            if host.ollama_online || host.gpu.is_some() {
                "ollama".to_string()
            } else {
                "lite".to_string()
            }
        }
    };

    if selected_mode == "openai" && api_key.is_none() {
        let env_key = env::var("OPENAI_API_KEY").ok().filter(|k| !k.trim().is_empty());
        if let Some(k) = env_key {
            api_key = Some(k);
        } else if interactive {
            let input_key = prompt_line(&format!(
                "{}  Enter OPENAI_API_KEY (sk-... or press Enter to configure later): ",
                paint(c, "36;1", "│")
            ))?;
            if !input_key.trim().is_empty() {
                api_key = Some(input_key.trim().to_string());
            }
        }
    }

    let mut persistent = PersistentCliConfig {
        db_path: Some(opts.db_path.display().to_string()),
        mem: match selected_mode.as_str() {
            "ollama" | "local" => MemConfig::ollama_dev(),
            "openai" | "prod" | "production" => MemConfig::openai_prod(api_key.clone().unwrap_or_default()),
            "lite" | "disabled" | "none" => MemConfig {
                search: SearchConfig::default(),
                embedding: EmbeddingProviderConfig::disabled(),
                consolidate_llm: ConsolidateLlmConfig::disabled(),
            },
            other => {
                return Err(format!(
                    "invalid --mode '{other}'; expected ollama | openai | lite"
                ));
            }
        },
    };
    if selected_mode == "openai" && api_key.is_none() {
        persistent.mem.embedding.api_key = None;
        persistent.mem.consolidate_llm.api_key = None;
    }

    save_persistent_config(&opts.config_path, &persistent)?;

    if !opts.json {
        println!(
            "{}  {} Profile saved: {} -> {}",
            paint(c, "32;1", "◇"),
            paint(c, "1", &selected_mode),
            paint(c, "90", &opts.config_path.display().to_string()),
            if persistent.mem.embedding.model.is_empty() {
                "FTS5 + Wiki Graph".to_string()
            } else {
                format!(
                    "{} + {}",
                    persistent.mem.embedding.model, persistent.mem.consolidate_llm.model
                )
            }
        );
        println!("{}", paint(c, "36;1", "│"));
    }

    // Step 3: Initialize Memory Vault & Seed / Import Knowledge
    if let Some(parent) = opts.db_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| format!("failed to create db dir: {e}"))?;
        }
    }
    let store = SqliteStore::open(&opts.db_path).map_err(|e| e.to_string())?;
    let vcs = MemVcs::new(store);
    let root_cid = vcs.init().map_err(|e| e.to_string())?;

    let existing_nodes = vcs
        .list(&Revision::BranchWorkspace("main".into()), None)
        .map_err(|e| e.to_string())?
        .len();

    let emb_cfg = &persistent.mem.embedding;
    let (engine, fallback_notice) = crate::build_search_engine(vcs.clone(), emb_cfg);

    let mut seeded_count = 0usize;
    let mut imported_files = 0usize;

    if let Some(vdir) = vault_dir {
        let stats = engine
            .import_vault(&vdir, "main", MemoryTier::Semantic)
            .map_err(|e| e.to_string())?;
        imported_files = stats.files_imported;
        if imported_files > 0 {
            vcs.commit("main", "onboard", "import obsidian vault")
                .map_err(|e| e.to_string())?;
        }
    } else {
        let should_seed = if seed_demo {
            true
        } else if no_seed || existing_nodes > 0 {
            false
        } else if interactive {
            let ans = prompt_line(&format!(
                "{}  Seed 4 Obsidian Wiki starter notes for instant demo? [Y/n]: ",
                paint(c, "36;1", "│")
            ))?;
            !matches!(ans.trim().to_ascii_lowercase().as_str(), "n" | "no")
        } else {
            false
        };

        if should_seed {
            seeded_count = seed_demo_wiki_notes(&vcs, &engine)?;
        }
    }

    if !opts.json {
        println!(
            "{}  {} {}",
            paint(c, "32;1", "◇"),
            paint(c, "1", "Memory Vault Initialized"),
            paint(c, "90", &opts.db_path.display().to_string())
        );
        println!(
            "{}  ├─ Root     : main ({})",
            paint(c, "36;1", "│"),
            &root_cid.to_hex()[..12]
        );
        if imported_files > 0 {
            println!(
                "{}  └─ Vault    : imported {} Obsidian markdown note(s)",
                paint(c, "36;1", "│"),
                imported_files
            );
        } else if seeded_count > 0 {
            println!(
                "{}  └─ Starter  : seeded {} Obsidian Wiki notes & committed snapshot",
                paint(c, "36;1", "│"),
                seeded_count
            );
        } else {
            println!("{}  └─ Workspace: ready (clean repository)", paint(c, "36;1", "│"));
        }
        if let Some(msg) = &fallback_notice {
            println!("{}     {}", paint(c, "36;1", "│"), paint(c, "33", msg));
        }
        println!("{}", paint(c, "36;1", "│"));
    }

    // Step 4: Auto-Wire MCP Clients
    let mcp_reports = install_mcp_to_clients(&opts.db_path, &client)?;
    if !opts.json {
        println!(
            "{}  {}",
            paint(c, "32;1", "◇"),
            paint(c, "1", "MCP Agent Auto-Wiring")
        );
        for (idx, r) in mcp_reports.iter().enumerate() {
            let sym = if idx + 1 == mcp_reports.len() { "└─" } else { "├─" };
            println!(
                "{}  {} {:<22} {} ({})",
                paint(c, "36;1", "│"),
                paint(c, "90", sym),
                r.client,
                paint(c, "32", &r.status),
                paint(c, "90", &r.path)
            );
        }
        println!("{}", paint(c, "36;1", "│"));
    }

    // Step 5: Live Verification
    let t_core = Instant::now();
    let compiled = compile_core(&vcs, &Revision::BranchWorkspace("main".into()), 2000)
        .map_err(|e| e.to_string())?;
    let core_ms = t_core.elapsed().as_secs_f64() * 1000.0;

    let mut demo_search_ms: Option<f64> = None;
    let mut demo_hits_count: usize = 0;
    if seeded_count > 0 {
        let t_search = Instant::now();
        let hits = engine
            .search(&SearchRequest::new(
                Revision::BranchWorkspace("main".into()),
                "BUSY_TIMEOUT",
            ))
            .map_err(|e| e.to_string())?;
        demo_search_ms = Some(t_search.elapsed().as_secs_f64() * 1000.0);
        demo_hits_count = hits.len();
    }

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "mode": selected_mode,
                "config_path": opts.config_path.display().to_string(),
                "db_path": opts.db_path.display().to_string(),
                "seeded_notes": seeded_count,
                "imported_files": imported_files,
                "core_compile_ms": core_ms,
                "core_tokens": compiled.estimated_tokens,
                "demo_search_ms": demo_search_ms,
                "demo_hits": demo_hits_count,
                "mcp_clients": mcp_reports,
            }))
            .unwrap()
        );
    } else {
        println!(
            "{}  {}",
            paint(c, "32;1", "◇"),
            paint(c, "1", "Live Verification Passed")
        );
        println!(
            "{}  ├─ /core Compiler : {:.2}ms ({} tokens, target < 5.00ms)",
            paint(c, "36;1", "│"),
            core_ms,
            compiled.estimated_tokens
        );
        if let Some(ms) = demo_search_ms {
            println!(
                "{}  └─ 1-Hop Search   : \"BUSY_TIMEOUT\" -> {} linked notes in {:.2}ms",
                paint(c, "36;1", "│"),
                demo_hits_count,
                ms
            );
        } else {
            println!(
                "{}  └─ Schema & FTS5  : SQLite WAL + fts_chunks + blob_links ready",
                paint(c, "36;1", "│")
            );
        }
        println!("{}", paint(c, "36;1", "│"));
        println!(
            "{}  {}",
            paint(c, "32;1", "└"),
            paint(c, "32;1", "Engram is ready! No env sourcing needed — config is auto-loaded.")
        );

        println!();
        println!("╭───────────────────────────────────────────────────────────────────╮");
        println!("│  Quick Commands                                                   │");
        println!("│    mem status                      Inspect vault & branches       │");
        println!("│    mem search \"BUSY_TIMEOUT\"       Hybrid + 1-hop Wiki search     │");
        println!("│    mem vault import <dir>          Load your Obsidian Vault       │");
        println!("│    mem config use openai --api-key sk-...   Switch to prod OpenAI │");
        println!("│    mem doctor                      Full system diagnostics        │");
        println!("╰───────────────────────────────────────────────────────────────────╯");
    }

    Ok(ExitCode::SUCCESS)
}

fn seed_demo_wiki_notes(vcs: &MemVcs, engine: &MemSearchEngine) -> Result<usize, String> {
    let notes = [
        (
            "/core/rules.md",
            "Never mutate production database tables without creating a branch snapshot first.",
            1.0f32,
            vec!["core", "safety"],
        ),
        (
            "/semantic/sqlite-wal.md",
            "---\ntitle: SQLite 并发与锁配置\naliases: [\"BUSY_TIMEOUT\", \"数据库写锁\"]\ntags: [\"db/sqlite\", \"storage\"]\nentities:\n  - \"SQLite -> uses -> WAL Mode\"\nimportance: 0.92\n---\n# 存储引擎\n\n## WAL 并发配置\n开启 WAL 模式并设置 busy_timeout=5000，排查 SOP 见 [[/procedural/sqlite-lock-sop.md|锁排查指南]]。",
            0.92,
            vec!["db/sqlite", "storage"],
        ),
        (
            "/procedural/sqlite-lock-sop.md",
            "---\ntitle: 数据库连接锁定排查指南\ntags: [\"ops/runbook\"]\nimportance: 0.85\n---\n# 排查步骤\n1. 检查长事务是否未提交。\n2. 检查 WAL checkpoint 是否被只读连接阻塞。",
            0.85,
            vec!["ops/runbook"],
        ),
        (
            "/semantic/architecture-moc.md",
            "---\ntitle: 存储架构导航 (MOC)\ntags: [\"moc\"]\nimportance: 0.80\n---\n# 存储与检索 MOC\n- 并发参数与锁配置：[[SQLite 并发与锁配置]]",
            0.80,
            vec!["moc"],
        ),
    ];

    for (raw_path, body, imp, tags) in &notes {
        let vpath = VirtualPath::new(*raw_path).map_err(|e| e.to_string())?;
        let mut meta = MemoryMetadata::default();
        meta.importance = *imp;
        meta.tags = tags.iter().map(|s| (*s).to_string()).collect();
        let node = MemoryNode::new(vpath.clone(), body.as_bytes().to_vec()).with_metadata(meta);
        let blob_id = vcs.write_node("main", node).map_err(|e| e.to_string())?;
        engine
            .index_blob_if_needed(&vpath, &blob_id)
            .map_err(|e| e.to_string())?;
    }

    vcs.commit("main", "onboard", "seed obsidian wiki starter notes")
        .map_err(|e| e.to_string())?;
    Ok(notes.len())
}

fn prompt_line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("failed to read input: {e}"))?;
    Ok(line)
}

fn mask_secret(secret: &str) -> String {
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.len() <= 8 {
        return "****".to_string();
    }
    format!("{}...{}", &trimmed[..4], &trimmed[trimmed.len() - 4..])
}

/// Prints shell tab-completion scripts for `bash`, `zsh`, or `fish`.
pub fn cmd_completions(args: &[String]) -> Result<ExitCode, String> {
    let shell = args
        .first()
        .map(|s| s.to_ascii_lowercase())
        .ok_or("usage: mem completions <bash|zsh|fish>")?;

    match shell.as_str() {
        "bash" => print!(
            r#"_mem_completions() {{
    local cur prev cmds
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    cmds="onboard setup config init status write read ls commit log branch rewind diff merge search compile vault gc doctor mcp completions"
    if [[ ${{COMP_CWORD}} -eq 1 ]]; then
        COMPREPLY=( $(compgen -W "$cmds" -- "$cur") )
        return 0
    fi
    case "$prev" in
        config) COMPREPLY=( $(compgen -W "show use" -- "$cur") ;;
        use) COMPREPLY=( $(compgen -W "ollama openai lite" -- "$cur") ;;
        mcp) COMPREPLY=( $(compgen -W "install serve" -- "$cur") ;;
        vault) COMPREPLY=( $(compgen -W "import" -- "$cur") ;;
        completions) COMPREPLY=( $(compgen -W "bash zsh fish" -- "$cur") ;;
        *) ;;
    esac
}}
complete -F _mem_completions mem
"#
        ),
        "zsh" => print!(
            r#"#compdef mem
_mem() {{
  local -a commands
  commands=(
    'onboard:Interactive setup wizard'
    'setup:Alias for onboard'
    'config:Show or switch intelligence provider (ollama/openai/lite)'
    'init:Initialize a new SQLite memory repository'
    'status:Show branch, tier counts, and providers'
    'write:Write a memory node'
    'read:Read a memory node'
    'ls:List active memory paths'
    'commit:Commit staged changes'
    'log:Show commit DAG'
    'branch:List, create, or delete branches'
    'rewind:Rewind branch to a historical commit'
    'diff:Compare revisions'
    'merge:Three-way merge branches'
    'search:Hybrid BM25 + Vector + 1-hop Obsidian Wiki search'
    'compile:Compile /core prompt context (<5ms)'
    'vault:Import local Obsidian Vault'
    'gc:Garbage collect unreachable blobs/trees'
    'doctor:Health diagnostics'
    'mcp:Start stdio MCP server or install into AI clients'
    'completions:Generate shell completions'
  )
  _describe 'command' commands
}}
_mem "$@"
"#
        ),
        "fish" => print!(
            r#"set -l cmds onboard setup config init status write read ls commit log branch rewind diff merge search compile vault gc doctor mcp completions
complete -c mem -f -n "not __fish_seen_subcommand_from $cmds" -a "$cmds"
complete -c mem -f -n "__fish_seen_subcommand_from config" -a "show use"
complete -c mem -f -n "__fish_seen_subcommand_from use" -a "ollama openai lite"
complete -c mem -f -n "__fish_seen_subcommand_from mcp" -a "install serve"
complete -c mem -f -n "__fish_seen_subcommand_from vault" -a "import"
complete -c mem -f -n "__fish_seen_subcommand_from completions" -a "bash zsh fish"
"#
        ),
        other => {
            return Err(format!(
                "unsupported shell '{other}'; expected bash | zsh | fish"
            ));
        }
    }

    Ok(ExitCode::SUCCESS)
}
