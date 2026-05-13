//! HTTP routes: `/health`, `/metrics`, and the Monad JSON-RPC ingress proxy.
//!
//! See `docs/ARCHITECTURE.md` §"End-to-end data flow": searchers either hit
//! the node's JSON-RPC directly or post to the sidecar's `/rpc/monad`, which
//! transparently forwards to the Monad EL.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;

use crate::config::Config;
use crate::error::AppError;
use crate::forward::{build_client, forward_jsonrpc};
use crate::metrics::Metrics;

#[derive(Clone)]
pub struct HttpState {
    pub config: Config,
    pub client: reqwest::Client,
    pub metrics: Arc<Metrics>,
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics_endpoint))
        .route("/rpc/monad", post(proxy_monad))
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

/// Transparent JSON-RPC forward to the Monad EL (e.g. `eth_sendRawTransaction`).
async fn proxy_monad(
    State(s): State<HttpState>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let v: Value = serde_json::from_slice(&body).map_err(|e| {
        s.metrics.record_http("/rpc/monad", 400);
        AppError::BadRequest(format!("invalid JSON: {e}"))
    })?;
    let resp = forward_jsonrpc(
        &s.client,
        &s.config.monad_rpc_url,
        v,
        s.config.http_timeout(),
    )
    .await
    .map_err(|e| {
        s.metrics.record_http("/rpc/monad", 502);
        AppError::Upstream(e)
    })?;
    let response = proxy_response(resp).await?;
    s.metrics
        .record_http("/rpc/monad", response.status().as_u16());
    Ok(response)
}

async fn proxy_response(resp: reqwest::Response) -> Result<Response, AppError> {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = resp.bytes().await?;
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type.as_str())
        .body(Body::from(bytes.to_vec()))
        .map_err(|e| AppError::Internal(e.to_string()))
}

impl HttpState {
    pub fn try_new(config: Config, metrics: Arc<Metrics>) -> Result<Self, reqwest::Error> {
        let client = build_client(&config)?;
        Ok(HttpState {
            config,
            client,
            metrics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::to_bytes;
    use axum::http::Request;
    use std::path::PathBuf;
    use tower::ServiceExt;

    use crate::metrics::Metrics;

    fn test_state(monad_rpc_url: String) -> HttpState {
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            monad_rpc_url,
            http_timeout_secs: 5,
            max_body_bytes: 1 << 20,
            txpool_socket: None,
            tx_priority_hex: "0xffff".into(),
            policy_config: Option::<PathBuf>::None,
        };
        HttpState::try_new(config, Metrics::new()).expect("state")
    }

    #[tokio::test]
    async fn health_returns_structured_snapshot() {
        let app = router(test_state("http://127.0.0.1:1".into()));
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
        let app = router(test_state("http://127.0.0.1:1".into()));
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

    #[tokio::test]
    async fn proxy_monad_rejects_invalid_json() {
        let app = router(test_state("http://127.0.0.1:1".into()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/rpc/monad")
                    .header("content-type", "application/json")
                    .body(Body::from("not-json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn proxy_monad_returns_bad_gateway_when_upstream_unreachable() {
        // Port 1 is reserved/closed; the forward should fail and surface as 502.
        let app = router(test_state("http://127.0.0.1:1".into()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/rpc/monad")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }
}
