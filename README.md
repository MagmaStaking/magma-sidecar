# magma-sidecar

Rust **sidecar** between **searchers / rbuilder** and the **Monad** node, aligned with [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

This repo is intentionally **separate** from **`monad-bft`** (its own Cargo project, not a workspace member of the node repo). It provides **HTTP bridging** (ingress to rbuilder, egress of signed builder bundles to Monad) plus optional **txpool IPC** priority streaming (same framing/RLP as `monad-eth-txpool-ipc` in `monad-bft`).

## Build

`Cargo.toml` uses **path dependencies** into a sibling checkout of `monad-bft` for the IPC crates:

- `../monad-bft/monad-eth-txpool-ipc`
- `../monad-bft/monad-eth-txpool-types`

Place `magma-sidecar` and `monad-bft` next to each other (or edit those paths). Then:

```bash
cd magma-sidecar
cargo build --release
```

## What it does

| Surface | Purpose |
|--------|---------|
| `GET /health` | Liveness JSON |
| `POST /rpc/rbuilder` | Forward JSON-RPC body to rbuilder (`eth_sendBundle`, `eth_sendRawTransaction`, …) |
| `POST /rpc/monad` | Forward JSON-RPC body to Monad EL (escape hatch) |
| `POST /v1/submit-builder-bundle` | Body = signed bundle params; sidecar wraps `monad_submitBuilderBundle` and POSTs to Monad |
| **Txpool IPC** (optional) | `--txpool-socket` connects to the node’s txpool Unix socket, consumes `EthTxPoolEvent` batches, and sends RLP `EthTxPoolIpcTx` with configurable priority before the node applies ordering. |

Signing for `monad_submitBuilderBundle` stays in **rbuilder** (or your tool); the sidecar only forwards the JSON-RPC to `MAGMA_MONAD_RPC_URL`.

## Run

```bash
cd magma-sidecar
cargo run --release -- \
  --bind 0.0.0.0:8089 \
  --monad-rpc-url http://127.0.0.1:8545 \
  --rbuilder-rpc-url http://127.0.0.1:8645
```

**Optional txpool IPC** (same socket the node exposes for `EthTxPoolIpcClient`):

```bash
cargo run --release -- \
  --bind 0.0.0.0:8089 \
  --monad-rpc-url http://127.0.0.1:8545 \
  --rbuilder-rpc-url http://127.0.0.1:8645 \
  --txpool-socket /path/to/mempool.sock \
  --tx-priority 0xffff
```

Environment (optional):

- `MAGMA_SIDECAR_BIND` — default `127.0.0.1:8089`
- `MAGMA_MONAD_RPC_URL` — Monad JSON-RPC base URL
- `MAGMA_RBUILDER_RPC_URL` — rbuilder incoming JSON-RPC base URL
- `MAGMA_TXPOOL_SOCKET` — Unix socket path for txpool IPC (see above)
- `MAGMA_TX_PRIORITY` — hex priority for outbound `EthTxPoolIpcTx` (default `0xffff`)
- `RUST_LOG` — e.g. `info,magma_sidecar=debug`

### Example: forward a bundle to rbuilder via sidecar

Point your client at the sidecar instead of rbuilder directly:

```bash
curl -s http://127.0.0.1:8089/rpc/rbuilder \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_sendBundle","params":[...],"id":1}'
```

### Example: submit pre-signed builder bundle to Monad

```bash
curl -s http://127.0.0.1:8089/v1/submit-builder-bundle \
  -H 'Content-Type: application/json' \
  -d '{
    "transactions": ["0f8b..."],
    "signature": "...",
    "signer": "...",
    "timestamp": 1234567890
  }'
```

## Related repos

- `mev-entrypoint` — `MagmaSearcherGateway` contracts  
- `rbuilder-private` — block builder, `mev_profit_addresses`, Monad bundle signing  
- `monad-bft` — node (txpool IPC protocol lives here; this repo links to it only via `path` deps for types)  

## Roadmap

- Prometheus metrics (`/metrics`) if needed for ops.
