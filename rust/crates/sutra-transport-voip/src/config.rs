//! Configuration: the engine-wide gateway settings (environment) and the per-deployment
//! channel model (the `transport: voip` channel definitions).
//!
//! Engine-wide (one gateway per engine process):
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `SUTRA_VOIP_GATEWAY` | `generic` or `rustpbx` | `generic` |
//! | `SUTRA_VOIP_RWI_URL` | the PBX control WebSocket (`ws://pbx:8080/rwi/v1`, or `wss://` for TLS) | required for `rustpbx` |
//! | `SUTRA_VOIP_RWI_CA_FILE` | extra trusted CA certificates (PEM) for a `wss://` control URL | none: the platform's roots |
//! | `SUTRA_VOIP_RWI_TOKEN` | the control bearer token — a reference (`env:…`, `secret:…`) | required for `rustpbx` |
//! | `SUTRA_VOIP_PUBLIC_MEDIA_URL` | how the gateway reaches this engine (`ws://engine:8080`) | `ws://localhost:8080` |
//! | `SUTRA_VOIP_MEDIA_TOKEN` | the deployment's media credential — a reference. A media-stream client sends it as `Authorization: Bearer`; the PBX's webhook / IVR-step URLs carry it (or the header); the PCM bridge never sees it — each bridge URL carries a short-lived token minted for its one call | none: those routes refuse every request |
//! | `SUTRA_VOIP_BRIDGE_TOKEN_TTL_SECONDS` | how long a minted bridge token is valid (each opens one bridge socket) | `60` |
//! | `SUTRA_VOIP_BRIDGE_RATE` | the PCM bridge sample rate requested from the gateway | `8000` |
//! | `SUTRA_VOIP_TRUNK` | the default trunk for originate | none |
//! | `SUTRA_VOIP_PROMPTS_DIR` | the directory recorded prompts are played from (`play` with `prompt`) | none: a `prompt` play is refused |
//! | `SUTRA_VOIP_RECORDING_STORE` | where recordings go: `s3://<bucket>[/<prefix>]` (any S3-compatible store) or `memory://<bucket>` | none: a call asked to be recorded is refused |
//! | `SUTRA_VOIP_RECORDING_ENDPOINT` | the store's URL for a self-hosted / non-AWS store (`http://store:9000`, path-style) | AWS |
//! | `SUTRA_VOIP_RECORDING_REGION` | the store's region | `us-east-1` |
//! | `SUTRA_VOIP_RECORDING_ACCESS_KEY` / `_SECRET_KEY` | the store's credentials — references (`env:…`, `secret:…`) | required for `s3://` |
//! | `SUTRA_VOIP_RECORDING_CREATE_BUCKET` | create the bucket at boot when it does not exist | `false` |
//! | `SUTRA_VOIP_RECORDING_KEK` | the key-encryption key recordings are sealed under — a reference | required unless encryption is `none` |
//! | `SUTRA_VOIP_RECORDING_KEY_ID` | the key id sealed recordings name | `recordings` |
//! | `SUTRA_VOIP_RECORDING_ENCRYPTION` | `envelope` (sealed, the default) or `none` (the store encrypts) | `envelope` |
//! | `SUTRA_VOIP_RECORDING_MAX_SECONDS` | the longest recording kept (audio beyond it is not recorded) | `7200` |
//! | `SUTRA_VOIP_RECORDING_SPOOL_DIR` | where each recording's part in progress is spooled (sealed) until it is uploaded, so a crash loses at most a chunk; `none` holds parts in memory | `<temp dir>/sutra-voip-spool` |
//! | `SUTRA_VOIP_RECORDING_SWEEP_SECONDS` | how often interrupted recordings are looked for (and at start-up); `0`: at start-up only | `600` |
//! | `SUTRA_VOIP_INSTANCE_ID` | who writes a recording (stable across restarts); distinct per process sharing a host | `HOSTNAME` |
//! | `SUTRA_VOIP_ORIGINATE_ALLOW` | the destinations `originate` may dial: glob patterns (`+1555*`, `sip:*@pbx.internal`, `*`), comma-separated | none: **every originate is refused** |
//! | `SUTRA_VOIP_ORIGINATE_HOME_PREFIX` | the home numbering plan (`+1`): `+` numbers outside it are international | none: every `+` number is international |
//! | `SUTRA_VOIP_ORIGINATE_ALLOW_INTERNATIONAL` / `_ALLOW_PREMIUM` | dial international / premium-rate numbers without naming them specifically | `false` |
//! | `SUTRA_VOIP_ORIGINATE_BLOCK_PREFIXES` | the premium-rate prefixes refused | a built-in list ([`crate::policy::DEFAULT_BLOCKED_PREFIXES`]) |
//! | `SUTRA_VOIP_ORIGINATE_RATE_PER_DESTINATION` / `_RATE_GLOBAL` | originates per destination per hour / in total per minute (`0`: unlimited) | `10` / `60` |
//!
//! Per channel (inbound, `transport: voip`): the event kind comes from `event:` (or a `bind:`
//! naming the kind); a `bind: number:…` channel is a `call-start` channel. Turn settings sit
//! on the `call-start` channel (and on any channel of the deployment for originated calls):
//! `silence-ms` (700), `max-turn-ms` (30000), `min-speech-ms` (60), `energy-threshold`
//! (500, linear RMS), `dead-air-ms` (2000). The `call-turn` channel may set `turn-body: json`
//! (the default — the utterance as base64 WAV inside a JSON message) or `turn-body: wav`.
//!
//! A `call-transcript` channel turns on live transcription of listen-only calls:
//! `transcribe.channel` names an outbound `llm` channel of the same deployment (it must
//! `require: [realtime]`; the session opens on its first candidate with live sessions, falling
//! back like any channel call, and the candidates carry the provider, model, transcription model
//! and key), `transcribe.language` (a hint), `transcribe.drain-ms` (how long the session may
//! finish after the hang-up; 3000). A `call-keyword` channel names the phrases it reports in
//! `phrases:` (a list, or comma-separated).
//!
//! **Meeting sessions** (a gateway that streams every participant of a meeting on one tagged
//! socket, see [`crate::protocol`]): the `call-start` channel may set `mode: listen` (no turns;
//! a tagged session is always listen-only), `record: true` (record each participant's audio —
//! pair it with `<q:audit capture="media">` on the process) and the video sampling,
//! `video.fps` (1), `video.enabled` (true), `video.format` (`image/jpeg`), `video.max-width`
//! (640), `video.quality` (70), `video.extra-frames` (true: a frame of every participant at
//! turn boundaries and audio-flagged moments). A `call-participant` channel receives joins and
//! leaves; a `call-segment` channel cuts the session into per-participant media segments
//! (written to the recording store, delivered to the flow as references), see
//! [`crate::meeting::SegmentSettings`]. Analysis of the segments is the flow's.

