# Releasing magma-sidecar

Maintainer runbook for cutting a release and getting it onto a validator. The
heavy lifting is automated in [`.github/workflows/build-and-publish.yml`](../.github/workflows/build-and-publish.yml);
this doc covers the human steps around it.

## Prerequisites (one-time / org-level)

If the APT repo has never been stood up, do [`APT_REPO_SETUP.md`](APT_REPO_SETUP.md)
first — it's the unambiguous, step-by-step bootstrap for everything below.

Assumed already in place:

- GitHub secrets: `APT_SIGNING_KEY` (base64 GPG private key), `APT_REPO_TOKEN`
  (fine-grained PAT with Contents:write on the APT repo); repo var `APT_REPO`
  (optional; defaults to `MagmaStaking/magma-sidecar-apt-repo`).
- The APT repo (`MagmaStaking/magma-sidecar-apt-repo`) seeded with the signing
  **public** key and served by GitHub Pages (once public).
- `GITHUB_TOKEN` with `packages: write` (provided by Actions) for GHCR.

## Before every release

1. **Bake the gateway address for the target network.** `mainnet`, `testnet`, and
   `localnet` all have their `MagmaSearcherGateway` address baked into
   [`src/policy.rs`](../src/policy.rs) today. If a gateway is redeployed (or a new
   network is added — it starts as a `0x0` placeholder that the startup guard refuses
   to boot on), update its address (and `base_fee_floor_wei` if non-zero) there and
   merge to `main` before releasing.
2. **Land everything on `main`** and confirm CI (`ci.yml`: fmt + clippy + test) is green.
   `Cargo.lock` must be committed.
3. **Update [`CHANGELOG.md`](../CHANGELOG.md):** move `Unreleased` items into a new
   version section with today's date, and fill in the **Compatibility** row — the
   `monad-bft` IPC `rev` (from `Cargo.toml`) and the Monad node release this build was
   validated against. Validators use this to avoid pairing mismatched versions.
4. **Pick the version.** Semantic versioning; while `0.x`, treat every minor as
   potentially breaking. The CI tag regex accepts **only** `v<major>.<minor>.<patch>`
   — pre-release suffixes like `-beta.1` are rejected, so use a `0.x` version to signal
   a beta rather than a suffix.

## Cut the release

```bash
git checkout main && git pull
git tag vX.Y.Z          # e.g. v0.1.0 — must match v<major>.<minor>.<patch> exactly
git push origin vX.Y.Z
```

Pushing the tag triggers the pipeline, which:

1. Derives the version from the tag.
2. Builds and pushes the multi-arch Docker image to
   `ghcr.io/magmastaking/magma-sidecar` (`:X.Y.Z`, `:X.Y`, `:X`, `:latest`);
   this image is a development/test artifact and is not approved for validator
   hosts.
3. Builds `amd64` + `arm64` `.deb`s on native runners.
4. Creates the GitHub Release with both `.deb`s attached.
5. Regenerates and GPG-signs the APT index and pushes it to the GitHub Pages APT repo.

### Staging / dry run

A manual **workflow_dispatch** run (no tag) publishes a `0~dev.<sha>` package to the
APT repo **without** cutting a GitHub Release — use it to smoke-test the pipeline
before committing to a real tag:

```bash
gh workflow run build-and-publish.yml --ref main
```

## Verify

```bash
# GitHub release + attached .debs
gh release view vX.Y.Z

# APT metadata refreshed and signed (public Pages URL)
curl -fsSL https://magmastaking.github.io/magma-sidecar-apt-repo/dists/stable/InRelease | head
curl -fsSL https://magmastaking.github.io/magma-sidecar-apt-repo/dists/stable/main/binary-amd64/Packages \
  | grep -A1 '^Package: magma-sidecar'

# Development/test Docker image present
docker manifest inspect ghcr.io/magmastaking/magma-sidecar:X.Y.Z >/dev/null && echo OK
```

