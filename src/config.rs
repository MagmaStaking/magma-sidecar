//! Configuration (CLI + environment).
//!
//! See `docs/ARCHITECTURE.md`. The sidecar is a thin process with two surfaces:
//! an observability HTTP server (`/health`, `/metrics`), and the txpool IPC
//! reprioritizer.

use std::path::PathBuf;

use alloy_primitives::U256;
use clap::Parser;
use std::net::SocketAddr;

use crate::policy::Network;

/// Parse hex `U256` for `--tx-priority-hex` (with or without `0x`).
pub fn parse_u256_hex(s: &str) -> Result<U256, String> {
    let s = s.trim();
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    U256::from_str_radix(hex, 16).map_err(|e| e.to_string())
}

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
    /// Unset = ingress-only (no reprioritization). The `.deb` seeds this to the
    /// conventional validator path `/home/monad/monad-bft/mempool.sock`.
    #[arg(long, env = "MAGMA_TXPOOL_SOCKET")]
    pub txpool_socket: Option<PathBuf>,

    /// Fallback hex priority used when no `--network` is configured, or for txs the
    /// policy elects not to recompute (matches node `DEFAULT_TX_PRIORITY`).
    #[arg(long, env = "MAGMA_TX_PRIORITY", default_value = "0xffff")]
    pub tx_priority_hex: String,

    /// Which network's `MagmaSearcherGateway` to score against. Omitting this
    /// disables gateway-aware scoring entirely — every Insert is stamped with
    /// `--tx-priority-hex` (legacy mode, single-tenant local dev only). The
    /// per-network gateway addresses live in `src/policy.rs`.
    #[arg(long, env = "MAGMA_NETWORK", value_enum)]
    pub network: Option<Network>,

    /// How long (milliseconds) the backrun pairing pool holds a cached target tx
    /// or a parked bid before expiring it. Only used in `--network` (policy) mode.
    #[arg(long, env = "MAGMA_BACKRUN_POOL_TTL_MS", default_value_t = 2500)]
    pub backrun_pool_ttl_ms: u64,

    /// Upper bound on the number of candidate-target txs the backrun pairing pool
    /// caches at once (oldest evicted first). Only used in policy mode.
    #[arg(long, env = "MAGMA_BACKRUN_POOL_MAX", default_value_t = 4096)]
    pub backrun_pool_max: usize,
}
