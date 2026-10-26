//! The routes this transport serves over the engine's shared HTTP listener:
//!
//! | Route | What |
//! |---|---|
//! | `GET /voip/stream` + `Authorization: Bearer <media token>` | the generic media-stream protocol (WebSocket) |
//! | `GET /voip/bridge?token=<bridge token>&call_id=…&samplerate=…` | the PCM bridge a PBX dials (WebSocket) |
//! | `POST /voip/rustpbx/webhook?token=…` | the PBX's event tap (`call_created`, `call_hangup`, …) |
//! | `POST /voip/rustpbx/ivr-step[/{phase}]?token=…` | the PBX's IVR step provider: answers with a bridge action to `/voip/bridge` |
//!
//! Credentials:
//!
//! - **the media stream** takes the media token only as an `Authorization: Bearer` header; a
//!   token in its URL is refused (URLs end up in proxies' and servers' logs).
//! - **the PCM bridge** is dialled by a PBX whose bridge client sends no custom headers, so its
//!   URL carries a credential — but never the media token: a bridge token minted for that one
//!   call when it is handed to the bridge, valid for a minute and for one socket
//!   ([`crate::tokens`]). A client that can send headers may use `Authorization: Bearer
//!   <media token>` instead.
//! - **the PBX's webhook and IVR-step URLs** are static PBX configuration; they take the media
//!   token as `Authorization: Bearer` or, for a PBX that cannot send headers, as `?token=`.
//!
//! No log line here carries a URL's query string.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::audio::{AudioEncoding, AudioFormat};
use crate::gateway::GatewayEvent;
use crate::hub::{
    Call, MediaOptions, MediaOut, VoipHub, PATH_BRIDGE, PATH_IVR_STEP, PATH_STREAM, PATH_WEBHOOK,
};
use crate::protocol::{self, Framing, GenericFrame};
use crate::rustpbx;
use crate::tokens;

/// How long a generic-protocol socket may take to send its `start` frame.
const START_TIMEOUT: Duration = Duration::from_secs(10);

type Params = Query<HashMap<String, String>>;

/// The router over `hub`.
pub fn media_router(hub: Arc<VoipHub>) -> Router {
    Router::new()
        .route(PATH_STREAM, get(stream_socket))
        .route(PATH_BRIDGE, get(bridge_socket))
        .route(PATH_WEBHOOK, post(webhook))
        .route(PATH_IVR_STEP, post(ivr_step))
        .route(&format!("{PATH_IVR_STEP}/{{phase}}"), post(ivr_step_phase))
        .with_state(hub)
}

fn refuse(hub: &VoipHub) -> Response {
    if hub.config().media_token.is_none() {
        tracing::warn!("voip: request refused — no media token is configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "voip media token not configured",
        )
            .into_response();
    }
    StatusCode::UNAUTHORIZED.into_response()
}

/// The `Authorization: Bearer` credential of a request.
fn bearer_of(headers: &HeaderMap) -> Option<&str> {
    tokens::bearer(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    )
}

/// Whether a request carries the media token as a bearer header.
fn bearer_ok(hub: &VoipHub, headers: &HeaderMap) -> bool {
    bearer_of(headers).is_some_and(|token| hub.token_ok(Some(token)))
}

async fn stream_socket(
    State(hub): State<Arc<VoipHub>>,
    Query(params): Params,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if params.contains_key("token") {
        tracing::warn!(
            "voip: media-stream request refused — a token in the URL is not accepted; send it as `Authorization: Bearer`"
        );
        return refuse(&hub);
    }
    if !bearer_ok(&hub, &headers) {
        return refuse(&hub);
    }
    upgrade.on_upgrade(move |socket| serve_generic(hub, socket))
}

async fn bridge_socket(
    State(hub): State<Arc<VoipHub>>,
    Query(params): Params,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let named = params
        .get("call_id")
        .map(String::as_str)
        .unwrap_or_default();
    if !bearer_ok(&hub, &headers) {
        if let Err(refusal) = hub.bridge_token_ok(params.get("token").map(String::as_str), named) {
            tracing::warn!(
                call_id = %named,
                reason = refusal.as_str(),
                "voip: bridge socket refused — its bridge token is {}",
                refusal.as_str()
            );
            return refuse(&hub);
        }
    }
    let rate = params
        .get(rustpbx::wire::BRIDGE_Q_SAMPLERATE)
        .and_then(|r| r.parse::<u32>().ok())
        .filter(|r| *r > 0)
        .unwrap_or(hub.config().bridge_rate);
    let call_id = params.get("call_id").cloned().filter(|id| !id.is_empty());
    let caller = params.get("caller").cloned();
    let called = params.get("called").cloned();
    upgrade.on_upgrade(move |socket| serve_bridge(hub, socket, call_id, rate, caller, called))
}

