//! Stateful backrun bid/target pairing.
//!
//! The flat `policy::compute_priority` scalar ranks every gateway tx absolutely,
//! which is correct for top-of-block competition but wrong for backruns: a large
//! backrun bid sorts *ahead of* the very tx it wants to land behind. This module
//! keeps the small amount of state needed to instead place a backrun bid
//! *immediately behind its target*, using the structured priority encoders in
//! [`crate::policy`].
//!
//! Design of this pairing model:
//! - **Bidirectional matching.** We index pending bids by target hash and flush
//!   them when the target shows up later, so pairing is independent of arrival
//!   order (rather than only pairing when the target is already pooled at bid
//!   time).
//! - **Competitive bids.** Every bid for a target is streamed; the bit-field
//!   self-orders them by total realized validator value (the same
//!   `priority_fee * gas_limit + bidAmount` scalar TOB bids use), directly behind
//!   the single opportunity, so the seated backrun pays the proposer the most.
//! - **TTL + capacity + metrics.** The cache and pending bids expire on a
//!   configurable TTL and are capacity-bounded; pairing counters are surfaced for
//!   observability.
//!
//! We cache and re-inject the decoded alloy `TxEnvelope` (which round-trips
//! through the IPC wire format) rather than carrying the original RLP separately.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_consensus::{Transaction, TxEnvelope};
use alloy_primitives::{TxHash, U256};

use crate::policy::{
    compute_priority, decode_gateway_call, encode_backrun_bid_priority,
    encode_opportunity_priority, encode_tob_priority, PolicyConfig,
};

/// A transaction the sidecar wants the node to re-inject with `priority`.
#[derive(Debug, Clone)]
pub struct Reinjection {
    /// Hash of `tx`, carried explicitly so the caller can dedup without recomputing.
    pub hash: TxHash,
    pub tx: TxEnvelope,
    pub priority: U256,
}

/// What happened while observing one Insert. The caller folds the counters into
/// metrics and sends every `reinjection`.
#[derive(Debug, Default)]
pub struct ObserveResult {
    pub reinjections: Vec<Reinjection>,
    /// Number of (target -> bid) pairings completed during this observe.
    pub pairs_matched: u64,
    /// Number of bids newly parked because their target wasn't pooled yet.
    pub bids_pended: u64,
    /// Number of oldest parked bids evicted to enforce the pending-bid cap.
    pub bids_evicted: u64,
    /// True when the tx was vanilla traffic left for the node's own ordering.
    pub skipped_non_gateway: bool,
    /// True when the tx targeted the gateway but did not contain a valid gateway call.
    pub skipped_invalid_gateway: bool,
}

struct CachedTx {
    tx: TxEnvelope,
    seen_at: Instant,
}

struct PendingBid {
    hash: TxHash,
    tx: TxEnvelope,
    /// `compute_priority` of the bid tx (`priority_fee * gas_limit + bidAmount`) —
    /// the total realized validator value we rank competing backruns by. Computed at
    /// park time (it depends only on the static tx + policy, so it's stable until
    /// the target arrives).
    scalar: U256,
    target: TxHash,
    seen_at: Instant,
}

/// Holds recently-seen txs (potential backrun targets) and bids waiting for their
/// target to appear. Not thread-safe; owned by the single IPC loop task.
pub struct PendingPool {
    policy: Arc<PolicyConfig>,
    fallback: U256,
    ttl: Duration,
    max_cache_entries: usize,
    max_pending_entries: usize,

    /// Recently-seen txs keyed by hash, any of which a future bid may target.
    cache: HashMap<TxHash, CachedTx>,
    /// Insertion order for FIFO capacity eviction / TTL pruning of `cache`.
    cache_order: VecDeque<TxHash>,

    /// Bids whose target hasn't been seen yet, keyed by target hash.
    pending: HashMap<TxHash, Vec<PendingBid>>,
    /// Insertion order of pending bids for FIFO TTL pruning: (target, bid).
    pending_order: VecDeque<(TxHash, TxHash)>,
    pending_count: usize,

