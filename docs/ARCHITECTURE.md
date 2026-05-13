# Magma MEV architecture

This document describes how **searchers**, the **MEV gateway** (on-chain), and the **magma-sidecar** relate to **Monad** execution and transaction ordering. It is a technical overview—not a delivery timeline.

The model is **naive, tip-based MEV**: searchers compete for inclusion order through tips, and the sidecar reprioritizes the txpool accordingly.

## Goals

- Collect MEV bids in a **single contract surface** (`MagmaSearcherGateway`) so fee rules are enforceable on-chain.
- Have the **magma-sidecar** observe the node's txpool and **assign per-tx priority based on tips** (priority fee + bid paid to the gateway), so the node orders MEV-relevant txs ahead of vanilla traffic.
- Drive ordering with **tips**, enforced through the node's existing priority surface.

## End-to-end data flow

**Ingress:** Searchers submit transactions either **directly to the Monad node's JSON-RPC** or **to magma-sidecar's HTTP ingress** (which forwards into the node's RPC). Either way, they land in the node's txpool.

**Reprioritization:** magma-sidecar is connected to the node's **txpool IPC** and observes `EthTxPoolEvent`s. For each inserted transaction it computes a **priority** from the tx's tip (priority fee + bid declared to `MagmaSearcherGateway`, decoded from `magmaSearcherGatewayCall` calldata) and re-injects the tx with that priority over IPC. The node uses the supplied priority when constructing the next block.

```mermaid
flowchart LR
  subgraph onchain [On-chain]
    S[Searcher contracts]
    G[MagmaSearcherGateway]
    S -->|"magmaSearcherGatewayCall"| G
  end

  subgraph offchain [Off-chain]
    SB[Searcher bots]
    SC[magma-sidecar]
    M[Monad node]
  end

  SB -->|"raw tx via HTTP"| SC
  SB -->|"raw tx via JSON-RPC"| M
  SC -->|"forward eth_sendRawTransaction"| M
  M -->|"EthTxPoolEvent stream (IPC)"| SC
  SC -->|"EthTxPoolIpcTx (RLP + priority)"| M
```

## Components

### Searcher + MEV gateway (`mev-entrypoint`)

- **`MagmaSearcherGateway`**: entrypoint that forwards to a searcher implementation and enforces a minimum **net native-token gain** on the gateway contract balance.
- **`MagmaSearcher`** (base): authorization, gateway-only entry, and repayment of the bid to the gateway in ETH.
- Searchers implement MEV logic; **fees accumulate on the gateway** for later settlement (or future withdrawal paths).
- The gateway is the on-chain anchor for the sidecar's tip computation: the `bidAmount` declared on a `magmaSearcherGatewayCall` (and, for plain top-ups via the gateway's `receive()`, `tx.value`) is treated as part of that tx's effective tip when ranking it.

### Magma sidecar (this repository)

- **Role**: sit beside the Monad node, observe txpool events, and feed back tx priorities so MEV-relevant traffic is ordered ahead of vanilla traffic.
- **Ingress (HTTP)**: accept JSON-RPC from searchers (e.g. `eth_sendRawTransaction`) and forward to the Monad EL JSON-RPC, giving searchers a routing alternative to hitting the node directly.
- **Reprioritization (IPC)**: subscribe to the node's txpool over the Unix socket, classify each `Insert` event, compute a tip-based priority, and stream the tx back over IPC with that priority.

**Repository boundary:** `magma-sidecar` is a standalone Cargo project. For txpool IPC it depends on `monad-eth-txpool-ipc` / `monad-eth-txpool-types` via **path dependencies** to a sibling checkout of `monad-bft` (see [`README.md`](../README.md)). Wire formats and socket paths are defined in `monad-bft` and consumed here.

### Sidecar implementation (this repo)

The Rust binary **`magma-sidecar`** exposes HTTP endpoints documented in [`README.md`](../README.md):

- **Ingress:** `POST /rpc/monad` forwards arbitrary JSON-RPC (including `eth_sendRawTransaction`) to the Monad EL.
- **Health:** `GET /health` for liveness.

