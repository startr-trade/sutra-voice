//! Meeting sessions end to end on the hub: a scripted gateway streams two participants on one
//! tagged socket (each its own audio and sampled frames), a scripted live session transcribes
//! each participant's track, and a scripted video-understanding model analyses the segments —
//! both reached through `llm` channels of the deployment, the analysis falling back from a
//! first candidate that is down to the second.
//! Proven: the sampling policy reaches the gateway, a turn boundary asks for a frame of
//! everyone, transcripts are attributed to the track they came from, only the external
//! participant is analysed (the staff one never reaches the model), the model's answer is
//! stripped of a sensitive inference, and the hang-up carries a sealed recording per
//! participant whose SHA-256 is the stored object's.

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sutra_llm_spi::{
    AudioSession, AudioSessionConfig, AudioSessionEvent, BoxFuture, Diagnostic, LlmFailure,
    LlmProvider, LlmProviderConfig, LlmProviderFactory, LlmRequest, LlmResponse, LlmVideo,
};
use sutra_transport_voip::audio::{pcm16_to_le, wav_unwrap};
use sutra_transport_voip::config::route_set_of;
use sutra_transport_voip::gateway::{GatewayControl, GenericGateway};
use sutra_transport_voip::hub::VoipHub;
use sutra_transport_voip::listen::Recorder;
use sutra_transport_voip::media::media_router;
use sutra_transport_voip::meeting::{
    effective_role, pick_frames, strip_forbidden, AnalysisPolicy, SampledFrame, SpeechChange,
    SpeechTracker,
};
use sutra_transport_voip::protocol::{
    parse_generic_text, parse_tagged, GenericFrame, TaggedHeader, TaggedKind,
};
use sutra_transport_voip::recording::open_stream;
use sutra_transport_voip::store::{MemoryRecordingStore, RecordingStore, StoreLocation};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::support::*;

const TOKEN: &str = "media-secret";
const KEK: &str = "a test key-encryption secret";
const RATE: u32 = 16_000;

const SESSION_CHANNELS: &str = r#"
channels:
  - name: session
    transport: voip
    bind: "number:+15550190000"
    mode: listen
    record: true
    silence-ms: 300
    video:
      fps: 2
      max-width: 320
  - name: participant
    transport: voip
    event: call-participant
  - name: hangup
    transport: voip
    event: call-hangup
  - name: transcript
    transport: voip
    event: call-transcript
    transcribe:
      channel: meeting-transcription
      drain-ms: 500
  - name: analysis
    transport: voip
    event: call-analysis
    analyze:
      channel: video-analysis
      segment-seconds: 2
      max-frames: 4
  - name: meeting-transcription
    transport: llm
    direction: outbound
    bind: "llm://meeting-transcription"
    ack-mode: on-complete
    llm:
      require: [realtime]
      candidates:
        - provider: test-meeting-live
          model: live-test
  - name: video-analysis
    transport: llm
    direction: outbound
    bind: "llm://video-analysis"
    ack-mode: on-complete
    llm:
      require: [video-understanding]
      timeout-seconds: 5
      candidates:
        - provider: test-video
          model: video-down
        - provider: test-video
          model: video-test
"#;

// ---- a scripted live session: one segment per session, after 1 s of audio -----------------------

struct OneSegmentSession {
    received_ms: Mutex<u64>,
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<AudioSessionEvent>>,
    tx: mpsc::UnboundedSender<AudioSessionEvent>,
}

impl AudioSession for OneSegmentSession {
    fn send_audio<'a>(&'a self, frame: &'a [u8]) -> BoxFuture<'a, Result<(), LlmFailure>> {
        Box::pin(async move {
            let mut ms = self.received_ms.lock().unwrap();
            let before = *ms;
            *ms += (frame.len() / 2) as u64 * 1000 / u64::from(RATE);
            if before < 1000 && *ms >= 1000 {
                let _ = self.tx.send(AudioSessionEvent::SpeechStarted {
                    item_id: "s1".into(),
                    audio_ms: 200,
                });
                let _ = self.tx.send(AudioSessionEvent::SpeechStopped {
                    item_id: "s1".into(),
                    audio_ms: 900,
                });
                let _ = self.tx.send(AudioSessionEvent::InputTranscript {
                    item_id: "s1".into(),
                    text: "Here is what I think.".into(),
                    is_final: true,
                    speaker: None,
                });
            }
            Ok(())
        })
    }

