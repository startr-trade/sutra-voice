//! Consolidated conformance-test binary for the voice distribution.
//!
//! Each suite is a module under `tests/all/`, referenced here via an explicit `#[path]` so cargo
//! compiles ONE test binary. Tier markers: every tier-2 test fn name starts `tc_` and carries
//! `#[ignore = "docker"]`; tier-1 tests (variant lint) carry no marker.

// Force-link the schema-less formats so their `inventory::submit!` registrations are present in
// THIS test binary: `sutra_loader`'s `assemble_dir` / `lint_dir` lint each example against
// `sutra_codec_spi::builtin_codecs()` / `builtin_formats()`, and a package binding a format the
// binary did not link fails CODEC_NOT_FOUND.
use sutra_formats as _;

#[path = "all/support.rs"]
mod support;

#[path = "all/analysis_flows.rs"]
mod analysis_flows;
#[path = "all/tc_ivr_dtmf.rs"]
mod tc_ivr_dtmf;
#[path = "all/tc_meeting_assistant.rs"]
mod tc_meeting_assistant;
#[path = "all/tc_video_meeting_assistant.rs"]
mod tc_video_meeting_assistant;
#[path = "all/tc_voice_assistant.rs"]
mod tc_voice_assistant;
