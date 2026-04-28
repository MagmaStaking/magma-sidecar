# magma-sidecar

Rust **sidecar** for a Monad validator. It does two things, both described in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md):

1. **HTTP ingress** for searchers — `POST /rpc/monad` transparently forwards JSON-RPC (e.g. `eth_sendRawTransaction`) into the Monad EL.
2. **Txpool IPC reprioritization** — connects to the node's txpool Unix socket, observes `EthTxPoolEvent`s, and re-injects each transaction with a **tip-derived priority** so the node orders MEV-relevant traffic ahead of vanilla traffic.

This repo is intentionally **separate** from `monad-bft` (its own Cargo project, not a workspace member of the node repo). It pulls `monad-eth-txpool-ipc` and `monad-eth-txpool-types` straight from the upstream **`category-labs/monad-bft`** repo, pinned to a specific commit in `Cargo.toml`.

## Build

```bash
cd magma-sidecar
cargo build --release
```

The first build clones `monad-bft` into `~/.cargo/git/` (a few hundred MB, one-time cost). Subsequent builds reuse the cache. To upgrade the IPC protocol version, bump the `rev` on **both** `monad-eth-txpool-ipc` and `monad-eth-txpool-types` together — they must come from the same tree to keep the wire format in sync.

If you'd rather develop against a local checkout (e.g. while changing the IPC protocol in lockstep), point both crates at a sibling checkout instead:

```toml
monad-eth-txpool-ipc   = { path = "../monad-bft/monad-eth-txpool-ipc" }
monad-eth-txpool-types = { path = "../monad-bft/monad-eth-txpool-types" }
```

## Surfaces

| Surface | Purpose |
|---------|---------|
| `GET /health` | Structured liveness: IPC state, counters, last-event/last-send timestamps |
| `GET /metrics` | Prometheus exposition (counters, gauges; namespaced `magma_sidecar_*`) |
| `POST /rpc/monad` | Forward JSON-RPC body to the Monad EL (`eth_sendRawTransaction`, `eth_chainId`, …) |
| **Txpool IPC** (optional) | `--txpool-socket` connects to the node's txpool Unix socket, consumes `EthTxPoolEvent` batches, and re-injects each `Insert` as `EthTxPoolIpcTx` with a tip-based priority. |

## Run

```bash
cd magma-sidecar
cargo run --release -- \
  --bind 0.0.0.0:8089 \
  --monad-rpc-url http://127.0.0.1:8545
```

**With txpool IPC + tip policy** (same socket the node exposes for `EthTxPoolIpcClient`):

```bash
cargo run --release -- \
  --bind 0.0.0.0:8089 \
  --monad-rpc-url http://127.0.0.1:8545 \
  --txpool-socket /path/to/mempool.sock \
  --policy-config /path/to/policy.toml \
  --tx-priority 0xffff
```

Without `--policy-config`, the sidecar falls back to stamping every `Insert` with the constant `--tx-priority` (legacy mode).

### Tip policy file (TOML)

```toml
# Optional: floor the priority-fee component for legacy/EIP-2930 txs whose
# `gas_price` overstates the proposer-visible tip when base fee > 0.
base_fee_floor_wei = 0

# Allowlist of MagmaSearcherGateway contracts. `weight` (default 1) scales the
# value-routed-into-gateway component of the score; set to 0 to ignore an entry.
[[gateway]]
address = "0x00000000000000000000000000000000000000aa"
weight  = 1
label   = "MagmaSearcherGateway (mainnet)"

[[gateway]]
address = "0x00000000000000000000000000000000000000bb"
```

The score is `priority_fee × gas_limit + Σ value_into_allowlisted_gateway × weight`. See `docs/ARCHITECTURE.md` §"Priority policy" and `src/policy.rs` for the precise definition.

Environment (optional):

- `MAGMA_SIDECAR_BIND` — default `127.0.0.1:8089`
- `MAGMA_MONAD_RPC_URL` — Monad JSON-RPC base URL (target of `/rpc/monad`)
- `MAGMA_TXPOOL_SOCKET` — Unix socket path for txpool IPC
- `MAGMA_POLICY_CONFIG` — path to the TOML tip policy
- `MAGMA_TX_PRIORITY` — fallback hex priority for outbound `EthTxPoolIpcTx` (default `0xffff`)
- `RUST_LOG` — e.g. `info,magma_sidecar=debug`

### Example: forward a raw tx via the sidecar

Point your client at the sidecar instead of the node directly:

```bash
curl -s http://127.0.0.1:8089/rpc/monad \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_sendRawTransaction","params":["0x..."],"id":1}'
```

## Related repos

- `mev-entrypoint` — `MagmaSearcherGateway` contracts and test scripts
- `monad-bft` — node (txpool IPC protocol lives here; this repo links via `path` deps for types)

## Roadmap

The implementation now matches `docs/ARCHITECTURE.md`. Open follow-ups:

- **Tighten gateway-value detection:** today only direct `to == gateway` sends contribute; richer attribution (event-based, sub-call value flows) is the "future tightening" called out in the architecture doc §"Tip classification fidelity".
- **Backrun pairing & richer policies:** the `PriorityMode::Policy` decision is per-tx; pair-aware scoring would be a new mode behind the same surface.
- **Integration test against a fake IPC socket:** the IPC loop's I/O path is currently exercised end-to-end only in dev (`docs/LOCAL_DEVELOPMENT.md`).
