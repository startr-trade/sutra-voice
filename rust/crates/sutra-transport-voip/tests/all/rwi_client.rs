//! The rustpbx RWI client against a stub RWI WebSocket server.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sutra_transport_voip::config::{GatewaySettings, VoipConfig};
use sutra_transport_voip::media::media_router;
use sutra_transport_voip::rustpbx::{wire, RwiSettings};
use sutra_transport_voip::start_hub;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;

use super::support::*;

const RWI_TOKEN: &str = "rwi-secret";
const MEDIA_TOKEN: &str = "media-secret";

enum StubCmd {
    Event(Value),
    Drop,
}

struct Stub {
    url: String,
    commands: mpsc::UnboundedReceiver<Value>,
    push: mpsc::UnboundedSender<StubCmd>,
    connections: mpsc::UnboundedReceiver<()>,
}

/// The upgrade check (the callback's `Result` shape is tungstenite's).
#[allow(clippy::result_large_err)]
fn check_auth(request: &Request, response: Response) -> Result<Response, ErrorResponse> {
    let auth = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    if request.uri().path() == wire::RWI_PATH && auth == Some(&format!("Bearer {RWI_TOKEN}")) {
        Ok(response)
    } else {
        let mut refused = ErrorResponse::new(None);
        *refused.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
        Err(refused)
    }
}

/// An RWI server: checks the bearer token, answers every command `command_completed`
/// (with `data.call_id`), records the commands, and pushes events / drops on request.
async fn stub() -> Stub {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (cmd_tx, commands) = mpsc::unbounded_channel();
    let (push, mut push_rx) = mpsc::unbounded_channel::<StubCmd>();
    let (conn_tx, connections) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, check_auth).await else {
                continue;
            };
            let _ = conn_tx.send(());
            loop {
                tokio::select! {
                    frame = ws.next() => {
                        let Some(Ok(Message::Text(text))) = frame else { break };
                        let command: Value = serde_json::from_str(text.as_str()).unwrap();
                        let reply = json!({
                            "type": wire::REPLY_COMPLETED,
                            "action_id": command["action_id"],
                            "action": command["action"],
                            "status": "success",
                            "data": {"call_id": command["params"]["call_id"]},
                        });
                        let _ = cmd_tx.send(command);
                        if ws.send(Message::text(reply.to_string())).await.is_err() { break; }
                    }
                    cmd = push_rx.recv() => match cmd {
                        Some(StubCmd::Event(event)) => { let _ = ws.send(Message::text(event.to_string())).await; }
                        Some(StubCmd::Drop) | None => break,
                    }
                }
            }
        }
    });
    Stub {
        url: format!("ws://{addr}{}", wire::RWI_PATH),
        commands,
        push,
        connections,
    }
}

async fn next_command(stub: &mut Stub, action: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let command = stub.commands.recv().await.unwrap();
            if command["action"] == action {
                return command;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {action} within 5 s"))
}

struct Rig {
    hub: Arc<sutra_transport_voip::hub::VoipHub>,
    addr: std::net::SocketAddr,
    rx: mpsc::UnboundedReceiver<sutra_channels::InboundMessage>,
    stub: Stub,
}

async fn rig() -> Rig {
    rig_with(CHANNELS).await
}

async fn rig_with(channels: &str) -> Rig {
    let mut stub = stub().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = VoipConfig::generic(&format!("ws://{addr}"), Some(MEDIA_TOKEN));
    let mut settings = RwiSettings::new(stub.url.clone(), RWI_TOKEN);
    settings.command_timeout = Duration::from_secs(3);
    settings.reconnect_min = Duration::from_millis(50);
    config.gateway = GatewaySettings::Rustpbx(settings);
    config.default_trunk = Some("carrier_a".into());
    let (intake, rx) = FakeIntake::new();
    let hub = start_hub(config, intake, &definitions(channels, "billing")).unwrap();
    let router = media_router(Arc::clone(&hub));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), stub.connections.recv())
        .await
        .unwrap();
    Rig {
        hub,
        addr,
        rx,
        stub,
    }
}

#[tokio::test]
async fn bad_token_is_refused_and_the_client_keeps_retrying() {
    let stub = stub().await;
    let (events, _keep) = mpsc::unbounded_channel();
    let mut settings = RwiSettings::new(stub.url.clone(), "wrong");
    settings.command_timeout = Duration::from_millis(300);
    let client = sutra_transport_voip::rustpbx::RwiClient::spawn(settings, events);
    let error = client
        .send_command(wire::CMD_ANSWER, json!({"call_id": "x"}))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        sutra_transport_voip::gateway::GatewayError::Retryable(_)
    ));
    assert!(!client.is_connected());
}