use std::collections::BTreeMap;
use std::sync::Arc;

use sutra_channels::{ChannelDefinition, Diagnostic};
use sutra_transport_spi::EnvRefResolver;

use crate::binding::{is_number_bind, NumberRouter};
use crate::codes;
use crate::features::DEFAULT_DEAD_AIR_MS;
use crate::gateway::GENERIC_GATEWAY;
use crate::rustpbx::{RwiSettings, RUSTPBX_GATEWAY};
use crate::vad::VadConfig;
use crate::TRANSPORT;

pub const ENV_GATEWAY: &str = "SUTRA_VOIP_GATEWAY";
pub const ENV_RWI_URL: &str = "SUTRA_VOIP_RWI_URL";
pub const ENV_RWI_TOKEN: &str = "SUTRA_VOIP_RWI_TOKEN";
pub const ENV_RWI_CA_FILE: &str = "SUTRA_VOIP_RWI_CA_FILE";
pub const ENV_PUBLIC_MEDIA_URL: &str = "SUTRA_VOIP_PUBLIC_MEDIA_URL";
pub const ENV_MEDIA_TOKEN: &str = "SUTRA_VOIP_MEDIA_TOKEN";
pub const ENV_BRIDGE_RATE: &str = "SUTRA_VOIP_BRIDGE_RATE";
pub const ENV_TRUNK: &str = "SUTRA_VOIP_TRUNK";
pub const ENV_PROMPTS_DIR: &str = "SUTRA_VOIP_PROMPTS_DIR";
pub const ENV_RECORDING_STORE: &str = "SUTRA_VOIP_RECORDING_STORE";
pub const ENV_RECORDING_ENDPOINT: &str = "SUTRA_VOIP_RECORDING_ENDPOINT";
pub const ENV_RECORDING_REGION: &str = "SUTRA_VOIP_RECORDING_REGION";
pub const ENV_RECORDING_ACCESS_KEY: &str = "SUTRA_VOIP_RECORDING_ACCESS_KEY";
pub const ENV_RECORDING_SECRET_KEY: &str = "SUTRA_VOIP_RECORDING_SECRET_KEY";
pub const ENV_RECORDING_CREATE_BUCKET: &str = "SUTRA_VOIP_RECORDING_CREATE_BUCKET";
pub const ENV_RECORDING_KEK: &str = "SUTRA_VOIP_RECORDING_KEK";
pub const ENV_RECORDING_KEY_ID: &str = "SUTRA_VOIP_RECORDING_KEY_ID";
pub const ENV_RECORDING_ENCRYPTION: &str = "SUTRA_VOIP_RECORDING_ENCRYPTION";
pub const ENV_RECORDING_MAX_SECONDS: &str = "SUTRA_VOIP_RECORDING_MAX_SECONDS";
pub const ENV_RECORDING_SPOOL_DIR: &str = "SUTRA_VOIP_RECORDING_SPOOL_DIR";
pub const ENV_RECORDING_SWEEP_SECONDS: &str = "SUTRA_VOIP_RECORDING_SWEEP_SECONDS";
pub const ENV_BRIDGE_TOKEN_TTL_SECONDS: &str = "SUTRA_VOIP_BRIDGE_TOKEN_TTL_SECONDS";
pub const ENV_ORIGINATE_ALLOW: &str = "SUTRA_VOIP_ORIGINATE_ALLOW";
pub const ENV_ORIGINATE_HOME_PREFIX: &str = "SUTRA_VOIP_ORIGINATE_HOME_PREFIX";
pub const ENV_ORIGINATE_ALLOW_INTERNATIONAL: &str = "SUTRA_VOIP_ORIGINATE_ALLOW_INTERNATIONAL";
pub const ENV_ORIGINATE_ALLOW_PREMIUM: &str = "SUTRA_VOIP_ORIGINATE_ALLOW_PREMIUM";
pub const ENV_ORIGINATE_BLOCK_PREFIXES: &str = "SUTRA_VOIP_ORIGINATE_BLOCK_PREFIXES";
pub const ENV_ORIGINATE_RATE_PER_DESTINATION: &str = "SUTRA_VOIP_ORIGINATE_RATE_PER_DESTINATION";
pub const ENV_ORIGINATE_RATE_GLOBAL: &str = "SUTRA_VOIP_ORIGINATE_RATE_GLOBAL";

/// How long a transcription session may run on after the hang-up, by default.
pub const DEFAULT_TRANSCRIBE_DRAIN_MS: u64 = 3000;
/// The longest recording kept, by default (two hours).
pub const DEFAULT_RECORDING_MAX_SECONDS: u64 = 7200;

pub const DEFAULT_PUBLIC_MEDIA_URL: &str = "ws://localhost:8080";