**Txpool IPC:** with `--txpool-socket` / `MAGMA_TXPOOL_SOCKET`, the sidecar connects to the node's txpool Unix socket (length-delimited frames, bincode event batches in, RLP `EthTxPoolIpcTx` out, as implemented in `monad-eth-txpool-ipc`). It subscribes to `EthTxPoolEvent` streams and re-injects **Insert** transactions with a computed **priority**, deduplicating echoes of its own reinjections.

### Priority policy

The sidecar's priority is a **tip-based score**:

```
tip(tx) = effective_priority_fee(tx) * gas_used(tx)
       + bid_routed_to_MagmaSearcherGateway(tx) * gateway_weight
```

- `effective_priority_fee` is the EIP-1559 priority fee component the validator would receive.
- `bid_routed_to_MagmaSearcherGateway` is read statically from the signed tx when `to` is an allowlisted gateway:
  - if calldata is a `magmaSearcherGatewayCall(address sender, uint256 bidAmount, address searcherContract, bytes searcherCallData)`, the sidecar decodes `bidAmount` from calldata. This is the on-chain enforced minimum net ETH gain on the gateway contract, so it is the right number to rank by — and `magmaSearcherGatewayCall` is `nonpayable`, so `tx.value` is always 0 on this path.
  - otherwise the sidecar falls back to `tx.value` (the gateway's `receive()` payable path: a direct top-up to the gateway).
- `gateway_weight` is a per-gateway multiplier (default 1) declared in the policy file; setting it to 0 ignores an allowlisted gateway entirely.

The score is mapped into the IPC priority field (a `U256`-shaped slot in `EthTxPoolIpcTx`); ordering is per-tx by tip. The plumbing is policy-agnostic, so richer policies (e.g. backrun pairing, sub-call attribution) can replace the scoring function in place.

The `--tx-priority` constant serves as a fallback for txs the sidecar elects not to recompute (e.g. echoes, malformed input).

### Monad node (`monad-bft`)

- Execution, consensus, **txpool**, and RPC layers.
- Accepts raw transactions on JSON-RPC from any source (including the sidecar's `/rpc/monad`) and emits txpool events over IPC.
- Honors the priority supplied by the sidecar's `EthTxPoolIpcTx` reinjection when constructing the next block, so the **tip-derived ordering** becomes the effective inclusion order.

## Platform topics (not specific to this repo)

These items affect **classification quality**, **ingress**, and **reward routing**; they are tracked in Monad / platform workstreams.

### Tip classification fidelity

- The naive policy reads what is statically derivable from the signed tx: priority fee, `to`, `value`, and the `bidAmount` argument of `magmaSearcherGatewayCall`. This requires the gateway to be the direct `to` of the tx; wrapper / proxy calls that reach the gateway via a sub-call are not currently attributed.
- A future tightening: read gateway-emitted events / use `monad-bft` speculative state to attribute bid amounts post-hoc (including sub-call paths) and feed them into priority for the next block.

### Transaction ingress

- The **dedicated searcher endpoints** (sidecar HTTP and node RPC) ensure MEV-relevant traffic lands on the local txpool the sidecar is wired to, where reprioritization applies.

### Rewards: block proposer vs MEV sink

- The gateway acts as a dedicated **MEV sink**, separate from the validator's block-reward recipient; combined with indexing and rebates it lets **protocol MEV** be accounted for independently of **validator block rewards**.

## Related repos

| Repo | Role |
|------|------|
| `mev-entrypoint` | Gateway + searcher interfaces (Solidity) |
| `monad-bft` | Monad node; txpool, RPC, consensus, IPC protocol |
| `magma-sidecar` (this repo) | Sidecar service: HTTP ingress + tip-based txpool reprioritization |

## Glossary

| Term | Meaning |
|------|--------|
| **Gateway** | `MagmaSearcherGateway`—on-chain sink for enforced bids |
| **Tip** | Priority fee component + bid declared to the gateway (via `magmaSearcherGatewayCall`'s `bidAmount`, or `tx.value` for plain top-ups) in the same tx |
| **Reprioritizing** | Sidecar re-injects a tx over IPC with a tip-derived priority so the node orders it ahead of vanilla traffic |

---

*If you want this doc to name concrete IPC methods, RPC method names, or deployment topology (one process vs split), add those as subsections once the interfaces are frozen.*
