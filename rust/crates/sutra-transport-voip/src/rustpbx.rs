//! The rustpbx gateway adapter. The names below were checked against rustpbx 0.5.0's source
//! (not its docs, which disagree in places) and against a running 0.5.1.
//!
//! **Control** is RWI: a WebSocket at `/rwi/v1`, authenticated at upgrade with
//! `Authorization: Bearer <token>`. Commands are `{action, action_id, params}`; every reply is
//! asynchronous — `{"type":"command_completed","action_id",…,"data"}` or
//! `{"type":"command_failed","action_id",…,"error"}`. Events are FLAT JSON objects keyed by a
//! snake_case `event_type` (`call_ringing` with `early_media`, `call_answered`, `call_busy`,
//! `call_no_answer`, `call_hangup` with `reason` / `hangup_by` / `sip_status`). There is no
//! call-error event: a failed originate surfaces as `call_hangup`. A client controls a call
//! only after `session.attach_call` (or because it originated it).
//!
//! **Inbound discovery** is not an RWI event: the PBX announces `call_created` through its
//! `[rwi_webhook]` HTTP tap (envelope `{"rwi":"1.0","timestamp","call_id","event_type","event":{…}}`),
//! which this transport serves on `POST /voip/rustpbx/webhook`; the transport then attaches.
//! The call's media reaches Sutra when the DID's IVR step provider (also served here, on
//! `POST /voip/rustpbx/ivr-step`) answers with a `voip_bridge` action pointing at the media
//! endpoint.
//!
//! **Outbound** is `call.originate` (with the `trunk` selector) → `call_answered` →
//! `call.transfer` to `voip_bridge:<media url>`. The trunk must list its codecs (`codec = […]`):
//! rustpbx builds the originated call's offer from them, and replaces the destination's host with
//! the trunk's own.
//!
//! **Hang-up** always goes over RWI: the PBX never closes the media socket, and closing Sutra's
//! side leaves a silent call up.
//!
//! Every command and event name lives in [`wire`], so a PBX upgrade that renames one is a
//! one-table change.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_rustls::rustls;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use crate::gateway::{GatewayControl, GatewayError, GatewayEvent, OriginateRequest};

/// The gateway name.
pub const RUSTPBX_GATEWAY: &str = "rustpbx";

/// Every rustpbx wire name in one place.
pub mod wire {
    /// Default RWI WebSocket path on the PBX's HTTP listener.
    pub const RWI_PATH: &str = "/rwi/v1";

    // Command envelope keys.
    pub const KEY_ACTION: &str = "action";
    pub const KEY_ACTION_ID: &str = "action_id";
    pub const KEY_PARAMS: &str = "params";
    pub const KEY_TYPE: &str = "type";
    pub const KEY_DATA: &str = "data";
    pub const KEY_ERROR: &str = "error";
    pub const KEY_EVENT_TYPE: &str = "event_type";
    /// The webhook envelope's nested event object.
    pub const KEY_EVENT: &str = "event";
    pub const KEY_CALL_ID: &str = "call_id";

    // Commands.
    pub const CMD_ORIGINATE: &str = "call.originate";
    pub const CMD_HANGUP: &str = "call.hangup";
    pub const CMD_TRANSFER: &str = "call.transfer";
    pub const CMD_ANSWER: &str = "call.answer";
    pub const CMD_SEND_DTMF: &str = "call.send_dtmf";
    pub const CMD_ATTACH_CALL: &str = "session.attach_call";
    /// `session.attach_call` mode for full control.
    pub const ATTACH_MODE_CONTROL: &str = "control";

    // Command parameter names.
    pub const P_CALL_ID: &str = "call_id";
    pub const P_DESTINATION: &str = "destination";
    pub const P_CALLER_ID: &str = "caller_id";
    pub const P_TIMEOUT_SECS: &str = "timeout_secs";
    pub const P_EXTRA_HEADERS: &str = "extra_headers";
    /// The trunk selector on `call.originate` (present in the source, absent from the docs).
    pub const P_TRUNK: &str = "trunk";
    pub const P_TARGET: &str = "target";
    pub const P_REASON: &str = "reason";
    pub const P_MODE: &str = "mode";

    // Replies.
    pub const REPLY_COMPLETED: &str = "command_completed";
    pub const REPLY_FAILED: &str = "command_failed";

    // Events (flat, snake_case `event_type`).
    pub const EVT_CALL_CREATED: &str = "call_created";
    pub const EVT_CALL_RINGING: &str = "call_ringing";
    pub const EVT_CALL_ANSWERED: &str = "call_answered";
    pub const EVT_CALL_BUSY: &str = "call_busy";
    pub const EVT_CALL_NO_ANSWER: &str = "call_no_answer";
    pub const EVT_CALL_HANGUP: &str = "call_hangup";
    pub const EVT_CALL_TRANSFER_FAILED: &str = "call_transfer_failed";

