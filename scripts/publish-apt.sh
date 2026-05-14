#!/usr/bin/env bash
#
# Publish .deb files to the Magma S3 APT repository.
#
# Used by .github/workflows/build-and-publish.yml on tag pushes and manual
# dispatch. Safe to run locally for a smoke test as long as you have the AWS
# creds and the signing key on disk (will skip GPG signing if no key is set).
#
# Usage:
#   S3_BUCKET=magma-apt-repo                \
#   APT_SIGNING_KEY_B64="$(...)"            \
#   scripts/publish-apt.sh path/to/*.deb
#
# Layout in S3:
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

: "${S3_BUCKET:?S3_BUCKET is required}"
SUITE="${SUITE:-stable}"
COMPONENT="${COMPONENT:-main}"
ARCHES="${ARCHES:-amd64 arm64}"
PACKAGE="${PACKAGE:-magma-sidecar}"
WORKDIR="$(mktemp -d -t magma-apt-XXXXXX)"
trap 'rm -rf "${WORKDIR}"' EXIT

POOL_REL="pool/${COMPONENT}/$(printf '%s' "${PACKAGE}" | cut -c1)/${PACKAGE}"
echo "Staging in ${WORKDIR}"
echo "Pool path: ${POOL_REL}"

mkdir -p "${WORKDIR}/${POOL_REL}"
for f in "$@"; do
    [ -f "$f" ] || { echo "warn: '$f' not found, skipping" >&2; continue; }
    install -m 0644 "$f" "${WORKDIR}/${POOL_REL}/"
done

# Hydrate from S3 so the new index includes every previously published
# version. First publish is a no-op (sync just creates pool/).
echo "Hydrating existing pool from s3://${S3_BUCKET}/pool/ ..."
aws s3 sync "s3://${S3_BUCKET}/pool/" "${WORKDIR}/pool/" --no-progress \
    --exclude "*" --include "*.deb" || true

cd "${WORKDIR}"

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

echo ""
echo "Uploading to s3://${S3_BUCKET}/ ..."
aws s3 sync "pool/" "s3://${S3_BUCKET}/pool/" \
    --content-type "application/vnd.debian.binary-package" \
    --exclude "*" --include "*.deb"

for arch in ${ARCHES}; do
    bin_dir="dists/${SUITE}/${COMPONENT}/binary-${arch}"
    aws s3 cp "${bin_dir}/Packages"    \
        "s3://${S3_BUCKET}/${bin_dir}/Packages"    --content-type "text/plain"
    aws s3 cp "${bin_dir}/Packages.gz" \
        "s3://${S3_BUCKET}/${bin_dir}/Packages.gz" --content-type "application/gzip"
done

aws s3 cp "dists/${SUITE}/Release" \
    "s3://${S3_BUCKET}/dists/${SUITE}/Release" --content-type "text/plain"
if [ -f "dists/${SUITE}/InRelease" ]; then
    aws s3 cp "dists/${SUITE}/InRelease" \
        "s3://${S3_BUCKET}/dists/${SUITE}/InRelease" --content-type "text/plain"
fi
if [ -f "dists/${SUITE}/Release.gpg" ]; then
    aws s3 cp "dists/${SUITE}/Release.gpg" \
        "s3://${S3_BUCKET}/dists/${SUITE}/Release.gpg" \
        --content-type "application/pgp-signature"
fi

echo ""
echo "Published. Repo URL: https://${S3_BUCKET}.s3.amazonaws.com"
