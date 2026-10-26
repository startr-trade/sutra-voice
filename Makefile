# sutra-voice — repo-root convenience targets.
#
# Thin wrappers over the canonical cargo/docker commands, mirroring the engine's tiering so the
# same habits carry over:
#
#   tier   target             what runs                                       needs
#   ----   ----------------   ---------------------------------------------   ------------------
#   1      make test          no docker; every container test is #[ignore]     —
#   2      make test-docker   the voice suites (rustpbx, SIPp, LiveKit, ...)   docker + make image
#
# EVERY Rust target operates on this workspace (rust/). The engine is the submodule at engine/,
# consumed as PATH DEPENDENCIES — never a member of this workspace. A fresh clone starts with
# `make submodule`. The WebRTC worker (tools/sutra-webrtc-worker) is its own workspace, built
# only in Docker by the worker-* targets.

VOICE_ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
ENGINE_ROOT ?= $(VOICE_ROOT)/engine

# The out-of-workspace harness seam (engine sutra-testkit, feature `conformance`):
#   SUTRA_REPO_ROOT    → the ENGINE tree (engine-side infra and tools).
#   SUTRA_EXAMPLES_DIR → THIS repo's examples/, the voice packages the suites assemble.
# rust/.cargo/config.toml sets the same two, so a bare `cargo test` also resolves.
export SUTRA_REPO_ROOT ?= $(ENGINE_ROOT)
export SUTRA_EXAMPLES_DIR ?= $(VOICE_ROOT)/examples

# The voice engine image: `sutra-voice-dist` → `sutra-voice-engine`, built by ./Dockerfile with
# the repo root as context. IMAGE is the local tag tier-2 boots; the test targets export it as
# SUTRA_ENGINE_IMAGE, which sutra_testkit::conformance::engine honours (its default is the
# PUBLIC engine image, which has no `voip` transport — every voice suite would fail).
IMAGE ?= sutra-voice-engine:dev

# Concurrent container-spawning tests. The voice suites each hold a PBX/SFU and several
# far-end containers; 2 keeps their real-time audio paths unstarved on a dev box.
TEST_THREADS ?= 2

.PHONY: help submodule build test test-docker lint fmt fmt-check audit scan-deps image \
        image-release scan-image codeql docker-clean \
        worker-image worker-toolchain worker-lint worker-test worker-scan

help: ## Show this help.
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

submodule: ## Check out the pinned engine submodule (first thing after a fresh clone).
	git submodule update --init --recursive

build: ## Build the workspace (engine crates come in as path deps).
	cd rust && cargo build --workspace

test: ## Tier-1: the no-docker suite.
	cd rust && cargo test --workspace

## Tier-2: the docker suites. `make test-docker P=<crate>` narrows it to one crate. Run
## `make image` first — `sutra-voice-engine:dev` is a mutable tag, and a stale one silently
## tests yesterday's binary. The video suite builds `sutra-webrtc-worker:dev` itself
## (`make worker-image`) when it is missing.
test-docker: export SUTRA_ENGINE_IMAGE = $(IMAGE)
test-docker: ## Tier-2: the voice docker suites (P=<crate> narrows; TEST_THREADS=<n>).
ifdef P
	cd rust && cargo test -p $(P) --no-fail-fast -- --ignored --test-threads=$(TEST_THREADS)
else
	cd rust && cargo test --workspace --no-fail-fast -- --ignored --test-threads=$(TEST_THREADS)
endif

lint: ## clippy -D warnings over the workspace.
	cd rust && cargo clippy --workspace --all-targets -- -D warnings

fmt: ## rustfmt the workspace.
	cd rust && cargo fmt --all

fmt-check: ## rustfmt in check mode (CI gate).
	cd rust && cargo fmt --all --check

## cargo-audit + cargo-deny over rust/Cargo.lock (the graph includes the engine crates).
## Install once: cargo install cargo-audit cargo-deny --locked
audit: ## Supply-chain gate: cargo-audit + cargo-deny (licenses/bans/sources/advisories).
	cd rust && cargo audit
	cd rust && cargo deny check

## Security scans, the same scanners the engine runs (scripts/security-scan.sh). Each FAILS on
## any finding; a finding with no reachable fix goes in .trivyignore.yaml, time-boxed.
scan-deps: ## Trivy over the source tree (lockfiles, secrets); fast, run with every change.
	scripts/security-scan.sh deps

scan-image: ## Trivy over the voice engine image (IMAGE=; `make image` first).
	scripts/security-scan.sh image $(IMAGE)

codeql: ## CodeQL for Rust over this repository; several minutes.
	scripts/security-scan.sh codeql

image: ## Build the local tier-2 voice engine image (IMAGE=, default sutra-voice-engine:dev; fast `release-it`).
	DOCKER_BUILDKIT=1 docker build --build-arg CARGO_PROFILE=release-it -t $(IMAGE) $(VOICE_ROOT)

image-release: ## Build the SHIPPED-profile voice engine image (full thin-LTO; slow).
	DOCKER_BUILDKIT=1 docker build -t sutra-voice-engine:release $(VOICE_ROOT)

docker-clean: ## Reap leaked test containers (the engine's scripts/dev-docker-cleanup.sh; CUTOFF=<min>).
	bash $(ENGINE_ROOT)/scripts/dev-docker-cleanup.sh $(CUTOFF)

## The WebRTC media worker (tools/sutra-webrtc-worker): its OWN Cargo workspace, outside rust/,
## so `make lint` / `make test` never need what it needs — libwebrtc and a clang 21 toolchain.
## Everything that compiles it runs in the Dockerfile's `toolchain` stage; cargo deny / audit read
## only its lockfile, so they run on the host like `make audit`. Cargo's registry and target
## directory live in named volumes, so a re-run is incremental.
WORKER_DIR := tools/sutra-webrtc-worker
WORKER_IMAGE ?= sutra-webrtc-worker:dev
WORKER_TOOLCHAIN := sutra-webrtc-worker-toolchain:dev
WORKER_RUN = docker run --rm -v $(VOICE_ROOT)/$(WORKER_DIR):/src:ro \
	-v sutra-webrtc-worker-registry:/usr/local/cargo/registry \
	-v sutra-webrtc-worker-target:/target -e CARGO_TARGET_DIR=/target $(WORKER_TOOLCHAIN)

worker-image: ## Build the WebRTC media worker image (WORKER_IMAGE=, default sutra-webrtc-worker:dev).
	docker build -t $(WORKER_IMAGE) $(VOICE_ROOT)/$(WORKER_DIR)

worker-toolchain:
	docker build -q --target toolchain -t $(WORKER_TOOLCHAIN) $(VOICE_ROOT)/$(WORKER_DIR) >/dev/null

worker-lint: worker-toolchain ## Worker: fmt --check + clippy -D warnings (in Docker), cargo deny + audit (lockfile).
	$(WORKER_RUN) sh -c 'cargo fmt --all --check && cargo clippy --locked --workspace --all-targets -- -D warnings'
	cd $(WORKER_DIR) && cargo deny check && cargo audit

worker-test: worker-toolchain ## Worker: the unit tests (in Docker).
	$(WORKER_RUN) cargo test --locked --workspace

worker-scan: ## Trivy over the worker image (`make worker-image` first).
	scripts/security-scan.sh image $(WORKER_IMAGE)
