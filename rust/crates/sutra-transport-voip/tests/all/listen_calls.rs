//! Listen-only calls end to end on the hub, with a scripted gateway, a scripted live
//! transcription session (a model provider registered by this test, reached through an `llm`
//! channel whose first candidate is down, so the session opens on the fallback) and an in-memory object
//! store: no turns are cut, the PIN goes out paced as DTMF, the consent prompt is played and
//! recorded, transcript segments and a keyword are delivered, and the hang-up carries the
//! sealed recording's reference — whose SHA-256 is the stored object's.

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use sutra_llm_spi::{
    AudioSession, AudioSessionConfig, AudioSessionEvent, BoxFuture, Diagnostic, LlmFailure,
    LlmProvider, LlmProviderConfig, LlmProviderFactory, LlmRequest, LlmResponse,
};
use sutra_objectstore::{sealed, MemoryObjectStore, ObjectStore, StoreLocation};
use sutra_transport_voip::audio::{pcm16_to_le, wav_unwrap, wav_wrap};
use sutra_transport_voip::config::{route_set_of, RecordingSettings, VoipConfig};
use sutra_transport_voip::gateway::{GatewayControl, GatewayError, GatewayEvent, OriginateRequest};
use sutra_transport_voip::hub::{OpError, VoipHub};
use sutra_transport_voip::listen::Recorder;
use sutra_transport_voip::media::media_router;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::support::*;

const TOKEN: &str = "media-secret";
const KEK: &str = "a test key-encryption secret";

const MEETING_CHANNELS: &str = r#"
channels:
  - name: progress
    transport: voip
    event: call-progress
  - name: hangup
    transport: voip
    event: call-hangup
  - name: transcript
    transport: voip
    event: call-transcript
    transcribe:
      channel: live-transcription
      drain-ms: 1000
  - name: live-transcription
    transport: llm
    direction: outbound
    bind: "llm://live-transcription"
    ack-mode: on-complete
    llm:
      require: [realtime]
      fallback-on: [unavailable]
      candidates:
        - provider: test-live
          model: live-down
        - provider: test-live
          model: live-test
          transcription-model: transcribe-test
  - name: keyword
    transport: voip
    event: call-keyword
    phrases: [action item, deadline]
"#;

// ---- a scripted live session -------------------------------------------------------------------

/// Answers transcripts as audio arrives: after 600 ms of audio one segment with speech markers
/// (mentioning an action item), and at the commit (the hang-up) a second one without markers.
struct ScriptedSession {
    received_ms: Mutex<u64>,
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<AudioSessionEvent>>,
    tx: mpsc::UnboundedSender<AudioSessionEvent>,
    rate: u32,
}

impl AudioSession for ScriptedSession {
    fn send_audio<'a>(&'a self, frame: &'a [u8]) -> BoxFuture<'a, Result<(), LlmFailure>> {
        Box::pin(async move {
            let mut ms = self.received_ms.lock().unwrap();
            let before = *ms;
            *ms += (frame.len() / 2) as u64 * 1000 / u64::from(self.rate);
            if before < 600 && *ms >= 600 {
                let _ = self.tx.send(AudioSessionEvent::SpeechStarted {
                    item_id: "i1".into(),
                    audio_ms: 100,
                });
                let _ = self.tx.send(AudioSessionEvent::SpeechStopped {
                    item_id: "i1".into(),
                    audio_ms: 580,
                });
                let _ = self.tx.send(AudioSessionEvent::InputTranscript {
                    item_id: "i1".into(),
                    text: "Let's take an Action Item for Sam.".into(),
                    is_final: true,
                    speaker: None,
                });
            }
            Ok(())
        })
    }

    fn commit(&self) -> BoxFuture<'_, Result<(), LlmFailure>> {
        Box::pin(async move {
            let _ = self.tx.send(AudioSessionEvent::InputTranscript {
                item_id: "i2".into(),
                text: "Thanks, everyone.".into(),
                is_final: true,
                speaker: None,
            });
            Ok(())
        })
    }

    fn next_event(&self) -> BoxFuture<'_, Option<AudioSessionEvent>> {
        Box::pin(async move { self.events.lock().await.recv().await })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// The configs every session of this provider was opened with.
static OPENED: Mutex<Vec<AudioSessionConfig>> = Mutex::new(Vec::new());

/// `live-down` refuses every session as unavailable; any other model opens a scripted one.
struct LiveProvider {
    model: String,
}

