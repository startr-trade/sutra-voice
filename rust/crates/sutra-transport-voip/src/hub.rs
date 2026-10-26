//! The live call registry: every call the transport knows (announced, originated, or with
//! media connected), its turn detector and analytics, its paced playback, and its ordered
//! delivery lane into the engine.
//!
//! Each call delivers through its own lane — one task draining an unbounded queue in order —
//! so a slow dispatch on one call never delays another call's turns, and one call's messages
//! always reach the engine in the order they happened.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Instant;

use base64::Engine as _;
use serde_json::{json, Value};
use sutra_channels::{AckDecision, Diagnostic, InboundIntake, InboundMessage, OutboundMessage};
use tokio::sync::{mpsc, watch};

use crate::audio::{self, AudioFormat};
use crate::codes;
use crate::config::{CallSettings, EventKind, ModuleRoute, RouteSet, TurnBody, VoipConfig};
use crate::features::{analyze_turn, CallAnalytics, PitchConfig};
use crate::gateway::{GatewayControl, GatewayError, GatewayEvent, OriginateRequest};
use crate::listen::{self, ListenState, SpeakerTrack};
use crate::meeting::{self, MeetingState, SegmentSettings};
use crate::pacing::{frames_of, pace, PlaybackOutcome};
use crate::policy::{OriginateGuard, Refusal};
use crate::protocol::{self, Framing};
use crate::recording::{Recorder, RecordingOwner};
use crate::tokens::{BridgeTokens, TokenRefusal};
use crate::vad::{TurnDetector, VadEvent};

/// Header names on the inbound messages.
pub mod headers {
    pub const CALL_ID: &str = "x-call-id";
    pub const CALLED: &str = "x-called";
    pub const CALLER: &str = "x-caller";
    pub const DIRECTION: &str = "x-call-direction";
    pub const GATEWAY: &str = "x-gateway";
    pub const AUDIO_FORMAT: &str = "x-audio-format";
    pub const CONTENT_TYPE: &str = "content-type";
    pub const TURN_INDEX: &str = "x-turn-index";
    pub const TURN_END_REASON: &str = "x-turn-end-reason";
    pub const TURN_FEATURES: &str = "x-turn-features";
    pub const BARGE_IN: &str = "x-barge-in";
    pub const DTMF_DIGIT: &str = "x-dtmf-digit";
    pub const CALL_STATUS: &str = "x-call-status";
    pub const HANGUP_REASON: &str = "x-hangup-reason";
    /// Outbound: the play request's mark name (generic protocol echoes it).
    pub const MARK: &str = "x-mark";
    pub const TRANSFER_TARGET: &str = "x-transfer-target";
    pub const DESTINATION: &str = "x-destination";
    pub const CALLER_ID: &str = "x-caller-id";
    /// On `call-transcript` / `call-keyword`: the segment's sequence number in the call.
    pub const SEGMENT_SEQ: &str = "x-segment-seq";
    /// On `call-keyword`: the phrase heard.
    pub const KEYWORD: &str = "x-keyword";
    /// On `call-participant` / `call-segment` / a meeting's `call-transcript`: the participant.
    pub const PARTICIPANT_ID: &str = "x-participant-id";
    /// On `call-participant`: `joined` or `left`.
    pub const PARTICIPANT_EVENT: &str = "x-participant-event";
    /// On `call-segment`: the segment's number (from 1).
    pub const SEGMENT: &str = "x-segment";
}

/// What a media socket's opening frame said beyond the call id and format.
#[derive(Debug, Clone, Default)]
pub struct MediaOptions {
    /// One socket carries every participant of a meeting (`"streams":"tagged"`).
    pub tagged: bool,
    /// The opening frame's headers (`x-room`, …), passed on in the `call-start` body.
    pub headers: BTreeMap<String, String>,
}

/// What a meeting session's segments came to so far.
#[derive(Debug, Default)]
struct SegmentTally {
    /// `call-segment` messages delivered (one per participant and segment).
    delivered: u64,
    /// Segments whose media could not be written (delivered with `status: "failed"`).
    failed: u64,
    /// Segments not cut: the session waited for consent.
    skipped: u64,
    /// Per participant index: segments delivered.
    per_participant: BTreeMap<u16, u64>,
}

/// What an originate's `mode` asks for.
pub const MODE_LISTEN: &str = "listen";
/// The gap between two digits a `dtmf` request sends, unless it says otherwise.
pub const DEFAULT_DTMF_GAP_MS: u64 = 250;
/// How long after the media connects a `dtmfAfterAnswer` is sent, unless it says otherwise.
pub const DEFAULT_DTMF_DELAY_MS: u64 = 1500;

/// Route paths served over the shared listener.
pub const PATH_STREAM: &str = "/voip/stream";
pub const PATH_BRIDGE: &str = "/voip/bridge";
pub const PATH_WEBHOOK: &str = "/voip/rustpbx/webhook";
pub const PATH_IVR_STEP: &str = "/voip/rustpbx/ivr-step";

/// Call direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallDirection {
    Inbound,
    Outbound,
}

impl CallDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            CallDirection::Inbound => "inbound",
            CallDirection::Outbound => "outbound",
        }
    }
}

/// A frame for a media socket's writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaOut {
    Binary(Vec<u8>),
    Text(String),
    Close,
}

/// The connected media socket of a call.
#[derive(Clone)]
pub struct MediaLink {
    pub out: mpsc::Sender<MediaOut>,
    pub framing: Framing,
    pub format: AudioFormat,
}

#[derive(Debug, Clone, Default)]
struct CallMeta {
    caller: Option<String>,
    called: Option<String>,
    module: Option<Arc<ModuleRoute>>,
    start_channel: Option<String>,
    settings: CallSettings,
    last_progress: Option<String>,
    /// Digits to send once the media is up (`dtmfAfterAnswer` on the originate), and when.
    dtmf_after_answer: Option<(String, u64)>,
}

struct AudioState {
    detector: Option<TurnDetector>,
    analytics: CallAnalytics,
    turn_overtalk_ms: u64,
    turn_interrupted: bool,
    turns: u64,
}

#[derive(Clone)]
struct Delivery {
    kind: EventKind,
    message: InboundMessage,
}

/// One call.
pub struct Call {
    pub call_id: String,
    pub direction: CallDirection,
    meta: Mutex<CallMeta>,
    audio: Mutex<AudioState>,
    media: Mutex<Option<MediaLink>>,
    sequence: AtomicU64,
    lane: mpsc::UnboundedSender<Delivery>,
    playing: AtomicBool,
    barge_in_enabled: AtomicBool,
    play_generation: AtomicU64,
    play_lock: tokio::sync::Mutex<()>,
    answered: AtomicBool,
    /// An engine instance exists for this call (outbound always; inbound once the call-start
    /// was accepted) — only then are a hang-up and later events delivered.
    started: AtomicBool,
    ended: AtomicBool,
    attached: AtomicBool,
    /// A listen-only call: no turns, no barge-in (originate `mode: listen`).
    listen_only: AtomicBool,
    /// The call's audio is recorded (the originate request was marked `capture=media`, the
    /// channel says `record:`, or the flow asked with `record-start`).
    record: AtomicBool,
    /// The recording waits for consent (`record: after-consent` on the call-start channel).
    record_after_consent: AtomicBool,
    /// When (ms into the call) an announcement marked `consent: true` played to the end, on a
    /// call with no media plane of its own yet (a conversation call nobody records).
    consent_ms: Mutex<Option<u64>>,
    /// The media plane of a listen-only or recorded call, once its media is up.
    listen: Mutex<Option<ListenState>>,
    /// A meeting session (a tagged socket): every participant's media plane.
    meeting: Mutex<Option<MeetingState>>,
    /// The call's transcript segments so far (they number from 1, across participants).
    segment_seq: AtomicU64,
    /// Held while a meeting's segments are cut and written (periodic and final runs never
    /// overlap).
    segment_lock: tokio::sync::Mutex<()>,
    segment_tally: Mutex<SegmentTally>,
    /// The audit routing of the instance that owns the call, from the first request that
    /// carried it (the originate, or any later operation on the call).
    audit: Mutex<Option<sutra_channels::TransportAuditContext>>,
    created: Instant,
    closed: watch::Sender<bool>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Call {
    pub fn is_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::SeqCst)
    }

    pub fn is_listen_only(&self) -> bool {
        self.listen_only.load(Ordering::SeqCst)
    }

    pub fn is_recorded(&self) -> bool {
        self.record.load(Ordering::SeqCst)
    }

    pub fn has_media(&self) -> bool {
        lock(&self.media).is_some()
    }

    /// Whether the call is a meeting session (one tagged socket, many participants).
    pub fn is_meeting(&self) -> bool {
        lock(&self.meeting).is_some()
    }

    /// Resolves when the transport closes this call's media.
    pub fn closed(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }

    fn media(&self) -> Option<MediaLink> {
        lock(&self.media).clone()
    }
}

/// A failed outbound operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpError {
    Retryable(Diagnostic),
    Permanent(Diagnostic),
}

impl OpError {
    fn permanent(code: &str, message: impl Into<String>) -> OpError {
        OpError::Permanent(Diagnostic::error(code, message))
    }

    fn retryable(code: &str, message: impl Into<String>) -> OpError {
        OpError::Retryable(Diagnostic::error(code, message))
    }

    fn from_gateway(error: GatewayError) -> OpError {
        match error {
            GatewayError::Retryable(m) => OpError::retryable(codes::GATEWAY_UNAVAILABLE, m),
            GatewayError::Permanent(m) => OpError::permanent(codes::GATEWAY_REJECTED, m),
        }
    }
}

/// The transport's live state.
pub struct VoipHub {
    config: VoipConfig,
    intake: Arc<dyn InboundIntake>,
    routes: RwLock<Arc<RouteSet>>,
    gateway: Arc<dyn GatewayControl>,
    calls: Mutex<HashMap<String, Arc<Call>>>,
    /// Announced inbound calls awaiting media, oldest first — the fallback correlation for a
    /// bridge socket that arrives without a call id.
    awaiting_media: Mutex<VecDeque<String>>,
    /// The ids of the calls that ended most recently (newest last, at most
    /// [`ENDED_MEMORY`]): an operation on one of them is answered "ended" rather than failed,
    /// since a caller hanging up while the flow is mid-step is an ordinary race.
    ended: Mutex<VecDeque<String>>,
    /// Where recordings go (`SUTRA_VOIP_RECORDING_STORE`).
    recorder: Option<Arc<Recorder>>,
    /// Mints and checks the per-call bridge tokens (`None` without a media token).
    bridge_tokens: Option<BridgeTokens>,
    /// The toll-fraud controls on `originate`.
    originate_guard: OriginateGuard,
    /// Where this transport's audit events go.
    audit: crate::audit::AuditJournal,
    this: Weak<VoipHub>,
}

