use sutra_transport_voip::protocol::*;

#[test]
fn generic_start_dtmf_mark_stop_parse() {
    let start = parse_generic_text(
        r#"{"event":"start","callId":"c1","streamId":"s1","format":"audio/L16;rate=16000","from":"+1444","to":"+1555","direction":"inbound","headers":{"X-Tenant":"a","n":3}}"#,
    )
    .unwrap();
    let GenericFrame::Start(start) = start else {
        panic!()
    };
    assert_eq!(start.call_id, "c1");
    assert_eq!(start.stream_id.as_deref(), Some("s1"));
    assert_eq!(start.format.as_deref(), Some("audio/L16;rate=16000"));
    assert_eq!(start.headers.get("x-tenant").map(String::as_str), Some("a"));
    assert_eq!(start.headers.get("n").map(String::as_str), Some("3"));
    assert_eq!(
        parse_generic_text(r#"{"event":"dtmf","digit":"7"}"#).unwrap(),
        GenericFrame::Dtmf { digit: "7".into() }
    );
    assert_eq!(
        parse_generic_text(r#"{"event":"mark","name":"m1"}"#).unwrap(),
        GenericFrame::Mark { name: "m1".into() }
    );
    assert_eq!(
        parse_generic_text(r#"{"event":"stop","reason":"bye"}"#).unwrap(),
        GenericFrame::Stop {
            reason: Some("bye".into())
        }
    );
    assert!(parse_generic_text(r#"{"event":"start"}"#).is_err());
    assert!(parse_generic_text(r#"{"event":"nope"}"#).is_err());
    assert!(parse_generic_text("not json").is_err());
}

#[test]
fn generic_outbound_frames() {
    let clear: serde_json::Value = serde_json::from_str(&generic_clear()).unwrap();
    assert_eq!(clear, serde_json::json!({"event":"clear"}));
    let mark: serde_json::Value = serde_json::from_str(&generic_mark("m")).unwrap();
    assert_eq!(mark, serde_json::json!({"event":"mark","name":"m"}));
    let hangup: serde_json::Value = serde_json::from_str(&generic_hangup()).unwrap();
    assert_eq!(hangup, serde_json::json!({"event":"hangup"}));
}

#[test]
fn bridge_dtmf_frames_both_ways() {
    assert_eq!(
        parse_bridge_text(r#"{"type":"dtmf","digit":"5","leg_id":"caller"}"#).unwrap(),
        BridgeDtmf {
            digit: "5".into(),
            leg_id: Some("caller".into())
        }
    );
    assert_eq!(
        parse_bridge_text(r##"{"type":"dtmf","digit":"#"}"##)
            .unwrap()
            .digit,
        "#"
    );
    assert!(parse_bridge_text(r#"{"type":"start"}"#).is_err());
    assert!(parse_bridge_text(r#"{"digit":"5"}"#).is_err());
    let out: serde_json::Value = serde_json::from_str(&bridge_dtmf("5")).unwrap();
    assert_eq!(out, serde_json::json!({"type":"dtmf","digit":"5"}));
}

#[test]
fn rustpbx_events_parse_flat_and_webhook_envelopes() {
    use sutra_transport_voip::gateway::GatewayEvent;
    use sutra_transport_voip::rustpbx::parse_event;
    let flat = serde_json::json!({"event_type":"call_ringing","call_id":"c1","early_media":true,"direction":"outbound"});
    assert_eq!(
        parse_event(&flat),
        Some(GatewayEvent::Ringing {
            call_id: "c1".into(),
            early_media: true
        })
    );
    let hangup = serde_json::json!({"event_type":"call_hangup","call_id":"c1","reason":"noAnswer","hangup_by":"system","sip_status":480});
    assert_eq!(
        parse_event(&hangup),
        Some(GatewayEvent::Hangup {
            call_id: "c1".into(),
            reason: Some("noAnswer".into()),
            hangup_by: Some("system".into()),
            sip_status: Some(480)
        })
    );
    let webhook = serde_json::json!({
        "rwi":"1.0","timestamp":"2026-01-01T00:00:00Z","call_id":"c9","event_type":"call_created",
        "event":{"event_type":"call_created","call_id":"c9","caller":"sip:+1444@carrier","callee":"sip:15551230000@pbx","called_phone":null,"direction":"inbound","trunk":null}
    });
    assert_eq!(
        parse_event(&webhook),
        Some(GatewayEvent::Created {
            call_id: "c9".into(),
            caller: Some("sip:+1444@carrier".into()),
            callee: Some("sip:15551230000@pbx".into()),
            direction: Some("inbound".into())
        })
    );
    for name in ["call_answered", "call_busy", "call_no_answer"] {
        assert!(
            parse_event(&serde_json::json!({"event_type": name, "call_id": "x"})).is_some(),
            "{name}"
        );
    }
    assert_eq!(
        parse_event(&serde_json::json!({"type":"command_completed","action_id":"1"})),
        None
    );
    assert_eq!(
        parse_event(&serde_json::json!({"event_type":"media_play_started","call_id":"x"})),
        None
    );
}

#[test]
fn a_party_is_its_number_not_its_uri() {
    use sutra_transport_voip::hub::party_number;
    assert_eq!(
        party_number("sip:+15557654321@203.0.113.7:5060"),
        "+15557654321"
    );
    assert_eq!(
        party_number("<sips:alice@example.test;transport=tls>"),
        "alice"
    );
    assert_eq!(
        party_number("tel:+15551230000;phone-context=x"),
        "+15551230000"
    );
    assert_eq!(party_number(" +15551230000 "), "+15551230000");
}

#[test]
fn an_originate_destination_is_a_sip_uri_or_a_number_through_the_trunk() {
    use sutra_transport_voip::hub::sip_destination;
    assert_eq!(
        sip_destination("sip:bob@pbx", None).as_deref(),
        Some("sip:bob@pbx")
    );
    assert_eq!(
        sip_destination("bob@pbx", None).as_deref(),
        Some("sip:bob@pbx")
    );
    assert_eq!(
        sip_destination("+1 (555) 000-1111", Some("carrier")).as_deref(),
        Some("sip:+15550001111@carrier")
    );
    assert_eq!(
        sip_destination("tel:+15550001111", Some("carrier")).as_deref(),
        Some("sip:+15550001111@carrier")
    );
    assert_eq!(
        sip_destination("+15550001111", None),
        None,
        "a number needs a trunk"
    );
    assert_eq!(sip_destination("not a number", Some("carrier")), None);
}
