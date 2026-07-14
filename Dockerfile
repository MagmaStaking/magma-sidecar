# syntax=docker/dockerfile:1.7
#
# magma-sidecar: txpool IPC reprioritizer for Monad (+ /health, /metrics).
# See README.md and docs/ARCHITECTURE.md.
#
# DEVELOPMENT/TEST ONLY. This image is not an approved validator-host
# distribution. Validator production deployments must use the signed Debian
# package and its hardened systemd unit.
#
# Build:    docker build -t magma-sidecar .
# Run:      docker run --rm -p 127.0.0.1:8089:8089 magma-sidecar
#           (no txpool socket = observability-only: /health, /metrics)
#
# With txpool IPC + network policy (bind-mount the node's socket dir; pick the
# network whose gateway should be scored. Keep the in-container socket path
# short to stay under the AF_UNIX 107-byte limit, see
# docs/LOCAL_DEVELOPMENT.md §1a):
#
#   docker run --rm -p 127.0.0.1:8089:8089 \
#     -v /run/monad:/run/monad:ro \
#     -e MAGMA_TXPOOL_SOCKET=/run/monad/mempool.sock \
#     -e MAGMA_NETWORK=localnet \
#     magma-sidecar

ARG RUST_VERSION=1.91
ARG DEBIAN_RELEASE=bookworm

############################
# 1. Builder
############################
FROM rust:${RUST_VERSION}-slim-${DEBIAN_RELEASE} AS builder

# git:             fetch monad-bft crates pinned by `rev` in Cargo.toml.
# ca-certificates: HTTPS for crates.io and github.com.
# pkg-config:      harmless, satisfies any build script that probes for it.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        git \
        ca-certificates \
        pkg-config \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

# BuildKit cache mounts keep cargo's registry/git index and the target/
# directory across builds, so iterative rebuilds only recompile the sidecar
# itself (not the ~hundreds of MB of upstream monad-bft deps).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release --locked --bin magma-sidecar \
 && cp /app/target/release/magma-sidecar /usr/local/bin/magma-sidecar \
 && strip /usr/local/bin/magma-sidecar

############################
# 2. Runtime
############################
FROM debian:${DEBIAN_RELEASE}-slim AS runtime

# The sidecar makes no outbound network calls (txpool IPC is a local Unix
# socket; the HTTP server is inbound-only), so no CA bundle is needed at runtime.
RUN groupadd --system --gid 1000 magma \
 && useradd  --system --uid 1000 --gid magma \
        --home-dir /nonexistent --shell /usr/sbin/nologin magma

COPY --from=builder /usr/local/bin/magma-sidecar /usr/local/bin/magma-sidecar

USER magma:magma

# Defaults are overridable via -e at `docker run` or a compose env_file.
# Bind on the container interface so Docker can forward the explicitly
# loopback-only host publication (`-p 127.0.0.1:8089:8089`). Do not publish
# this unauthenticated observability endpoint on every host interface.
ENV MAGMA_SIDECAR_BIND=0.0.0.0:8089 \
    RUST_LOG=info,magma_sidecar=info

EXPOSE 8089

ENTRYPOINT ["/usr/local/bin/magma-sidecar"]
