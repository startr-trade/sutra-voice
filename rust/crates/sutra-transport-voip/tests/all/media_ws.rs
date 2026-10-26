//! WebSocket round trips on the media routes with a fake engine intake.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sutra_transport_voip::audio::{mulaw_encode, pcm16_to_le, wav_wrap};
use sutra_transport_voip::config::route_set_of;
use sutra_transport_voip::gateway::GenericGateway;
use sutra_transport_voip::hub::VoipHub;
use sutra_transport_voip::media::media_router;
use tokio_tungstenite::tungstenite::Message;

use super::support::*;

const TOKEN: &str = "media-secret";

async fn start() -> (
    Arc<VoipHub>,
    std::net::SocketAddr,
    tokio::sync::mpsc::UnboundedReceiver<sutra_channels::InboundMessage>,
) {
    let (intake, rx) = FakeIntake::new();
    let routes = route_set_of(&definitions(CHANNELS, "billing")).unwrap();
    let hub = VoipHub::new(
        test_config("ws://unused", Some(TOKEN)),
        intake,
        routes,
        Arc::new(GenericGateway),
    );
    let addr = serve(media_router(Arc::clone(&hub))).await;
    (hub, addr, rx)
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send_pcm(ws: &mut Client, samples: &[i16], encode: impl Fn(&[i16]) -> Vec<u8>) {
    for frame in samples.chunks(160) {
        ws.send(Message::binary(encode(frame))).await.unwrap();
    }
}

/// Count binary frames arriving within `window`; also returns the text frames seen.
async fn drain(ws: &mut Client, window: Duration) -> (usize, Vec<String>, bool) {
    let mut binary = 0;
    let mut texts = Vec::new();
    let mut closed = false;
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(next) = tokio::time::timeout_at(deadline, ws.next()).await {
        match next {
            Some(Ok(Message::Binary(_))) => binary += 1,
            Some(Ok(Message::Text(t))) => texts.push(t.to_string()),
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                closed = true;
                break;
            }
            _ => {}
        }
    }
    (binary, texts, closed)
}

#[tokio::test]
async fn bridge_round_trip_start_dtmf_turn_play_hangup() {
    let (hub, addr, mut rx) = start().await;
    // A wrong token is refused at upgrade.
    let refused =
        tokio_tungstenite::connect_async(format!("ws://{addr}/voip/bridge?token=nope&call_id=c1"))
            .await;
    assert!(refused.is_err());

    let url = bridge_url(
        &hub,
        addr,
        "c1",
        "&called=%2B15551230000&caller=%2B14445550000&samplerate=8000&codec=pcm",
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let started = next_on(&mut rx, "start").await;
    assert_eq!(started.headers["x-call-id"], "c1");
    assert_eq!(started.headers["x-called"], "+15551230000");
    assert_eq!(started.headers["x-caller"], "+14445550000");
    assert_eq!(started.module_key, "acme/billing/1.0.0");
    let body: serde_json::Value = serde_json::from_slice(&started.body).unwrap();
    assert_eq!(body["format"], "audio/L16;rate=8000");

    ws.send(Message::text(
        r#"{"type":"dtmf","digit":"5","leg_id":"caller"}"#,
    ))
    .await
    .unwrap();
    let dtmf = next_on(&mut rx, "dtmf").await;
    assert_eq!(dtmf.headers["x-dtmf-digit"], "5");

    let mut speech = tone(180.0, 600, 8000, 8000.0);
    speech.extend(silence(300, 8000));
    send_pcm(&mut ws, &speech, pcm16_to_le).await;
    let turn = next_on(&mut rx, "turn").await;
    assert_eq!(turn.content_type.as_deref(), Some("application/json"));
    assert_eq!(turn.headers["x-audio-format"], "audio/L16;rate=8000");
    assert_eq!(turn.headers["x-turn-index"], "1");
    assert_eq!(turn.headers["x-barge-in"], "false");
    // The default `turn-body: json` carries the WAV as base64, in the shape a model channel
    // takes (`audio: {data, format}`), next to the turn's features.
    let turn_body: serde_json::Value = serde_json::from_slice(&turn.body).unwrap();
    assert_eq!(turn_body["callId"], "c1");
    assert_eq!(turn_body["turnIndex"], 1);
    assert_eq!(turn_body["bargeIn"], false);
    assert_eq!(turn_body["audio"]["format"], "audio/wav");
    assert_eq!(turn_body["features"]["durationMs"], 600);
    let wav_bytes = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(turn_body["audio"]["data"].as_str().unwrap())
            .unwrap()
    };
    assert_eq!(&wav_bytes[0..4], b"RIFF");
    let features: serde_json::Value =
        serde_json::from_str(&turn.headers["x-turn-features"]).unwrap();
    assert_eq!(features["durationMs"], 600);
    assert!(
        (features["pitchHz"]["mean"].as_f64().unwrap() - 180.0).abs() < 5.0,
        "{features}"
    );

    // Play 200 ms (a WAV body at 16 kHz, resampled to the bridge's 8 kHz): 10 frames of 320 B, paced.
    let wav = wav_wrap(&tone(300.0, 200, 16000, 6000.0), 16000);
    let mut play = outbound(
        "voip://acme/billing/1.0.0/play",
        Some("c1"),
        "",
        "audio/wav",
    );
    play.body = wav;
    let started_at = tokio::time::Instant::now();
    let queued = hub
        .operate(Some("acme/billing/1.0.0"), "play", &play, false)
        .await
        .unwrap();
    assert_eq!(queued["queuedMs"], 200);
    let mut sizes = Vec::new();
    while sizes.len() < 10 {
        if let Some(Ok(Message::Binary(b))) = ws.next().await {
            sizes.push(b.len());
        }
    }
    assert!(sizes.iter().all(|s| *s == 320));
    assert!(
        started_at.elapsed() >= Duration::from_millis(170),
        "paced, not burst: {:?}",
        started_at.elapsed()
    );

    let hangup = hub
        .operate(
            None,
            "hangup",
            &outbound("voip://hangup", Some("c1"), "", "application/json"),
            false,
        )
        .await
        .unwrap();
    assert_eq!(hangup["status"], "hung-up");
    let ended = next_on(&mut rx, "hangup").await;
    let body: serde_json::Value = serde_json::from_slice(&ended.body).unwrap();
    assert_eq!(body["reason"], "hangup-requested");
    assert_eq!(body["features"]["turns"], 1);
    assert!(body["features"]["callerTalkMs"].as_u64().unwrap() >= 580);
    let (_, _, closed) = drain(&mut ws, Duration::from_secs(2)).await;
    assert!(closed, "the transport closes the media socket on hang-up");
    assert_eq!(hub.call_count(), 0);

    // A flow racing the hang-up gets "ended", not a failure; an id never seen still fails.
    let late = hub
        .operate(
            Some("acme/billing/1.0.0"),
            "play",
            &outbound("voip://play", Some("c1"), "", "audio/wav"),
            true,
        )
        .await
        .unwrap();
    assert_eq!(late["status"], "ended");
    assert_eq!(late["interrupted"], true);
    let again = hub
        .operate(
            None,
            "hangup",
            &outbound("voip://hangup", Some("c1"), "", "application/json"),
            false,
        )
        .await
        .unwrap();
    assert_eq!(again["status"], "ended");
    let unknown = hub
        .operate(
            None,
            "transfer",
            &outbound("voip://transfer", Some("never"), "", "application/json"),
            false,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        unknown,
        sutra_transport_voip::hub::OpError::Permanent(_)
    ));
}