/// How many ended call ids the hub remembers.
pub const ENDED_MEMORY: usize = 4096;

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// The number (or user name) of a party: a SIP / tel URI contributes its user part (no
/// scheme, host, port or `;` parameters), so `x-caller` / `x-called` carry `+15557654321`, not
/// `sip:+15557654321@203.0.113.7:5060`. A bare value is returned trimmed.
pub fn party_number(raw: &str) -> String {
    let mut value = raw.trim();
    if let Some(open) = value.find('<') {
        value = value[open + 1..].split('>').next().unwrap_or_default();
    }
    for scheme in ["sips:", "sip:", "tel:"] {
        if value.len() >= scheme.len() && value[..scheme.len()].eq_ignore_ascii_case(scheme) {
            value = &value[scheme.len()..];
            break;
        }
    }
    let value = value.split('@').next().unwrap_or_default();
    value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn party(raw: Option<String>) -> Option<String> {
    raw.map(|v| party_number(&v)).filter(|v| !v.is_empty())
}

/// Percent-encode a URL query component (RFC 3986 unreserved characters pass).
pub fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

impl VoipHub {
    /// Build the hub around `gateway`. Gateway events must be fed to [`Self::on_gateway_event`]
    /// (see [`Self::pump_gateway_events`]).
    pub fn new(
        config: VoipConfig,
        intake: Arc<dyn InboundIntake>,
        routes: RouteSet,
        gateway: Arc<dyn GatewayControl>,
    ) -> Arc<VoipHub> {
        let recorder =
            config.recording.as_ref().and_then(|settings| {
                match Recorder::from_settings(settings) {
                    Ok(recorder) => Some(Arc::new(recorder)),
                    Err(e) => {
                        tracing::warn!("voip: the recording store is unusable: {e}");
                        None
                    }
                }
            });
        VoipHub::with_recorder(config, intake, routes, gateway, recorder)
    }

    /// [`Self::new`] with an explicit recordings sink (tests use an in-memory store).
    pub fn with_recorder(
        config: VoipConfig,
        intake: Arc<dyn InboundIntake>,
        routes: RouteSet,
        gateway: Arc<dyn GatewayControl>,
        recorder: Option<Arc<Recorder>>,
    ) -> Arc<VoipHub> {
        let audit = recorder
            .as_ref()
            .map(|r| r.audit().clone())
            .unwrap_or_else(crate::audit::AuditJournal::process_wide);
        VoipHub::with_parts(config, intake, routes, gateway, recorder, audit)
    }

    /// [`Self::with_recorder`] writing audit events to `audit` (tests inject a registry of
    /// their own; the engine's is [`crate::audit::AuditJournal::process_wide`]).
    pub fn with_parts(
        config: VoipConfig,
        intake: Arc<dyn InboundIntake>,
        routes: RouteSet,
        gateway: Arc<dyn GatewayControl>,
        recorder: Option<Arc<Recorder>>,
        audit: crate::audit::AuditJournal,
    ) -> Arc<VoipHub> {
        let bridge_tokens = config
            .media_token
            .as_deref()
            .map(|token| BridgeTokens::new(token, config.bridge_token_ttl_seconds));
        let originate_guard = OriginateGuard::new(config.originate.clone());
        Arc::new_cyclic(|this| VoipHub {
            bridge_tokens,
            originate_guard,
            config,
            intake,
            routes: RwLock::new(Arc::new(routes)),
            gateway,
            calls: Mutex::new(HashMap::new()),
            awaiting_media: Mutex::new(VecDeque::new()),
            ended: Mutex::new(VecDeque::new()),
            recorder,
            audit,
            this: this.clone(),
        })
    }

    /// The recordings sink, when one is configured.
    pub fn recorder(&self) -> Option<&Arc<Recorder>> {
        self.recorder.as_ref()
    }

    /// Where this transport's audit events go.
    pub fn audit(&self) -> &crate::audit::AuditJournal {
        &self.audit
    }

    /// Keep the audit routing a request on `call` carries (the first one wins: one call belongs
    /// to one instance), and hand it to the call's recordings in progress.
    fn note_audit_context(&self, call: &Call, message: &OutboundMessage) {
        let Some(context) = crate::audit::context_of(&message.headers) else {
            return;
        };
        {
            let mut known = lock(&call.audit);
            if known.is_some() {
                return;
            }
            *known = Some(context.clone());
        }
        if let Some(writer) = lock(&call.listen)
            .as_ref()
            .and_then(|s| s.recording.as_ref())
        {
            writer.set_audit_context(&context);
        }
        if let Some(state) = lock(&call.meeting).as_ref() {
            for participant in state.participants.values() {
                if let Some(writer) = participant.audio.recording.as_ref() {
                    writer.set_audit_context(&context);
                }
            }
        }
    }

    /// Spawn the task feeding gateway events into the hub.
    pub fn pump_gateway_events(
        self: &Arc<Self>,
        mut events: mpsc::UnboundedReceiver<GatewayEvent>,
    ) {
        let hub = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(hub) = hub.upgrade() else { return };
                hub.on_gateway_event(event);
            }
        });
    }

    pub fn config(&self) -> &VoipConfig {
        &self.config
    }

    pub fn gateway(&self) -> &Arc<dyn GatewayControl> {
        &self.gateway
    }

    pub fn routes(&self) -> Arc<RouteSet> {
        Arc::clone(&self.routes.read().unwrap_or_else(|p| p.into_inner()))
    }

    pub fn swap_routes(&self, routes: RouteSet) {
        *self.routes.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(routes);
    }

    pub fn call(&self, call_id: &str) -> Option<Arc<Call>> {
        lock(&self.calls).get(call_id).cloned()
    }

    pub fn call_count(&self) -> usize {
        lock(&self.calls).len()
    }

    /// Whether `presented` matches the configured media token (constant time). With no token
    /// configured every request is refused.
    pub fn token_ok(&self, presented: Option<&str>) -> bool {
        match (&self.config.media_token, presented) {
            (Some(expected), Some(presented)) => sutra_channels::auth::constant_time_equals(
                expected.as_bytes(),
                presented.as_bytes(),
            ),
            _ => false,
        }
    }

    /// A bridge token for `call_id` (`""` for a bridge socket that will name no call); `None`
    /// without a media token.
    pub fn mint_bridge_token(&self, call_id: &str) -> Option<String> {
        self.bridge_tokens.as_ref().map(|t| t.mint(call_id))
    }

    /// Check (and spend) a bridge socket's token for the call it names.
    pub fn bridge_token_ok(&self, token: Option<&str>, call_id: &str) -> Result<(), TokenRefusal> {
        match &self.bridge_tokens {
            Some(tokens) => tokens.verify(token, call_id),
            None => Err(TokenRefusal::Missing),
        }
    }

    /// The bridge URL the gateway dials for `call_id`: it carries a token minted for this one
    /// call, short-lived and good for one socket — never the media token itself.
    pub fn bridge_url(&self, call_id: Option<&str>) -> String {
        let mut url = format!(
            "{}{PATH_BRIDGE}?token={}",
            self.config.public_media_url,
            encode_query(
                &self
                    .mint_bridge_token(call_id.unwrap_or_default())
                    .unwrap_or_default()
            )
        );
        if let Some(call_id) = call_id {
            url.push_str(&format!("&call_id={}", encode_query(call_id)));
        }
        url.push_str(&format!(
            "&{}={}&{}={}",
            crate::rustpbx::wire::BRIDGE_Q_SAMPLERATE,
            self.config.bridge_rate,
            crate::rustpbx::wire::BRIDGE_Q_CODEC,
            crate::rustpbx::wire::BRIDGE_CODEC_PCM
        ));
        url
    }

    fn new_call(&self, call_id: &str, direction: CallDirection, meta: CallMeta) -> Arc<Call> {
        let (lane, mut queue) = mpsc::unbounded_channel::<Delivery>();
        let (closed, _) = watch::channel(false);
        let frame_ms = meta.settings.vad.frame_ms;
        let dead_air = meta.settings.dead_air_ms;
        let call = Arc::new(Call {
            call_id: call_id.to_string(),
            direction,
            meta: Mutex::new(meta),
            audio: Mutex::new(AudioState {
                detector: None,
                analytics: CallAnalytics::new(frame_ms, dead_air),
                turn_overtalk_ms: 0,
                turn_interrupted: false,
                turns: 0,
            }),
            media: Mutex::new(None),
            sequence: AtomicU64::new(0),
            lane,
            playing: AtomicBool::new(false),
            barge_in_enabled: AtomicBool::new(true),
            play_generation: AtomicU64::new(0),
            play_lock: tokio::sync::Mutex::new(()),
            answered: AtomicBool::new(false),
            started: AtomicBool::new(direction == CallDirection::Outbound),
            ended: AtomicBool::new(false),
            attached: AtomicBool::new(false),
            listen_only: AtomicBool::new(false),
            record: AtomicBool::new(false),
            record_after_consent: AtomicBool::new(false),
            consent_ms: Mutex::new(None),
            listen: Mutex::new(None),
            meeting: Mutex::new(None),
            segment_seq: AtomicU64::new(0),
            segment_lock: tokio::sync::Mutex::new(()),
            segment_tally: Mutex::new(SegmentTally::default()),
            audit: Mutex::new(None),
            created: Instant::now(),
            closed,
        });
        let hub = self.this.clone();
        let lane_call = Arc::downgrade(&call);
        let intake = Arc::clone(&self.intake);
        tokio::spawn(async move {
            while let Some(delivery) = queue.recv().await {
                let decision = deliver_with_requeue(&*intake, &delivery).await;
                if delivery.kind == EventKind::CallStart {
                    let (Some(hub), Some(call)) = (hub.upgrade(), lane_call.upgrade()) else {
                        continue;
                    };
                    if decision != AckDecision::Ack {
                        tracing::info!(call_id = %call.call_id, "voip: call-start refused; hanging up");
                        hub.end_call(&call, EndOptions::rejected("refused"));
                    }
                } else if decision != AckDecision::Ack {
                    tracing::warn!(
                        kind = delivery.kind.as_str(),
                        "voip: delivery not accepted ({decision:?})"
                    );
                }
            }
        });
        lock(&self.calls).insert(call_id.to_string(), Arc::clone(&call));
        call
    }

    fn enqueue(
        &self,
        call: &Call,
        kind: EventKind,
        mut extra: BTreeMap<String, String>,
        body: Vec<u8>,
        content_type: &str,
    ) {
        let meta = lock(&call.meta).clone();
        let Some(module) = meta.module.as_ref() else {
            return;
        };
        let channel = if kind == EventKind::CallStart {
            meta.start_channel.clone()
        } else {
            module.channel(kind).map(str::to_string)
        };
        let Some(channel) = channel else {
            tracing::debug!(kind = kind.as_str(), module = %module.module_key, "voip: no channel for event; skipped");
            return;
        };
        let mut message_headers = BTreeMap::new();
        message_headers.insert(headers::CALL_ID.to_string(), call.call_id.clone());
        message_headers.insert(
            headers::DIRECTION.to_string(),
            call.direction.as_str().to_string(),
        );
        message_headers.insert(
            headers::GATEWAY.to_string(),
            self.gateway.kind().to_string(),
        );
        if let Some(called) = &meta.called {
            message_headers.insert(headers::CALLED.to_string(), called.clone());
        }
        if let Some(caller) = &meta.caller {
            message_headers.insert(headers::CALLER.to_string(), caller.clone());
        }
        message_headers.insert(headers::CONTENT_TYPE.to_string(), content_type.to_string());
        message_headers.append(&mut extra);
        let sequence = call.sequence.fetch_add(1, Ordering::SeqCst);
        let message = InboundMessage {
            tenant: module.tenant.clone(),
            module_key: module.module_key.clone(),
            channel,
            headers: message_headers,
            body: body.into(),
            content_type: Some(content_type.to_string()),
            idempotency_key: format!("voip:{}:{}:{sequence}", call.call_id, kind.as_str()),
            explicit_event_id: true,
            received_at: now_rfc3339(),
            cloud_event: None,
        };
        let _ = call.lane.send(Delivery { kind, message });
    }

    fn enqueue_json(
        &self,
        call: &Call,
        kind: EventKind,
        extra: BTreeMap<String, String>,
        body: Value,
    ) {
        self.enqueue(
            call,
            kind,
            extra,
            body.to_string().into_bytes(),
            "application/json",
        );
    }

    fn progress(&self, call: &Call, status: &str, mut detail: Value) {
        {
            let mut meta = lock(&call.meta);
            if meta.last_progress.as_deref() == Some(status) {
                return;
            }
            meta.last_progress = Some(status.to_string());
            let filtered = meta
                .module
                .as_ref()
                .and_then(|m| m.progress_statuses.as_ref())
                .is_some_and(|allowed| !allowed.contains(status));
            if filtered {
                return;
            }
        }
        detail["callId"] = json!(call.call_id);
        detail["status"] = json!(status);
        let extra = BTreeMap::from([(headers::CALL_STATUS.to_string(), status.to_string())]);
        self.enqueue_json(call, EventKind::CallProgress, extra, detail);
    }

    // ---- listen-only calls -----------------------------------------------------------------

    /// The media of `call` is up: fork it to the recorder and the transcriber when the call is
    /// recorded or listen-only, and send the digits an originate asked to send after answer.
    fn start_listening(&self, call: &Arc<Call>, rate: u32) {
        if let Some((digits, delay_ms)) = lock(&call.meta).dtmf_after_answer.take() {
            let target = Arc::clone(call);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                if let Err(OpError::Permanent(d) | OpError::Retryable(d)) =
                    send_digits(&target, &digits, DEFAULT_DTMF_GAP_MS).await
                {
                    tracing::warn!(call_id = %target.call_id, "voip: dtmf after answer failed: {}", d.message);
                }
            });
        }
        let (listen_only, record) = (call.is_listen_only(), call.is_recorded());
        if !listen_only && !record {
            return;
        }
        let max_seconds = self
            .recorder
            .as_ref()
            .map(|r| r.max_seconds)
            .unwrap_or(crate::config::DEFAULT_RECORDING_MAX_SECONDS);
        let mut state =
            ListenState::new(rate, record, max_seconds).with_window(listen::TAP_WINDOW_SECONDS);
        state.record_after_consent = call.record_after_consent.load(Ordering::SeqCst);
        let module = lock(&call.meta).module.clone();
        if let Some(module) = module.clone().filter(|_| listen_only) {
            if let Some(settings) = module.transcribe.clone() {
                let (frames, done) = listen::spawn_transcriber(
                    self.this.clone(),
                    Arc::clone(call),
                    settings,
                    module.keyword_phrases.clone(),
                    rate,
                    module.module_key.clone(),
                    None,
                );
                state.frames = Some(frames);
                state.transcript = Some(done);
            }
        }
        if record && !state.record_after_consent {
            let owner = self.recording_owner(call);
            let _ = self.begin_call_recording(call, owner, &mut state);
        }
        *lock(&call.listen) = Some(state);
    }

    /// Who a recording of `call` belongs to.
    fn recording_owner(&self, call: &Call) -> (String, RecordingOwner) {
        let module = lock(&call.meta).module.clone();
        let module_key = module
            .as_ref()
            .map(|m| m.module_key.clone())
            .unwrap_or_default();
        let owner = RecordingOwner {
            call_id: call.call_id.clone(),
            tenant: module
                .as_ref()
                .map(|m| m.tenant.clone())
                .unwrap_or_default(),
            module_key: module_key.clone(),
            participant_id: None,
            participant_index: None,
            audit: lock(&call.audit).clone(),
        };
        (module_key, owner)
    }

    /// Start recording a (non-meeting) call into `state`: `<call>.wav`, then `<call>-2.wav`, …
    /// for each later `record-start`. `owner` is [`Self::recording_owner`], taken before
    /// `state`'s lock.
    fn begin_call_recording(
        &self,
        call: &Call,
        (module_key, owner): (String, RecordingOwner),
        state: &mut ListenState,
    ) -> Result<(), String> {
        let Some(recorder) = self.recorder.as_ref() else {
            return Err("no recording store is configured".to_string());
        };
        let name = match state.finished.len() {
            0 => call.call_id.clone(),
            n => format!("{}-{}", call.call_id, n + 1),
        };
        let started = state.start_recording(recorder, &module_key, &[&name], owner);
        if let Err(e) = &started {
            tracing::warn!(call_id = %call.call_id, "voip: the recording could not start: {e}");
        }
        started
    }

    /// Start the recording of every present participant of a meeting session that is due one
    /// (recorded, and — for `after-consent` — consented) and has none.
    fn start_participant_recordings(&self, call: &Call) {
        let Some(recorder) = self.recorder.as_ref() else {
            return;
        };
        let (module_key, base) = self.recording_owner(call);
        let mut guard = lock(&call.meeting);
        let Some(state) = guard.as_mut() else { return };
        let due = state.record
            && (!state.record_after_consent
                || state.consent_at_ms.is_some()
                || state.record_started);
        if !due {
            return;
        }
        for participant in state.participants.values_mut() {
            if participant.left_at_ms.is_some() || participant.audio.recording.is_some() {
                continue;
            }
            let name = match participant.audio.finished.len() {
                0 => format!("p{}", participant.index),
                n => format!("p{}-{}", participant.index, n + 1),
            };
            let owner = RecordingOwner {
                participant_id: Some(participant.participant_id.clone()),
                participant_index: Some(participant.index),
                ..base.clone()
            };
            if let Err(e) = participant.audio.start_recording(
                recorder,
                &module_key,
                &[&call.call_id, &name],
                owner,
            ) {
                tracing::warn!(call_id = %call.call_id, index = participant.index, "voip: a participant recording could not start: {e}");
            }
        }
    }

    /// Consent was just given on `call` (an announcement marked `consent: true` played to the
    /// end): start the recordings that were waiting for it.
    fn on_consent(&self, call: &Arc<Call>) {
        if call.is_meeting() {
            self.start_participant_recordings(call);
            return;
        }
        let owner = self.recording_owner(call);
        let mut guard = lock(&call.listen);
        if let Some(state) = guard.as_mut() {
            if state.record && state.record_after_consent && state.recording.is_none() {
                let _ = self.begin_call_recording(call, owner, state);
            }
        }
    }

    /// Finish a listen-only / recorded call after its end: drain the transcriber, write the
    /// recording, then deliver the hang-up with both outcomes.
    fn finish_listening(
        &self,
        call: &Arc<Call>,
        mut state: ListenState,
        extra: BTreeMap<String, String>,
        mut body: Value,
    ) {
        let Some(hub) = self.this.upgrade() else {
            return;
        };
        let call = Arc::clone(call);
        let drain_ms = lock(&call.meta)
            .module
            .as_ref()
            .and_then(|m| m.transcribe.as_ref().map(|t| t.drain_ms))
            .unwrap_or(0);
        tokio::spawn(async move {
            // The recording runs to the hang-up, and closing the frames channel tells the
            // transcriber the call is over.
            state.catch_up();
            state.frames = None;
            if let Some(done) = state.transcript.take() {
                let bound = std::time::Duration::from_millis(drain_ms + 15_000);
                body["transcript"] = match tokio::time::timeout(bound, done).await {
                    Ok(Ok(summary)) => summary,
                    _ => json!({"status": "failed", "detail": "the transcription did not finish"}),
                };
            }
            if state.record {
                let (last, all) =
                    listen::finish_recording(hub.recorder.as_deref(), &call, &mut state).await;
                body["recording"] = last;
                body["recordings"] = json!(all);
            }
            body["mode"] = json!(if call.is_listen_only() {
                MODE_LISTEN
            } else {
                "conversation"
            });
            hub.enqueue_json(&call, EventKind::CallHangup, extra, body);
        });
    }

    /// Deliver one final transcript segment on the `call-transcript` channel; answers its
    /// sequence number in the call and its start in call (or session) time. A meeting
    /// participant's segment (`track`) is attributed to that participant exactly.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn deliver_transcript_segment(
        &self,
        call: &Call,
        item_id: &str,
        text: &str,
        speaker: Option<&str>,
        start_ms: u64,
        end_ms: u64,
        origin: &listen::TranscriberSource,
        track: Option<&SpeakerTrack>,
    ) -> (u64, u64) {
        let seq = call.segment_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let offset = track.map(|t| t.offset_ms).unwrap_or(0);
        let (start, end) = (start_ms + offset, end_ms + offset);
        let features = match track {
            Some(track) => {
                let mut guard = lock(&call.meeting);
                guard
                    .as_mut()
                    .map(|state| {
                        let features = state
                            .participants
                            .get(&track.index)
                            .map(|p| listen::segment_features(&p.audio, start_ms, end_ms))
                            .unwrap_or(Value::Null);
                        state.add_line(
                            track.index,
                            meeting::TranscriptLine {
                                start_ms: start,
                                end_ms: end,
                                text: text.to_string(),
                            },
                        );
                        features
                    })
                    .unwrap_or(Value::Null)
            }
            None => lock(&call.listen)
                .as_ref()
                .map(|state| listen::segment_features(state, start_ms, end_ms))
                .unwrap_or(Value::Null),
        };
        let (speaker, source) = match (track, speaker) {
            (Some(track), _) => (track.label.as_str(), "track"),
            (None, Some(speaker)) => (speaker, "provider"),
            (None, None) => (listen::UNKNOWN_SPEAKER, "none"),
        };
        let mut body = json!({
            "callId": call.call_id,
            "seq": seq,
            "key": format!("{}:{seq}", call.call_id),
            "itemId": item_id,
            "speaker": speaker,
            "speakerSource": source,
            "startMs": start,
            "endMs": end,
            "text": text,
            "channel": origin.channel,
            "provider": origin.provider,
            "model": origin.model,
            "features": features,
        });
        let mut extra = BTreeMap::from([(headers::SEGMENT_SEQ.to_string(), seq.to_string())]);
        if let Some(track) = track {
            body["participantId"] = json!(track.participant_id);
            body["participantIndex"] = json!(track.index);
            extra.insert(
                headers::PARTICIPANT_ID.to_string(),
                track.participant_id.clone(),
            );
        }
        self.enqueue_json(call, EventKind::CallTranscript, extra, body);
        (seq, start)
    }

    /// Deliver a `call-keyword`: `phrase` was heard in segment `seq`.
    pub(crate) fn deliver_keyword(
        &self,
        call: &Call,
        phrase: &str,
        seq: u64,
        at_ms: u64,
        text: &str,
    ) {
        let body = json!({
            "callId": call.call_id,
            "phrase": phrase,
            "segmentSeq": seq,
            "atMs": at_ms,
            "text": text,
        });
        let extra = BTreeMap::from([
            (headers::SEGMENT_SEQ.to_string(), seq.to_string()),
            (headers::KEYWORD.to_string(), phrase.to_string()),
        ]);
        self.enqueue_json(call, EventKind::CallKeyword, extra, body);
    }

    // ---- meeting sessions -------------------------------------------------------------------

    /// A meeting session's media is up: every second, cut the segments that have closed (a
    /// `call-segment` channel), write their media and deliver their references, and let go of
    /// what no segment needs any more — so a participant's memory stays bounded however long
    /// the meeting runs.
    fn start_meeting(&self, call: &Arc<Call>) {
        let hub = self.this.clone();
        let weak = Arc::downgrade(call);
        tokio::spawn(async move {
            let tick = std::time::Duration::from_millis(meeting::SEGMENT_TICK_MS);
            loop {
                tokio::time::sleep(tick).await;
                let (Some(hub), Some(call)) = (hub.upgrade(), weak.upgrade()) else {
                    return;
                };
                if call.is_ended() {
                    return;
                }
                let _running = call.segment_lock.lock().await;
                let taken = {
                    let mut guard = lock(&call.meeting);
                    let Some(state) = guard.as_mut() else { return };
                    // Transcript lines arrive a little after their audio.
                    let upto = state.elapsed_ms().saturating_sub(meeting::SEGMENT_LAG_MS);
                    state.take_segments(upto, false)
                };
                hub.deliver_segments(&call, taken).await;
            }
        });
    }

    /// The `policy` frame a meeting session's gateway is sent once the session is accepted.
    pub fn meeting_policy_frame(&self, call: &Call) -> Option<String> {
        let guard = lock(&call.meeting);
        let video = &guard.as_ref()?.video;
        Some(protocol::generic_policy(
            video.enabled,
            video.fps,
            &video.format,
            video.max_width,
            video.quality,
        ))
    }

    /// A participant of a meeting session is present: track it, transcribe its own audio, and
    /// tell the flow (`call-participant`).
    pub fn on_participant_joined(&self, call: &Arc<Call>, joined: &protocol::ParticipantJoined) {
        if call.is_ended() {
            return;
        }
        let (module, vad) = {
            let meta = lock(&call.meta);
            (meta.module.clone(), meta.settings.vad)
        };
        let (body, track, rate) = {
            let mut guard = lock(&call.meeting);
            let Some(state) = guard.as_mut() else { return };
            let rate = state.rate;
            let Some(participant) = state.join(joined, &vad) else {
                return;
            };
            let track = SpeakerTrack {
                participant_id: participant.participant_id.clone(),
                index: participant.index,
                label: participant.label.clone(),
                offset_ms: participant.joined_at_ms,
            };
            let body = json!({
                "callId": call.call_id,
                "event": "joined",
                "participantId": participant.participant_id,
                "participantIndex": participant.index,
                "label": participant.label,
                "role": participant.role,
                "declaredRole": participant.declared_role,
                "segmented": participant.segmented,
                "atMs": participant.joined_at_ms,
            });
            (body, track, rate)
        };
        if let Some(module) = &module {
            if let Some(settings) = module.transcribe.clone() {
                let index = track.index;
                let (frames, done) = listen::spawn_transcriber(
                    self.this.clone(),
                    Arc::clone(call),
                    settings,
                    module.keyword_phrases.clone(),
                    rate,
                    module.module_key.clone(),
                    Some(track.clone()),
                );
                if let Some(participant) = lock(&call.meeting)
                    .as_mut()
                    .and_then(|state| state.participants.get_mut(&index))
                {
                    participant.audio.frames = Some(frames);
                    participant.transcript = Some(done);
                }
            }
        }
        self.start_participant_recordings(call);
        let extra = BTreeMap::from([
            (headers::PARTICIPANT_ID.to_string(), track.participant_id),
            (headers::PARTICIPANT_EVENT.to_string(), "joined".to_string()),
        ]);
        self.enqueue_json(call, EventKind::CallParticipant, extra, body);
    }

    /// A participant of a meeting session left.
    pub fn on_participant_left(
        &self,
        call: &Arc<Call>,
        participant_id: &str,
        reason: Option<&str>,
    ) {
        if call.is_ended() {
            return;
        }
        let body = {
            let mut guard = lock(&call.meeting);
            let Some(state) = guard.as_mut() else { return };
            let Some(index) = state.leave(participant_id, reason.map(str::to_string)) else {
                return;
            };
            let participant = &state.participants[&index];
            json!({
                "callId": call.call_id,
                "event": "left",
                "participantId": participant.participant_id,
                "participantIndex": index,
                "label": participant.label,
                "role": participant.role,
                "reason": reason,
                "atMs": participant.left_at_ms,
                "talkMs": participant.speech.voiced_ms,
            })
        };
        let extra = BTreeMap::from([
            (
                headers::PARTICIPANT_ID.to_string(),
                participant_id.to_string(),
            ),
            (headers::PARTICIPANT_EVENT.to_string(), "left".to_string()),
        ]);
        self.enqueue_json(call, EventKind::CallParticipant, extra, body);
    }

    /// A track of a meeting participant was (un)subscribed (kept for the hang-up's summary).
    pub fn on_track(&self, call: &Call, participant_id: &str, kind: &str, state: &str) {
        let mut guard = lock(&call.meeting);
        let Some(meeting) = guard.as_mut() else {
            return;
        };
        let Some(index) = meeting.ids.get(participant_id).copied() else {
            return;
        };
        if let Some(participant) = meeting.participants.get_mut(&index) {
            participant.tracks.insert(
                kind.chars().take(16).collect(),
                state.chars().take(16).collect(),
            );
        }
    }

    /// One tagged binary frame of a meeting session: a participant's audio, or a sampled frame
    /// of its camera. A moment the audio flags asks the gateway for a frame of everyone.
    pub fn on_tagged_frame(&self, call: &Arc<Call>, bytes: &[u8]) {
        if call.is_ended() {
            return;
        }
        let (header, payload) = match protocol::parse_tagged(bytes) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::debug!(call_id = %call.call_id, "voip: ignored tagged frame: {error}");
                return;
            }
        };
        match header.kind {
            protocol::TaggedKind::Audio => {
                let Some(link) = call.media() else { return };
                let Ok((samples, _)) = audio::decode(payload, &link.format) else {
                    return;
                };
                let reason = lock(&call.meeting)
                    .as_mut()
                    .and_then(|state| state.on_audio(header.index, &samples));
                if let Some(reason) = reason {
                    let _ = link
                        .out
                        .try_send(MediaOut::Text(protocol::generic_sample(None, reason)));
                }
            }
            protocol::TaggedKind::Video => {
                if let Some(state) = lock(&call.meeting).as_mut() {
                    state.on_frame(header.index, payload, header.requested());
                }
            }
        }
    }

    /// Write each segment's media to the recording store (the audio as a WAV file, each frame as
    /// an image; sealed when recordings are) and deliver its references on `call-segment`, one
    /// message per participant and segment. `taken` is what [`MeetingState::take_segments`]
    /// answered.
    async fn deliver_segments(&self, call: &Arc<Call>, taken: (Vec<meeting::SegmentInput>, u64)) {
        let (inputs, skipped) = taken;
        lock(&call.segment_tally).skipped += skipped;
        if inputs.is_empty() {
            return;
        }
        let module_key = lock(&call.meta)
            .module
            .as_ref()
            .map(|m| m.module_key.clone())
            .unwrap_or_default();
        for input in inputs {
            let body = self.write_segment(call, &module_key, input).await;
            {
                let mut tally = lock(&call.segment_tally);
                tally.delivered += 1;
                if body["status"] != "complete" {
                    tally.failed += 1;
                }
                let index = body["participantIndex"].as_u64().unwrap_or(0) as u16;
                *tally.per_participant.entry(index).or_default() += 1;
            }
            let extra = BTreeMap::from([
                (
                    headers::PARTICIPANT_ID.to_string(),
                    body["participantId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                ),
                (headers::SEGMENT.to_string(), body["segment"].to_string()),
            ]);
            self.enqueue_json(call, EventKind::CallSegment, extra, body);
        }
    }

    /// One participant's segment: its media written, and the `call-segment` body.
    async fn write_segment(
        &self,
        call: &Call,
        module_key: &str,
        input: meeting::SegmentInput,
    ) -> Value {
        let participant = input.participant_id.clone();
        let reference =
            |stored: crate::recording::StoredMedia, format: &str, start: u64, end: u64| {
                json!({
                    "kind": listen::MEDIA_REFERENCE_KIND,
                    "uri": stored.uri,
                    "sha256": stored.sha256,
                    "bytes": stored.bytes,
                    "format": format,
                    "participant": participant,
                    "startMs": start,
                    "endMs": end,
                    "sealed": stored.sealed,
                })
            };
        let audio_ms = input.samples.len() as u64 * 1000 / u64::from(input.rate.max(1));
        let features = {
            let (samples, rate, threshold) =
                (input.samples.clone(), input.rate, input.energy_threshold);
            tokio::task::spawn_blocking(move || {
                meeting::acoustic_features(&samples, rate, threshold)
            })
            .await
            .unwrap_or(Value::Null)
        };
        let mut body = json!({
            "callId": call.call_id,
            "key": format!("{}:{}:{}", call.call_id, input.index, input.segment),
            "participantId": input.participant_id,
            "participantIndex": input.index,
            "participant": format!("P{}", input.index),
            "label": input.label,
            "role": input.role,
            "declaredRole": input.declared_role,
            "segment": input.segment,
            "startMs": input.start_ms,
            "endMs": input.end_ms,
            "speaking": input.speaking,
            "talkMs": input.talk_ms,
            "audioMs": audio_ms,
            "features": features,
            "flaggedMoments": input.flags,
            "words": input.words,
            "present": input.present,
            "audio": Value::Null,
            "frames": [],
            "status": "complete",
        });
        let Some(recorder) = self.recorder.as_ref() else {
            body["status"] = json!("unavailable");
            body["detail"] =
                json!("no recording store is configured, so no segment media is written");
            return body;
        };
        let name = |suffix: &str| {
            [
                call.call_id.clone(),
                "segments".to_string(),
                format!("p{}", input.index),
                format!("{}{suffix}", input.segment),
            ]
        };
        let mut errors = Vec::new();
        if !input.samples.is_empty() {
            let wav = audio::wav_wrap(&input.samples, input.rate);
            let names = name("");
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            match recorder
                .put_media(
                    module_key,
                    &names,
                    "wav",
                    crate::recording::RECORDING_FORMAT,
                    wav,
                )
                .await
            {
                Ok(stored) => {
                    let mut audio_ref = reference(
                        stored,
                        crate::recording::RECORDING_FORMAT,
                        input.start_ms,
                        input.end_ms,
                    );
                    audio_ref["rate"] = json!(input.rate);
                    audio_ref["durationMs"] = json!(audio_ms);
                    body["audio"] = audio_ref;
                }
                Err(e) => errors.push(format!("audio: {e}")),
            }
        }
        let format = lock(&call.meeting)
            .as_ref()
            .map(|s| s.video.format.clone())
            .unwrap_or_else(|| meeting::DEFAULT_FRAME_FORMAT.to_string());
        let extension = if format == "image/png" { "png" } else { "jpg" };
        let mut frames = Vec::new();
        for (k, frame) in input.frames.into_iter().enumerate() {
            let names = name(&format!("-f{}", k + 1));
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            match recorder
                .put_media(module_key, &names, extension, &format, frame.data)
                .await
            {
                Ok(stored) => {
                    let mut frame_ref = reference(stored, &format, frame.at_ms, frame.at_ms);
                    frame_ref["atMs"] = json!(frame.at_ms.saturating_sub(input.start_ms));
                    frame_ref["requested"] = json!(frame.requested);
                    frames.push(frame_ref);
                }
                Err(e) => errors.push(format!("frame {}: {e}", k + 1)),
            }
        }
        body["frames"] = json!(frames);
        if !errors.is_empty() {
            tracing::warn!(call_id = %call.call_id, segment = input.segment, index = input.index, "voip: segment media could not be written: {}", errors.join("; "));
            body["status"] = json!("failed");
            body["detail"] = json!(errors.join("; "));
        }
        body
    }

    /// Finish a meeting session after its end: stop every participant's tap, drain the
    /// transcribers, write the last segment, write each participant's recording, then deliver the
    /// hang-up with all of it — so every transcript segment and every segment precedes it.
    fn finish_meeting(
        &self,
        call: &Arc<Call>,
        mut state: MeetingState,
        extra: BTreeMap<String, String>,
        mut body: Value,
    ) {
        let Some(hub) = self.this.upgrade() else {
            return;
        };
        let call = Arc::clone(call);
        let module = lock(&call.meta).module.clone();
        tokio::spawn(async move {
            let end_ms = state.elapsed_ms();
            let present: Vec<String> = state
                .participants
                .values()
                .filter(|p| p.left_at_ms.is_none())
                .map(|p| p.participant_id.clone())
                .collect();
            for id in present {
                state.leave(&id, Some("session-ended".to_string()));
            }
            let drain_ms = module
                .as_ref()
                .and_then(|m| m.transcribe.as_ref().map(|t| t.drain_ms))
                .unwrap_or(0);
            let mut transcripts: BTreeMap<u16, Value> = BTreeMap::new();
            let pending: Vec<(u16, tokio::sync::oneshot::Receiver<Value>)> = state
                .participants
                .values_mut()
                .filter_map(|p| p.transcript.take().map(|done| (p.index, done)))
                .collect();
            let bound = std::time::Duration::from_millis(drain_ms + 15_000);
            for (index, done) in pending {
                let summary = match tokio::time::timeout(bound, done).await {
                    Ok(Ok(summary)) => summary,
                    _ => json!({"status": "failed", "detail": "the transcription did not finish"}),
                };
                transcripts.insert(index, summary);
            }

            // The last segment (partial), written and delivered before the hang-up.
            let segments: Option<SegmentSettings> = state.segments.clone();
            if segments.is_some() {
                let _running = call.segment_lock.lock().await;
                let taken = state.take_segments(end_ms, true);
                hub.deliver_segments(&call, taken).await;
            }
            state.release_before(u64::MAX);

            let mut recordings = Vec::new();
            let mut participants = Vec::new();
            let tally = std::mem::take(&mut *lock(&call.segment_tally));
            let consent_at = state.consent_at_ms;
            for participant in state.participants.values_mut() {
                let recording = if !state.record {
                    Value::Null
                } else if hub.recorder.is_none() {
                    json!({"status": "unavailable", "detail": "no recording store is configured"})
                } else {
                    if let Some(pending) = participant.audio.stop_recording(&call.call_id) {
                        let mut reference = pending.reference(&call.call_id).await;
                        if reference["kind"] == listen::MEDIA_REFERENCE_KIND {
                            reference["participantId"] = json!(participant.participant_id);
                            reference["participantIndex"] = json!(participant.index);
                            // The session's announcement, in session time.
                            reference["consentAnnounced"] = json!(consent_at.is_some());
                            reference["consentAnnouncedAtMs"] = json!(consent_at);
                            reference["startMs"] = json!(
                                participant.joined_at_ms
                                    + reference["startMs"].as_u64().unwrap_or(0)
                            );
                            recordings.push(reference.clone());
                        }
                        participant.audio.finished.push(reference);
                    }
                    match participant.audio.finished.last() {
                        Some(last) => last.clone(),
                        None if participant.audio.recording_error.is_some() => json!({
                            "status": "failed", "detail": participant.audio.recording_error
                        }),
                        None => json!({
                            "status": "not-started",
                            "detail": "the recording never started (consent was never given)"
                        }),
                    }
                };
                participants.push(json!({
                    "participantId": participant.participant_id,
                    "participantIndex": participant.index,
                    "label": participant.label,
                    "role": participant.role,
                    "declaredRole": participant.declared_role,
                    "segmented": participant.segmented,
                    "segments": tally.per_participant.get(&participant.index).copied().unwrap_or(0),
                    "joinedAtMs": participant.joined_at_ms,
                    "leftAtMs": participant.left_at_ms,
                    "leftReason": participant.left_reason,
                    "talkMs": participant.speech.voiced_ms,
                    "transcriptLines": participant.line_count,
                    "transcript": transcripts.remove(&participant.index).unwrap_or(Value::Null),
                    "video": {
                        "regular": participant.frames_regular,
                        "requested": participant.frames_requested,
                        "dropped": participant.frames_dropped,
                    },
                    "tracks": participant.tracks,
                    "recording": recording,
                }));
            }
            body["mode"] = json!(MODE_LISTEN);
            body["session"] = json!("meeting");
            body["consentAnnounced"] = json!(state.consent_at_ms.is_some());
            body["consentAnnouncedAtMs"] = json!(state.consent_at_ms);
            body["participants"] = json!(participants);
            body["recordings"] = json!(recordings);
            body["recorded"] = json!(state.record);
            body["transcript"] = json!({
                "speakerLabels": "track",
                "segments": call.segment_seq.load(Ordering::SeqCst),
            });
            body["flaggedMoments"] = json!(state.flag_count);
            body["video"] = json!({
                "enabled": state.video.enabled,
                "fps": state.video.fps,
                "format": state.video.format,
                "extraFrames": state.video.extra_frames,
                // Frames are held in memory only until their segment is written, and only a
                // segmented participant's frames are written (`call-segment`).
                "framesRetained": false,
            });
            body["segments"] = match &segments {
                None => json!({"status": "off"}),
                Some(settings) => json!({
                    "status": if hub.recorder.is_none() { "unavailable" } else { "complete" },
                    "segmentMs": settings.segment_ms,
                    "maxFrames": settings.max_frames,
                    "roles": settings.roles,
                    "delivered": tally.delivered,
                    "failed": tally.failed,
                    "skipped": tally.skipped,
                }),
            };
            hub.enqueue_json(&call, EventKind::CallHangup, extra, body);
        });
    }

    // ---- inbound --------------------------------------------------------------------------

    /// A gateway announced an inbound call: remember it and take control of it.
    pub fn announce_inbound(
        &self,
        call_id: &str,
        caller: Option<String>,
        called: Option<String>,
    ) -> Arc<Call> {
        let (caller, called) = (party(caller), party(called));
        if let Some(existing) = self.call(call_id) {
            {
                let mut meta = lock(&existing.meta);
                if meta.caller.is_none() {
                    meta.caller = caller;
                }
                if meta.called.is_none() {
                    meta.called = called;
                }
            }
            self.attach_once(&existing);
            return existing;
        }
        let call = self.new_call(
            call_id,
            CallDirection::Inbound,
            CallMeta {
                caller,
                called,
                ..CallMeta::default()
            },
        );
        lock(&self.awaiting_media).push_back(call_id.to_string());
        self.attach_once(&call);
        call
    }

    fn attach_once(&self, call: &Arc<Call>) {
        if !self.gateway.has_control() || call.attached.swap(true, Ordering::SeqCst) {
            return;
        }
        let gateway = Arc::clone(&self.gateway);
        let call_id = call.call_id.clone();
        tokio::spawn(async move {
            if let Err(error) = gateway.attach(&call_id).await {
                tracing::warn!(call_id = %call_id, "voip: attach failed: {error}");
            }
        });
    }

    /// A media socket opened. `call_id` is the gateway's (bridge query or generic `start`);
    /// a bridge socket without one is matched to the oldest announced call awaiting media.
    #[allow(clippy::too_many_arguments)]
    pub fn open_media(
        &self,
        call_id: Option<&str>,
        framing: Framing,
        format: AudioFormat,
        caller: Option<String>,
        called: Option<String>,
        out: mpsc::Sender<MediaOut>,
        options: MediaOptions,
    ) -> Result<Arc<Call>, String> {
        let (caller, called) = (party(caller), party(called));
        let call_id = match call_id {
            Some(id) => id.to_string(),
            None => {
                let mut waiting = lock(&self.awaiting_media);
                loop {
                    let Some(id) = waiting.pop_front() else {
                        return Err("bridge socket without a call id and no call awaiting media"
                            .to_string());
                    };
                    if self
                        .call(&id)
                        .is_some_and(|c| !c.has_media() && !c.is_ended())
                    {
                        break id;
                    }
                }
            }
        };
        lock(&self.awaiting_media).retain(|id| id != &call_id);
        let call = match self.call(&call_id) {
            Some(call) if call.is_ended() => return Err(format!("call {call_id} already ended")),
            Some(call) if call.has_media() => {
                return Err(format!("call {call_id} already has media"))
            }
            Some(call) => call,
            None => self.new_call(&call_id, CallDirection::Inbound, CallMeta::default()),
        };
        {
            let mut meta = lock(&call.meta);
            if meta.caller.is_none() {
                meta.caller = caller;
            }
            if meta.called.is_none() {
                meta.called = called;
            }
        }
        if call.direction == CallDirection::Inbound {
            let called = lock(&call.meta).called.clone().unwrap_or_default();
            let routes = self.routes();
            let Some(target) = routes.numbers.resolve(&called).cloned() else {
                self.end_call(&call, EndOptions::rejected("no-route"));
                return Err(format!(
                    "no call-start channel binds the called number '{called}'"
                ));
            };
            {
                let mut meta = lock(&call.meta);
                meta.module = Some(Arc::clone(&target.module));
                meta.start_channel = Some(target.channel.clone());
                meta.settings = target.settings;
            }
            // What the call-start channel says about its sessions (a tagged session is always
            // listen-only: nobody takes turns with a meeting).
            call.listen_only.store(
                target.session.listen_only || options.tagged,
                Ordering::SeqCst,
            );
            if target.session.record && self.recorder.is_none() {
                tracing::warn!(call_id = %call.call_id, "voip: the session is to be recorded, but no recording store is configured");
            }
            call.record.store(target.session.record, Ordering::SeqCst);
            call.record_after_consent
                .store(target.session.record_after_consent, Ordering::SeqCst);
            if options.tagged {
                let max_seconds = self
                    .recorder
                    .as_ref()
                    .map(|r| r.max_seconds)
                    .unwrap_or(crate::config::DEFAULT_RECORDING_MAX_SECONDS);
                let mut meeting = MeetingState::new(
                    format.rate,
                    target.session.record,
                    target.session.video.clone(),
                    target.settings.dead_air_ms,
                    max_seconds,
                )
                .with_segments(target.module.segments.clone());
                meeting.record_after_consent = target.session.record_after_consent;
                *lock(&call.meeting) = Some(meeting);
            }
        }
        let settings = lock(&call.meta).settings;
        {
            let mut audio_state = lock(&call.audio);
            audio_state.detector = Some(TurnDetector::new(settings.vad, format.rate));
            audio_state.analytics = CallAnalytics::new(settings.vad.frame_ms, settings.dead_air_ms);
        }
        *lock(&call.media) = Some(MediaLink {
            out,
            framing,
            format,
        });
        if call.is_meeting() {
            self.start_meeting(&call);
        } else {
            self.start_listening(&call, format.rate);
        }
        self.attach_once(&call);
        match call.direction {
            CallDirection::Inbound => {
                let meta = lock(&call.meta).clone();
                let mut body = json!({
                    "callId": call.call_id,
                    "from": meta.caller,
                    "to": meta.called,
                    "direction": call.direction.as_str(),
                    "format": format.media_type(),
                    "gateway": self.gateway.kind(),
                });
                if call.is_meeting() {
                    body["session"] = json!("meeting");
                    body["mode"] = json!(MODE_LISTEN);
                    body["recorded"] = json!(call.is_recorded());
                    body["headers"] = json!(options.headers);
                }
                let extra =
                    BTreeMap::from([(headers::AUDIO_FORMAT.to_string(), format.media_type())]);
                // From here an instance is (being) started, so the call's end must reach it.
                call.started.store(true, Ordering::SeqCst);
                self.enqueue_json(&call, EventKind::CallStart, extra, body);
            }
            CallDirection::Outbound => {
                self.progress(&call, "connected", json!({"format": format.media_type()}));
            }
        }
        Ok(call)
    }

    /// Caller audio arrived on the media socket.
    pub fn on_audio(&self, call: &Arc<Call>, bytes: &[u8]) {
        if call.is_ended() {
            return;
        }
        let Some(link) = call.media() else { return };
        let samples = match audio::decode(bytes, &link.format) {
            Ok((samples, _)) => samples,
            Err(_) => return,
        };
        if let Some(state) = lock(&call.listen).as_mut() {
            state.on_audio(&samples);
        }
        let listen_only = call.is_listen_only();
        let playing = call.is_playing();
        let mut speech_started = false;
        let mut turns: Vec<(Vec<i16>, &'static str, u64, bool)> = Vec::new();
        let (vad, rate) = {
            let mut guard = lock(&call.audio);
            let state = &mut *guard;
            let Some(detector) = state.detector.as_mut() else {
                return;
            };
            let frame_ms = u64::from(detector.config().frame_ms);
            let analytics = &mut state.analytics;
            let overtalk = &mut state.turn_overtalk_ms;
            let interrupted = &mut state.turn_interrupted;
            detector.push(&samples, |info, event| {
                analytics.on_frame(info.voiced, playing);
                if (info.in_turn || matches!(event, Some(VadEvent::TurnEnded { .. })))
                    && playing
                    && info.voiced
                {
                    *overtalk += frame_ms;
                }
                match event {
                    Some(VadEvent::SpeechStarted) => speech_started = true,
                    Some(VadEvent::TurnEnded { samples, reason }) => {
                        turns.push((
                            samples,
                            reason.as_str(),
                            std::mem::take(overtalk),
                            std::mem::take(interrupted),
                        ));
                        analytics.on_turn();
                    }
                    None => {}
                }
            });
            (*detector.config(), link.format.rate)
        };
        if listen_only {
            // Listening only: the speech is transcribed, never cut into turns or barged in on.
            return;
        }
        if speech_started && playing && call.barge_in_enabled.load(Ordering::SeqCst) {
            self.barge_in(call, &link);
        }
        for (samples, reason, overtalk_ms, interrupted) in turns {
            let index = {
                let mut state = lock(&call.audio);
                state.turns += 1;
                state.turns
            };
            let mut features = analyze_turn(
                &samples,
                rate,
                vad.frame_ms,
                vad.energy_threshold,
                &PitchConfig::default(),
            );
            features.overtalk_ms = overtalk_ms;
            features.interrupted_playback = interrupted;
            let features_json = serde_json::to_string(&features).unwrap_or_default();
            let extra = BTreeMap::from([
                (
                    headers::AUDIO_FORMAT.to_string(),
                    AudioFormat::l16(rate).media_type(),
                ),
                (headers::TURN_INDEX.to_string(), index.to_string()),
                (headers::TURN_END_REASON.to_string(), reason.to_string()),
                (headers::BARGE_IN.to_string(), interrupted.to_string()),
                (headers::TURN_FEATURES.to_string(), features_json),
            ]);
            self.enqueue_turn(call, extra, &samples, rate);
        }
    }

    /// Deliver one utterance on the `call-turn` channel, as the module's `turn-body` says.
    fn enqueue_turn(
        &self,
        call: &Call,
        extra: BTreeMap<String, String>,
        samples: &[i16],
        rate: u32,
    ) {
        let wav = audio::wav_wrap(samples, rate);
        let turn_body = lock(&call.meta)
            .module
            .as_ref()
            .map(|m| m.turn_body)
            .unwrap_or_default();
        match turn_body {
            TurnBody::Wav => self.enqueue(call, EventKind::CallTurn, extra, wav, "audio/wav"),
            TurnBody::Json => {
                let header_text = |name: &str| extra.get(name).cloned();
                let features = header_text(headers::TURN_FEATURES)
                    .and_then(|f| serde_json::from_str::<Value>(&f).ok())
                    .unwrap_or(Value::Null);
                let body = json!({
                    "callId": call.call_id,
                    "turnIndex": header_text(headers::TURN_INDEX)
                        .and_then(|i| i.parse::<u64>().ok()),
                    "endReason": header_text(headers::TURN_END_REASON),
                    "bargeIn": header_text(headers::BARGE_IN).as_deref() == Some("true"),
                    "audio": {
                        "data": base64::engine::general_purpose::STANDARD.encode(&wav),
                        "format": "audio/wav",
                    },
                    "features": features,
                });
                self.enqueue_json(call, EventKind::CallTurn, extra, body);
            }
        }
    }

    fn barge_in(&self, call: &Call, link: &MediaLink) {
        call.play_generation.fetch_add(1, Ordering::SeqCst);
        {
            let mut state = lock(&call.audio);
            state.analytics.on_interruption();
            state.turn_interrupted = true;
        }
        if link.framing == Framing::Generic {
            let _ = link.out.try_send(MediaOut::Text(protocol::generic_clear()));
        }
        tracing::debug!(call_id = %call.call_id, "voip: barge-in");
    }

    /// A DTMF digit arrived on the media socket.
    pub fn on_dtmf(&self, call: &Arc<Call>, digit: &str) {
        if call.is_ended() {
            return;
        }
        let extra = BTreeMap::from([(headers::DTMF_DIGIT.to_string(), digit.to_string())]);
        self.enqueue_json(
            call,
            EventKind::CallDtmf,
            extra,
            json!({"callId": call.call_id, "digit": digit}),
        );
    }

    /// The media socket closed (the gateway dropped it, or a `stop` frame arrived).
    pub fn media_closed(&self, call: &Arc<Call>, reason: &str) {
        if call.is_ended() {
            return;
        }
        self.end_call(
            call,
            EndOptions {
                reason: reason.to_string(),
                hangup_by: None,
                sip_status: None,
                hangup_gateway: true,
                close_media: true,
                deliver: true,
            },
        );
    }

    // ---- gateway events -------------------------------------------------------------------

    pub fn on_gateway_event(&self, event: GatewayEvent) {
        match event {
            GatewayEvent::Connected => {}
            GatewayEvent::Created {
                call_id,
                caller,
                callee,
                direction,
            } => {
                if direction.as_deref() == Some("outbound")
                    || self
                        .call(&call_id)
                        .is_some_and(|c| c.direction == CallDirection::Outbound)
                {
                    return;
                }
                self.announce_inbound(&call_id, caller, callee);
            }
            GatewayEvent::Ringing {
                call_id,
                early_media,
            } => {
                if let Some(call) = self.outbound(&call_id) {
                    if !call.answered.load(Ordering::SeqCst) {
                        self.progress(&call, "ringing", json!({"earlyMedia": early_media}));
                    }
                }
            }
            GatewayEvent::Answered { call_id } => {
                let Some(call) = self.call(&call_id) else {
                    return;
                };
                if call.answered.swap(true, Ordering::SeqCst)
                    || call.direction != CallDirection::Outbound
                {
                    return;
                }
                self.progress(&call, "answered", json!({}));
                let gateway = Arc::clone(&self.gateway);
                let url = self.bridge_url(Some(&call_id));
                let hub = self.this.clone();
                tokio::spawn(async move {
                    if let Err(error) = gateway.bridge(&call.call_id, &url).await {
                        let error = crate::tokens::redact_tokens(&error.to_string());
                        tracing::warn!(call_id = %call.call_id, "voip: bridging media failed: {error}");
                        if let Some(hub) = hub.upgrade() {
                            hub.progress(
                                &call,
                                "failed",
                                json!({"reason": format!("bridge: {error}")}),
                            );
                            hub.end_call(&call, EndOptions::failed("bridge-failed"));
                        }
                    }
                });
            }
            GatewayEvent::Busy { call_id } => self.outbound_outcome(&call_id, "busy"),
            GatewayEvent::NoAnswer { call_id } => self.outbound_outcome(&call_id, "no-answer"),
            GatewayEvent::Hangup {
                call_id,
                reason,
                hangup_by,
                sip_status,
            } => {
                let Some(call) = self.call(&call_id) else {
                    return;
                };
                if call.direction == CallDirection::Outbound
                    && !call.answered.load(Ordering::SeqCst)
                {
                    self.progress(
                        &call,
                        "failed",
                        json!({"reason": reason, "sipStatus": sip_status}),
                    );
                }
                self.end_call(
                    &call,
                    EndOptions {
                        reason: reason.unwrap_or_else(|| "hangup".to_string()),
                        hangup_by,
                        sip_status,
                        hangup_gateway: false,
                        close_media: true,
                        deliver: true,
                    },
                );
            }
            GatewayEvent::TransferFailed { call_id, reason } => {
                let Some(call) = self.outbound(&call_id) else {
                    return;
                };
                if call.has_media() {
                    return;
                }
                self.progress(&call, "failed", json!({"reason": reason}));
                self.end_call(&call, EndOptions::failed("bridge-failed"));
            }
        }
    }

    fn outbound(&self, call_id: &str) -> Option<Arc<Call>> {
        self.call(call_id)
            .filter(|c| c.direction == CallDirection::Outbound)
    }

    fn outbound_outcome(&self, call_id: &str, status: &str) {
        let Some(call) = self.outbound(call_id) else {
            return;
        };
        self.progress(&call, status, json!({}));
        self.end_call(
            &call,
            EndOptions {
                reason: status.to_string(),
                hangup_by: None,
                sip_status: None,
                hangup_gateway: false,
                close_media: true,
                deliver: true,
            },
        );
    }

    /// End a call exactly once: flush a turn in progress, stop playback, deliver the
    /// `call-hangup` (with the call's features) when an instance exists, close the media and
    /// — when asked and the gateway has a control channel — hang up at the gateway.
    pub fn end_call(&self, call: &Arc<Call>, options: EndOptions) {
        if call.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        call.play_generation.fetch_add(1, Ordering::SeqCst);
        lock(&self.calls).remove(&call.call_id);
        lock(&self.awaiting_media).retain(|id| id != &call.call_id);
        {
            let mut ended = lock(&self.ended);
            ended.push_back(call.call_id.clone());
            while ended.len() > ENDED_MEMORY {
                ended.pop_front();
            }
        }
        let had_media = call.has_media();
        let (flushed, rate, vad, summary) = {
            let mut state = lock(&call.audio);
            let rate = call
                .media()
                .map(|l| l.format.rate)
                .unwrap_or(audio::DEFAULT_RATE);
            let vad = state.detector.as_ref().map(|d| *d.config());
            let flushed = state.detector.as_mut().and_then(TurnDetector::flush);
            if flushed.is_some() {
                state.analytics.on_turn();
            }
            (flushed, rate, vad, state.analytics.summary())
        };
        let listening = lock(&call.listen).take();
        let meeting_state = lock(&call.meeting).take();
        if options.deliver && call.started.load(Ordering::SeqCst) {
            let flushed = if call.is_listen_only() { None } else { flushed };
            if let (Some(VadEvent::TurnEnded { samples, reason }), Some(vad)) = (flushed, vad) {
                let features = analyze_turn(
                    &samples,
                    rate,
                    vad.frame_ms,
                    vad.energy_threshold,
                    &PitchConfig::default(),
                );
                let extra = BTreeMap::from([
                    (
                        headers::AUDIO_FORMAT.to_string(),
                        AudioFormat::l16(rate).media_type(),
                    ),
                    (
                        headers::TURN_END_REASON.to_string(),
                        reason.as_str().to_string(),
                    ),
                    (
                        headers::TURN_FEATURES.to_string(),
                        serde_json::to_string(&features).unwrap_or_default(),
                    ),
                ]);
                self.enqueue_turn(call, extra, &samples, rate);
            }
            let had_media = had_media && meeting_state.is_none();
            let body = json!({
                "callId": call.call_id,
                "reason": options.reason,
                "hangupBy": options.hangup_by,
                "sipStatus": options.sip_status,
                "durationMs": call.created.elapsed().as_millis() as u64,
                "features": if had_media { serde_json::to_value(&summary).unwrap_or(Value::Null) } else { Value::Null },
            });
            let extra =
                BTreeMap::from([(headers::HANGUP_REASON.to_string(), options.reason.clone())]);
            if let Some(state) = meeting_state {
                self.finish_meeting(call, state, extra, body);
            } else {
                match listening {
                    // The recording is written and the transcript drained first, so the hang-up
                    // carries the recording's reference and every segment precedes it.
                    Some(state) => self.finish_listening(call, state, extra, body),
                    None => self.enqueue_json(call, EventKind::CallHangup, extra, body),
                }
            }
        } else {
            // No hang-up goes out, but a recording in progress is still completed (not left
            // for the sweep as if the process had crashed).
            close_recordings(&call.call_id, listening, meeting_state);
        }
        if options.close_media {
            if let Some(link) = lock(&call.media).take() {
                let _ = link.out.try_send(MediaOut::Close);
            }
            call.closed.send_replace(true);
        }
        if options.hangup_gateway && self.gateway.has_control() {
            let gateway = Arc::clone(&self.gateway);
            let call_id = call.call_id.clone();
            let reason = options.reason.clone();
            tokio::spawn(async move {
                for attempt in 0..3u32 {
                    match gateway.hangup(&call_id, Some(&reason)).await {
                        Ok(()) | Err(GatewayError::Permanent(_)) => return,
                        Err(GatewayError::Retryable(error)) => {
                            tracing::warn!(call_id = %call_id, attempt, "voip: hang-up failed: {error}");
                            tokio::time::sleep(std::time::Duration::from_millis(500 << attempt))
                                .await;
                        }
                    }
                }
            });
        }
    }

    // ---- outbound operations --------------------------------------------------------------

    /// `voip://originate`: mint (or take) the call id, register the call, dial.
    pub async fn originate(
        &self,
        module_key: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Value, OpError> {
        let body = json_body(message);
        let field = |keys: &[&str], header: &str| -> Option<String> {
            keys.iter()
                .find_map(|k| body.get(*k).and_then(value_text))
                .or_else(|| message.headers.get(header).cloned())
                .filter(|v| !v.trim().is_empty())
        };
        let destination = field(&["destination", "to"], headers::DESTINATION).ok_or_else(|| {
            OpError::permanent(codes::OUTBOUND_INVALID, "originate needs a 'destination'")
        })?;
        let trunk = field(&["trunk"], "x-trunk").or_else(|| self.config.default_trunk.clone());
        let raw_destination = destination.clone();
        let destination = sip_destination(&destination, trunk.as_deref()).ok_or_else(|| {
            OpError::permanent(
                codes::OUTBOUND_INVALID,
                format!(
                    "originate destination '{destination}' is not a SIP URI (a bare number needs a trunk)"
                ),
            )
        })?;
        let call_id = field(&["callId", "call_id"], headers::CALL_ID)
            .unwrap_or_else(|| mint_call_id(&message.outbox_key));
        if self.call(&call_id).is_some() {
            return Ok(json!({"callId": call_id, "status": "originating"}));
        }
        let audit_context = crate::audit::context_of(&message.headers);
        if let Err(refusal) = self.originate_guard.check(&raw_destination, &destination) {
            return Err(self.refuse_originate(
                module_key,
                audit_context.as_ref(),
                &call_id,
                &raw_destination,
                &refusal,
            ));
        }
        let listen_only = field(&["mode"], "x-call-mode")
            .is_some_and(|m| m.trim().eq_ignore_ascii_case(MODE_LISTEN));
        let captured = message
            .headers
            .get(sutra_channels::AUDIT_CAPTURE_HEADER)
            .is_some_and(|v| {
                v.trim()
                    .eq_ignore_ascii_case(sutra_channels::AUDIT_CAPTURE_MEDIA)
            });
        // `record`: `true`, `after-consent` (the recording starts once an announcement marked
        // `consent: true` has played to the end, as on a `record: after-consent` channel) or
        // `false`. A request the process marked `capture=media` is recorded either way.
        let (asked, after_consent) =
            match field(&["record"], "x-record")
                .map(|v| v.trim().to_ascii_lowercase())
                .as_deref()
            {
                None | Some("false") => (false, false),
                Some("true") => (true, false),
                Some("after-consent") => (true, true),
                Some(other) => return Err(OpError::permanent(
                    codes::OUTBOUND_INVALID,
                    format!(
                        "originate 'record' must be true, false or after-consent, got '{other}'"
                    ),
                )),
            };
        let record = captured || asked;
        if record && self.recorder.is_none() {
            return Err(OpError::permanent(
                codes::RECORDING_UNAVAILABLE,
                format!(
                    "the call is to be recorded (capture=media), but no recording store is configured ({})",
                    crate::config::ENV_RECORDING_STORE
                ),
            ));
        }
        let dtmf_after_answer = field(
            &["dtmfAfterAnswer", "dtmf_after_answer"],
            "x-dtmf-after-answer",
        )
        .map(|digits| {
            let delay = body
                .get("dtmfDelayMs")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_DTMF_DELAY_MS);
            (digits, delay)
        });
        if let Some((digits, _)) = &dtmf_after_answer {
            validate_digits(digits)?;
        }
        let module = self
            .routes()
            .module_for_outbound(module_key)
            .ok_or_else(|| {
                OpError::permanent(
                    codes::OUTBOUND_INVALID,
                    "no deployment has voip channels to receive this call's events",
                )
            })?;
        let mut extra_headers = BTreeMap::new();
        if let Some(Value::Object(map)) = body.get("headers") {
            for (k, v) in map {
                if let Some(v) = value_text(v) {
                    extra_headers.insert(k.clone(), v);
                }
            }
        }
        let request = OriginateRequest {
            call_id: call_id.clone(),
            destination: destination.clone(),
            caller_id: field(&["callerId", "caller_id", "from"], headers::CALLER_ID),
            timeout_secs: body
                .get("timeoutSecs")
                .or_else(|| body.get("timeout_secs"))
                .and_then(Value::as_u64),
            trunk,
            extra_headers,
        };
        let call = self.new_call(
            &call_id,
            CallDirection::Outbound,
            CallMeta {
                caller: request.caller_id.clone(),
                called: Some(destination),
                settings: module.settings,
                module: Some(module),
                dtmf_after_answer,
                ..CallMeta::default()
            },
        );
        call.attached.store(true, Ordering::SeqCst);
        *lock(&call.audit) = audit_context;
        call.listen_only.store(listen_only, Ordering::SeqCst);
        call.record.store(record, Ordering::SeqCst);
        call.record_after_consent
            .store(record && after_consent, Ordering::SeqCst);
        if let Err(error) = self.gateway.originate(&request).await {
            call.ended.store(true, Ordering::SeqCst);
            lock(&self.calls).remove(&call_id);
            return Err(OpError::from_gateway(error));
        }
        Ok(json!({
            "callId": call_id,
            "status": "originating",
            "mode": if listen_only { MODE_LISTEN } else { "conversation" },
            "recorded": record,
            "record": if !record { "false" } else if after_consent { "after-consent" } else { "true" },
        }))
    }

    /// Whether `call_id` ended recently (see [`ENDED_MEMORY`]).
    pub fn has_ended(&self, call_id: &str) -> bool {
        lock(&self.ended).iter().any(|id| id == call_id)
    }

    /// The live call a request names. `Ok(Err(answer))` when the call has already ended: the
    /// operation is moot, and its answer (`status: "ended"`) says so instead of failing the
    /// flow that raced the hang-up.
    fn target_call(&self, message: &OutboundMessage) -> Result<Result<Arc<Call>, Value>, OpError> {
        let call_id = call_id_of(message).ok_or_else(|| {
            OpError::permanent(
                codes::OUTBOUND_INVALID,
                "the request names no call ('x-call-id')",
            )
        })?;
        if let Some(call) = self.call(&call_id) {
            return Ok(Ok(call));
        }
        if self.has_ended(&call_id) {
            return Ok(Err(
                json!({"callId": call_id, "status": "ended", "playedMs": 0, "interrupted": true}),
            ));
        }
        Err(OpError::permanent(
            codes::CALL_UNKNOWN,
            format!("call {call_id} is not active"),
        ))
    }

    /// `voip://play`: decode, resample to the call's wire rate, and play paced. With `wait`
    /// the result is the playback outcome; otherwise it answers once queued.
    pub async fn play(&self, message: &OutboundMessage, wait: bool) -> Result<Value, OpError> {
        let call = match self.target_call(message)? {
            Ok(call) => call,
            Err(ended) => return Ok(ended),
        };
        let Some(link) = call.media() else {
            return Err(OpError::retryable(
                codes::CALL_NOT_CONNECTED,
                format!("call {} has no media yet", call.call_id),
            ));
        };
        let samples = match play_prompt_name(message) {
            Some(name) => {
                let path = prompt_path(self.config.prompts_dir.as_deref(), &name)
                    .map_err(|reason| OpError::permanent(codes::OUTBOUND_INVALID, reason))?;
                load_prompt(path, link.format.rate)
                    .await
                    .map_err(|reason| OpError::permanent(codes::AUDIO_INVALID, reason))?
            }
            None => decode_play_audio(message, link.format.rate)
                .map_err(|reason| OpError::permanent(codes::AUDIO_INVALID, reason))?,
        };
        let barge_in = !message
            .headers
            .get(headers::BARGE_IN)
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("false"));
        let mark = message.headers.get(headers::MARK).cloned();
        let consent = json_body(message)
            .get("consent")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let hub = self.this.upgrade();
        if wait {
            let (outcome, consented) = play_paced(&call, samples, barge_in, mark, consent).await;
            if consented {
                self.on_consent(&call);
            }
            Ok(json!({
                "callId": call.call_id,
                "playedMs": outcome.played_ms(20),
                "interrupted": outcome.interrupted,
            }))
        } else {
            let queued_ms = (samples.len() as u64 * 1000) / u64::from(link.format.rate.max(1));
            let call_for_task = Arc::clone(&call);
            tokio::spawn(async move {
                let (_, consented) =
                    play_paced(&call_for_task, samples, barge_in, mark, consent).await;
                if let (true, Some(hub)) = (consented, hub) {
                    hub.on_consent(&call_for_task);
                }
            });
            Ok(json!({"callId": call.call_id, "queuedMs": queued_ms}))
        }
    }

    /// `voip://hangup`: hang up at the gateway (always, when it has a control channel), then
    /// end the call locally — which delivers the `call-hangup`.
    pub async fn hangup(&self, message: &OutboundMessage) -> Result<Value, OpError> {
        let call_id = call_id_of(message).ok_or_else(|| {
            OpError::permanent(
                codes::OUTBOUND_INVALID,
                "the request names no call ('x-call-id')",
            )
        })?;
        if self.call(&call_id).is_none() && self.has_ended(&call_id) {
            return Ok(json!({"callId": call_id, "status": "ended"}));
        }
        let reason = json_body(message).get("reason").and_then(value_text);
        if self.gateway.has_control() {
            self.gateway
                .hangup(&call_id, reason.as_deref())
                .await
                .map_err(OpError::from_gateway)?;
        } else if let Some(link) = self.call(&call_id).and_then(|c| c.media()) {
            let _ = link
                .out
                .send(MediaOut::Text(protocol::generic_hangup()))
                .await;
        }
        if let Some(call) = self.call(&call_id) {
            self.end_call(
                &call,
                EndOptions {
                    reason: reason.unwrap_or_else(|| "hangup-requested".to_string()),
                    hangup_by: Some("sutra".to_string()),
                    sip_status: None,
                    hangup_gateway: false,
                    close_media: true,
                    deliver: true,
                },
            );
        }
        Ok(json!({"callId": call_id, "status": "hung-up"}))
    }

    /// `voip://transfer`: hand the call to `target` (e.g. a human agent's extension).
    pub async fn transfer(&self, message: &OutboundMessage) -> Result<Value, OpError> {
        let call = match self.target_call(message)? {
            Ok(call) => call,
            Err(ended) => return Ok(ended),
        };
        let target = json_body(message)
            .get("target")
            .and_then(value_text)
            .or_else(|| message.headers.get(headers::TRANSFER_TARGET).cloned())
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| {
                OpError::permanent(codes::OUTBOUND_INVALID, "transfer needs a 'target'")
            })?;
        self.gateway
            .transfer(&call.call_id, &target)
            .await
            .map_err(OpError::from_gateway)?;
        // The gateway now owns the leg; the media socket is left for it to drop.
        self.end_call(
            &call,
            EndOptions {
                reason: "transferred".to_string(),
                hangup_by: Some("sutra".to_string()),
                sip_status: None,
                hangup_gateway: false,
                close_media: false,
                deliver: true,
            },
        );
        Ok(json!({"callId": call.call_id, "status": "transferred", "target": target}))
    }

    /// `voip://dtmf`: send digits to the call over the PCM bridge.
    pub async fn dtmf(&self, message: &OutboundMessage) -> Result<Value, OpError> {
        let call = match self.target_call(message)? {
            Ok(call) => call,
            Err(ended) => return Ok(ended),
        };
        let body = json_body(message);
        let digits = body
            .get("digits")
            .and_then(value_text)
            .ok_or_else(|| OpError::permanent(codes::OUTBOUND_INVALID, "dtmf needs 'digits'"))?;
        validate_digits(&digits)?;
        let gap_ms = body
            .get("gapMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_DTMF_GAP_MS);
        let sent = send_digits(&call, &digits, gap_ms).await?;
        // The digits themselves are not echoed: a PIN is a secret.
        Ok(json!({"callId": call.call_id, "sent": sent}))
    }

    /// `voip://record-start`: start recording the call now. Recording waits for consent: the
    /// request is refused unless an announcement marked `consent: true` has played to the end on
    /// the call, or the request itself says `consent: true` (the flow obtained it — the caller
    /// pressed a key, say). A meeting session starts every present participant's recording.
    pub async fn record_start(&self, message: &OutboundMessage) -> Result<Value, OpError> {
        let call = match self.target_call(message)? {
            Ok(call) => call,
            Err(ended) => return Ok(ended),
        };
        if self.recorder.is_none() {
            return Err(OpError::permanent(
                codes::RECORDING_UNAVAILABLE,
                format!(
                    "record-start: no recording store is configured ({})",
                    crate::config::ENV_RECORDING_STORE
                ),
            ));
        }
        let Some(link) = call.media() else {
            return Err(OpError::retryable(
                codes::CALL_NOT_CONNECTED,
                format!("call {} has no media yet", call.call_id),
            ));
        };
        let asserted = json_body(message)
            .get("consent")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let announced = lock(&call.consent_ms).is_some()
            || lock(&call.listen)
                .as_ref()
                .is_some_and(|s| s.consent_at_ms.is_some())
            || lock(&call.meeting)
                .as_ref()
                .is_some_and(|s| s.consent_at_ms.is_some());
        if !asserted && !announced {
            let module_key = lock(&call.meta)
                .module
                .as_ref()
                .map(|m| m.module_key.clone());
            self.audit.append(
                crate::audit::scope_of(lock(&call.audit).as_ref(), module_key.as_deref()),
                crate::audit::event(crate::audit::events::CONSENT_REQUIRED)
                    .with_field("callId", call.call_id.clone())
                    .with_diagnostic_code(codes::CONSENT_REQUIRED),
            );
            return Err(OpError::permanent(
                codes::CONSENT_REQUIRED,
                "record-start: recording needs consent first — play the announcement with                  `consent: true` (to the end), or send `consent: true` once the caller agreed",
            ));
        }
        call.record.store(true, Ordering::SeqCst);
        if call.is_meeting() {
            if let Some(state) = lock(&call.meeting).as_mut() {
                state.record = true;
                state.record_started = true;
            }
            self.start_participant_recordings(&call);
            return Ok(
                json!({"callId": call.call_id, "status": "recording", "session": "meeting"}),
            );
        }
        let owner = self.recording_owner(&call);
        let consent_ms = *lock(&call.consent_ms);
        let mut guard = lock(&call.listen);
        let state = guard.get_or_insert_with(|| {
            let max_seconds = self
                .recorder
                .as_ref()
                .map(|r| r.max_seconds)
                .unwrap_or(crate::config::DEFAULT_RECORDING_MAX_SECONDS);
            let mut state = ListenState::new(link.format.rate, true, max_seconds)
                .with_window(listen::TAP_WINDOW_SECONDS);
            // The announcement played before this media plane existed; it is reported, at the
            // call's clock, as before the recording began.
            state.consent_at_ms = consent_ms.map(|_| 0);
            state
        });
        state.record = true;
        if state.recording.is_none() {
            self.begin_call_recording(&call, owner, state)
                .map_err(|e| OpError::retryable(codes::RECORDING_UNAVAILABLE, e))?;
        }
        let uri = state.recording.as_ref().map(|w| w.uri.clone());
        let start_ms = state.recording_origin * 1000 / u64::from(state.rate.max(1));
        Ok(json!({
            "callId": call.call_id,
            "status": "recording",
            "uri": uri,
            "startMs": start_ms,
            "consent": if asserted { "asserted" } else { "announced" },
        }))
    }

    /// `voip://record-stop`: stop the recording in progress and answer its media reference
    /// (the hang-up carries every recording of the call in `recordings`).
    pub async fn record_stop(&self, message: &OutboundMessage) -> Result<Value, OpError> {
        let call = match self.target_call(message)? {
            Ok(call) => call,
            Err(ended) => return Ok(ended),
        };
        if call.is_meeting() {
            let pending: Vec<(u16, String, listen::PendingRecording)> = {
                let mut guard = lock(&call.meeting);
                let Some(state) = guard.as_mut() else {
                    return Ok(json!({"callId": call.call_id, "status": "ended"}));
                };
                state.record_started = false;
                state.record_after_consent = true;
                state.consent_at_ms = None;
                state
                    .participants
                    .values_mut()
                    .filter_map(|p| {
                        p.audio
                            .stop_recording(&call.call_id)
                            .map(|r| (p.index, p.participant_id.clone(), r))
                    })
                    .collect()
            };
            let mut references = Vec::new();
            for (index, id, recording) in pending {
                let mut reference = recording.reference(&call.call_id).await;
                reference["participantId"] = json!(id);
                reference["participantIndex"] = json!(index);
                if let Some(p) = lock(&call.meeting)
                    .as_mut()
                    .and_then(|s| s.participants.get_mut(&index))
                {
                    p.audio.finished.push(reference.clone());
                }
                references.push(reference);
            }
            return Ok(
                json!({"callId": call.call_id, "status": "stopped", "recordings": references}),
            );
        }
        let pending = lock(&call.listen)
            .as_mut()
            .and_then(|state| state.stop_recording(&call.call_id));
        let Some(pending) = pending else {
            return Ok(json!({"callId": call.call_id, "status": "not-recording"}));
        };
        let reference = pending.reference(&call.call_id).await;
        if let Some(state) = lock(&call.listen).as_mut() {
            state.finished.push(reference.clone());
        }
        Ok(json!({"callId": call.call_id, "status": "stopped", "recording": reference}))
    }

    /// Look for interrupted recordings (see [`Recorder::sweep`]) and deliver what was recovered
    /// on each call's `call-hangup` channel, unless that call's hang-up already went out.
    pub async fn sweep_recordings(&self) -> usize {
        let Some(recorder) = self.recorder.clone() else {
            return 0;
        };
        let recovered = recorder.sweep().await;
        let count = recovered.len();
        for (call_id, recordings) in crate::recording::by_call(recovered) {
            if recordings.iter().all(|r| r.hangup_delivered) {
                continue;
            }
            let owner = &recordings[0].owner;
            let routes = self.routes();
            let channel = routes
                .modules
                .get(&owner.module_key)
                .and_then(|m| m.channel(EventKind::CallHangup).map(str::to_string));
            let Some(channel) = channel else {
                tracing::warn!(call_id = %call_id, module = %owner.module_key, "voip: a recovered recording's deployment has no call-hangup channel; only its manifest records it");
                continue;
            };
            let references: Vec<Value> = recordings.iter().map(|r| r.reference.clone()).collect();
            let body = json!({
                "callId": call_id,
                "reason": "recording-recovered",
                "recovered": true,
                "recording": references[0],
                "recordings": references,
            });
            let message = InboundMessage {
                tenant: owner.tenant.clone(),
                module_key: owner.module_key.clone(),
                channel,
                headers: BTreeMap::from([
                    (headers::CALL_ID.to_string(), call_id.clone()),
                    (
                        headers::HANGUP_REASON.to_string(),
                        "recording-recovered".to_string(),
                    ),
                    (
                        headers::GATEWAY.to_string(),
                        self.gateway.kind().to_string(),
                    ),
                    (
                        headers::CONTENT_TYPE.to_string(),
                        "application/json".to_string(),
                    ),
                ]),
                body: body.to_string().into_bytes().into(),
                content_type: Some("application/json".to_string()),
                idempotency_key: format!("voip:{call_id}:call-hangup:recovered"),
                explicit_event_id: true,
                received_at: now_rfc3339(),
                cloud_event: None,
            };
            let decision = deliver_with_requeue(
                &*self.intake,
                &Delivery {
                    kind: EventKind::CallHangup,
                    message,
                },
            )
            .await;
            tracing::warn!(call_id = %call_id, recordings = recordings.len(), decision = ?decision, "voip: a recovered recording was delivered to its call's flow");
            self.audit.append(
                crate::audit::scope_of(owner.audit.as_ref(), Some(&owner.module_key)),
                crate::audit::event(crate::audit::events::RECORDING_RECOVERED_DELIVERED)
                    .with_field("callId", call_id.clone())
                    .with_field("recordings", recordings.len())
                    .with_field("decision", format!("{decision:?}"))
                    .with_diagnostic_code(codes::RECORDING_RECOVERED),
            );
            for recording in &recordings {
                recorder.mark_delivered(&recording.key).await;
            }
        }
        count
    }

    /// Route an outbound operation by name.
    pub async fn operate(
        &self,
        module_key: Option<&str>,
        operation: &str,
        message: &OutboundMessage,
        wait: bool,
    ) -> Result<Value, OpError> {
        if operation != "originate" {
            if let Some(call) = call_id_of(message).and_then(|id| self.call(&id)) {
                self.note_audit_context(&call, message);
            }
        }
        match operation {
            "originate" => self.originate(module_key, message).await,
            "play" => self.play(message, wait).await,
            "hangup" => self.hangup(message).await,
            "transfer" => self.transfer(message).await,
            "dtmf" => self.dtmf(message).await,
            "record-start" => self.record_start(message).await,
            "record-stop" => self.record_stop(message).await,
            other => Err(OpError::permanent(
                codes::OUTBOUND_INVALID,
                format!(
                    "unknown voip operation '{other}' (originate, play, hangup, transfer, dtmf, record-start, record-stop)"
                ),
            )),
        }
    }

    /// The refusal of an originate the toll-fraud controls stopped: an audit event (to the
    /// requesting instance's journal, or the deployment's when the request carried no routing),
    /// and the diagnostic the flow sees (a rate limit is retryable; the rest are not).
    fn refuse_originate(
        &self,
        module_key: Option<&str>,
        context: Option<&sutra_channels::TransportAuditContext>,
        call_id: &str,
        destination: &str,
        refusal: &Refusal,
    ) -> OpError {
        let masked = crate::policy::masked(destination);
        tracing::warn!(
            rule = refusal.rule(),
            module = module_key.unwrap_or_default(),
            call_id = %call_id,
            destination = %masked,
            "voip: originate refused ({}): {}",
            refusal.rule(),
            refusal.explain()
        );
        let (name, code) = if refusal.is_rate_limit() {
            (
                crate::audit::events::ORIGINATE_RATE_LIMITED,
                codes::ORIGINATE_RATE_LIMITED,
            )
        } else {
            (
                crate::audit::events::ORIGINATE_REFUSED,
                codes::ORIGINATE_REFUSED,
            )
        };
        let module_key = module_key.map(str::to_string).or_else(|| {
            self.routes()
                .module_for_outbound(None)
                .map(|m| m.module_key.clone())
        });
        self.audit.append(
            crate::audit::scope_of(context, module_key.as_deref()),
            crate::audit::event(name)
                .with_field("rule", refusal.rule())
                .with_field("callId", call_id.to_string())
                .with_sensitive_field("destination", masked.clone())
                .with_diagnostic_code(code),
        );
        let message = format!(
            "originate to {masked} refused ({}): {}",
            refusal.rule(),
            refusal.explain()
        );
        if refusal.is_rate_limit() {
            OpError::retryable(codes::ORIGINATE_RATE_LIMITED, message)
        } else {
            OpError::permanent(codes::ORIGINATE_REFUSED, message)
        }
    }

    /// End every call's media and stop the gateway client.
    pub fn shutdown(&self) {
        self.gateway.shutdown();
    }
}

/// How many times, and how far apart, a delivery the engine asked to have redelivered
/// (`NackRequeue` — e.g. the instance is mid-step and holds its claim) is retried before it is
/// given up. A call's later events wait behind it, so they keep their order.
pub const REQUEUE_ATTEMPTS: u32 = 40;
pub const REQUEUE_DELAY_MIN_MS: u64 = 50;
pub const REQUEUE_DELAY_MAX_MS: u64 = 1000;

async fn deliver_with_requeue(intake: &dyn InboundIntake, delivery: &Delivery) -> AckDecision {
    let mut delay = REQUEUE_DELAY_MIN_MS;
    let mut decision = intake.deliver(delivery.message.clone()).await;
    // A call-start is never redelivered: a refused start is a busy / rejected call.
    if delivery.kind == EventKind::CallStart {
        return decision;
    }
    let mut attempt = 1;
    while decision == AckDecision::NackRequeue && attempt < REQUEUE_ATTEMPTS {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        delay = (delay * 2).min(REQUEUE_DELAY_MAX_MS);
        attempt += 1;
        decision = intake.deliver(delivery.message.clone()).await;
    }
    decision
}

/// Complete the recordings of a call whose end is not delivered.
fn close_recordings(call_id: &str, listening: Option<ListenState>, meeting: Option<MeetingState>) {
    let mut pending = Vec::new();
    if let Some(mut state) = listening {
        pending.extend(state.stop_recording(call_id));
    }
    if let Some(mut state) = meeting {
        for participant in state.participants.values_mut() {
            pending.extend(participant.audio.stop_recording(call_id));
        }
    }
    if pending.is_empty() {
        return;
    }
    let call_id = call_id.to_string();
    tokio::spawn(async move {
        for recording in pending {
            let _ = recording.reference(&call_id).await;
        }
    });
}

/// How [`VoipHub::end_call`] ends a call.
#[derive(Debug, Clone)]
pub struct EndOptions {
    pub reason: String,
    pub hangup_by: Option<String>,
    pub sip_status: Option<u16>,
    /// Send a hang-up to the gateway.
    pub hangup_gateway: bool,
    pub close_media: bool,
    /// Deliver the `call-hangup` (when an instance exists).
    pub deliver: bool,
}

impl EndOptions {
    fn rejected(reason: &str) -> EndOptions {
        EndOptions {
            reason: reason.to_string(),
            hangup_by: Some("sutra".to_string()),
            sip_status: None,
            hangup_gateway: true,
            close_media: true,
            deliver: false,
        }
    }

    fn failed(reason: &str) -> EndOptions {
        EndOptions {
            reason: reason.to_string(),
            hangup_by: Some("sutra".to_string()),
            sip_status: None,
            hangup_gateway: true,
            close_media: true,
            deliver: true,
        }
    }
}

/// Play `samples` paced; answers the outcome and whether this playback gave consent (an
/// announcement marked `consent: true`, played to the end, the first on the call).
async fn play_paced(
    call: &Arc<Call>,
    samples: Vec<i16>,
    barge_in: bool,
    mark: Option<String>,
    consent: bool,
) -> (PlaybackOutcome, bool) {
    let generation = call.play_generation.load(Ordering::SeqCst);
    let _turn = call.play_lock.lock().await;
    let none = PlaybackOutcome {
        frames_sent: 0,
        frames_total: 0,
        interrupted: true,
    };
    if call.play_generation.load(Ordering::SeqCst) != generation || call.is_ended() {
        return (none, false);
    }
    let Some(link) = call.media() else {
        return (none, false);
    };
    let frame_ms = 20;
    let frames: Vec<MediaOut> = frames_of(&samples, link.format.rate, frame_ms)
        .iter()
        .map(|frame| MediaOut::Binary(audio::encode(frame, &link.format)))
        .collect();
    call.barge_in_enabled
        .store(barge_in && !call.is_listen_only(), Ordering::SeqCst);
    call.playing.store(true, Ordering::SeqCst);
    // Where the playback starts in the call; the recording holds its chunks from there until
    // the played audio is mixed in.
    let offset = lock(&call.listen).as_mut().map(|state| {
        state.catch_up();
        let at = state.position();
        if let Some(position) = state.recording_position(at) {
            if let Some(writer) = state.recording.as_mut() {
                writer.hold(position);
            }
        }
        at
    });
    let session_offset = lock(&call.meeting).as_ref().map(MeetingState::elapsed_ms);
    let outcome = pace(frames, frame_ms, &link.out, || {
        call.play_generation.load(Ordering::SeqCst) != generation || call.is_ended()
    })
    .await;
    call.playing.store(false, Ordering::SeqCst);
    let mut consented = false;
    if let (Some(offset), Some(state)) = (offset, lock(&call.listen).as_mut()) {
        // What was played goes into the recording where it played.
        let frame_len = (link.format.rate * frame_ms / 1000) as usize;
        let played = (outcome.frames_sent * frame_len).min(samples.len());
        if let Some(position) = state.recording_position(offset) {
            if let Some(writer) = state.recording.as_mut() {
                writer.mix(position, &samples[..played]);
            }
        }
        if consent && !outcome.interrupted && state.consent_at_ms.is_none() {
            state.consent_at_ms = Some(offset * 1000 / u64::from(state.rate.max(1)));
            consented = true;
        }
    }
    if let (Some(at), Some(state)) = (session_offset, lock(&call.meeting).as_mut()) {
        if consent && !outcome.interrupted && state.consent_at_ms.is_none() {
            state.consent_at_ms = Some(at);
            consented = true;
        }
    }
    if offset.is_none() && session_offset.is_none() && consent && !outcome.interrupted {
        let mut at = lock(&call.consent_ms);
        if at.is_none() {
            *at = Some(call.created.elapsed().as_millis() as u64);
            consented = true;
        }
    }
    if !outcome.interrupted && link.framing == Framing::Generic {
        if let Some(name) = mark {
            let _ = link
                .out
                .send(MediaOut::Text(protocol::generic_mark(&name)))
                .await;
        }
    }
    (outcome, consented)
}

/// Whether `digits` are DTMF keys (`0-9`, `*`, `#`, `A-D`), at most 64 of them.
fn validate_digits(digits: &str) -> Result<(), OpError> {
    let valid = !digits.is_empty()
        && digits.len() <= 64
        && digits
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '*' | '#' | 'A'..='D' | 'a'..='d'));
    if valid {
        Ok(())
    } else {
        Err(OpError::permanent(
            codes::OUTBOUND_INVALID,
            "dtmf digits must be 1-64 of 0-9, *, #, A-D",
        ))
    }
}

