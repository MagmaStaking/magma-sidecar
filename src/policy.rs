//! Tip-based priority policy.
//!
//! Implements the scoring described in `docs/ARCHITECTURE.md` §"Priority policy":
//!
//! ```text
//! tip(tx) = effective_priority_fee(tx) * gas_used(tx)
//!        + bid_routed_to_MagmaSearcherGateway(tx)
//! ```
//!
//! At txpool time we don't have execution results or the next-block base fee, so:
//! - `gas_used` is approximated by `gas_limit` (a tight upper bound used by every
//!   pre-execution scorer).
//! - `effective_priority_fee` is approximated by `priority_fee_or_price()` clamped
//!   against the network's `base_fee_floor_wei` constant. With Monad's current
//!   0-wei base fee this clamp is a no-op; the constant exists so we can raise
//!   the floor per-network without code churn if base fee ever climbs above 0.
//! - `bid_routed_to_MagmaSearcherGateway` is the statically detectable bid into
//!   the network's single allowlisted gateway. We only count it when `to ==
//!   gateway` AND calldata is `magmaSearcherGatewayCall(address, uint256
//!   bidAmount, address, bytes)` — we then decode `bidAmount` from calldata.
//!   This is the on-chain enforced minimum net ETH gain on the gateway (see
//!   `mev-entrypoint/src/MagmaSearcherGateway.sol`) and is the right number to
//!   rank by, since `magmaSearcherGatewayCall` is `nonpayable` and so
//!   `tx.value` is always 0 on this path.
//!
//!   Anything else to the gateway address — empty calldata, a non-matching
//!   selector, a direct `receive()` top-up — gets a bid component of zero. We
//!   deliberately do *not* fall back to `tx.value`: a `receive()` top-up is an
//!   operational deposit, not a searcher bid declared as a minimum net gain,
//!   so ranking it as one would conflate two different intents and let anyone
//!   buy priority by sending native value to the gateway.
//!
//! ## Network selection
//!
//! There is exactly one `MagmaSearcherGateway` per network (mainnet, testnet,
//! localnet). The address is baked into this file rather than loaded from a
//! config file at runtime, so a gateway redeploy ships as a versioned binary
//! (and a new `.deb`) rather than an out-of-band ops change. Pick the network
//! at startup with `--network` (or `MAGMA_NETWORK`); if you don't, the sidecar
//! falls back to stamping every Insert with the constant `--tx-priority-hex`
//! and ignores gateway scoring entirely.

use alloy_consensus::{Transaction, TxEnvelope};
use alloy_primitives::{address, Address, U256};
use alloy_sol_types::{sol, SolCall};
use clap::ValueEnum;

sol! {
    /// ABI for `MagmaSearcherGateway.magmaSearcherGatewayCall`. We only use the
    /// generated `SELECTOR` and `abi_decode` — no contract binding needed.
    #[allow(missing_docs)]
    interface IMagmaSearcherGateway {
        function magmaSearcherGatewayCall(
            address sender,
            uint256 bidAmount,
            address searcherContract,
            bytes searcherCallData
        ) external;
    }
}

/// Networks the sidecar knows how to score for. Selected via `--network` /
/// `MAGMA_NETWORK`. Adding a network is a 3-line change: variant + gateway()
/// + base_fee_floor_wei() arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Network {
    /// Magma mainnet.
    Mainnet,
    /// Magma testnet.
    Testnet,
    /// Local Monad devnet — gateway address comes from
    /// `mev-entrypoint/test-scripts/script/DeployCounterSearchers.s.sol`,
    /// deterministic for anvil account #0 at nonce 0.
    Localnet,
}

impl Network {
    /// The single allowlisted `MagmaSearcherGateway` for this network.
    pub const fn gateway(self) -> Address {
        match self {
            // TODO(magma): replace with the real mainnet gateway address once deployed.
            Self::Mainnet => address!("0000000000000000000000000000000000000000"),
            // TODO(magma): replace with the real testnet gateway address once deployed.
            Self::Testnet => address!("0000000000000000000000000000000000000000"),
            // Deterministic deployment from `make deploy` in mev-entrypoint/test-scripts/.
            Self::Localnet => address!("8f86403a4de0bb5791fa46b8e795c547942fe4cf"),
        }
    }

