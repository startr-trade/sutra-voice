//! Audit events this transport writes to the engine's audit journal, through the engine's
//! transport-audit SPI ([`sutra_channels::transport_audit`]).
//!
//! The engine stamps the audit routing of the instance a request belongs to onto every message
//! this transport receives (the transport answers `writes_audit_events() = true`), so an event
//! about a call lands in the journal of the instance that owns the call. An event no instance
//! owns (an originate refused before any call exists, from a process that is not audited) goes
//! to the deployment's journal instead.
//!
//! | Event type | When | Fields |
//! |---|---|---|
//! | `voip.ORIGINATE_REFUSED` | the toll-fraud controls refused an originate | `rule`, `callId`, `destination` (masked, sensitive) |
//! | `voip.ORIGINATE_RATE_LIMITED` | an originate over a rate limit | the same |
//! | `voip.CONSENT_REQUIRED` | `record-start` refused: no consent yet | `callId` |
//! | `voip.RECORDING_RECOVERED` | the sweep completed an interrupted recording (or aborted an empty one) | `callId`, `key`, `capturedMs`, `complete` |
//! | `voip.RECORDING_RECOVERED_DELIVERED` | a recovered recording's reference was delivered to the call's flow | `callId`, `recordings`, `decision` |
//!
//! Fields are recorded only when the process audits at `payload` level; at any other level the
//! event is written with its type, node and diagnostic code and an empty payload.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};
use sutra_channels::{
    transport_audit, TransportAuditContext, TransportAuditEvent, TransportAuditOutcome,
    TransportAuditRegistry, TransportAuditScope, AUDIT_JOURNAL_DEPLOYMENT_HEADER,
    AUDIT_JOURNAL_HEADER, AUDIT_JOURNAL_INSTANCE_HEADER,
};

/// The event names (the namespace, `voip`, is the transport's).
pub mod events {
    pub const ORIGINATE_REFUSED: &str = "ORIGINATE_REFUSED";
    pub const ORIGINATE_RATE_LIMITED: &str = "ORIGINATE_RATE_LIMITED";
    pub const CONSENT_REQUIRED: &str = "CONSENT_REQUIRED";
    pub const RECORDING_RECOVERED: &str = "RECORDING_RECOVERED";
    pub const RECORDING_RECOVERED_DELIVERED: &str = "RECORDING_RECOVERED_DELIVERED";
}

/// Where this transport's audit events go: the process-wide registry the engine installs its
/// journal into, or (tests) a registry of its own.
#[derive(Clone, Default)]
pub struct AuditJournal {
    injected: Option<Arc<TransportAuditRegistry>>,
}

impl AuditJournal {
    /// The engine's journal ([`sutra_channels::transport_audit`]).
    pub fn process_wide() -> AuditJournal {
        AuditJournal { injected: None }
    }

    /// A registry of the caller's own (tests install a capturing journal into it).
    pub fn injected(registry: Arc<TransportAuditRegistry>) -> AuditJournal {
        AuditJournal {
            injected: Some(registry),
        }
    }

    pub fn registry(&self) -> &TransportAuditRegistry {
        self.injected
            .as_deref()
            .unwrap_or_else(|| transport_audit())
    }

    /// Append `event` (named `name` in the `voip` namespace) to the journal `scope` names. Never
    /// blocks; a full queue is logged, anything else the journal answers is not an error here.
    pub fn append(&self, scope: TransportAuditScope, event: TransportAuditEvent) {
        let name = event.name.clone();
        match self.registry().append(scope, event) {
            TransportAuditOutcome::Dropped => {
                tracing::warn!(event = %name, "voip: the audit journal's queue is full; an audit event was dropped")
            }
            TransportAuditOutcome::Refused(reason) => {
                tracing::warn!(event = %name, "voip: the audit journal refused an event: {reason}")
            }
            _ => {}
        }
    }
}

/// A new event of this transport.
pub fn event(name: &str) -> TransportAuditEvent {
    TransportAuditEvent::new(crate::TRANSPORT, name)
}

/// The journal an event about `context`'s instance goes to, else the deployment `module_key`
/// names.
pub fn scope_of(
    context: Option<&TransportAuditContext>,
    module_key: Option<&str>,
) -> TransportAuditScope {
    match context {
        Some(context) => TransportAuditScope::Instance(context.clone()),
        None => TransportAuditScope::Deployment {
            module_key: module_key.unwrap_or_default().to_string(),
        },
    }
}

/// The audit routing a message carries (`None`: its process is not audited).
pub fn context_of(headers: &BTreeMap<String, String>) -> Option<TransportAuditContext> {
    TransportAuditContext::from_headers(headers)
}

/// `context` as the reserved headers it came in (how a recording manifest keeps it, so a
/// recovery after a restart still writes to the instance's journal).
pub fn context_to_json(context: &TransportAuditContext) -> Value {
    json!({
        AUDIT_JOURNAL_HEADER: sutra_channels::transport_audit::journal_policy_value(context.capture, context.sink.as_deref()),
        AUDIT_JOURNAL_DEPLOYMENT_HEADER: context.deployment_id,
        AUDIT_JOURNAL_INSTANCE_HEADER: context.instance_id,
    })
}

/// The context [`context_to_json`] wrote.
pub fn context_from_json(value: &Value) -> Option<TransportAuditContext> {
    let headers: BTreeMap<String, String> = value
        .as_object()?
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
        .collect();
    TransportAuditContext::from_headers(&headers)
}
