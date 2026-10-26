//! Meeting sessions: one tagged media socket carrying every participant of a meeting — each
//! participant's own audio, and sampled video frames — from a gateway that joins the meeting as
//! a participant itself (a WebRTC room, say). See [`crate::protocol`] for the framing.
//!
//! Per participant the transport keeps what a listen-only call keeps (its audio, recorded to the
//! object store as it arrives; a live transcription session, so every transcript segment is
//! attributed EXACTLY — it comes from that participant's own track, no diarization), plus the
//! sampled frames of its camera and a speech tracker that finds turn boundaries, interruptions,
//! raised energy and long silences. Those moments ask the gateway for an extra frame of EVERY
//! participant, so a listener's reaction is captured as well as the speaker's.
//!
//! **Segments** (a `call-segment` channel, [`SegmentSettings`]): the session is cut into fixed
//! time segments. As each segment closes, each participant's audio of the segment and the frames
//! sampled in it are written to the recording store as sealed objects, and one `call-segment`
//! message per participant carries their media references (`{kind: "media", uri, sha256,
//! participant, startMs, endMs}`) with the segment's generic measurements: talk time, acoustic
//! features, the flagged moments, the words spoken in it (pseudonymised speakers) and who was
//! present. **What is analysed, by which model, with which prompt, and how the signals are
//! fused is the flow's business**: it reads the references and calls a model through an `llm`
//! channel whose `media-store` reads the objects.
//!
//! **Bounded memory.** A participant's audio is kept only for a window (the longer of
//! [`crate::listen::TAP_WINDOW_SECONDS`] and a segment plus its lag); frames, transcript lines,
//! voiced spans and flagged moments are let go once their segment has been written. A
//! participant's footprint does not grow with the length of the meeting
//! ([`MeetingState::memory_bytes`]).
//!
//! Roles are what the participant was admitted with (a gateway-declared attribute), never
//! inferred; `segment.roles` limits which participants are cut into segments at all, so the
//! media of anyone else is never written as a segment.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use sutra_channels::{ChannelDefinition, Diagnostic};

use crate::codes;
use crate::features::{analyze_turn, frame_rms, PitchConfig};
use crate::listen::ListenState;

/// The role of a participant the gateway declared none for, unless `segment.default-role` says
/// otherwise.
pub const ROLE_UNKNOWN: &str = "unknown";
/// The image format frames are sampled in by default.
pub const DEFAULT_FRAME_FORMAT: &str = "image/jpeg";
/// The most frames kept per participant (beyond it frames are counted as dropped).
pub const MAX_FRAMES_PER_PARTICIPANT: usize = 7200;
/// The most frame bytes kept per session.
pub const MAX_FRAME_BYTES_PER_SESSION: u64 = 256 * 1024 * 1024;
/// The least time between two `sample` requests to the gateway.
pub const SAMPLE_REQUEST_GAP_MS: u64 = 500;
/// The least time between two raised-energy flags of one participant.
pub const RAISED_ENERGY_GAP_MS: u64 = 3000;
/// How far behind the session clock a segment is cut (transcript lines arrive a little after
/// their audio).
pub const SEGMENT_LAG_MS: u64 = 2000;
/// How often the session looks for segments that have closed.
pub const SEGMENT_TICK_MS: u64 = 1000;
/// The segment length, by default.
pub const DEFAULT_SEGMENT_SECONDS: u64 = 10;
/// The most frames one segment carries, by default.
pub const DEFAULT_SEGMENT_FRAMES: usize = 8;

fn property<'a>(definition: &'a ChannelDefinition, key: &str) -> Option<&'a str> {
    definition
        .properties
        .get(key)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

fn invalid(definition: &ChannelDefinition, message: String) -> Diagnostic {
    Diagnostic::error(
        codes::CHANNEL_INVALID,
        format!("channel '{}': {message}", definition.binding.channel_name),
    )
}

fn bool_property(definition: &ChannelDefinition, key: &str) -> Result<Option<bool>, Diagnostic> {
    match property(definition, key) {
        None => Ok(None),
        Some(v) if v.eq_ignore_ascii_case("true") => Ok(Some(true)),
        Some(v) if v.eq_ignore_ascii_case("false") => Ok(Some(false)),
        Some(other) => Err(invalid(
            definition,
            format!("'{key}' must be true or false, got '{other}'"),
        )),
    }
}

fn parsed_property<T: std::str::FromStr>(
    definition: &ChannelDefinition,
    key: &str,
) -> Result<Option<T>, Diagnostic> {
    match property(definition, key) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<T>()
            .map(Some)
            .map_err(|_| invalid(definition, format!("'{key}' must be a number, got '{raw}'"))),
    }
}

