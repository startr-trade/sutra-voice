//! Meeting sessions: one tagged media socket carrying every participant of a meeting — each
//! participant's own audio, and sampled video frames — from a gateway that joins the meeting as
//! a participant itself (a WebRTC room, say). See [`crate::protocol`] for the framing.
//!
//! Per participant the transport keeps what a listen-only call keeps for the whole call (a tap
//! of its audio, recorded at the end; a live transcription session, so every transcript segment
//! is attributed EXACTLY — it comes from that participant's own track, no diarization), plus
//! the sampled frames of its camera and a speech tracker that finds turn boundaries,
//! interruptions, raised energy and long silences. Those moments ask the gateway for an extra
//! frame of EVERY participant, so a listener's reaction is captured as well as the speaker's.
//!
//! **Analysis** (a `call-analysis` channel, [`AnalysisSettings`]): the session is cut into fixed
//! time segments; for each segment and each participant whose role is analysed, the
//! participant's audio of the segment, its frames and the words spoken in the segment go to a
//! model with video understanding — through the `llm` channel `analyze.channel` names, so the
//! channel's candidates, fallback, `require:` and limits apply — which answers one JSON object — expression,
//! engagement/attention, body-language cues, a fused emotion estimate naming the modalities that
//! support it and its confidence, and whether the modalities disagree. Each answer is one
//! `call-analysis` message. Analysis runs after the session (`when: post-call`, the default) or
//! as each segment closes (`when: periodic`).
//!
//! **Compliance, enforced here rather than left to each flow:** roles are what the participant
//! was admitted with (a gateway-declared attribute), never inferred; only roles listed in
//! `analyze.roles` (default `external`) are analysed, and a staff role is refused there unless
//! `analyze.allow-staff: true` — inferring the emotions of people at work is prohibited in the
//! EU (AI Act, Art. 5(1)(f)). A participant who is not analysed never has a frame or its audio
//! sent to a model. The model is told the participant only by a pseudonym (`P<index>`), is
//! asked not to identify anyone or infer sensitive attributes, and keys that would carry such
//! an inference are stripped from its answer. Frames live in memory only, until their segment
//! has been analysed or the session ends; they are never stored.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sutra_channels::{ChannelDefinition, Diagnostic};
use sutra_llm_spi::{
    LlmAudio, LlmChannelHandle, LlmFailure, LlmMessage, LlmRequest, LlmVideo, LlmVideoFrame,
};

use crate::audio;
use crate::codes;
use crate::features::{analyze_turn, frame_rms, PitchConfig};
use crate::listen::ListenState;

/// Roles that name people at work: never analysed unless a deployment opts in explicitly.
pub const STAFF_ROLES: [&str; 5] = ["staff", "internal", "employee", "agent", "host"];
/// The role of a participant the gateway declared none for, unless `analyze.default-role` says
/// otherwise.
pub const ROLE_UNKNOWN: &str = "unknown";
/// The role analysed by default.
pub const ROLE_EXTERNAL: &str = "external";
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
/// Keys stripped from a model's answer, at any depth: inferences no analysis may carry.
pub const FORBIDDEN_ANSWER_KEYS: [&str; 16] = [
    "age",
    "gender",
    "sex",
    "race",
    "ethnicity",
    "religion",
    "politics",
    "political",
    "sexualorientation",
    "health",
    "disability",
    "identity",
    "name",
    "nationality",
    "identifiedas",
    "biometricid",
];

fn is_staff_role(role: &str) -> bool {
    STAFF_ROLES.contains(&role)
}

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
    /// `record: true` — record the audio (per participant in a tagged session).
    pub record: bool,
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
        Ok(SessionSettings {
            listen_only,
            record: bool_property(definition, "record")?.unwrap_or(false),
            video,
        })
    }
}

/// When a session is analysed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisTiming {
    /// Every segment, after the session ended.
    PostCall,
    /// Each segment as it closes, while the session runs (the rest at the end).
    Periodic,
}

impl AnalysisTiming {
    pub fn as_str(&self) -> &'static str {
        match self {
            AnalysisTiming::PostCall => "post-call",
            AnalysisTiming::Periodic => "periodic",
        }
    }
}