    fn commit(&self) -> BoxFuture<'_, Result<(), LlmFailure>> {
        Box::pin(async { Ok(()) })
    }

    fn next_event(&self) -> BoxFuture<'_, Option<AudioSessionEvent>> {
        Box::pin(async move { self.events.lock().await.recv().await })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

struct MeetingLive;

impl LlmProvider for MeetingLive {
    fn capabilities(&self) -> sutra_llm_spi::LlmCapabilities {
        sutra_llm_spi::LlmCapabilities {
            realtime: true,
            ..Default::default()
        }
    }

    fn complete<'a>(
        &'a self,
        _request: &'a LlmRequest,
    ) -> BoxFuture<'a, Result<LlmResponse, LlmFailure>> {
        Box::pin(async {
            Err(LlmFailure::Permanent {
                detail: "not a chat model".into(),
            })
        })
    }

    fn open_audio_session<'a>(
        &'a self,
        _config: &'a AudioSessionConfig,
    ) -> BoxFuture<'a, Result<Box<dyn AudioSession>, LlmFailure>> {
        Box::pin(async move {
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(AudioSessionEvent::Ready);
            Ok(Box::new(OneSegmentSession {
                received_ms: Mutex::new(0),
                events: tokio::sync::Mutex::new(rx),
                tx,
            }) as Box<dyn AudioSession>)
        })
    }
}

fn build_meeting_live(_config: &LlmProviderConfig) -> Result<Arc<dyn LlmProvider>, Diagnostic> {
    Ok(Arc::new(MeetingLive))
}

inventory::submit! {
    LlmProviderFactory { provider: "test-meeting-live", build: build_meeting_live }
}

// ---- a scripted video-understanding model ------------------------------------------------------

/// (prompt text, frames, has audio) of every analysis request.
static ANALYSED: Mutex<Vec<(String, usize, bool)>> = Mutex::new(Vec::new());
/// How many analysis requests the `video-down` candidate refused.
static REFUSED_DOWN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `video-down` is overloaded on every request; any other model answers.
struct VideoModel {
    model: String,
}

impl LlmProvider for VideoModel {
    fn capabilities(&self) -> sutra_llm_spi::LlmCapabilities {
        sutra_llm_spi::LlmCapabilities {
            json_output: true,
            audio_input: true,
            audio_understanding: true,
            video_understanding: true,
            ..Default::default()
        }
    }

    fn complete<'a>(
        &'a self,
        request: &'a LlmRequest,
    ) -> BoxFuture<'a, Result<LlmResponse, LlmFailure>> {
        Box::pin(async move {
            if self.model == "video-down" {
                REFUSED_DOWN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Err(LlmFailure::Retryable {
                    class: sutra_llm_spi::RetryableClass::RateLimited,
                    retry_after: None,
                    detail: "overloaded".into(),
                });
            }
            let message = &request.messages[0];
            let frames = match &message.video {
                Some(LlmVideo::Frames(frames)) => frames.len(),
                _ => 0,
            };
            ANALYSED.lock().unwrap().push((
                message.content.clone(),
                frames,
                message.audio.is_some(),
            ));
            let answer = json!({
                "expression": "attentive",
                "engagement": "high",
                "attention": "on the speaker",
                "bodyLanguage": ["nodding"],
                "emotion": {"label": "interested", "confidence": 0.7,
                            "modalities": {"voice": true, "face": true, "body": true, "words": true}},
                "disagreement": {"flagged": false, "detail": null},
                "age": 41,
                "reactions": [{"atMs": 300, "to": "P2", "cue": "nods", "Gender": "x"}],
            });
            Ok(LlmResponse::text(
                format!("```json\n{answer}\n```"),
                sutra_llm_spi::FinishReason::Stop,
            ))
        })
    }
}

fn build_video(config: &LlmProviderConfig) -> Result<Arc<dyn LlmProvider>, Diagnostic> {
    Ok(Arc::new(VideoModel {
        model: config.model.clone(),
    }))
}

inventory::submit! {
    LlmProviderFactory { provider: "test-video", build: build_video }
}

// ---- the session ----------------------------------------------------------------------------------

fn json_of(message: &sutra_channels::InboundMessage) -> Value {
    serde_json::from_slice(&message.body).unwrap()
}