    /// Targets we've already emitted an opportunity reinjection for, so competing
    /// bids that arrive later don't re-stream the target.
    streamed_targets: HashSet<TxHash>,
}

impl PendingPool {
    pub fn new(
        policy: Arc<PolicyConfig>,
        fallback: U256,
        ttl: Duration,
        max_cache_entries: usize,
        max_pending_entries: usize,
    ) -> Self {
        Self {
            policy,
            fallback,
            ttl,
            max_cache_entries: max_cache_entries.max(1),
            max_pending_entries: max_pending_entries.max(1),
            cache: HashMap::new(),
            cache_order: VecDeque::new(),
            pending: HashMap::new(),
            pending_order: VecDeque::new(),
            pending_count: 0,
            streamed_targets: HashSet::new(),
        }
    }

    /// Current number of parked (unmatched) bids.
    pub fn pending_len(&self) -> usize {
        self.pending_count
    }

    /// Current number of cached candidate-target txs.
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Observe an inserted tx. Convenience wrapper using the wall clock.
    pub fn observe(&mut self, hash: TxHash, tx: TxEnvelope) -> ObserveResult {
        self.observe_at(hash, tx, Instant::now())
    }

    /// Observe an inserted tx at an explicit instant (for deterministic tests).
    pub fn observe_at(&mut self, hash: TxHash, tx: TxEnvelope, now: Instant) -> ObserveResult {
        let mut res = ObserveResult::default();

        // Anything we see could later be referenced as a backrun target.
        self.insert_cache(hash, tx.clone(), now);

        let is_gateway = tx
            .to()
            .is_some_and(|to| self.policy.is_allowlisted_gateway(&to));

        if is_gateway {
            if let Some(call) = decode_gateway_call(&tx) {
                if call.is_tob() {
                    // Top-of-block bid: rank by the fee+bid scalar, bit-255 tagged.
                    let scalar = compute_priority(&tx, &self.policy);
                    let priority = if scalar.is_zero() {
                        self.fallback
                    } else {
                        encode_tob_priority(scalar)
                    };
                    res.reinjections.push(Reinjection { hash, tx, priority });
                    return res;
                }

                // Backrun bid: rank by the same fee+bid scalar as TOB (total realized
                // validator value), then try to pair with its target.
                let bid_target = call.target_tx_hash;
                let scalar = compute_priority(&tx, &self.policy);
                if self.cache.contains_key(&bid_target) {
                    self.emit_opportunity_once(bid_target, now, &mut res);
                    res.reinjections.push(Reinjection {
                        hash,
                        tx,
                        priority: encode_backrun_bid_priority(bid_target, scalar),
                    });
                    res.pairs_matched += 1;
                } else {
                    res.bids_evicted += self.push_pending(PendingBid {
                        hash,
                        tx,
                        scalar,
                        target: bid_target,
                        seen_at: now,
                    });
                    res.bids_pended += 1;
                }
                return res;
            }

            // Gateway address but not a valid `magmaSearcherGatewayCall` (e.g.
            // a `receive()` top-up or malformed calldata). It carries no
            // sidecar-recognized bid and must not amplify node work via IPC.
            res.skipped_invalid_gateway = true;
            return res;
        }

        // Non-gateway tx. If bids are waiting on this hash, it's their target.
        if let Some(bids) = self.pending.remove(&hash) {
            self.emit_opportunity_once(hash, now, &mut res);
            for b in bids {
                self.pending_count -= 1;
                res.reinjections.push(Reinjection {
                    hash: b.hash,
                    tx: b.tx,
                    priority: encode_backrun_bid_priority(hash, b.scalar),
                });
                res.pairs_matched += 1;
            }
            return res;
        }

        // Plain vanilla traffic: cached as a candidate target, but left for the
        // node's own ordering.
        res.skipped_non_gateway = true;
        res
    }