/// Split a socket and spawn its writer; returns the reader half and the writer's sender.
fn split_socket(
    socket: WebSocket,
) -> (
    futures_util::stream::SplitStream<WebSocket>,
    mpsc::Sender<MediaOut>,
) {
    let (mut sink, stream) = socket.split();
    let (out, mut queue) = mpsc::channel::<MediaOut>(64);
    tokio::spawn(async move {
        while let Some(frame) = queue.recv().await {
            let sent = match frame {
                MediaOut::Binary(bytes) => sink.send(Message::Binary(bytes.into())).await,
                MediaOut::Text(text) => sink.send(Message::Text(text.into())).await,
                MediaOut::Close => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            };
            if sent.is_err() {
                break;
            }
        }
    });
    (stream, out)
}

async fn serve_bridge(
    hub: Arc<VoipHub>,
    socket: WebSocket,
    call_id: Option<String>,
    rate: u32,
    caller: Option<String>,
    called: Option<String>,
) {
    let (mut stream, out) = split_socket(socket);
    let call = match hub.open_media(
        call_id.as_deref(),
        Framing::Bridge,
        AudioFormat::l16(rate),
        caller,
        called,
        out.clone(),
        MediaOptions::default(),
    ) {
        Ok(call) => call,
        Err(reason) => {
            tracing::warn!("voip: bridge socket refused: {reason}");
            let _ = out.send(MediaOut::Close).await;
            return;
        }
    };
    drop(out);
    let mut closed = call.closed();
    let reason = loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(Message::Binary(bytes))) => {
                    // The PBX drops sub-sample messages; so do we.
                    if bytes.len() >= 2 {
                        hub.on_audio(&call, &bytes);
                    }
                }
                Some(Ok(Message::Text(text))) => match protocol::parse_bridge_text(text.as_str()) {
                    Ok(dtmf) => hub.on_dtmf(&call, &dtmf.digit),
                    Err(error) => tracing::debug!("voip: ignored bridge text frame: {error}"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break "media-closed",
                Some(Ok(_)) => {}
            },
            _ = closed.wait_for(|c| *c) => break "closed",
        }
    };
    // A PBX with a control channel drops the bridge socket when the call ends and reports the
    // end there, with who hung up and why. Give that report a moment to arrive first, so the
    // `call-hangup` carries it rather than a bare "media-closed".
    if reason == "media-closed" && hub.gateway().has_control() {
        tokio::time::sleep(CONTROL_GRACE).await;
    }
    hub.media_closed(&call, reason);
}

/// How long a dropped bridge socket waits for the control channel's own end-of-call report.
const CONTROL_GRACE: Duration = Duration::from_millis(500);

