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
//!   against an optional `base_fee_floor` from the policy file. With the default
//!   `base_fee_floor = 0` this is `max_priority_fee_per_gas` for EIP-1559 and
//!   `gas_price` for legacy/EIP-2930 — the latter is a generous upper bound, but
//!   that matches the design's "naive, tip-based MEV" framing.
//! - `bid_routed_to_MagmaSearcherGateway` is the statically detectable bid into
//!   an allowlisted gateway in the same tx. We only count it when `to ==
//!   gateway` AND calldata is `magmaSearcherGatewayCall(address, uint256
//!   bidAmount, address, bytes)` — we then decode `bidAmount` from calldata.
//!   This is the on-chain enforced minimum net ETH gain on the gateway (see
//!   `mev-entrypoint/src/MagmaSearcherGateway.sol`) and is the right number to
//!   rank by, since `magmaSearcherGatewayCall` is `nonpayable` and so
//!   `tx.value` is always 0 on this path.
//!
//!   Anything else to a gateway address — empty calldata, a non-matching
//!   selector, a direct `receive()` top-up — gets a bid component of zero. We
//!   deliberately do *not* fall back to `tx.value`: a `receive()` top-up is an
//!   operational deposit, not a searcher bid declared as a minimum net gain,
//!   so ranking it as one would conflate two different intents and let anyone
//!   buy priority by sending native value to the gateway.
//!
//!   Per-gateway multipliers in the policy file let us weight trusted gateways
//!   without changing code; richer attribution (event-based, sub-call value
//!   flows that bypass the direct `to == gateway` invariant) remains the
//!   follow-up tightening called out in the doc.

use std::collections::HashMap;
use std::path::Path;

use alloy_consensus::{Transaction, TxEnvelope};
use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall};
use serde::Deserialize;

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

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("read policy file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse policy file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid policy: {0}")]
    Invalid(String),
}

/// Raw on-disk shape (TOML).
///
/// `base_fee_floor_wei` is a `u64` because TOML's integer type maxes at i64;
/// `u64::MAX` wei is ~1.8e19 (≈18.4 ETH per gas), well above any realistic floor.
#[derive(Debug, Deserialize)]
struct PolicyFile {
    #[serde(default)]
    base_fee_floor_wei: Option<u64>,
    #[serde(default)]
    gateway: Vec<GatewayEntry>,
}

#[derive(Debug, Deserialize)]
struct GatewayEntry {
    address: Address,
    /// Optional integer weight applied to value routed into this gateway.
    /// `None` means weight = 1. Use e.g. 2 to double-count routing into a
    /// preferred gateway, or 0 to ignore an allowlisted gateway entirely.
    #[serde(default)]
    weight: Option<u64>,
    /// Optional human label, ignored by the scorer (kept for ops).
    #[serde(default)]
    #[allow(dead_code)]
    label: Option<String>,
}

/// Parsed, validated tip policy.
#[derive(Debug, Clone, Default)]
pub struct PolicyConfig {
    base_fee_floor_wei: u128,
    gateway_weights: HashMap<Address, u64>,
}

impl PolicyConfig {
    /// Load a policy from a TOML file. Example:
    ///
    /// ```toml
    /// base_fee_floor_wei = 0
    ///
    /// [[gateway]]
    /// address = "0x0000000000000000000000000000000000000000"
    /// weight = 1
    /// label  = "MagmaSearcherGateway (mainnet)"
    /// ```
    pub fn load(path: impl AsRef<Path>) -> Result<Self, PolicyError> {
        let path_ref = path.as_ref();
        let path_display = path_ref.display().to_string();
        let raw = std::fs::read_to_string(path_ref).map_err(|source| PolicyError::Read {
            path: path_display.clone(),
            source,
        })?;
        let file: PolicyFile = toml::from_str(&raw).map_err(|source| PolicyError::Parse {
            path: path_display,
            source,
        })?;

        let mut gateway_weights = HashMap::with_capacity(file.gateway.len());
        for g in file.gateway {
            let weight = g.weight.unwrap_or(1);
            if gateway_weights.insert(g.address, weight).is_some() {
                return Err(PolicyError::Invalid(format!(
                    "duplicate gateway address {}",
                    g.address
                )));
            }
        }

        Ok(Self {
            base_fee_floor_wei: file.base_fee_floor_wei.unwrap_or(0) as u128,
            gateway_weights,
        })
    }

    /// Number of allowlisted gateways. Useful for log lines and `/health`.
    pub fn gateway_count(&self) -> usize {
        self.gateway_weights.len()
    }

    /// True when `addr` is an allowlisted `MagmaSearcherGateway`.
    ///
    /// Gateways with `weight = 0` are still considered allowlisted by this
    /// predicate (they were declared in the policy, just deliberately
    /// neutralized in the score). This keeps "is this a gateway tx?" decoupled
    /// from "what score does it get?".
    pub fn is_allowlisted_gateway(&self, addr: &Address) -> bool {
        self.gateway_weights.contains_key(addr)
    }

