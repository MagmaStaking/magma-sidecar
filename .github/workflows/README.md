# GitHub Workflows

## Workflows

### `ci.yml`
- Triggers: every push to any branch, pull requests
- Jobs: `cargo fmt --check`, `cargo test --all-targets --locked`
- Purpose: gatekeeping, no artifacts published

### `build-and-publish.yml`
- Triggers:
  - Tag push matching `v*` (e.g. `v1.0.0`)
  - Manual `workflow_dispatch` (publishes a `0~dev.<sha>` build)
- Jobs:
  - `build-docker` — matrix over `amd64` (ubuntu-24.04) and `arm64` (ubuntu-24.04-arm); native (no QEMU) per-arch development/test images pushed by digest
  - `merge-docker` — stitches the per-arch digests into one multi-arch development/test tag at `ghcr.io/magmastaking/magma-sidecar` (not approved for validator hosts)
  - `build-deb` — matrix over `amd64` (ubuntu-24.04) and `arm64` (ubuntu-24.04-arm); both native builds
  - `publish-release-and-apt` — only on tag or manual dispatch; attaches `.deb`s to a GitHub Release (tags only) and publishes both arches to the signed GitHub Pages APT repo

## Version scheme

| Trigger | Package version | Docker tags |
|---|---|---|
| `v1.2.3` tag | `1.2.3` | `1.2.3`, `1.2`, `1`, `latest` |
| `workflow_dispatch` | `0~dev.<short-sha>` | `dev`, `sha-<short-sha>` |

The leading `0~` in the dev version is a [Debian pre-release marker](https://www.debian.org/doc/debian-policy/ch-controlfields.html#version) — dev builds always compare lower than any real release, so a host on `1.2.3` won't get downgraded by mistake.

## Required secrets

| Name | Purpose |
|---|---|
| `GITHUB_TOKEN` | Auto-provided. Used to push to GHCR + create releases. |
| `APT_SIGNING_KEY` | Base64-encoded GPG **private** key used to sign `Release`. Generate with `gpg --export-secret-keys <key-id> \| base64 \| tr -d '\n'`. |
| `APT_REPO_TOKEN` | Fine-grained PAT with **Contents: write** on the APT repo. Lets CI `git push` the index over HTTPS (the built-in `GITHUB_TOKEN` can't reach another repo, and the org has SSH deploy keys disabled). |

## Required vars (optional, with defaults)

| Name | Default | Purpose |
|---|---|---|
| `APT_REPO` | `MagmaStaking/magma-sidecar-apt-repo` | The Pages-hosted APT repo (pool store + served site). |

## One-time external setup (done outside this repo)

The CI workflow assumes the following already exist; the first tagged release will fail until they're set up. All of this is automated by
[`bootstrap-apt-repo.sh`](https://github.com/MagmaStaking/magma-apt) in the private `magma-apt` runbook:

1. **APT repo** `MagmaStaking/magma-sidecar-apt-repo` seeded with `.nojekyll`, the public signing key, and an empty `pool/`, served by **GitHub Pages** (Deploy from a branch → `main` / root). Must be **public** for `apt` to fetch anonymously.
2. **GPG keypair** for signing the APT `Release` file. The public key is committed to the APT repo root as `magma-apt-key.gpg.bin`; the private key becomes `APT_SIGNING_KEY`.
3. **Fine-grained PAT** with `Contents: write` on the APT repo; its value becomes `APT_REPO_TOKEN`. (SSH deploy keys are disabled org-wide, so a PAT is used for the cross-repo push.)
4. **Repo secrets/var** populated as listed above.

## Consumer install snippet

Once the APT repo is live (public + Pages enabled), end users install with:

```bash
BASE="https://magmastaking.github.io/magma-sidecar-apt-repo"
sudo mkdir -p /etc/apt/keyrings
sudo wget -qO /etc/apt/keyrings/magma.gpg "$BASE/magma-apt-key.gpg.bin"

echo "deb [signed-by=/etc/apt/keyrings/magma.gpg] $BASE stable main" \
  | sudo tee /etc/apt/sources.list.d/magma.list

sudo apt update
sudo apt install magma-sidecar
```
