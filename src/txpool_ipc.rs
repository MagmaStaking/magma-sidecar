//! Txpool Unix socket: subscribe to `EthTxPoolEvent` batches, score each `Insert`
//! by the configured tip policy, and re-inject as RLP `EthTxPoolIpcTx` over the
//! same socket. Same wire protocol as `monad-eth-txpool-ipc`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy_consensus::TxEnvelope;
use alloy_primitives::TxHash;
use alloy_primitives::U256;
use futures::{SinkExt, StreamExt};
use monad_eth_txpool_ipc::EthTxPoolIpcClient;
use monad_eth_txpool_types::{EthTxPoolEventType, EthTxPoolIpcTx};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::metrics::{IpcState, Metrics};
use crate::policy::{compute_priority, PolicyConfig};

const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// How priority is decided for each Insert.
#[derive(Debug, Clone)]
pub enum PriorityMode {
    /// Stamp every tx with the same constant (legacy / fallback).
    Constant(U256),
    /// Score each tx via `policy::compute_priority`.
    Policy {
        policy: Arc<PolicyConfig>,
        fallback: U256,
    },
}

impl PriorityMode {
    fn describe(&self) -> &'static str {
        match self {
            PriorityMode::Constant(_) => "constant",
            PriorityMode::Policy { .. } => "policy",
        }
    }

    /// Decide the outbound priority for a tx under this mode. Extracted from the
    /// IPC loop so it's unit-testable without a real Unix socket.
    pub fn decide_priority(&self, tx: &TxEnvelope) -> U256 {
        match self {
            PriorityMode::Constant(p) => *p,
            PriorityMode::Policy { policy, fallback } => {
                let p = compute_priority(tx, policy);
                if p.is_zero() {
                    *fallback
                } else {
                    p
                }
            }
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
        let mut prioritized: HashSet<TxHash> = HashSet::new();

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
                                if prioritized.contains(&hash) {
                                    continue;
                                }
                                metrics.record_insert();
                                let priority = mode.decide_priority(&tx);
                                debug!(?hash, %priority, "reinjecting with computed priority");
                                let ipc = EthTxPoolIpcTx {
                                    tx,
                                    priority,
                                    extra_data: vec![],
                                };
                                if let Err(e) = client.send(ipc).await {
                                    warn!(?e, "txpool IPC send failed; reconnecting");
                                    metrics.record_send_failure();
                                    break 'connection;
                                }
                                metrics.record_prioritized();
                                prioritized.insert(hash);
                            }
                            EthTxPoolEventType::Commit
                            | EthTxPoolEventType::Drop { .. }
                            | EthTxPoolEventType::Evict { .. } => {
                                prioritized.remove(&ev.tx_hash);
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

    use alloy_consensus::{Signed, TxEip1559};
    use alloy_primitives::{address, b256, Signature, TxKind};

    fn dummy_envelope(value_wei: u64, to: alloy_primitives::Address) -> TxEnvelope {
        let sig = Signature::new(U256::from(1u64), U256::from(1u64), false);
        TxEnvelope::Eip1559(Signed::new_unchecked(
            TxEip1559 {
                chain_id: 1,
                nonce: 0,
                gas_limit: 21_000,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 5,
                to: TxKind::Call(to),
                value: U256::from(value_wei),
                access_list: Default::default(),
                input: Default::default(),
            },
            sig,
            b256!("00000000000000000000000000000000000000000000000000000000000000aa"),
        ))
    }

    #[test]
    fn constant_mode_always_returns_constant() {
        let mode = PriorityMode::Constant(U256::from(0xffffu64));
        let tx = dummy_envelope(1_000_000, address!("00000000000000000000000000000000000000aa"));
        assert_eq!(mode.decide_priority(&tx), U256::from(0xffffu64));
    }

    #[test]
    fn policy_mode_uses_computed_score_when_nonzero() {
        let gw = address!("00000000000000000000000000000000000000bb");
        let policy = PolicyConfig::from_parts(0, &[(gw, 1)]);
        let mode = PriorityMode::Policy {
            policy: Arc::new(policy),
            fallback: U256::from(0xffffu64),
        };
        let tx = dummy_envelope(1_000_000, gw);
        // 5 * 21_000 + 1_000_000 = 1_105_000
        assert_eq!(mode.decide_priority(&tx), U256::from(1_105_000u64));
    }

    #[test]
    fn policy_mode_falls_back_when_score_zero() {
        // Build a tx whose computed priority is zero: zero priority fee + non-gateway.
        let sig = Signature::new(U256::from(1u64), U256::from(1u64), false);
        let zero_fee_tx = TxEnvelope::Eip1559(Signed::new_unchecked(
            TxEip1559 {
                chain_id: 1,
                nonce: 0,
                gas_limit: 21_000,
                max_fee_per_gas: 0,
                max_priority_fee_per_gas: 0,
                to: TxKind::Call(address!("00000000000000000000000000000000000000cc")),
                value: U256::ZERO,
                access_list: Default::default(),
                input: Default::default(),
            },
            sig,
            b256!("00000000000000000000000000000000000000000000000000000000000000bb"),
        ));
        let mode = PriorityMode::Policy {
            policy: Arc::new(PolicyConfig::default()),
            fallback: U256::from(0x42u64),
        };
        assert_eq!(mode.decide_priority(&zero_fee_tx), U256::from(0x42u64));
    }
}
