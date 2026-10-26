//! The media-stream protocol as the worker speaks it: the generic protocol of the engine's voip
//! transport (`start` / `stop` / `mark`, binary audio both ways) extended for a multi-party
//! session (`"streams": "tagged"`):
//!
//! - worker → engine text: `participant-joined`, `participant-left`, `track`;
//! - worker → engine binary: a [`FrameHeader`] then the payload — PCM16 LE mono audio at the
//!   session rate, or one JPEG image;
//! - engine → worker text: `policy` (the video sampling the engine wants), `sample` (an extra
//!   frame now), plus the generic `clear` / `mark` / `hangup`; binary = audio for the bot to
//!   speak into the room.

use serde_json::{json, Value};

/// The tagged frame header's length.
pub const HEADER_LEN: usize = 12;
/// `kind`: PCM16 little-endian mono audio at the session's rate.
pub const KIND_AUDIO: u8 = 1;
/// `kind`: one image (JPEG).
pub const KIND_VIDEO: u8 = 2;
/// Video flag: this frame answers an engine `sample` request.
pub const FLAG_REQUESTED: u8 = 0x01;

/// The 12-byte header of a tagged binary frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub kind: u8,
    pub flags: u8,
    /// The participant's index in the session (from its `participant-joined`).
    pub index: u16,
    /// Capture time, milliseconds since the session's start.
    pub at_ms: u64,
}

