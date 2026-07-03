#!/usr/bin/env bash
#
# Publish .deb files to the Magma APT repository hosted on GitHub Pages.
#
# The repo `MagmaStaking/magma-sidecar-apt-repo` is BOTH the pool store and the
# published site: GitHub Pages serves it (Settings -> Pages -> "Deploy from a
# branch" = `main` / root) at https://magmastaking.github.io/magma-sidecar-apt-repo.
# Publishing = clone it (read-only, to hydrate the pool), drop the new .deb into
# pool/, regenerate + GPG-sign the index, then commit the changed files back via
# the GitHub GraphQL API. Pages redeploys automatically.
#
# Why GraphQL createCommitOnBranch (not `git push`, not the Git Data REST API):
# the org enforces a "require signed commits" ruleset on every branch. Only
# GitHub App/Actions tokens get auto-signed on `git push`/REST; a PAT does not.
# The GraphQL `createCommitOnBranch` mutation, however, is signed by GitHub for
# ANY token (incl. a fine-grained PAT), so its commits land as *Verified* and
# satisfy the ruleset. Uses APT_REPO_TOKEN (Contents:write); the built-in
# GITHUB_TOKEN can't reach another repo and the org disables SSH deploy keys.
#
# Note: createCommitOnBranch inlines file contents (base64) in the request. We
# send only the files that changed for this release (the new .deb(s) plus the
# small regenerated index), so the payload stays a few MB regardless of how
# large the pool grows over time.
#
# Used by .github/workflows/build-and-publish.yml on tag pushes and manual
# dispatch. Safe to run locally: set PUSH=0 to build + sign the tree without
# committing (still needs APT_REPO_TOKEN for the initial read-only clone).
#
# Usage:
#   APT_REPO=MagmaStaking/magma-sidecar-apt-repo \
#   APT_REPO_TOKEN="github_pat_..."              \
#   APT_SIGNING_KEY_B64="$(...)"                 # optional locally, required in CI \
#   scripts/publish-apt.sh path/to/*.deb
#
# Layout in the repo (repo root == Pages site root):
#   .nojekyll                                   (disables Jekyll; seeded once)
#   magma-apt-key.gpg.bin                        (public signing key; seeded once)
#   pool/main/m/magma-sidecar/<deb-files>
#   dists/stable/main/binary-<arch>/Packages{,.gz}
#   dists/stable/Release
#   dists/stable/Release.gpg
#   dists/stable/InRelease

set -euo pipefail