#[tokio::test]
async fn inbound_webhook_call_created_attaches_then_bridge_and_hangup() {
    let mut rig = rig().await;
    let addr = rig.addr;
    // The webhook refuses a missing token.
    assert_eq!(http_post(addr, "/voip/rustpbx/webhook", "{}").await.0, 401);
    let created = json!({
        "rwi": "1.0", "timestamp": "2026-01-01T00:00:00Z", "call_id": "in-1", "event_type": "call_created",
        "event": {"event_type": "call_created", "call_id": "in-1", "caller": "sip:+14445550000@carrier",
                  "callee": "sip:15551230000@pbx", "direction": "inbound", "trunk": null}
    });
    let (status, _) = http_post(
        addr,
        &format!("/voip/rustpbx/webhook?token={MEDIA_TOKEN}"),
        &created.to_string(),
    )
    .await;
    assert_eq!(status, 200);
    let attach = next_command(&mut rig.stub, wire::CMD_ATTACH_CALL).await;
    assert_eq!(
        attach["params"],
        json!({"call_id": "in-1", "mode": "control"})
    );

    // The IVR step provider answers with a bridge action for this call.
    let (status, body) = http_post(
        addr,
        &format!("/voip/rustpbx/ivr-step?token={MEDIA_TOKEN}"),
        r#"{"call_id":"in-1"}"#,
    )
    .await;
    assert_eq!(status, 200);
    let action: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(action["type"], "voip_bridge");
    let uri = action["create_room_uri"].as_str().unwrap().to_string();
    assert!(
        uri.starts_with(&format!(
            "ws://{addr}/voip/bridge?token={MEDIA_TOKEN}&call_id=in-1"
        )),
        "{uri}"
    );
    assert!(uri.contains("samplerate=8000") && uri.contains("codec=pcm"));

    // The PBX dials it.
    let (mut ws, _) = tokio_tungstenite::connect_async(uri).await.unwrap();
    let started = next_on(&mut rig.rx, "start").await;
    assert_eq!(started.headers["x-call-id"], "in-1");
    assert_eq!(started.headers["x-gateway"], "rustpbx");
    // No second attach for the same call.
    // The caller hangs up: the PBX reports it over RWI (it never closes the bridge socket).
    rig.stub
        .push
        .send(StubCmd::Event(json!({"event_type": "call_hangup", "call_id": "in-1", "reason": "caller", "hangup_by": "caller", "sip_status": 200})))
        .unwrap_or_else(|_| panic!());
    let ended = next_on(&mut rig.rx, "hangup").await;
    let body: Value = serde_json::from_slice(&ended.body).unwrap();
    assert_eq!(body["reason"], "caller");
    assert_eq!(body["hangupBy"], "caller");
    // Sutra closes its side.
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return true,
                _ => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed);
    assert!(rig
        .stub
        .commands
        .try_recv()
        .map(|c| c["action"] != wire::CMD_ATTACH_CALL)
        .unwrap_or(true));
}