impl LlmProvider for LiveProvider {
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
        config: &'a AudioSessionConfig,
    ) -> BoxFuture<'a, Result<Box<dyn AudioSession>, LlmFailure>> {
        Box::pin(async move {
            if self.model == "live-down" {
                return Err(LlmFailure::Retryable {
                    class: sutra_llm_spi::RetryableClass::Unavailable,
                    retry_after: None,
                    detail: "the live service is down".into(),
                });
            }
            OPENED.lock().unwrap().push(config.clone());
            let rate = match config.input {
                sutra_llm_spi::AudioFormat::L16 { rate, .. } => rate,
                _ => 8000,
            };
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(AudioSessionEvent::Ready);
            Ok(Box::new(ScriptedSession {
                received_ms: Mutex::new(0),
                events: tokio::sync::Mutex::new(rx),
                tx,
                rate,
            }) as Box<dyn AudioSession>)
        })
    }
}

fn build_live(config: &LlmProviderConfig) -> Result<Arc<dyn LlmProvider>, Diagnostic> {
    Ok(Arc::new(LiveProvider {
        model: config.model.clone(),
    }))
}

inventory::submit! {
    LlmProviderFactory { provider: "test-live", build: build_live }
}

// ---- a scripted gateway --------------------------------------------------------------------------