/// A fake JPEG (the transport never decodes a frame).
fn frame_bytes(n: u8) -> Vec<u8> {
    vec![0xFF, 0xD8, 0xFF, 0xE0, n, n, 0xFF, 0xD9]
}

fn audio_frame(index: u16, at_ms: u64, samples: &[i16]) -> Message {
    let header = TaggedHeader {
        kind: TaggedKind::Audio,
        flags: 0,
        index,
        at_ms,
    };
    Message::binary(header.encode(&pcm16_to_le(samples)))
}

fn video_frame(index: u16, at_ms: u64, requested: bool, n: u8) -> Message {
    let header = TaggedHeader {
        kind: TaggedKind::Video,
        flags: u8::from(requested),
        index,
        at_ms,
    };
    Message::binary(header.encode(&frame_bytes(n)))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_meeting_session_is_attributed_sampled_analysed_per_role_and_recorded_per_participant() {
    let (intake, mut rx) = FakeIntake::new();
    let defs = definitions(SESSION_CHANNELS, "video-meetings");
    publish_llm_channels(&defs);
    let routes = route_set_of(&defs).unwrap();
    let store =
        MemoryRecordingStore::new(StoreLocation::parse("memory://recordings/meetings").unwrap());
    let recorder = Arc::new(Recorder::new(
        Arc::new(store.clone()) as Arc<dyn RecordingStore>,
        Some(KEK),
        "rec-key",
        60,
    ));
    let hub = VoipHub::with_recorder(
        test_config("ws://unused", Some(TOKEN)),
        intake,
        routes,
        Arc::new(GenericGateway) as Arc<dyn GatewayControl>,
        Some(recorder),
    );
    let addr = serve(media_router(Arc::clone(&hub))).await;
    let ws = connect_stream(addr, TOKEN).await;
    let (mut tx, mut from_engine) = ws.split();
    tx.send(Message::text(
        json!({"event": "start", "callId": "standup-1", "streamId": "RM_1",
               "format": "audio/L16;rate=16000", "from": "livekit:standup",
               "to": "+15550190000", "direction": "inbound",
               "headers": {"x-room": "standup"}, "streams": "tagged",
               "video": {"formats": ["image/jpeg"]}})
        .to_string(),
    ))
    .await
    .unwrap();

    // The sampling policy is the first thing the gateway hears.
    let policy = loop {
        if let Some(Ok(Message::Text(text))) = from_engine.next().await {
            break serde_json::from_str::<Value>(&text).unwrap();
        }
    };
    assert_eq!(policy["event"], "policy");
    assert_eq!(policy["video"]["fps"], 2.0);
    assert_eq!(policy["video"]["maxWidth"], 320);
    assert_eq!(policy["video"]["format"], "image/jpeg");
    // Every later engine frame (sample requests) is collected in the background.
    let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
    let collected = Arc::clone(&requests);
    tokio::spawn(async move {
        while let Some(Ok(frame)) = from_engine.next().await {
            if let Message::Text(text) = frame {
                collected
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&text).unwrap());
            }
        }
    });

    let start = next_on(&mut rx, "session").await;
    let body = json_of(&start);
    assert_eq!(body["session"], "meeting");
    assert_eq!(body["recorded"], true);
    assert_eq!(body["headers"]["x-room"], "standup");

    for (id, index, label, role) in [
        ("PA_sam", 1, "Sam", "external"),
        ("PA_alex", 2, "Alex", "staff"),
    ] {
        tx.send(Message::text(
            json!({"event": "participant-joined", "participantId": id, "index": index,
                   "label": label, "role": role, "atMs": 0})
            .to_string(),
        ))
        .await
        .unwrap();
    }
    let joined = [
        json_of(&next_on(&mut rx, "participant").await),
        json_of(&next_on(&mut rx, "participant").await),
    ];
    assert_eq!(joined[0]["participantId"], "PA_sam");
    assert_eq!(joined[0]["analysed"], true);
    assert_eq!(joined[1]["participantId"], "PA_alex");
    assert_eq!(joined[1]["analysed"], false);
    assert_eq!(joined[1]["analysisPolicy"], "staff-not-opted-in");

    // 3.2 s in real time: Sam speaks 0.2-1.4 s, Alex 1.6-2.6 s; both cameras at 2 fps.
    let loud = tone(220.0, 20, RATE, 9000.0);
    let quiet = silence(20, RATE);
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(20));
    for step in 0..160u64 {
        ticker.tick().await;
        let at = step * 20;
        let sam = if (200..1400).contains(&at) {
            &loud
        } else {
            &quiet
        };
        let alex = if (1600..2600).contains(&at) {
            &loud
        } else {
            &quiet
        };
        tx.send(audio_frame(1, at, sam)).await.unwrap();
        tx.send(audio_frame(2, at, alex)).await.unwrap();
        if at % 500 == 0 {
            tx.send(video_frame(1, at, false, step as u8))
                .await
                .unwrap();
            tx.send(video_frame(2, at, false, step as u8))
                .await
                .unwrap();
        }
    }
    // The gateway answers the engine's sample requests with a frame of each participant.
    tx.send(video_frame(1, 3200, true, 200)).await.unwrap();
    tx.send(video_frame(2, 3200, true, 200)).await.unwrap();
    for id in ["PA_sam", "PA_alex"] {
        tx.send(Message::text(
            json!({"event": "participant-left", "participantId": id, "reason": "left"}).to_string(),
        ))
        .await
        .unwrap();
    }
    tx.send(Message::text(
        json!({"event": "stop", "reason": "room-empty"}).to_string(),
    ))
    .await
    .unwrap();

    // A turn boundary asked for a frame of everyone.
    let sampled = requests.lock().unwrap().clone();
    assert!(
        sampled.iter().any(|r| r["event"] == "sample"
            && r["reason"] == "turn-boundary"
            && r["participantId"].is_null()),
        "{sampled:?}"
    );

    // Transcripts: one per participant, attributed by track, in session time.
    let mut transcripts = Vec::new();
    while transcripts.len() < 2 {
        transcripts.push(json_of(&next_on(&mut rx, "transcript").await));
    }
    transcripts.sort_by_key(|t| t["participantIndex"].as_u64());
    for (t, (id, label)) in transcripts
        .iter()
        .zip([("PA_sam", "Sam"), ("PA_alex", "Alex")])
    {
        assert_eq!(t["speaker"], label, "{t}");
        assert_eq!(t["speakerSource"], "track");
        assert_eq!(t["participantId"], id);
        assert!(t["startMs"].as_u64().unwrap() >= 200, "{t}");
    }

    // The hang-up comes after every analysis; only Sam's segments were analysed.
    let mut analyses = Vec::new();
    let hangup = loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("the hang-up within 10 s")
            .unwrap();
        match message.channel.as_str() {
            "analysis" => analyses.push(json_of(&message)),
            "hangup" => break json_of(&message),
            _ => {}
        }
    };
    assert_eq!(analyses.len(), 2, "two 2 s segments of Sam: {analyses:?}");
    for (i, a) in analyses.iter().enumerate() {
        assert_eq!(a["participantId"], "PA_sam");
        assert_eq!(a["segment"], i as u64 + 1);
        assert_eq!(a["status"], "complete");
        assert_eq!(a["result"]["emotion"]["label"], "interested");
        assert!(a["result"].get("age").is_none(), "{a}");
        assert!(a["result"]["reactions"][0].get("Gender").is_none(), "{a}");
        assert_eq!(a["removedKeys"], json!(["age", "reactions.0.Gender"]));
        let frames = a["frames"].as_u64().unwrap();
        assert!((1..=4).contains(&frames), "{a}");
        assert_eq!(a["audioSent"], true);
        assert_eq!(a["key"], format!("standup-1:1:{}", i + 1));
        // Through the channel: the first candidate was overloaded, the second answered.
        assert_eq!(a["channel"], "video-analysis");
        assert_eq!(a["provider"], "test-video");
        assert_eq!(a["model"], "video-test");
    }
    assert_eq!(
        REFUSED_DOWN.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "each segment was tried on the first candidate first"
    );
    let analysed = ANALYSED.lock().unwrap().clone();
    assert_eq!(analysed.len(), 2);
    for (prompt, frames, audio) in &analysed {
        assert!(prompt.starts_with("Analyse participant P1 "), "{prompt}");
        assert!(
            !prompt.contains("Sam") && !prompt.contains("Alex"),
            "pseudonyms only: {prompt}"
        );
        assert!(*frames >= 1 && *audio);
    }

    assert_eq!(hangup["session"], "meeting");
    assert_eq!(hangup["analysis"]["results"], 2);
    assert_eq!(hangup["analysis"]["channel"], "video-analysis");
    assert_eq!(hangup["analysis"]["model"], "video-test");
    assert_eq!(hangup["analysis"]["analysedRoles"], json!(["external"]));
    assert_eq!(hangup["video"]["framesRetained"], false);
    let participants = hangup["participants"].as_array().unwrap();
    assert_eq!(participants.len(), 2);
    assert_eq!(participants[0]["analysed"], true);
    assert_eq!(participants[0]["analysis"]["segments"], 2);
    assert_eq!(participants[1]["analysed"], false);
    assert!(participants[1]["analysis"].is_null());
    for p in participants {
        assert_eq!(p["video"]["regular"], 7, "{p}");
        assert_eq!(p["video"]["requested"], 1, "{p}");
        assert!(p["talkMs"].as_u64().unwrap() >= 800, "{p}");
    }
    let recordings = hangup["recordings"].as_array().unwrap();
    assert_eq!(recordings.len(), 2);
    let kek = sutra_crypto::Kek::from_secret(KEK.as_bytes());
    for (i, reference) in recordings.iter().enumerate() {
        let uri = reference["uri"].as_str().unwrap();
        let expected = format!("acme/video-meetings/1.0.0/standup-1/p{}.wav.sealed", i + 1);
        assert!(uri.ends_with(&expected), "{uri}");
        let full = uri.strip_prefix("memory://recordings/").unwrap();
        let key = full.strip_prefix("meetings/").unwrap();
        let stored = store.get(key).await.unwrap().expect("stored");
        assert_eq!(
            reference["sha256"],
            sutra_objectstore::sha256_hex(&stored).as_str()
        );
        let opened = open_stream(&kek, full, &stored).unwrap();
        assert!(opened.complete);
        let wav = opened.plaintext;
        let (samples, rate) = wav_unwrap(&wav).unwrap();
        assert_eq!(rate, RATE);
        assert!(
            samples.len() as u32 >= RATE * 3,
            "{} samples",
            samples.len()
        );
        assert_eq!(reference["participantIndex"], i as u64 + 1);
    }
}