/// Which gateway control client to run.
#[derive(Debug, Clone)]
pub enum GatewaySettings {
    Generic,
    Rustpbx(RwiSettings),
}

/// Where and how recordings are written.
#[derive(Clone)]
pub struct RecordingSettings {
    pub store: sutra_objectstore::StoreLocation,
    pub endpoint: Option<String>,
    pub region: String,
    /// Resolved credentials (`None` for a `memory://` store).
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub create_bucket: bool,
    /// The resolved key-encryption secret; `None` = stored as is (`encryption: none`).
    pub kek_secret: Option<String>,
    pub key_id: String,
    pub max_seconds: u64,
    /// Where parts in progress are spooled; `None` holds them in memory.
    pub spool_dir: Option<std::path::PathBuf>,
    /// How often interrupted recordings are swept (0: at start-up only).
    pub sweep_seconds: u64,
}

impl std::fmt::Debug for RecordingSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingSettings")
            .field("store", &self.store)
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("credentials", &self.access_key.as_ref().map(|_| "<set>"))
            .field("create_bucket", &self.create_bucket)
            .field("sealed", &self.kek_secret.is_some())
            .field("key_id", &self.key_id)
            .field("max_seconds", &self.max_seconds)
            .field("spool_dir", &self.spool_dir)
            .field("sweep_seconds", &self.sweep_seconds)
            .finish()
    }
}

impl RecordingSettings {
    /// An in-memory store with sealing under `kek_secret` (tests).
    pub fn memory(bucket: &str, kek_secret: Option<&str>) -> RecordingSettings {
        RecordingSettings {
            store: sutra_objectstore::StoreLocation::parse(&format!("memory://{bucket}"))
                .expect("a valid bucket name"),
            endpoint: None,
            region: "us-east-1".to_string(),
            access_key: None,
            secret_key: None,
            create_bucket: false,
            kek_secret: kek_secret.map(str::to_string),
            key_id: "recordings".to_string(),
            max_seconds: DEFAULT_RECORDING_MAX_SECONDS,
            spool_dir: None,
            sweep_seconds: crate::recording::DEFAULT_SWEEP_SECONDS,
        }
    }

    fn from_lookup(
        get: &dyn Fn(&str) -> Option<String>,
        resolve: &dyn Fn(&str) -> Result<Option<String>, Diagnostic>,
    ) -> Result<Option<RecordingSettings>, Diagnostic> {
        let invalid = |message: String| Diagnostic::error(codes::CONFIG_INVALID, message);
        let Some(uri) = get(ENV_RECORDING_STORE) else {
            return Ok(None);
        };
        let store = sutra_objectstore::StoreLocation::parse(&uri)
            .map_err(|e| invalid(format!("{ENV_RECORDING_STORE}: {e}")))?;
        let encryption = get(ENV_RECORDING_ENCRYPTION)
            .unwrap_or_else(|| "envelope".to_string())
            .to_ascii_lowercase();
        let kek_secret = match encryption.as_str() {
            "envelope" => Some(resolve(ENV_RECORDING_KEK)?.ok_or_else(|| {
                invalid(format!(
                    "{ENV_RECORDING_STORE} is set: recordings are sealed under {ENV_RECORDING_KEK}                      (or set {ENV_RECORDING_ENCRYPTION}=none when the store encrypts at rest)"
                ))
            })?),
            "none" => None,
            other => {
                return Err(invalid(format!(
                    "{ENV_RECORDING_ENCRYPTION}: '{other}' is not envelope or none"
                )))
            }
        };
        let (access_key, secret_key) = if store.scheme == "s3" {
            let access = resolve(ENV_RECORDING_ACCESS_KEY)?;
            let secret = resolve(ENV_RECORDING_SECRET_KEY)?;
            if access.is_none() || secret.is_none() {
                return Err(invalid(format!(
                    "an s3:// recording store needs {ENV_RECORDING_ACCESS_KEY} and {ENV_RECORDING_SECRET_KEY}"
                )));
            }
            (access, secret)
        } else {
            (None, None)
        };
        let max_seconds = match get(ENV_RECORDING_MAX_SECONDS) {
            None => DEFAULT_RECORDING_MAX_SECONDS,
            Some(raw) => raw.parse::<u64>().ok().filter(|v| *v > 0).ok_or_else(|| {
                invalid(format!(
                    "{ENV_RECORDING_MAX_SECONDS}: '{raw}' is not a positive number"
                ))
            })?,
        };
        let create_bucket = match get(ENV_RECORDING_CREATE_BUCKET).as_deref() {
            None => false,
            Some(v) if v.eq_ignore_ascii_case("true") => true,
            Some(v) if v.eq_ignore_ascii_case("false") => false,
            Some(other) => {
                return Err(invalid(format!(
                    "{ENV_RECORDING_CREATE_BUCKET}: '{other}' is not true or false"
                )))
            }
        };
        let spool_dir = match get(ENV_RECORDING_SPOOL_DIR) {
            Some(v) if v.eq_ignore_ascii_case("none") => None,
            Some(v) => Some(std::path::PathBuf::from(v)),
            None => Some(std::env::temp_dir().join("sutra-voip-spool")),
        };
        let sweep_seconds = match get(ENV_RECORDING_SWEEP_SECONDS) {
            None => crate::recording::DEFAULT_SWEEP_SECONDS,
            Some(raw) => raw.parse::<u64>().map_err(|_| {
                invalid(format!(
                    "{ENV_RECORDING_SWEEP_SECONDS}: '{raw}' is not a number"
                ))
            })?,
        };
        Ok(Some(RecordingSettings {
            spool_dir,
            sweep_seconds,
            store,
            endpoint: get(ENV_RECORDING_ENDPOINT),
            region: get(ENV_RECORDING_REGION).unwrap_or_else(|| "us-east-1".to_string()),
            access_key,
            secret_key,
            create_bucket,
            kek_secret,
            key_id: get(ENV_RECORDING_KEY_ID).unwrap_or_else(|| "recordings".to_string()),
            max_seconds,
        }))
    }
}

