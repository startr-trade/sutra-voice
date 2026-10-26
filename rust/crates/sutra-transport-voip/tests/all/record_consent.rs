//! Recording inbound calls behind a consent gate: `voip://record-start` is refused until consent
//! is given (an announcement marked `consent: true` played to the end, or the request saying
//! the caller agreed); `record-stop` answers the reference; a `record: after-consent` channel
//! starts recording only once the announcement has played.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures_util::SinkExt;
use sutra_objectstore::{MemoryObjectStore, ObjectStore, StoreLocation};
use sutra_transport_voip::audio::{pcm16_to_le, wav_wrap};
use sutra_transport_voip::config::route_set_of;
use sutra_transport_voip::gateway::{GatewayControl, GenericGateway};
use sutra_transport_voip::hub::{OpError, VoipHub};
use sutra_transport_voip::media::media_router;
use sutra_transport_voip::recording::{open_stream, Recorder};
use tokio_tungstenite::tungstenite::Message;

use super::support::*;

const TOKEN: &str = "media-secret";
const KEK: &str = "a test key-encryption secret";
const MODULE: &str = "acme/billing/1.0.0";

struct Rig {
    hub: Arc<VoipHub>,
    addr: std::net::SocketAddr,
    rx: tokio::sync::mpsc::UnboundedReceiver<sutra_channels::InboundMessage>,
    store: MemoryObjectStore,
}

async fn rig(channels: &str) -> Rig {
    let (intake, rx) = FakeIntake::new();
    let routes = route_set_of(&definitions(channels, "billing")).unwrap();
    let store = MemoryObjectStore::new(StoreLocation::parse("memory://recordings/calls").unwrap());
    let recorder = Arc::new(
        Recorder::new(
            Arc::new(store.clone()) as Arc<dyn ObjectStore>,
            Some(KEK),
            "rec-key",
            600,
        )
        .with_part_bytes(16 * 1024),
    );
    let hub = VoipHub::with_recorder(
        test_config("ws://unused", Some(TOKEN)),
        intake,
        routes,
        Arc::new(GenericGateway) as Arc<dyn GatewayControl>,
        Some(recorder),
    );
    let addr = serve(media_router(Arc::clone(&hub))).await;
    Rig {
        hub,
        addr,
        rx,
        store,
    }
}

fn play_request(call_id: &str, consent: bool) -> sutra_channels::OutboundMessage {
    let wav = wav_wrap(&tone(440.0, 300, 8000, 9000.0), 8000);
    let body = serde_json::json!({
        "audio": {"data": base64::engine::general_purpose::STANDARD.encode(wav), "format": "audio/wav"},
        "consent": consent,
    });
    outbound(
        &format!("voip://{MODULE}/play"),
        Some(call_id),
        &body.to_string(),
        "application/json",
    )
}

fn op(call_id: &str, operation: &str, body: &str) -> sutra_channels::OutboundMessage {
    outbound(
        &format!("voip://{MODULE}/{operation}"),
        Some(call_id),
        body,
        "application/json",
    )
}

