# Changelog

All notable changes to **magma-sidecar** are documented here. This project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html); while the major version is
`0`, behaviour may change between minor releases.

## [Unreleased]

### Added

- Baked in the deployed `MagmaSearcherGateway` proxy addresses for `mainnet`
  (`0xe0232Cf5ee0c6d79118498c29a267D80881011C5`) and `testnet`
  (`0x21615eDffD849eEd1C08e780032Da3bCd1003CD3`), replacing the `0x0`
  placeholders. All three networks are now runnable.

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

[0.1.0]: https://github.com/MagmaStaking/magma-sidecar/releases/tag/v0.1.0
