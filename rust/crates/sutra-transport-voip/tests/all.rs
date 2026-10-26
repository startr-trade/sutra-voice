//! The sutra-transport-voip suite (tier 1: no docker; `recording_minio` is tier 2,
//! `#[ignore = "docker"]`).
mod all {
    pub mod support;

    mod audio_codecs;
    mod audit_events;
    mod binding_precedence;
    mod features_synthetic;
    mod listen_calls;
    mod media_auth;
    mod media_ws;
    mod meeting_sessions;
    mod originate_policy;
    mod pacing_realtime;
    mod prompts;
    mod protocol_frames;
    mod record_consent;
    mod recording_durable;
    mod recording_minio;
    mod rwi_client;
    mod vad_turns;
}
