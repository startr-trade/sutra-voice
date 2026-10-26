//! Listen-only calls: the media plane of a call the flow only listens to (a meeting, say).
//!
//! The far end's audio is forked to two consumers, and neither goes through the flow:
//!
//! - **the recorder** — the call's audio kept in memory while the call is up (the far end, with
//!   the transport's own prompts mixed in where they were played), written once at the
//!   hang-up as a WAV file to the deployment's object store, sealed with envelope encryption
//!   ([`sutra_objectstore::sealed`]). The flow receives a media REFERENCE on the `call-hangup`
//!   message (`recording`: uri, SHA-256 of the stored object, duration, format, whether the
//!   consent announcement was played) — never the media.
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
use sutra_objectstore::{sealed, ObjectStore, ObjectStoreError, StoredObject};
use tokio::sync::{mpsc, oneshot};

use crate::audio::{self, AudioFormat};
use crate::config::{EventKind, RecordingSettings, TranscribeSettings};
use crate::features::{analyze_turn, PitchConfig};
use crate::hub::{Call, VoipHub};

/// The media type recordings are written in.
pub const RECORDING_FORMAT: &str = "audio/wav";
/// The `kind` a media reference carries (what `<q:audit capture="media">` collects).
pub const MEDIA_REFERENCE_KIND: &str = "media";
/// What `speaker` says when the provider labels none.
pub const UNKNOWN_SPEAKER: &str = "unknown";

/// The recordings sink: the object store plus the sealing key.
pub struct Recorder {
    pub store: Arc<dyn ObjectStore>,
    kek: Option<sutra_crypto::Kek>,
    key_id: String,
    pub max_seconds: u64,
}

impl Recorder {
    /// Build the sink `settings` describe.
    pub fn from_settings(settings: &RecordingSettings) -> Result<Recorder, ObjectStoreError> {
        let store: Arc<dyn ObjectStore> = match settings.store.scheme.as_str() {
            "memory" => Arc::new(sutra_objectstore::MemoryObjectStore::new(
                settings.store.clone(),
            )),
            _ => {
                let credentials = sutra_objectstore::sigv4::Credentials {
                    access_key: settings.access_key.clone().unwrap_or_default(),
                    secret_key: settings.secret_key.clone().unwrap_or_default(),
                    session_token: None,
                };
                let mut config = sutra_objectstore::S3Config::new(
                    settings.store.clone(),
                    &settings.region,
                    credentials,
                );
                config.endpoint = settings.endpoint.clone();
                Arc::new(sutra_objectstore::S3ObjectStore::new(config)?)
            }
        };
        Ok(Recorder::new(
            store,
            settings.kek_secret.as_deref(),
            &settings.key_id,
            settings.max_seconds,
        ))
    }

    pub fn new(
        store: Arc<dyn ObjectStore>,
        kek_secret: Option<&str>,
        key_id: &str,
        max_seconds: u64,
    ) -> Recorder {
        Recorder {
            store,
            kek: kek_secret.map(|s| sutra_crypto::Kek::from_secret(s.as_bytes())),
            key_id: key_id.to_string(),
            max_seconds,
        }
    }

    pub fn sealed(&self) -> bool {
        self.kek.is_some()
    }

    /// Write `wav` for `call_id` of `module_key`; retried on an unavailable store.
    async fn write(
        &self,
        module_key: &str,
        call_id: &str,
        wav: Vec<u8>,
    ) -> Result<(StoredObject, Option<String>), String> {
        self.write_at(module_key, &[call_id], wav).await
    }

