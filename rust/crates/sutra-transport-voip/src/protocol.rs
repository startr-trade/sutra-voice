//! The two media WebSocket framings this transport serves.
//!
//! **Generic media-stream protocol** (`GET /voip/stream`) — the gateway-neutral one:
//! - gateway → Sutra text frames: `{"event":"start","callId","streamId","format":"audio/L16;rate=8000","from","to","direction","headers"}`,
//!   `{"event":"dtmf","digit"}`, `{"event":"mark","name"}` (an echoed mark), `{"event":"stop","reason"}`;
//!   binary frames are audio in the declared format.
//! - Sutra → gateway: binary frames are audio to play; `{"event":"clear"}` (barge-in: drop
//!   queued audio), `{"event":"mark","name"}` (echo back when playback reaches it),
//!   `{"event":"hangup"}`.
//!
//! **Tagged streams** (additive; a session whose `start` says `"streams":"tagged"`) — one socket
//! carries every participant of a meeting, each with its own audio and sampled video:
//! - gateway → Sutra text frames: `{"event":"participant-joined","participantId","index","label",
//!   "role","atMs"}`, `{"event":"participant-left","participantId","reason","atMs"}`,
//!   `{"event":"track","participantId","kind","state","atMs"}` (informational);
//! - binary frames carry a 12-byte header ([`TaggedHeader`]): kind (1 audio, 2 video image),
//!   flags (video: bit 0 = the frame answers a `sample` request), the participant's `index`
//!   (u16, big-endian), the capture time in ms since the session started (u64, big-endian); the
//!   payload is PCM16 LE mono at the `start` format's rate, or one image (`image/jpeg`).
//! - Sutra → gateway: `{"event":"policy","video":{"enabled","fps","format","maxWidth","quality"}}`
//!   (the sampling the channel set; no video is sent before it) and
//!   `{"event":"sample","participantId":null|"<id>","reason"}` (send a frame now — a turn
//!   boundary, an interruption, raised energy, a long silence). Binary frames remain audio to
//!   play (the bot speaking), `clear` / `mark` / `hangup` as above.
//!
//! **PCM bridge** (`GET /voip/bridge`) — the media-bridge framing of a PBX that dials Sutra:
//! binary frames both ways are headerless PCM16 little-endian mono at the `samplerate` the
//! bridge URL declared (one frame per RTP packet, about 20 ms, inbound); the only text frame
//! is `{"type":"dtmf","digit":"5"}` in either direction (inbound may add `leg_id`). It has no
//! clear, mark or hang-up frame, which is why playback is paced in real time and hang-up goes
//! through the gateway's control channel.

use std::collections::BTreeMap;

use serde_json::{json, Value};

/// A parsed generic-protocol text frame.
#[derive(Debug, Clone, PartialEq)]
pub enum GenericFrame {
    Start(StreamStart),
    Dtmf {
        digit: String,
    },
    Mark {
        name: String,
    },
    Stop {
        reason: Option<String>,
    },
    /// Tagged streams: a participant is present (or arrived).
    ParticipantJoined(ParticipantJoined),
    /// Tagged streams: a participant left.
    ParticipantLeft {
        participant_id: String,
        reason: Option<String>,
    },
    /// Tagged streams: a track was (un)subscribed — informational.
    Track {
        participant_id: String,
        kind: String,
        state: String,
    },
}

/// A tagged session's `participant-joined` frame. `participantId` is the gateway's opaque
/// participant id and `label` the display label the participant chose — never an identity the
/// gateway inferred. `role` is the role the participant was admitted with (a token attribute),
/// when the gateway knows one.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParticipantJoined {
    pub participant_id: String,
    pub index: u16,
    pub label: Option<String>,
    pub role: Option<String>,
    pub at_ms: Option<u64>,
}

