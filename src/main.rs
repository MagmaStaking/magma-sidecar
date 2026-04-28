//! Magma sidecar — HTTP ingress + tip-based txpool reprioritization for a Monad node.
//!
//! See `docs/ARCHITECTURE.md` for the end-to-end design.

mod config;
mod error;
mod forward;
mod metrics;
mod policy;
mod routes;
mod txpool_ipc;

use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use config::Config;
use metrics::Metrics;
use policy::PolicyConfig;
use routes::{router, HttpState};
use tokio::sync::watch;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;
use txpool_ipc::PriorityMode;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::parse();
    let fallback_priority = config::parse_u256_hex(&config.tx_priority_hex).map_err(|e| {
        format!("invalid --tx-priority / MAGMA_TX_PRIORITY ({e}); use hex, e.g. 0xffff or ffff")
    })?;

    let priority_mode = match config.policy_config.as_ref() {
        Some(path) => {
            let policy = PolicyConfig::load(path)?;
            tracing::info!(
                path = %path.display(),
                gateways = policy.gateway_count(),
                "loaded tip policy"
            );
            PriorityMode::Policy {
                policy: Arc::new(policy),
                fallback: fallback_priority,
            }
        }
        None => {
            tracing::info!(
                priority = %fallback_priority,
                "no policy file; using constant priority for every Insert"
            );
            PriorityMode::Constant(fallback_priority)
        }
    };

    let max_body = config.max_body_bytes;
    let bind: SocketAddr = config.bind;
    let txpool_socket = config.txpool_socket.clone();

    let metrics = Metrics::new();
    let state = HttpState::try_new(config, metrics.clone())?;
    tracing::info!(
        %bind,
        monad_rpc = %state.config.monad_rpc_url,
        txpool_ipc = ?txpool_socket,
        "starting magma-sidecar"
    );

    let app = router(state)
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(bind).await?;

    let (shutdown_tx, _) = watch::channel(false);

    let ipc_task = if let Some(path) = txpool_socket {
        let shutdown_rx = shutdown_tx.subscribe();
        let metrics_for_ipc = metrics.clone();
        Some(tokio::spawn(async move {
            txpool_ipc::run_txpool_priority_loop(path, priority_mode, metrics_for_ipc, shutdown_rx)
                .await;
        }))
    } else {
        None
    };

    let shutdown_tx_signal = shutdown_tx.clone();
    let graceful = async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal received");
        let _ = shutdown_tx_signal.send(true);
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(graceful)
        .await?;

    if let Some(task) = ipc_task {
        let _ = task.await;
    }

    Ok(())
}