// ---- the policy and the pieces ------------------------------------------------------------------

/// `analyze.channel` must name an `llm` channel that requires video understanding; a model
/// named on the voip channel itself is refused.
#[test]
fn an_analysis_channel_reference_is_validated_at_load() {
    let yaml = |analyze: &str, require: &str| {
        format!(
            r#"
channels:
  - name: analysis
    transport: voip
    event: call-analysis
    analyze:
{analyze}
  - name: video-analysis
    transport: llm
    direction: outbound
    bind: "llm://video-analysis"
    ack-mode: on-complete
    llm:
{require}
      candidates:
        - provider: test-video
          model: video-test
"#
        )
    };
    let require = "      require: [video-understanding]";
    let refused = |yaml: String| {
        let error = route_set_of(&definitions(&yaml, "m")).unwrap_err();
        assert_eq!(
            error.code, "SUTRA.VOIP.MODEL_CHANNEL_INVALID",
            "{}",
            error.message
        );
        error.message
    };
    let direct = refused(yaml(
        "      provider: test-video\n      model: video-test",
        require,
    ));
    assert!(direct.contains("analyze.provider"), "{direct}");
    let undeclared = refused(yaml(
        "      channel: video-analysis",
        "      require: [json-output]",
    ));
    assert!(
        undeclared.contains("require: [video-understanding]"),
        "{undeclared}"
    );
    let settings = route_set_of(&definitions(
        &yaml("      channel: video-analysis", require),
        "m",
    ))
    .unwrap()
    .modules["acme/m/1.0.0"]
        .analysis
        .clone()
        .unwrap();
    assert_eq!(settings.channel, "video-analysis");
}