    /// Drop expired cache entries and pending bids. Returns the number of pending
    /// bids that expired without ever matching a target.
    pub fn prune(&mut self, now: Instant) -> u64 {
        // Cache: pop expired from the front (oldest first).
        while let Some(front) = self.cache_order.front().copied() {
            match self.cache.get(&front) {
                Some(c) if now.duration_since(c.seen_at) >= self.ttl => {
                    self.cache.remove(&front);
                    self.streamed_targets.remove(&front);
                    self.cache_order.pop_front();
                }
                Some(_) => break, // front not expired; the rest are newer.
                None => {
                    self.cache_order.pop_front();
                } // stale order entry (already removed).
            }
        }

        // Pending bids: same FIFO discipline.
        let mut expired = 0u64;
        while let Some((target, bid)) = self.pending_order.front().copied() {
            let mut handled = false;
            if let Some(vec) = self.pending.get_mut(&target) {
                if let Some(pos) = vec.iter().position(|b| b.hash == bid) {
                    if now.duration_since(vec[pos].seen_at) >= self.ttl {
                        vec.remove(pos);
                        self.pending_count -= 1;
                        expired += 1;
                        if vec.is_empty() {
                            self.pending.remove(&target);
                        }
                        self.pending_order.pop_front();
                        continue;
                    } else {
                        // Oldest pending bid not yet expired; stop.
                        break;
                    }
                }
                handled = true;
            }
            // Order entry refers to a bid that was already matched/removed.
            let _ = handled;
            self.pending_order.pop_front();
        }

        expired
    }

    /// Forget a tx that left the pool (committed/dropped/evicted), wherever it
    /// appears: as a cached target, a streamed target, a pending target, or a
    /// pending bid.
    pub fn forget(&mut self, hash: TxHash) {
        self.cache.remove(&hash);
        self.streamed_targets.remove(&hash);

        if let Some(vec) = self.pending.remove(&hash) {
            self.pending_count -= vec.len();
        }
        for vec in self.pending.values_mut() {
            if let Some(pos) = vec.iter().position(|b| b.hash == hash) {
                vec.remove(pos);
                self.pending_count -= 1;
                break;
            }
        }
        self.pending.retain(|_, v| !v.is_empty());
    }

    /// Emit the opportunity (target) reinjection at most once per target.
    fn emit_opportunity_once(&mut self, target: TxHash, _now: Instant, res: &mut ObserveResult) {
        if self.streamed_targets.contains(&target) {
            return;
        }
        if let Some(cached) = self.cache.get(&target) {
            res.reinjections.push(Reinjection {
                hash: target,
                tx: cached.tx.clone(),
                priority: encode_opportunity_priority(target),
            });
            self.streamed_targets.insert(target);
        }
    }

    fn insert_cache(&mut self, hash: TxHash, tx: TxEnvelope, now: Instant) {
        if self
            .cache
            .insert(hash, CachedTx { tx, seen_at: now })
            .is_none()
        {
            self.cache_order.push_back(hash);
        }
        while self.cache.len() > self.max_cache_entries {
            match self.cache_order.pop_front() {
                Some(old) => {
                    if self.cache.remove(&old).is_some() {
                        self.streamed_targets.remove(&old);
                    }
                }
                None => break,
            }
        }
    }