#[derive(Default)]
struct ScriptedGateway {
    originated: Mutex<Vec<OriginateRequest>>,
    bridged: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl GatewayControl for ScriptedGateway {
    fn kind(&self) -> &str {
        "scripted"
    }
    fn has_control(&self) -> bool {
        true
    }
    async fn originate(&self, request: &OriginateRequest) -> Result<(), GatewayError> {
        self.originated.lock().unwrap().push(request.clone());
        Ok(())
    }
    async fn attach(&self, _call_id: &str) -> Result<(), GatewayError> {
        Ok(())
    }
    async fn bridge(&self, call_id: &str, _media_url: &str) -> Result<(), GatewayError> {
        self.bridged.lock().unwrap().push(call_id.to_string());
        Ok(())
    }
    async fn transfer(&self, _call_id: &str, _target: &str) -> Result<(), GatewayError> {
        Ok(())
    }
    async fn hangup(&self, _call_id: &str, _reason: Option<&str>) -> Result<(), GatewayError> {
        Ok(())
    }
}

struct Rig {
    hub: Arc<VoipHub>,
    addr: std::net::SocketAddr,
    rx: mpsc::UnboundedReceiver<sutra_channels::InboundMessage>,
    gateway: Arc<ScriptedGateway>,
    store: MemoryObjectStore,
}

async fn rig(with_recorder: bool) -> Rig {
    let (intake, rx) = FakeIntake::new();
    let defs = definitions(MEETING_CHANNELS, "meetings");
    publish_llm_channels(&defs);
    let routes = route_set_of(&defs).unwrap();
    let gateway = Arc::new(ScriptedGateway::default());
    let store = MemoryObjectStore::new(StoreLocation::parse("memory://recordings/calls").unwrap());
    let recorder = with_recorder.then(|| {
        Arc::new(Recorder::new(
            Arc::new(store.clone()) as Arc<dyn ObjectStore>,
            Some(KEK),
            "rec-key",
            60,
        ))
    });
    let hub = VoipHub::with_recorder(
        VoipConfig::generic("ws://unused", Some(TOKEN)),
        intake,
        routes,
        Arc::clone(&gateway) as Arc<dyn GatewayControl>,
        recorder,
    );
    let addr = serve(media_router(Arc::clone(&hub))).await;
    Rig {
        hub,
        addr,
        rx,
        gateway,
        store,
    }
}

fn originate_request(record: bool) -> sutra_channels::OutboundMessage {
    let mut message = outbound(
        "voip://originate",
        None,
        r#"{"callId":"meet-1","destination":"sip:+15550100@trunk","mode":"listen"}"#,
        "application/json",
    );
    if record {
        message
            .headers
            .insert("sutra-audit-capture".to_string(), "media".to_string());
    }
    message
}

fn json_of(message: &sutra_channels::InboundMessage) -> serde_json::Value {
    serde_json::from_slice(&message.body).unwrap()
}

#[tokio::test]
async fn a_listen_only_call_is_recorded_sealed_transcribed_and_reported_at_the_hang_up() {
    let mut rig = rig(true).await;
    let answer = rig
        .hub
        .operate(None, "originate", &originate_request(true), true)
        .await
        .unwrap();
    assert_eq!(answer["mode"], "listen");
    assert_eq!(answer["recorded"], true);
    rig.hub.on_gateway_event(GatewayEvent::Answered {
        call_id: "meet-1".into(),
    });
    let url = format!(
        "ws://{}/voip/bridge?token={TOKEN}&call_id=meet-1&samplerate=8000&codec=pcm",
        rig.addr
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let connected = next_on(&mut rig.rx, "progress").await;
    assert_eq!(json_of(&connected)["status"], "answered");
    let connected = next_on(&mut rig.rx, "progress").await;
    assert_eq!(json_of(&connected)["status"], "connected");
    assert_eq!(rig.gateway.bridged.lock().unwrap().as_slice(), ["meet-1"]);

    // The PIN, paced, then the consent announcement (played to the end, so recorded as given).
    let dtmf = outbound(
        "voip://dtmf",
        Some("meet-1"),
        r#"{"digits":"4321#","gapMs":30}"#,
        "application/json",
    );
    let sent = rig.hub.operate(None, "dtmf", &dtmf, true).await.unwrap();
    assert_eq!(sent["sent"], 5);
    assert!(sent.get("digits").is_none(), "the PIN is not echoed");
    let mut keys = Vec::new();
    while keys.len() < 5 {
        if let Some(Ok(Message::Text(t))) = ws.next().await {
            let frame: serde_json::Value = serde_json::from_str(&t).unwrap();
            keys.push(frame["digit"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(keys.concat(), "4321#");
    let prompt = wav_wrap(&tone(440.0, 200, 8000, 8000.0), 8000);
    let play = outbound(
        "voip://play",
        Some("meet-1"),
        &serde_json::json!({"consent": true, "audio": {
            "data": base64::engine::general_purpose::STANDARD.encode(&prompt),
            "format": "audio/wav"}})
        .to_string(),
        "application/json",
    );
    let played = rig.hub.operate(None, "play", &play, true).await.unwrap();
    assert_eq!(played["interrupted"], false);

    // The far end talks for 1.2 s: speech, then silence — and no turn is ever cut.
    let mut speech = tone(220.0, 800, 8000, 9000.0);
    speech.extend(silence(400, 8000));
    for frame in speech.chunks(160) {
        ws.send(Message::binary(pcm16_to_le(frame))).await.unwrap();
    }
    let segment = next_on(&mut rig.rx, "transcript").await;
    let body = json_of(&segment);
    assert_eq!(body["seq"], 1);
    assert_eq!(body["key"], "meet-1:1");
    assert_eq!(body["speaker"], "unknown");
    assert_eq!(body["speakerSource"], "none");
    assert_eq!(
        (body["startMs"].as_u64(), body["endMs"].as_u64()),
        (Some(100), Some(580))
    );
    // Through the channel: its first candidate was down, the session opened on the second.
    assert_eq!(body["channel"], "live-transcription");
    assert_eq!(body["provider"], "test-live");
    assert_eq!(body["model"], "live-test");
    assert!(body["features"]["durationMs"].as_u64().is_some(), "{body}");
    let keyword = next_on(&mut rig.rx, "keyword").await;
    assert_eq!(json_of(&keyword)["phrase"], "action item");
    assert_eq!(keyword.headers["x-call-id"], "meet-1");

    // The conference ends the call.
    rig.hub.on_gateway_event(GatewayEvent::Hangup {
        call_id: "meet-1".into(),
        reason: Some("bye".into()),
        hangup_by: Some("callee".into()),
        sip_status: None,
    });
    let closing = next_on(&mut rig.rx, "transcript").await;
    assert_eq!(json_of(&closing)["seq"], 2);
    assert_eq!(json_of(&closing)["text"], "Thanks, everyone.");
    let hangup = next_on(&mut rig.rx, "hangup").await;
    let ended = json_of(&hangup);
    assert_eq!(ended["mode"], "listen");
    assert_eq!(ended["transcript"]["status"], "complete");
    assert_eq!(ended["transcript"]["segments"], 2);
    assert_eq!(ended["transcript"]["keywords"], 1);
    assert_eq!(ended["transcript"]["speakerLabels"], "none");
    assert_eq!(ended["transcript"]["channel"], "live-transcription");
    assert_eq!(ended["transcript"]["model"], "live-test");
    let recording = &ended["recording"];
    assert_eq!(recording["kind"], "media");
    assert_eq!(recording["format"], "audio/wav");
    assert_eq!(recording["consentAnnounced"], true);
    let consent_at = recording["consentAnnouncedAtMs"].as_u64().unwrap();
    assert!(
        consent_at <= 1000,
        "the announcement was played at the start ({consent_at} ms)"
    );
    assert_eq!(recording["encryption"]["keyId"], "rec-key");
    let uri = recording["uri"].as_str().unwrap();
    assert_eq!(
        uri,
        "memory://recordings/calls/acme/meetings/1.0.0/meet-1.wav.sealed"
    );
    let duration = recording["durationMs"].as_u64().unwrap();
    // At least the audio the far end sent; silence fills the recording up to the call's clock.
    assert!((1150..=5000).contains(&duration), "{duration}");

    // The stored object: its hash is the audited one; it opens with the KEK only.
    let key = "acme/meetings/1.0.0/meet-1.wav.sealed";
    let stored = rig
        .store
        .get(key)
        .await
        .unwrap()
        .expect("the recording is stored");
    assert_eq!(recording["sha256"], sutra_objectstore::sha256_hex(&stored));
    assert_eq!(
        rig.store.content_type(&format!("calls/{key}")).as_deref(),
        Some(sealed::SEALED_CONTENT_TYPE)
    );
    let kek = sutra_crypto::Kek::from_secret(KEK.as_bytes());
    let (header, wav) = sealed::open(&kek, &format!("calls/{key}"), &stored).unwrap();
    assert_eq!(header.content_type, "audio/wav");
    let (samples, rate) = wav_unwrap(&wav).unwrap();
    assert_eq!(rate, 8000);
    // The consent prompt is mixed in at the start of the recording.
    let from = (consent_at * 8) as usize;
    let loud = samples[from..from + 1600]
        .iter()
        .filter(|s| s.unsigned_abs() > 4000)
        .count();
    assert!(
        loud > 400,
        "the announcement is in the recording ({loud} loud samples)"
    );
    let opened = OPENED.lock().unwrap().last().cloned().unwrap();
    assert!(opened.transcription_only);
    assert_eq!(opened.model, "live-test");
    assert_eq!(
        opened.transcription_model.as_deref(),
        Some("transcribe-test")
    );
    assert!(opened.output.is_none());
    // No call-turn was delivered (there is no such channel, and none was attempted).
    while let Ok(message) = rig.rx.try_recv() {
        assert_ne!(message.channel, "turn");
    }
}

#[tokio::test]
async fn a_recorded_call_without_a_store_is_refused() {
    let rig = rig(false).await;
    let refused = rig
        .hub
        .operate(None, "originate", &originate_request(true), true)
        .await
        .unwrap_err();
    match refused {
        OpError::Permanent(d) => assert_eq!(d.code, "SUTRA.VOIP.RECORDING_UNAVAILABLE"),
        other => panic!("{other:?}"),
    }
    assert!(rig.gateway.originated.lock().unwrap().is_empty());
    // Not recorded, listen-only: allowed.
    let answer = rig
        .hub
        .operate(None, "originate", &originate_request(false), true)
        .await
        .unwrap();
    assert_eq!(answer["recorded"], false);
}

#[tokio::test]
async fn dtmf_digits_are_checked() {
    let mut rig = rig(true).await;
    rig.hub
        .operate(None, "originate", &originate_request(false), true)
        .await
        .unwrap();
    rig.hub.on_gateway_event(GatewayEvent::Answered {
        call_id: "meet-1".into(),
    });
    let url = format!(
        "ws://{}/voip/bridge?token={TOKEN}&call_id=meet-1&samplerate=8000&codec=pcm",
        rig.addr
    );
    let (_ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let _ = next_on(&mut rig.rx, "progress").await;
    let bad = outbound(
        "voip://dtmf",
        Some("meet-1"),
        r#"{"digits":"12x"}"#,
        "application/json",
    );
    assert!(matches!(
        rig.hub.operate(None, "dtmf", &bad, true).await,
        Err(OpError::Permanent(_))
    ));
}

#[test]
fn recording_settings_come_from_the_environment() {
    let resolver: sutra_transport_spi::EnvRefResolver = |r| Ok(format!("resolved:{r}"));
    let lookup = |pairs: &'static [(&'static str, &'static str)]| {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    };
    let config = VoipConfig::from_lookup(
        lookup(&[
            (
                "SUTRA_VOIP_RECORDING_STORE",
                "s3://meeting-recordings/calls",
            ),
            ("SUTRA_VOIP_RECORDING_ENDPOINT", "http://store:9000"),
            ("SUTRA_VOIP_RECORDING_ACCESS_KEY", "env:A"),
            ("SUTRA_VOIP_RECORDING_SECRET_KEY", "env:S"),
            ("SUTRA_VOIP_RECORDING_KEK", "env:K"),
            ("SUTRA_VOIP_RECORDING_CREATE_BUCKET", "true"),
        ]),
        resolver,
    )
    .unwrap();
    let recording = config.recording.expect("a store");
    assert_eq!(recording.store.bucket, "meeting-recordings");
    assert_eq!(recording.kek_secret.as_deref(), Some("resolved:env:K"));
    assert!(recording.create_bucket);
    assert!(
        !format!("{recording:?}").contains("resolved:env:S"),
        "no secret in Debug"
    );
    // s3 without credentials, and envelope without a KEK, are refused at boot.
    assert!(VoipConfig::from_lookup(
        lookup(&[
            ("SUTRA_VOIP_RECORDING_STORE", "s3://meeting-recordings"),
            ("SUTRA_VOIP_RECORDING_KEK", "env:K"),
        ]),
        resolver,
    )
    .is_err());
    assert!(VoipConfig::from_lookup(
        lookup(&[("SUTRA_VOIP_RECORDING_STORE", "memory://meeting-recordings")]),
        resolver,
    )
    .is_err());
    let plain = VoipConfig::from_lookup(
        lookup(&[
            ("SUTRA_VOIP_RECORDING_STORE", "memory://meeting-recordings"),
            ("SUTRA_VOIP_RECORDING_ENCRYPTION", "none"),
        ]),
        resolver,
    )
    .unwrap();
    assert!(plain.recording.unwrap().kek_secret.is_none());
    let _ = RecordingSettings::memory("rec", None);
}

#[test]
fn transcription_and_keyword_channels_parse() {
    let routes = route_set_of(&definitions(MEETING_CHANNELS, "meetings")).unwrap();
    let module = &routes.modules["acme/meetings/1.0.0"];
    let transcribe = module.transcribe.as_ref().expect("transcribe settings");
    assert_eq!(transcribe.channel, "live-transcription");
    assert_eq!(transcribe.drain_ms, 1000);
    assert_eq!(module.keyword_phrases, vec!["action item", "deadline"]);
}

/// `transcribe.channel` must name an outbound, calling `llm` channel of the same deployment that
/// requires live sessions; a provider named on the voip channel itself is refused.
#[test]
fn a_transcription_channel_reference_is_validated_at_load() {
    let refused = |yaml: &str| -> String {
        let error = route_set_of(&definitions(yaml, "meetings")).unwrap_err();
        assert_eq!(
            error.code, "SUTRA.VOIP.MODEL_CHANNEL_INVALID",
            "{}",
            error.message
        );
        error.message
    };
    let transcript = |block: &str| {
        format!(
            "channels:\n  - name: transcript\n    transport: voip\n    event: call-transcript\n    \
             transcribe:\n{block}"
        )
    };
    let llm = |name: &str, extra: &str| {
        format!(
            "  - name: {name}\n    transport: llm\n    direction: outbound\n    bind: \"llm://{name}\"\n    \
             ack-mode: on-complete\n    llm:\n{extra}      candidates:\n        - provider: test-live\n          \
             model: live-test\n"
        )
    };
    // A direct provider setting.
    let message = refused(&transcript(
        "      provider: test-live\n      model: live-test\n",
    ));
    assert!(message.contains("transcribe.provider"), "{message}");
    // No channel at all.
    assert!(refused(&transcript("      drain-ms: 500\n")).contains("needs a 'channel'"));
    // A channel that does not exist.
    assert!(refused(&transcript("      channel: nowhere\n")).contains("names no channel"));
    // A channel that is not an llm channel.
    let not_llm = transcript("      channel: hangup\n")
        + "  - name: hangup\n    transport: voip\n    event: call-hangup\n";
    assert!(refused(&not_llm).contains("not an outbound 'transport: llm'"));
    // An llm channel that does not require live sessions.
    let no_realtime = transcript("      channel: words\n") + &llm("words", "");
    assert!(refused(&no_realtime).contains("require: [realtime]"));
    // A job channel.
    let job = transcript("      channel: words\n")
        + &llm("words", "      mode: job\n      require: [realtime]\n");
    assert!(refused(&job).contains("mode: job"));
    // And the valid shape loads.
    let valid = transcript("      channel: words\n") + &llm("words", "      require: [realtime]\n");
    assert!(route_set_of(&definitions(&valid, "meetings")).is_ok());
}
