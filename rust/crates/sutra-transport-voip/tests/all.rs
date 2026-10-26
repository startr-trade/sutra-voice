//! The sutra-transport-voip suite (tier 1: no docker).
mod all {
    pub mod support;

    mod audio_codecs;
    mod binding_precedence;
    mod features_synthetic;
    mod listen_calls;
    mod media_ws;
    mod meeting_sessions;
    mod pacing_realtime;
    mod prompts;
    mod protocol_frames;
    mod rwi_client;
    mod vad_turns;
}
