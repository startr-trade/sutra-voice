# CLAUDE.md

Guidance for an AI agent working cold in this repository. [`README.md`](README.md) is the
narrative; this file is what to know before exploring, and the rules that are easy to break.

## What this repo is

**sutra-voice** — telephony and video for the Sutra workflow engine: the `voip` transport, the
WebRTC media worker, the voice / IVR / meeting / video examples and their end-to-end suites.
Open source, MIT OR Apache-2.0, same as the engine. Future home: `github.com/startr-trade/sutra-voice`.

It is **not** the engine. The engine is the `engine/` git submodule (the public Sutra
repository, `github.com/startr-trade/sutra`), consumed as path dependencies. This workspace's
members are only `sutra-transport-voip`, `sutra-voice-dist` and `sutra-voice-conformance`.

## The one thing worth knowing before you explore

**There is no BPMN implementation here.** Nodes, gateways, events, wait states, channels, `llm`
channels, the `q:` namespace, the loader — all engine. "What can a flow do?" is answered by
[`engine/rust/crates/sutra-bpmn/bpmn-support.md`](engine/rust/crates/sutra-bpmn/bpmn-support.md),
not by reading this tree. What this repo adds is one transport (registered through
`sutra-transport-spi` via `inventory::submit!`), a composition root that force-links it, and a
separate media worker.

## The submodule seam

```bash
git submodule update --init      # required before anything builds
git submodule status             # the pinned engine SHA
```

- **Never edit anything under `engine/`.** An engine change is made in the engine repository,
  lands there, and arrives here as a pin bump (a commit that moves the submodule SHA and nothing
  else). An in-place edit is a dirty submodule the next `submodule update` silently discards.
- **If this repo needs something an engine crate does not expose, make it public API in the
  engine** — never copy engine code here. The generic seams (`llm` channels with audio,
  `AudioSession`, the `llm` channel registry, `q:audit capture="media"`, `sutra-objectstore`,
  capacity pools, `session-end`) are engine features and stay there.
- **The first pin is not on GitHub yet.** It is a local commit of the public engine checkout,
  fetchable from GitHub only once that commit is pushed. Until then, fetch it from a local engine
  checkout: `git -C engine fetch <path>` (`engine/` carries such a `local` remote on the machine
  that created this repo).

## Load-bearing rules

1. **The image is `sutra-voice-engine:dev`, not `sutra-rust-engine:dev`.** The testkit's default
   image is the public engine, which has no `voip` transport; `make test-docker` exports
   `SUTRA_ENGINE_IMAGE`. Rebuild (`make image`) before trusting a tier-2 green — the tag is
   mutable and a stale one tests yesterday's binary.
2. **Zero-warning clippy is a hard gate:** `make lint` (workspace-wide, `-D warnings`). A scoped
   `cargo clippy -p …` is not the gate.
3. **The worker is its own workspace.** `tools/sutra-webrtc-worker` builds only in Docker
   (`make worker-*`); the host needs no clang or libwebrtc. Never add it to `rust/Cargo.toml`.
4. **Third-party versions track the engine's.** `rust/Cargo.toml` repeats the engine's
   requirements for shared crates so the lockfile resolves one version of each; `rust/deny.toml`
   and `rust/.cargo/audit.toml` carry the engine's accepted advisories (the graph includes the
   engine crates). When the pin moves, diff them against `engine/rust/`.
5. **Secrets are references, never literals** (`env:`, `file:`, `${…}`) — the worker and the
   transport both refuse literal keys.
6. **Nothing private belongs here.** No product, vendor or registry name of any downstream
   distribution, no references to design or plan documents that are not in this repository.
   Use fictional placeholders in examples.

## Verifying a change

```bash
make fmt-check && make lint && make test && make audit   # = CI
make scan-deps                                           # Trivy, with every change
make image && make scan-image && make test-docker        # tier 2, when the change could touch it
make worker-lint worker-test                             # when tools/ changed
```

Tier-2 suites spawn rustpbx / LiveKit / SIPp / MinIO / postgres containers; `make docker-clean`
reaps leaked ones. The default target dir is `rust/target`; on a disk-constrained machine a
shared `CARGO_TARGET_DIR` with the engine checkout is safe (path dependencies from different
trees get different package ids, so only third-party artifacts are shared).

## Where to read next

| Question | Page |
|---|---|
| The `voip` transport: channels, media protocol, gateways, `SUTRA_VOIP_*` | [`docs/voip-transport.md`](docs/voip-transport.md) |
| The WebRTC worker and meeting sessions | [`docs/webrtc-worker.md`](docs/webrtc-worker.md) |
| Each example's flow, data and run commands | `examples/<name>/README.md` |
| BPMN, channels, `llm` channels, packaging | the engine book, `https://sutra.startr.trade/` |
