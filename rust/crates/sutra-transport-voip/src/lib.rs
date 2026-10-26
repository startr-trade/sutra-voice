//! The VoIP transport: turn-based phone and VoIP calls driven by a flow.
//!
//! The call leg's codec is negotiated per leg at call setup and is the media gateway's
//! business: the gateway transcodes at the edge and hands Sutra linear audio (or G.711), and
//! Sutra answers in kind. So this crate only ever handles PCM16, G.711 and WAV ([`audio`]),
//! all in pure Rust.
//!
//! **Media** arrives on a WebSocket served over the engine's shared HTTP listener
//! ([`media`]): the gateway-neutral media-stream protocol, or the PCM bridge a PBX dials
//! ([`protocol`]). Caller audio is segmented into turns by an energy VAD ([`vad`]); each turn
//! is delivered as a `call-turn` message (WAV body) carrying deterministic acoustic features
//! ([`features`]). Playback to the caller is paced in real time, 20 ms per frame
//! ([`pacing`]), so barge-in — caller speech during playback — is simply ceasing to send.
//!
//! **Control** goes through a [`gateway::GatewayControl`]: `generic` (no control channel) or
//! `rustpbx` ([`rustpbx`], a reconnecting RWI client).
//!
//! **Channels** (`transport: voip`, inbound): `call-start` (bound to numbers, see
//! [`binding`]), `call-turn`, `call-dtmf`, `call-progress`, `call-hangup` — the last one is the
//! natural session-end message. Every message carries `x-call-id` / `x-called` / `x-caller`.
//! **Outbound** (`voip://<operation>`): `originate`, `play`, `hangup`, `transfer`, `dtmf`,
//! `record-start`, `record-stop`, as
//! an outbox sink (the scheme is registered as a reply sink, so a bare `voip://originate` is
//! qualified with the deployment and the call's events return to it) and as an inline caller
//! (a synchronous channel-call answers with the minted call id, or the playback outcome).
//!
//! Engine-wide settings are environment variables, see [`config`].
#![forbid(unsafe_code)]

pub mod audio;
pub mod audit;
pub mod binding;
pub mod config;
pub mod features;
pub mod gateway;
pub mod hub;
pub mod listen;
pub mod media;
pub mod meeting;
pub mod pacing;
pub mod policy;
pub mod protocol;
pub mod recording;
pub mod rustpbx;
pub mod tokens;
pub mod vad;

use std::sync::{Arc, RwLock};

use axum::Router;
use sutra_channels::config::ChannelDefinition;
use sutra_channels::diag::Diagnostic;
use sutra_channels::http::EngineHandle;
use sutra_channels::sink::SinkRegistry;
use sutra_channels::{
    BoxFuture, InboundIntake, InlineCaller, InlineFailure, InlineResponse, MessageSink,
    OutboundMessage, SendOutcome,
};
use sutra_transport_spi::{
    EngineIntake, EnvRefResolver, InlineCallerFactory, ReplySinkFactory, TransportChannels,
    TransportFactory,
};
use tokio::sync::mpsc;

use crate::config::{route_set_of, GatewaySettings, VoipConfig};
use crate::gateway::{GatewayControl, GenericGateway};
use crate::hub::{OpError, VoipHub};
use crate::rustpbx::RwiClient;

/// The `transport:` discriminator and outbound URI scheme.
pub const TRANSPORT: &str = "voip";

/// Stable diagnostic codes.
pub mod codes {
    pub const BIND_INVALID: &str = "SUTRA.VOIP.BIND_INVALID";
    pub const BIND_DUPLICATE: &str = "SUTRA.VOIP.BIND_DUPLICATE";
    pub const CHANNEL_INVALID: &str = "SUTRA.VOIP.CHANNEL_INVALID";
    pub const CONFIG_INVALID: &str = "SUTRA.VOIP.CONFIG_INVALID";
    /// A `transcribe.channel` / `analyze.channel` does not name a usable `llm` channel of the
    /// deployment, or a `transcribe.*` / `analyze.*` block names a model directly.
    pub const MODEL_CHANNEL_INVALID: &str = "SUTRA.VOIP.MODEL_CHANNEL_INVALID";
    pub const OUTBOUND_INVALID: &str = "SUTRA.VOIP.OUTBOUND_INVALID";
    pub const CALL_UNKNOWN: &str = "SUTRA.VOIP.CALL_UNKNOWN";
    pub const CALL_NOT_CONNECTED: &str = "SUTRA.VOIP.CALL_NOT_CONNECTED";
    pub const AUDIO_INVALID: &str = "SUTRA.VOIP.AUDIO_INVALID";
    pub const GATEWAY_UNAVAILABLE: &str = "SUTRA.VOIP.GATEWAY_UNAVAILABLE";
    pub const GATEWAY_REJECTED: &str = "SUTRA.VOIP.GATEWAY_REJECTED";
    pub const NOT_RUNNING: &str = "SUTRA.VOIP.NOT_RUNNING";
    /// A call is to be recorded but no recording store is configured.
    pub const RECORDING_UNAVAILABLE: &str = "SUTRA.VOIP.RECORDING_UNAVAILABLE";
    /// `record-start` before consent was given (see [`crate::hub::VoipHub::record_start`]).
    pub const CONSENT_REQUIRED: &str = "SUTRA.VOIP.CONSENT_REQUIRED";
    /// An originate the toll-fraud controls refused ([`crate::policy`]).
    pub const ORIGINATE_REFUSED: &str = "SUTRA.VOIP.ORIGINATE_REFUSED";
    /// An originate over a rate limit ([`crate::policy`]); retryable.
    pub const ORIGINATE_RATE_LIMITED: &str = "SUTRA.VOIP.ORIGINATE_RATE_LIMITED";
    /// An interrupted recording the sweep recovered ([`crate::recording::Recorder::sweep`]).
    pub const RECORDING_RECOVERED: &str = "SUTRA.VOIP.RECORDING_RECOVERED";
}