#[test]
fn a_staff_role_is_refused_for_analysis_without_the_explicit_opt_in() {
    let yaml = |extra: &str| {
        format!(
            r#"
channels:
  - name: analysis
    transport: voip
    event: call-analysis
    analyze:
      channel: video-analysis
      roles: [external, staff]
{extra}
  - name: video-analysis
    transport: llm
    direction: outbound
    bind: "llm://video-analysis"
    ack-mode: on-complete
    llm:
      require: [video-understanding]
      candidates:
        - provider: test-video
          model: video-test
"#
        )
    };
    let refused = route_set_of(&definitions(&yaml(""), "m")).unwrap_err();
    assert!(refused.message.contains("AI Act"), "{}", refused.message);
    let allowed = route_set_of(&definitions(&yaml("      allow-staff: true"), "m")).unwrap();
    let settings = allowed.modules["acme/m/1.0.0"].analysis.clone().unwrap();
    assert_eq!(settings.policy_for("staff"), AnalysisPolicy::Analysed);
    assert_eq!(settings.policy_for("external"), AnalysisPolicy::Analysed);
    assert_eq!(
        settings.policy_for("unknown"),
        AnalysisPolicy::RoleNotAnalysed
    );
    // An undeclared role is not analysed by default.
    assert_eq!(effective_role(None, Some(&settings)), "unknown");
    assert_eq!(
        effective_role(Some(" External "), Some(&settings)),
        "external"
    );
}

