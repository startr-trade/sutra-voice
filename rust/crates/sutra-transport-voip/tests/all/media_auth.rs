//! Credentials stay out of URLs: the PCM bridge takes only a bridge token minted for its call
//! (expired, reused, forged and other-call tokens are refused, and so is the media token
//! itself); the media stream takes the media token only as an `Authorization` header.

use std::sync::Arc;
use std::time::Duration;

use sutra_transport_voip::config::route_set_of;
use sutra_transport_voip::gateway::GenericGateway;
use sutra_transport_voip::hub::VoipHub;
use sutra_transport_voip::media::media_router;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use super::support::*;

const TOKEN: &str = "media-secret";

async fn start(ttl_seconds: u64) -> (Arc<VoipHub>, std::net::SocketAddr) {
    let (intake, _rx) = FakeIntake::new();
    let routes = route_set_of(&definitions(CHANNELS, "billing")).unwrap();
    let mut config = test_config("ws://unused", Some(TOKEN));
    config.bridge_token_ttl_seconds = ttl_seconds;
    let hub = VoipHub::new(config, intake, routes, Arc::new(GenericGateway));
    let addr = serve(media_router(Arc::clone(&hub))).await;
    (hub, addr)
}

async fn opens(url: &str) -> bool {
    tokio_tungstenite::connect_async(url).await.is_ok()
}

#[tokio::test]
async fn the_bridge_takes_one_fresh_token_minted_for_its_call() {
    let (hub, addr) = start(60).await;
    let token = hub.mint_bridge_token("b1").unwrap();
    let url = format!("ws://{addr}/voip/bridge?token={token}&call_id=b1&called=%2B15551230000");
    // The bridge URL the hub hands a PBX carries a minted token, never the media token.
    let handed = hub.bridge_url(Some("b1"));
    assert!(
        !handed.contains(TOKEN) && handed.contains("token=v1."),
        "{handed}"
    );

    // Minted for another call: forged as far as this call is concerned.
    assert!(
        !opens(&format!(
            "ws://{addr}/voip/bridge?token={token}&call_id=b2&called=%2B15551230000"
        ))
        .await
    );
    assert!(opens(&url).await, "the fresh token opens its call's bridge");
    assert!(!opens(&url).await, "a token is good for one socket");
    // A tampered MAC, the media token itself, nothing at all.
    let mut forged = hub.mint_bridge_token("b3").unwrap();
    let last = forged.pop().unwrap();
    forged.push(if last == 'A' { 'B' } else { 'A' });
    assert!(
        !opens(&format!(
            "ws://{addr}/voip/bridge?token={forged}&call_id=b3"
        ))
        .await
    );
    assert!(!opens(&format!("ws://{addr}/voip/bridge?token={TOKEN}&call_id=b3")).await);
    assert!(!opens(&format!("ws://{addr}/voip/bridge?call_id=b3")).await);
}

#[tokio::test]
async fn an_expired_bridge_token_is_refused() {
    let (hub, addr) = start(1).await;
    let token = hub.mint_bridge_token("e1").unwrap();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(!opens(&format!("ws://{addr}/voip/bridge?token={token}&call_id=e1")).await);
}

#[tokio::test]
async fn the_media_stream_needs_the_token_as_a_header_never_in_the_url() {
    let (_hub, addr) = start(60).await;
    assert!(
        !opens(&format!("ws://{addr}/voip/stream?token={TOKEN}")).await,
        "a token in the URL is refused, even the right one"
    );
    assert!(!opens(&format!("ws://{addr}/voip/stream")).await);
    let with = |value: &str| {
        let mut request = format!("ws://{addr}/voip/stream")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", value.parse().unwrap());
        request
    };
    assert!(tokio_tungstenite::connect_async(with("Bearer wrong"))
        .await
        .is_err());
    assert!(
        tokio_tungstenite::connect_async(with(&format!("Basic {TOKEN}")))
            .await
            .is_err()
    );
    assert!(
        tokio_tungstenite::connect_async(with(&format!("Bearer {TOKEN}")))
            .await
            .is_ok()
    );
}