/// The `call-analysis` channel's `analyze.*`: which model analyses which participants, how.
///
/// | Key | Meaning | Default |
/// |---|---|---|
/// | `analyze.channel` | the `llm` channel of this deployment that analyses (it must `require: [video-understanding]`; its candidates name the models, its `timeout-seconds` bounds a call) | required |
/// | `analyze.roles` | the roles analysed | `[external]` |
/// | `analyze.allow-staff` | permit a staff role (`staff`, `internal`, `employee`, `agent`, `host`) in `roles` — see the EU note on the module | `false` |
/// | `analyze.default-role` | the role of a participant the gateway declared none for | `unknown` |
/// | `analyze.segment-seconds` | the length of a segment (2–300) | `10` |
/// | `analyze.when` | `post-call` or `periodic` | `post-call` |
/// | `analyze.max-frames` | the most frames of one participant sent for one segment (1–32) | `8` |
/// | `analyze.audio` | send the participant's audio of the segment too, when a candidate of the channel takes audio (a candidate that cannot is then skipped for that call) | `true` |
#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisSettings {
    /// `analyze.channel`: the `llm` channel the analysis calls.
    pub channel: String,
    pub roles: Vec<String>,
    pub allow_staff: bool,
    pub default_role: String,
    pub segment_ms: u64,
    pub timing: AnalysisTiming,
    pub max_frames: usize,
    pub audio: bool,
}

/// Why a participant is, or is not, analysed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisPolicy {
    Analysed,
    /// The role is not among `analyze.roles`.
    RoleNotAnalysed,
    /// A staff role without `analyze.allow-staff`.
    StaffNotOptedIn,
    /// The deployment has no `call-analysis` channel.
    NoAnalysis,
}

impl AnalysisPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            AnalysisPolicy::Analysed => "analysed",
            AnalysisPolicy::RoleNotAnalysed => "role-not-analysed",
            AnalysisPolicy::StaffNotOptedIn => "staff-not-opted-in",
            AnalysisPolicy::NoAnalysis => "no-analysis-channel",
        }
    }
}

impl AnalysisSettings {
    pub fn parse(
        definition: &ChannelDefinition,
        definitions: &[ChannelDefinition],
    ) -> Result<Option<AnalysisSettings>, Diagnostic> {
        crate::config::refuse_direct_model_keys(definition, "analyze")?;
        let Some(channel) = property(definition, "analyze.channel") else {
            if definition
                .properties
                .keys()
                .any(|k| k.starts_with("analyze."))
            {
                return Err(Diagnostic::error(
                    codes::MODEL_CHANNEL_INVALID,
                    format!(
                        "channel '{}': analyze needs a 'channel' (an llm channel)",
                        definition.binding.channel_name
                    ),
                ));
            }
            return Ok(None);
        };
        crate::config::check_model_channel(
            definition,
            "analyze",
            channel,
            crate::config::ModelChannelUse::Analysis,
            definitions,
        )?;
        let roles = list_property(definition, "analyze.roles")
            .unwrap_or_else(|| vec![ROLE_EXTERNAL.to_string()]);
        let allow_staff = bool_property(definition, "analyze.allow-staff")?.unwrap_or(false);
        if let Some(staff) = roles.iter().find(|r| is_staff_role(r)) {
            if !allow_staff {
                return Err(invalid(
                    definition,
                    format!(
                        "analyze.roles names the staff role '{staff}'. Inferring the emotions of \
                         people at work is prohibited in the EU (AI Act, Art. 5(1)(f)); set \
                         analyze.allow-staff: true only where that prohibition does not apply \
                         and the people concerned opted in"
                    ),
                ));
            }
        }
        let segment_seconds =
            parsed_property::<u64>(definition, "analyze.segment-seconds")?.unwrap_or(10);
        if !(2..=300).contains(&segment_seconds) {
            return Err(invalid(
                definition,
                format!("analyze.segment-seconds must be 2-300, got {segment_seconds}"),
            ));
        }
        let timing = match property(definition, "analyze.when") {
            None => AnalysisTiming::PostCall,
            Some(w) if w.eq_ignore_ascii_case("post-call") => AnalysisTiming::PostCall,
            Some(w) if w.eq_ignore_ascii_case("periodic") => AnalysisTiming::Periodic,
            Some(other) => {
                return Err(invalid(
                    definition,
                    format!("analyze.when must be post-call or periodic, got '{other}'"),
                ))
            }
        };
        Ok(Some(AnalysisSettings {
            channel: channel.to_string(),
            roles,
            allow_staff,
            default_role: property(definition, "analyze.default-role")
                .map(|r| r.to_ascii_lowercase())
                .unwrap_or_else(|| ROLE_UNKNOWN.to_string()),
            segment_ms: segment_seconds * 1000,
            timing,
            max_frames: parsed_property::<usize>(definition, "analyze.max-frames")?
                .unwrap_or(8)
                .clamp(1, 32),
            audio: bool_property(definition, "analyze.audio")?.unwrap_or(true),
        }))
    }