#[tokio::test]
async fn originate_answered_transfers_to_the_bridge_and_hangup_always_goes_over_rwi() {
    let mut rig = rig().await;
    let module = "acme/billing/1.0.0";
    let request = outbound(
        "voip://acme/billing/1.0.0/originate",
        None,
        r#"{"destination":"sip:+16665550000@carrier","callerId":"+15551230000","timeoutSecs":25,"headers":{"X-Campaign":"c1"}}"#,
        "application/json",
    );
    let reply = rig
        .hub
        .operate(Some(module), "originate", &request, true)
        .await
        .unwrap();
    let call_id = reply["callId"].as_str().unwrap().to_string();
    assert!(call_id.starts_with("sutra-"));
    // Minted deterministically from the outbox key: a redelivery keeps the id and does not redial.
    let again = rig
        .hub
        .operate(Some(module), "originate", &request, true)
        .await
        .unwrap();
    assert_eq!(again["callId"], call_id.as_str());

    let originate = next_command(&mut rig.stub, wire::CMD_ORIGINATE).await;
    assert_eq!(
        originate["params"],
        json!({"call_id": call_id, "destination": "sip:+16665550000@carrier", "caller_id": "+15551230000",
               "timeout_secs": 25, "trunk": "carrier_a", "extra_headers": {"X-Campaign": "c1"}})
    );

    rig.stub
        .push
        .send(StubCmd::Event(
            json!({"event_type": "call_ringing", "call_id": call_id, "early_media": false}),
        ))
        .ok();
    let ringing = next_on(&mut rig.rx, "progress").await;
    assert_eq!(ringing.headers["x-call-status"], "ringing");
    assert_eq!(ringing.module_key, module);
    rig.stub
        .push
        .send(StubCmd::Event(
            json!({"event_type": "call_answered", "call_id": call_id}),
        ))
        .ok();
    assert_eq!(
        next_on(&mut rig.rx, "progress").await.headers["x-call-status"],
        "answered"
    );

    let transfer = next_command(&mut rig.stub, wire::CMD_TRANSFER).await;
    let target = transfer["params"]["target"].as_str().unwrap().to_string();
    let prefix = format!(
        "voip_bridge:ws://{}/voip/bridge?token={MEDIA_TOKEN}&call_id={call_id}",
        rig.addr
    );
    assert!(target.starts_with(&prefix), "{target}");

    // The PBX dials the bridge: a `connected` progress, no call-start for an originated call.
    let (_ws, _) = tokio_tungstenite::connect_async(target.trim_start_matches("voip_bridge:"))
        .await
        .unwrap();
    assert_eq!(
        next_on(&mut rig.rx, "progress").await.headers["x-call-status"],
        "connected"
    );

    // Session end: hang-up is sent over RWI.
    let hangup = outbound(
        "voip://acme/billing/1.0.0/hangup",
        Some(&call_id),
        "",
        "application/json",
    );
    rig.hub
        .operate(Some(module), "hangup", &hangup, false)
        .await
        .unwrap();
    let sent = next_command(&mut rig.stub, wire::CMD_HANGUP).await;
    assert_eq!(sent["params"]["call_id"], call_id.as_str());
    let ended = next_on(&mut rig.rx, "hangup").await;
    assert_eq!(ended.headers["x-call-id"], call_id);
    assert!(rig
        .rx
        .try_recv()
        .map(|m| m.channel != "start")
        .unwrap_or(true));

    // Even for a call this engine no longer tracks, the hang-up still goes to the PBX.
    let stale = outbound("voip://hangup", Some("gone-1"), "", "application/json");
    rig.hub
        .operate(None, "hangup", &stale, false)
        .await
        .unwrap();
    assert_eq!(
        next_command(&mut rig.stub, wire::CMD_HANGUP).await["params"]["call_id"],
        "gone-1"
    );
}

#[tokio::test]
async fn busy_outcome_and_reconnect_reattaches_controlled_calls() {
    let mut rig = rig().await;
    let module = "acme/billing/1.0.0";
    let first = outbound(
        "voip://originate",
        None,
        r#"{"destination":"sip:+1777@carrier","callId":"ob-busy"}"#,
        "application/json",
    );
    rig.hub
        .operate(Some(module), "originate", &first, true)
        .await
        .unwrap();
    next_command(&mut rig.stub, wire::CMD_ORIGINATE).await;
    rig.stub
        .push
        .send(StubCmd::Event(
            json!({"event_type": "call_busy", "call_id": "ob-busy"}),
        ))
        .ok();
    assert_eq!(
        next_on(&mut rig.rx, "progress").await.headers["x-call-status"],
        "busy"
    );
    let ended = next_on(&mut rig.rx, "hangup").await;
    assert_eq!(ended.headers["x-hangup-reason"], "busy");

    let second = outbound(
        "voip://originate",
        None,
        r#"{"destination":"sip:+1888@carrier","callId":"ob-live"}"#,
        "application/json",
    );
    rig.hub
        .operate(Some(module), "originate", &second, true)
        .await
        .unwrap();
    next_command(&mut rig.stub, wire::CMD_ORIGINATE).await;
    // The control socket drops; the client reconnects and re-attaches the live call.
    rig.stub.push.send(StubCmd::Drop).ok();
    tokio::time::timeout(Duration::from_secs(5), rig.stub.connections.recv())
        .await
        .unwrap();
    let reattach = next_command(&mut rig.stub, wire::CMD_ATTACH_CALL).await;
    assert_eq!(reattach["params"]["call_id"], "ob-live");
}

fn literal(reference: &str) -> Result<String, String> {
    Ok(reference.to_string())
}

fn rustpbx_lookup(url: &str, ca_file: Option<String>) -> impl Fn(&str) -> Option<String> {
    let url = url.to_string();
    move |key| match key {
        "SUTRA_VOIP_GATEWAY" => Some("rustpbx".to_string()),
        "SUTRA_VOIP_RWI_URL" => Some(url.clone()),
        "SUTRA_VOIP_RWI_TOKEN" => Some(RWI_TOKEN.to_string()),
        "SUTRA_VOIP_RWI_CA_FILE" => ca_file.clone(),
        _ => None,
    }
}