/// Engine-wide transport settings.
#[derive(Debug, Clone)]
pub struct VoipConfig {
    pub gateway: GatewaySettings,
    /// Base URL the gateway dials for media, without a trailing slash.
    pub public_media_url: String,
    /// The resolved media / webhook token; `None` refuses every media request.
    pub media_token: Option<String>,
    pub bridge_rate: u32,
    pub default_trunk: Option<String>,
    /// How long the gateway may take to open the bridge socket.
    pub bridge_connect_timeout_ms: u64,
    /// Where recorded prompts live: a `play` request naming `prompt` plays
    /// `<dir>/<prompt>.wav`. `None` refuses such a request.
    pub prompts_dir: Option<std::path::PathBuf>,
    /// Where recordings go; `None` refuses a call asked to be recorded.
    pub recording: Option<RecordingSettings>,
    /// How long a minted bridge token is valid.
    pub bridge_token_ttl_seconds: u64,
    /// The toll-fraud controls on `originate`.
    pub originate: crate::policy::OriginatePolicy,
}

impl VoipConfig {
    /// A generic-gateway configuration (tests and the gateway-less deployment).
    pub fn generic(public_media_url: &str, media_token: Option<&str>) -> VoipConfig {
        VoipConfig {
            gateway: GatewaySettings::Generic,
            public_media_url: public_media_url.trim_end_matches('/').to_string(),
            media_token: media_token.map(str::to_string),
            bridge_rate: 8000,
            default_trunk: None,
            bridge_connect_timeout_ms: 10_000,
            prompts_dir: None,
            recording: None,
            bridge_token_ttl_seconds: crate::tokens::DEFAULT_TOKEN_TTL_SECONDS,
            originate: crate::policy::OriginatePolicy::default(),
        }
    }

