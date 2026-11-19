//! Multi-project Engram server: control plane (`server.db`), API key RBAC, and HTTP endpoints.

mod audit;
mod auth;
mod control;
mod error;
mod http;
mod keys;
mod mcp;

use std::sync::Arc;

pub use audit::AuditEntry;
pub use auth::{authorize, Action, ApiKey, Role};
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
            axum::serve(listener, router(cp)).await
        })
}
