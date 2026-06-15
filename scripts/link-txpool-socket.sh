#!/usr/bin/env bash
#
# Point the short txpool-IPC symlink at the *current* monad-bft single-node run.
#
# Every `nets/run.sh` in monad-bft mints a brand-new per-run socket at
#   docker/single-node/logs/<timestamp>-<hash>/node/mempool.sock
# owned by root:root mode 0755, and that long path is one byte over the AF_UNIX
# `sun_path` limit (107 usable bytes). Both facts make the sidecar's connect(2)
# fail with a "txpool IPC connect failed; retrying" loop. This script fixes both
# in one shot, every fresh run:
#   1. chmod 666 the live socket so your host user may connect.
#   2. (re)point a short symlink (default /tmp/monad-mempool.sock) at it.
# See docs/LOCAL_DEVELOPMENT.md §1a.
#
# Usage (from anywhere):
#   scripts/link-txpool-socket.sh
#
# Overrides (env vars):
#   MONAD_BFT_DIR   monad-bft repo root           (default: sibling ../monad-bft)
#   LINK_PATH       short symlink to create        (default: /tmp/monad-mempool.sock)
#
# chmod + symlink replacement need root; the script calls `sudo` for just those
# steps (no-op if you are already root). Everything else runs unprivileged.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

MONAD_BFT_DIR="${MONAD_BFT_DIR:-$ROOT/../monad-bft}"
LINK_PATH="${LINK_PATH:-/tmp/monad-mempool.sock}"

if [[ ! -d "$MONAD_BFT_DIR" ]]; then
  echo "monad-bft repo not found at: $MONAD_BFT_DIR" >&2
  echo "Set MONAD_BFT_DIR=/path/to/monad-bft and re-run." >&2
  exit 1
fi

LOGS_DIR="$MONAD_BFT_DIR/docker/single-node/logs"
if [[ ! -d "$LOGS_DIR" ]]; then
  echo "No single-node logs dir at: $LOGS_DIR" >&2
  echo "Start the node first: cd $MONAD_BFT_DIR/docker/single-node && nets/run.sh --use-prebuilt" >&2
  exit 1
fi

# Newest per-run socket on disk. `ls -td` orders by mtime so head -1 is the latest run.
SOCK="$(ls -td "$LOGS_DIR"/*/node/mempool.sock 2>/dev/null | head -n1 || true)"
if [[ -z "$SOCK" ]]; then
  echo "No mempool.sock found under $LOGS_DIR/*/node/" >&2
  echo "Is the node up? cd $MONAD_BFT_DIR/docker/single-node && nets/run.sh --use-prebuilt" >&2
  exit 1
fi

# Soft sanity check: warn if the newest socket on disk isn't from the running container.
# The run id is the parent-of-parent directory name (e.g. 20260612_152453-15ce4430ca38f44e).
RUN_ID="$(basename "$(dirname "$(dirname "$SOCK")")")"
if command -v docker >/dev/null 2>&1; then
  if ! docker ps --format '{{.Names}}' 2>/dev/null | grep -q "^${RUN_ID}-monad_node"; then
    echo "WARNING: newest socket on disk is run '$RUN_ID', but no matching monad_node container is running." >&2
    echo "         The node may be down or mid-restart; linking anyway." >&2
  fi
fi

SUDO=""
if [[ "$(id -u)" -ne 0 ]]; then
  SUDO="sudo"
fi

echo "Linking txpool socket:"
echo "  run      : $RUN_ID"
echo "  socket   : $SOCK"
echo "  link     : $LINK_PATH"

# 1. Make the socket connectable by the host user (it's created root:root 0755).
$SUDO chmod 666 "$SOCK"

# 2. Replace the symlink atomically (-n so we don't drop it *inside* a stale target dir).
$SUDO ln -sfn "$SOCK" "$LINK_PATH"

echo "Done. Verify:"
ls -lL "$LINK_PATH"
