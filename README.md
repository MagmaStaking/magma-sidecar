# magma-sidecar

Rust **sidecar** for a Monad validator described in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md):

- **Txpool IPC reprioritization** — connects to the node's txpool Unix socket, observes `EthTxPoolEvent`s, and re-injects each MEV-relevant transaction with a **tip-derived priority** so the node orders it ahead of vanilla traffic.

Searchers submit transactions to the Monad node's JSON-RPC as usual. The sidecar also exposes an observability-only HTTP server (`/health`, `/metrics`) to monitor its health and performance.

This repo is **separate** from `monad-bft` (its own Cargo project, not a workspace member of the node repo). It pulls `monad-eth-txpool-ipc` and `monad-eth-txpool-types` straight from the upstream **`category-labs/monad-bft`** repo, pinned to a specific commit in `Cargo.toml`.

## Installation

The signed Debian package is the only supported deployment path for validator
hosts. Docker images and source builds are development conveniences, not
validator production distributions. For local development against a Monad
devnet, jump to [Run from source](#run-from-source) and
[`docs/LOCAL_DEVELOPMENT.md`](docs/LOCAL_DEVELOPMENT.md).

### Option 1: Debian package via APT (recommended for validator hosts)

Follow [`docs/VALIDATOR_INSTALL.md`](docs/VALIDATOR_INSTALL.md) before enabling
the service. The stock Monad node and RPC units still need drop-ins pointing to
`/var/run/monad-ipc/mempool.sock`.

Hosts a versioned `.deb` with a hardened systemd unit running as the dedicated
`magma-sidecar` user. Access to the node's txpool IPC socket is granted only by
an ACL under `/var/run/monad-ipc`; the sidecar is not a member of the `monad`
group.

Every published version also has a GPG-signed release manifest under
`https://magmastaking.github.io/magma-sidecar-apt-repo/releases/<version>/`
containing the source commit, container digest, and `.deb` SHA256 hashes.
Verification commands are in
[`docs/RELEASE_VERIFICATION.md`](docs/RELEASE_VERIFICATION.md).

The Debian package ships:

- Binary: `/usr/bin/magma-sidecar`
- Systemd unit: `/lib/systemd/system/magma-sidecar.service` (runs as `User=magma-sidecar`, hardened, `Restart=always`)
- Config template: `/etc/magma-sidecar/sidecar.env.example`
- IPC ACL helper: `/usr/lib/magma-sidecar/monad-ipc-setup`
- Validator runbook: `/usr/share/doc/magma-sidecar/VALIDATOR_INSTALL.md`
- Release verification guide: `/usr/share/doc/magma-sidecar/RELEASE_VERIFICATION.md`

The `postinst` script seeds `/etc/magma-sidecar/sidecar.env` from the example **only on first install** — upgrades never clobber operator-edited config.



You can also grab a release `.deb` directly from [GitHub Releases](https://github.com/MagmaStaking/magma-sidecar/releases) (`amd64` + `arm64` are both published) and `sudo dpkg -i magma-sidecar_<version>_<arch>.deb` if you don't want the APT repo.

### Option 2: Docker (development only)

Multi-arch images at `ghcr.io/magmastaking/magma-sidecar` (`linux/amd64` + `linux/arm64`).

> **Do not deploy this image on validator hosts.** It is published only for
> local development and test environments. It is not part of the validator
> approval path and does not ship the rootless, read-only,
> no-new-privileges deployment policy required for that environment.

```bash
docker pull ghcr.io/magmastaking/magma-sidecar:latest
# Or pin: ghcr.io/magmastaking/magma-sidecar:1.0.0

# Reprioritize a node's txpool: bind-mount the node's IPC socket and pick the network.
docker run --rm -p 127.0.0.1:8089:8089 \
  -v /run/monad:/run/monad:ro \
  -e MAGMA_TXPOOL_SOCKET=/run/monad/mempool.sock \
  -e MAGMA_NETWORK=localnet \
  ghcr.io/magmastaking/magma-sidecar:latest
```

See the comment block at the top of [`Dockerfile`](Dockerfile) for the full incantation (the AF_UNIX 107-byte path limit comes up here; [`docs/LOCAL_DEVELOPMENT.md`](docs/LOCAL_DEVELOPMENT.md) §1a has the workaround). Without `MAGMA_TXPOOL_SOCKET` the container just serves `/health` and `/metrics` and does no reprioritization.

The observability endpoints are unauthenticated. Keep the host publication
loopback-only as shown; use an authenticated reverse proxy or a tightly scoped
monitoring network if remote scraping is required.

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
# Environment="RUST_LOG=info,magma_sidecar=trace"
```

## Surfaces

| Surface | Purpose |
|---------|---------|
| `GET /health` | Structured liveness: IPC state, counters, last-event/last-send timestamps |
| `GET /metrics` | Prometheus exposition (counters, gauges; namespaced `magma_sidecar_*`) |
| **Txpool IPC** (optional) | `--txpool-socket` connects to the node's txpool Unix socket, consumes `EthTxPoolEvent` batches, and re-injects only `Insert`s targeting an allowlisted `MagmaSearcherGateway` as `EthTxPoolIpcTx` with a tip-based priority. |

## Run

Reprioritization requires a txpool socket. Run **with txpool IPC + tip policy** (same socket the node exposes for `EthTxPoolIpcClient`):

```bash
cd magma-sidecar
cargo run --release -- \
  --bind 127.0.0.1:8089 \
  --txpool-socket /path/to/mempool.sock \
  --network localnet
```

Without `--txpool-socket` the sidecar just serves `/health` and `/metrics` and does no reprioritization.

The sidecar **only reinjects valid `magmaSearcherGatewayCall` transactions** whose `to` is the network's allowlisted `MagmaSearcherGateway`, plus referenced backrun targets. Vanilla traffic is observed (so `tx_inserts_observed` still climbs) but left alone, so the node's default ordering applies and the sidecar doesn't fight other reprioritizers for unrelated txs. Invalid/unsupported gateway calls are also left alone to avoid IPC amplification. Skips are counted separately in `txpool_skipped_non_gateway_total` and `txpool_skipped_invalid_gateway_total`.

Bids that carry a non-zero `targetTxHash` are treated as **backruns**: the sidecar pairs them with their target tx and reinjects both so the bid lands immediately behind the target (rather than being ranked absolutely, which a large bid would otherwise win). Pairing works regardless of which tx the node sees first and is bounded by a configurable TTL plus compiled-in capacity limits; see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) §"Backrun pairing". Pairing activity is exposed via the `backrun_*` metrics and `/health`.

### Networks and the gateway address

There is one `MagmaSearcherGateway` per network. The address is baked into [`src/policy.rs`](src/policy.rs) so a gateway redeploy ships as a versioned binary:

| `--network` | Gateway address | Notes |
|---|---|---|
| `mainnet` | `0xe0232Cf5ee0c6d79118498c29a267D80881011C5` | `MagmaSearcherGateway` proxy on Monad mainnet (chain id 143) |
| `testnet` | `0x21615eDffD849eEd1C08e780032Da3bCd1003CD3` | `MagmaSearcherGateway` proxy on Monad testnet (chain id 10143) |
| `localnet` | `0xe7f1725e7734ce288f8367e1bb143e90bb3f0512` | deterministic deployment from `mev-entrypoint/test-scripts/` `make deploy` against the local Monad devnet |

The score is `priority_fee × gas_limit + bid`, where the bid is:

- the `bidAmount` argument decoded from `magmaSearcherGatewayCall(address sender, uint256 bidAmount, uint64 targetBlockNumber, bytes32 targetTxHash, bool requireExclusiveSlot, address searcherContract, bytes searcherCallData)` calldata when `to == gateway` and the selector matches (the on-chain enforced minimum net ETH gain on the gateway contract; see `mev-entrypoint`), or
- no sidecar priority for any other call to the gateway (empty calldata, a non-matching selector, a direct `receive()` top-up). These transactions are observed but not reinjected. We deliberately do not credit `tx.value`: a `receive()` deposit is an operational top-up, not a searcher bid declared as a minimum net gain, and treating it as one would let anyone buy priority by sending native value to the gateway.

See `docs/ARCHITECTURE.md` §"Priority policy" and `src/policy.rs` for the precise definition.

Environment (optional, every variable maps 1:1 to a CLI flag — CLI > env > default):

- `MAGMA_SIDECAR_BIND` — default `127.0.0.1:8089` (observability HTTP: `/health`, `/metrics`)
- `MAGMA_TXPOOL_SOCKET` — Unix socket path for txpool IPC (packaged default `/var/run/monad-ipc/mempool.sock`; omit/comment to disable reprioritization)
- `MAGMA_NETWORK` — `mainnet` | `testnet` | `localnet` (default `mainnet`; selects the baked-in gateway to score against)
- `MAGMA_BACKRUN_POOL_TTL_MS` — how long the backrun pairing pool holds a cached target / parked bid (default `2500`, CLI flag `--backrun-pool-ttl-ms`)
- `RUST_LOG` — production default `info`; use `info,magma_sidecar=trace` temporarily for per-transaction diagnostics

Safety-sensitive priority and state-cap values are compiled into the binary:
fallback priority `0xffff`, target cache `4096`, pending bids `4096`, and
reinjection dedup hashes `16384`. Validators do not need to tune them.

For local dev, copy `.env.example` to `.env.local` (gitignored), edit anything host-specific, then `set -a; source .env.local; set +a; cargo run --release` — see [`docs/LOCAL_DEVELOPMENT.md`](docs/LOCAL_DEVELOPMENT.md) §2 for the full flow.

### Check liveness

```bash
curl -s http://127.0.0.1:8089/health | jq
# {"status":"ok","ipc_state":"connected","tx_inserts_observed":N,"tx_prioritized":N,...}
```

## Related repos

- `mev-entrypoint` — `MagmaSearcherGateway` contracts and test scripts
- `monad-bft` — node (txpool IPC protocol lives here; this repo links the IPC type crates as git deps pinned by `rev`, with an optional sibling-checkout `path` override for local dev)
