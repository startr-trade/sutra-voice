//! The pure-Rust half of the WebRTC media worker — everything that does not touch libwebrtc, so
//! it builds and tests anywhere: the media-stream protocol's tagged frames and text frames
//! ([`protocol`]), secret references ([`secret`]), the per-participant video sampler
//! ([`sampler`]), the I420 → JPEG encoder ([`jpeg`]) and the scripted participant's synthetic
//! media ([`synth`]).
#![forbid(unsafe_code)]

pub mod jpeg;
pub mod protocol;
pub mod sampler;
pub mod secret;
pub mod synth;
