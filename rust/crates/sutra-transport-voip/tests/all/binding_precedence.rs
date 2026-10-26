use sutra_transport_voip::binding::*;
use sutra_transport_voip::codes;
use sutra_transport_voip::config::{route_set_of, EventKind};

use super::support::{definitions, CHANNELS};

#[test]
fn bind_specs_parse_lists_prefixes_and_catch_all() {
    assert_eq!(
        parse_number_bind("number:+1 (555) 123-0000, number:+1555999*, 44*,*").unwrap(),
        vec![
            NumberPattern::Exact("15551230000".into()),
            NumberPattern::Prefix("1555999".into()),
            NumberPattern::Prefix("44".into()),
            NumberPattern::Any,
        ]
    );
    assert!(parse_number_bind("POST /x").is_err());
    assert!(parse_number_bind("number:").is_err());
    assert!(parse_number_bind("number:+1*55").is_err());
    assert_eq!(
        normalize_number("sip:+15551230000@carrier.example;user=phone"),
        "15551230000"
    );
    assert_eq!(normalize_number("tel:+1-555"), "1555");
}

#[test]
fn most_specific_binding_wins() {
    let router = NumberRouter::build(vec![
        ("any".to_string(), "number:*", "any"),
        ("short".to_string(), "number:+1555*", "short"),
        ("long".to_string(), "number:+1555123*", "long"),
        (
            "exact".to_string(),
            "number:+15551230000,+15551230001",
            "exact",
        ),
    ])
    .unwrap();
    assert_eq!(router.resolve("+15551230000"), Some(&"exact"));
    assert_eq!(router.resolve("sip:15551230001@pbx"), Some(&"exact"));
    assert_eq!(router.resolve("+15551239999"), Some(&"long"));
    assert_eq!(router.resolve("+15559999999"), Some(&"short"));
    assert_eq!(router.resolve("+447700900000"), Some(&"any"));
    assert_eq!(router.resolve(""), Some(&"any"));

    let no_catch_all = NumberRouter::build(vec![("a".to_string(), "number:+1555*", 1)]).unwrap();
    assert_eq!(no_catch_all.resolve("+44"), None);
}

#[test]
fn duplicate_binds_across_definitions_fail_closed() {
    for (a, b) in [
        ("number:+15551230000", "number:+1 555 123 0000"),
        ("number:+1555*", "number:+1555*"),
        ("number:*", "number:*"),
    ] {
        let error = NumberRouter::build(vec![
            ("first".to_string(), a, 1),
            ("second".to_string(), b, 2),
        ])
        .unwrap_err();
        assert_eq!(error.code, codes::BIND_DUPLICATE);
        assert_eq!(
            error.attributes.get("conflictsWith").map(String::as_str),
            Some("first")
        );
    }
    // Overlapping but distinct patterns are fine.
    assert!(NumberRouter::build(vec![
        ("a".to_string(), "number:+1555*", 1),
        ("b".to_string(), "number:+15551*", 2)
    ])
    .is_ok());

    // Across two deployments' channel definitions.
    let mut defs = definitions(CHANNELS, "billing");
    defs.extend(definitions(CHANNELS, "support"));
    let error = route_set_of(&defs).unwrap_err();
    assert_eq!(error.code, codes::BIND_DUPLICATE);
}

#[test]
fn route_set_maps_kinds_and_settings() {
    let routes = route_set_of(&definitions(CHANNELS, "billing")).unwrap();
    let target = routes.numbers.resolve("+15551230000").unwrap();
    assert_eq!(target.channel, "start");
    assert_eq!(target.settings.vad.silence_ms, 200);
    assert_eq!(target.settings.vad.max_turn_ms, 30_000);
    assert_eq!(target.module.module_key, "acme/billing/1.0.0");
    assert_eq!(target.module.channel(EventKind::CallTurn), Some("turn"));
    assert_eq!(target.module.channel(EventKind::CallHangup), Some("hangup"));
    assert!(routes.numbers.resolve("+19999").is_none());
    let bad = definitions(
        "channels:\n  - name: x\n    transport: voip\n    event: call-nope\n",
        "m",
    );
    assert_eq!(route_set_of(&bad).unwrap_err().code, codes::CHANNEL_INVALID);
}