/// A YAML list (`[a, b]`, or one item per line) or a comma-separated value, lower-cased.
fn list_property(definition: &ChannelDefinition, key: &str) -> Option<Vec<String>> {
    property(definition, key).map(|raw| {
        raw.trim_matches(['[', ']'])
            .split([',', '\n'])
            .map(|s| {
                s.trim()
                    .trim_start_matches('-')
                    .trim()
                    .trim_matches(['"', '\''])
                    .to_ascii_lowercase()
            })
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// How a session's video is sampled (the `call-start` channel's `video.*`).
#[derive(Debug, Clone, PartialEq)]
pub struct VideoPolicy {
    pub enabled: bool,
    /// Regular frames per second, per participant (fractional allowed).
    pub fps: f64,
    pub format: String,
    pub max_width: u32,
    pub quality: u8,
    /// Ask for a frame of every participant at turn boundaries and audio-flagged moments.
    pub extra_frames: bool,
}

impl Default for VideoPolicy {
    fn default() -> Self {
        VideoPolicy {
            enabled: true,
            fps: 1.0,
            format: DEFAULT_FRAME_FORMAT.to_string(),
            max_width: 640,
            quality: 70,
            extra_frames: true,
        }
    }
}

/// What a `call-start` channel says about the sessions it starts.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionSettings {
    /// `mode: listen` — no turns are cut (a tagged session is listen-only regardless).
    pub listen_only: bool,
    /// `record: true` / `record: after-consent` — record the audio (per participant in a
    /// tagged session).
    pub record: bool,
    /// `record: after-consent` — the recording starts only once consent is given: a `play`
    /// marked `consent: true` played to the end, or a `record-start` naming consent.
    /// `record: true` records from the moment the media connects, and leaves consent to the
    /// deployment (an announcement the PBX plays before the call reaches the engine, say).
    pub record_after_consent: bool,
    pub video: VideoPolicy,
}

impl SessionSettings {
    pub fn parse(definition: &ChannelDefinition) -> Result<SessionSettings, Diagnostic> {
        let listen_only = match property(definition, "mode") {
            None => false,
            Some(m) if m.eq_ignore_ascii_case("listen") => true,
            Some(m) if m.eq_ignore_ascii_case("conversation") => false,
            Some(other) => {
                return Err(invalid(
                    definition,
                    format!("mode must be listen or conversation, got '{other}'"),
                ))
            }
        };
        let mut video = VideoPolicy::default();
        if let Some(enabled) = bool_property(definition, "video.enabled")? {
            video.enabled = enabled;
        }
        if let Some(fps) = parsed_property::<f64>(definition, "video.fps")? {
            if !(fps > 0.0 && fps <= 30.0) {
                return Err(invalid(
                    definition,
                    format!("video.fps must be above 0 and at most 30, got {fps}"),
                ));
            }
            video.fps = fps;
        }
        if let Some(format) = property(definition, "video.format") {
            let format = format.to_ascii_lowercase();
            if format != "image/jpeg" && format != "image/png" {
                return Err(invalid(
                    definition,
                    format!("video.format must be image/jpeg or image/png, got '{format}'"),
                ));
            }
            video.format = format;
        }
        if let Some(width) = parsed_property::<u32>(definition, "video.max-width")? {
            video.max_width = width.clamp(16, 4096);
        }
        if let Some(quality) = parsed_property::<u8>(definition, "video.quality")? {
            video.quality = quality.clamp(1, 100);
        }
        if let Some(extra) = bool_property(definition, "video.extra-frames")? {
            video.extra_frames = extra;
        }
        let (record, record_after_consent) = match property(definition, "record") {
            Some(v) if v.eq_ignore_ascii_case("after-consent") => (true, true),
            _ => (
                bool_property(definition, "record")
                    .map_err(|_| {
                        invalid(
                            definition,
                            "'record' must be true, false or after-consent".to_string(),
                        )
                    })?
                    .unwrap_or(false),
                false,
            ),
        };
        Ok(SessionSettings {
            listen_only,
            record,
            record_after_consent,
            video,
        })
    }
}

/// The `call-segment` channel's `segment.*`: how a meeting session is cut into per-participant
/// media segments for the flow.
///
/// | Key | Meaning | Default |
/// |---|---|---|
/// | `segment.seconds` | the length of a segment (2–300) | `10` |
/// | `segment.max-frames` | the most sampled frames of one participant written for one segment (0–32; 0: audio only) | `8` |
/// | `segment.roles` | the roles cut into segments (a participant with any other role never has a segment written) | every role |
/// | `segment.default-role` | the role of a participant the gateway declared none for | `unknown` |
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentSettings {
    pub segment_ms: u64,
    pub max_frames: usize,
    /// `None`: every participant.
    pub roles: Option<Vec<String>>,
    pub default_role: String,
}

impl Default for SegmentSettings {
    fn default() -> Self {
        SegmentSettings {
            segment_ms: DEFAULT_SEGMENT_SECONDS * 1000,
            max_frames: DEFAULT_SEGMENT_FRAMES,
            roles: None,
            default_role: ROLE_UNKNOWN.to_string(),
        }
    }
}

/// `analyze.*` keys on a voip channel: the transport no longer analyses, so they are refused
/// with a pointer to where analysis lives now.
pub fn refuse_analysis_keys(definition: &ChannelDefinition) -> Result<(), Diagnostic> {
    if let Some(key) = definition
        .properties
        .keys()
        .find(|k| k.starts_with("analyze."))
    {
        return Err(invalid(
            definition,
            format!(
                "'{key}': the voip transport no longer analyses meetings. Declare a \
                 `call-segment` channel (segment.*): each segment's media arrives as references, \
                 and the flow calls the model through an `llm` channel with a `media-store`"
            ),
        ));
    }
    Ok(())
}

impl SegmentSettings {
    pub fn parse(definition: &ChannelDefinition) -> Result<SegmentSettings, Diagnostic> {
        refuse_analysis_keys(definition)?;
        let seconds = parsed_property::<u64>(definition, "segment.seconds")?
            .unwrap_or(DEFAULT_SEGMENT_SECONDS);
        if !(2..=300).contains(&seconds) {
            return Err(invalid(
                definition,
                format!("segment.seconds must be 2-300, got {seconds}"),
            ));
        }
        let max_frames = parsed_property::<usize>(definition, "segment.max-frames")?
            .unwrap_or(DEFAULT_SEGMENT_FRAMES);
        if max_frames > 32 {
            return Err(invalid(
                definition,
                format!("segment.max-frames must be 0-32, got {max_frames}"),
            ));
        }
        Ok(SegmentSettings {
            segment_ms: seconds * 1000,
            max_frames,
            roles: list_property(definition, "segment.roles"),
            default_role: property(definition, "segment.default-role")
                .map(|r| r.to_ascii_lowercase())
                .unwrap_or_else(|| ROLE_UNKNOWN.to_string()),
        })
    }

    /// Whether a participant with `role` is cut into segments.
    pub fn segments_role(&self, role: &str) -> bool {
        self.roles
            .as_ref()
            .is_none_or(|roles| roles.iter().any(|r| r == role))
    }

    /// How much of a participant's audio is kept in memory: enough to cut a segment after its
    /// lag and one tick, and never less than the transcript-feature window.
    pub fn tap_window_seconds(&self) -> u64 {
        (self.segment_ms + SEGMENT_LAG_MS + 2 * SEGMENT_TICK_MS)
            .div_ceil(1000)
            .max(crate::listen::TAP_WINDOW_SECONDS)
    }
}

/// The role a participant is treated as: the declared one, else the default.
pub fn effective_role(declared: Option<&str>, segments: Option<&SegmentSettings>) -> String {
    match declared.map(str::trim).filter(|r| !r.is_empty()) {
        Some(role) => role.to_ascii_lowercase(),
        None => segments
            .map(|s| s.default_role.clone())
            .unwrap_or_else(|| ROLE_UNKNOWN.to_string()),
    }
}

// ---- per-participant state --------------------------------------------------------------------

/// One sampled video frame (in memory only).
#[derive(Debug, Clone)]
pub struct SampledFrame {
    /// Session time (ms since the session started, the transport's clock).
    pub at_ms: u64,
    pub data: Vec<u8>,
    /// The frame answers a `sample` request (a turn boundary, a flagged moment).
    pub requested: bool,
}

/// One final transcript segment of a participant, in session time.
#[derive(Debug, Clone)]
pub struct TranscriptLine {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// A moment the audio flagged.
#[derive(Debug, Clone, PartialEq)]
pub struct FlaggedMoment {
    pub at_ms: u64,
    pub reason: &'static str,
    pub index: u16,
}

/// What a participant's speech did in one audio frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechChange {
    Started,
    Ended,
    Loud,
}

/// A small energy speech tracker: voiced after `min_speech_ms` of voiced frames, silent after
/// `silence_ms` of unvoiced ones; `Loud` when a voiced frame's energy is well above the
/// participant's running mean.
#[derive(Debug, Clone)]
pub struct SpeechTracker {
    frame_samples: usize,
    frame_ms: u64,
    threshold: f64,
    silence_ms: u64,
    min_speech_ms: u64,
    partial: Vec<i16>,
    voiced_run_ms: u64,
    silence_run_ms: u64,
    pub speaking: bool,
    /// Voiced ms in total.
    pub voiced_ms: u64,
    mean_rms: f64,
    voiced_frames: u64,
    last_loud_ms: Option<u64>,
    /// Ms of audio pushed so far.
    pub elapsed_ms: u64,
}

impl SpeechTracker {
    pub fn new(rate: u32, vad: &crate::vad::VadConfig) -> SpeechTracker {
        let frame_ms = u64::from(vad.frame_ms.max(10));
        SpeechTracker {
            frame_samples: ((u64::from(rate) * frame_ms / 1000) as usize).max(1),
            frame_ms,
            threshold: vad.energy_threshold,
            silence_ms: u64::from(vad.silence_ms.min(1500)),
            min_speech_ms: u64::from(vad.min_speech_ms),
            partial: Vec::new(),
            voiced_run_ms: 0,
            silence_run_ms: 0,
            speaking: false,
            voiced_ms: 0,
            mean_rms: 0.0,
            voiced_frames: 0,
            last_loud_ms: None,
            elapsed_ms: 0,
        }
    }

    /// Feed audio; answers each change with its offset (ms of this participant's audio).
    pub fn push(&mut self, samples: &[i16]) -> Vec<(u64, SpeechChange)> {
        self.partial.extend_from_slice(samples);
        let mut changes = Vec::new();
        let mut offset = 0;
        while self.partial.len() - offset >= self.frame_samples {
            let rms = frame_rms(&self.partial[offset..offset + self.frame_samples]);
            offset += self.frame_samples;
            self.elapsed_ms += self.frame_ms;
            let voiced = rms >= self.threshold;
            if voiced {
                self.voiced_ms += self.frame_ms;
                self.voiced_run_ms += self.frame_ms;
                self.silence_run_ms = 0;
                self.voiced_frames += 1;
                if self.voiced_frames > 100
                    && rms > 2.0 * self.mean_rms
                    && rms > 2.0 * self.threshold
                    && self
                        .last_loud_ms
                        .is_none_or(|at| self.elapsed_ms >= at + RAISED_ENERGY_GAP_MS)
                {
                    self.last_loud_ms = Some(self.elapsed_ms);
                    changes.push((self.elapsed_ms, SpeechChange::Loud));
                }
                self.mean_rms += (rms - self.mean_rms) / (self.voiced_frames.min(500) as f64);
                if !self.speaking && self.voiced_run_ms >= self.min_speech_ms.max(self.frame_ms) {
                    self.speaking = true;
                    changes.push((
                        self.elapsed_ms.saturating_sub(self.voiced_run_ms),
                        SpeechChange::Started,
                    ));
                }
            } else {
                self.silence_run_ms += self.frame_ms;
                self.voiced_run_ms = 0;
                if self.speaking && self.silence_run_ms >= self.silence_ms {
                    self.speaking = false;
                    changes.push((
                        self.elapsed_ms.saturating_sub(self.silence_run_ms),
                        SpeechChange::Ended,
                    ));
                }
            }
        }
        self.partial.drain(..offset);
        changes
    }
}

/// One participant of a session.
pub struct MeetingParticipant {
    pub participant_id: String,
    pub index: u16,
    pub label: String,
    pub role: String,
    pub declared_role: Option<String>,
    /// The participant is cut into segments (a `call-segment` channel, and its role is one of
    /// `segment.roles`).
    pub segmented: bool,
    pub joined_at_ms: u64,
    pub left_at_ms: Option<u64>,
    pub left_reason: Option<String>,
    /// The participant's audio since it joined (offsets relative to `joined_at_ms`; only the
    /// last window is held), with the frames channel to its transcriber.
    pub audio: ListenState,
    /// The transcriber's summary, once it finished.
    pub transcript: Option<tokio::sync::oneshot::Receiver<Value>>,
    pub speech: SpeechTracker,
    /// Voiced stretches not yet behind a written segment, session time.
    pub spans: Vec<(u64, u64)>,
    span_start: Option<u64>,
    /// Frames not yet written with their segment (kept only for a segmented participant).
    pub frames: Vec<SampledFrame>,
    pub frames_regular: u64,
    pub frames_requested: u64,
    pub frames_dropped: u64,
    /// Transcript lines not yet behind a written segment.
    pub lines: Vec<TranscriptLine>,
    /// Transcript lines in total.
    pub line_count: u64,
    pub tracks: BTreeMap<String, String>,
}

impl MeetingParticipant {
    /// Whether the participant was present in `[start, end)`.
    pub fn present_in(&self, start_ms: u64, end_ms: u64) -> bool {
        self.joined_at_ms < end_ms && self.left_at_ms.is_none_or(|left| left > start_ms)
    }

    /// Voiced ms within `[start, end)`.
    pub fn talk_ms_in(&self, start_ms: u64, end_ms: u64) -> u64 {
        let open = self.span_start.map(|s| (s, u64::MAX));
        self.spans
            .iter()
            .copied()
            .chain(open)
            .map(|(s, e)| e.min(end_ms).saturating_sub(s.max(start_ms)))
            .sum()
    }

    /// The participant's audio in `[start, end)` of session time, as far as the window holds it.
    pub fn audio_in(&self, start_ms: u64, end_ms: u64) -> Vec<i16> {
        let rate = u64::from(self.audio.rate.max(1));
        let from = start_ms.saturating_sub(self.joined_at_ms) * rate / 1000;
        let to = end_ms.saturating_sub(self.joined_at_ms) * rate / 1000;
        self.audio.tap_between(from, to).to_vec()
    }

    /// The bytes this participant holds in memory: its audio window, its frames, the text and
    /// spans not yet let go.
    pub fn memory_bytes(&self) -> usize {
        self.audio.tap.len() * 2
            + self
                .audio
                .recording
                .as_ref()
                .map_or(0, |w| w.pending_samples() * 2)
            + self.frames.iter().map(|f| f.data.len()).sum::<usize>()
            + self.lines.iter().map(|l| l.text.len() + 16).sum::<usize>()
            + self.spans.len() * 16
    }
}

/// The session's clock: real time, or (tests) a counter the test advances.
#[derive(Clone)]
pub enum SessionClock {
    Real(Instant),
    Manual(Arc<AtomicU64>),
}

impl SessionClock {
    pub fn now_ms(&self) -> u64 {
        match self {
            SessionClock::Real(started) => started.elapsed().as_millis() as u64,
            SessionClock::Manual(ms) => ms.load(Ordering::SeqCst),
        }
    }
}

/// A meeting session's state (inside the call, behind a lock).
pub struct MeetingState {
    clock: SessionClock,
    pub rate: u32,
    pub record: bool,
    /// The recordings (and segments) wait for consent (`record: after-consent`).
    pub record_after_consent: bool,
    /// A `record-start` started the recordings.
    pub record_started: bool,
    pub video: VideoPolicy,
    /// How the session is cut into segments (`None`: no `call-segment` channel).
    pub segments: Option<SegmentSettings>,
    pub participants: BTreeMap<u16, MeetingParticipant>,
    pub ids: HashMap<String, u16>,
    pub consent_at_ms: Option<u64>,
    /// Flagged moments not yet behind a written segment.
    pub flags: Vec<FlaggedMoment>,
    /// Flagged moments in total.
    pub flag_count: u64,
    last_sample_request_ms: Option<u64>,
    silence_since_ms: Option<u64>,
    silence_flagged: bool,
    pub dead_air_ms: u64,
    pub frame_bytes: u64,
    /// The first segment not yet cut.
    pub next_segment: u64,
    pub max_seconds: u64,
}

impl MeetingState {
    pub fn new(
        rate: u32,
        record: bool,
        video: VideoPolicy,
        dead_air_ms: u64,
        max_seconds: u64,
    ) -> MeetingState {
        MeetingState {
            clock: SessionClock::Real(Instant::now()),
            rate,
            record,
            record_after_consent: false,
            record_started: false,
            video,
            segments: None,
            participants: BTreeMap::new(),
            ids: HashMap::new(),
            consent_at_ms: None,
            flags: Vec::new(),
            flag_count: 0,
            last_sample_request_ms: None,
            silence_since_ms: None,
            silence_flagged: false,
            dead_air_ms: dead_air_ms.max(500),
            frame_bytes: 0,
            next_segment: 0,
            max_seconds,
        }
    }

    /// Cut the session into segments as `settings` say.
    pub fn with_segments(mut self, settings: Option<SegmentSettings>) -> MeetingState {
        self.segments = settings;
        self
    }

    /// Run on a clock the caller advances (ms since the session started).
    pub fn with_manual_clock(mut self, clock: Arc<AtomicU64>) -> MeetingState {
        self.clock = SessionClock::Manual(clock);
        self
    }

    /// Ms since the session started.
    pub fn elapsed_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// How much audio a participant keeps in memory.
    pub fn tap_window_seconds(&self) -> u64 {
        self.segments
            .as_ref()
            .map(SegmentSettings::tap_window_seconds)
            .unwrap_or(crate::listen::TAP_WINDOW_SECONDS)
    }

    /// Whether segments may be cut now: a session recorded `after-consent` cuts none before
    /// consent is given (the same gate as its recordings).
    pub fn segments_allowed(&self) -> bool {
        !(self.record_after_consent && self.consent_at_ms.is_none() && !self.record_started)
    }

    pub fn participant_by_id(&self, id: &str) -> Option<&MeetingParticipant> {
        self.ids.get(id).and_then(|i| self.participants.get(i))
    }

    /// The bytes every participant holds in memory, and the session's own flagged moments.
    pub fn memory_bytes(&self) -> usize {
        self.participants
            .values()
            .map(MeetingParticipant::memory_bytes)
            .sum::<usize>()
            + self.flags.len() * std::mem::size_of::<FlaggedMoment>()
    }

    /// Add a participant (a repeated join of a known id is ignored: `None`).
    pub fn join(
        &mut self,
        joined: &crate::protocol::ParticipantJoined,
        vad: &crate::vad::VadConfig,
    ) -> Option<&mut MeetingParticipant> {
        if self.ids.contains_key(&joined.participant_id)
            || self.participants.contains_key(&joined.index)
        {
            return None;
        }
        let role = effective_role(joined.role.as_deref(), self.segments.as_ref());
        let segmented = self
            .segments
            .as_ref()
            .is_some_and(|s| s.segments_role(&role));
        let label = joined
            .label
            .clone()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| format!("Participant {}", joined.index));
        let at = self.elapsed_ms();
        let window = self.tap_window_seconds();
        let participant = MeetingParticipant {
            participant_id: joined.participant_id.clone(),
            index: joined.index,
            label: label.chars().take(80).collect(),
            role,
            declared_role: joined.role.clone(),
            segmented,
            joined_at_ms: at,
            left_at_ms: None,
            left_reason: None,
            audio: ListenState::new(self.rate, self.record, self.max_seconds).with_window(window),
            transcript: None,
            speech: SpeechTracker::new(self.rate, vad),
            spans: Vec::new(),
            span_start: None,
            frames: Vec::new(),
            frames_regular: 0,
            frames_requested: 0,
            frames_dropped: 0,
            lines: Vec::new(),
            line_count: 0,
            tracks: BTreeMap::new(),
        };
        self.ids.insert(joined.participant_id.clone(), joined.index);
        self.participants.insert(joined.index, participant);
        self.participants.get_mut(&joined.index)
    }

    /// A participant left: its tap stops, its transcriber is told to finish.
    pub fn leave(&mut self, participant_id: &str, reason: Option<String>) -> Option<u16> {
        let at = self.elapsed_ms();
        let index = *self.ids.get(participant_id)?;
        let participant = self.participants.get_mut(&index)?;
        if participant.left_at_ms.is_some() {
            return None;
        }
        participant.audio.catch_up();
        participant.audio.frames = None;
        if let Some(start) = participant.span_start.take() {
            participant.spans.push((start, at));
        }
        participant.left_at_ms = Some(at);
        participant.left_reason = reason;
        Some(index)
    }

    /// One final transcript line of participant `index`, in session time.
    pub fn add_line(&mut self, index: u16, line: TranscriptLine) {
        if let Some(participant) = self.participants.get_mut(&index) {
            participant.line_count += 1;
            participant.lines.push(line);
        }
    }

    /// One audio frame of participant `index`. Answers the reason to ask for a frame of every
    /// participant now, if the audio flagged a moment and a request is due.
    pub fn on_audio(&mut self, index: u16, samples: &[i16]) -> Option<&'static str> {
        let now = self.elapsed_ms();
        let participant = self.participants.get_mut(&index)?;
        if participant.left_at_ms.is_some() {
            return None;
        }
        participant.audio.on_audio(samples);
        let changes = participant.speech.push(samples);
        let joined = participant.joined_at_ms;
        let mut reason = None;
        for (offset, change) in changes {
            let at = joined + offset;
            match change {
                SpeechChange::Started => {
                    participant.span_start = Some(at);
                    reason = Some("turn-boundary");
                }
                SpeechChange::Ended => {
                    if let Some(start) = participant.span_start.take() {
                        participant.spans.push((start, at));
                    }
                    reason = reason.or(Some("turn-boundary"));
                }
                SpeechChange::Loud => reason = Some("raised-energy"),
            }
        }
        let started_now = reason == Some("turn-boundary") && participant.speech.speaking;
        if started_now
            && self
                .participants
                .values()
                .any(|p| p.index != index && p.left_at_ms.is_none() && p.speech.speaking)
        {
            reason = Some("interruption");
        }
        // A long silence: nobody has spoken for dead-air ms (flagged once per silence).
        let anyone = self
            .participants
            .values()
            .any(|p| p.left_at_ms.is_none() && p.speech.speaking);
        if anyone {
            self.silence_since_ms = None;
            self.silence_flagged = false;
        } else {
            let since = *self.silence_since_ms.get_or_insert(now);
            if !self.silence_flagged && now >= since + self.dead_air_ms {
                self.silence_flagged = true;
                reason = reason.or(Some("long-silence"));
            }
        }
        let reason = reason?;
        self.flags.push(FlaggedMoment {
            at_ms: now,
            reason,
            index,
        });
        self.flag_count += 1;
        let due = self
            .last_sample_request_ms
            .is_none_or(|last| now >= last + SAMPLE_REQUEST_GAP_MS);
        if self.video.enabled && self.video.extra_frames && due {
            self.last_sample_request_ms = Some(now);
            Some(reason)
        } else {
            None
        }
    }

    /// One sampled frame of participant `index`.
    pub fn on_frame(&mut self, index: u16, data: &[u8], requested: bool) {
        let at = self.elapsed_ms();
        let bytes_left = MAX_FRAME_BYTES_PER_SESSION.saturating_sub(self.frame_bytes);
        let keep_frames = self.segments.as_ref().is_some_and(|s| s.max_frames > 0);
        let Some(participant) = self.participants.get_mut(&index) else {
            return;
        };
        if participant.left_at_ms.is_some() {
            return;
        }
        if requested {
            participant.frames_requested += 1;
        } else {
            participant.frames_regular += 1;
        }
        // A participant who is not cut into segments: counted, never kept.
        if !participant.segmented || !keep_frames {
            return;
        }
        if participant.frames.len() >= MAX_FRAMES_PER_PARTICIPANT || data.len() as u64 > bytes_left
        {
            participant.frames_dropped += 1;
            return;
        }
        self.frame_bytes += data.len() as u64;
        participant.frames.push(SampledFrame {
            at_ms: at,
            data: data.to_vec(),
            requested,
        });
    }

    /// Let go of what lies wholly before `upto_ms` (its segments have been written, or it is past
    /// every window): frames, transcript lines, voiced spans and flagged moments.
    pub fn release_before(&mut self, upto_ms: u64) {
        let mut released = 0u64;
        for participant in self.participants.values_mut() {
            participant.frames.retain(|f| {
                let keep = f.at_ms >= upto_ms;
                if !keep {
                    released += f.data.len() as u64;
                }
                keep
            });
            participant
                .lines
                .retain(|l| l.end_ms.max(l.start_ms + 1) > upto_ms);
            participant.spans.retain(|(_, end)| *end > upto_ms);
        }
        self.flags.retain(|f| f.at_ms >= upto_ms);
        self.frame_bytes = self.frame_bytes.saturating_sub(released);
    }

    /// Cut segments `[self.next_segment, …)` whose window ends by `end_ms` (the last one may be
    /// partial when `final_run`), advancing `next_segment`, then let go of what lies before the
    /// next one. Answers one input per segmented participant present in each segment — none when
    /// the session has no `call-segment` channel or [`Self::segments_allowed`] says no (those
    /// segments are skipped: answered in the count).
    pub fn take_segments(&mut self, end_ms: u64, final_run: bool) -> (Vec<SegmentInput>, u64) {
        let Some(settings) = self.segments.clone() else {
            // No segments: only let go of what is past every window.
            let window = self.tap_window_seconds() * 1000;
            self.release_before(end_ms.saturating_sub(window));
            return (Vec::new(), 0);
        };
        let seg = settings.segment_ms.max(1);
        let closed = if final_run {
            end_ms.div_ceil(seg)
        } else {
            end_ms / seg
        };
        let allowed = self.segments_allowed();
        let mut inputs = Vec::new();
        let mut skipped = 0;
        for segment in self.next_segment..closed {
            let start = segment * seg;
            let stop = ((segment + 1) * seg).min(end_ms);
            if stop <= start {
                continue;
            }
            if !allowed {
                skipped += 1;
                continue;
            }
            let words: Vec<Value> = self.words_in(start, stop);
            let present: Vec<Value> = self
                .participants
                .values()
                .filter(|p| p.present_in(start, stop))
                .map(|p| {
                    json!({
                        "participant": format!("P{}", p.index),
                        "role": p.role,
                        "talkMs": p.talk_ms_in(start, stop),
                    })
                })
                .collect();
            for participant in self.participants.values() {
                if !participant.segmented || !participant.present_in(start, stop) {
                    continue;
                }
                let samples = participant.audio_in(start, stop);
                let frames: Vec<SampledFrame> = participant
                    .frames
                    .iter()
                    .filter(|f| f.at_ms >= start && f.at_ms < stop)
                    .cloned()
                    .collect();
                let flags: Vec<Value> = self
                    .flags
                    .iter()
                    .filter(|f| f.at_ms >= start && f.at_ms < stop)
                    .map(|f| {
                        json!({"atMs": f.at_ms - start, "reason": f.reason, "participant": format!("P{}", f.index)})
                    })
                    .collect();
                let talk_ms = participant.talk_ms_in(start, stop);
                inputs.push(SegmentInput {
                    segment: segment + 1,
                    start_ms: start,
                    end_ms: stop,
                    participant_id: participant.participant_id.clone(),
                    index: participant.index,
                    label: participant.label.clone(),
                    role: participant.role.clone(),
                    declared_role: participant.declared_role.clone(),
                    speaking: talk_ms >= 500,
                    talk_ms,
                    samples,
                    rate: self.rate,
                    frames: pick_frames(frames, settings.max_frames),
                    energy_threshold: participant.speech.threshold,
                    words: words.clone(),
                    present: present.clone(),
                    flags,
                });
            }
        }
        self.next_segment = self.next_segment.max(closed);
        self.release_before(self.next_segment * seg);
        (inputs, skipped)
    }

    fn words_in(&self, start: u64, stop: u64) -> Vec<Value> {
        let mut words: Vec<(u64, Value)> = self
            .participants
            .values()
            .flat_map(|p| {
                p.lines
                    .iter()
                    .filter(move |l| l.start_ms < stop && l.end_ms.max(l.start_ms + 1) > start)
                    .map(move |l| {
                        (
                            l.start_ms,
                            json!({
                                "speaker": format!("P{}", p.index),
                                "startMs": l.start_ms.saturating_sub(start),
                                "endMs": l.end_ms.saturating_sub(start),
                                "text": l.text,
                            }),
                        )
                    })
            })
            .collect();
        words.sort_by_key(|(at, _)| *at);
        words.into_iter().map(|(_, w)| w).collect()
    }
}

