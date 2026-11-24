use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use mem_daemon::{serve, DaemonConfig};
use mem_search::{ConsolidateLlmConfig, EmbeddingProviderConfig, ProviderBackend};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut cfg = DaemonConfig {
        host: env::var("MEM_DAEMON_HOST")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string()),
        port: env::var("MEM_DAEMON_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7700),
        db_path: env::var("MEM_DB_PATH")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_db_path),
        embedding: EmbeddingProviderConfig::from_env(),
        consolidate_llm: ConsolidateLlmConfig::from_env(),
        auth_token: env::var("MEM_DAEMON_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty()),
        consolidate_interval_secs: None,
        consolidate_auto_merge: false,
    };

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "--host" => {
                i += 1;
                cfg.host = args.get(i).ok_or("--host requires an address")?.clone();
            }
            "--port" => {
                i += 1;
                let raw = args.get(i).ok_or("--port requires a port number")?;
                cfg.port = raw.parse().map_err(|_| format!("invalid port '{raw}'"))?;
            }
            "--db" => {
                i += 1;
                let raw = args.get(i).ok_or("--db requires a file path")?;
                cfg.db_path = PathBuf::from(raw);
            }
            "--provider" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or("--provider requires ollama|openai|disabled")?;
                let backend = ProviderBackend::parse(raw)
                    .ok_or_else(|| format!("unknown provider '{raw}'"))?;
                cfg.embedding = match backend {
                    ProviderBackend::Disabled => EmbeddingProviderConfig::disabled(),
                    ProviderBackend::Ollama => EmbeddingProviderConfig::ollama_default(),
                    ProviderBackend::OpenAi => EmbeddingProviderConfig::openai_production(
                        env::var("OPENAI_API_KEY").unwrap_or_default(),
                    ),
                };
                cfg.consolidate_llm = match backend {
                    ProviderBackend::Disabled => ConsolidateLlmConfig::disabled(),
                    ProviderBackend::Ollama => ConsolidateLlmConfig::ollama_default(),
                    ProviderBackend::OpenAi => ConsolidateLlmConfig::openai_production(
                        env::var("OPENAI_API_KEY").unwrap_or_default(),
                    ),
                };
            }
            "--token" => {
                i += 1;
                cfg.auth_token = Some(args.get(i).ok_or("--token requires a secret")?.clone());
            }
            "--consolidate-interval" => {
                i += 1;
                let raw = args
                    .get(i)
                    .ok_or("--consolidate-interval requires seconds")?;
                let secs: u64 = raw
                    .parse()
                    .map_err(|_| format!("invalid interval '{raw}'"))?;
                cfg.consolidate_interval_secs = Some(secs);
            }
            "--consolidate-auto-merge" => {
                cfg.consolidate_auto_merge = true;
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
        i += 1;
    }

    tracing::info!(
        host = %cfg.host,
        port = cfg.port,
        db = %cfg.db_path.display(),
        auth = cfg.auth_token.is_some(),
        "starting mem-daemon HTTP/SSE/MCP server"
    );

    serve(cfg).await.map_err(|e| e.to_string())
}

fn default_db_path() -> PathBuf {
    if let Some(home) = env::var_os("MEM_HOME").or_else(|| env::var_os("HOME")) {
        let mut p = PathBuf::from(home);
        if env::var_os("MEM_HOME").is_none() {
            p.push(".ai-memory");
        }
        p.push("memory.db");
        p
    } else {
        PathBuf::from("memory.db")
    }
}

fn print_help() {
    println!(
        r#"mem-daemon — HTTP REST, SSE & MCP Daemon for Engram

USAGE:
  mem-daemon [OPTIONS]

OPTIONS:
  --host <ADDR>                  Bind address (default: 127.0.0.1 or $MEM_DAEMON_HOST)
  --port <PORT>                  TCP port (default: 7700 or $MEM_DAEMON_PORT)
  --db <PATH>                    SQLite database path (default: ~/.ai-memory/memory.db)
  --provider <MODE>              Provider: ollama | openai | disabled
  --token <SECRET>               Require Bearer token for /v1/* and /mcp*
  --consolidate-interval <SECS>  Enable periodic background sleep consolidation
  --consolidate-auto-merge       Auto-merge clean consolidation branches into main
  -h, --help                     Print help
"#
    );
}
