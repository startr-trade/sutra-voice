//! Listen-only calls: the media plane of a call the flow only listens to (a meeting, say).
//!
//! The far end's audio is forked to two consumers, and neither goes through the flow:
//!
//! - **the recorder** — the call's audio (the far end, with the transport's own prompts mixed in
//!   where they were played) streamed to the deployment's object store while the call is up,
//!   sealed chunk by chunk ([`crate::recording`]). The flow receives a media REFERENCE on the
//!   `call-hangup` message (`recording`: uri, SHA-256 of the stored object, duration, format,
//!   whether it is complete, whether the consent announcement was played) — never the media.
//! - **the transcriber** — a live audio session ([`sutra_llm_spi::AudioSession`]) opened
//!   transcription-only through the `llm` channel `transcribe.channel` names (its first
//!   candidate with live sessions, with the channel's fallback); each final transcript
//!   segment is delivered on the `call-transcript` channel (one message per segment, to a
//!   process that stores it), and a segment containing a configured phrase raises a
//!   `call-keyword` message into the call's own instance.
//!
//! **Speakers.** The live sessions this transport opens (OpenAI Realtime, Gemini Live) do not
//! label speakers, and a conference dial-in delivers ONE mixed audio channel, so every segment
//! says `speaker: "unknown"` with `speakerSource: "none"` unless the provider sets one.
//! Attribution is left to the report (a model asked to attribute the transcript to the
//! participants), and marked there as inferred.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde_json::{json, Value};
use sutra_llm_spi::{AudioSessionConfig, AudioSessionEvent, TurnDetection};
use tokio::sync::{mpsc, oneshot};

use crate::audio::{self, AudioFormat};
use crate::config::{EventKind, TranscribeSettings};
use crate::features::{analyze_turn, PitchConfig};
use crate::hub::{Call, VoipHub};
use crate::recording::{FinishedRecording, RecordingWriter};
pub use crate::recording::{Recorder, RECORDING_FORMAT};

/// The `kind` a media reference carries (what `<q:audit capture="media">` collects).
pub const MEDIA_REFERENCE_KIND: &str = "media";
/// What `speaker` says when the provider labels none.
pub const UNKNOWN_SPEAKER: &str = "unknown";
/// How much of a listen-only call's audio is kept in memory for transcript-segment features.
pub const TAP_WINDOW_SECONDS: u64 = 120;

/// A listen-only call's media-plane state (inside the call, behind a lock).
pub struct ListenState {
    /// The far end's most recent audio, at `rate`: samples `[tap_base, tap_base + tap.len())`
    /// of the call (counted from when the media connected).
    pub tap: Vec<i16>,
    pub tap_base: u64,
    /// The most samples the tap keeps.
    pub window_samples: usize,
    pub rate: u32,
    /// Whether the audio is to be recorded (the request was marked `capture=media`, the
    /// channel says `record:`, or the flow asked with `record-start`).
    pub record: bool,
    /// The recording waits for consent (`record: after-consent`).
    pub record_after_consent: bool,
    /// The recording in progress, and where in the call it started (samples).
    pub recording: Option<RecordingWriter>,
    pub recording_origin: u64,
    /// Recordings already finished (a `record-stop`), as media references.
    pub finished: Vec<Value>,
    /// Why a recording could not start, when it could not.
    pub recording_error: Option<String>,
    /// The longest recording kept.
    pub max_seconds: u64,
    /// Frames to the transcriber (`None` when the call is not transcribed).
    pub frames: Option<mpsc::UnboundedSender<Vec<i16>>>,
    /// The transcriber's result, once it has finished.
    pub transcript: Option<oneshot::Receiver<Value>>,
    /// Whether the consent announcement was played to the end, and where (call time).
    pub consent_at_ms: Option<u64>,
    /// Whether audio beyond the limit was dropped.
    pub truncated: bool,
    /// When the media connected: the tap is kept aligned to this clock.
    started: std::time::Instant,
}