async fn serve_generic(hub: Arc<VoipHub>, socket: WebSocket) {
    let (mut stream, out) = split_socket(socket);
    let start = tokio::time::timeout(START_TIMEOUT, async {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Text(text)) => match protocol::parse_generic_text(text.as_str()) {
                    Ok(GenericFrame::Start(start)) => return Some(start),
                    Ok(_) => continue,
                    Err(error) => tracing::debug!("voip: ignored frame before start: {error}"),
                },
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => continue,
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    let Some(start) = start else {
        let _ = out.send(MediaOut::Close).await;
        return;
    };
    let format = start
        .format
        .as_deref()
        .map(|f| AudioFormat::parse(f).ok_or(f.to_string()))
        .unwrap_or(Ok(AudioFormat::l16(8000)));
    let format = match format {
        Ok(format) if format.encoding != AudioEncoding::Wav => format,
        _ => {
            tracing::warn!(
                "voip: generic stream declared an unsupported format {:?}",
                start.format
            );
            let _ = out.send(MediaOut::Close).await;
            return;
        }
    };
    let call: Arc<Call> = match hub.open_media(
        Some(&start.call_id),
        Framing::Generic,
        format,
        start.from.clone(),
        start.to.clone(),
        out.clone(),
        MediaOptions {
            tagged: start.tagged,
            headers: start.headers.clone(),
        },
    ) {
        Ok(call) => call,
        Err(reason) => {
            tracing::warn!("voip: generic stream refused: {reason}");
            let _ = out.send(MediaOut::Text(protocol::generic_hangup())).await;
            let _ = out.send(MediaOut::Close).await;
            return;
        }
    };
    // A meeting session's gateway samples video as the channel says, from now on.
    if let Some(policy) = hub.meeting_policy_frame(&call) {
        let _ = out.send(MediaOut::Text(policy)).await;
    }
    drop(out);
    let tagged = start.tagged && call.is_meeting();
    let mut closed = call.closed();
    let reason = loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(Message::Binary(bytes))) if tagged => hub.on_tagged_frame(&call, &bytes),
                Some(Ok(Message::Binary(bytes))) => hub.on_audio(&call, &bytes),
                Some(Ok(Message::Text(text))) => match protocol::parse_generic_text(text.as_str()) {
                    Ok(GenericFrame::Dtmf { digit }) => hub.on_dtmf(&call, &digit),
                    Ok(GenericFrame::ParticipantJoined(joined)) => {
                        hub.on_participant_joined(&call, &joined)
                    }
                    Ok(GenericFrame::ParticipantLeft { participant_id, reason }) => {
                        hub.on_participant_left(&call, &participant_id, reason.as_deref())
                    }
                    Ok(GenericFrame::Track { participant_id, kind, state }) => {
                        hub.on_track(&call, &participant_id, &kind, &state)
                    }
                    Ok(GenericFrame::Stop { .. }) => break "stop",
                    Ok(_) => {}
                    Err(error) => tracing::debug!("voip: ignored frame: {error}"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break "media-closed",
                Some(Ok(_)) => {}
            },
            _ = closed.wait_for(|c| *c) => break "closed",
        }
    };
    hub.media_closed(&call, reason);
}

/// The media token as a bearer header, or (for a PBX that sends no headers) as `?token=`.
fn pbx_request_ok(hub: &VoipHub, params: &HashMap<String, String>, headers: &HeaderMap) -> bool {
    bearer_ok(hub, headers) || hub.token_ok(params.get("token").map(String::as_str))
}

async fn webhook(
    State(hub): State<Arc<VoipHub>>,
    Query(params): Params,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !pbx_request_ok(&hub, &params, &headers) {
        return refuse(&hub);
    }
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "body is not JSON").into_response();
    };
    if let Some(event) = rustpbx::parse_event(&value) {
        hub.on_gateway_event(event);
    }
    StatusCode::OK.into_response()
}

/// The first string under any of `keys`, searched depth-first through objects.
fn find_text(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(Value::String(s)) = map.get(*key) {
                    if !s.is_empty() {
                        return Some(s.clone());
                    }
                }
            }
            map.values().find_map(|nested| find_text(nested, keys))
        }
        _ => None,
    }
}

fn ivr_answer(hub: &VoipHub, body: &[u8]) -> Response {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let call_id = find_text(&value, &["call_id", "callId", "session_id", "sessionId"]);
    if let Some(call_id) = &call_id {
        if hub.call(call_id).is_none() {
            hub.on_gateway_event(GatewayEvent::Created {
                call_id: call_id.clone(),
                caller: find_text(&value, &["caller", "from"]),
                callee: find_text(&value, &["called_phone", "callee", "to"]),
                direction: Some("inbound".to_string()),
            });
        }
    }
    let action = rustpbx::ivr_bridge_action(
        &hub.bridge_url(call_id.as_deref()),
        hub.config().bridge_connect_timeout_ms,
    );
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        action.to_string(),
    )
        .into_response()
}

async fn ivr_step(
    State(hub): State<Arc<VoipHub>>,
    Query(params): Params,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !pbx_request_ok(&hub, &params, &headers) {
        return refuse(&hub);
    }
    ivr_answer(&hub, &body)
}

async fn ivr_step_phase(
    State(hub): State<Arc<VoipHub>>,
    Path(phase): Path<String>,
    Query(params): Params,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !pbx_request_ok(&hub, &params, &headers) {
        return refuse(&hub);
    }
    match phase.as_str() {
        "start" => ivr_answer(&hub, &body),
        // `end` / `fail` are notifications; the call's end arrives on the control channel.
        _ => (StatusCode::OK, [("content-type", "application/json")], "{}").into_response(),
    }
}