/// At most `max` frames: every requested frame first (they mark the moments that matter), then
/// regular ones evenly spaced; in time order.
pub fn pick_frames(frames: Vec<SampledFrame>, max: usize) -> Vec<SampledFrame> {
    if frames.len() <= max {
        return frames;
    }
    let (mut requested, regular): (Vec<_>, Vec<_>) = frames.into_iter().partition(|f| f.requested);
    requested.truncate(max);
    let room = max - requested.len();
    if room > 0 && !regular.is_empty() {
        let step = regular.len() as f64 / room as f64;
        for i in 0..room {
            let at = ((i as f64 + 0.5) * step) as usize;
            if let Some(frame) = regular.get(at.min(regular.len() - 1)) {
                requested.push(frame.clone());
            }
        }
    }
    requested.sort_by_key(|f| f.at_ms);
    requested
}

/// One participant over one segment: its media (written to the store before the flow hears of
/// it) and the segment's generic measurements.
#[derive(Debug, Clone)]
pub struct SegmentInput {
    /// Counts from 1.
    pub segment: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub participant_id: String,
    pub index: u16,
    pub label: String,
    pub role: String,
    pub declared_role: Option<String>,
    pub speaking: bool,
    pub talk_ms: u64,
    pub samples: Vec<i16>,
    pub rate: u32,
    pub frames: Vec<SampledFrame>,
    /// The participant's voicing threshold, for [`acoustic_features`].
    pub energy_threshold: f64,
    /// The words spoken in the segment by anyone (pseudonymised speakers).
    pub words: Vec<Value>,
    /// Who was present (pseudonymised), with their talk time.
    pub present: Vec<Value>,
    pub flags: Vec<Value>,
}

/// The generic acoustic features of a stretch of one participant's audio (duration, voicing,
/// energy and pitch contours): the measurements a flow fuses with what a model says. CPU-bound
/// (a pitch estimate per voiced frame), so the hub runs it off the async workers and outside the
/// session's lock.
pub fn acoustic_features(samples: &[i16], rate: u32, energy_threshold: f64) -> Value {
    if samples.is_empty() {
        return Value::Null;
    }
    let vad = crate::vad::VadConfig::default();
    serde_json::to_value(analyze_turn(
        samples,
        rate,
        vad.frame_ms,
        energy_threshold,
        &PitchConfig::default(),
    ))
    .unwrap_or(Value::Null)
}
