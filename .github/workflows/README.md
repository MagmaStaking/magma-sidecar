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
  - `build-docker` — matrix over `amd64` (ubuntu-24.04) and `arm64` (ubuntu-24.04-arm); native (no QEMU) per-arch images pushed by digest
  - `merge-docker` — stitches the per-arch digests into one multi-arch tag at `ghcr.io/magmastaking/magma-sidecar`
  - `build-deb` — matrix over `amd64` (ubuntu-24.04) and `arm64` (ubuntu-24.04-arm); both native builds
  - `publish-release-and-apt` — only on tag or manual dispatch; attaches `.deb`s to a GitHub Release (tags only) and publishes both arches to the signed S3 APT repo

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
| `AWS_ACCESS_KEY_ID` | IAM credentials for the S3 APT bucket. |
| `AWS_SECRET_ACCESS_KEY` | Paired with the above. |
| `APT_SIGNING_KEY` | Base64-encoded GPG **private** key used to sign `Release`. Generate with `gpg --armor --export-secret-keys <key-id> \| base64 -w0`. |

## Required vars (optional, with defaults)

| Name | Default | Purpose |
|---|---|---|
| `AWS_REGION` | `eu-central-1` | Region of the S3 bucket. |
| `S3_APT_BUCKET` | `magma-apt-repo` | Bucket name. |

## One-time external setup (done outside this repo)

The CI workflow assumes the following already exist; the first tagged release will fail until they're set up:

1. **S3 bucket** named `magma-apt-repo` (or whatever you set `S3_APT_BUCKET` to) with public-read on `pool/*` and `dists/*` (or a CloudFront distribution if you want HTTPS without bucket-website hosting).
2. **GPG keypair** for signing the APT `Release` file. Publish the public key somewhere reachable so end users can install it:
   ```bash
   sudo wget -qO /etc/apt/keyrings/magma.gpg https://<your-host>/magma-apt-key.gpg.bin
   ```
3. **IAM user / role** with `s3:PutObject`, `s3:GetObject`, `s3:ListBucket` on the bucket. Add the access key + secret as repo secrets.
4. **Repo secrets/vars** populated as listed above.

## Consumer install snippet

Once the APT repo is live, end users install with:

```bash
sudo mkdir -p /etc/apt/keyrings
sudo wget -qO /etc/apt/keyrings/magma.gpg https://<your-host>/magma-apt-key.gpg.bin

echo "deb [signed-by=/etc/apt/keyrings/magma.gpg] https://magma-apt-repo.s3.amazonaws.com stable main" \
  | sudo tee /etc/apt/sources.list.d/magma.list

sudo apt update
sudo apt install magma-sidecar
```