/// The generic protocol's `start` frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StreamStart {
    pub call_id: String,
    pub stream_id: Option<String>,
    pub format: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub direction: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// `"streams":"tagged"`: binary frames carry a [`TaggedHeader`] (one socket, many
    /// participants).
    pub tagged: bool,
    /// The image formats the gateway can send (`video.formats`).
    pub video_formats: Vec<String>,
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match value.get(*key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

/// Parse one generic-protocol text frame.
pub fn parse_generic_text(text: &str) -> Result<GenericFrame, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("frame is not JSON: {e}"))?;
    let event = string_field(&value, &["event"]).ok_or("frame has no 'event'")?;
    match event.as_str() {
        "start" => {
            let call_id = string_field(&value, &["callId", "call_id"])
                .ok_or("'start' frame has no 'callId'")?;
            let mut headers = BTreeMap::new();
            if let Some(Value::Object(map)) = value.get("headers") {
                for (key, header_value) in map {
                    let rendered = match header_value {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    headers.insert(key.to_ascii_lowercase(), rendered);
                }
            }
            Ok(GenericFrame::Start(StreamStart {
                call_id,
                stream_id: string_field(&value, &["streamId", "stream_id"]),
                format: string_field(&value, &["format"]),
                from: string_field(&value, &["from"]),
                to: string_field(&value, &["to"]),
                direction: string_field(&value, &["direction"]),
                headers,
                tagged: string_field(&value, &["streams"])
                    .is_some_and(|s| s.eq_ignore_ascii_case("tagged")),
                video_formats: value
                    .get("video")
                    .and_then(|v| v.get("formats"))
                    .and_then(Value::as_array)
                    .map(|formats| {
                        formats
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|f| f.trim().to_ascii_lowercase())
                            .collect()
                    })
                    .unwrap_or_default(),
            }))
        }
        "participant-joined" => {
            let participant_id = string_field(&value, &["participantId", "participant_id"])
                .ok_or("'participant-joined' frame has no 'participantId'")?;
            let index = value
                .get("index")
                .and_then(Value::as_u64)
                .filter(|i| (1..=u64::from(u16::MAX)).contains(i))
                .ok_or("'participant-joined' frame needs an 'index' of 1-65535")?
                as u16;
            Ok(GenericFrame::ParticipantJoined(ParticipantJoined {
                participant_id,
                index,
                label: string_field(&value, &["label"]),
                role: string_field(&value, &["role"]).map(|r| r.trim().to_ascii_lowercase()),
                at_ms: value.get("atMs").and_then(Value::as_u64),
            }))
        }
        "participant-left" => Ok(GenericFrame::ParticipantLeft {
            participant_id: string_field(&value, &["participantId", "participant_id"])
                .ok_or("'participant-left' frame has no 'participantId'")?,
            reason: string_field(&value, &["reason"]),
        }),
        "track" => Ok(GenericFrame::Track {
            participant_id: string_field(&value, &["participantId", "participant_id"])
                .unwrap_or_default(),
            kind: string_field(&value, &["kind"]).unwrap_or_default(),
            state: string_field(&value, &["state"]).unwrap_or_default(),
        }),
        "dtmf" => Ok(GenericFrame::Dtmf {
            digit: string_field(&value, &["digit"]).ok_or("'dtmf' frame has no 'digit'")?,
        }),
        "mark" => Ok(GenericFrame::Mark {
            name: string_field(&value, &["name"]).unwrap_or_default(),
        }),
        "stop" => Ok(GenericFrame::Stop {
            reason: string_field(&value, &["reason"]),
        }),
        other => Err(format!("unknown event '{other}'")),
    }
}

/// Sutra → gateway generic-protocol control frames.
pub fn generic_clear() -> String {
    json!({"event": "clear"}).to_string()
}

pub fn generic_mark(name: &str) -> String {
    json!({"event": "mark", "name": name}).to_string()
}

pub fn generic_hangup() -> String {
    json!({"event": "hangup"}).to_string()
}

