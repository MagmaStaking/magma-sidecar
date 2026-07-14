//! Txpool Unix socket: subscribe to `EthTxPoolEvent` batches, score each `Insert`
//! by the configured tip policy, and re-inject as RLP `EthTxPoolIpcTx` over the
//! same socket. Same wire protocol as `monad-eth-txpool-ipc`.
//!
//! The loop owns a [`PendingPool`] so it can pair backrun bids with their target
//! tx and emit *multiple* reinjections per Insert (the target, then the bid).
//! See `crate::backrun` for the pairing logic.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_primitives::TxHash;
use alloy_primitives::U256;
use futures::{SinkExt, StreamExt};
use monad_eth_txpool_ipc::EthTxPoolIpcClient;
use monad_eth_txpool_types::{EthTxPoolEventType, EthTxPoolIpcTx};
use tokio::sync::watch;
use tracing::{error, info, trace, warn};

use crate::backrun::{PendingPool, Reinjection};
use crate::metrics::{IpcState, Metrics};
use crate::policy::PolicyConfig;

const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// The tip policy applied to each Insert. Only transactions whose `to` is an
/// allowlisted `MagmaSearcherGateway` (plus the targets they reference) are
/// reinjected; vanilla traffic is observed (for matching + metrics) but left to
/// the node's own ordering.
#[derive(Debug, Clone)]
pub struct PriorityMode {
    pub policy: Arc<PolicyConfig>,
    /// Priority used for a gateway tx whose computed score is exactly zero.
    pub fallback: U256,
    pub ttl: Duration,
    pub max_cache_entries: usize,
    pub max_pending_entries: usize,
    pub sent_cache_max: usize,
}

struct SentCache {
    priorities: HashMap<TxHash, U256>,
    order: VecDeque<TxHash>,
    max_entries: usize,
}

impl SentCache {
    fn new(max_entries: usize) -> Self {
        Self {
            priorities: HashMap::new(),
            order: VecDeque::new(),
            max_entries: max_entries.max(1),
        }
    }

    fn contains(&self, hash: &TxHash) -> bool {
        self.priorities.contains_key(hash)
    }

    /// Insert a sent hash and return the number of oldest entries evicted.
    fn insert(&mut self, hash: TxHash, priority: U256) -> u64 {
        if self.priorities.insert(hash, priority).is_none() {
            self.order.push_back(hash);
        }

        // Lifecycle events remove entries from the map without an O(n) queue
        // scan. Compact stale queue entries before they can become another
        // unbounded structure.
        if self.order.len() > self.max_entries.saturating_mul(2) {
            let priorities = &self.priorities;
            self.order
                .retain(|candidate| priorities.contains_key(candidate));
        }

        let mut evicted = 0;
        while self.priorities.len() > self.max_entries {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if self.priorities.remove(&oldest).is_some() {
                evicted += 1;
            }
        }
        evicted
    }

    fn remove(&mut self, hash: &TxHash) {
        self.priorities.remove(hash);
    }

    fn len(&self) -> usize {
        self.priorities.len()
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

    info!("starting txpool reprioritizer");
    metrics.set_ipc_state(IpcState::Connecting);

    // State persists across reconnects: the pool keeps pending bids / cached
    // targets warm, and `sent` dedups echoes of txs we already reinjected.
    let mut pool = PendingPool::new(
        mode.policy,
        mode.fallback,
        mode.ttl,
        mode.max_cache_entries,
        mode.max_pending_entries,
    );
    let mut sent = SentCache::new(mode.sent_cache_max);

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
                                if sent.contains(&hash) {
                                    continue;
                                }
                                metrics.record_insert();

                                let reinjections: Vec<Reinjection> = {
                                    let expired = pool.prune(Instant::now());
                                    if expired > 0 {
                                        metrics.add_backrun_expired(expired);
                                    }
                                    let res = pool.observe(hash, tx);
                                    if res.skipped_non_gateway {
                                        trace!(
                                            ?hash,
                                            "skipping reinjection: tx not bound for an allowlisted gateway"
                                        );
                                        metrics.record_skipped_non_gateway();
                                    }
                                    if res.skipped_invalid_gateway {
                                        trace!(
                                            ?hash,
                                            "skipping reinjection: invalid or unsupported gateway call"
                                        );
                                        metrics.record_skipped_invalid_gateway();
                                    }
                                    metrics.add_backrun_pairs(res.pairs_matched);
                                    metrics.add_backrun_pended(res.bids_pended);
                                    metrics.add_backrun_evicted(res.bids_evicted);
                                    metrics.set_backrun_pending(pool.pending_len() as i64);
                                    metrics.set_backrun_cache(pool.cache_len() as i64);
                                    res.reinjections
                                };

                                let mut send_failed = false;
                                for r in reinjections {
                                    if sent.contains(&r.hash) {
                                        continue;
                                    }
                                    trace!(hash = ?r.hash, priority = %r.priority, "reinjecting with computed priority");
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
                                    let evicted = sent.insert(r.hash, r.priority);
                                    metrics.add_sent_cache_evictions(evicted);
                                    metrics.set_sent_cache(sent.len() as i64);
                                }
                                if send_failed {
                                    break 'connection;
                                }
                            }
                            EthTxPoolEventType::Commit
                            | EthTxPoolEventType::Drop { .. }
                            | EthTxPoolEventType::Evict { .. } => {
                                sent.remove(&ev.tx_hash);
                                metrics.set_sent_cache(sent.len() as i64);
                                pool.forget(ev.tx_hash);
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
    fn builds_empty_backrun_pool() {
        let policy =
            PolicyConfig::for_test(address!("00000000000000000000000000000000000000bb"), 0);
        let mode = PriorityMode {
            policy: Arc::new(policy),
            fallback: U256::from(0xffffu64),
            ttl: Duration::from_millis(2500),
            max_cache_entries: 16,
            max_pending_entries: 16,
            sent_cache_max: 16,
        };
        let pool = PendingPool::new(
            mode.policy,
            mode.fallback,
            mode.ttl,
            mode.max_cache_entries,
            mode.max_pending_entries,
        );
        assert_eq!(pool.pending_len(), 0);
        assert_eq!(pool.cache_len(), 0);
    }

    #[test]
    fn sent_cache_evicts_oldest_hash() {
        let mut sent = SentCache::new(2);
        let first = TxHash::repeat_byte(1);
        let second = TxHash::repeat_byte(2);
        let third = TxHash::repeat_byte(3);

        assert_eq!(sent.insert(first, U256::from(1)), 0);
        assert_eq!(sent.insert(second, U256::from(2)), 0);
        assert_eq!(sent.insert(third, U256::from(3)), 1);
        assert!(!sent.contains(&first));
        assert!(sent.contains(&second));
        assert!(sent.contains(&third));
        assert_eq!(sent.len(), 2);
    }

    #[test]
    fn sent_cache_compacts_stale_order_entries() {
        let mut sent = SentCache::new(2);
        for byte in 0..10 {
            let hash = TxHash::repeat_byte(byte);
            sent.insert(hash, U256::from(byte));
            sent.remove(&hash);
        }
        assert!(sent.order.len() <= sent.max_entries.saturating_mul(2));
        assert_eq!(sent.len(), 0);
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
        let mode = PriorityMode {
            policy: policy.clone(),
            fallback: U256::from(0xffffu64),
            ttl: Duration::from_millis(2500),
            max_cache_entries: 1024,
            max_pending_entries: 1024,
            sent_cache_max: 4096,
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