    /// Whether `role` is analysed, and why (not).
    pub fn policy_for(&self, role: &str) -> AnalysisPolicy {
        if is_staff_role(role) && !self.allow_staff {
            AnalysisPolicy::StaffNotOptedIn
        } else if self.roles.iter().any(|r| r == role) {
            AnalysisPolicy::Analysed
        } else {
            AnalysisPolicy::RoleNotAnalysed
        }
    }
}

/// The role a participant is treated as: the declared one, else the default.
pub fn effective_role(declared: Option<&str>, analysis: Option<&AnalysisSettings>) -> String {
    match declared.map(str::trim).filter(|r| !r.is_empty()) {
        Some(role) => role.to_ascii_lowercase(),
        None => analysis
            .map(|a| a.default_role.clone())
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
    pub policy: AnalysisPolicy,
    pub joined_at_ms: u64,
    pub left_at_ms: Option<u64>,
    pub left_reason: Option<String>,
    /// The participant's audio since it joined (offsets relative to `joined_at_ms`), with the
    /// frames channel to its transcriber.
    pub audio: ListenState,
    /// The transcriber's summary, once it finished.
    pub transcript: Option<tokio::sync::oneshot::Receiver<Value>>,
    pub speech: SpeechTracker,
    /// Voiced stretches, session time.
    pub spans: Vec<(u64, u64)>,
    span_start: Option<u64>,
    pub frames: Vec<SampledFrame>,
    pub frames_regular: u64,
    pub frames_requested: u64,
    pub frames_dropped: u64,
    pub lines: Vec<TranscriptLine>,
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

    /// The participant's audio in `[start, end)` of session time.
    pub fn audio_in(&self, start_ms: u64, end_ms: u64) -> Vec<i16> {
        let rate = u64::from(self.audio.rate.max(1));
        let from = start_ms.saturating_sub(self.joined_at_ms) * rate / 1000;
        let to = end_ms.saturating_sub(self.joined_at_ms) * rate / 1000;
        let (from, to) = (
            (from as usize).min(self.audio.tap.len()),
            (to as usize).min(self.audio.tap.len()),
        );
        self.audio.tap[from..to].to_vec()
    }
}

/// A meeting session's state (inside the call, behind a lock).
pub struct MeetingState {
    started: Instant,
    pub rate: u32,
    pub record: bool,
    pub video: VideoPolicy,
    pub participants: BTreeMap<u16, MeetingParticipant>,
    pub ids: HashMap<String, u16>,
    pub consent_at_ms: Option<u64>,
    pub flags: Vec<FlaggedMoment>,
    last_sample_request_ms: Option<u64>,
    silence_since_ms: Option<u64>,
    silence_flagged: bool,
    pub dead_air_ms: u64,
    pub frame_bytes: u64,
    /// The first segment not yet analysed.
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
            started: Instant::now(),
            rate,
            record,
            video,
            participants: BTreeMap::new(),
            ids: HashMap::new(),
            consent_at_ms: None,
            flags: Vec::new(),
            last_sample_request_ms: None,
            silence_since_ms: None,
            silence_flagged: false,
            dead_air_ms: dead_air_ms.max(500),
            frame_bytes: 0,
            next_segment: 0,
            max_seconds,
        }
    }

    /// Ms since the session started.
    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn participant_by_id(&self, id: &str) -> Option<&MeetingParticipant> {
        self.ids.get(id).and_then(|i| self.participants.get(i))
    }

    /// Add a participant (a repeated join of a known id is ignored: `None`).
    pub fn join(
        &mut self,
        joined: &crate::protocol::ParticipantJoined,
        analysis: Option<&AnalysisSettings>,
        vad: &crate::vad::VadConfig,
    ) -> Option<&mut MeetingParticipant> {
        if self.ids.contains_key(&joined.participant_id)
            || self.participants.contains_key(&joined.index)
        {
            return None;
        }
        let role = effective_role(joined.role.as_deref(), analysis);
        let policy = analysis
            .map(|a| a.policy_for(&role))
            .unwrap_or(AnalysisPolicy::NoAnalysis);
        let label = joined
            .label
            .clone()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| format!("Participant {}", joined.index));
        let at = self.elapsed_ms();
        let participant = MeetingParticipant {
            participant_id: joined.participant_id.clone(),
            index: joined.index,
            label: label.chars().take(80).collect(),
            role,
            declared_role: joined.role.clone(),
            policy,
            joined_at_ms: at,
            left_at_ms: None,
            left_reason: None,
            audio: ListenState::new(self.rate, self.record, self.max_seconds),
            transcript: None,
            speech: SpeechTracker::new(self.rate, vad),
            spans: Vec::new(),
            span_start: None,
            frames: Vec::new(),
            frames_regular: 0,
            frames_requested: 0,
            frames_dropped: 0,
            lines: Vec::new(),
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
        // A participant who is not analysed: counted, never kept.
        if participant.policy != AnalysisPolicy::Analysed {
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

    /// Drop the frames before `upto_ms` (their segments have been analysed).
    pub fn release_frames_before(&mut self, upto_ms: u64) {
        let mut released = 0u64;
        for participant in self.participants.values_mut() {
            participant.frames.retain(|f| {
                let keep = f.at_ms >= upto_ms;
                if !keep {
                    released += f.data.len() as u64;
                }
                keep
            });
        }
        self.frame_bytes = self.frame_bytes.saturating_sub(released);
    }

    /// The analysis inputs of segments `[self.next_segment, upto)` whose window ends by
    /// `end_ms` (the last one may be partial when `final_run`), advancing `next_segment`.
    pub fn take_segments(
        &mut self,
        settings: &AnalysisSettings,
        end_ms: u64,
        final_run: bool,
    ) -> Vec<SegmentInput> {
        let seg = settings.segment_ms.max(1);
        let closed = if final_run {
            end_ms.div_ceil(seg)
        } else {
            end_ms / seg
        };
        let mut inputs = Vec::new();
        for segment in self.next_segment..closed {
            let start = segment * seg;
            let stop = ((segment + 1) * seg).min(end_ms);
            if stop <= start {
                continue;
            }
            let words: Vec<Value> = self.words_in(start, stop);
            let others: Vec<Value> = self
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
                if participant.policy != AnalysisPolicy::Analysed
                    || !participant.present_in(start, stop)
                {
                    continue;
                }
                let samples = participant.audio_in(start, stop);
                let frames: Vec<SampledFrame> = participant
                    .frames
                    .iter()
                    .filter(|f| f.at_ms >= start && f.at_ms < stop)
                    .cloned()
                    .collect();
                let features = if samples.is_empty() {
                    Value::Null
                } else {
                    let vad = crate::vad::VadConfig::default();
                    serde_json::to_value(analyze_turn(
                        &samples,
                        self.rate,
                        vad.frame_ms,
                        participant.speech.threshold,
                        &PitchConfig::default(),
                    ))
                    .unwrap_or(Value::Null)
                };
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
                    speaking: talk_ms >= 500,
                    talk_ms,
                    samples,
                    rate: self.rate,
                    frames: pick_frames(frames, settings.max_frames),
                    features,
                    words: words.clone(),
                    present: others.clone(),
                    flags,
                });
            }
        }
        self.next_segment = self.next_segment.max(closed);
        inputs
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