/// What a tagged binary frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaggedKind {
    Audio,
    Video,
}

/// The 12-byte header of a tagged binary frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaggedHeader {
    pub kind: TaggedKind,
    pub flags: u8,
    /// The participant's `index` (from its `participant-joined`).
    pub index: u16,
    /// Capture time, ms since the session started (the gateway's clock).
    pub at_ms: u64,
}

/// Bytes in a [`TaggedHeader`].
pub const TAGGED_HEADER_LEN: usize = 12;
/// Video flag: the frame answers a `sample` request.
pub const FLAG_REQUESTED: u8 = 0x01;

impl TaggedHeader {
    /// Whether a video frame answers a `sample` request.
    pub fn requested(&self) -> bool {
        self.flags & FLAG_REQUESTED != 0
    }

    pub fn encode(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(TAGGED_HEADER_LEN + payload.len());
        out.push(match self.kind {
            TaggedKind::Audio => 1,
            TaggedKind::Video => 2,
        });
        out.push(self.flags);
        out.extend_from_slice(&self.index.to_be_bytes());
        out.extend_from_slice(&self.at_ms.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }
}

/// Split a tagged binary frame into its header and payload.
pub fn parse_tagged(bytes: &[u8]) -> Result<(TaggedHeader, &[u8]), String> {
    if bytes.len() < TAGGED_HEADER_LEN {
        return Err(format!(
            "tagged frame of {} bytes has no header",
            bytes.len()
        ));
    }
    let kind = match bytes[0] {
        1 => TaggedKind::Audio,
        2 => TaggedKind::Video,
        other => return Err(format!("unknown tagged frame kind {other}")),
    };
    let index = u16::from_be_bytes([bytes[2], bytes[3]]);
    let mut at = [0u8; 8];
    at.copy_from_slice(&bytes[4..12]);
    Ok((
        TaggedHeader {
            kind,
            flags: bytes[1],
            index,
            at_ms: u64::from_be_bytes(at),
        },
        &bytes[TAGGED_HEADER_LEN..],
    ))
}

/// Sutra → gateway: the video sampling a tagged session runs at.
pub fn generic_policy(
    enabled: bool,
    fps: f64,
    format: &str,
    max_width: u32,
    quality: u8,
) -> String {
    json!({"event": "policy", "video": {
        "enabled": enabled, "fps": fps, "format": format, "maxWidth": max_width, "quality": quality,
    }})
    .to_string()
}

/// Sutra → gateway: send a frame of `participant` (every participant when `None`) now.
pub fn generic_sample(participant: Option<&str>, reason: &str) -> String {
    json!({"event": "sample", "participantId": participant, "reason": reason}).to_string()
}

/// A parsed bridge text frame (DTMF is the only one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeDtmf {
    pub digit: String,
    pub leg_id: Option<String>,
}

/// Parse one bridge text frame: `{"type":"dtmf","digit":"5","leg_id":"caller"}`.
pub fn parse_bridge_text(text: &str) -> Result<BridgeDtmf, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("frame is not JSON: {e}"))?;
    match string_field(&value, &["type"]).as_deref() {
        Some("dtmf") => Ok(BridgeDtmf {
            digit: string_field(&value, &["digit"]).ok_or("'dtmf' frame has no 'digit'")?,
            leg_id: string_field(&value, &["leg_id"]),
        }),
        Some(other) => Err(format!("unknown frame type '{other}'")),
        None => Err("frame has no 'type'".to_string()),
    }
}

/// Sutra → PBX DTMF frame.
pub fn bridge_dtmf(digit: &str) -> String {
    json!({"type": "dtmf", "digit": digit}).to_string()
}

/// Which framing a media socket speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    Generic,
    Bridge,
}

impl Framing {
    pub fn as_str(&self) -> &'static str {
        match self {
            Framing::Generic => "generic",
            Framing::Bridge => "bridge",
        }
    }
}
