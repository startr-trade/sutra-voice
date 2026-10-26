//! The transport's audit events, through the engine's transport-audit SPI: the transport asks
//! the engine for the journal identity, and an originate refused by the toll-fraud controls, a
//! `record-start` refused for want of consent, and an interrupted recording the sweep recovers
//! are written to the journal of the instance that owns the call (to the deployment's when the
//! request carried no routing). A capturing journal installed in a registry of the test's own
//! stands in for the engine's.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use sutra_channels::{
    transport_audit_payload, CapturingTransportAuditJournal, InlineCaller, MessageSink,
    TransportAuditContext, TransportAuditEvent, TransportAuditRegistry, TransportAuditScope,
    AUDIT_JOURNAL_DEPLOYMENT_HEADER, AUDIT_JOURNAL_HEADER, AUDIT_JOURNAL_INSTANCE_HEADER,
};
use sutra_objectstore::{MemoryObjectStore, ObjectStore, StoreLocation};
use sutra_transport_voip::audit::AuditJournal;
use sutra_transport_voip::config::{route_set_of, VoipConfig};
use sutra_transport_voip::gateway::{GatewayControl, GenericGateway};
use sutra_transport_voip::hub::{OpError, VoipHub};
use sutra_transport_voip::media::media_router;
use sutra_transport_voip::policy::OriginatePolicy;
use sutra_transport_voip::recording::{Recorder, RecordingOwner};

use super::support::*;

const MODULE: &str = "acme/billing/1.0.0";
const KEK: &str = "a test key-encryption secret";

fn journal() -> (AuditJournal, Arc<CapturingTransportAuditJournal>) {
    let registry = Arc::new(TransportAuditRegistry::new());
    let capturing = Arc::new(CapturingTransportAuditJournal::new());
    registry.install(Arc::clone(&capturing) as Arc<dyn sutra_channels::TransportAuditJournal>);
    (AuditJournal::injected(registry), capturing)
}

/// The routing the engine stamps on a request of instance `instance` audited at `capture`.
fn routing(instance: &str, capture: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            AUDIT_JOURNAL_HEADER.to_string(),
            format!("capture={capture}"),
        ),
        (
            AUDIT_JOURNAL_DEPLOYMENT_HEADER.to_string(),
            "dep-1".to_string(),
        ),
        (
            AUDIT_JOURNAL_INSTANCE_HEADER.to_string(),
            instance.to_string(),
        ),
    ])
}

fn context(instance: &str, capture: &str) -> TransportAuditContext {
    TransportAuditContext::from_headers(&routing(instance, capture)).unwrap()
}

fn events(
    journal: &CapturingTransportAuditJournal,
) -> Vec<(TransportAuditScope, TransportAuditEvent)> {
    journal.events()
}

#[test]
fn the_transport_asks_the_engine_for_the_journal_identity() {
    assert!(MessageSink::writes_audit_events(
        &sutra_transport_voip::VoipSink
    ));
    assert!(InlineCaller::writes_audit_events(
        &sutra_transport_voip::VoipInlineCaller
    ));
}

#[tokio::test]
async fn a_refused_originate_is_written_to_the_requesting_instances_journal() {
    let (audit, captured) = journal();
    let (intake, _rx) = FakeIntake::new();
    let routes = route_set_of(&definitions(CHANNELS, "billing")).unwrap();
    let mut config = VoipConfig::generic("ws://unused", Some("media-secret"));
    config.default_trunk = Some("carrier".into());
    config.originate = OriginatePolicy {
        per_destination_per_hour: 1,
        ..OriginatePolicy::allowing(&["+1555*"])
    };
    let hub = VoipHub::with_parts(
        config,
        intake,
        routes,
        Arc::new(GenericGateway) as Arc<dyn GatewayControl>,
        None,
        audit,
    );
    let originate = |destination: &str, n: u32, headers: BTreeMap<String, String>| {
        let mut message = outbound(
            &format!("voip://{MODULE}/originate"),
            None,
            &serde_json::json!({"destination": destination, "callId": format!("o-{n}")})
                .to_string(),
            "application/json",
        );
        message.headers.extend(headers);
        message
    };

    // Off the allow-list, from an instance audited at payload level.
    let refused = hub
        .operate(
            Some(MODULE),
            "originate",
            &originate("+447700900123", 1, routing("inst-1", "payload")),
            true,
        )
        .await;
    assert!(matches!(refused, Err(OpError::Permanent(_))));
    let all = events(&captured);
    assert_eq!(all.len(), 1);
    let (scope, event) = &all[0];
    assert_eq!(
        *scope,
        TransportAuditScope::Instance(context("inst-1", "payload"))
    );
    assert_eq!(event.event_type().unwrap(), "voip.ORIGINATE_REFUSED");
    assert_eq!(
        event.diagnostic_code.as_deref(),
        Some("SUTRA.VOIP.ORIGINATE_REFUSED")
    );
    assert_eq!(event.fields["callId"], "o-1");
    assert_eq!(event.fields["rule"], "not-on-allow-list");
    // The destination is sensitive: written as the redaction placeholder, never as its value.
    assert!(event.sensitive.contains("destination"));
    let payload = transport_audit_payload(context("inst-1", "payload").capture, event);
    assert!(!payload.contains("7700900123"), "{payload}");
    assert!(payload.contains("not-on-allow-list"), "{payload}");

    // The same from a process that is not audited: the deployment's journal.
    hub.operate(
        Some(MODULE),
        "originate",
        &originate("+447700900123", 2, BTreeMap::new()),
        true,
    )
    .await
    .unwrap_err();
    let all = events(&captured);
    assert_eq!(
        all[1].0,
        TransportAuditScope::Deployment {
            module_key: MODULE.to_string()
        }
    );

    // A rate limit is its own event type (and the flow sees a retryable failure).
    let _ = hub
        .operate(
            Some(MODULE),
            "originate",
            &originate("+15551230000", 3, routing("inst-3", "metadata")),
            true,
        )
        .await;
    let limited = hub
        .operate(
            Some(MODULE),
            "originate",
            &originate("+15551230000", 4, routing("inst-3", "metadata")),
            true,
        )
        .await;
    assert!(matches!(limited, Err(OpError::Retryable(_))));
    let all = events(&captured);
    let (scope, event) = all.last().unwrap();
    assert_eq!(event.event_type().unwrap(), "voip.ORIGINATE_RATE_LIMITED");
    assert_eq!(
        *scope,
        TransportAuditScope::Instance(context("inst-3", "metadata"))
    );
    // At metadata level the event is written without its fields.
    assert_eq!(
        transport_audit_payload(context("inst-3", "metadata").capture, event),
        "{}"
    );
}