async fn speak(ws: &mut Client, ms: u32) {
    for chunk in tone(200.0, ms, 8000, 3000.0).chunks(160) {
        ws.send(Message::Binary(pcm16_to_le(chunk).into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn body_of(message: &sutra_channels::InboundMessage) -> serde_json::Value {
    serde_json::from_slice(&message.body).unwrap()
}

#[tokio::test]
async fn record_start_waits_for_consent_and_record_stop_answers_the_reference() {
    let mut rig = rig(CHANNELS).await;
    let url = bridge_url(&rig.hub, rig.addr, "r1", "&called=%2B15551230000");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    next_on(&mut rig.rx, "start").await;

    // No consent yet: refused, nothing recorded.
    let refused = rig
        .hub
        .operate(
            Some(MODULE),
            "record-start",
            &op("r1", "record-start", "{}"),
            true,
        )
        .await
        .unwrap_err();
    match refused {
        OpError::Permanent(d) => assert_eq!(d.code, "SUTRA.VOIP.CONSENT_REQUIRED"),
        other => panic!("{other:?}"),
    }
    // An announcement NOT marked as the consent one does not count either.
    rig.hub
        .operate(Some(MODULE), "play", &play_request("r1", false), true)
        .await
        .unwrap();
    assert!(rig
        .hub
        .operate(
            Some(MODULE),
            "record-start",
            &op("r1", "record-start", "{}"),
            true
        )
        .await
        .is_err());

    // The consent announcement, played to the end: now recording may start.
    let played = rig
        .hub
        .operate(Some(MODULE), "play", &play_request("r1", true), true)
        .await
        .unwrap();
    assert_eq!(played["interrupted"], false);
    let started = rig
        .hub
        .operate(
            Some(MODULE),
            "record-start",
            &op("r1", "record-start", "{}"),
            true,
        )
        .await
        .unwrap();
    assert_eq!(started["status"], "recording", "{started}");
    assert_eq!(started["consent"], "announced");
    speak(&mut ws, 600).await;
    let stopped = rig
        .hub
        .operate(
            Some(MODULE),
            "record-stop",
            &op("r1", "record-stop", "{}"),
            true,
        )
        .await
        .unwrap();
    let first = &stopped["recording"];
    assert_eq!(first["kind"], "media", "{stopped}");
    assert_eq!(first["complete"], true);
    assert_eq!(first["consentAnnounced"], true);
    assert!(first["durationMs"].as_u64().unwrap() >= 500, "{first}");
    let key = "acme/billing/1.0.0/r1.wav.sealed";
    let object = rig.store.get(key).await.unwrap().expect("stored");
    assert_eq!(first["sha256"], sutra_objectstore::sha256_hex(&object));
    let kek = sutra_crypto::Kek::from_secret(KEK.as_bytes());
    assert!(
        open_stream(&kek, &format!("calls/{key}"), &object)
            .unwrap()
            .complete
    );

    // A second stretch, the flow vouching for consent itself: a second object.
    let again = rig
        .hub
        .operate(
            Some(MODULE),
            "record-start",
            &op("r1", "record-start", r#"{"consent":true}"#),
            true,
        )
        .await
        .unwrap();
    assert_eq!(again["consent"], "asserted");
    speak(&mut ws, 200).await;
    rig.hub
        .operate(Some(MODULE), "hangup", &op("r1", "hangup", "{}"), true)
        .await
        .unwrap();
    let hangup = body_of(&next_on(&mut rig.rx, "hangup").await);
    let recordings = hangup["recordings"].as_array().expect("recordings");
    assert_eq!(recordings.len(), 2, "{hangup}");
    assert!(recordings[1]["uri"]
        .as_str()
        .unwrap()
        .ends_with("/r1-2.wav.sealed"));
    assert_eq!(hangup["recording"], recordings[1]);
}

const AFTER_CONSENT: &str = r#"
channels:
  - name: start
    transport: voip
    bind: "number:+15551230000"
    record: after-consent
  - name: hangup
    transport: voip
    event: call-hangup
"#;

#[tokio::test]
async fn an_after_consent_channel_records_only_once_the_announcement_has_played() {
    let mut rig = rig(AFTER_CONSENT).await;
    // Hung up before consent: nothing was recorded.
    let url = bridge_url(&rig.hub, rig.addr, "a1", "&called=%2B15551230000");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    next_on(&mut rig.rx, "start").await;
    speak(&mut ws, 300).await;
    rig.hub
        .operate(Some(MODULE), "hangup", &op("a1", "hangup", "{}"), true)
        .await
        .unwrap();
    let hangup = body_of(&next_on(&mut rig.rx, "hangup").await);
    assert_eq!(hangup["recording"]["status"], "not-started", "{hangup}");
    assert!(rig.store.keys().iter().all(|k| !k.contains("a1.wav")));

    // Consent given: the recording starts when the announcement ends.
    let url = bridge_url(&rig.hub, rig.addr, "a2", "&called=%2B15551230000");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    next_on(&mut rig.rx, "start").await;
    rig.hub
        .operate(Some(MODULE), "play", &play_request("a2", true), true)
        .await
        .unwrap();
    speak(&mut ws, 400).await;
    rig.hub
        .operate(Some(MODULE), "hangup", &op("a2", "hangup", "{}"), true)
        .await
        .unwrap();
    let hangup = body_of(&next_on(&mut rig.rx, "hangup").await);
    let recording = &hangup["recording"];
    assert_eq!(recording["kind"], "media", "{hangup}");
    assert_eq!(recording["complete"], true);
    assert_eq!(recording["consentAnnounced"], true);
    assert!(
        recording["startMs"].as_u64().unwrap() >= 250,
        "it began after the 300 ms announcement: {recording}"
    );
    assert!(
        recording["consentAnnouncedAtMs"].as_i64().unwrap() <= 0,
        "the announcement precedes the recording: {recording}"
    );
}
