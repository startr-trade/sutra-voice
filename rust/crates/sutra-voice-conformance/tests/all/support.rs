//! The engine's conformance harness (`sutra_testkit::conformance`, feature `conformance`),
//! re-exported under the `crate::support::*` paths the suites use. Nothing suite-specific lives
//! here: the harness resolves the examples through `SUTRA_EXAMPLES_DIR` (this repository's
//! `examples/`, set by `rust/.cargo/config.toml`) and the engine image through
//! `SUTRA_ENGINE_IMAGE` (`sutra-voice-engine:dev`, exported by the Makefile).
pub use sutra_testkit::conformance::{compose, engine, util};