/// Everything one model call gets for one participant and one segment.
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
    pub speaking: bool,
    pub talk_ms: u64,
    pub samples: Vec<i16>,
    pub rate: u32,
    pub frames: Vec<SampledFrame>,
    pub features: Value,
    /// The words spoken in the segment by anyone (pseudonymised speakers).
    pub words: Vec<Value>,
    /// Who was present (pseudonymised), with their talk time.
    pub present: Vec<Value>,
    pub flags: Vec<Value>,
}

/// The analysis instructions (the system prompt).
pub const ANALYSIS_INSTRUCTIONS: &str = "You analyse ONE participant of a recorded meeting over \
ONE time segment, from that participant's own audio and sampled camera frames, and the words \
spoken in the segment by everyone present. Participants are named only by pseudonyms (P1, P2, \
...). Report observable cues only: facial expression, gaze and attention, nods and head shakes, \
posture and posture shifts, gestures, fidgeting, turning away; the voice's tone and energy; and \
the participant's reaction to what others said, even while listening. Never identify the person \
and never estimate age, gender, ethnicity, health, religion, political or sexual orientation or \
any other sensitive attribute; do not describe appearance beyond those cues. Answer with ONE \
JSON object: {\"expression\": string, \"engagement\": \"high\"|\"medium\"|\"low\", \
\"attention\": string, \"bodyLanguage\": [string], \"emotion\": {\"label\": string, \
\"confidence\": number 0..1, \"modalities\": {\"voice\": bool, \"face\": bool, \"body\": bool, \
\"words\": bool}}, \"disagreement\": {\"flagged\": bool, \"detail\": string or null}, \
\"reactions\": [{\"atMs\": number, \"to\": string, \"cue\": string}]}. The modalities say which \
signals support the emotion estimate. When the signals disagree (a calm voice with tense \
posture and averted gaze, say) set disagreement.flagged and describe it - never average it \
away. A missing signal (no frames, no speech) is false, not guessed. The estimate is advisory, \
for people to review.";

