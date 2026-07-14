//! Magma sidecar — tip-based txpool IPC reprioritization for a Monad node.
//!
//! See `docs/ARCHITECTURE.md` for the end-to-end design.

mod backrun;
mod config;
mod metrics;
mod policy;
mod routes;
mod txpool_ipc;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::U256;
use clap::Parser;
use config::Config;
use metrics::Metrics;
use policy::PolicyConfig;
use routes::{router, HttpState};
use tokio::sync::watch;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;
use txpool_ipc::PriorityMode;

const FALLBACK_PRIORITY: u64 = 0xffff;
const BACKRUN_CACHE_MAX: usize = 4096;
const BACKRUN_PENDING_MAX: usize = 4096;
const SENT_CACHE_MAX: usize = 16384;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::parse();

    let network = config.network;
    let policy = PolicyConfig::for_network(network);
    // Refuse to start a network whose gateway address isn't baked into this build
    // yet (a not-yet-filled-in network resolves to 0x0). A tx can never target 0x0,
    // so the allowlist would match nothing and the sidecar would run as a silent
    // no-op reprioritizer — the worst failure mode for a validator. Fail loudly.
    if policy.gateway_is_unset() {
        return Err(format!(
            "network '{}' has no MagmaSearcherGateway address baked into this build \
             (resolves to the zero address); refusing to start a no-op reprioritizer. \
             Upgrade to a release that bakes in the '{}' gateway, or use \
             --network localnet for local development.",
            network.as_str(),
            network.as_str(),
        )
        .into());
    }
    tracing::info!(
        network = network.as_str(),
        gateway = %policy.gateway(),
        base_fee_floor_wei = policy.base_fee_floor_wei(),
        "loaded tip policy"
    );
    let priority_mode = PriorityMode {
        policy: Arc::new(policy),
        fallback: U256::from(FALLBACK_PRIORITY),
        ttl: Duration::from_millis(config.backrun_pool_ttl_ms),
        max_cache_entries: BACKRUN_CACHE_MAX,
        max_pending_entries: BACKRUN_PENDING_MAX,
        sent_cache_max: SENT_CACHE_MAX,
    };

    let bind: SocketAddr = config.bind;
    let txpool_socket = config.txpool_socket.clone();

    let metrics = Metrics::new();
    let state = HttpState::new(metrics.clone());
    tracing::info!(
        %bind,
        txpool_ipc = ?txpool_socket,
        "starting magma-sidecar"
    );

    let app = router(state).layer(TraceLayer::new_for_http());

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