    /// Build a policy in-memory. Used by tests in other modules; production
    /// code paths go through [`PolicyConfig::load`].
    #[cfg(test)]
    pub fn from_parts(base_fee_floor_wei: u128, gateways: &[(Address, u64)]) -> Self {
        Self {
            base_fee_floor_wei,
            gateway_weights: gateways.iter().copied().collect(),
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
        Some(to) => match policy.gateway_weights.get(&to).copied() {
            Some(weight) => match gateway_bid_amount(tx) {
                Some(raw) => raw.saturating_mul(U256::from(weight)),
                // Only the explicit `magmaSearcherGatewayCall` path counts as a bid.
                // Plain `receive()` top-ups (or any other calldata to the gateway) are
                // operational deposits, not searcher bids — see module docs.
                None => U256::ZERO,
            },
            None => U256::ZERO,
        },
        None => U256::ZERO,
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
    use alloy_primitives::{address, Bytes};
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

    fn policy_with_gateway(addr: Address, weight: u64) -> PolicyConfig {
        let mut gateway_weights = HashMap::new();
        gateway_weights.insert(addr, weight);
        PolicyConfig {
            base_fee_floor_wei: 0,
            gateway_weights,
        }
    }

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
        let policy = PolicyConfig::default();
        // 5 * 21_000 = 105_000; no gateway match so value_component = 0.
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
        let policy = policy_with_gateway(gw, 1);
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
        let policy = policy_with_gateway(gw, 1);
        // 5 * 21_000 + 7_000_000 = 7_105_000
        assert_eq!(compute_priority(&tx, &policy), U256::from(7_105_000u64));
    }

    #[test]
    fn gateway_call_weight_scales_bid_amount() {
        let gw = address!("00000000000000000000000000000000000000bb");
        let tx = signed_eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 5,
            to: TxKind::Call(gw),
            value: U256::ZERO,
            access_list: Default::default(),
            input: gateway_call_calldata(U256::from(1_000u64)),
        });
        let policy = policy_with_gateway(gw, 3);
        // 5 * 21_000 + 3 * 1_000 = 108_000
        assert_eq!(compute_priority(&tx, &policy), U256::from(108_000u64));
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
        let policy = policy_with_gateway(gw, 1);
        // 5 * 21_000 + 0 = 105_000
        assert_eq!(compute_priority(&tx, &policy), U256::from(105_000u64));
    }

    #[test]
    fn gateway_weight_does_not_apply_to_receive_topup() {
        // Weight only multiplies a *decoded* `bidAmount`. A `receive()` top-up has no
        // bid to multiply, so the weight is irrelevant — score is just the priority fee.
        let gw = address!("00000000000000000000000000000000000000bb");
        let tx = signed_eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 5,
            to: TxKind::Call(gw),
            value: U256::from(1_000u64),
            access_list: Default::default(),
            input: Default::default(),
        });
        let policy = policy_with_gateway(gw, 3);
        // 5 * 21_000 + 0 = 105_000   (weight has no bid to multiply)
        assert_eq!(compute_priority(&tx, &policy), U256::from(105_000u64));
    }

    #[test]
    fn weight_zero_zeroes_decoded_bid() {
        // Decoded `bidAmount = 1_000`, weight = 0 → bid component is zeroed regardless.
        let gw = address!("00000000000000000000000000000000000000bb");
        let tx = signed_eip1559(TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 5,
            to: TxKind::Call(gw),
            value: U256::ZERO,
            access_list: Default::default(),
            input: gateway_call_calldata(U256::from(1_000u64)),
        });
        let policy = policy_with_gateway(gw, 0);
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
        let policy = policy_with_gateway(gw, 1);
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
        let policy = PolicyConfig {
            base_fee_floor_wei: 10,
            gateway_weights: HashMap::new(),
        };
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
        let policy = policy_with_gateway(gw, 1);
        assert_eq!(compute_priority(&tx, &policy), U256::from(7 * 21_000u64));
    }

    #[test]
    fn loads_toml_with_gateway_table() {
        let dir = tempdir();
        let path = dir.join("policy.toml");
        std::fs::write(
            &path,
            r#"
base_fee_floor_wei = 25

[[gateway]]
address = "0x00000000000000000000000000000000000000aa"
weight = 2
label  = "primary"

[[gateway]]
address = "0x00000000000000000000000000000000000000bb"
"#,
        )
        .unwrap();
        let p = PolicyConfig::load(&path).expect("policy loads");
        assert_eq!(p.base_fee_floor_wei, 25);
        assert_eq!(p.gateway_count(), 2);
        assert_eq!(
            p.gateway_weights[&address!("00000000000000000000000000000000000000aa")],
            2
        );
        assert_eq!(
            p.gateway_weights[&address!("00000000000000000000000000000000000000bb")],
            1
        );
    }

    #[test]
    fn rejects_duplicate_gateway() {
        let dir = tempdir();
        let path = dir.join("dup.toml");
        std::fs::write(
            &path,
            r#"
[[gateway]]
address = "0x00000000000000000000000000000000000000aa"
[[gateway]]
address = "0x00000000000000000000000000000000000000aa"
"#,
        )
        .unwrap();
        let err = PolicyConfig::load(&path).expect_err("should reject duplicate");
        assert!(matches!(err, PolicyError::Invalid(_)), "{err:?}");
    }

    /// Tiny tempdir helper so we don't pull in the `tempfile` crate just for two tests.
    fn tempdir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let name = format!(
            "magma-sidecar-policy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        p.push(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