/// The model request for `input`.
pub fn analysis_request(
    input: &SegmentInput,
    settings: &AnalysisSettings,
    audio_input: bool,
) -> LlmRequest {
    let context = json!({
        "participant": format!("P{}", input.index),
        "role": input.role,
        "segment": {"index": input.segment, "startMs": input.start_ms, "endMs": input.end_ms},
        "speaking": input.speaking,
        "talkMs": input.talk_ms,
        "present": input.present,
        "words": input.words,
        "acoustic": input.features,
        "flaggedMoments": input.flags,
        "frames": input.frames.len(),
        "audio": audio_input && settings.audio && !input.samples.is_empty(),
    });
    let mut message = LlmMessage::user(format!(
        "Analyse participant P{} over this segment. Context (JSON): {context}",
        input.index
    ));
    if audio_input && settings.audio && !input.samples.is_empty() {
        message.audio = Some(LlmAudio {
            data: audio::wav_wrap(&input.samples, input.rate),
            format: sutra_llm_spi::AudioFormat::Wav,
        });
    }
    if !input.frames.is_empty() {
        message.video = Some(LlmVideo::Frames(
            input
                .frames
                .iter()
                .map(|f| LlmVideoFrame {
                    data: f.data.clone(),
                    media_type: DEFAULT_FRAME_FORMAT.to_string(),
                    at_ms: Some(f.at_ms.saturating_sub(input.start_ms)),
                })
                .collect(),
        ));
    }
    LlmRequest {
        // The channel's candidates name the model, and its timeout bounds the call.
        model: String::new(),
        system: Some(ANALYSIS_INSTRUCTIONS.to_string()),
        messages: vec![message],
        tools: Vec::new(),
        json_output: true,
        max_output_tokens: Some(1024),
        temperature: Some(0.0),
        deadline: Duration::ZERO,
        audio_output: None,
        output_voice: None,
    }
}

/// Parse a model's answer: a JSON object, possibly inside a fenced block.
pub fn parse_answer(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.trim_end().strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();
    match serde_json::from_str::<Value>(unfenced) {
        Ok(value @ Value::Object(_)) => Some(value),
        _ => {
            let (open, close) = (unfenced.find('{')?, unfenced.rfind('}')?);
            serde_json::from_str::<Value>(unfenced.get(open..=close)?)
                .ok()
                .filter(Value::is_object)
        }
    }
}

/// Remove every key of [`FORBIDDEN_ANSWER_KEYS`] (case- and separator-insensitive), at any
/// depth; answers the paths removed.
pub fn strip_forbidden(value: &mut Value) -> Vec<String> {
    fn walk(value: &mut Value, path: &str, removed: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                let doomed: Vec<String> = map
                    .keys()
                    .filter(|k| {
                        let folded: String = k
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric())
                            .collect::<String>()
                            .to_ascii_lowercase();
                        FORBIDDEN_ANSWER_KEYS.contains(&folded.as_str())
                    })
                    .cloned()
                    .collect();
                for key in doomed {
                    map.remove(&key);
                    removed.push(format!("{path}{key}"));
                }
                for (key, nested) in map.iter_mut() {
                    walk(nested, &format!("{path}{key}."), removed);
                }
            }
            Value::Array(items) => {
                for (i, nested) in items.iter_mut().enumerate() {
                    walk(nested, &format!("{path}{i}."), removed);
                }
            }
            _ => {}
        }
    }
    let mut removed = Vec::new();
    walk(value, "", &mut removed);
    removed
}