    fn push_pending(&mut self, bid: PendingBid) -> u64 {
        let key = bid.target;
        let bid_hash = bid.hash;
        self.pending.entry(key).or_default().push(bid);
        self.pending_order.push_back((key, bid_hash));
        self.pending_count += 1;

        let mut evicted = 0;
        while self.pending_count > self.max_pending_entries {
            let Some((target, hash)) = self.pending_order.pop_front() else {
                break;
            };
            let mut remove_target = false;
            if let Some(bids) = self.pending.get_mut(&target) {
                if let Some(pos) = bids.iter().position(|candidate| candidate.hash == hash) {
                    bids.remove(pos);
                    self.pending_count -= 1;
                    evicted += 1;
                    remove_target = bids.is_empty();
                }
            }
            if remove_target {
                self.pending.remove(&target);
            }
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_consensus::{Signed, TxEip1559};
    use alloy_primitives::{address, b256, Address, Bytes, Signature, TxKind, B256};

    use crate::policy::{
        backrun_id, encode_backrun_bid_priority, encode_opportunity_priority,
        gateway_call_calldata, gateway_call_calldata_with_target,
    };

    const GW: Address = address!("00000000000000000000000000000000000000bb");
    const OTHER: Address = address!("00000000000000000000000000000000000000cc");

    fn pool() -> PendingPool {
        PendingPool::new(
            Arc::new(PolicyConfig::for_test(GW, 0)),
            U256::from(0xffffu64),
            Duration::from_millis(2500),
            1024,
            1024,
        )
    }

    fn sig() -> Signature {
        Signature::new(U256::from(1u64), U256::from(1u64), false)
    }

    fn tx_hash(byte: u8) -> B256 {
        let mut b = [0u8; 32];
        b[0] = byte;
        B256::from(b)
    }

    /// A vanilla (non-gateway) tx that could serve as a backrun target.
    fn vanilla(to: Address) -> TxEnvelope {
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
                input: Bytes::new(),
            },
            sig(),
            b256!("00000000000000000000000000000000000000000000000000000000000000aa"),
        ))
    }

    fn gateway_tx(input: Bytes) -> TxEnvelope {
        gateway_tx_with_gas(input, 5, 21_000)
    }

    /// Gateway tx with a custom priority fee / gas limit, so tests can vary the gas
    /// half of the `fee + bid` scalar that backruns now rank by.
    fn gateway_tx_with_gas(
        input: Bytes,
        max_priority_fee_per_gas: u128,
        gas_limit: u64,
    ) -> TxEnvelope {
        TxEnvelope::Eip1559(Signed::new_unchecked(
            TxEip1559 {
                chain_id: 1,
                nonce: 0,
                gas_limit,
                // base fee floor is 0 in tests, so effective priority fee == this value
                // (kept >= the priority fee so the cap never bites).
                max_fee_per_gas: max_priority_fee_per_gas.max(100),
                max_priority_fee_per_gas,
                to: TxKind::Call(GW),
                value: U256::ZERO,
                access_list: Default::default(),
                input,
            },
            sig(),
            b256!("00000000000000000000000000000000000000000000000000000000000000aa"),
        ))
    }

    #[test]
    fn tob_bid_is_encoded_and_streamed_immediately() {
        let mut p = pool();
        let res = p.observe(
            tx_hash(1),
            gateway_tx(gateway_call_calldata(U256::from(7u64))),
        );
        assert_eq!(res.reinjections.len(), 1);
        // bit 255 set => sorted as a top-of-block bid.
        assert!(res.reinjections[0].priority.bit(255));
        assert_eq!(res.pairs_matched, 0);
        assert!(!res.skipped_non_gateway);
    }

    #[test]
    fn non_gateway_tx_is_cached_but_not_reinjected() {
        let mut p = pool();
        let res = p.observe(tx_hash(2), vanilla(OTHER));
        assert!(res.reinjections.is_empty());
        assert!(res.skipped_non_gateway);
        assert_eq!(p.cache_len(), 1);
    }

    #[test]
    fn target_already_pooled_pairs_immediately() {
        let mut p = pool();
        let target = tx_hash(3);
        // Target arrives first as vanilla traffic.
        p.observe(target, vanilla(OTHER));
        // Then the backrun bid referencing it.
        let res = p.observe(
            tx_hash(4),
            gateway_tx(gateway_call_calldata_with_target(U256::from(9u64), target)),
        );
        assert_eq!(res.pairs_matched, 1);
        // Opportunity (target) first, then the bid.
        assert_eq!(res.reinjections.len(), 2);
        assert_eq!(res.reinjections[0].hash, target);
        assert_eq!(
            res.reinjections[0].priority,
            encode_opportunity_priority(target)
        );
        // Backruns rank by the fee+bid scalar (total realized validator value), not the
        // bare bidAmount: 5 * 21_000 (priority_fee * gas_limit) + 9 (bidAmount) = 105_009.
        assert_eq!(
            res.reinjections[1].priority,
            encode_backrun_bid_priority(target, U256::from(105_009u64))
        );
        // They share the same backrun group.
        let group = |x: U256| x >> 129usize;
        assert_eq!(
            group(res.reinjections[0].priority),
            group(res.reinjections[1].priority)
        );
        assert_eq!(
            backrun_id(target) << 129usize,
            group(res.reinjections[1].priority) << 129usize
        );
    }

    #[test]
    fn bid_before_target_pairs_when_target_arrives() {
        let mut p = pool();
        let target = tx_hash(5);
        // Bid arrives before its target is pooled => parked.
        let res = p.observe(
            tx_hash(6),
            gateway_tx(gateway_call_calldata_with_target(U256::from(3u64), target)),
        );
        assert!(res.reinjections.is_empty());
        assert_eq!(res.bids_pended, 1);
        assert_eq!(p.pending_len(), 1);

        // Now the target shows up as vanilla traffic; the parked bid pairs.
        let res = p.observe(target, vanilla(OTHER));
        assert_eq!(res.pairs_matched, 1);
        assert_eq!(res.reinjections.len(), 2);
        assert_eq!(res.reinjections[0].hash, target);
        assert_eq!(p.pending_len(), 0);
    }

    #[test]
    fn multiple_competing_bids_all_stream_and_self_order() {
        let mut p = pool();
        let target = tx_hash(7);
        // Two bids park before the target shows up.
        p.observe(
            tx_hash(8),
            gateway_tx(gateway_call_calldata_with_target(U256::from(10u64), target)),
        );
        p.observe(
            tx_hash(9),
            gateway_tx(gateway_call_calldata_with_target(
                U256::from(100u64),
                target,
            )),
        );
        assert_eq!(p.pending_len(), 2);

        let res = p.observe(target, vanilla(OTHER));
        assert_eq!(res.pairs_matched, 2);
        // 1 opportunity + 2 bids.
        assert_eq!(res.reinjections.len(), 3);
        // The opportunity outranks both bids; the bigger bid outranks the smaller.
        let opp = res.reinjections[0].priority;
        let bids: Vec<U256> = res.reinjections[1..].iter().map(|r| r.priority).collect();
        for b in &bids {
            assert!(opp > *b);
        }
        let max = bids.iter().copied().max().unwrap();
        let min = bids.iter().copied().min().unwrap();
        assert!(max > min);
    }

    #[test]
    fn backrun_bids_rank_by_total_value_not_bare_bid() {
        // A backrun with a *lower* gateway bid but *higher* gas outranks a higher-bid,
        // lower-gas competitor: we rank by total realized validator value
        // (priority_fee * gas_limit + bidAmount), the same scalar TOB uses.
        let mut p = pool();
        let target = tx_hash(30);
        p.observe(target, vanilla(OTHER));

        // A: big bid (1000), cheap gas -> scalar = 1 * 21_000 + 1000 = 22_000.
        let res_a = p.observe(
            tx_hash(31),
            gateway_tx_with_gas(
                gateway_call_calldata_with_target(U256::from(1000u64), target),
                1,
                21_000,
            ),
        );
        // B: small bid (100), rich gas -> scalar = 1000 * 21_000 + 100 = 21_000_100.
        let res_b = p.observe(
            tx_hash(32),
            gateway_tx_with_gas(
                gateway_call_calldata_with_target(U256::from(100u64), target),
                1000,
                21_000,
            ),
        );

        // First bid streams opportunity + bid; the second (target already streamed) just the bid.
        let prio_a = res_a.reinjections.last().unwrap().priority;
        let prio_b = res_b.reinjections.last().unwrap().priority;
        assert!(
            prio_b > prio_a,
            "higher total-value backrun (more gas) must outrank the higher bare-bid one"
        );
        // Counterfactual: ranking by bare bidAmount would have put A (1000) above B (100).
        assert!(
            encode_backrun_bid_priority(target, U256::from(1000u64))
                > encode_backrun_bid_priority(target, U256::from(100u64))
        );
    }

    #[test]
    fn later_bid_after_target_streamed_does_not_restream_target() {
        let mut p = pool();
        let target = tx_hash(11);
        p.observe(target, vanilla(OTHER));
        // First bid: emits opportunity + bid.
        let first = p.observe(
            tx_hash(12),
            gateway_tx(gateway_call_calldata_with_target(U256::from(1u64), target)),
        );
        assert_eq!(first.reinjections.len(), 2);
        // Second bid: opportunity already streamed => only the bid.
        let second = p.observe(
            tx_hash(13),
            gateway_tx(gateway_call_calldata_with_target(U256::from(2u64), target)),
        );
        assert_eq!(second.reinjections.len(), 1);
        assert_eq!(second.pairs_matched, 1);
    }

    #[test]
    fn pending_bid_expires_after_ttl() {
        let mut p = pool();
        let start = Instant::now();
        let target = tx_hash(14);
        p.observe_at(
            tx_hash(15),
            gateway_tx(gateway_call_calldata_with_target(U256::from(5u64), target)),
            start,
        );
        assert_eq!(p.pending_len(), 1);

        // Before TTL: still pending.
        assert_eq!(p.prune(start + Duration::from_millis(100)), 0);
        assert_eq!(p.pending_len(), 1);

        // After TTL: expired and counted.
        let expired = p.prune(start + Duration::from_millis(3000));
        assert_eq!(expired, 1);
        assert_eq!(p.pending_len(), 0);
    }

    #[test]
    fn cache_entry_expires_after_ttl() {
        let mut p = pool();
        let start = Instant::now();
        p.observe_at(tx_hash(16), vanilla(OTHER), start);
        assert_eq!(p.cache_len(), 1);
        p.prune(start + Duration::from_millis(3000));
        assert_eq!(p.cache_len(), 0);
    }

    #[test]
    fn capacity_evicts_oldest_cache_entry() {
        let mut p = PendingPool::new(
            Arc::new(PolicyConfig::for_test(GW, 0)),
            U256::from(0xffffu64),
            Duration::from_millis(2500),
            2,
            2,
        );
        p.observe(tx_hash(20), vanilla(OTHER));
        p.observe(tx_hash(21), vanilla(OTHER));
        p.observe(tx_hash(22), vanilla(OTHER));
        assert_eq!(p.cache_len(), 2);
        // Oldest (20) evicted; a bid targeting it can no longer pair immediately.
        let res = p.observe(
            tx_hash(23),
            gateway_tx(gateway_call_calldata_with_target(
                U256::from(1u64),
                tx_hash(20),
            )),
        );
        assert_eq!(res.bids_pended, 1);
    }

    #[test]
    fn forget_removes_pending_and_cache_state() {
        let mut p = pool();
        let target = tx_hash(30);
        let bid_hash = tx_hash(31);
        p.observe(
            bid_hash,
            gateway_tx(gateway_call_calldata_with_target(U256::from(1u64), target)),
        );
        assert_eq!(p.pending_len(), 1);
        p.forget(bid_hash);
        assert_eq!(p.pending_len(), 0);
    }

    #[test]
    fn capacity_evicts_oldest_pending_bid() {
        let mut p = PendingPool::new(
            Arc::new(PolicyConfig::for_test(GW, 0)),
            U256::from(0xffffu64),
            Duration::from_millis(2500),
            8,
            2,
        );
        let first_target = tx_hash(40);
        let second_target = tx_hash(41);
        let third_target = tx_hash(42);

        p.observe(
            tx_hash(43),
            gateway_tx(gateway_call_calldata_with_target(
                U256::from(1u64),
                first_target,
            )),
        );
        p.observe(
            tx_hash(44),
            gateway_tx(gateway_call_calldata_with_target(
                U256::from(1u64),
                second_target,
            )),
        );
        let res = p.observe(
            tx_hash(45),
            gateway_tx(gateway_call_calldata_with_target(
                U256::from(1u64),
                third_target,
            )),
        );

        assert_eq!(res.bids_evicted, 1);
        assert_eq!(p.pending_len(), 2);
        let res = p.observe(first_target, vanilla(OTHER));
        assert!(res.reinjections.is_empty());
        assert!(res.skipped_non_gateway);
    }

    #[test]
    fn invalid_gateway_call_is_not_reinjected() {
        let mut p = pool();
        let res = p.observe(tx_hash(50), gateway_tx(Bytes::new()));
        assert!(res.reinjections.is_empty());
        assert!(res.skipped_invalid_gateway);
        assert!(!res.skipped_non_gateway);
    }
}
