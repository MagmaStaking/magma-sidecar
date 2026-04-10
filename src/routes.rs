//! HTTP routes: health, rbuilder ingress proxy, Monad bundle egress.

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::error::AppError;
use crate::forward::{build_client, forward_jsonrpc};

#[derive(Clone)]
pub struct HttpState {
    pub config: Config,
    pub client: reqwest::Client,
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/rpc/rbuilder", post(proxy_rbuilder))
        .route("/rpc/monad", post(proxy_monad))
        .route("/v1/submit-builder-bundle", post(submit_builder_bundle))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "service": "magma-sidecar",
    }))
}

/// Transparent JSON-RPC forward to rbuilder (searchers / tools send bundles or raw txs here).
async fn proxy_rbuilder(
    State(s): State<HttpState>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let v: Value = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("invalid JSON: {e}")))?;
    let resp = forward_jsonrpc(
        &s.client,
        &s.config.rbuilder_rpc_url,
        v,
        s.config.http_timeout(),
    )
    .await?;
    proxy_response(resp).await
}

/// Transparent JSON-RPC forward to Monad node (escape hatch).
async fn proxy_monad(
    State(s): State<HttpState>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let v: Value = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("invalid JSON: {e}")))?;
    let resp = forward_jsonrpc(
        &s.client,
        &s.config.monad_rpc_url,
        v,
        s.config.http_timeout(),
    )
    .await?;
    proxy_response(resp).await
}

/// Pre-signed builder bundle from rbuilder → sidecar → `monad_submitBuilderBundle` on Monad RPC.
/// Signing stays in rbuilder; sidecar only wraps transport.
#[derive(Debug, Deserialize, Serialize)]
pub struct BuilderBundleParams {
    pub transactions: Vec<String>,
    pub signature: String,
    pub signer: String,
    pub timestamp: u64,
}

async fn submit_builder_bundle(
    State(s): State<HttpState>,
    Json(params): Json<BuilderBundleParams>,
) -> Result<impl IntoResponse, AppError> {
    let request = json!({
        "jsonrpc": "2.0",
        "method": "monad_submitBuilderBundle",
        "params": params,
        "id": 1u64
    });
    let resp = forward_jsonrpc(
        &s.client,
        &s.config.monad_rpc_url,
        request,
        s.config.http_timeout(),
    )
    .await?;
    proxy_response(resp).await
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
    pub fn try_new(config: Config) -> Result<Self, reqwest::Error> {
        let client = build_client(&config)?;
        Ok(HttpState { config, client })
    }
}
