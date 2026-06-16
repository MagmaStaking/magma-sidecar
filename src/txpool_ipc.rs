//! Txpool Unix socket: subscribe to `EthTxPoolEvent` batches, score each `Insert`
//! by the configured tip policy, and re-inject as RLP `EthTxPoolIpcTx` over the
//! same socket. Same wire protocol as `monad-eth-txpool-ipc`.
//!
//! In `Policy` mode the loop owns a [`PendingPool`] so it can pair backrun bids
//! with their target tx and emit *multiple* reinjections per Insert (the target,
//! then the bid). See `crate::backrun` for the pairing logic.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_primitives::TxHash;
use alloy_primitives::U256;
use futures::{SinkExt, StreamExt};
use monad_eth_txpool_ipc::EthTxPoolIpcClient;
use monad_eth_txpool_types::{EthTxPoolEventType, EthTxPoolIpcTx};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::backrun::{PendingPool, Reinjection};
use crate::metrics::{IpcState, Metrics};
use crate::policy::PolicyConfig;

const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// How priority is decided for each Insert.
#[derive(Debug, Clone)]
pub enum PriorityMode {
    /// Stamp every tx with the same constant (legacy / fallback when no policy
    /// file is supplied). This mode does **not** filter on the gateway
    /// allowlist because it has none — every Insert is reinjected.
    Constant(U256),
    /// Score each tx via the tip policy, pairing backrun bids with their target.
    /// Only transactions whose `to` is an allowlisted `MagmaSearcherGateway`
    /// (plus the targets they reference) are reinjected; vanilla traffic is
    /// observed (for matching + metrics) but left to the node's own ordering.
    Policy {
        policy: Arc<PolicyConfig>,
        fallback: U256,
        ttl: Duration,
        max_entries: usize,
    },
}

impl PriorityMode {
    fn describe(&self) -> &'static str {
        match self {
            PriorityMode::Constant(_) => "constant",
            PriorityMode::Policy { .. } => "policy",
        }
    }
}

/// Per-mode runtime state, held across reconnects so the backrun pool isn't lost
/// on a transient socket drop.
enum ModeState {
    Constant(U256),
    // Boxed: `PendingPool` is much larger than the `Constant` variant.
    Policy(Box<PendingPool>),
}

impl ModeState {
    fn from_mode(mode: &PriorityMode) -> Self {
        match mode {
            PriorityMode::Constant(p) => ModeState::Constant(*p),
            PriorityMode::Policy {
                policy,
                fallback,
                ttl,
                max_entries,
            } => ModeState::Policy(Box::new(PendingPool::new(
                policy.clone(),
                *fallback,
                *ttl,
                *max_entries,
            ))),
        }
    }
}

