# Changelog

All notable changes to **magma-sidecar** are documented here. This project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html); while the major version is
`0`, behaviour may change between minor releases.

## [Unreleased]

### Changed

- `--network` / `MAGMA_NETWORK` now defaults to **`mainnet`** so a standard
  validator install reprioritizes against the mainnet gateway out of the box.
  Local development must set `--network localnet` explicitly.

### Removed

- Dropped the legacy "no network" mode that stamped every txpool `Insert` with a
  constant priority. The sidecar now always runs the gateway-allowlist tip policy;
  `--tx-priority-hex` remains only as the fallback for gateway txs whose computed
  score is exactly zero.

### Fixed

- Corrected the `localnet` gateway address in `README.md` to match the value
  baked into `src/policy.rs` (`0xe7f1725e…`), and refreshed
  `docs/LOCAL_DEVELOPMENT.md` to reflect the mainnet-defaulted `.env.example`.

## [0.1.03] - 2026-07-02

### Added

- Baked in the deployed `MagmaSearcherGateway` proxy addresses for `mainnet`
  (`0xe0232Cf5ee0c6d79118498c29a267D80881011C5`) and `testnet`
  (`0x21615eDffD849eEd1C08e780032Da3bCd1003CD3`), replacing the `0x0`
  placeholders. All three networks are now runnable.
- Hardened the systemd unit with cgroup resource caps (`MemoryHigh`/`MemoryMax`,
  `MemorySwapMax`, `TasksMax`, `IOWeight`, `LimitNOFILE`) and CPU containment
  (`CPUWeight`/`CPUQuota` plus pinning to non-consensus cores), so a runaway or
  compromised sidecar can't starve the validator. Retunable per host via a
  systemd drop-in — see `docs/RELEASING.md`.

### Changed

- APT distribution moved from S3 to GitHub Pages; packages are published via
  GitHub-signed commits (see `docs/RELEASING.md`). Install instructions and the
  repo/key URLs in `README.md` are updated accordingly.

### Removed

- Dropped the transparent JSON-RPC ingress (`POST /rpc/monad`) and its `MAGMA_MONAD_RPC_URL`
  configuration. Searchers submit to the Monad node's JSON-RPC directly and the sidecar reprioritizes what it observes on
  the txpool IPC socket. The HTTP server now serves `/health` and `/metrics` only.

## [0.1.0] - 2026-06-25

Initial beta release.

A co-located sidecar for a Monad validator that:

- reprioritizes the node's txpool over IPC, ranking transactions to the allowlisted
  `MagmaSearcherGateway` by tip (`priority_fee × gas_limit + bidAmount`), including
  backrun bid/target pairing.

Ships with Prometheus `/metrics` + `/health`, a multi-arch Docker image, and a Debian
package with a hardened systemd unit. See `README.md` and `docs/ARCHITECTURE.md`.

**Compatibility:** built against `monad-bft` IPC rev `cd04c9e` (record the validated
Monad node release here before tagging).

**Note:** at the time of this release `mainnet`/`testnet` gateway addresses were `0x0`
placeholders (only `localnet` was runnable; the startup guard enforced this). The real
addresses were baked in later — see the `Unreleased` section above.

[0.1.03]: https://github.com/MagmaStaking/magma-sidecar/releases/tag/v0.1.03
[0.1.0]: https://github.com/MagmaStaking/magma-sidecar/releases/tag/v0.1.0