/// Send `digits` over the call's PCM bridge, `gap_ms` apart (a key the far end can tell from
/// the next). Answers how many were sent.
async fn send_digits(call: &Call, digits: &str, gap_ms: u64) -> Result<usize, OpError> {
    let link = call
        .media()
        .ok_or_else(|| OpError::retryable(codes::CALL_NOT_CONNECTED, "call has no media yet"))?;
    if link.framing != Framing::Bridge {
        return Err(OpError::permanent(
            codes::OUTBOUND_INVALID,
            "the generic media protocol carries no outbound DTMF",
        ));
    }
    let mut sent = 0;
    for (i, digit) in digits.chars().enumerate() {
        if call.is_ended() {
            break;
        }
        if i > 0 && gap_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(gap_ms)).await;
        }
        let _ = link
            .out
            .send(MediaOut::Text(protocol::bridge_dtmf(
                &digit.to_ascii_uppercase().to_string(),
            )))
            .await;
        sent += 1;
    }
    Ok(sent)
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn json_body(message: &OutboundMessage) -> Value {
    let looks_json = message
        .content_type
        .as_deref()
        .is_some_and(|ct| ct.to_ascii_lowercase().contains("json"))
        || message.body.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
    if !looks_json {
        return Value::Object(Default::default());
    }
    serde_json::from_slice(&message.body).unwrap_or(Value::Object(Default::default()))
}