#[tokio::test]
async fn caller_speech_during_playback_barges_in() {
    let (hub, addr, mut rx) = start().await;
    let url = bridge_url(&hub, addr, "c2", "&called=15551230000");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    next_on(&mut rx, "start").await;
    // 3 s of prompt.
    let mut play = outbound("voip://play", Some("c2"), "", "audio/L16;rate=8000");
    play.body = pcm16_to_le(&tone(300.0, 3000, 8000, 6000.0));
    hub.operate(None, "play", &play, false).await.unwrap();
    let (before, _, _) = drain(&mut ws, Duration::from_millis(300)).await;
    assert!(before >= 10, "playback running: {before}");
    // The caller talks over it.
    send_pcm(&mut ws, &tone(180.0, 200, 8000, 8000.0), pcm16_to_le).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (after, _, _) = drain(&mut ws, Duration::from_millis(500)).await;
    assert!(
        after <= 3,
        "playback stops on barge-in, got {after} frames after"
    );
    send_pcm(&mut ws, &silence(300, 8000), pcm16_to_le).await;
    let turn = next_on(&mut rx, "turn").await;
    assert_eq!(turn.headers["x-barge-in"], "true");
    let features: serde_json::Value =
        serde_json::from_str(&turn.headers["x-turn-features"]).unwrap();
    assert_eq!(features["interruptedPlayback"], true);
    assert!(features["overtalkMs"].as_u64().unwrap() > 0, "{features}");
    // A no-barge-in prompt keeps playing over speech.
    let mut firm = outbound("voip://play", Some("c2"), "", "audio/L16;rate=8000");
    firm.body = pcm16_to_le(&tone(300.0, 1000, 8000, 6000.0));
    firm.headers.insert("x-barge-in".into(), "false".into());
    hub.operate(None, "play", &firm, false).await.unwrap();
    drain(&mut ws, Duration::from_millis(100)).await;
    send_pcm(&mut ws, &tone(180.0, 200, 8000, 8000.0), pcm16_to_le).await;
    let (kept, _, _) = drain(&mut ws, Duration::from_millis(400)).await;
    assert!(kept >= 15, "x-barge-in:false keeps playing: {kept}");
}