/// The hub the outbound sink and inline caller reach (they are built by fn-pointer hooks with
/// no access to the transport instance). Set by [`spawn_boxed`]; the latest spawn wins.
static ACTIVE_HUB: RwLock<Option<Arc<VoipHub>>> = RwLock::new(None);

/// Install `hub` as the one the outbound seams reach.
pub fn install_hub(hub: Option<Arc<VoipHub>>) {
    *ACTIVE_HUB.write().unwrap_or_else(|p| p.into_inner()) = hub;
}

fn active_hub() -> Option<Arc<VoipHub>> {
    ACTIVE_HUB.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Build the gateway client `settings` names (spawning its connection task on the current
/// runtime) and the hub around it.
pub fn start_hub(
    config: VoipConfig,
    intake: Arc<dyn InboundIntake>,
    definitions: &[ChannelDefinition],
) -> Result<Arc<VoipHub>, Diagnostic> {
    let routes = route_set_of(definitions)?;
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let gateway: Arc<dyn GatewayControl> = match &config.gateway {
        GatewaySettings::Generic => Arc::new(GenericGateway),
        GatewaySettings::Rustpbx(settings) => RwiClient::spawn(settings.clone(), events_tx),
    };
    let create_bucket = config.recording.as_ref().is_some_and(|r| r.create_bucket);
    let hub = VoipHub::new(config, intake, routes, gateway);
    if let (true, Some(recorder)) = (create_bucket, hub.recorder()) {
        let store = Arc::clone(&recorder.store);
        tokio::spawn(async move {
            match store.ensure_bucket().await {
                Ok(()) => tracing::info!("voip: recording bucket ready"),
                Err(e) => tracing::warn!("voip: the recording bucket could not be created: {e}"),
            }
        });
    }
    hub.pump_gateway_events(events_rx);
    if let Some(settings) = hub.config().recording.clone() {
        // Recordings an earlier run left unfinished: at start-up, then periodically.
        let weak = Arc::downgrade(&hub);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            loop {
                let Some(hub) = weak.upgrade() else { return };
                let recovered = hub.sweep_recordings().await;
                if recovered > 0 {
                    tracing::info!(recovered, "voip: interrupted recordings recovered");
                }
                drop(hub);
                if settings.sweep_seconds == 0 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(settings.sweep_seconds)).await;
            }
        });
    }
    Ok(hub)
}

/// The live transport.
pub struct VoipTransport {
    hub: Arc<VoipHub>,
    router: Router,
}

impl VoipTransport {
    pub fn new(hub: Arc<VoipHub>) -> VoipTransport {
        let router = media::media_router(Arc::clone(&hub));
        VoipTransport { hub, router }
    }

    pub fn hub(&self) -> &Arc<VoipHub> {
        &self.hub
    }
}

#[async_trait::async_trait]
impl TransportChannels for VoipTransport {
    fn transport(&self) -> &str {
        TRANSPORT
    }

    /// Media arrives over the shared listener; the gateway control client is not a consumer.
    fn consumer_count(&self) -> usize {
        0
    }

    async fn rewire(&self, active: &[ChannelDefinition]) {
        match route_set_of(active) {
            Ok(routes) => self.hub.swap_routes(routes),
            Err(d) => {
                tracing::warn!(code = %d.code, "voip route rebuild failed on activation flip: {}", d.message)
            }
        }
    }

    async fn drain(&self) {
        self.hub.shutdown();
    }

    fn stop_all_detached(&self, _runtime: &tokio::runtime::Handle) {
        self.hub.shutdown();
    }

    fn inbound_router(&self) -> Option<Router> {
        Some(self.router.clone())
    }
}

fn spawn_boxed(
    definitions: &[ChannelDefinition],
    engine: EngineHandle,
    _pool: Option<sqlx::PgPool>,
    resolver: EnvRefResolver,
    handle: tokio::runtime::Handle,
) -> Result<Arc<dyn TransportChannels>, Diagnostic> {
    let config = VoipConfig::from_lookup(|key| std::env::var(key).ok(), resolver)?;
    let _runtime = handle.enter();
    let hub = start_hub(config, Arc::new(EngineIntake::new(engine)), definitions)?;
    install_hub(Some(Arc::clone(&hub)));
    Ok(Arc::new(VoipTransport::new(hub)))
}