Promote progressively: validate on **testnet** (with the testnet gateway baked in)
before tagging a mainnet-targeted release.

## Install on a validator

The package creates a dedicated `magma-sidecar` system user. It must not be
added to the `monad` group. Before enabling the service, follow
[`VALIDATOR_INSTALL.md`](VALIDATOR_INSTALL.md) to move the mempool socket to
`/var/run/monad-ipc/mempool.sock` and grant ACL-only access.

```bash
sudo apt update
sudo apt install magma-sidecar=X.Y.Z        # pin the version explicitly
# or, from the GitHub Release:
#   sudo dpkg -i magma-sidecar_X.Y.Z_amd64.deb

sudo vim /etc/magma-sidecar/sidecar.env     # confirm socket path and network
sudo systemctl enable --now magma-sidecar
sudo systemctl status magma-sidecar
journalctl -u magma-sidecar -f              # "loaded tip policy network=..." then "connected to Monad txpool IPC"
curl -s http://127.0.0.1:8089/health | jq   # ipc_state: "connected"
```

`postinst` seeds `sidecar.env` only on first install, so upgrades never clobber
operator config.

### Resource limits & CPU pinning

The shipped unit ([`debian/sidecar/magma-sidecar.service`](../debian/sidecar/magma-sidecar.service))
applies cgroup v2 caps so a runaway or compromised sidecar can't starve the
node: `MemoryHigh`/`MemoryMax` (soft throttle then hard OOM ceiling), `TasksMax`,
`CPUWeight` + `CPUQuota` (yields under contention, capped at one core),
`IOWeight`, and `LimitNOFILE`. It also **pins to cores 12-15** (`AllowedCPUs`/
`CPUAffinity`) — the RPC cores — to keep the reprioritizer off the node's
consensus cores (8-11), where scheduler jitter would matter most; the RPC path
is far less latency-critical, so co-locating there is cheap. The memory/task
caps are conservative starting points, not measured — validate and retune:

```bash
systemctl status magma-sidecar     # reports memory peak
systemd-cgtop                       # live CPU/mem/IO per service
```

If a host's core layout differs from the standard 8-11 (consensus) / 12-15 (RPC)
split, override the pinning (or any cap) via a **drop-in** rather than editing
the packaged unit — drop-ins survive `apt upgrade`:

```bash
# See which cores are already claimed so you can pick a non-consensus set:
systemctl show monad-bft -p AllowedCPUs -p CPUAffinity   # node (consensus)
systemctl show monad-rpc -p AllowedCPUs -p CPUAffinity   # RPC

sudo systemctl edit magma-sidecar
# In the editor:
#   [Service]
#   AllowedCPUs=<non-consensus cores>
#   CPUAffinity=<non-consensus cores>
#   # Or raise the memory ceiling on a busy host:
#   MemoryMax=1G
sudo systemctl daemon-reload && sudo systemctl restart magma-sidecar
systemctl show magma-sidecar -p MemoryMax -p CPUQuotaPerSecUSec -p AllowedCPUs -p TasksMax
```

## Upgrade / rollback

- **Upgrade:** `sudo apt update && sudo apt install magma-sidecar=X.Y.Z` — `prerm`
  stops the unit, `postinst` `try-restart`s it, config is preserved.
- **Rollback:** `sudo apt install magma-sidecar=<previous> && sudo systemctl restart magma-sidecar`
  (or `dpkg -i` the older release `.deb`). Because the gateway address is baked per
  binary, rolling back also reverts the gateway — fine unless a gateway redeploy
  happened in between, in which case roll the address *forward* in a new patch release
  instead of rolling the binary back.

## Post-release

- Confirm a clean-box `apt update && apt install magma-sidecar=X.Y.Z` works on both
  `amd64` and `arm64`.
- Open a fresh `## [Unreleased]` section in `CHANGELOG.md`.
