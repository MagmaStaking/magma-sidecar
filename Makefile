# magma-sidecar developer Makefile.
#
# Thin convenience layer over `cargo`, `docker`, and `debian/sidecar/build-deb.sh`.
# CI does not depend on these targets — they exist for local iteration.
#
# Common usage:
#
#   make                       # cargo build --release
#   make test                  # cargo test --all-targets --locked
#   make build-deb             # native-arch .deb in build/
#   make build-deb-arm64       # cross-build via `cross`
#   make docker-build          # local single-arch docker image (tag: magma-sidecar:dev)
#   make install               # dpkg -i the most recent .deb in build/
#   make clean                 # rm -rf build/ + cargo clean
#
# Override VERSION at invocation: `make build-deb VERSION=1.2.3`. Default
# pulls from git (matches CI's `0~dev.<sha>` convention for non-tagged HEADs).

SHELL          := /usr/bin/env bash
.SHELLFLAGS    := -eu -o pipefail -c
.DEFAULT_GOAL  := build

# Version detection: prefer the current annotated tag, fall back to a Debian
# pre-release marker so dev .debs always compare lower than any real release.
VERSION ?= $(shell \
    git describe --tags --exact-match 2>/dev/null \
        | sed -e 's/^v//' \
    || printf '0~dev.%s' "$$(git rev-parse --short HEAD 2>/dev/null || echo unknown)" \
)

HOST_ARCH      := $(shell dpkg --print-architecture 2>/dev/null || echo amd64)
ARCH           ?= $(HOST_ARCH)
DOCKER_IMAGE   ?= magma-sidecar
DOCKER_TAG     ?= dev
PACKAGE        := magma-sidecar
BUILD_DIR      := build

# Resolve the most recently built .deb for `make install` / `uninstall`.
LATEST_DEB     = $(shell ls -1t $(BUILD_DIR)/$(PACKAGE)_*_$(ARCH).deb 2>/dev/null | head -n1)

.PHONY: help build release test fmt fmt-check lint check \
        build-deb build-deb-amd64 build-deb-arm64 \
        docker-build docker-run \
        install uninstall purge \
        service-start service-stop service-restart service-status service-logs \
        clean distclean print-version

help: ## Show available targets
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z][a-zA-Z0-9_-]*:.*?## / {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# ----- Cargo --------------------------------------------------------------

build: release ## Build the release binary (alias for `release`)

release: ## cargo build --release --locked
	cargo build --release --locked --bin magma-sidecar

test: ## cargo test --all-targets --locked
	cargo test --all-targets --locked

fmt: ## cargo fmt --all
	cargo fmt --all

fmt-check: ## cargo fmt --all -- --check (matches CI)
	cargo fmt --all -- --check

lint: ## cargo clippy --all-targets -- -D warnings
	cargo clippy --all-targets --locked -- -D warnings

check: fmt-check lint test ## Run everything CI runs (fmt + clippy + test)

# ----- Debian package -----------------------------------------------------

build-deb: ## Build a .deb for the host arch (override with ARCH=arm64)
	./debian/sidecar/build-deb.sh "$(VERSION)" "$(ARCH)"

build-deb-amd64: ## Build an amd64 .deb (cross via `cross` on arm hosts)
	$(MAKE) build-deb ARCH=amd64

build-deb-arm64: ## Build an arm64 .deb (cross via `cross` on amd64 hosts)
	$(MAKE) build-deb ARCH=arm64

# ----- Docker -------------------------------------------------------------

docker-build: ## Build a single-arch local docker image (tag: $(DOCKER_IMAGE):$(DOCKER_TAG))
	docker build -t $(DOCKER_IMAGE):$(DOCKER_TAG) .

docker-run: ## Run the locally-built image in ingress-only mode against host RPC
	docker run --rm -p 8089:8089 \
	    -e MAGMA_MONAD_RPC_URL=http://host.docker.internal:8545 \
	    $(DOCKER_IMAGE):$(DOCKER_TAG)

# ----- Local install / service helpers ------------------------------------

install: ## sudo dpkg -i the most recent .deb in build/ (run `make build-deb` first)
	@test -n "$(LATEST_DEB)" \
	    || (echo "No .deb found in $(BUILD_DIR)/ for ARCH=$(ARCH). Run 'make build-deb' first." >&2; exit 1)
	@echo "Installing $(LATEST_DEB)"
	sudo dpkg -i "$(LATEST_DEB)"

uninstall: ## sudo dpkg --remove magma-sidecar (keeps /etc/magma-sidecar/sidecar.env)
	sudo dpkg --remove $(PACKAGE)

purge: ## sudo dpkg --purge magma-sidecar (also wipes /etc/magma-sidecar/sidecar.env)
	sudo dpkg --purge $(PACKAGE)

service-start: ## systemctl start magma-sidecar
	sudo systemctl start $(PACKAGE)

service-stop: ## systemctl stop magma-sidecar
	sudo systemctl stop $(PACKAGE)

service-restart: ## systemctl restart magma-sidecar
	sudo systemctl restart $(PACKAGE)

service-status: ## systemctl status magma-sidecar
	systemctl status $(PACKAGE) --no-pager

service-logs: ## journalctl -u magma-sidecar -f
	journalctl -u $(PACKAGE) -f

# ----- Housekeeping -------------------------------------------------------

clean: ## rm -rf $(BUILD_DIR) (keeps target/ and cargo state)
	rm -rf $(BUILD_DIR)

distclean: clean ## clean + cargo clean (full rebuild on next make)
	cargo clean

print-version: ## Echo the resolved VERSION (useful for CI debugging)
	@echo "$(VERSION)"
