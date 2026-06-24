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

/// End-to-end IPC test: drive the **real** `EthTxPoolIpcStream` server (from the
/// pinned `monad-eth-txpool-ipc`) against the production `run_txpool_priority_loop`
/// over an actual Unix socket. This exercises — and pins — the full wire contract:
/// the snapshot handshake frame, length-delimited bincode `EthTxPoolEvent` batches
/// inbound, and RLP-encoded `EthTxPoolIpcTx` replies outbound. A future bump of the
/// monad-bft `rev` that changes the framing or types will break these tests rather
/// than silently desyncing against live validators.
#[cfg(test)]
mod ipc_integration_tests {
    use super::*;

    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};

    use alloy_consensus::{Signed, TxEip1559, TxEnvelope};
    use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256};
    use monad_eth_txpool_ipc::EthTxPoolIpcStream;
    use monad_eth_txpool_types::{EthTxPoolEvent, EthTxPoolSnapshot};

    use crate::policy::{
        compute_priority, encode_tob_priority, gateway_call_calldata, PolicyConfig,
    };

    const GW: Address = address!("00000000000000000000000000000000000000bb");
    const OTHER: Address = address!("00000000000000000000000000000000000000cc");

    /// A short, collision-free socket path under the system temp dir (well within
    /// the AF_UNIX `sun_path` limit).
    fn unique_socket_path() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "magma-ipc-it-{}-{nanos}-{n}.sock",
            std::process::id()
        ))
    }

    fn sig() -> Signature {
        Signature::new(U256::from(1u64), U256::from(1u64), false)
    }

    fn tx_hash(byte: u8) -> B256 {
        let mut b = [0u8; 32];
        b[0] = byte;
        B256::from(b)
    }

    fn eip1559(to: Address, input: Bytes, hash: B256) -> TxEnvelope {
        TxEnvelope::Eip1559(Signed::new_unchecked(
            TxEip1559 {
                chain_id: 1,
                nonce: 0,
                gas_limit: 21_000,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 5,
                to: TxKind::Call(to),
                value: U256::ZERO,
                access_list: Default::default(),
                input,
            },
            sig(),
            hash,
        ))
    }

    fn insert_event(hash: B256, from: Address, tx: TxEnvelope) -> EthTxPoolEvent {
        EthTxPoolEvent {
            tx_hash: hash,
            action: EthTxPoolEventType::Insert {
                address: from,
                owned: false,
                tx,
            },
        }
    }

    fn policy_mode() -> (Arc<PolicyConfig>, PriorityMode) {
        let policy = Arc::new(PolicyConfig::for_test(GW, 0));
        let mode = PriorityMode::Policy {
            policy: policy.clone(),
            fallback: U256::from(0xffffu64),
            ttl: Duration::from_millis(2500),
            max_entries: 1024,
        };
        (policy, mode)
    }

    /// Run one event batch through a real IPC server + the production loop.
    ///
    /// Returns the txs the server saw reinjected and the loop's metrics. We
    /// gate teardown on the metrics reaching `(expect_inserts, expect_prioritized,
    /// expect_skipped)` so assertions aren't racy against the async loop.
    async fn run_roundtrip(
        events: Vec<EthTxPoolEvent>,
        mode: PriorityMode,
        expect_reinjections: usize,
        expect_inserts: u64,
        expect_prioritized: u64,
        expect_skipped: u64,
    ) -> (Vec<EthTxPoolIpcTx>, Arc<Metrics>) {
        let path = unique_socket_path();
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).expect("bind unix socket");

        // Server side: accept the sidecar's connection, push the batch, and
        // collect everything it reinjects.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut srv = EthTxPoolIpcStream::new(
                stream,
                EthTxPoolSnapshot {
                    txs: HashSet::new(),
                },
            );
            srv.send_tx_events(events).expect("send events");

            let mut got = Vec::new();
            while got.len() < expect_reinjections {
                match tokio::time::timeout(Duration::from_secs(5), srv.next()).await {
                    Ok(Some(tx)) => got.push(tx),
                    _ => break,
                }
            }
            // Drain window to catch any *unexpected* extra reinjection (e.g. a
            // non-gateway tx that should have been skipped).
            while let Ok(Some(tx)) =
                tokio::time::timeout(Duration::from_millis(500), srv.next()).await
            {
                got.push(tx);
            }
            got
        });

        let metrics = Metrics::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let client = tokio::spawn(run_txpool_priority_loop(
            path.clone(),
            mode,
            metrics.clone(),
            shutdown_rx,
        ));

        let got = server.await.expect("server task");

        // Wait for the loop to finish processing the batch before asserting on
        // counters, so metric reads aren't racing the async send path.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let s = metrics.snapshot();
            if s.tx_inserts_observed >= expect_inserts
                && s.tx_prioritized >= expect_prioritized
                && s.tx_skipped_non_gateway >= expect_skipped
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let _ = shutdown_tx.send(true);
        let _ = client.await;
        let _ = std::fs::remove_file(&path);

        (got, metrics)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn gateway_bid_is_reinjected_with_computed_priority() {
        let (policy, mode) = policy_mode();
        let bid = U256::from(7_000_000u64);
        let gw_tx = eip1559(GW, gateway_call_calldata(bid), tx_hash(1));
        // A lead-block (TOB) gateway bid is reinjected with the bit-255-tagged scalar.
        let expected = encode_tob_priority(compute_priority(&gw_tx, &policy));

        let (got, metrics) = run_roundtrip(
            vec![insert_event(tx_hash(1), OTHER, gw_tx)],
            mode,
            1,
            1,
            1,
            0,
        )
        .await;

        assert_eq!(got.len(), 1, "exactly one reinjection for the gateway bid");
        assert_eq!(
            got[0].priority, expected,
            "reinjected priority must match the policy's computed value across the wire"
        );

        let snap = metrics.snapshot();
        assert_eq!(snap.tx_inserts_observed, 1);
        assert_eq!(snap.tx_prioritized, 1);
        assert_eq!(snap.tx_skipped_non_gateway, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_gateway_tx_is_observed_but_not_reinjected() {
        let (_policy, mode) = policy_mode();
        let vanilla = eip1559(OTHER, Bytes::new(), tx_hash(2));

        let (got, metrics) = run_roundtrip(
            vec![insert_event(tx_hash(2), OTHER, vanilla)],
            mode,
            0,
            1,
            0,
            1,
        )
        .await;

        assert!(got.is_empty(), "vanilla traffic must not be reinjected");
        let snap = metrics.snapshot();
        assert_eq!(snap.tx_inserts_observed, 1);
        assert_eq!(snap.tx_prioritized, 0);
        assert_eq!(snap.tx_skipped_non_gateway, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_insert_is_reinjected_once() {
        let (_policy, mode) = policy_mode();
        let bid = U256::from(5u64);
        // Two Insert events carrying the same tx_hash within one batch: the second
        // is a dedup echo and must be dropped before scoring.
        let gw_tx = eip1559(GW, gateway_call_calldata(bid), tx_hash(3));
        let dup = eip1559(GW, gateway_call_calldata(bid), tx_hash(3));

        let (got, metrics) = run_roundtrip(
            vec![
                insert_event(tx_hash(3), OTHER, gw_tx),
                insert_event(tx_hash(3), OTHER, dup),
            ],
            mode,
            1,
            1,
            1,
            0,
        )
        .await;

        assert_eq!(
            got.len(),
            1,
            "duplicate tx_hash must dedup to a single reinjection"
        );
        let snap = metrics.snapshot();
        // The echo is dropped before `record_insert`, so only one is observed/prioritized.
        assert_eq!(snap.tx_inserts_observed, 1);
        assert_eq!(snap.tx_prioritized, 1);
    }
}
