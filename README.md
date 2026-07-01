# magma-sidecar

Rust **sidecar** for a Monad validator. It does two things, both described in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md):

1. **HTTP ingress** for searchers — `POST /rpc/monad` transparently forwards JSON-RPC (e.g. `eth_sendRawTransaction`) into the Monad EL.
2. **Txpool IPC reprioritization** — connects to the node's txpool Unix socket, observes `EthTxPoolEvent`s, and re-injects each transaction with a **tip-derived priority** so the node orders MEV-relevant traffic ahead of vanilla traffic.

This repo is intentionally **separate** from `monad-bft` (its own Cargo project, not a workspace member of the node repo). It pulls `monad-eth-txpool-ipc` and `monad-eth-txpool-types` straight from the upstream **`category-labs/monad-bft`** repo, pinned to a specific commit in `Cargo.toml`.

## Installation

Three supported install paths, in roughly recommended order for production use. For local development against a Monad devnet, jump to [Run from source](#run-from-source) and [`docs/LOCAL_DEVELOPMENT.md`](docs/LOCAL_DEVELOPMENT.md).

### Option 1: Debian package via APT (recommended for validator hosts)

Hosts a versioned `.deb` with a systemd unit, dropped under `monad:monad` so it has the right uid to read the node's txpool IPC socket.

```bash
# Add the Magma APT repo and signing key (one-time).
sudo mkdir -p /etc/apt/keyrings
sudo wget -qO /etc/apt/keyrings/magma.gpg https://magmastaking.github.io/magma-sidecar-apt-repo/magma-apt-key.gpg.bin
echo "deb [signed-by=/etc/apt/keyrings/magma.gpg] https://magmastaking.github.io/magma-sidecar-apt-repo stable main" \
  | sudo tee /etc/apt/sources.list.d/magma.list
sudo apt update

# Install.
sudo apt install magma-sidecar
# Or a specific version:  sudo apt install magma-sidecar=1.0.0

# Configure: at minimum set MAGMA_MONAD_RPC_URL and MAGMA_NETWORK
# (mainnet | testnet | localnet). MAGMA_TXPOOL_SOCKET defaults to the standard
# monad-bft path (/home/monad/monad-bft/mempool.sock); override only if your node
# writes it elsewhere. The gateway address for each network is baked into the
# binary; no extra config file to drop in.
sudo $EDITOR /etc/magma-sidecar/sidecar.env

# Start.
sudo systemctl enable --now magma-sidecar
sudo systemctl status magma-sidecar
sudo journalctl -u magma-sidecar -f
```

The Debian package ships:

- Binary: `/usr/bin/magma-sidecar`
- Systemd unit: `/lib/systemd/system/magma-sidecar.service` (runs as `User=monad`, hardened, `Restart=always`)
- Config template: `/etc/magma-sidecar/sidecar.env.example`

The `postinst` script seeds `/etc/magma-sidecar/sidecar.env` from the example **only on first install** — upgrades never clobber operator-edited config.

You can also grab a release `.deb` directly from [GitHub Releases](https://github.com/MagmaStaking/magma-sidecar/releases) (`amd64` + `arm64` are both published) and `sudo dpkg -i magma-sidecar_<version>_<arch>.deb` if you don't want the APT repo.

### Option 2: Docker (recommended for non-validator use cases — gateways, dev, k8s)

Multi-arch images at `ghcr.io/magmastaking/magma-sidecar` (`linux/amd64` + `linux/arm64`).

```bash
docker pull ghcr.io/magmastaking/magma-sidecar:latest
# Or pin: ghcr.io/magmastaking/magma-sidecar:1.0.0

# Ingress-only (no txpool IPC reprioritization).
docker run --rm -p 8089:8089 \
  -e MAGMA_MONAD_RPC_URL=http://host.docker.internal:8545 \
  ghcr.io/magmastaking/magma-sidecar:latest
```

For txpool IPC mode, bind-mount the node's socket and a policy file — see the comment block at the top of [`Dockerfile`](Dockerfile) for the full incantation (the AF_UNIX 107-byte path limit comes up here; [`docs/LOCAL_DEVELOPMENT.md`](docs/LOCAL_DEVELOPMENT.md) §1a has the workaround).

### Option 3: Build from source

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

To produce a `.deb` from a local checkout (the same flow CI uses), prefer the [Makefile](Makefile):

```bash
make help                                 # list every target
make build-deb                            # native-arch .deb in build/
make build-deb-arm64                      # cross via `cross`
make install                              # sudo dpkg -i the latest build/*.deb
make service-status                       # systemctl status magma-sidecar
make purge                                # sudo dpkg --purge magma-sidecar
```

Or invoke the script directly (e.g. for non-Make environments):

```bash
./debian/sidecar/build-deb.sh 0.1.0-local            # native arch
./debian/sidecar/build-deb.sh 0.1.0-local arm64      # cross via `cross`
sudo dpkg -i build/magma-sidecar_0.1.0-local_*.deb
```

## Service management

For the Debian-installed service:

```bash
sudo systemctl start magma-sidecar      # start
sudo systemctl stop magma-sidecar       # stop
sudo systemctl restart magma-sidecar    # restart (after editing sidecar.env)
sudo systemctl status magma-sidecar     # current state + last few log lines
sudo systemctl enable magma-sidecar     # start on boot
sudo systemctl disable magma-sidecar    # don't start on boot

sudo journalctl -u magma-sidecar -f     # follow logs
sudo journalctl -u magma-sidecar -n 200 # last 200 lines
```

To override systemd-managed bits without editing the shipped unit:

```bash
sudo systemctl edit magma-sidecar
# In the drop-in editor:
# [Service]
# Environment="RUST_LOG=info,magma_sidecar=debug"
```

## Surfaces

| Surface | Purpose |
|---------|---------|
| `GET /health` | Structured liveness: IPC state, counters, last-event/last-send timestamps |
| `GET /metrics` | Prometheus exposition (counters, gauges; namespaced `magma_sidecar_*`) |
| `POST /rpc/monad` | Forward JSON-RPC body to the Monad EL (`eth_sendRawTransaction`, `eth_chainId`, …) |
| **Txpool IPC** (optional) | `--txpool-socket` connects to the node's txpool Unix socket, consumes `EthTxPoolEvent` batches, and (in policy mode) re-injects only `Insert`s targeting an allowlisted `MagmaSearcherGateway` as `EthTxPoolIpcTx` with a tip-based priority. |

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
  --network localnet \
  --tx-priority-hex 0xffff
```

In policy mode the sidecar **only reinjects** transactions whose `to` is the network's allowlisted `MagmaSearcherGateway` — vanilla traffic is observed (so `tx_inserts_observed` still climbs) but left alone, so the node's default ordering applies and the sidecar doesn't fight other reprioritizers (e.g. `fastlane-sidecar`) for unrelated txs. Skipped txs are counted in `txpool_skipped_non_gateway_total`.

Bids that carry a non-zero `targetTxHash` are treated as **backruns**: the sidecar pairs them with their target tx and reinjects both so the bid lands immediately behind the target (rather than being ranked absolutely, which a large bid would otherwise win). Pairing works regardless of which tx the node sees first and is bounded by `--backrun-pool-ttl-ms` / `--backrun-pool-max`; see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) §"Backrun pairing". Pairing activity is exposed via the `backrun_*` metrics and `/health`.

Without `--network`, the sidecar falls back to stamping every `Insert` with the constant `--tx-priority-hex` (legacy mode, no gateway filter — only suitable for single-tenant local dev).

### Networks and the gateway address

There is exactly one `MagmaSearcherGateway` per network. The address is baked into [`src/policy.rs`](src/policy.rs) so a gateway redeploy ships as a versioned binary rather than an ops change:

| `--network` | Gateway address | Notes |
|---|---|---|
| `mainnet` | (TODO: real address) | placeholder until the mainnet gateway is deployed |
| `testnet` | (TODO: real address) | placeholder until the testnet gateway is deployed |
| `localnet` | `0x8f86403a4de0bb5791fa46b8e795c547942fe4cf` | deterministic deployment from `mev-entrypoint/test-scripts/make deploy` against the local Monad devnet |

The score is `priority_fee × gas_limit + bid`, where the bid is:

- the `bidAmount` argument decoded from `magmaSearcherGatewayCall(address sender, uint256 bidAmount, uint64 targetBlockNumber, bytes32 targetTxHash, bool requireExclusiveSlot, address searcherContract, bytes searcherCallData)` calldata when `to == gateway` and the selector matches (the on-chain enforced minimum net ETH gain on the gateway contract; see `mev-entrypoint`), or
- **zero** for any other call to the gateway (empty calldata, a non-matching selector, a direct `receive()` top-up). We deliberately do not credit `tx.value`: a `receive()` deposit is an operational top-up, not a searcher bid declared as a minimum net gain, and treating it as one would let anyone buy priority by sending native value to the gateway.

See `docs/ARCHITECTURE.md` §"Priority policy" and `src/policy.rs` for the precise definition.

Environment (optional, every variable maps 1:1 to a CLI flag — CLI > env > default):

- `MAGMA_SIDECAR_BIND` — default `127.0.0.1:8089`
- `MAGMA_MONAD_RPC_URL` — Monad JSON-RPC base URL (target of `/rpc/monad`)
- `MAGMA_TXPOOL_SOCKET` — Unix socket path for txpool IPC (default `/home/monad/monad-bft/mempool.sock`; omit/comment to run ingress-only)
- `MAGMA_NETWORK` — `mainnet` | `testnet` | `localnet` (omit to disable gateway scoring)
- `MAGMA_TX_PRIORITY` — fallback hex priority for outbound `EthTxPoolIpcTx` (default `0xffff`, CLI flag `--tx-priority-hex`)
- `MAGMA_BACKRUN_POOL_TTL_MS` — how long the backrun pairing pool holds a cached target / parked bid (default `2500`, CLI flag `--backrun-pool-ttl-ms`)
- `MAGMA_BACKRUN_POOL_MAX` — max candidate-target txs cached for backrun pairing (default `4096`, CLI flag `--backrun-pool-max`)
- `RUST_LOG` — e.g. `info,magma_sidecar=debug`

For local dev, copy `.env.example` to `.env.local` (gitignored), edit anything host-specific, then `set -a; source .env.local; set +a; cargo run --release` — see [`docs/LOCAL_DEVELOPMENT.md`](docs/LOCAL_DEVELOPMENT.md) §2 for the full flow.

### Example: forward a raw tx via the sidecar

Point your client at the sidecar instead of the node directly:

```bash
curl -s http://127.0.0.1:8089/rpc/monad \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_sendRawTransaction","params":["0x..."],"id":1}'
```

## Related repos

- `mev-entrypoint` — `MagmaSearcherGateway` contracts and test scripts
- `monad-bft` — node (txpool IPC protocol lives here; this repo links the IPC type crates as git deps pinned by `rev`, with an optional sibling-checkout `path` override for local dev)