/// How far the tap may fall behind the call's clock before the gap is filled with silence (a
/// gateway sends no frames while the far end sends no RTP, e.g. a muted conference).
pub const TAP_SLACK_MS: u64 = 200;

impl ListenState {
    pub fn new(rate: u32, record: bool, max_seconds: u64) -> ListenState {
        ListenState {
            tap: Vec::new(),
            tap_base: 0,
            window_samples: (max_seconds.saturating_mul(u64::from(rate))) as usize,
            rate,
            record,
            record_after_consent: false,
            recording: None,
            recording_origin: 0,
            finished: Vec::new(),
            recording_error: None,
            max_seconds,
            frames: None,
            transcript: None,
            consent_at_ms: None,
            truncated: false,
            started: std::time::Instant::now(),
        }
    }

    /// Keep only the last `seconds` of audio in memory.
    pub fn with_window(mut self, seconds: u64) -> ListenState {
        self.window_samples = (seconds.saturating_mul(u64::from(self.rate))) as usize;
        self
    }

    /// Samples since the media connected.
    pub fn position(&self) -> u64 {
        self.tap_base + self.tap.len() as u64
    }

    /// The recording's position for call position `at`, when it was recording then.
    pub fn recording_position(&self, at: u64) -> Option<u64> {
        self.recording
            .as_ref()
            .and_then(|_| at.checked_sub(self.recording_origin))
    }

    fn keep(&mut self, samples: &[i16]) {
        self.tap.extend_from_slice(samples);
        if self.tap.len() > self.window_samples {
            let drop = self.tap.len() - self.window_samples;
            self.tap.drain(..drop);
            self.tap_base += drop as u64;
        }
        if let Some(writer) = self.recording.as_mut() {
            writer.push(samples);
            if writer.truncated {
                self.truncated = true;
            }
        }
    }

    /// Fill the tap with silence up to the call's clock when it has fallen behind by more than
    /// [`TAP_SLACK_MS`] (the transcriber hears the silence too, so its offsets stay offsets
    /// into the recording).
    pub fn catch_up(&mut self) {
        let rate = u64::from(self.rate.max(1));
        let due = self.started.elapsed().as_millis() as u64 * rate / 1000;
        let have = self.position();
        if have + TAP_SLACK_MS * rate / 1000 >= due {
            return;
        }
        let gap = (due - have) as usize;
        self.keep(&vec![0; gap]);
        if let Some(frames) = &self.frames {
            let frame = (rate / 50) as usize;
            for _ in 0..gap / frame.max(1) {
                let _ = frames.send(vec![0; frame]);
            }
        }
    }

    /// Keep one frame of the far end's audio and hand it to the transcriber.
    pub fn on_audio(&mut self, samples: &[i16]) {
        self.catch_up();
        self.keep(samples);
        if let Some(frames) = &self.frames {
            let _ = frames.send(samples.to_vec());
        }
    }

    /// Milliseconds of audio so far.
    pub fn elapsed_ms(&self) -> u64 {
        self.position() * 1000 / u64::from(self.rate.max(1))
    }

    /// The far end's audio in `[from, to)` (call samples), as far as the tap still holds it.
    pub fn tap_between(&self, from: u64, to: u64) -> &[i16] {
        let from = from
            .saturating_sub(self.tap_base)
            .min(self.tap.len() as u64) as usize;
        let to = to.saturating_sub(self.tap_base).min(self.tap.len() as u64) as usize;
        &self.tap[from..to.max(from)]
    }

    /// Start recording now (`segments` name the object), unless a recording is in progress.
    pub fn start_recording(
        &mut self,
        recorder: &Arc<Recorder>,
        module_key: &str,
        segments: &[&str],
        owner: crate::recording::RecordingOwner,
    ) -> Result<(), String> {
        if self.recording.is_some() {
            return Ok(());
        }
        self.catch_up();
        match recorder.begin(module_key, segments, self.rate, owner) {
            Ok(writer) => {
                self.recording = Some(writer);
                self.recording_origin = self.position();
                self.record = true;
                self.recording_error = None;
                Ok(())
            }
            Err(e) => {
                self.recording_error = Some(e.clone());
                Err(e)
            }
        }
    }