    /// Read the settings through `lookup` (the process environment in production), resolving
    /// the two secrets through `resolver`.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
        resolver: EnvRefResolver,
    ) -> Result<VoipConfig, Diagnostic> {
        let get = |key: &str| {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let resolve = |key: &str| -> Result<Option<String>, Diagnostic> {
            match get(key) {
                None => Ok(None),
                Some(reference) => resolver(&reference).map(Some).map_err(|reason| {
                    Diagnostic::error(codes::CONFIG_INVALID, format!("{key}: {reason}"))
                }),
            }
        };
        let gateway_name = get(ENV_GATEWAY).unwrap_or_else(|| GENERIC_GATEWAY.to_string());
        let gateway = match gateway_name.to_ascii_lowercase().as_str() {
            GENERIC_GATEWAY => GatewaySettings::Generic,
            RUSTPBX_GATEWAY => {
                let url = get(ENV_RWI_URL).ok_or_else(|| {
                    Diagnostic::error(
                        codes::CONFIG_INVALID,
                        format!("{ENV_GATEWAY}=rustpbx needs {ENV_RWI_URL}"),
                    )
                })?;
                let token = resolve(ENV_RWI_TOKEN)?.ok_or_else(|| {
                    Diagnostic::error(
                        codes::CONFIG_INVALID,
                        format!("{ENV_GATEWAY}=rustpbx needs {ENV_RWI_TOKEN}"),
                    )
                })?;
                let lowered = url.to_ascii_lowercase();
                if !lowered.starts_with("ws://") && !lowered.starts_with("wss://") {
                    return Err(Diagnostic::error(
                        codes::CONFIG_INVALID,
                        format!("{ENV_RWI_URL}: '{url}' is not a ws:// or wss:// URL"),
                    ));
                }
                let mut settings = RwiSettings::new(url, token);
                settings.ca_file = get(ENV_RWI_CA_FILE).map(std::path::PathBuf::from);
                if let Some(path) = &settings.ca_file {
                    crate::rustpbx::tls_config(&settings).map_err(|reason| {
                        Diagnostic::error(
                            codes::CONFIG_INVALID,
                            format!("{ENV_RWI_CA_FILE}: {reason} ({})", path.display()),
                        )
                    })?;
                }
                GatewaySettings::Rustpbx(settings)
            }
            other => {
                return Err(Diagnostic::error(
                    codes::CONFIG_INVALID,
                    format!("{ENV_GATEWAY}: unknown gateway '{other}' (generic, rustpbx)"),
                ))
            }
        };
        let bridge_rate = match get(ENV_BRIDGE_RATE) {
            None => 8000,
            Some(raw) => raw
                .parse::<u32>()
                .ok()
                .filter(|r| *r >= 8000)
                .ok_or_else(|| {
                    Diagnostic::error(
                        codes::CONFIG_INVALID,
                        format!("{ENV_BRIDGE_RATE}: '{raw}' is not a sample rate of 8000 or more"),
                    )
                })?,
        };
        let prompts_dir = get(ENV_PROMPTS_DIR).map(std::path::PathBuf::from);
        if let Some(dir) = &prompts_dir {
            if !dir.is_dir() {
                return Err(Diagnostic::error(
                    codes::CONFIG_INVALID,
                    format!("{ENV_PROMPTS_DIR}: '{}' is not a directory", dir.display()),
                ));
            }
        }
        let recording = RecordingSettings::from_lookup(&get, &resolve)?;
        let originate = originate_policy_of(&get)?;
        let bridge_token_ttl_seconds = match get(ENV_BRIDGE_TOKEN_TTL_SECONDS) {
            None => crate::tokens::DEFAULT_TOKEN_TTL_SECONDS,
            Some(raw) => raw.parse::<u64>().ok().filter(|v| *v > 0).ok_or_else(|| {
                Diagnostic::error(
                    codes::CONFIG_INVALID,
                    format!("{ENV_BRIDGE_TOKEN_TTL_SECONDS}: '{raw}' is not a positive number"),
                )
            })?,
        };
        Ok(VoipConfig {
            bridge_token_ttl_seconds,
            originate,
            gateway,
            public_media_url: get(ENV_PUBLIC_MEDIA_URL)
                .unwrap_or_else(|| DEFAULT_PUBLIC_MEDIA_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            media_token: resolve(ENV_MEDIA_TOKEN)?,
            bridge_rate,
            default_trunk: get(ENV_TRUNK),
            bridge_connect_timeout_ms: 10_000,
            prompts_dir,
            recording,
        })
    }
}

/// The toll-fraud controls from the environment (see [`crate::policy`]).
fn originate_policy_of(
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<crate::policy::OriginatePolicy, Diagnostic> {
    let invalid = |message: String| Diagnostic::error(codes::CONFIG_INVALID, message);
    let list = |raw: String| -> Vec<String> {
        raw.split([',', ' ', '\n', '\t'])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let flag = |key: &str| -> Result<bool, Diagnostic> {
        match get(key).as_deref() {
            None => Ok(false),
            Some(v) if v.eq_ignore_ascii_case("true") => Ok(true),
            Some(v) if v.eq_ignore_ascii_case("false") => Ok(false),
            Some(other) => Err(invalid(format!("{key}: '{other}' is not true or false"))),
        }
    };
    let count = |key: &str, default: u32| -> Result<u32, Diagnostic> {
        match get(key) {
            None => Ok(default),
            Some(raw) => raw
                .parse::<u32>()
                .map_err(|_| invalid(format!("{key}: '{raw}' is not a number"))),
        }
    };
    let mut policy = crate::policy::OriginatePolicy {
        allow: get(ENV_ORIGINATE_ALLOW).map(list).unwrap_or_default(),
        allow_international: flag(ENV_ORIGINATE_ALLOW_INTERNATIONAL)?,
        allow_premium: flag(ENV_ORIGINATE_ALLOW_PREMIUM)?,
        home_prefix: get(ENV_ORIGINATE_HOME_PREFIX),
        per_destination_per_hour: count(
            ENV_ORIGINATE_RATE_PER_DESTINATION,
            crate::policy::DEFAULT_RATE_PER_DESTINATION_PER_HOUR,
        )?,
        global_per_minute: count(
            ENV_ORIGINATE_RATE_GLOBAL,
            crate::policy::DEFAULT_RATE_GLOBAL_PER_MINUTE,
        )?,
        ..crate::policy::OriginatePolicy::default()
    };
    if let Some(raw) = get(ENV_ORIGINATE_BLOCK_PREFIXES) {
        policy.blocked_prefixes = list(raw);
    }
    if let Some(home) = &policy.home_prefix {
        if !home.starts_with('+')
            || home.len() < 2
            || !home[1..].chars().all(|c| c.is_ascii_digit())
        {
            return Err(invalid(format!(
                "{ENV_ORIGINATE_HOME_PREFIX}: '{home}' is not a + and a country code"
            )));
        }
    }
    Ok(policy)
}

/// The inbound message kinds this transport delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventKind {
    CallStart,
    CallTurn,
    CallDtmf,
    CallProgress,
    CallHangup,
    /// One final transcript segment of a listen-only call.
    CallTranscript,
    /// A configured phrase was heard on a listen-only call.
    CallKeyword,
    /// A participant of a meeting session joined or left.
    CallParticipant,
    /// One participant's media over one time segment of a meeting session, as references.
    CallSegment,
}

impl EventKind {
    pub const ALL: [EventKind; 9] = [
        EventKind::CallStart,
        EventKind::CallTurn,
        EventKind::CallDtmf,
        EventKind::CallProgress,
        EventKind::CallHangup,
        EventKind::CallTranscript,
        EventKind::CallKeyword,
        EventKind::CallParticipant,
        EventKind::CallSegment,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::CallStart => "call-start",
            EventKind::CallTurn => "call-turn",
            EventKind::CallDtmf => "call-dtmf",
            EventKind::CallProgress => "call-progress",
            EventKind::CallHangup => "call-hangup",
            EventKind::CallTranscript => "call-transcript",
            EventKind::CallKeyword => "call-keyword",
            EventKind::CallParticipant => "call-participant",
            EventKind::CallSegment => "call-segment",
        }
    }

    pub fn parse(name: &str) -> Option<EventKind> {
        let name = name.trim();
        EventKind::ALL
            .into_iter()
            .find(|kind| kind.as_str().eq_ignore_ascii_case(name))
    }
}

/// Per-call turn and analytics settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CallSettings {
    pub vad: VadConfig,
    pub dead_air_ms: u64,
}

impl Default for CallSettings {
    fn default() -> Self {
        CallSettings {
            vad: VadConfig::default(),
            dead_air_ms: DEFAULT_DEAD_AIR_MS,
        }
    }
}

fn number_property<T: std::str::FromStr>(
    definition: &ChannelDefinition,
    key: &str,
) -> Result<Option<T>, Diagnostic> {
    match definition.properties.get(key) {
        None => Ok(None),
        Some(raw) => raw.trim().parse::<T>().map(Some).map_err(|_| {
            Diagnostic::error(
                codes::CHANNEL_INVALID,
                format!(
                    "channel '{}': '{key}' must be a number, got '{raw}'",
                    definition.binding.channel_name
                ),
            )
        }),
    }
}

