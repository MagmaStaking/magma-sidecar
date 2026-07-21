# GitHub Workflows

## Workflows

### `ci.yml`
- Triggers: every push to any branch, pull requests
- Jobs: `cargo fmt --check`, `cargo test --all-targets --locked`
- Purpose: gatekeeping, no artifacts published

### `build-and-publish.yml`
- Triggers:
  - Tag push matching `v*` (e.g. `v1.0.0`) — full release + APT publish
  - Manual `workflow_dispatch` from **`main` only** — build smoke test (Docker +
    `.deb` artifacts / GHCR `:dev` tags); **does not** sign or publish to APT
- Jobs:
  - `build-docker` — matrix over `amd64` (ubuntu-24.04) and `arm64` (ubuntu-24.04-arm); native (no QEMU) per-arch development/test images pushed by digest
  - `merge-docker` — stitches the per-arch digests into one multi-arch development/test tag at `ghcr.io/magmastaking/magma-sidecar` (not approved for validator hosts)
  - `build-deb` — matrix over `amd64` (ubuntu-24.04) and `arm64` (ubuntu-24.04-arm); both native builds
  - `publish-release-and-apt` — **tags only**; uses the `apt-publish` Environment; signs a release manifest, attaches release artifacts to GitHub Releases, and publishes both arches plus the manifest to the signed GitHub Pages APT repo

## Version scheme

| Trigger | Package version | Docker tags | APT / GitHub Release |
|---|---|---|---|
| `v1.2.3` tag | `1.2.3` | `1.2.3`, `1.2`, `1`, `latest` | Yes (via `apt-publish`) |
| `workflow_dispatch` on `main` | `0~dev.<short-sha>` | `dev`, `sha-<short-sha>` | No |

The leading `0~` in the dev version is a [Debian pre-release marker](https://www.debian.org/doc/debian-policy/ch-controlfields.html#version) — kept for local/artifact naming consistency even though dispatch no longer publishes to APT.

## Environment: `apt-publish` (required)

Production APT credentials must **not** be repository secrets. They belong on a
GitHub Environment so branch workflows cannot reach them.

### One-time setup

```bash
SIDECAR_REPO=MagmaStaking/magma-sidecar

# 1. Create the Environment (idempotent).
gh api -X PUT "repos/${SIDECAR_REPO}/environments/apt-publish" --input - <<'EOF'
{
  "deployment_branch_policy": {
    "protected_branches": false,
    "custom_branch_policies": true
  }
}
EOF

# 2. Allow only release tags (not branches / workflow_dispatch).
gh api -X POST "repos/${SIDECAR_REPO}/environments/apt-publish/deployment-branch-policies" \
  -f name='v*' -f type=tag

# 3. In the GitHub UI: Settings → Environments → apt-publish →
#    Required reviewers → add release admins. (Optional but strongly recommended.)

# 4. Move secrets onto the Environment (then DELETE the repo-level copies).
#    If the values are still only in repo secrets, copy them first:
gh secret set APT_SIGNING_KEY -R "${SIDECAR_REPO}" --env apt-publish < apt_signing_key.b64
printf '%s' "${APT_REPO_TOKEN}" | gh secret set APT_REPO_TOKEN -R "${SIDECAR_REPO}" --env apt-publish

# 5. Remove repository secrets so branch jobs cannot read them.
gh secret delete APT_SIGNING_KEY -R "${SIDECAR_REPO}" || true
gh secret delete APT_REPO_TOKEN -R "${SIDECAR_REPO}" || true
```

`APT_REPO` may stay a repository variable (non-secret).

### Why this matters

Anyone with write can push a branch that edits the workflow and dispatch it.
Repository secrets are then available to that branch code. Environment secrets
tied to `v*` tag refs (plus required reviewers) close that path: a branch run
cannot obtain `APT_SIGNING_KEY` / `APT_REPO_TOKEN`.

## Required Environment secrets (`apt-publish`)

| Name | Purpose |
|---|---|
| `APT_SIGNING_KEY` | Base64-encoded GPG **private** key used to sign `Release`. Generate with `gpg --export-secret-keys <key-id> \| base64 \| tr -d '\n'`. |
| `APT_REPO_TOKEN` | Fine-grained PAT with **Contents: write** on the APT repo. Lets CI push the index over HTTPS (the built-in `GITHUB_TOKEN` can't reach another repo, and the org has SSH deploy keys disabled). |

`GITHUB_TOKEN` is still auto-provided for GHCR + GitHub Releases.

## Required vars (optional, with defaults)

| Name | Default | Purpose |
|---|---|---|
| `APT_REPO` | `MagmaStaking/magma-sidecar-apt-repo` | The Pages-hosted APT repo (pool store + served site). |

## Action pinning

Third-party Actions are pinned to full commit SHAs (with a version comment).
Dependabot (`.github/dependabot.yml`) opens PRs for Action updates.

## One-time external setup (done outside this repo)

The CI workflow assumes the following already exist; the first tagged release will fail until they're set up. All of this is automated by
[`bootstrap-apt-repo.sh`](https://github.com/MagmaStaking/magma-apt) in the private `magma-apt` runbook:

1. **APT repo** `MagmaStaking/magma-sidecar-apt-repo` seeded with `.nojekyll`, the public signing key, and an empty `pool/`, served by **GitHub Pages** (Deploy from a branch → `main` / root). Must be **public** for `apt` to fetch anonymously.
2. **GPG keypair** for signing the APT `Release` file. The public key is committed to the APT repo root as `magma-apt-key.gpg.bin`; the private key becomes Environment secret `APT_SIGNING_KEY`.
3. **Fine-grained PAT** with `Contents: write` on the APT repo; its value becomes Environment secret `APT_REPO_TOKEN`. (SSH deploy keys are disabled org-wide, so a PAT is used for the cross-repo push.)
4. **Environment `apt-publish`** configured as above (tag policy, reviewers, secrets). Repo var `APT_REPO` populated.

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

Advanced signing-key, release-manifest, package-hash, and provenance checks are
documented in
[`docs/RELEASE_VERIFICATION.md`](../../docs/RELEASE_VERIFICATION.md).