if [ $# -lt 1 ]; then
    echo "Usage: $0 <deb-file> [<deb-file>...]" >&2
    exit 2
fi

: "${APT_REPO:?APT_REPO is required (e.g. MagmaStaking/magma-sidecar-apt-repo)}"
: "${APT_REPO_TOKEN:?APT_REPO_TOKEN is required (fine-grained PAT, Contents:write)}"
SUITE="${SUITE:-stable}"
COMPONENT="${COMPONENT:-main}"
ARCHES="${ARCHES:-amd64 arm64}"
PACKAGE="${PACKAGE:-magma-sidecar}"
BRANCH="${APT_REPO_BRANCH:-main}"

# gh reads GH_TOKEN for API/GraphQL calls; the PAT is what signs (via the
# createCommitOnBranch mutation) and what the clone below authenticates with.
export GH_TOKEN="${APT_REPO_TOKEN}"

# Create one GitHub-signed (Verified) commit adding/updating the given files.
#   gql_commit <owner/repo> <branch> <expected-head-oid> <headline> <root> <path...>
# Echoes the new commit oid on success.
gql_commit() {
    local repo="$1" branch="$2" oid="$3" headline="$4" root="$5"; shift 5
    local paths=("$@") adds=() p b64 joined body errf newoid

    for p in "${paths[@]}"; do
        b64="$(base64 < "${root}/${p}" | tr -d '\n')"
        adds+=("{\"path\":\"${p}\",\"contents\":\"${b64}\"}")
    done
    joined="$(IFS=,; printf '%s' "${adds[*]}")"

    body="$(mktemp)"; errf="$(mktemp)"
    {
        printf '{"query":"mutation($input: CreateCommitOnBranchInput!){createCommitOnBranch(input:$input){commit{oid}}}",'
        printf '"variables":{"input":{'
        printf '"branch":{"repositoryNameWithOwner":"%s","branchName":"%s"},' "${repo}" "${branch}"
        printf '"message":{"headline":"%s"},' "${headline}"
        printf '"fileChanges":{"additions":[%s]},' "${joined}"
        printf '"expectedHeadOid":"%s"}}}' "${oid}"
    } > "${body}"

    newoid="$(gh api graphql --input "${body}" --jq '.data.createCommitOnBranch.commit.oid' 2>"${errf}" || true)"
    if [ -z "${newoid}" ] || [ "${newoid}" = "null" ]; then
        echo "error: createCommitOnBranch failed:" >&2
        cat "${errf}" >&2
        rm -f "${body}" "${errf}"
        return 1
    fi
    rm -f "${body}" "${errf}"
    printf '%s' "${newoid}"
}

# Resolve .deb paths to absolute BEFORE we cd into the clone.
DEBS=()
for f in "$@"; do
    if [ -f "$f" ]; then
        DEBS+=("$(readlink -f "$f")")
    else
        echo "warn: '$f' not found, skipping" >&2
    fi
done
[ "${#DEBS[@]}" -gt 0 ] || { echo "error: no .deb files found among args" >&2; exit 1; }

WORKDIR="$(mktemp -d -t magma-apt-XXXXXX)"
trap 'rm -rf "${WORKDIR}"' EXIT

# ----- HTTPS auth for the read-only clone (token kept out of URLs/logs) ----
REMOTE="https://github.com/${APT_REPO}.git"
AUTH_B64="$(printf 'x-access-token:%s' "${APT_REPO_TOKEN}" | base64 | tr -d '\n')"
GIT_AUTH=(-c "http.https://github.com/.extraheader=AUTHORIZATION: basic ${AUTH_B64}")

# ----- Clone the pool store (this replaces the old `aws s3 sync` hydrate) --
REPO_DIR="${WORKDIR}/repo"
echo "Cloning ${APT_REPO} (branch ${BRANCH}) ..."
git "${GIT_AUTH[@]}" clone --depth 1 --branch "${BRANCH}" "${REMOTE}" "${REPO_DIR}"
cd "${REPO_DIR}"

# Belt-and-suspenders: make sure Jekyll stays disabled so files/dirs are served
# verbatim (Jekyll would drop entries and mangle metadata).
[ -f .nojekyll ] || touch .nojekyll

POOL_REL="pool/${COMPONENT}/$(printf '%s' "${PACKAGE}" | cut -c1)/${PACKAGE}"
echo "Pool path: ${POOL_REL}"
mkdir -p "${POOL_REL}"
for f in "${DEBS[@]}"; do
    install -m 0644 "$f" "${POOL_REL}/"
done

# apt-ftparchive does the heavy lifting (Packages with size+hashes per entry)
# instead of hand-rolling md5/sha1/sha256 loops.
for arch in ${ARCHES}; do
    bin_dir="dists/${SUITE}/${COMPONENT}/binary-${arch}"
    mkdir -p "${bin_dir}"
    echo "Indexing arch=${arch}"
    apt-ftparchive --arch "${arch}" packages "pool/" > "${bin_dir}/Packages"
    gzip -9kf "${bin_dir}/Packages"
done

cat > "${WORKDIR}/aptftp.conf" <<EOF
APT::FTPArchive::Release::Suite "${SUITE}";
APT::FTPArchive::Release::Codename "${SUITE}";
APT::FTPArchive::Release::Components "${COMPONENT}";
APT::FTPArchive::Release::Architectures "${ARCHES}";
APT::FTPArchive::Release::Origin "Magma";
APT::FTPArchive::Release::Label "Magma APT Repository";
APT::FTPArchive::Release::Description "Hydrogen Labs / Magma packages";
EOF

apt-ftparchive -c "${WORKDIR}/aptftp.conf" release "dists/${SUITE}" \
    > "dists/${SUITE}/Release"
# `Date:` isn't emitted by apt-ftparchive's `release` mode in some versions;
# prepend it unconditionally to be safe.
{ printf 'Date: %s\n' "$(date -Ru)"; cat "dists/${SUITE}/Release"; } \
    > "dists/${SUITE}/Release.tmp" \
    && mv "dists/${SUITE}/Release.tmp" "dists/${SUITE}/Release"

# GPG signing is optional locally (lets you smoke-test the layout offline),
# mandatory in CI — fail loudly if the key var was provided but is malformed.
if [ -n "${APT_SIGNING_KEY_B64:-}" ]; then
    echo "Importing signing key..."
    printf '%s' "${APT_SIGNING_KEY_B64}" | base64 -d | gpg --import --batch --yes
    gpg --batch --yes --digest-algo SHA256 \
        --clearsign -o "dists/${SUITE}/InRelease" "dists/${SUITE}/Release"
    gpg --batch --yes --digest-algo SHA256 \
        -abs -o "dists/${SUITE}/Release.gpg" "dists/${SUITE}/Release"
else
    echo "warn: APT_SIGNING_KEY_B64 not set; skipping GPG signing" >&2
fi

# ----- Commit changed files via GraphQL (Verified) ------------------------
# Collect what changed vs the cloned HEAD (added + modified; we never delete)
# and commit it all in a single, atomic Verified commit (see below).
CHANGED=()
while IFS= read -r line; do
    [ -n "$line" ] && CHANGED+=("$line")
done < <(git -c core.quotepath=false status --porcelain=v1 --untracked-files=all | cut -c4-)

if [ "${#CHANGED[@]}" -eq 0 ]; then
    echo "No changes to publish (identical index already present)."
    exit 0
fi

if [ "${PUSH:-1}" != "1" ]; then
    echo "PUSH=0 set; skipping commit. ${#CHANGED[@]} file(s) staged in ${REPO_DIR}"
    printf '  %s\n' "${CHANGED[@]}"
    exit 0
fi

STAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
HEAD_OID="$(git rev-parse HEAD)"

# Commit the pool (debs) and index files together in ONE createCommitOnBranch.
# One commit => one push event => one Pages deploy, so back-to-back publishes
# can't spawn two racing Pages builds. It's also atomic: a client can never
# observe an index that references a .deb not yet on the branch.
echo "Committing ${#CHANGED[@]} file(s) via GraphQL (Verified) ..."
HEAD_OID="$(gql_commit "${APT_REPO}" "${BRANCH}" "${HEAD_OID}" \
    "Publish ${PACKAGE} ${STAMP}" "${REPO_DIR}" "${CHANGED[@]}")"

echo ""
echo "Published. HEAD now ${HEAD_OID}"
echo "Repo URL: https://magmastaking.github.io/magma-sidecar-apt-repo"
