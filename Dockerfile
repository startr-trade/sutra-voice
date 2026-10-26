# syntax=docker/dockerfile:1
# sutra-voice-engine — the Sutra engine with the `voip` transport linked in.
#
# Build context = THIS repository's root, because the build needs `rust/` (this workspace) AND
# `engine/rust/` (the submodule its path dependencies point into) side by side:
#
#   docker build -t sutra-voice-engine:dev .          (or: make image)
#
# The stages mirror the engine's rust/Dockerfile: cargo-chef dependency caching
# (chef → planner → builder) and a distroless runtime. The layout inside the builder keeps the
# relative path `rust/../engine/rust` intact (/build/rust and /build/engine/rust), so the
# workspace's `../engine/rust/crates/*` path dependencies resolve unchanged.
#
# Run it exactly like the engine image — the same `SUTRA_*` environment (SUTRA_DEPLOYMENTS_DIR,
# SUTRA_HTTP_PORT, SUTRA_DATASOURCE_*), plus the `SUTRA_VOIP_*` settings of the voip transport
# (docs/src/guides/voip-transport.md).

# ---- chef (shared toolchain layer) -------------------------------------------
# The bookworm variant, so the builder's glibc matches the runtime image below. make + g++ are
# for the Kafka transport's vendored librdkafka build (see the engine's rust/Dockerfile).
FROM rust:1.96-slim-bookworm AS chef
RUN apt-get update \
    && apt-get install -y --no-install-recommends make g++ \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /build/rust

# ---- planner: compute the dependency recipe ----------------------------------
FROM chef AS planner
COPY engine/rust /build/engine/rust
COPY rust/Cargo.toml rust/Cargo.lock ./
COPY rust/crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

# ---- builder: cook dependencies (cached layer), then build the distribution ---
FROM chef AS builder
# CARGO_PROFILE: `release` (default — the shipped profile) or `release-it` (fast dev/IT image).
ARG CARGO_PROFILE=release
# The engine sources are path dependencies, so they are needed to cook as well as to build.
COPY engine/rust /build/engine/rust
COPY --from=planner /build/rust/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/rust/target \
    cargo chef cook --profile ${CARGO_PROFILE} --recipe-path recipe.json
COPY rust/Cargo.toml rust/Cargo.lock ./
COPY rust/crates ./crates
# The target dir is a cache mount (not part of the image), so copy the finished binary OUT of it.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/rust/target \
    cargo build --profile ${CARGO_PROFILE} -p sutra-voice-dist \
    && cp target/${CARGO_PROFILE}/sutra-voice-engine /build/sutra-voice-engine

# ---- runtime (distroless) ----------------------------------------------------
# The same base as the engine image, pinned by the same digest: glibc + libgcc + libstdc++ + CA
# certificates, no shell, no package manager. Refresh it when the engine refreshes its own.
FROM gcr.io/distroless/cc-debian12@sha256:e8e7ee4b8b106d4c5fde9e422a321b2b8a2d5cca546c97adcce927f3e1d36e36
COPY --from=builder /build/sutra-voice-engine /usr/local/bin/sutra-voice-engine

# The engine-internal migrations, out of the submodule (the engine image ships the same tree).
COPY engine/rust/crates/sutra-persistence/migrations /opt/sutra/db/migration
ENV SUTRA_DB_MIGRATIONS=/opt/sutra/db/migration

# Container-internal default port (the host side is mapped dynamically by the test harness).
ENV SUTRA_HTTP_PORT=8080
EXPOSE 8080

# Non-root (k8s restricted PSS friendly).
USER 10001

# Docker-level health: the distroless base has no shell or curl, so the probe is a mode of the
# binary itself (`--health-check` GETs its own /sutra/health/ready and exits 0/1).
HEALTHCHECK --interval=30s --timeout=5s --start-period=40s --retries=3 \
    CMD ["/usr/local/bin/sutra-voice-engine", "--health-check"]

ENTRYPOINT ["/usr/local/bin/sutra-voice-engine"]