    /// Per-network floor for the priority-fee component of legacy / EIP-2930 txs,
    /// in wei. `priority_fee_or_price()` returns `gas_price` for those tx types,
    /// which overstates the proposer-visible tip once base fee > 0; we clamp it
    /// to `max_fee - base_fee_floor_wei` so the scorer doesn't over-credit them.
    ///
    /// Monad currently runs with base fee == 0 on every network, so all three
    /// constants are 0 today. The per-network split is here so we can raise the
    /// floor in lockstep with a network parameter change without a code
    /// restructure.
    pub const fn base_fee_floor_wei(self) -> u128 {
        match self {
            Self::Mainnet | Self::Testnet | Self::Localnet => 0,
        }
    }

    /// Short label used in log lines and structured health output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
            Self::Localnet => "localnet",
        }
    }
}

/// Compiled policy: the (gateway, base_fee_floor) pair the scorer closes over.
/// Construct via `PolicyConfig::for_network` in production; the `for_test`
/// helper exists so unit tests can pin an arbitrary gateway address without
/// depending on which `Network` it's keyed under.
#[derive(Debug, Clone, Copy)]
pub struct PolicyConfig {
    gateway: Address,
    base_fee_floor_wei: u128,
}

impl PolicyConfig {
    pub fn for_network(network: Network) -> Self {
        Self {
            gateway: network.gateway(),
            base_fee_floor_wei: network.base_fee_floor_wei(),
        }
    }

    /// The single allowlisted gateway address for this policy.
    pub fn gateway(&self) -> Address {
        self.gateway
    }

    /// True when `addr` is the network's allowlisted `MagmaSearcherGateway`.
    pub fn is_allowlisted_gateway(&self, addr: &Address) -> bool {
        *addr == self.gateway
    }

    /// Per-network base-fee floor applied to `priority_fee_or_price()` clamps.
    pub fn base_fee_floor_wei(&self) -> u128 {
        self.base_fee_floor_wei
    }

    /// Test-only constructor: pin an arbitrary gateway + floor without going
    /// through the `Network` enum. Lets tests reuse `compute_priority` against
    /// synthetic addresses that won't collide with real deployments.
    #[cfg(test)]
    pub(crate) fn for_test(gateway: Address, base_fee_floor_wei: u128) -> Self {
        Self {
            gateway,
            base_fee_floor_wei,
        }
    }
}

/// Compute the tip-derived priority for a transaction.
///
/// Returns `U256` to match the wire field on `EthTxPoolIpcTx::priority`.
pub fn compute_priority(tx: &TxEnvelope, policy: &PolicyConfig) -> U256 {
    let priority_fee = effective_priority_fee_per_gas(tx, policy.base_fee_floor_wei);
    let gas_limit = tx.gas_limit() as u128;
    let fee_component = U256::from(priority_fee).saturating_mul(U256::from(gas_limit));

    let bid_component = match tx.to() {
        Some(to) if to == policy.gateway => gateway_bid_amount(tx).unwrap_or(U256::ZERO),
        // Only the explicit `magmaSearcherGatewayCall` path counts as a bid.
        // Plain `receive()` top-ups (or any other calldata to the gateway) are
        // operational deposits, not searcher bids — see module docs.
        _ => U256::ZERO,
    };

    fee_component.saturating_add(bid_component)
}

/// Decode the `bidAmount` argument of `MagmaSearcherGateway.magmaSearcherGatewayCall`
/// from the tx's calldata. Returns `None` when the selector doesn't match; the caller
/// treats that as a zero bid (we do not fall back to `tx.value` — see module docs).
fn gateway_bid_amount(tx: &TxEnvelope) -> Option<U256> {
    let input = tx.input();
    if input.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = input[..4].try_into().ok()?;
    if selector != IMagmaSearcherGateway::magmaSearcherGatewayCallCall::SELECTOR {
        return None;
    }
    // `abi_decode_raw` consumes the args portion (without the 4-byte selector).
    let call =
        IMagmaSearcherGateway::magmaSearcherGatewayCallCall::abi_decode_raw(&input[4..]).ok()?;
    Some(call.bidAmount)
}