    /// The recording's media reference template (what the finished object adds to).
    fn reference_of(&self, writer: &RecordingWriter, call_id: &str) -> Value {
        json!({
            "kind": MEDIA_REFERENCE_KIND,
            "uri": writer.uri,
            "durationMs": writer.duration_ms(),
            "format": RECORDING_FORMAT,
            "rate": writer.rate(),
            "channels": 1,
            "encryption": writer.key_id.as_ref().map(|id| json!({
                "scheme": crate::recording::SEALED_STREAM_SCHEME,
                "cipher": crate::recording::CIPHER,
                "keyId": id,
            })),
            "consentAnnounced": self.consent_at_ms.is_some(),
            // Relative to the recording's own start (negative: announced before it began).
            "consentAnnouncedAtMs": self.consent_at_ms.map(|at| {
                at as i64 - (self.recording_origin * 1000 / u64::from(self.rate.max(1))) as i64
            }),
            "startMs": self.recording_origin * 1000 / u64::from(self.rate.max(1)),
            "truncated": writer.truncated,
            "callId": call_id,
        })
    }

    /// End the recording in progress: answers its reference template and the receiver of the
    /// stored outcome (await it outside the lock, then [`complete_reference`]).
    pub fn stop_recording(&mut self, call_id: &str) -> Option<PendingRecording> {
        self.catch_up();
        let writer = self.recording.take()?;
        let reference = self.reference_of(&writer, call_id);
        Some(PendingRecording {
            reference,
            done: writer.finish(),
        })
    }
}

/// A recording being finished.
pub struct PendingRecording {
    pub reference: Value,
    pub done: oneshot::Receiver<Result<FinishedRecording, String>>,
}

impl PendingRecording {
    /// The recording's media reference, once it is stored (or why it is not).
    pub async fn reference(self, call_id: &str) -> Value {
        let mut reference = self.reference;
        match self.done.await {
            Ok(Ok(stored)) => {
                reference["uri"] = json!(stored.uri);
                reference["sha256"] = json!(stored.sha256);
                reference["bytes"] = json!(stored.bytes);
                reference["complete"] = json!(true);
                reference
            }
            Ok(Err(detail)) => {
                tracing::warn!(call_id = %call_id, "voip: the recording could not be stored: {detail}");
                json!({"status": "failed", "detail": detail, "uri": reference["uri"]})
            }
            Err(_) => json!({"status": "failed", "detail": "the recording's uploader stopped"}),
        }
    }
}

/// The recording outcome of a listen-only / recorded call at its end: the reference of the
/// recording in progress (finished now), else the last finished one, else why there is none.
pub async fn finish_recording(
    recorder: Option<&Recorder>,
    call: &Call,
    state: &mut ListenState,
) -> (Value, Vec<Value>) {
    if let Some(pending) = state.stop_recording(&call.call_id) {
        let reference = pending.reference(&call.call_id).await;
        state.finished.push(reference);
    }
    let all = state.finished.clone();
    let last = match all.last() {
        Some(last) => last.clone(),
        None if recorder.is_none() => {
            json!({"status": "unavailable", "detail": "no recording store is configured"})
        }
        None if state.recording_error.is_some() => {
            json!({"status": "failed", "detail": state.recording_error})
        }
        None if state.record_after_consent => {
            json!({"status": "not-started", "detail": "consent was never given, so nothing was recorded"})
        }
        None => json!({"status": "not-started", "detail": "the recording never started"}),
    };
    (last, all)
}

/// Whose track a transcriber hears (a meeting session's participant): its segments are
/// attributed to that participant exactly, and their offsets moved to session time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerTrack {
    pub participant_id: String,
    pub index: u16,
    pub label: String,
    /// When the participant's audio began, in session time.
    pub offset_ms: u64,
}

