# syntax=docker/dockerfile:1.7
#
# The WebRTC media worker image: `sutra-webrtc-worker` (the meeting bot) and
# `sutra-webrtc-participant` (a scripted participant for tests and demos).
#
#   docker build -t sutra-webrtc-worker:dev tools/sutra-webrtc-worker      (or: make worker-image)
#
# Why a builder this heavy: the LiveKit SDK links libwebrtc, a prebuilt static C++ library
# compiled against its own (hermetic) libc++, and webrtc-sys compiles its C++ glue against those
# headers — which need clang 21.1 or newer (the floor is read from the artifact's
# __configuration/compiler.h). Debian ships clang 14, so clang 21 comes from apt.llvm.org. glib
# headers are needed at build time only; the X11 / DRM / VA-API libraries libwebrtc can use are
# loaded lazily and only for screen capture and hardware codecs, which the worker never uses.
#
# libwebrtc (156 MB for amd64, 172 MB for arm64) is fetched once, checked against its SHA-256,
# and handed to the build with LK_CUSTOM_WEBRTC, so the build never downloads it again; cargo's
# registry and target directory are BuildKit cache mounts. The image builds for linux/amd64 and
# linux/arm64; the release builds each natively on a runner of that architecture.

ARG RUST_IMAGE=rust:1.96-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663
# The engine's own runtime base: the binaries link only libc, libm and libgcc_s, which is what
# distroless/cc carries (plus CA certificates and a non-root user).
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12@sha256:e8e7ee4b8b106d4c5fde9e422a321b2b8a2d5cca546c97adcce927f3e1d36e36

FROM ${RUST_IMAGE} AS toolchain
ARG LLVM_VERSION=21
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gnupg unzip pkg-config libglib2.0-dev \
 && curl -fsSL https://apt.llvm.org/llvm-snapshot.gpg.key | gpg --dearmor -o /usr/share/keyrings/llvm.gpg \
 && echo "deb [signed-by=/usr/share/keyrings/llvm.gpg] http://apt.llvm.org/bookworm/ llvm-toolchain-bookworm-${LLVM_VERSION} main" \
      > /etc/apt/sources.list.d/llvm.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends clang-${LLVM_VERSION} lld-${LLVM_VERSION} \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add rustfmt clippy
# The prebuilt libwebrtc this SDK release expects (webrtc-sys-build WEBRTC_TAG = webrtc-a545de4),
# for the architecture this stage runs on — the build is native, never a cross-compile, so the
# toolchain image's own Debian architecture names it. Each archive is checked against its SHA-256.
ARG WEBRTC_RELEASE=https://github.com/livekit/rust-sdks/releases/download/webrtc-a545de4
RUN set -eu; \
    case "$(dpkg --print-architecture)" in \
      amd64) arch=x64;   sum=2ece73b9a72a2cacd73d0c62ac3a414e202f37885c3c8f49dddb43c1443ac0bd ;; \
      arm64) arch=arm64; sum=e87dff71422c5cb1470f6cf00ecc4ad6964e746c5039831b7206b946b5d13c5f ;; \
      *) echo "no prebuilt libwebrtc for $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/webrtc.zip "${WEBRTC_RELEASE}/webrtc-linux-${arch}-release.zip"; \
    echo "${sum}  /tmp/webrtc.zip" | sha256sum -c -; \
    mkdir -p /opt/webrtc; unzip -q /tmp/webrtc.zip -d /opt/webrtc; rm /tmp/webrtc.zip; \
    ln -s "/opt/webrtc/linux-${arch}-release" /opt/webrtc/current; \
    test -d /opt/webrtc/current/include
ENV CC=clang-${LLVM_VERSION} \
    CXX=clang++-${LLVM_VERSION} \
    LK_CUSTOM_WEBRTC=/opt/webrtc/current \
    CARGO_TERM_COLOR=never
WORKDIR /src

FROM toolchain AS builder
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,id=sutra-webrtc-worker-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=sutra-webrtc-worker-target,target=/src/target \
    cargo build --release --locked \
 && mkdir -p /out \
 && cp target/release/sutra-webrtc-worker target/release/sutra-webrtc-participant /out/

FROM ${RUNTIME_IMAGE} AS runtime
COPY --from=builder /out/sutra-webrtc-worker /out/sutra-webrtc-participant /usr/local/bin/
USER nonroot:nonroot
ENV RUST_LOG=info
ENTRYPOINT ["/usr/local/bin/sutra-webrtc-worker"]
