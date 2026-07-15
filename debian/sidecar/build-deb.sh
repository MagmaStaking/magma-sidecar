#!/usr/bin/env bash
#
# Build a Debian package for magma-sidecar.
#
# Usage:
#   ./debian/sidecar/build-deb.sh <version> [arch]
#
#   version: package version, e.g. 1.0.0 or 0~dev.abc1234
#   arch:    amd64 | arm64. Defaults to the host's dpkg arch.
#
# When arch matches the host, we build natively. When it differs, we attempt a
# cross build via `cross` (rust target must be installable). CI publishes both
# arches by running this script on the matching native runner (ubuntu-24.04
# for amd64, ubuntu-24.04-arm for arm64) — see .github/workflows/build-and-publish.yml.

set -euo pipefail
umask 022

if [ $# -lt 1 ]; then
    echo "Error: VERSION argument is required" >&2
    echo "Usage: $0 <version> [arch]" >&2
    exit 1
fi

PACKAGE_NAME="magma-sidecar"
VERSION="$1"
ARCH="${2:-$(dpkg --print-architecture 2>/dev/null || echo amd64)}"
BUILD_DIR="build"
DEB_DIR="${BUILD_DIR}/${ARCH}/debian"

case "${ARCH}" in
    amd64) RUST_TARGET="x86_64-unknown-linux-gnu" ;;
    arm64) RUST_TARGET="aarch64-unknown-linux-gnu" ;;
    *)
        echo "Error: unsupported arch '${ARCH}'. Use amd64 or arm64." >&2
        exit 1
        ;;
esac

HOST_ARCH="$(dpkg --print-architecture 2>/dev/null || echo amd64)"

echo "Building ${PACKAGE_NAME} v${VERSION} for ${ARCH} (rust target ${RUST_TARGET})"

rm -rf "${BUILD_DIR}/${ARCH}"
mkdir -p "${DEB_DIR}/DEBIAN" \
         "${DEB_DIR}/usr/bin" \
         "${DEB_DIR}/usr/lib/magma-sidecar" \
         "${DEB_DIR}/usr/lib/sysusers.d" \
         "${DEB_DIR}/usr/share/doc/magma-sidecar" \
         "${DEB_DIR}/lib/systemd/system" \
         "${DEB_DIR}/etc/magma-sidecar"

# Make sure the requested rust target is installed locally; harmless no-op if it
# already is. Cross-arch builds also need `cross` (cargo-zigbuild would work too;
# we pick `cross` because it's the standard CI helper and has prebuilt images).
rustup target add "${RUST_TARGET}" >/dev/null 2>&1 || true

if [ "${ARCH}" = "${HOST_ARCH}" ]; then
    echo "Native build (host arch == target arch == ${ARCH})"
    cargo build --release --locked --target "${RUST_TARGET}" --bin magma-sidecar
else
    echo "Cross build (host=${HOST_ARCH} -> target=${ARCH}) via 'cross'"
    if ! command -v cross >/dev/null 2>&1; then
        echo "Error: 'cross' is required for cross-arch builds." >&2
        echo "Install with: cargo install cross --locked" >&2
        exit 1
    fi
    cross build --release --locked --target "${RUST_TARGET}" --bin magma-sidecar
fi

BIN_SRC="target/${RUST_TARGET}/release/magma-sidecar"
if [ ! -f "${BIN_SRC}" ]; then
    echo "Error: expected binary not found at ${BIN_SRC}" >&2
    exit 1
fi

install -m 0755 "${BIN_SRC}" "${DEB_DIR}/usr/bin/magma-sidecar"
# strip drops debug info; skip if the host strip can't handle a foreign ELF.
strip "${DEB_DIR}/usr/bin/magma-sidecar" 2>/dev/null || \
    echo "warn: strip failed (foreign ELF?); shipping unstripped binary"

install -m 0644 debian/sidecar/magma-sidecar.service \
    "${DEB_DIR}/lib/systemd/system/magma-sidecar.service"
install -m 0644 debian/sidecar/magma-sidecar.sysusers \
    "${DEB_DIR}/usr/lib/sysusers.d/magma-sidecar.conf"
install -m 0755 debian/sidecar/monad-ipc-setup \
    "${DEB_DIR}/usr/lib/magma-sidecar/monad-ipc-setup"
install -m 0644 README.md docs/VALIDATOR_INSTALL.md docs/RELEASE_VERIFICATION.md \
    "${DEB_DIR}/usr/share/doc/magma-sidecar/"

# Ship the env template as `.example` so postinst can seed the real
# /etc/magma-sidecar/sidecar.env on first install without ever clobbering an
# operator-edited file on upgrade.
install -m 0644 .env.example "${DEB_DIR}/etc/magma-sidecar/sidecar.env.example"

# Substitute Version and Architecture into the shipped control template.
install -m 0644 debian/sidecar/control "${DEB_DIR}/DEBIAN/control"
sed -i "s/^Version: .*/Version: ${VERSION}/" "${DEB_DIR}/DEBIAN/control"
sed -i "s/^Architecture: .*/Architecture: ${ARCH}/" "${DEB_DIR}/DEBIAN/control"

install -m 0755 debian/sidecar/postinst "${DEB_DIR}/DEBIAN/postinst"
install -m 0755 debian/sidecar/prerm    "${DEB_DIR}/DEBIAN/prerm"
install -m 0755 debian/sidecar/postrm   "${DEB_DIR}/DEBIAN/postrm"

# `conffiles` tells dpkg the shipped `.example` template is operator-editable.
# /etc/magma-sidecar/sidecar.env (created by postinst on first install) is
# tracked by the postinst's seed-once-then-skip logic, not by dpkg.
cat > "${DEB_DIR}/DEBIAN/conffiles" <<EOF
/etc/magma-sidecar/sidecar.env.example
EOF

DEB_OUT="${BUILD_DIR}/${PACKAGE_NAME}_${VERSION}_${ARCH}.deb"
echo "Building .deb -> ${DEB_OUT}"
dpkg-deb --root-owner-group --build "${DEB_DIR}" "${DEB_OUT}"

echo ""
echo "Package built: ${DEB_OUT}"
dpkg-deb --info "${DEB_OUT}"
echo ""
echo "Contents:"
dpkg-deb --contents "${DEB_OUT}"
