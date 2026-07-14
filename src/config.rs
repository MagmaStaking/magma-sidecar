//! Configuration (CLI + environment).
//!
//! See `docs/ARCHITECTURE.md`. The sidecar is a thin process with two surfaces:
//! an observability HTTP server (`/health`, `/metrics`), and the txpool IPC
//! reprioritizer.

use std::path::PathBuf;

use clap::Parser;
use std::net::SocketAddr;

use crate::policy::Network;

#[derive(Debug, Clone, Parser)]
#[command(name = "magma-sidecar")]
#[command(
    about = "Sidecar for Monad: tip-based txpool IPC reprioritization (see docs/ARCHITECTURE.md)"
)]
pub struct Config {
    /// Address to bind the observability HTTP server (`/health`, `/metrics`), e.g. 0.0.0.0:8089
    #[arg(long, env = "MAGMA_SIDECAR_BIND", default_value = "127.0.0.1:8089")]
    pub bind: SocketAddr,

    /// Optional path to Monad txpool IPC Unix socket (same wire as `monad-eth-txpool-ipc`).
    /// When set, the sidecar subscribes to txpool events and re-injects `EthTxPoolIpcTx`
    /// with a tip-derived priority (see `docs/ARCHITECTURE.md` §"Priority policy").
    /// Unset = observability-only (`/health`, `/metrics`; no reprioritization). The
    /// `.deb` seeds this to the conventional validator path `/home/monad/monad-bft/mempool.sock`.
    #[arg(long, env = "MAGMA_TXPOOL_SOCKET")]
    pub txpool_socket: Option<PathBuf>,

    /// Which network's `MagmaSearcherGateway` to score against. Defaults to
    /// `mainnet`; use `localnet` for local development. The per-network gateway
    /// addresses are baked into `src/policy.rs`.
    #[arg(long, env = "MAGMA_NETWORK", value_enum, default_value = "mainnet")]
    pub network: Network,

    /// How long (milliseconds) the backrun pairing pool holds a cached target tx
    /// or a parked bid before expiring it.
    #[arg(long, env = "MAGMA_BACKRUN_POOL_TTL_MS", default_value_t = 2500)]
    pub backrun_pool_ttl_ms: u64,
}
