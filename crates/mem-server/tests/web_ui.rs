//! Embedded web client routing (S4b).

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use mem_search::MemSearchEngine;
use mem_server::{router, ControlPlane};
use tower::ServiceExt;

async fn get(uri: &str) -> (StatusCode, String, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let (cp, _) = ControlPlane::open(dir.path(), Box::new(MemSearchEngine::new_bm25_only)).unwrap();
    let app = router(Arc::new(cp));
    let resp = app
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    };
    let (ctype, csp) = (header("content-type"), header("content-security-policy"));
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (
        status,
        ctype,
        csp,
        String::from_utf8_lossy(&body).to_string(),
    )
}

fn web_built() -> bool {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/dist/index.html")
        .exists()
}

#[tokio::test]
async fn client_routes_serve_index_or_build_hint() {
    for uri in ["/", "/projects/alpha/files", "/login"] {
        let (status, ctype, csp, body) = get(uri).await;
        if web_built() {
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert!(ctype.starts_with("text/html"), "{uri}: {ctype}");
            assert!(body.contains("<div id=\"root\">"), "{uri}");
            assert!(
                csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"),
                "{csp}"
            );
        } else {
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert!(body.contains("npm --prefix web"), "{body}");
        }
    }
}

#[tokio::test]
async fn api_mcp_and_missing_assets_never_fall_back_to_html() {
    for uri in [
        "/api/v1/nope",
        "/p/alpha/nope",
        "/assets/missing-abc.js",
        "/assets/../server.db",
    ] {
        let (status, ctype, _, _) = get(uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert!(!ctype.starts_with("text/html"), "{uri}: {ctype}");
    }
}