    /// Write `wav` at `<module path>/<segments…>.wav` (each segment made safe); retried on an
    /// unavailable store.
    pub(crate) async fn write_at(
        &self,
        module_key: &str,
        segments: &[&str],
        wav: Vec<u8>,
    ) -> Result<(StoredObject, Option<String>), String> {
        let safe = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                        c
                    } else {
                        '-'
                    }
                })
                .collect()
        };
        let module_path: Vec<String> = module_key.split('/').map(safe).collect();
        let name: Vec<String> = segments.iter().map(|s| safe(s)).collect();
        let base = format!("{}/{}.wav", module_path.join("/"), name.join("/"));
        let (key, body, content_type) = match &self.kek {
            Some(kek) => {
                let key = format!("{base}{}", sealed::SEALED_EXTENSION);
                let full = self.store.full_key(&key);
                let body = sealed::seal(kek, &self.key_id, &full, RECORDING_FORMAT, &wav)?;
                (key, body, sealed::SEALED_CONTENT_TYPE)
            }
            None => (base, wav, RECORDING_FORMAT),
        };
        let mut delay = Duration::from_millis(500);
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.store.put(&key, body.clone(), content_type).await {
                Ok(stored) => return Ok((stored, self.kek.as_ref().map(|_| self.key_id.clone()))),
                Err(ObjectStoreError::Unavailable(m)) if attempt < 4 => {
                    tracing::warn!(
                        key = %key,
                        attempt,
                        "voip: recording write failed, retrying: {m}"
                    );
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// A listen-only call's media-plane state (inside the call, behind a lock).
pub struct ListenState {
    /// The far end's audio since the media connected, at `rate`.
    pub tap: Vec<i16>,
    pub rate: u32,
    /// Whether the audio is recorded (the request was marked `capture=media`).
    pub record: bool,
    /// The most samples kept.
    pub max_samples: usize,
    /// Played audio: (offset into the tap, samples), mixed into the recording at the end.
    pub played: Vec<(usize, Vec<i16>)>,
    /// Frames to the transcriber (`None` when the call is not transcribed).
    pub frames: Option<mpsc::UnboundedSender<Vec<i16>>>,
    /// The transcriber's result, once it has finished.
    pub transcript: Option<oneshot::Receiver<Value>>,
    /// Whether the consent announcement was played to the end, and where.
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
            rate,
            record,
            max_samples: (max_seconds.saturating_mul(u64::from(rate))) as usize,
            played: Vec::new(),
            frames: None,
            transcript: None,
            consent_at_ms: None,
            truncated: false,
            started: std::time::Instant::now(),
        }
    }

    /// Fill the tap with silence up to the call's clock when it has fallen behind by more than
    /// [`TAP_SLACK_MS`] (the transcriber hears the silence too, so its offsets stay offsets
    /// into the recording).
    pub fn catch_up(&mut self) {
        let rate = u64::from(self.rate.max(1));
        let due = self.started.elapsed().as_millis() as u64 * rate / 1000;
        let have = self.tap.len() as u64;
        if have + TAP_SLACK_MS * rate / 1000 >= due {
            return;
        }
        let gap = (due - have) as usize;
        let room = self.max_samples.saturating_sub(self.tap.len());
        if gap > room {
            self.truncated = true;
        }
        self.tap.resize(self.tap.len() + gap.min(room), 0);
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
        if self.tap.len() + samples.len() <= self.max_samples {
            self.tap.extend_from_slice(samples);
        } else {
            self.truncated = true;
        }
        if let Some(frames) = &self.frames {
            let _ = frames.send(samples.to_vec());
        }
    }

    /// Milliseconds of audio so far.
    pub fn elapsed_ms(&self) -> u64 {
        self.tap.len() as u64 * 1000 / u64::from(self.rate.max(1))
    }

    /// The recording: the far end's audio with the played audio mixed in where it played.
    pub fn mixed(&self) -> Vec<i16> {
        let mut out = self.tap.clone();
        for (offset, samples) in &self.played {
            let end = offset + samples.len();
            if out.len() < end {
                out.resize(end.min(self.max_samples.max(out.len())), 0);
            }
            for (slot, sample) in out.iter_mut().skip(*offset).zip(samples) {
                *slot = slot.saturating_add(*sample);
            }
        }
        out
    }
}

/// The finished recording's media reference, or the reason there is none.
pub async fn finish_recording(
    recorder: Option<&Recorder>,
    module_key: &str,
    call: &Call,
    state: &ListenState,
) -> Value {
    let Some(recorder) = recorder else {
        return json!({"status": "unavailable", "detail": "no recording store is configured"});
    };
    let samples = state.mixed();
    let duration_ms = samples.len() as u64 * 1000 / u64::from(state.rate.max(1));
    let wav = audio::wav_wrap(&samples, state.rate);
    match recorder.write(module_key, &call.call_id, wav).await {
        Ok((stored, key_id)) => json!({
            "kind": MEDIA_REFERENCE_KIND,
            "uri": stored.uri,
            "sha256": stored.sha256,
            "bytes": stored.bytes,
            "durationMs": duration_ms,
            "format": RECORDING_FORMAT,
            "rate": state.rate,
            "channels": 1,
            "encryption": key_id.map(|id| json!({
                "scheme": "sutra-sealed-1", "cipher": sealed::CIPHER, "keyId": id
            })),
            "consentAnnounced": state.consent_at_ms.is_some(),
            "consentAnnouncedAtMs": state.consent_at_ms,
            "truncated": state.truncated,
            "callId": call.call_id,
        }),
        Err(detail) => {
            tracing::warn!(call_id = %call.call_id, "voip: the recording could not be stored: {detail}");
            json!({"status": "failed", "detail": detail})
        }
    }
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
    let from = (start_ms * rate / 1000) as usize;
    let to = ((end_ms * rate / 1000) as usize).min(state.tap.len());
    if from >= to {
        return Value::Null;
    }
    let features = analyze_turn(
        &state.tap[from..to],
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