fn call_id_of(message: &OutboundMessage) -> Option<String> {
    message
        .headers
        .get(headers::CALL_ID)
        .cloned()
        .or_else(|| {
            let body = json_body(message);
            body.get("callId")
                .or_else(|| body.get("call_id"))
                .and_then(value_text)
        })
        .filter(|id| !id.trim().is_empty())
}

/// A deterministic call id for an outbox row, so a redelivered originate keeps its id.
pub fn mint_call_id(outbox_key: &str) -> String {
    if outbox_key.trim().is_empty() {
        return format!("sutra-{}", uuid::Uuid::new_v4().simple());
    }
    format!(
        "sutra-{}",
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, outbox_key.as_bytes()).simple()
    )
}

/// The SIP URI to dial: a SIP URI as is, `user@host` as `sip:user@host`, and a bare number
/// (`+15550001111`, `tel:+1…`) through `trunk` — `sip:<number>@<trunk>`, whose host the gateway
/// replaces with the trunk's own destination.
pub fn sip_destination(raw: &str, trunk: Option<&str>) -> Option<String> {
    let trimmed = raw.trim();
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with("sip:") || lowered.starts_with("sips:") {
        return Some(trimmed.to_string());
    }
    if trimmed.contains('@') {
        return Some(format!("sip:{trimmed}"));
    }
    let number = trimmed
        .strip_prefix("tel:")
        .or_else(|| trimmed.strip_prefix("TEL:"))
        .unwrap_or(trimmed)
        .split(';')
        .next()
        .unwrap_or_default();
    let dialable = !number.is_empty()
        && number
            .chars()
            .enumerate()
            .all(|(i, c)| c.is_ascii_digit() || (i == 0 && c == '+') || "-. ()".contains(c));
    match trunk {
        Some(trunk) if dialable => {
            let compact: String = number
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '+')
                .collect();
            Some(format!("sip:{compact}@{trunk}"))
        }
        _ => None,
    }
}