    // Event fields.
    pub const F_EARLY_MEDIA: &str = "early_media";
    pub const F_CALLER: &str = "caller";
    pub const F_CALLEE: &str = "callee";
    pub const F_CALLED_PHONE: &str = "called_phone";
    pub const F_DIRECTION: &str = "direction";
    pub const F_REASON: &str = "reason";
    pub const F_HANGUP_BY: &str = "hangup_by";
    pub const F_SIP_STATUS: &str = "sip_status";

    /// `call.transfer` target prefix that bridges the call's media to a WebSocket.
    pub const BRIDGE_TARGET_PREFIX: &str = "voip_bridge:";
    /// Bridge URL query parameters the PBX reads.
    pub const BRIDGE_Q_SAMPLERATE: &str = "samplerate";
    pub const BRIDGE_Q_CODEC: &str = "codec";
    pub const BRIDGE_CODEC_PCM: &str = "pcm";

    /// The IVR step provider's bridge action.
    pub const IVR_ACTION_VOIP_BRIDGE: &str = "voip_bridge";
    pub const IVR_KEY_CREATE_ROOM_URI: &str = "create_room_uri";
    pub const IVR_KEY_TIMEOUT_MS: &str = "timeout_ms";
}

fn text(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Parse one RWI event — flat (`{"event_type":…,"call_id":…}`) or a webhook envelope
/// (`{"event_type":…,"call_id":…,"event":{…}}`, whose nested fields win). Unknown event
/// types and replies are `None`.
pub fn parse_event(value: &Value) -> Option<GatewayEvent> {
    let event_type = text(value, wire::KEY_EVENT_TYPE)?;
    let mut fields: Map<String, Value> = value.as_object().cloned().unwrap_or_default();
    if let Some(Value::Object(nested)) = value.get(wire::KEY_EVENT) {
        for (key, nested_value) in nested {
            if !nested_value.is_null() {
                fields.insert(key.clone(), nested_value.clone());
            }
        }
    }
    let fields = Value::Object(fields);
    let call_id = text(&fields, wire::KEY_CALL_ID)?;
    Some(match event_type.as_str() {
        wire::EVT_CALL_CREATED => GatewayEvent::Created {
            call_id,
            caller: text(&fields, wire::F_CALLER),
            callee: text(&fields, wire::F_CALLED_PHONE).or_else(|| text(&fields, wire::F_CALLEE)),
            direction: text(&fields, wire::F_DIRECTION),
        },
        wire::EVT_CALL_RINGING => GatewayEvent::Ringing {
            call_id,
            early_media: fields
                .get(wire::F_EARLY_MEDIA)
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        wire::EVT_CALL_ANSWERED => GatewayEvent::Answered { call_id },
        wire::EVT_CALL_BUSY => GatewayEvent::Busy { call_id },
        wire::EVT_CALL_NO_ANSWER => GatewayEvent::NoAnswer { call_id },
        wire::EVT_CALL_HANGUP => GatewayEvent::Hangup {
            call_id,
            reason: text(&fields, wire::F_REASON),
            hangup_by: text(&fields, wire::F_HANGUP_BY),
            sip_status: fields
                .get(wire::F_SIP_STATUS)
                .and_then(Value::as_u64)
                .and_then(|s| u16::try_from(s).ok()),
        },
        wire::EVT_CALL_TRANSFER_FAILED => GatewayEvent::TransferFailed {
            call_id,
            reason: text(&fields, wire::F_REASON),
        },
        _ => return None,
    })
}

/// Build a command envelope.
pub fn command(action: &str, action_id: &str, params: Value) -> Value {
    json!({
        wire::KEY_ACTION: action,
        wire::KEY_ACTION_ID: action_id,
        wire::KEY_PARAMS: params,
    })
}

/// `call.originate` params.
pub fn originate_params(request: &OriginateRequest) -> Value {
    let mut params = Map::new();
    params.insert(wire::P_CALL_ID.into(), json!(request.call_id));
    params.insert(wire::P_DESTINATION.into(), json!(request.destination));
    if let Some(caller_id) = &request.caller_id {
        params.insert(wire::P_CALLER_ID.into(), json!(caller_id));
    }
    if let Some(timeout) = request.timeout_secs {
        params.insert(wire::P_TIMEOUT_SECS.into(), json!(timeout));
    }
    if let Some(trunk) = &request.trunk {
        params.insert(wire::P_TRUNK.into(), json!(trunk));
    }
    if !request.extra_headers.is_empty() {
        params.insert(wire::P_EXTRA_HEADERS.into(), json!(request.extra_headers));
    }
    Value::Object(params)
}

/// The `call.transfer` target bridging a call to `media_url`.
pub fn bridge_target(media_url: &str) -> String {
    format!("{}{media_url}", wire::BRIDGE_TARGET_PREFIX)
}

/// The IVR step provider's answer: bridge the call's media to `media_url`.
pub fn ivr_bridge_action(media_url: &str, timeout_ms: u64) -> Value {
    json!({
        wire::KEY_TYPE: wire::IVR_ACTION_VOIP_BRIDGE,
        wire::IVR_KEY_CREATE_ROOM_URI: media_url,
        wire::IVR_KEY_TIMEOUT_MS: timeout_ms,
    })
}

/// RWI client settings.
#[derive(Debug, Clone)]
pub struct RwiSettings {
    /// `ws://host:port/rwi/v1`, or `wss://…` for TLS (verified against the native roots plus
    /// [`Self::ca_file`]).
    pub url: String,
    /// Extra trusted CA certificates (PEM) for a `wss://` PBX with a private CA.
    pub ca_file: Option<std::path::PathBuf>,
    /// The resolved bearer token.
    pub token: String,
    /// How long a command waits for its reply (and for a connection).
    pub command_timeout: Duration,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
}

impl RwiSettings {
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> RwiSettings {
        RwiSettings {
            url: url.into(),
            ca_file: None,
            token: token.into(),
            command_timeout: Duration::from_secs(10),
            reconnect_min: Duration::from_millis(250),
            reconnect_max: Duration::from_secs(5),
        }
    }
}

type Reply = Result<Value, GatewayError>;

struct RwiShared {
    settings: RwiSettings,
    outgoing: Mutex<Option<mpsc::UnboundedSender<String>>>,
    connected: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
    /// Calls this client controls — re-attached after a reconnect (the PBX holds an orphaned
    /// call for a while before giving up on it).
    controlled: Mutex<BTreeSet<String>>,
    sequence: AtomicU64,
    events: mpsc::UnboundedSender<GatewayEvent>,
}

/// The reconnecting RWI WebSocket client.
pub struct RwiClient {
    shared: Arc<RwiShared>,
}

impl RwiClient {
    /// Start the connection loop on the current runtime. Events go to `events`.
    pub fn spawn(
        settings: RwiSettings,
        events: mpsc::UnboundedSender<GatewayEvent>,
    ) -> Arc<RwiClient> {
        let (connected, _) = watch::channel(false);
        let (stop, _) = watch::channel(false);
        let shared = Arc::new(RwiShared {
            settings,
            outgoing: Mutex::new(None),
            connected,
            stop,
            pending: Mutex::new(HashMap::new()),
            controlled: Mutex::new(BTreeSet::new()),
            sequence: AtomicU64::new(0),
            events,
        });
        tokio::spawn(connection_loop(Arc::clone(&shared)));
        Arc::new(RwiClient { shared })
    }

    pub fn is_connected(&self) -> bool {
        *self.shared.connected.borrow()
    }

    /// Calls currently under this client's control.
    pub fn controlled_calls(&self) -> Vec<String> {
        lock(&self.shared.controlled).iter().cloned().collect()
    }

    /// Send one command and await its reply's `data`.
    pub async fn send_command(&self, action: &str, params: Value) -> Result<Value, GatewayError> {
        let shared = &self.shared;
        let timeout = shared.settings.command_timeout;
        let mut connected = shared.connected.subscribe();
        if !*connected.borrow() {
            let wait = tokio::time::timeout(timeout, connected.wait_for(|up| *up)).await;
            if !matches!(wait, Ok(Ok(_))) {
                return Err(GatewayError::Retryable(format!(
                    "RWI control channel is not connected ({action})"
                )));
            }
        }
        let action_id = format!(
            "sutra-{}",
            shared.sequence.fetch_add(1, Ordering::Relaxed) + 1
        );
        let (reply_tx, reply_rx) = oneshot::channel();
        lock(&shared.pending).insert(action_id.clone(), reply_tx);
        let frame = command(action, &action_id, params).to_string();
        let sent = lock(&shared.outgoing)
            .as_ref()
            .map(|tx| tx.send(frame).is_ok())
            .unwrap_or(false);
        if !sent {
            lock(&shared.pending).remove(&action_id);
            return Err(GatewayError::Retryable(format!(
                "RWI control channel dropped before {action} was sent"
            )));
        }
        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err(GatewayError::Retryable(format!(
                "RWI control channel dropped awaiting the {action} reply"
            ))),
            Err(_) => {
                lock(&shared.pending).remove(&action_id);
                Err(GatewayError::Retryable(format!(
                    "RWI {action} reply timed out"
                )))
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

async fn connection_loop(shared: Arc<RwiShared>) {
    let mut stop = shared.stop.subscribe();
    let mut backoff = shared.settings.reconnect_min;
    loop {
        if *stop.borrow() {
            return;
        }
        match connect(&shared.settings).await {
            Ok(socket) => {
                backoff = shared.settings.reconnect_min;
                serve_connection(&shared, socket, &mut stop).await;
            }
            Err(error) => {
                tracing::warn!(url = %shared.settings.url, "RWI connect failed: {error}");
            }
        }
        // Down: fail every in-flight command so callers retry.
        lock(&shared.outgoing).take();
        shared.connected.send_replace(false);
        for (_, reply) in lock(&shared.pending).drain() {
            let _ = reply.send(Err(GatewayError::Retryable(
                "RWI control channel disconnected".to_string(),
            )));
        }
        if *stop.borrow() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(shared.settings.reconnect_max);
    }
}

/// The byte stream under the RWI WebSocket: plain TCP for `ws://`, rustls for `wss://`.
trait RwiIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> RwiIo for T {}

type Socket = tokio_tungstenite::WebSocketStream<Box<dyn RwiIo>>;

/// The TLS client configuration for a `wss://` control URL: the platform's native roots plus,
/// when configured, the PEM certificates of `settings.ca_file` (a PBX with a private CA).
pub fn tls_config(settings: &RwiSettings) -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if let Some(path) = &settings.ca_file {
        use rustls::pki_types::pem::PemObject;
        let mut added = 0usize;
        for cert in rustls::pki_types::CertificateDer::pem_file_iter(path)
            .map_err(|e| format!("RWI CA file {}: {e}", path.display()))?
        {
            let cert = cert.map_err(|e| format!("RWI CA file {}: {e}", path.display()))?;
            roots
                .add(cert)
                .map_err(|e| format!("RWI CA file {}: {e}", path.display()))?;
            added += 1;
        }
        if added == 0 {
            return Err(format!(
                "RWI CA file {} holds no certificate",
                path.display()
            ));
        }
    }
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("TLS configuration: {e}"))?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

async fn connect(settings: &RwiSettings) -> Result<Socket, String> {
    let mut request = settings
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("invalid RWI url: {e}"))?;
    let bearer = HeaderValue::from_str(&format!("Bearer {}", settings.token))
        .map_err(|_| "the RWI token is not a valid header value".to_string())?;
    request.headers_mut().insert("authorization", bearer);
    let uri = request.uri().clone();
    let tls = match uri.scheme_str() {
        Some("wss") => true,
        Some("ws") => false,
        other => {
            return Err(format!(
                "the RWI url must be ws:// or wss://, not {}",
                other.unwrap_or("<none>")
            ))
        }
    };
    let host = uri
        .host()
        .ok_or_else(|| "the RWI url has no host".to_string())?
        .trim_matches(['[', ']'])
        .to_string();
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let opening = async {
        let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| format!("connect {host}:{port}: {e}"))?;
        let io: Box<dyn RwiIo> = if tls {
            let config = tls_config(settings)?;
            let name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|_| format!("'{host}' is not a TLS server name"))?;
            let stream = tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await
                .map_err(|e| format!("TLS handshake with {host}:{port}: {e}"))?;
            Box::new(stream)
        } else {
            Box::new(tcp)
        };
        tokio_tungstenite::client_async(request, io)
            .await
            .map(|(socket, _response)| socket)
            .map_err(|e| e.to_string())
    };
    match tokio::time::timeout(settings.command_timeout, opening).await {
        Ok(result) => result,
        Err(_) => Err("connect timed out".to_string()),
    }
}

async fn serve_connection(
    shared: &Arc<RwiShared>,
    socket: Socket,
    stop: &mut watch::Receiver<bool>,
) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    // Re-attach every call this client controlled before the drop, ahead of anything else.
    let controlled: Vec<String> = lock(&shared.controlled).iter().cloned().collect();
    for call_id in controlled {
        let id = format!(
            "sutra-reattach-{}",
            shared.sequence.fetch_add(1, Ordering::Relaxed) + 1
        );
        let frame = command(
            wire::CMD_ATTACH_CALL,
            &id,
            json!({wire::P_CALL_ID: call_id, wire::P_MODE: wire::ATTACH_MODE_CONTROL}),
        );
        if sink.send(Message::text(frame.to_string())).await.is_err() {
            return;
        }
    }
    *lock(&shared.outgoing) = Some(out_tx);
    shared.connected.send_replace(true);
    let _ = shared.events.send(GatewayEvent::Connected);
    loop {
        tokio::select! {
            outgoing = out_rx.recv() => {
                let Some(frame) = outgoing else { break };
                if sink.send(Message::text(frame)).await.is_err() {
                    break;
                }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => handle_frame(shared, text.as_str()),
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            _ = stop.changed() => {
                let _ = sink.send(Message::Close(None)).await;
                break;
            }
        }
    }
}

fn handle_frame(shared: &RwiShared, frame: &str) {
    let Ok(value) = serde_json::from_str::<Value>(frame) else {
        tracing::warn!("RWI sent a non-JSON frame");
        return;
    };
    match text(&value, wire::KEY_TYPE).as_deref() {
        Some(wire::REPLY_COMPLETED) | Some(wire::REPLY_FAILED) => {
            let action_id = text(&value, wire::KEY_ACTION_ID).unwrap_or_default();
            let Some(reply) = lock(&shared.pending).remove(&action_id) else {
                if text(&value, wire::KEY_TYPE).as_deref() == Some(wire::REPLY_FAILED) {
                    tracing::warn!(
                        action_id = %action_id,
                        "RWI command failed: {}",
                        text(&value, wire::KEY_ERROR).unwrap_or_default()
                    );
                }
                return;
            };
            let result = if text(&value, wire::KEY_TYPE).as_deref() == Some(wire::REPLY_COMPLETED) {
                Ok(value.get(wire::KEY_DATA).cloned().unwrap_or(Value::Null))
            } else {
                Err(GatewayError::Permanent(
                    text(&value, wire::KEY_ERROR).unwrap_or_else(|| "command failed".to_string()),
                ))
            };
            let _ = reply.send(result);
        }
        _ => {
            if let Some(event) = parse_event(&value) {
                if let GatewayEvent::Hangup { call_id, .. }
                | GatewayEvent::Busy { call_id }
                | GatewayEvent::NoAnswer { call_id } = &event
                {
                    lock(&shared.controlled).remove(call_id);
                }
                let _ = shared.events.send(event);
            }
        }
    }
}

#[async_trait::async_trait]
impl GatewayControl for RwiClient {
    fn kind(&self) -> &str {
        RUSTPBX_GATEWAY
    }

    fn has_control(&self) -> bool {
        true
    }

    async fn originate(&self, request: &OriginateRequest) -> Result<(), GatewayError> {
        // The originating client owns the call at once; record it before the command so a
        // reconnect while it is in flight re-attaches it.
        lock(&self.shared.controlled).insert(request.call_id.clone());
        let result = self
            .send_command(wire::CMD_ORIGINATE, originate_params(request))
            .await;
        if matches!(result, Err(GatewayError::Permanent(_))) {
            lock(&self.shared.controlled).remove(&request.call_id);
        }
        result.map(|_| ())
    }

    async fn attach(&self, call_id: &str) -> Result<(), GatewayError> {
        self.send_command(
            wire::CMD_ATTACH_CALL,
            json!({wire::P_CALL_ID: call_id, wire::P_MODE: wire::ATTACH_MODE_CONTROL}),
        )
        .await?;
        lock(&self.shared.controlled).insert(call_id.to_string());
        Ok(())
    }

    async fn bridge(&self, call_id: &str, media_url: &str) -> Result<(), GatewayError> {
        self.transfer(call_id, &bridge_target(media_url)).await
    }

    async fn transfer(&self, call_id: &str, target: &str) -> Result<(), GatewayError> {
        self.send_command(
            wire::CMD_TRANSFER,
            json!({wire::P_CALL_ID: call_id, wire::P_TARGET: target}),
        )
        .await
        .map(|_| ())
    }

    async fn hangup(&self, call_id: &str, reason: Option<&str>) -> Result<(), GatewayError> {
        let mut params = json!({wire::P_CALL_ID: call_id});
        if let Some(reason) = reason {
            params[wire::P_REASON] = json!(reason);
        }
        let result = self.send_command(wire::CMD_HANGUP, params).await;
        match result {
            // The call is already gone at the PBX — the goal of a hang-up is met.
            Ok(_) | Err(GatewayError::Permanent(_)) => {
                lock(&self.shared.controlled).remove(call_id);
                Ok(())
            }
            Err(retryable) => Err(retryable),
        }
    }

    fn shutdown(&self) {
        self.shared.stop.send_replace(true);
    }
}