/// The fields every answer should carry.
pub const ANSWER_FIELDS: [&str; 6] = [
    "expression",
    "engagement",
    "attention",
    "bodyLanguage",
    "emotion",
    "disagreement",
];

/// The analysis model: the `llm` channel the analysis calls, plus whether a candidate of it
/// takes audio.
pub struct AnalysisModel {
    pub channel: Arc<dyn LlmChannelHandle>,
    pub audio_input: bool,
}

/// Resolve the `llm` channel `settings` name in the deployment `module_key`, or the reason it
/// cannot be used.
pub fn analysis_model(
    settings: &AnalysisSettings,
    module_key: &str,
) -> Result<AnalysisModel, String> {
    let channel = sutra_llm_spi::llm_channels()
        .get(module_key, &settings.channel)
        .ok_or_else(|| {
            format!(
                "the llm channel '{}' is not live (see the llm transport's load diagnostics)",
                settings.channel
            )
        })?;
    let capabilities = channel.capabilities();
    if !capabilities.video_understanding {
        return Err(format!(
            "no candidate of the llm channel '{}' has video understanding",
            settings.channel
        ));
    }
    Ok(AnalysisModel {
        audio_input: capabilities.audio_input,
        channel,
    })
}

/// Analyse one input: the `call-analysis` body (never carrying the media).
pub async fn analyse(
    model: &AnalysisModel,
    settings: &AnalysisSettings,
    call_id: &str,
    input: &SegmentInput,
) -> Value {
    let request = analysis_request(input, settings, model.audio_input);
    let mut outcome = model.channel.complete(&request).await;
    if let Err(LlmFailure::Retryable { retry_after, .. }) = &outcome {
        tokio::time::sleep(
            retry_after
                .unwrap_or(Duration::from_secs(1))
                .min(Duration::from_secs(10)),
        )
        .await;
        outcome = model.channel.complete(&request).await;
    }
    let mut body = json!({
        "callId": call_id,
        "key": format!("{call_id}:{}:{}", input.index, input.segment),
        "participantId": input.participant_id,
        "participantIndex": input.index,
        "label": input.label,
        "role": input.role,
        "segment": input.segment,
        "startMs": input.start_ms,
        "endMs": input.end_ms,
        "speaking": input.speaking,
        "talkMs": input.talk_ms,
        "frames": input.frames.len(),
        "framesRequested": input.frames.iter().filter(|f| f.requested).count(),
        "audioMs": input.samples.len() as u64 * 1000 / u64::from(input.rate.max(1)),
        "audioSent": model.audio_input && settings.audio && !input.samples.is_empty(),
        "features": input.features,
        "flaggedMoments": input.flags,
        "channel": settings.channel,
        "provider": Value::Null,
        "model": Value::Null,
    });
    if let Ok(answer) = &outcome {
        body["provider"] = json!(answer.provider);
        body["model"] = json!(answer.model);
    }
    match outcome {
        Ok(answer) => match parse_answer(&answer.response.text) {
            Some(mut parsed) => {
                let removed = strip_forbidden(&mut parsed);
                let missing: Vec<&str> = ANSWER_FIELDS
                    .iter()
                    .copied()
                    .filter(|f| parsed.get(*f).is_none())
                    .collect();
                body["status"] = json!("complete");
                body["result"] = parsed;
                body["removedKeys"] = json!(removed);
                body["missingFields"] = json!(missing);
            }
            None => {
                body["status"] = json!("failed");
                body["detail"] = json!("the model's answer is not a JSON object");
            }
        },
        Err(failure) => {
            tracing::warn!(
                call_id,
                segment = input.segment,
                index = input.index,
                "voip: segment analysis failed: {}",
                failure.detail()
            );
            body["status"] = json!("failed");
            body["detail"] = json!(failure.detail());
        }
    }
    body
}