impl CallSettings {
    /// Settings declared on `definition`, over `base`.
    pub fn from_definition(
        definition: &ChannelDefinition,
        base: CallSettings,
    ) -> Result<CallSettings, Diagnostic> {
        let mut settings = base;
        if let Some(v) = number_property::<u32>(definition, "silence-ms")? {
            settings.vad.silence_ms = v.max(settings.vad.frame_ms);
        }
        if let Some(v) = number_property::<u32>(definition, "max-turn-ms")? {
            settings.vad.max_turn_ms = v.max(settings.vad.frame_ms);
        }
        if let Some(v) = number_property::<u32>(definition, "min-speech-ms")? {
            settings.vad.min_speech_ms = v;
        }
        if let Some(v) = number_property::<f64>(definition, "energy-threshold")? {
            settings.vad.energy_threshold = v.max(0.0);
        }
        if let Some(v) = number_property::<u64>(definition, "dead-air-ms")? {
            settings.dead_air_ms = v;
        }
        Ok(settings)
    }

    fn declares_any(definition: &ChannelDefinition) -> bool {
        [
            "silence-ms",
            "max-turn-ms",
            "min-speech-ms",
            "energy-threshold",
            "dead-air-ms",
        ]
        .iter()
        .any(|k| definition.properties.contains_key(*k))
    }
}

/// How a `call-turn` message carries the utterance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TurnBody {
    /// `application/json`: `{callId, turnIndex, endReason, bargeIn, audio: {data: <base64
    /// WAV>, format: "audio/wav"}, features}` — the shape a flow hands straight to a model
    /// channel (`audio: turn.audio`).
    #[default]
    Json,
    /// The WAV file itself (`audio/wav`), for a consumer that wants the bytes.
    Wav,
}

impl TurnBody {
    fn parse(definition: &ChannelDefinition) -> Result<TurnBody, Diagnostic> {
        match definition.properties.get("turn-body").map(|v| v.trim()) {
            None => Ok(TurnBody::Json),
            Some(v) if v.eq_ignore_ascii_case("json") => Ok(TurnBody::Json),
            Some(v) if v.eq_ignore_ascii_case("wav") => Ok(TurnBody::Wav),
            Some(other) => Err(Diagnostic::error(
                codes::CHANNEL_INVALID,
                format!(
                    "channel '{}': turn-body must be json or wav, got '{other}'",
                    definition.binding.channel_name
                ),
            )),
        }
    }
}

/// Every status a `call-progress` message can carry.
pub const PROGRESS_STATUSES: [&str; 6] = [
    "ringing",
    "answered",
    "connected",
    "busy",
    "no-answer",
    "failed",
];

/// The `statuses:` filter of a `call-progress` channel: a comma-separated subset of
/// [`PROGRESS_STATUSES`]. A flow that waits once for the outcome of a call declares only the
/// outcomes (`connected, busy, no-answer, failed`), so `ringing` and `answered` never satisfy
/// its wait.
fn progress_statuses_of(
    definition: &ChannelDefinition,
) -> Result<Option<std::collections::BTreeSet<String>>, Diagnostic> {
    let Some(raw) = definition.properties.get("statuses") else {
        return Ok(None);
    };
    let mut statuses = std::collections::BTreeSet::new();
    for status in raw
        .trim_matches(['[', ']'])
        .split([',', '\n'])
        .map(|s| {
            s.trim()
                .trim_start_matches('-')
                .trim()
                .trim_matches(['"', '\''])
                .to_ascii_lowercase()
        })
        .filter(|s| !s.is_empty())
    {
        if !PROGRESS_STATUSES.contains(&status.as_str()) {
            return Err(Diagnostic::error(
                codes::CHANNEL_INVALID,
                format!(
                    "channel '{}': unknown progress status '{status}' (one of {})",
                    definition.binding.channel_name,
                    PROGRESS_STATUSES.join(", ")
                ),
            ));
        }
        statuses.insert(status);
    }
    if statuses.is_empty() {
        return Err(Diagnostic::error(
            codes::CHANNEL_INVALID,
            format!(
                "channel '{}': statuses names no status",
                definition.binding.channel_name
            ),
        ));
    }
    Ok(Some(statuses))
}

/// One deployment's voip channels.
#[derive(Debug, Clone)]
pub struct ModuleRoute {
    pub tenant: String,
    pub module_key: String,
    /// Kind → channel name (call-start is per number target instead, see [`StartTarget`]).
    pub channels: BTreeMap<EventKind, String>,
    /// Settings for calls this deployment originates.
    pub settings: CallSettings,
    /// How the `call-turn` channel carries an utterance.
    pub turn_body: TurnBody,
    /// The `call-progress` statuses delivered (`statuses:` on the channel); `None` = all.
    pub progress_statuses: Option<std::collections::BTreeSet<String>>,
    /// Live transcription of this deployment's listen-only calls (the `call-transcript`
    /// channel's `transcribe.*`).
    pub transcribe: Option<TranscribeSettings>,
    /// The phrases the `call-keyword` channel reports, lower-cased.
    pub keyword_phrases: Vec<String>,
    /// How meeting sessions are cut into per-participant media segments (the `call-segment`
    /// channel's `segment.*`).
    pub segments: Option<crate::meeting::SegmentSettings>,
}

/// How a listen-only call is transcribed: a live audio session opened through an `llm` channel
/// of the same deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscribeSettings {
    /// `transcribe.channel`: the `llm` channel the session is opened through.
    pub channel: String,
    pub language: Option<String>,
    pub drain_ms: u64,
}

/// `transcribe.*` keys that named a provider directly. The model is configured on
/// the `llm` channel the block names instead, so they are refused with a pointer to it.
pub const DIRECT_MODEL_KEYS: [&str; 6] = [
    "provider",
    "model",
    "transcription-model",
    "base-url",
    "api-key",
    "timeout-ms",
];

