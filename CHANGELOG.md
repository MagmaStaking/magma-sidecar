# Changelog

All notable changes to **magma-sidecar** are documented here. This project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html); while the major version is
`0`, behaviour may change between minor releases.

## [Unreleased]

## [0.1.08] - 2026-07-15

### Added

- GPG-signed release manifests containing the exact source commit, multi-arch
  image digest, and SHA256 hashes for each published `.deb`; manifests are
  attached to tagged GitHub Releases and published under the versioned APT
  repository path.
- `docs/RELEASE_VERIFICATION.md` with signing-key fingerprint,
  release-manifest, package-hash, and GitHub provenance checks.

### Changed

- APT publication now fails closed if its signing key or signed release
  manifest is missing.
- Native service resource limits now match the validator-sidecar security
  profile (`MemoryMax=512M`, `TasksMax=64`, `LimitNOFILE=4096`).

## [0.1.07] - 2026-07-14

### Added

- Dedicated, unprivileged `magma-sidecar` system user, provisioned via
  `systemd-sysusers`. The service no longer shares the node's `monad` identity
  and is never added to the `monad` group.
- `debian/sidecar/monad-ipc-setup`, a root `ExecStartPre` helper that creates
  the `/var/run/monad-ipc` tmpfs directory and default ACLs before the node
  binds the socket, giving the sidecar ACL-only access (`r-x` on the directory,
  `rw-` on the socket).
- `docs/VALIDATOR_INSTALL.md`: the supported production install path, with a
  step-by-step, copy-safe procedure for relocating the `monad-bft` /
  `monad-rpc` mempool socket to `/var/run/monad-ipc`.
- Build provenance attestation for the published `.deb` packages
  (`actions/attest-build-provenance`), verifiable with
  `gh attestation verify <pkg>.deb --repo MagmaStaking/magma-sidecar`.

### Changed

- `MAGMA_TXPOOL_SOCKET` now defaults to `/var/run/monad-ipc/mempool.sock`
  instead of `/home/monad/monad-bft/mempool.sock`.
- Docker images are explicitly scoped to development/test use and are not an
  approved validator-host distribution; the Debian package is the only
  supported deployment path for validator hosts.
- Safety-sensitive capacities (backrun pool/pending bounds, reinjection dedup
  cache) and the fallback priority are compiled in rather than exposed as
  environment variables, so validator configuration cannot weaken them.
- Per-transaction reinjection/skip logging moved from `debug` to `trace`, and
  the default `RUST_LOG` is now `info`.

### Security

- Hardened the native systemd unit: dedicated `User`/`Group`, empty
  `CapabilityBoundingSet`, `SystemCallFilter=@system-service`,
  `ProtectHome=true`, `ProtectKernelLogs`, `ProtectClock`, and a loopback-only
  network policy (`IPAddressDeny=any` + `127.0.0.0/8`/`::1/128` allow,
  `RestrictAddressFamilies`).
- Bounded the pending-bid pool and reinjection dedup map, and stopped
  reinjecting invalid/unsupported gateway calls, to prevent memory growth and
  IPC amplification from a hostile or malformed transaction stream. New metrics:
  `backrun_bids_evicted_total`, `txpool_skipped_invalid_gateway_total`,
  `txpool_sent_cache_evictions_total`, `txpool_sent_cache`.

## [0.1.04] - 2026-07-03

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

[0.1.07]: https://github.com/MagmaStaking/magma-sidecar/releases/tag/v0.1.07
[0.1.04]: https://github.com/MagmaStaking/magma-sidecar/releases/tag/v0.1.04
[0.1.03]: https://github.com/MagmaStaking/magma-sidecar/releases/tag/v0.1.03
[0.1.0]: https://github.com/MagmaStaking/magma-sidecar/releases/tag/v0.1.0
