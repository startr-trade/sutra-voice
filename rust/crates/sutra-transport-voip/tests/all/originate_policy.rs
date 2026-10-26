//! The toll-fraud controls on `voip://originate`, through the hub: deny-all until configured,
//! the allow-list, premium and international numbers, and the rate limits. A destination that
//! passes reaches the gateway (here the generic one, which refuses for its own reason).

use std::sync::Arc;

use sutra_transport_voip::config::{route_set_of, VoipConfig};
use sutra_transport_voip::gateway::GenericGateway;
use sutra_transport_voip::hub::{OpError, VoipHub};
use sutra_transport_voip::policy::OriginatePolicy;

use super::support::*;

fn hub(policy: OriginatePolicy) -> Arc<VoipHub> {
    let (intake, _rx) = FakeIntake::new();
    let routes = route_set_of(&definitions(CHANNELS, "billing")).unwrap();
    let mut config = VoipConfig::generic("ws://unused", Some("media-secret"));
    config.default_trunk = Some("carrier".into());
    config.originate = policy;
    VoipHub::new(config, intake, routes, Arc::new(GenericGateway))
}

async fn originate(hub: &VoipHub, destination: &str, n: u32) -> Result<serde_json::Value, OpError> {
    let body = serde_json::json!({"destination": destination, "callId": format!("o-{n}")});
    hub.operate(
        Some("acme/billing/1.0.0"),
        "originate",
        &outbound(
            "voip://acme/billing/1.0.0/originate",
            None,
            &body.to_string(),
            "application/json",
        ),
        true,
    )
    .await
}

fn code(result: Result<serde_json::Value, OpError>) -> (bool, String) {
    match result {
        Ok(v) => panic!("dialled: {v}"),
        Err(OpError::Permanent(d)) => (false, d.code),
        Err(OpError::Retryable(d)) => (true, d.code),
    }
}

const REFUSED: &str = "SUTRA.VOIP.ORIGINATE_REFUSED";
/// What the generic gateway answers: the destination got past the controls.
const DIALLED: &str = "SUTRA.VOIP.GATEWAY_REJECTED";

#[tokio::test]
async fn nothing_is_dialled_by_default() {
    let hub = hub(VoipConfig::generic("x", None).originate);
    let (retryable, code) = code(originate(&hub, "+15551230000", 1).await);
    assert_eq!((retryable, code.as_str()), (false, REFUSED));
}

#[tokio::test]
async fn each_rule_refuses_with_a_diagnostic() {
    let hub = hub(OriginatePolicy {
        home_prefix: Some("+1".into()),
        per_destination_per_hour: 2,
        global_per_minute: 4,
        ..OriginatePolicy::allowing(&["+1*", "+4420*", "sip:*@pbx.internal"])
    });
    // On the list: dialled.
    assert_eq!(code(originate(&hub, "+15551230000", 1).await).1, DIALLED);
    assert_eq!(
        code(originate(&hub, "sip:1001@pbx.internal", 2).await).1,
        DIALLED
    );
    // Off the list.
    assert_eq!(
        code(originate(&hub, "sip:1001@elsewhere", 3).await).1,
        REFUSED
    );
    // Premium-rate, though `+1*` covers it.
    assert_eq!(code(originate(&hub, "+19005550100", 4).await).1, REFUSED);
    // International: only the specifically allowed range.
    assert_eq!(code(originate(&hub, "+33155550100", 5).await).1, REFUSED);
    assert_eq!(code(originate(&hub, "+442071234567", 6).await).1, DIALLED);
    // The same destination a third time within the hour.
    assert_eq!(code(originate(&hub, "+15551230000", 7).await).1, DIALLED);
    let (retryable, c) = code(originate(&hub, "+15551230000", 8).await);
    assert_eq!(
        (retryable, c.as_str()),
        (true, "SUTRA.VOIP.ORIGINATE_RATE_LIMITED")
    );
    // Four dialled in the minute: the global limit.
    let (retryable, c) = code(originate(&hub, "+15551239999", 9).await);
    assert_eq!(
        (retryable, c.as_str()),
        (true, "SUTRA.VOIP.ORIGINATE_RATE_LIMITED")
    );
}

#[test]
fn the_controls_read_from_the_environment() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    };
    let resolver: sutra_transport_spi::EnvRefResolver = |r: &str| Ok(r.to_string());
    let config = VoipConfig::from_lookup(env(&[]), resolver).unwrap();
    assert!(config.originate.allow.is_empty(), "deny-all by default");
    let config = VoipConfig::from_lookup(
        env(&[
            ("SUTRA_VOIP_ORIGINATE_ALLOW", "+1555*, sip:*@pbx"),
            ("SUTRA_VOIP_ORIGINATE_HOME_PREFIX", "+1"),
            ("SUTRA_VOIP_ORIGINATE_ALLOW_INTERNATIONAL", "true"),
            ("SUTRA_VOIP_ORIGINATE_RATE_PER_DESTINATION", "3"),
            ("SUTRA_VOIP_ORIGINATE_RATE_GLOBAL", "0"),
            ("SUTRA_VOIP_ORIGINATE_BLOCK_PREFIXES", "+1900,+44"),
        ]),
        resolver,
    )
    .unwrap();
    let policy = &config.originate;
    assert_eq!(policy.allow, vec!["+1555*", "sip:*@pbx"]);
    assert_eq!(policy.home_prefix.as_deref(), Some("+1"));
    assert!(policy.allow_international && !policy.allow_premium);
    assert_eq!(
        (policy.per_destination_per_hour, policy.global_per_minute),
        (3, 0)
    );
    assert_eq!(policy.blocked_prefixes, vec!["+1900", "+44"]);
    assert!(
        VoipConfig::from_lookup(env(&[("SUTRA_VOIP_ORIGINATE_HOME_PREFIX", "1")]), resolver)
            .is_err()
    );
}