/// Refuse a direct model setting under `block.` on `definition`.
pub(crate) fn refuse_direct_model_keys(
    definition: &ChannelDefinition,
    block: &str,
) -> Result<(), Diagnostic> {
    for key in DIRECT_MODEL_KEYS {
        if definition
            .properties
            .contains_key(&format!("{block}.{key}"))
        {
            return Err(Diagnostic::error(
                codes::MODEL_CHANNEL_INVALID,
                format!(
                    "channel '{}': '{block}.{key}' is not a voip setting — the model is configured \
                     on an `llm` channel of this deployment, named by '{block}.channel'",
                    definition.binding.channel_name
                ),
            ));
        }
    }
    Ok(())
}

/// What a `transcribe.channel` reference needs of the `llm` channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelChannelUse {
    /// Live transcription: the channel must `require: [realtime]`.
    Transcription,
}

impl ModelChannelUse {
    fn required(self) -> &'static str {
        match self {
            ModelChannelUse::Transcription => "realtime",
        }
    }
}

/// Check that `target`, named by `<block>.channel` on `definition`, is an outbound `llm`
/// channel of the same deployment that calls (not `mode: job`) and requires what `usage` needs.
pub(crate) fn check_model_channel(
    definition: &ChannelDefinition,
    block: &str,
    target: &str,
    usage: ModelChannelUse,
    definitions: &[ChannelDefinition],
) -> Result<(), Diagnostic> {
    let name = &definition.binding.channel_name;
    let module_key = definition.binding.namespace.module_key();
    let invalid = |why: String| {
        Diagnostic::error(
            codes::MODEL_CHANNEL_INVALID,
            format!("channel '{name}': {block}.channel '{target}' {why}"),
        )
    };
    let Some(llm) = definitions.iter().find(|d| {
        d.binding.channel_name == target && d.binding.namespace.module_key() == module_key
    }) else {
        return Err(invalid(format!("names no channel of {module_key}")));
    };
    let is_llm = llm
        .transport
        .as_deref()
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("llm"));
    if !is_llm || !llm.is_outbound() {
        return Err(invalid(
            "is not an outbound 'transport: llm' channel".to_string(),
        ));
    }
    if llm
        .properties
        .get("llm.mode")
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("job"))
    {
        return Err(invalid(
            "is a 'mode: job' channel; the transport calls it and needs 'mode: call'".to_string(),
        ));
    }
    let required = usage.required();
    let declared = llm
        .properties
        .get("llm.require")
        .map(|raw| {
            raw.trim_matches(['[', ']'])
                .split([',', '\n'])
                .map(|s| {
                    s.trim()
                        .trim_start_matches('-')
                        .trim()
                        .trim_matches(['"', '\''])
                })
                .any(|s| s.eq_ignore_ascii_case(required))
        })
        .unwrap_or(false);
    if !declared {
        return Err(invalid(format!(
            "must declare 'require: [{required}]', so every candidate that serves it can"
        )));
    }
    Ok(())
}

