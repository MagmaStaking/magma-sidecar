//! HTTP routes: `/health` and `/metrics`.
//!
//! The HTTP surface is observability-only. Searchers submit to the Monad node's JSON-RPC
//! directly, and the sidecar reprioritizes what it sees on the txpool IPC
//! socket (see `docs/ARCHITECTURE.md`).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use crate::metrics::Metrics;

#[derive(Clone)]
pub struct HttpState {
    pub metrics: Arc<Metrics>,
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics_endpoint))
        .with_state(state)
}

async fn health(State(s): State<HttpState>) -> impl IntoResponse {
    let snap = s.metrics.snapshot();
    s.metrics.record_http("/health", 200);
    Json(snap)
}

async fn metrics_endpoint(State(s): State<HttpState>) -> Response {
    match s.metrics.render_prometheus() {
        Ok(body) => {
            s.metrics.record_http("/metrics", 200);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
                .body(Body::from(body))
                .unwrap()
        }
        Err(e) => {
            s.metrics.record_http("/metrics", 500);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from(format!("metrics encode error: {e}")))
                .unwrap()
        }
    }
}

impl HttpState {
    pub fn new(metrics: Arc<Metrics>) -> Self {
        HttpState { metrics }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::to_bytes;
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::metrics::Metrics;

    fn test_state() -> HttpState {
        HttpState::new(Metrics::new())
    }

    #[tokio::test]
    async fn health_returns_structured_snapshot() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["service"], "magma-sidecar");
        assert_eq!(v["ipc_state"], "disabled");
        assert_eq!(v["tx_inserts_observed"], 0);
        assert_eq!(v["tx_prioritized"], 0);
    }

    #[tokio::test]
    async fn metrics_exposes_prometheus_text() {
        let app = router(test_state());
        // First scrape: must succeed and include scalar gauges (which are
        // always emitted). Vec-typed counters need an observation first, and
        // the route's own bump happens *after* render returns.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.starts_with("text/plain"), "ct={ct}");
        let body = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let txt = std::str::from_utf8(&body).unwrap();
        assert!(
            txt.contains("magma_sidecar_txpool_ipc_state"),
            "missing ipc_state gauge in first scrape:\n{txt}"
        );

        // Second scrape: now the http counter has been observed at least once.
        let resp2 = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body2 = to_bytes(resp2.into_body(), 1 << 20).await.unwrap();
        let txt2 = std::str::from_utf8(&body2).unwrap();
        assert!(
            txt2.contains(r#"magma_sidecar_http_requests_total{route="/metrics""#),
            "missing http counter in second scrape:\n{txt2}"
        );
    }
}
