//! Recorded prompts: `play` with `{"prompt": "<name>"}` plays `<prompts dir>/<name>.wav`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use sutra_transport_voip::audio::wav_wrap;
use sutra_transport_voip::config::{route_set_of, VoipConfig};
use sutra_transport_voip::gateway::GenericGateway;
use sutra_transport_voip::hub::{prompt_path, OpError, VoipHub};
use sutra_transport_voip::media::media_router;
use tokio_tungstenite::tungstenite::Message;

use super::support::*;

const TOKEN: &str = "media-secret";

fn prompts_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sutra-voip-prompts-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("menus")).unwrap();
    // 200 ms at 16 kHz: the call's 8 kHz bridge plays it as 10 frames of 20 ms.
    std::fs::write(
        dir.join("menus/main.wav"),
        wav_wrap(&tone(400.0, 200, 16000, 6000.0), 16000),
    )
    .unwrap();
    std::fs::write(dir.join("not-audio.wav"), b"plain text").unwrap();
    dir
}

#[test]
fn a_prompt_name_stays_inside_the_prompts_directory() {
    let dir = std::path::Path::new("/prompts");
    assert_eq!(
        prompt_path(Some(dir), "menus/main").unwrap(),
        dir.join("menus/main.wav"),
        "the .wav extension is implied"
    );
    assert_eq!(
        prompt_path(Some(dir), "goodbye.wav").unwrap(),
        dir.join("goodbye.wav")
    );
    for escape in [
        "../etc/passwd",
        "menus/../../x",
        "/abs",
        "a//b",
        ".",
        "a\\b",
        "a b",
    ] {
        assert!(
            prompt_path(Some(dir), escape).is_err(),
            "'{escape}' is refused"
        );
    }
    let unset = prompt_path(None, "menu").unwrap_err();
    assert!(unset.contains("SUTRA_VOIP_PROMPTS_DIR"), "{unset}");
}

#[tokio::test]
async fn play_names_a_recorded_prompt_and_it_is_played_paced() {
    let dir = prompts_dir();
    let (intake, mut rx) = FakeIntake::new();
    let routes = route_set_of(&definitions(CHANNELS, "billing")).unwrap();
    let mut config = test_config("ws://unused", Some(TOKEN));
    config.prompts_dir = Some(dir.clone());
    let hub = VoipHub::new(config, intake, routes, Arc::new(GenericGateway));
    let addr = serve(media_router(Arc::clone(&hub))).await;
    let url = bridge_url(&hub, addr, "p1", "&called=15551230000");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    next_on(&mut rx, "start").await;

    let play = outbound(
        "voip://play",
        Some("p1"),
        r#"{"callId":"p1","prompt":"menus/main"}"#,
        "application/json",
    );
    let reader = tokio::spawn(async move {
        let mut frames = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while let Ok(Some(Ok(message))) = tokio::time::timeout_at(deadline, ws.next()).await {
            if let Message::Binary(bytes) = message {
                assert_eq!(bytes.len(), 320, "20 ms of 8 kHz PCM16");
                frames += 1;
            }
        }
        (frames, ws)
    });
    let played = hub.operate(None, "play", &play, true).await.unwrap();
    assert_eq!(played["playedMs"], 200, "{played}");
    assert_eq!(played["interrupted"], false);
    let (frames, _ws) = reader.await.unwrap();
    assert_eq!(frames, 10);

    // A missing prompt, a file that is not WAV, and an escaping name fail permanently.
    for (body, why) in [
        (r#"{"callId":"p1","prompt":"nope"}"#, "missing"),
        (r#"{"callId":"p1","prompt":"not-audio"}"#, "not WAV"),
        (r#"{"callId":"p1","prompt":"../menus/main"}"#, "escaping"),
    ] {
        let refused = hub
            .operate(
                None,
                "play",
                &outbound("voip://play", Some("p1"), body, "application/json"),
                true,
            )
            .await
            .unwrap_err();
        assert!(matches!(refused, OpError::Permanent(_)), "{why}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_prompts_directory_must_exist() {
    let resolver: sutra_transport_spi::EnvRefResolver = |r: &str| Ok(r.to_string());
    let missing = VoipConfig::from_lookup(
        |key| (key == "SUTRA_VOIP_PROMPTS_DIR").then(|| "/no/such/prompts".to_string()),
        resolver,
    )
    .unwrap_err();
    assert!(missing.message.contains("not a directory"), "{missing:?}");
    let dir = std::env::temp_dir();
    let ok = VoipConfig::from_lookup(
        |key| (key == "SUTRA_VOIP_PROMPTS_DIR").then(|| dir.to_string_lossy().to_string()),
        resolver,
    )
    .unwrap();
    assert_eq!(
        ok.prompts_dir.as_deref(),
        Some(std::env::temp_dir().as_path())
    );
}
