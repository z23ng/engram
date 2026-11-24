//! Multi-process HTTP REST, Server-Sent Events (SSE), and HTTP/SSE MCP daemon for Engram.

pub mod events;
pub mod mcp_rpc;
pub mod routes;
pub mod state;
pub mod ui;

use std::future::Future;
use std::time::Duration;

use mem_consolidate::{ConsolidationEngine, ConsolidationMergeStatus, ConsolidationRequest};
use mem_core::{MemError, Result};
use serde_json::json;
use tokio::net::TcpListener;

pub use events::MemoryEvent;
pub use routes::build_router;
pub use state::{build_search_engine_for_config, DaemonConfig, DaemonState};

/// Starts the `mem-daemon` HTTP/SSE/MCP server bound to `cfg.host:cfg.port` with graceful shutdown.
pub async fn serve(cfg: DaemonConfig) -> Result<()> {
    let state = DaemonState::from_config(&cfg)?;
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| MemError::Storage(format!("failed to bind {addr}: {e}")))?;

    if let Some(interval_secs) = cfg.consolidate_interval_secs.filter(|&s| s > 0) {
        spawn_background_consolidator(state.clone(), interval_secs, cfg.consolidate_auto_merge);
    }

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    serve_on_listener(listener, state, shutdown).await
}

/// Serves `state` on an already-bound [`TcpListener`] until `shutdown` resolves.
pub async fn serve_on_listener<F>(
    listener: TcpListener,
    state: DaemonState,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let app = build_router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| MemError::Storage(format!("daemon server failed: {e}")))
}

/// Spawns an asynchronous cold-path periodic consolidator running safely on isolated `consolidate/*` branches.
pub fn spawn_background_consolidator(
    state: DaemonState,
    interval_secs: u64,
    auto_merge: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
        // Skip immediate startup tick so server startup stays instantaneous.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let search = state.search.clone();
            let llm_cfg = state.llm_cfg.clone();
            let res = tokio::task::spawn_blocking(move || {
                let req = ConsolidationRequest::new("main")
                    .with_auto_merge(auto_merge)
                    .with_archive_processed(auto_merge)
                    .with_delete_branch_on_merge(auto_merge);
                ConsolidationEngine::new(search)
                    .with_llm_config(llm_cfg)
                    .consolidate(&req)
            })
            .await;

            if let Ok(Ok(report)) = res {
                if !report.dry_run && report.episodes_scanned > 0 {
                    let branch = match &report.merge_status {
                        ConsolidationMergeStatus::FastForward { .. }
                        | ConsolidationMergeStatus::Merged { .. } => report.source_branch.clone(),
                        _ => report
                            .consolidation_branch
                            .clone()
                            .unwrap_or_else(|| report.source_branch.clone()),
                    };
                    state.emit(
                        "consolidated",
                        branch,
                        json!({
                            "source_branch": report.source_branch,
                            "consolidation_branch": report.consolidation_branch,
                            "episodes_scanned": report.episodes_scanned,
                            "cards_distilled": report.distilled_cards.len(),
                        }),
                    );
                }
            }
        }
    })
}
