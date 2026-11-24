//! Multi-project Engram server: control plane (`server.db`), API key RBAC, and HTTP endpoints.

mod api;
mod audit;
mod auth;
mod browse;
mod changes;
mod consolidate;
mod control;
mod dav;
mod dav_commit;
mod davfs;
mod error;
mod http;
mod keys;
mod mcp;
mod web;

use std::sync::Arc;

pub use audit::AuditEntry;
pub use auth::{authorize, Action, ApiKey, Role};
pub use consolidate::{system_key, ConsolidationRun};
pub use control::{ControlPlane, EngineFactory};
pub use error::ServerError;
pub use http::router;

/// Serves the HTTP API on `listen` until the process stops.
/// Prints `listening on <addr>` once the socket is bound.
pub fn run_server(cp: Arc<ControlPlane>, listen: &str) -> std::io::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let listener = tokio::net::TcpListener::bind(listen).await?;
            println!("listening on {}", listener.local_addr()?);
            match cp.fail_interrupted_runs() {
                Ok(0) => {}
                Ok(n) => eprintln!("marked {n} interrupted consolidation run(s) as failed"),
                Err(e) => eprintln!("failed to clear interrupted consolidation runs: {e}"),
            }
            if cp.llm_config().backend != mem_search::ProviderBackend::Disabled {
                tokio::spawn(consolidate::scheduler(cp.clone()));
            }
            axum::serve(listener, router(cp)).await
        })
}
