//! The gateway control seam: what the transport asks of the PBX / media gateway beyond the
//! media socket (originate, attach, bridge-to-media, transfer, hang-up), and the call events
//! the gateway reports back.
//!
//! Two implementations ship: [`GenericGateway`] (`gateway: generic` — no control client; the
//! media stream is all there is, so hang-up is a frame on the media socket and originate /
//! transfer are unsupported) and [`crate::rustpbx::RwiClient`] (`gateway: rustpbx`).

use std::collections::BTreeMap;

/// A gateway command failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    /// The control channel is down or timed out — retrying may succeed.
    Retryable(String),
    /// The gateway refused the command, or the gateway cannot do it at all.
    Permanent(String),
}

impl GatewayError {
    pub fn message(&self) -> &str {
        match self {
            GatewayError::Retryable(m) | GatewayError::Permanent(m) => m,
        }
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// An outbound call request. The transport mints `call_id` before the gateway sees it, so
/// the flow can correlate on it from the start.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OriginateRequest {
    pub call_id: String,
    /// A SIP URI.
    pub destination: String,
    pub caller_id: Option<String>,
    pub timeout_secs: Option<u64>,
    /// The gateway's trunk name to dial through.
    pub trunk: Option<String>,
    pub extra_headers: BTreeMap<String, String>,
}

/// A call event reported by the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    /// A call the gateway knows about (announced inbound calls; also echoed for originated
    /// calls).
    Created {
        call_id: String,
        caller: Option<String>,
        callee: Option<String>,
        direction: Option<String>,
    },
    Ringing {
        call_id: String,
        early_media: bool,
    },
    Answered {
        call_id: String,
    },
    Busy {
        call_id: String,
    },
    NoAnswer {
        call_id: String,
    },
    Hangup {
        call_id: String,
        reason: Option<String>,
        hangup_by: Option<String>,
        sip_status: Option<u16>,
    },
    TransferFailed {
        call_id: String,
        reason: Option<String>,
    },
    /// The control channel (re)connected.
    Connected,
}

impl GatewayEvent {
    pub fn call_id(&self) -> Option<&str> {
        match self {
            GatewayEvent::Created { call_id, .. }
            | GatewayEvent::Ringing { call_id, .. }
            | GatewayEvent::Answered { call_id }
            | GatewayEvent::Busy { call_id }
            | GatewayEvent::NoAnswer { call_id }
            | GatewayEvent::Hangup { call_id, .. }
            | GatewayEvent::TransferFailed { call_id, .. } => Some(call_id),
            GatewayEvent::Connected => None,
        }
    }
}

/// Control over calls on a gateway.
#[async_trait::async_trait]
pub trait GatewayControl: Send + Sync {
    /// The `gateway:` name (`generic`, `rustpbx`).
    fn kind(&self) -> &str;
    /// Whether this gateway has a control channel at all. Without one, hang-up is a media
    /// frame and originate / transfer are unsupported.
    fn has_control(&self) -> bool;
    async fn originate(&self, request: &OriginateRequest) -> Result<(), GatewayError>;
    /// Take control of a call the gateway announced (inbound).
    async fn attach(&self, call_id: &str) -> Result<(), GatewayError>;
    /// Bridge the call's media to Sutra's media endpoint at `media_url`.
    async fn bridge(&self, call_id: &str, media_url: &str) -> Result<(), GatewayError>;
    /// Transfer the call to `target` (an extension or SIP URI).
    async fn transfer(&self, call_id: &str, target: &str) -> Result<(), GatewayError>;
    async fn hangup(&self, call_id: &str, reason: Option<&str>) -> Result<(), GatewayError>;
    /// Stop the control client (shutdown).
    fn shutdown(&self) {}
}

/// `gateway: generic` — the media stream is the whole interface.
#[derive(Debug, Default)]
pub struct GenericGateway;

pub const GENERIC_GATEWAY: &str = "generic";

#[async_trait::async_trait]
impl GatewayControl for GenericGateway {
    fn kind(&self) -> &str {
        GENERIC_GATEWAY
    }

    fn has_control(&self) -> bool {
        false
    }

    async fn originate(&self, _request: &OriginateRequest) -> Result<(), GatewayError> {
        Err(GatewayError::Permanent(
            "the generic gateway has no control channel; originate needs a gateway with one"
                .to_string(),
        ))
    }

    async fn attach(&self, _call_id: &str) -> Result<(), GatewayError> {
        Ok(())
    }

    async fn bridge(&self, _call_id: &str, _media_url: &str) -> Result<(), GatewayError> {
        Ok(())
    }

    async fn transfer(&self, _call_id: &str, _target: &str) -> Result<(), GatewayError> {
        Err(GatewayError::Permanent(
            "the generic gateway has no control channel; transfer needs a gateway with one"
                .to_string(),
        ))
    }

    async fn hangup(&self, _call_id: &str, _reason: Option<&str>) -> Result<(), GatewayError> {
        Ok(())
    }
}