/// Which channel, and which of its candidates, a transcriber's session runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriberSource {
    pub channel: String,
    pub provider: String,
    pub model: String,
}

/// One transcript segment in the making.
#[derive(Default)]
struct Pending {
    start_ms: Option<u64>,
    end_ms: Option<u64>,
}

/// Start the transcriber of `call`: frames in through the returned sender, the summary out
/// through the receiver once the frames channel closes and the session has drained.
pub fn spawn_transcriber(
    hub: Weak<VoipHub>,
    call: Arc<Call>,
    settings: TranscribeSettings,
    phrases: Vec<String>,
    rate: u32,
    module_key: String,
    track: Option<SpeakerTrack>,
) -> (mpsc::UnboundedSender<Vec<i16>>, oneshot::Receiver<Value>) {
    let (frames_tx, frames_rx) = mpsc::unbounded_channel();
    let (done_tx, done_rx) = oneshot::channel();
    tokio::spawn(async move {
        let summary = transcribe(
            hub, call, settings, phrases, rate, module_key, track, frames_rx,
        )
        .await;
        let _ = done_tx.send(summary);
    });
    (frames_tx, done_rx)
}

#[allow(clippy::too_many_arguments)]
async fn transcribe(
    hub: Weak<VoipHub>,
    call: Arc<Call>,
    settings: TranscribeSettings,
    phrases: Vec<String>,
    rate: u32,
    module_key: String,
    track: Option<SpeakerTrack>,
    mut frames: mpsc::UnboundedReceiver<Vec<i16>>,
) -> Value {
    let mut base = json!({
        "channel": settings.channel,
        "provider": Value::Null,
        "model": Value::Null,
        "speakerLabels": if track.is_some() { "track" } else { "none" },
    });
    if let Some(track) = &track {
        base["participantId"] = json!(track.participant_id);
    }
    let fail = |base: &Value, status: &str, detail: String| {
        tracing::warn!(call_id = %call.call_id, channel = %settings.channel, "voip: transcription {status}: {detail}");
        let mut v = base.clone();
        v["status"] = json!(status);
        v["detail"] = json!(detail);
        v["segments"] = json!(0);
        v
    };
    let Some(channel) = sutra_llm_spi::llm_channels().get(&module_key, &settings.channel) else {
        return fail(
            &base,
            "unavailable",
            format!(
                "the llm channel '{}' is not live (see the llm transport's load diagnostics)",
                settings.channel
            ),
        );
    };
    let input = sutra_llm_spi::AudioFormat::L16 { rate, channels: 1 };
    let opened = match channel
        .open_audio_session(&AudioSessionConfig {
            // The channel's candidate names the model (and its transcription model).
            model: String::new(),
            instructions: None,
            input,
            output: None,
            voice: None,
            transcription_model: None,
            turn_detection: TurnDetection::Server,
            transcription_only: true,
            language: settings.language.clone(),
            connect_deadline: Duration::from_secs(10),
        })
        .await
    {
        Ok(s) => s,
        Err(e) => return fail(&base, "unavailable", e.detail().to_string()),
    };
    base["provider"] = json!(opened.provider);
    base["model"] = json!(opened.model);
    let source = TranscriberSource {
        channel: settings.channel.clone(),
        provider: opened.provider.clone(),
        model: opened.model.clone(),
    };
    let session = opened.session;

    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut sent_samples: u64 = 0;
    let mut segments: u64 = 0;
    let mut keywords: u64 = 0;
    let mut ended = false;
    let mut errors = 0u64;
    let drain = tokio::time::sleep(Duration::from_secs(86_400 * 365));
    tokio::pin!(drain);
    let audio_ms = |samples: u64| samples * 1000 / u64::from(rate.max(1));
    loop {
        tokio::select! {
            frame = frames.recv(), if !ended => match frame {
                Some(samples) => {
                    sent_samples += samples.len() as u64;
                    let bytes = audio::pcm16_to_le(&samples);
                    if let Err(e) = session.send_audio(&bytes).await {
                        errors += 1;
                        if errors == 1 {
                            tracing::warn!(call_id = %call.call_id, "voip: transcription send failed: {}", e.detail());
                        }
                    }
                }
                None => {
                    ended = true;
                    let _ = session.commit().await;
                    drain.as_mut().reset(
                        tokio::time::Instant::now() + Duration::from_millis(settings.drain_ms),
                    );
                }
            },
            event = session.next_event() => {
                let Some(event) = event else { break };
                match event {
                    AudioSessionEvent::SpeechStarted { item_id, audio_ms: at } => {
                        pending.entry(item_id).or_default().start_ms = Some(at);
                    }
                    AudioSessionEvent::SpeechStopped { item_id, audio_ms: at } => {
                        pending.entry(item_id).or_default().end_ms = Some(at);
                    }
                    AudioSessionEvent::InputTranscript { item_id, text, is_final, speaker } => {
                        let now = audio_ms(sent_samples);
                        let entry = pending.entry(item_id.clone()).or_default();
                        if entry.start_ms.is_none() {
                            entry.start_ms = Some(now);
                        }
                        if !is_final {
                            continue;
                        }
                        let Pending { start_ms, end_ms } =
                            pending.remove(&item_id).unwrap_or_default();
                        let text = text.trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        let start = start_ms.unwrap_or(now);
                        let end = end_ms.unwrap_or(now).max(start);
                        segments += 1;
                        let Some(hub) = hub.upgrade() else { break };
                        let (seq, at) = hub.deliver_transcript_segment(
                            &call, &item_id, &text, speaker.as_deref(), start, end,
                            &source, track.as_ref(),
                        );
                        let lowered = text.to_lowercase();
                        for phrase in phrases.iter().filter(|p| lowered.contains(p.as_str())) {
                            keywords += 1;
                            hub.deliver_keyword(&call, phrase, seq, at, &text);
                        }
                    }
                    AudioSessionEvent::Error { detail } => {
                        errors += 1;
                        tracing::warn!(call_id = %call.call_id, "voip: transcription session error: {detail}");
                    }
                    AudioSessionEvent::Closed => break,
                    // A transcription session's own answers (a provider without a listen-only
                    // mode) are not the call's business.
                    _ => {}
                }
                if ended && pending.is_empty() {
                    break;
                }
            },
            _ = &mut drain, if ended => break,
        }
    }
    session.close().await;
    let mut summary = base;
    summary["status"] = json!("complete");
    summary["segments"] = json!(segments);
    summary["keywords"] = json!(keywords);
    summary["audioMs"] = json!(audio_ms(sent_samples));
    summary["errors"] = json!(errors);
    summary
}

/// The acoustic features of `[start_ms, end_ms)` of the tap, when it holds that audio.
pub fn segment_features(state: &ListenState, start_ms: u64, end_ms: u64) -> Value {
    let rate = u64::from(state.rate.max(1));
    let audio = state.tap_between(start_ms * rate / 1000, end_ms * rate / 1000);
    if audio.is_empty() {
        return Value::Null;
    }
    let features = analyze_turn(
        audio,
        state.rate,
        20,
        crate::vad::VadConfig::default().energy_threshold,
        &PitchConfig::default(),
    );
    serde_json::to_value(features).unwrap_or(Value::Null)
}

/// The headers of a transcript or keyword message.
pub fn segment_headers(kind: EventKind, seq: u64) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("x-segment-seq".to_string(), seq.to_string()),
        ("x-event".to_string(), kind.as_str().to_string()),
    ])
}

/// Decode a media frame for the tap.
pub fn decode_frame(bytes: &[u8], format: &AudioFormat) -> Option<Vec<i16>> {
    audio::decode(bytes, format).ok().map(|(s, _)| s)
}