/// The recorded prompt a play request names (`{"prompt": "<name>"}`), if it names one.
pub fn play_prompt_name(message: &OutboundMessage) -> Option<String> {
    json_body(message)
        .get("prompt")
        .and_then(value_text)
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// The file a prompt name refers to: `<dir>/<name>`, with `.wav` appended when the last
/// segment has no extension. A name is one or more `/`-separated segments of ASCII letters,
/// digits, `.`, `_` and `-` — never empty, `.` or `..` — so it cannot leave the directory.
pub fn prompt_path(
    dir: Option<&std::path::Path>,
    name: &str,
) -> Result<std::path::PathBuf, String> {
    let dir = dir.ok_or_else(|| {
        format!(
            "play names prompt '{name}', but no prompts directory is configured ({})",
            crate::config::ENV_PROMPTS_DIR
        )
    })?;
    if name.len() > 200 {
        return Err("prompt name is longer than 200 characters".to_string());
    }
    let mut path = dir.to_path_buf();
    let segments: Vec<&str> = name.split('/').collect();
    for segment in &segments {
        let valid_chars = segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if segment.is_empty() || *segment == "." || *segment == ".." || !valid_chars {
            return Err(format!("prompt name '{name}' is not a relative file name"));
        }
        path.push(segment);
    }
    if !segments.last().is_some_and(|last| last.contains('.')) {
        path.set_extension("wav");
    }
    Ok(path)
}

/// Read a recorded prompt (a WAV file) and resample it to `rate`.
async fn load_prompt(path: std::path::PathBuf, rate: u32) -> Result<Vec<i16>, String> {
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("prompt {}: {e}", path.display()))?;
    if !bytes.starts_with(b"RIFF") {
        return Err(format!("prompt {} is not a WAV file", path.display()));
    }
    let format = AudioFormat::parse("audio/wav").ok_or("audio/wav is not a known format")?;
    audio::convert_to_rate(&bytes, &format, rate)
}