#[tokio::test]
async fn generic_protocol_start_pcmu_clear_and_hangup_frames() {
    let (hub, addr, mut rx) = start().await;
    let mut ws = connect_stream(addr, TOKEN).await;
    ws.send(Message::text(
        r#"{"event":"start","callId":"g1","streamId":"s","format":"audio/PCMU","from":"+14445550000","to":"+15551230000","direction":"inbound"}"#,
    ))
    .await
    .unwrap();
    let started = next_on(&mut rx, "start").await;
    assert_eq!(started.headers["x-audio-format"], "audio/PCMU");
    ws.send(Message::text(r#"{"event":"dtmf","digit":"9"}"#))
        .await
        .unwrap();
    assert_eq!(next_on(&mut rx, "dtmf").await.headers["x-dtmf-digit"], "9");

    let mut play = outbound("voip://play", Some("g1"), "", "audio/L16;rate=8000");
    play.body = pcm16_to_le(&tone(300.0, 2000, 8000, 6000.0));
    hub.operate(None, "play", &play, false).await.unwrap();
    let (frames, _, _) = drain(&mut ws, Duration::from_millis(200)).await;
    assert!(frames >= 5);
    send_pcm(&mut ws, &tone(180.0, 200, 8000, 8000.0), |f| {
        f.iter().map(|s| mulaw_encode(*s)).collect()
    })
    .await;
    let (_, texts, _) = drain(&mut ws, Duration::from_millis(300)).await;
    assert!(
        texts.iter().any(|t| t.contains("\"clear\"")),
        "barge-in sends clear: {texts:?}"
    );

    hub.operate(
        None,
        "hangup",
        &outbound("voip://hangup", Some("g1"), "", "application/json"),
        false,
    )
    .await
    .unwrap();
    let (_, texts, closed) = drain(&mut ws, Duration::from_secs(2)).await;
    assert!(texts.iter().any(|t| t.contains("\"hangup\"")), "{texts:?}");
    assert!(closed);
    next_on(&mut rx, "hangup").await;
}

#[tokio::test]
async fn unbound_number_is_rejected_and_unknown_call_play_is_permanent() {
    let (hub, addr, mut rx) = start().await;
    let url = bridge_url(&hub, addr, "c3", "&called=%2B19990000000");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let (_, _, closed) = drain(&mut ws, Duration::from_secs(2)).await;
    assert!(closed);
    assert!(
        rx.try_recv().is_err(),
        "no call-start for an unbound number"
    );
    let error = hub
        .operate(
            None,
            "play",
            &outbound("voip://play", Some("nope"), "", "audio/wav"),
            false,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        sutra_transport_voip::hub::OpError::Permanent(_)
    ));
    // The generic gateway cannot originate.
    let error = hub
        .operate(
            Some("acme/billing/1.0.0"),
            "originate",
            &outbound(
                "voip://originate",
                None,
                r#"{"destination":"sip:+1@x"}"#,
                "application/json",
            ),
            false,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        sutra_transport_voip::hub::OpError::Permanent(_)
    ));
}

/// `turn-body: wav` delivers the WAV itself; a turn the engine asks to have redelivered
/// (`NackRequeue`: the instance is mid-step) is redelivered, in order, until accepted.
#[tokio::test]
async fn wav_turn_body_and_a_requeued_turn_is_redelivered() {
    let yaml = CHANNELS.replace(
        "    event: call-turn\n",
        "    event: call-turn\n    turn-body: wav\n",
    );
    let (intake, mut rx) = FakeIntake::new();
    let routes = route_set_of(&definitions(&yaml, "billing")).unwrap();
    let hub = VoipHub::new(
        test_config("ws://unused", Some(TOKEN)),
        Arc::clone(&intake) as Arc<dyn sutra_channels::InboundIntake>,
        routes,
        Arc::new(GenericGateway),
    );
    let addr = serve(media_router(Arc::clone(&hub))).await;
    let url = bridge_url(&hub, addr, "c9", "&called=%2B15551230000&samplerate=8000");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    next_on(&mut rx, "start").await;
    intake.script.lock().unwrap().extend([
        sutra_channels::AckDecision::NackRequeue,
        sutra_channels::AckDecision::NackRequeue,
    ]);
    let mut speech = tone(220.0, 400, 8000, 8000.0);
    speech.extend(silence(300, 8000));
    send_pcm(&mut ws, &speech, pcm16_to_le).await;
    let first = next_on(&mut rx, "turn").await;
    assert_eq!(first.content_type.as_deref(), Some("audio/wav"));
    assert_eq!(&first.body[0..4], b"RIFF");
    let second = next_on(&mut rx, "turn").await;
    let third = next_on(&mut rx, "turn").await;
    assert_eq!(
        first.idempotency_key, second.idempotency_key,
        "the same delivery, again"
    );
    assert_eq!(second.idempotency_key, third.idempotency_key);
    // Accepted on the third attempt: nothing further arrives for that turn.
    assert!(
        tokio::time::timeout(Duration::from_millis(400), next_on(&mut rx, "turn"))
            .await
            .is_err()
    );
}