impl TranscribeSettings {
    fn parse(
        definition: &ChannelDefinition,
        definitions: &[ChannelDefinition],
    ) -> Result<Option<TranscribeSettings>, Diagnostic> {
        let name = &definition.binding.channel_name;
        let get = |key: &str| {
            definition
                .properties
                .get(&format!("transcribe.{key}"))
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        refuse_direct_model_keys(definition, "transcribe")?;
        let Some(channel) = get("channel") else {
            if definition
                .properties
                .keys()
                .any(|k| k.starts_with("transcribe."))
            {
                return Err(Diagnostic::error(
                    codes::MODEL_CHANNEL_INVALID,
                    format!("channel '{name}': transcribe needs a 'channel' (an llm channel)"),
                ));
            }
            return Ok(None);
        };
        check_model_channel(
            definition,
            "transcribe",
            &channel,
            ModelChannelUse::Transcription,
            definitions,
        )?;
        let drain_ms = match get("drain-ms") {
            None => DEFAULT_TRANSCRIBE_DRAIN_MS,
            Some(raw) => raw.parse::<u64>().map_err(|_| {
                Diagnostic::error(
                    codes::CHANNEL_INVALID,
                    format!("channel '{name}': transcribe.drain-ms must be a number, got '{raw}'"),
                )
            })?,
        };
        Ok(Some(TranscribeSettings {
            channel,
            language: get("language"),
            drain_ms,
        }))
    }
}

/// The `phrases:` of a `call-keyword` channel (a YAML list or comma-separated), lower-cased.
fn keyword_phrases_of(definition: &ChannelDefinition) -> Vec<String> {
    let Some(raw) = definition.properties.get("phrases") else {
        return Vec::new();
    };
    raw.trim_matches(['[', ']'])
        .split([',', '\n'])
        .map(|s| {
            s.trim()
                .trim_start_matches('-')
                .trim()
                .trim_matches(['"', '\''])
                .to_lowercase()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

impl ModuleRoute {
    pub fn channel(&self, kind: EventKind) -> Option<&str> {
        self.channels.get(&kind).map(String::as_str)
    }
}

/// Where a called number lands.
#[derive(Debug, Clone)]
pub struct StartTarget {
    pub module: Arc<ModuleRoute>,
    pub channel: String,
    pub settings: CallSettings,
    /// What the `call-start` channel says about the session (listen-only, recorded, video).
    pub session: crate::meeting::SessionSettings,
}

/// The active voip routing: number binding plus each deployment's channels.
#[derive(Debug, Clone, Default)]
pub struct RouteSet {
    pub numbers: NumberRouter<StartTarget>,
    pub modules: BTreeMap<String, Arc<ModuleRoute>>,
}

impl RouteSet {
    /// The module serving outbound-call events: `module_key` when given and known, else the
    /// only deployment with a `call-progress` channel (first by key when several).
    pub fn module_for_outbound(&self, module_key: Option<&str>) -> Option<Arc<ModuleRoute>> {
        if let Some(key) = module_key {
            return self.modules.get(key).cloned();
        }
        let mut candidates = self
            .modules
            .values()
            .filter(|m| m.channel(EventKind::CallProgress).is_some());
        let first = candidates.next().cloned();
        if candidates.next().is_some() {
            tracing::warn!(
                "voip: an unqualified outbound destination matches several deployments; using the first"
            );
        }
        first
    }
}

/// The event kind an inbound definition serves.
pub fn event_kind_of(definition: &ChannelDefinition) -> Result<EventKind, Diagnostic> {
    let name = &definition.binding.channel_name;
    if let Some(event) = definition.properties.get("event") {
        if event.trim().eq_ignore_ascii_case("call-analysis") {
            return Err(Diagnostic::error(
                codes::CHANNEL_INVALID,
                format!(
                    "channel '{name}': 'call-analysis' was replaced by 'call-segment': the transport \
                     delivers each segment's media as references, and the flow analyses them \
                     (an `llm` channel with a `media-store`)"
                ),
            ));
        }
        return EventKind::parse(event).ok_or_else(|| {
            Diagnostic::error(
                codes::CHANNEL_INVALID,
                format!("channel '{name}': unknown event '{event}'"),
            )
        });
    }
    match definition.bind_spec.as_deref() {
        Some(spec) if is_number_bind(spec) => Ok(EventKind::CallStart),
        Some(spec) => EventKind::parse(spec).ok_or_else(|| {
            Diagnostic::error(
                codes::CHANNEL_INVALID,
                format!("channel '{name}': bind '{spec}' is neither a number bind nor an event kind"),
            )
        }),
        None => EventKind::parse(name).ok_or_else(|| {
            Diagnostic::error(
                codes::CHANNEL_INVALID,
                format!("channel '{name}': declare 'event:' (call-start, call-turn, call-dtmf, call-progress, call-hangup, call-transcript, call-keyword, call-participant, call-segment)"),
            )
        }),
    }
}

fn is_voip_inbound(definition: &ChannelDefinition) -> bool {
    definition
        .transport
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case(TRANSPORT))
        && !definition.is_outbound()
}

/// Build the routing from every active definition (non-voip and outbound ones are skipped).
/// Fails closed on an invalid channel or a number bound twice.
pub fn route_set_of(definitions: &[ChannelDefinition]) -> Result<RouteSet, Diagnostic> {
    let mut inbound: Vec<&ChannelDefinition> =
        definitions.iter().filter(|d| is_voip_inbound(d)).collect();
    inbound.sort_by(|a, b| {
        (a.binding.namespace.module_key(), &a.binding.channel_name)
            .cmp(&(b.binding.namespace.module_key(), &b.binding.channel_name))
    });
    let mut modules: BTreeMap<String, ModuleRoute> = BTreeMap::new();
    let mut starts: Vec<(&ChannelDefinition, String)> = Vec::new();
    for definition in &inbound {
        let kind = event_kind_of(definition)?;
        let key = definition.binding.namespace.module_key();
        let module = modules.entry(key.clone()).or_insert_with(|| ModuleRoute {
            tenant: definition.binding.namespace.tenant.clone(),
            module_key: key.clone(),
            channels: BTreeMap::new(),
            settings: CallSettings::default(),
            turn_body: TurnBody::Json,
            progress_statuses: None,
            transcribe: None,
            keyword_phrases: Vec::new(),
            segments: None,
        });
        crate::meeting::refuse_analysis_keys(definition)?;
        if CallSettings::declares_any(definition) && module.settings == CallSettings::default() {
            module.settings = CallSettings::from_definition(definition, CallSettings::default())?;
        }
        if kind == EventKind::CallStart {
            let spec = definition.bind_spec.as_deref().unwrap_or_default();
            if !is_number_bind(spec) {
                return Err(Diagnostic::error(
                    codes::BIND_INVALID,
                    format!(
                        "channel '{}': a call-start channel binds numbers ('bind: number:…')",
                        definition.binding.channel_name
                    ),
                ));
            }
            starts.push((definition, key));
            continue;
        }
        if let Some(existing) = module.channels.get(&kind) {
            return Err(Diagnostic::error(
                codes::CHANNEL_INVALID,
                format!(
                    "{key}: both '{existing}' and '{}' serve {}; one channel per kind",
                    definition.binding.channel_name,
                    kind.as_str()
                ),
            ));
        }
        if kind == EventKind::CallTurn {
            module.turn_body = TurnBody::parse(definition)?;
        }
        if kind == EventKind::CallProgress {
            module.progress_statuses = progress_statuses_of(definition)?;
        }
        if kind == EventKind::CallTranscript {
            module.transcribe = TranscribeSettings::parse(definition, definitions)?;
        }
        if kind == EventKind::CallKeyword {
            module.keyword_phrases = keyword_phrases_of(definition);
        }
        if kind == EventKind::CallSegment {
            module.segments = Some(crate::meeting::SegmentSettings::parse(definition)?);
        }
        module
            .channels
            .insert(kind, definition.binding.channel_name.clone());
    }
    let modules: BTreeMap<String, Arc<ModuleRoute>> = modules
        .into_iter()
        .map(|(key, module)| (key, Arc::new(module)))
        .collect();
    let mut entries = Vec::new();
    for (definition, key) in starts {
        let module = Arc::clone(&modules[&key]);
        let settings = CallSettings::from_definition(definition, module.settings)?;
        let session = crate::meeting::SessionSettings::parse(definition)?;
        entries.push((
            format!("{key}/{}", definition.binding.channel_name),
            definition.bind_spec.as_deref().unwrap_or_default(),
            StartTarget {
                module,
                channel: definition.binding.channel_name.clone(),
                settings,
                session,
            },
        ));
    }
    Ok(RouteSet {
        numbers: NumberRouter::build(entries)?,
        modules,
    })
}
