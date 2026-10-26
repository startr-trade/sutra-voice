//! sutra-voice conformance — the end-to-end gate of the voice distribution.
//!
//! This library is intentionally empty. Every gate lives under `tests/all/`: the tier-2 `tc_*`
//! suites boot the `sutra-voice-engine:dev` image (`make image`) via testcontainers next to a
//! media gateway (rustpbx or LiveKit), a scripted far end (SIPp or the WebRTC test participant),
//! the stub model server and an object store, and drive each example end to end; the tier-1
//! tests lint every example variant. The harness is the engine's `sutra_testkit::conformance`.
//!
//! Running the gate:
//! - tier-1:          `cargo test -p sutra-voice-conformance`
//! - tier-2 (docker): `make test-docker` (or `cargo test -p sutra-voice-conformance -- --ignored tc_`)