pub async fn run_txpool_priority_loop(
    socket_path: PathBuf,
    mode: PriorityMode,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) {
    if *shutdown.borrow() {
        return;
    }

    info!(mode = mode.describe(), "starting txpool reprioritizer");
    metrics.set_ipc_state(IpcState::Connecting);

    // State persists across reconnects: the pool keeps pending bids / cached
    // targets warm, and `sent` dedups echoes of txs we already reinjected.
    let mut state = ModeState::from_mode(&mode);
    let mut sent: HashMap<TxHash, U256> = HashMap::new();

    loop {
        if *shutdown.borrow() {
            metrics.set_ipc_state(IpcState::Disabled);
            return;
        }

        metrics.set_ipc_state(IpcState::Connecting);
        let connect = EthTxPoolIpcClient::new(&socket_path).await;
        let Ok((mut client, _snapshot)) = connect else {
            error!(path = %socket_path.display(), "txpool IPC connect failed; retrying");
            sleep_or_shutdown(RECONNECT_INTERVAL, &mut shutdown).await;
            continue;
        };

        info!(path = %socket_path.display(), "connected to Monad txpool IPC");
        metrics.set_ipc_state(IpcState::Connected);
        metrics.record_reconnect();

        'connection: loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        metrics.set_ipc_state(IpcState::Disabled);
                        return;
                    }
                }
                batch = client.next() => {
                    let Some(batch) = batch else {
                        warn!("txpool IPC stream ended; reconnecting");
                        break 'connection;
                    };
                    for ev in batch {
                        match &ev.action {
                            EthTxPoolEventType::Insert { .. } => metrics.record_event("insert"),
                            EthTxPoolEventType::Commit => metrics.record_event("commit"),
                            EthTxPoolEventType::Drop { .. } => metrics.record_event("drop"),
                            EthTxPoolEventType::Evict { .. } => metrics.record_event("evict"),
                        }
                        match ev.action {
                            EthTxPoolEventType::Insert { tx, .. } => {
                                let hash = ev.tx_hash;
                                // Echo of a tx we already reinjected: ignore.
                                if sent.contains_key(&hash) {
                                    continue;
                                }
                                metrics.record_insert();

                                let reinjections: Vec<Reinjection> = match &mut state {
                                    ModeState::Constant(p) => {
                                        vec![Reinjection { hash, tx, priority: *p }]
                                    }
                                    ModeState::Policy(pool) => {
                                        let expired = pool.prune(Instant::now());
                                        if expired > 0 {
                                            metrics.add_backrun_expired(expired);
                                        }
                                        let res = pool.observe(hash, tx);
                                        if res.skipped_non_gateway {
                                            debug!(
                                                ?hash,
                                                "skipping reinjection: tx not bound for an allowlisted gateway"
                                            );
                                            metrics.record_skipped_non_gateway();
                                        }
                                        metrics.add_backrun_pairs(res.pairs_matched);
                                        metrics.add_backrun_pended(res.bids_pended);
                                        metrics.set_backrun_pending(pool.pending_len() as i64);
                                        metrics.set_backrun_cache(pool.cache_len() as i64);
                                        res.reinjections
                                    }
                                };

                                let mut send_failed = false;
                                for r in reinjections {
                                    if sent.contains_key(&r.hash) {
                                        continue;
                                    }
                                    debug!(hash = ?r.hash, priority = %r.priority, "reinjecting with computed priority");
                                    let ipc = EthTxPoolIpcTx {
                                        tx: r.tx,
                                        priority: r.priority,
                                        extra_data: vec![],
                                    };
                                    if let Err(e) = client.send(ipc).await {
                                        warn!(?e, "txpool IPC send failed; reconnecting");
                                        metrics.record_send_failure();
                                        send_failed = true;
                                        break;
                                    }
                                    metrics.record_prioritized();
                                    sent.insert(r.hash, r.priority);
                                }
                                if send_failed {
                                    break 'connection;
                                }
                            }
                            EthTxPoolEventType::Commit
                            | EthTxPoolEventType::Drop { .. }
                            | EthTxPoolEventType::Evict { .. } => {
                                sent.remove(&ev.tx_hash);
                                if let ModeState::Policy(pool) = &mut state {
                                    pool.forget(ev.tx_hash);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn sleep_or_shutdown(duration: Duration, shutdown: &mut watch::Receiver<bool>) {
    tokio::select! {
        _ = tokio::time::sleep(duration) => {}
        _ = shutdown.changed() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::policy::PolicyConfig;
    use alloy_primitives::address;

    #[test]
    fn describe_reports_mode() {
        assert_eq!(
            PriorityMode::Constant(U256::from(1u64)).describe(),
            "constant"
        );
        let policy =
            PolicyConfig::for_test(address!("00000000000000000000000000000000000000bb"), 0);
        let mode = PriorityMode::Policy {
            policy: Arc::new(policy),
            fallback: U256::from(0xffffu64),
            ttl: Duration::from_millis(2500),
            max_entries: 16,
        };
        assert_eq!(mode.describe(), "policy");
    }

    #[test]
    fn mode_state_builds_pool_for_policy() {
        let policy =
            PolicyConfig::for_test(address!("00000000000000000000000000000000000000bb"), 0);
        let mode = PriorityMode::Policy {
            policy: Arc::new(policy),
            fallback: U256::from(0xffffu64),
            ttl: Duration::from_millis(2500),
            max_entries: 16,
        };
        match ModeState::from_mode(&mode) {
            ModeState::Policy(pool) => {
                assert_eq!(pool.pending_len(), 0);
                assert_eq!(pool.cache_len(), 0);
            }
            ModeState::Constant(_) => panic!("expected policy state"),
        }
    }
}