/// A `wss://` control URL is TLS from the first byte: the PBX sees a TLS ClientHello, never a
/// plaintext upgrade request.
#[tokio::test]
async fn a_wss_url_opens_tls_before_the_upgrade() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut settings =
        RwiSettings::new(format!("wss://localhost:{}/rwi/v1", addr.port()), RWI_TOKEN);
    settings.reconnect_max = Duration::from_millis(500);
    let (events, _events_rx) = mpsc::unbounded_channel();
    let client = sutra_transport_voip::rustpbx::RwiClient::spawn(settings, events);
    let (mut tcp, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("the client connects")
        .unwrap();
    let mut first = [0u8; 3];
    tcp.read_exact(&mut first).await.unwrap();
    assert_eq!(first[0], 0x16, "a TLS handshake record, not 'GET'");
    assert_eq!(first[1], 0x03, "TLS record version major");
    assert!(!client.is_connected());
    use sutra_transport_voip::gateway::GatewayControl;
    client.shutdown();
}

#[test]
fn the_rwi_url_must_be_a_websocket_url_and_a_ca_file_must_hold_a_certificate() {
    let refused = VoipConfig::from_lookup(rustpbx_lookup("http://pbx:8080/rwi/v1", None), literal)
        .unwrap_err();
    assert_eq!(refused.code, sutra_transport_voip::codes::CONFIG_INVALID);
    assert!(
        refused.message.contains("ws:// or wss://"),
        "{}",
        refused.message
    );

    let config = VoipConfig::from_lookup(rustpbx_lookup("wss://pbx:8443/rwi/v1", None), literal)
        .expect("a wss URL is accepted");
    let GatewaySettings::Rustpbx(settings) = config.gateway else {
        panic!("rustpbx gateway")
    };
    assert_eq!(settings.url, "wss://pbx:8443/rwi/v1");
    assert!(settings.ca_file.is_none());

    let dir = std::env::temp_dir().join(format!("voip-ca-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let empty = dir.join("empty.pem");
    std::fs::write(&empty, "not a certificate\n").unwrap();
    let refused = VoipConfig::from_lookup(
        rustpbx_lookup(
            "wss://pbx:8443/rwi/v1",
            Some(empty.to_string_lossy().to_string()),
        ),
        literal,
    )
    .unwrap_err();
    assert!(
        refused.message.contains("SUTRA_VOIP_RWI_CA_FILE"),
        "{}",
        refused.message
    );
    let missing = VoipConfig::from_lookup(
        rustpbx_lookup(
            "wss://pbx:8443/rwi/v1",
            Some(dir.join("absent.pem").to_string_lossy().to_string()),
        ),
        literal,
    )
    .unwrap_err();
    assert!(
        missing.message.contains("absent.pem"),
        "{}",
        missing.message
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `call-progress` channel with `statuses:` delivers only those: a flow that waits once for
/// the outcome of a call never sees `ringing` or `answered`. A bare number is dialled through
/// the trunk.
#[tokio::test]
async fn progress_statuses_deliver_only_the_outcome() {
    let channels = CHANNELS.replace(
        "    event: call-progress\n",
        "    event: call-progress\n    statuses: [connected, busy, no-answer, failed]\n",
    );
    let mut rig = rig_with(&channels).await;
    let module = "acme/billing/1.0.0";
    let request = outbound(
        "voip://acme/billing/1.0.0/originate",
        Some("rem-1"),
        r#"{"destination":"+1 666 555 0000"}"#,
        "application/json",
    );
    rig.hub
        .operate(Some(module), "originate", &request, true)
        .await
        .unwrap();
    let originate = next_command(&mut rig.stub, wire::CMD_ORIGINATE).await;
    assert_eq!(originate["params"]["call_id"], "rem-1");
    assert_eq!(
        originate["params"]["destination"],
        "sip:+16665550000@carrier_a"
    );
    for event in ["call_ringing", "call_answered"] {
        rig.stub
            .push
            .send(StubCmd::Event(
                json!({"event_type": event, "call_id": "rem-1"}),
            ))
            .ok();
    }
    let transfer = next_command(&mut rig.stub, wire::CMD_TRANSFER).await;
    let target = transfer["params"]["target"].as_str().unwrap().to_string();
    let (_ws, _) = tokio_tungstenite::connect_async(target.trim_start_matches("voip_bridge:"))
        .await
        .unwrap();
    let first = next_on(&mut rig.rx, "progress").await;
    assert_eq!(
        first.headers["x-call-status"], "connected",
        "ringing and answered are filtered"
    );

    let bad = CHANNELS.replace(
        "    event: call-progress\n",
        "    event: call-progress\n    statuses: [connected, picked-up]\n",
    );
    let refused =
        sutra_transport_voip::config::route_set_of(&definitions(&bad, "billing")).unwrap_err();
    assert_eq!(refused.code, sutra_transport_voip::codes::CHANNEL_INVALID);
    assert!(refused.message.contains("picked-up"), "{}", refused.message);
}