/// The audio of a play request: a raw audio body (`content-type` / `x-audio-format` name
/// its format; a RIFF body is WAV) or JSON `{"audio":{"data":<base64>,"format":…}}` (also
/// accepted flat as `{"data","format"}`), resampled to `rate`.
pub fn decode_play_audio(message: &OutboundMessage, rate: u32) -> Result<Vec<i16>, String> {
    let declared = message
        .headers
        .get(headers::AUDIO_FORMAT)
        .cloned()
        .or_else(|| message.content_type.clone())
        .unwrap_or_default();
    if declared.to_ascii_lowercase().contains("json") || message.body.first() == Some(&b'{') {
        let body: Value = serde_json::from_slice(&message.body)
            .map_err(|e| format!("play body is not JSON: {e}"))?;
        let audio_value = body.get("audio").unwrap_or(&body);
        let data = audio_value
            .get("data")
            .and_then(Value::as_str)
            .ok_or("play body has no audio 'data'")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .map_err(|e| format!("audio data is not base64: {e}"))?;
        let format_name = audio_value
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("audio/wav");
        let format = AudioFormat::parse(format_name)
            .ok_or_else(|| format!("unknown audio format '{format_name}'"))?;
        return audio::convert_to_rate(&bytes, &format, rate);
    }
    let format = if message.body.starts_with(b"RIFF") {
        AudioFormat::parse("audio/wav")
    } else {
        AudioFormat::parse(&declared)
    }
    .ok_or_else(|| format!("unknown audio format '{declared}'"))?;
    audio::convert_to_rate(&message.body, &format, rate)
}