#[test]
fn tagged_frames_and_participant_frames_parse() {
    let header = TaggedHeader {
        kind: TaggedKind::Video,
        flags: 1,
        index: 513,
        at_ms: 1_234_567,
    };
    let bytes = header.encode(b"\xFF\xD8");
    let (parsed, payload) = parse_tagged(&bytes).unwrap();
    assert_eq!(parsed, header);
    assert!(parsed.requested());
    assert_eq!(payload, b"\xFF\xD8");
    assert!(parse_tagged(&bytes[..11]).is_err());
    let joined = parse_generic_text(
        r#"{"event":"participant-joined","participantId":"PA_1","index":3,"label":"Kim","role":"Staff"}"#,
    )
    .unwrap();
    let GenericFrame::ParticipantJoined(joined) = joined else {
        panic!("{joined:?}")
    };
    assert_eq!((joined.index, joined.role.as_deref()), (3, Some("staff")));
    assert!(parse_generic_text(
        r#"{"event":"participant-joined","participantId":"PA_1","index":0}"#
    )
    .is_err());
    let start = parse_generic_text(
        r#"{"event":"start","callId":"c","streams":"tagged","video":{"formats":["image/jpeg"]}}"#,
    )
    .unwrap();
    let GenericFrame::Start(start) = start else {
        panic!()
    };
    assert!(start.tagged);
    assert_eq!(start.video_formats, ["image/jpeg"]);
}

#[test]
fn requested_frames_are_kept_first_and_regular_ones_spread() {
    let frame = |at_ms, requested| SampledFrame {
        at_ms,
        data: vec![],
        requested,
    };
    let mut frames: Vec<SampledFrame> = (0..10).map(|i| frame(i * 100, false)).collect();
    frames.push(frame(550, true));
    let picked = pick_frames(frames, 4);
    assert_eq!(picked.len(), 4);
    assert!(picked.iter().any(|f| f.requested));
    assert!(picked.windows(2).all(|w| w[0].at_ms <= w[1].at_ms));
}

#[test]
fn sensitive_inferences_are_stripped_at_any_depth() {
    let mut answer = json!({"emotion": {"label": "calm"}, "Ethnicity": "x",
                            "cues": [{"sexual_orientation": "y", "cue": "smile"}]});
    let removed = strip_forbidden(&mut answer);
    assert_eq!(removed, ["Ethnicity", "cues.0.sexual_orientation"]);
    assert_eq!(
        answer,
        json!({"emotion": {"label": "calm"}, "cues": [{"cue": "smile"}]})
    );
}

#[test]
fn the_speech_tracker_finds_turn_boundaries() {
    let vad = sutra_transport_voip::vad::VadConfig {
        silence_ms: 300,
        ..Default::default()
    };
    let mut tracker = SpeechTracker::new(RATE, &vad);
    let mut changes = tracker.push(&silence(200, RATE));
    changes.extend(tracker.push(&tone(200.0, 600, RATE, 9000.0)));
    changes.extend(tracker.push(&silence(500, RATE)));
    let kinds: Vec<SpeechChange> = changes.iter().map(|(_, c)| *c).collect();
    assert_eq!(kinds, [SpeechChange::Started, SpeechChange::Ended]);
    assert_eq!(changes[0].0, 200);
    assert_eq!(changes[1].0, 800);
    assert_eq!(tracker.voiced_ms, 600);
}