#[tokio::test]
async fn a_record_start_without_consent_is_written_to_the_calls_instance_journal() {
    let (audit, captured) = journal();
    let (intake, mut rx) = FakeIntake::new();
    let routes = route_set_of(&definitions(CHANNELS, "billing")).unwrap();
    let store = MemoryObjectStore::new(StoreLocation::parse("memory://recordings/calls").unwrap());
    let recorder = Arc::new(
        Recorder::new(
            Arc::new(store) as Arc<dyn ObjectStore>,
            Some(KEK),
            "rec-key",
            600,
        )
        .with_audit(audit.clone()),
    );
    let hub = VoipHub::with_recorder(
        test_config("ws://unused", Some("media-secret")),
        intake,
        routes,
        Arc::new(GenericGateway) as Arc<dyn GatewayControl>,
        Some(recorder),
    );
    let addr = serve(media_router(Arc::clone(&hub))).await;
    let url = bridge_url(&hub, addr, "c1", "&called=%2B15551230000");
    let (_ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    next_on(&mut rx, "start").await;

    let mut request = outbound(
        &format!("voip://{MODULE}/record-start"),
        Some("c1"),
        "{}",
        "application/json",
    );
    request.headers.extend(routing("inst-7", "payload"));
    let refused = hub
        .operate(Some(MODULE), "record-start", &request, true)
        .await
        .unwrap_err();
    assert!(matches!(refused, OpError::Permanent(d) if d.code == "SUTRA.VOIP.CONSENT_REQUIRED"));
    let all = events(&captured);
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(
        all[0].0,
        TransportAuditScope::Instance(context("inst-7", "payload"))
    );
    assert_eq!(all[0].1.event_type().unwrap(), "voip.CONSENT_REQUIRED");
    assert_eq!(all[0].1.fields["callId"], "c1");
}

/// The routing reaches a recording after it started (an inbound call learns it from the first
/// request of its instance): the manifest keeps it, so a recovery in the next run of the process
/// still writes to that instance's journal — the recovery and the delivery of its reference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recovered_recording_is_written_to_the_owning_instances_journal() {
    let (audit, captured) = journal();
    let store = MemoryObjectStore::new(StoreLocation::parse("memory://recordings/calls").unwrap());
    let recorder = |owner: &str| {
        Arc::new(
            Recorder::new(
                Arc::new(store.clone()) as Arc<dyn ObjectStore>,
                Some(KEK),
                "rec-key",
                7200,
            )
            .with_part_bytes(16 * 1024)
            .with_chunk_ms(100)
            .with_owner(owner)
            .with_audit(audit.clone()),
        )
    };
    let crashed = recorder("host-1");
    let mut writer = crashed
        .begin(
            MODULE,
            &["cut-9"],
            8000,
            RecordingOwner {
                call_id: "cut-9".into(),
                tenant: "acme".into(),
                module_key: MODULE.into(),
                participant_id: None,
                participant_index: None,
                audit: None,
            },
        )
        .unwrap();
    writer.set_audit_context(&context("inst-9", "payload"));
    for n in 0..200u64 {
        writer.push(
            &(0..160)
                .map(|i| ((n * 160 + i) % 9000) as i16)
                .collect::<Vec<_>>(),
        );
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The crash: the writer goes away without finishing.
    drop(writer);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while crashed.live_count() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The next run.
    let (intake, mut rx) = FakeIntake::new();
    let routes = route_set_of(&definitions(CHANNELS, "billing")).unwrap();
    let hub = VoipHub::with_recorder(
        test_config("ws://unused", Some("media-secret")),
        intake,
        routes,
        Arc::new(GenericGateway) as Arc<dyn GatewayControl>,
        Some(recorder("host-1")),
    );
    assert_eq!(hub.sweep_recordings().await, 1);
    next_on(&mut rx, "hangup").await;
    let all = events(&captured);
    let types: Vec<String> = all.iter().map(|(_, e)| e.event_type().unwrap()).collect();
    assert_eq!(
        types,
        [
            "voip.RECORDING_RECOVERED",
            "voip.RECORDING_RECOVERED_DELIVERED"
        ]
    );
    for (scope, event) in &all {
        assert_eq!(
            *scope,
            TransportAuditScope::Instance(context("inst-9", "payload")),
            "{event:?}"
        );
        assert_eq!(event.fields["callId"], "cut-9");
    }
    assert_eq!(all[0].1.fields["complete"], false);
    assert!(all[0].1.fields["capturedMs"].as_u64().unwrap() >= 3000);
}