/// The scheme's sink is the reply sink below (it must be: only a reply-sink scheme has its
/// bare destinations qualified with the deployment), so nothing registers here.
fn register_sink(_registry: &mut SinkRegistry) {}

/// Split `voip://[<tenant>/<module>/<version>/]<operation>` into its module key and operation.
pub fn parse_destination(destination: &str) -> Option<(Option<String>, String)> {
    let rest = destination.strip_prefix(&format!("{TRANSPORT}://"))?;
    let rest = rest
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_matches('/');
    if rest.is_empty() {
        return None;
    }
    match rest.rsplit_once('/') {
        Some((module, operation)) => {
            Some((Some(module.to_string()), operation.to_ascii_lowercase()))
        }
        None => Some((None, rest.to_ascii_lowercase())),
    }
}

async fn operate(message: &OutboundMessage, wait: bool) -> Result<serde_json::Value, OpError> {
    let Some(hub) = active_hub() else {
        return Err(OpError::Retryable(Diagnostic::error(
            codes::NOT_RUNNING,
            "the voip transport is not running",
        )));
    };
    let Some((module, operation)) = parse_destination(&message.destination) else {
        return Err(OpError::Permanent(Diagnostic::error(
            codes::OUTBOUND_INVALID,
            format!("'{}' is not a voip:// destination", message.destination),
        )));
    };
    let result = hub
        .operate(module.as_deref(), &operation, message, wait)
        .await;
    if let Err(OpError::Permanent(d) | OpError::Retryable(d)) = &result {
        // Names and sizes only: a request body may carry a caller's audio.
        tracing::warn!(
            destination = %message.destination,
            code = %d.code,
            content_type = message.content_type.as_deref().unwrap_or(""),
            body_bytes = message.body.len(),
            headers = ?message.headers.keys().collect::<Vec<_>>(),
            "voip: {operation} refused: {}",
            d.message
        );
    }
    result
}

/// The outbox sink for `voip://`.
pub struct VoipSink;

impl MessageSink for VoipSink {
    fn schemes(&self) -> Vec<String> {
        vec![TRANSPORT.to_string()]
    }

    fn records_media(&self) -> bool {
        true
    }

    /// Refusals, rate limits, recording recoveries and consent refusals are written to the
    /// instance's audit journal ([`sutra_channels::transport_audit`]), so the engine stamps the
    /// journal identity headers onto every message this scheme receives.
    fn writes_audit_events(&self) -> bool {
        true
    }

    fn send<'a>(&'a self, message: &'a OutboundMessage) -> BoxFuture<'a, SendOutcome> {
        Box::pin(async move {
            match operate(message, false).await {
                Ok(_) => SendOutcome::Delivered,
                Err(OpError::Retryable(d)) => SendOutcome::RetryableFailure(d),
                Err(OpError::Permanent(d)) => SendOutcome::PermanentFailure(d),
            }
        })
    }
}

/// The inline caller for `voip://`: answers with the operation's JSON result.
pub struct VoipInlineCaller;

impl InlineCaller for VoipInlineCaller {
    fn schemes(&self) -> Vec<String> {
        vec![TRANSPORT.to_string()]
    }

    fn records_media(&self) -> bool {
        true
    }

    /// Refusals, rate limits, recording recoveries and consent refusals are written to the
    /// instance's audit journal ([`sutra_channels::transport_audit`]), so the engine stamps the
    /// journal identity headers onto every message this scheme receives.
    fn writes_audit_events(&self) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        request: &'a OutboundMessage,
    ) -> BoxFuture<'a, Result<InlineResponse, InlineFailure>> {
        Box::pin(async move {
            match operate(request, true).await {
                Ok(value) => Ok(InlineResponse {
                    body: value.to_string().into_bytes(),
                    content_type: Some("application/json".to_string()),
                    headers: value
                        .get("callId")
                        .and_then(|v| v.as_str())
                        .map(|id| [(hub::headers::CALL_ID.to_string(), id.to_string())].into())
                        .unwrap_or_default(),
                }),
                Err(OpError::Retryable(d)) => Err(InlineFailure::Retryable(d)),
                Err(OpError::Permanent(d)) => Err(InlineFailure::Permanent(d)),
            }
        })
    }
}

fn build_reply_sink(_engine: EngineHandle) -> Arc<dyn MessageSink> {
    Arc::new(VoipSink)
}

fn build_inline_caller() -> Arc<dyn InlineCaller> {
    Arc::new(VoipInlineCaller)
}

inventory::submit! {
    TransportFactory {
        transport: TRANSPORT,
        spawn: spawn_boxed,
        register_sink,
        // Calls are long-lived sessions; a call-start's settle is its dispatch return.
        handles_on_complete: false,
    }
}

inventory::submit! {
    ReplySinkFactory {
        transport: TRANSPORT,
        build: build_reply_sink,
    }
}

inventory::submit! {
    InlineCallerFactory {
        transport: TRANSPORT,
        build: build_inline_caller,
    }
}