/// Build calldata for `magmaSearcherGatewayCall(sender, bidAmount, searcher, data)`.
///
/// Test-only helper, shared across modules so tests for the txpool IPC layer can
/// exercise the same bid-decoding path as the policy tests without duplicating
/// the ABI encoder.
#[cfg(test)]
pub(crate) fn gateway_call_calldata(bid_amount: U256) -> alloy_primitives::Bytes {
    use alloy_primitives::Bytes;
    IMagmaSearcherGateway::magmaSearcherGatewayCallCall {
        sender: address!("00000000000000000000000000000000000000ee"),
        bidAmount: bid_amount,
        searcherContract: address!("00000000000000000000000000000000000000ff"),
        searcherCallData: Bytes::from_static(b"opaque"),
    }
    .abi_encode()
    .into()
}

fn effective_priority_fee_per_gas(tx: &TxEnvelope, base_fee_floor: u128) -> u128 {
    // `priority_fee_or_price` returns `max_priority_fee_per_gas` for EIP-1559/4844/7702
    // and `gas_price` for legacy/EIP-2930. For legacy txs the latter overstates the
    // proposer-visible tip when base fee > 0, which is why `base_fee_floor` exists.
    let raw = tx.priority_fee_or_price();
    let max_fee = tx.max_fee_per_gas();
    // The validator can't realize more than (max_fee - base_fee_floor) per gas.
    let cap = max_fee.saturating_sub(base_fee_floor);
    raw.min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_consensus::{Signed, TxEip1559, TxEnvelope, TxLegacy};
    use alloy_primitives::{address, b256, Bytes, Signature, TxKind};

    fn dummy_sig() -> Signature {
        // arbitrary values; we never verify recovery in these tests.
        Signature::new(U256::from(1u64), U256::from(1u64), false)
    }

    fn signed_eip1559(tx: TxEip1559) -> TxEnvelope {
        TxEnvelope::Eip1559(Signed::new_unchecked(
            tx,
            dummy_sig(),
            b256!("0000000000000000000000000000000000000000000000000000000000000001"),
        ))
    }

    fn signed_legacy(tx: TxLegacy) -> TxEnvelope {
        TxEnvelope::Legacy(Signed::new_unchecked(
            tx,
            dummy_sig(),
            b256!("0000000000000000000000000000000000000000000000000000000000000002"),
        ))
    }

    /// Sentinel gateway address used by tests that want a policy whose gateway
    /// definitely doesn't match the tx's `to` (so the bid component is zero).
    const UNRELATED_GATEWAY: Address = address!("00000000000000000000000000000000000000ff");

    #[test]
    fn eip1559_priority_fee_only() {
        let tx = signed_eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 5,
            to: TxKind::Call(address!("00000000000000000000000000000000000000aa")),
            value: U256::from(1_000u64),
            access_list: Default::default(),
            input: Default::default(),
        });
        // Policy keyed to an unrelated gateway: no gateway match → bid component zero.
        let policy = PolicyConfig::for_test(UNRELATED_GATEWAY, 0);
        // 5 * 21_000 = 105_000
        assert_eq!(compute_priority(&tx, &policy), U256::from(105_000u64));
    }

    #[test]
    fn eip1559_gateway_receive_topup_does_not_count_value_as_bid() {
        // Empty calldata to a gateway is the `receive()` payable path: an operational
        // top-up, not a `magmaSearcherGatewayCall` bid. We deliberately do NOT credit
        // `tx.value` as a bid — score collapses to the priority-fee component only.
        let gw = address!("00000000000000000000000000000000000000bb");
        let tx = signed_eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 5,
            to: TxKind::Call(gw),
            value: U256::from(1_000_000u64),
            access_list: Default::default(),
            input: Default::default(),
        });
        let policy = PolicyConfig::for_test(gw, 0);
        // 5 * 21_000 + 0 = 105_000
        assert_eq!(compute_priority(&tx, &policy), U256::from(105_000u64));
    }

    #[test]
    fn gateway_call_uses_decoded_bid_amount() {
        let gw = address!("00000000000000000000000000000000000000bb");
        let tx = signed_eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 5,
            to: TxKind::Call(gw),
            // Non-payable on-chain, so always 0 in practice.
            value: U256::ZERO,
            access_list: Default::default(),
            input: gateway_call_calldata(U256::from(7_000_000u64)),
        });
        let policy = PolicyConfig::for_test(gw, 0);
        // 5 * 21_000 + 7_000_000 = 7_105_000
        assert_eq!(compute_priority(&tx, &policy), U256::from(7_105_000u64));
    }

    #[test]
    fn gateway_with_unknown_selector_gets_zero_bid() {
        // Calldata with a non-matching 4-byte prefix: we can't decode a bid, so the bid
        // component is zero (no `tx.value` fallback). Otherwise anyone could buy priority
        // by sending native value to the gateway with a junk selector.
        let gw = address!("00000000000000000000000000000000000000bb");
        let mut bogus = vec![0xde, 0xad, 0xbe, 0xef];
        bogus.extend_from_slice(&[0u8; 32]);
        let tx = signed_eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 5,
            to: TxKind::Call(gw),
            value: U256::from(2_000u64),
            access_list: Default::default(),
            input: Bytes::from(bogus),
        });
        let policy = PolicyConfig::for_test(gw, 0);
        // 5 * 21_000 + 0 = 105_000
        assert_eq!(compute_priority(&tx, &policy), U256::from(105_000u64));
    }

    #[test]
    fn non_gateway_recipient_excluded() {
        let gw = address!("00000000000000000000000000000000000000bb");
        let other = address!("00000000000000000000000000000000000000cc");
        let tx = signed_eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 5,
            to: TxKind::Call(other),
            value: U256::from(999_999u64),
            access_list: Default::default(),
            input: Default::default(),
        });
        let policy = PolicyConfig::for_test(gw, 0);
        assert_eq!(compute_priority(&tx, &policy), U256::from(105_000u64));
    }

    #[test]
    fn legacy_tx_uses_gas_price_minus_floor() {
        let tx = signed_legacy(TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 50,
            gas_limit: 21_000,
            to: TxKind::Call(address!("00000000000000000000000000000000000000aa")),
            value: U256::ZERO,
            input: Default::default(),
        });
        let policy = PolicyConfig::for_test(UNRELATED_GATEWAY, 10);
        // (50 - 10) * 21_000 = 840_000
        assert_eq!(compute_priority(&tx, &policy), U256::from(840_000u64));
    }

    #[test]
    fn create_tx_has_no_value_component() {
        let tx = signed_eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 7,
            to: TxKind::Create,
            value: U256::from(123u64),
            access_list: Default::default(),
            input: Default::default(),
        });
        let gw = address!("00000000000000000000000000000000000000bb");
        let policy = PolicyConfig::for_test(gw, 0);
        assert_eq!(compute_priority(&tx, &policy), U256::from(7 * 21_000u64));
    }

    #[test]
    fn network_constants_are_well_formed() {
        // Sanity: localnet is the only network with a real address today.
        assert_ne!(Network::Localnet.gateway(), Address::ZERO);
        assert_eq!(Network::Mainnet.base_fee_floor_wei(), 0);
        assert_eq!(Network::Testnet.base_fee_floor_wei(), 0);
        assert_eq!(Network::Localnet.base_fee_floor_wei(), 0);

        // for_network round-trips.
        let p = PolicyConfig::for_network(Network::Localnet);
        assert_eq!(p.gateway(), Network::Localnet.gateway());
        assert!(p.is_allowlisted_gateway(&Network::Localnet.gateway()));
        assert!(!p.is_allowlisted_gateway(&Address::ZERO));
    }
}
