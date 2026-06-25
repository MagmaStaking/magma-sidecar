# Changelog

All notable changes to **magma-sidecar** are documented here. This project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html); while the major version is
`0`, behaviour may change between minor releases.

## [0.1.0] - 2026-06-25

Initial beta release.

A co-located sidecar for a Monad validator that:

- forwards searcher JSON-RPC to the Monad EL (`POST /rpc/monad`), and
- reprioritizes the node's txpool over IPC, ranking transactions to the allowlisted
  `MagmaSearcherGateway` by tip (`priority_fee × gas_limit + bidAmount`), including
  backrun bid/target pairing.

Ships with Prometheus `/metrics` + `/health`, a multi-arch Docker image, and a Debian
package with a hardened systemd unit. See `README.md` and `docs/ARCHITECTURE.md`.

**Compatibility:** built against `monad-bft` IPC rev `cd04c9e` (record the validated
Monad node release here before tagging).

**Note:** `mainnet`/`testnet` gateway addresses are placeholders until deployed; only
`localnet` is runnable today (the startup guard enforces this).

[0.1.0]: https://github.com/MagmaStaking/magma-sidecar/releases/tag/v0.1.0