impl FrameHeader {
    pub fn encode(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.push(self.kind);
        out.push(self.flags);
        out.extend_from_slice(&self.index.to_be_bytes());
        out.extend_from_slice(&self.at_ms.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Split a tagged frame into its header and payload.
    pub fn decode(frame: &[u8]) -> Option<(FrameHeader, &[u8])> {
        if frame.len() < HEADER_LEN {
            return None;
        }
        let header = FrameHeader {
            kind: frame[0],
            flags: frame[1],
            index: u16::from_be_bytes([frame[2], frame[3]]),
            at_ms: u64::from_be_bytes(frame[4..12].try_into().ok()?),
        };
        Some((header, &frame[HEADER_LEN..]))
    }
}

/// PCM16 samples as little-endian bytes.
pub fn pcm16_le(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Little-endian bytes as PCM16 samples (a trailing odd byte is dropped).
pub fn pcm16_from_le(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

/// What a session's `start` frame says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartInfo {
    pub call_id: String,
    pub stream_id: String,
    pub room: String,
    pub called: String,
    pub rate: u32,
}

pub fn start(info: &StartInfo) -> String {
    json!({
        "event": "start",
        "callId": info.call_id,
        "streamId": info.stream_id,
        "format": format!("audio/L16;rate={}", info.rate),
        "from": format!("livekit:{}", info.room),
        "to": info.called,
        "direction": "inbound",
        "headers": {"x-room": info.room, "x-media-gateway": "livekit"},
        "streams": "tagged",
        "video": {"formats": ["image/jpeg"]},
    })
    .to_string()
}

pub fn participant_joined(
    participant_id: &str,
    index: u16,
    label: &str,
    role: Option<&str>,
    at_ms: u64,
) -> String {
    json!({
        "event": "participant-joined",
        "participantId": participant_id,
        "index": index,
        "label": label,
        "role": role,
        "atMs": at_ms,
    })
    .to_string()
}

pub fn participant_left(participant_id: &str, reason: &str, at_ms: u64) -> String {
    json!({
        "event": "participant-left",
        "participantId": participant_id,
        "reason": reason,
        "atMs": at_ms,
    })
    .to_string()
}

pub fn track(participant_id: &str, kind: &str, state: &str, at_ms: u64) -> String {
    json!({
        "event": "track",
        "participantId": participant_id,
        "kind": kind,
        "state": state,
        "atMs": at_ms,
    })
    .to_string()
}

pub fn mark(name: &str) -> String {
    json!({"event": "mark", "name": name}).to_string()
}

pub fn stop(reason: &str) -> String {
    json!({"event": "stop", "reason": reason}).to_string()
}

/// The label a participant is announced with: its display name, else `Participant <index>`.
pub fn label_of(display_name: &str, index: u16) -> String {
    let trimmed = display_name.trim();
    if trimmed.is_empty() {
        format!("Participant {index}")
    } else {
        trimmed.chars().take(80).collect()
    }
}

/// The video sampling the engine asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoPolicy {
    pub enabled: bool,
    pub fps: f64,
    pub max_width: u32,
    pub quality: u8,
}

impl Default for VideoPolicy {
    /// Until the engine says otherwise: no video.
    fn default() -> Self {
        VideoPolicy {
            enabled: false,
            fps: 1.0,
            max_width: 640,
            quality: 70,
        }
    }
}

/// A text frame from the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineFrame {
    Policy(VideoPolicy),
    Sample {
        participant_id: Option<String>,
        reason: String,
    },
    Clear,
    Mark(String),
    Hangup,
    /// Anything else (ignored, for forward compatibility).
    Other(String),
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Parse one engine text frame.
pub fn parse_engine_text(raw: &str) -> Result<EngineFrame, String> {
    let value: Value = serde_json::from_str(raw).map_err(|e| format!("not JSON: {e}"))?;
    let event = text(&value, "event").ok_or("frame has no 'event'")?;
    Ok(match event.as_str() {
        "policy" => {
            let video = value.get("video").cloned().unwrap_or(Value::Null);
            let defaults = VideoPolicy::default();
            let fps = video
                .get("fps")
                .and_then(Value::as_f64)
                .filter(|f| f.is_finite() && *f >= 0.0)
                .unwrap_or(defaults.fps)
                .min(30.0);
            let format = text(&video, "format").unwrap_or_else(|| "image/jpeg".to_string());
            EngineFrame::Policy(VideoPolicy {
                // Only JPEG is produced; a policy asking for another format gets none.
                enabled: video
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(!video.is_null())
                    && format.eq_ignore_ascii_case("image/jpeg")
                    && fps > 0.0,
                fps,
                max_width: video
                    .get("maxWidth")
                    .and_then(Value::as_u64)
                    .map(|w| w.clamp(16, 4096) as u32)
                    .unwrap_or(defaults.max_width),
                quality: video
                    .get("quality")
                    .and_then(Value::as_u64)
                    .map(|q| q.clamp(1, 100) as u8)
                    .unwrap_or(defaults.quality),
            })
        }
        "sample" => EngineFrame::Sample {
            participant_id: text(&value, "participantId"),
            reason: text(&value, "reason").unwrap_or_else(|| "requested".to_string()),
        },
        "clear" => EngineFrame::Clear,
        "mark" => EngineFrame::Mark(text(&value, "name").unwrap_or_default()),
        "hangup" => EngineFrame::Hangup,
        other => EngineFrame::Other(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_round_trips() {
        let header = FrameHeader {
            kind: KIND_VIDEO,
            flags: FLAG_REQUESTED,
            index: 513,
            at_ms: 0x0102_0304_0506,
        };
        let frame = header.encode(b"\xFF\xD8payload");
        assert_eq!(frame.len(), HEADER_LEN + 9);
        assert_eq!(&frame[..HEADER_LEN], &[2, 1, 2, 1, 0, 0, 1, 2, 3, 4, 5, 6]);
        let (decoded, payload) = FrameHeader::decode(&frame).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(payload, b"\xFF\xD8payload");
        assert!(FrameHeader::decode(&frame[..11]).is_none());
    }

    #[test]
    fn pcm_round_trips() {
        let samples = [0i16, 1, -1, i16::MAX, i16::MIN];
        let bytes = pcm16_le(&samples);
        assert_eq!(&bytes[..4], &[0, 0, 1, 0]);
        assert_eq!(pcm16_from_le(&bytes), samples);
    }

    #[test]
    fn policies_parse_with_defaults_and_limits() {
        let p = parse_engine_text(
            r#"{"event":"policy","video":{"enabled":true,"fps":0.5,"format":"image/jpeg","maxWidth":320,"quality":60}}"#,
        )
        .unwrap();
        assert_eq!(
            p,
            EngineFrame::Policy(VideoPolicy {
                enabled: true,
                fps: 0.5,
                max_width: 320,
                quality: 60
            })
        );
        let EngineFrame::Policy(p) =
            parse_engine_text(r#"{"event":"policy","video":{"fps":120}}"#).unwrap()
        else {
            panic!()
        };
        assert!(p.enabled);
        assert_eq!(p.fps, 30.0);
        assert_eq!((p.max_width, p.quality), (640, 70));
        let EngineFrame::Policy(p) =
            parse_engine_text(r#"{"event":"policy","video":{"enabled":false}}"#).unwrap()
        else {
            panic!()
        };
        assert!(!p.enabled);
        let EngineFrame::Policy(p) =
            parse_engine_text(r#"{"event":"policy","video":{"format":"image/png"}}"#).unwrap()
        else {
            panic!()
        };
        assert!(!p.enabled, "only JPEG is produced");
        let EngineFrame::Policy(p) = parse_engine_text(r#"{"event":"policy"}"#).unwrap() else {
            panic!()
        };
        assert!(!p.enabled, "no video section: no video");
    }

    #[test]
    fn engine_frames_parse() {
        assert_eq!(
            parse_engine_text(r#"{"event":"sample","participantId":null,"reason":"interruption"}"#)
                .unwrap(),
            EngineFrame::Sample {
                participant_id: None,
                reason: "interruption".into()
            }
        );
        assert_eq!(
            parse_engine_text(r#"{"event":"sample","participantId":"PA_1"}"#).unwrap(),
            EngineFrame::Sample {
                participant_id: Some("PA_1".into()),
                reason: "requested".into()
            }
        );
        assert_eq!(
            parse_engine_text(r#"{"event":"clear"}"#).unwrap(),
            EngineFrame::Clear
        );
        assert_eq!(
            parse_engine_text(r#"{"event":"mark","name":"consent"}"#).unwrap(),
            EngineFrame::Mark("consent".into())
        );
        assert_eq!(
            parse_engine_text(r#"{"event":"hangup"}"#).unwrap(),
            EngineFrame::Hangup
        );
        assert!(matches!(
            parse_engine_text(r#"{"event":"future"}"#).unwrap(),
            EngineFrame::Other(_)
        ));
        assert!(parse_engine_text("nope").is_err());
        assert!(parse_engine_text("{}").is_err());
    }

    #[test]
    fn text_frames_carry_only_the_protocol_fields() {
        let start: Value = serde_json::from_str(&start(&StartInfo {
            call_id: "standup-1".into(),
            stream_id: "RM_x".into(),
            room: "standup".into(),
            called: "+15550190000".into(),
            rate: 16000,
        }))
        .unwrap();
        assert_eq!(start["format"], "audio/L16;rate=16000");
        assert_eq!(start["streams"], "tagged");
        assert_eq!(start["from"], "livekit:standup");
        assert_eq!(start["headers"]["x-room"], "standup");
        let joined: Value =
            serde_json::from_str(&participant_joined("PA_1", 1, "Alex", Some("staff"), 20))
                .unwrap();
        let mut keys: Vec<&str> = joined
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            ["atMs", "event", "index", "label", "participantId", "role"]
        );
        let joined: Value =
            serde_json::from_str(&participant_joined("PA_2", 2, "Sam", None, 0)).unwrap();
        assert!(joined["role"].is_null());
        assert_eq!(label_of("  ", 3), "Participant 3");
        assert_eq!(label_of("Sam", 3), "Sam");
    }
}
