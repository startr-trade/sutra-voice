//! Tier 1: the analysis that moved out of the transport and into the example flows, evaluated on
//! the host with the engine's own DMN and FEEL evaluators — the same files the packages ship, read
//! from `examples/`.
//!
//! - **Who is analysed** (`rules/analysis-policy.dmn`, both meeting examples): external parties
//!   yes; staff only with the deployment's switch AND an opt-in; anyone else no.
//! - **The prompt** (`segment-analysis.bpmn`): the media go as `{ref, sha256}` parts, never as
//!   data; participants are pseudonyms.
//! - **The fusion** (`segment-analysis.bpmn`): only the fields asked for are kept (an inferred
//!   age or identity is dropped and listed); a modality claimed but not sent is false; an emotion
//!   no sent modality supports is withheld; a disagreement is kept as reported.
//! - **The digest** (`video-meeting.bpmn`): the dominant emotion and engagement of each analysed
//!   participant, and the segments the signals disagreed in.

use std::path::Path;

use serde_json::{json, Value};
use sutra_executor::variables::{feel_to_json, json_to_feel};

use crate::support::util;

/// The FEEL a flow assigns to `to` (the `<bpmn:from>` of the assignment whose `<bpmn:to>` it is).
fn assignment(bpmn: &Path, to: &str) -> String {
    let text = std::fs::read_to_string(bpmn).unwrap_or_else(|e| panic!("{}: {e}", bpmn.display()));
    let target = format!("<bpmn:to>{to}</bpmn:to>");
    let at = text
        .find(&target)
        .unwrap_or_else(|| panic!("{} assigns no '{to}'", bpmn.display()));
    let before = &text[..at];
    let open = before.rfind("<bpmn:from>").expect("a <bpmn:from>") + "<bpmn:from>".len();
    let close = before.rfind("</bpmn:from>").expect("a </bpmn:from>");
    before[open..close]
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// Evaluate `expression` against `variables` (a JSON object).
fn eval(expression: &str, variables: &Value) -> Value {
    let context = match json_to_feel(variables) {
        sutra_feel::FeelValue::Map(map) => map,
        other => panic!("variables must be an object: {other:?}"),
    };
    let value = sutra_feel::expressions::eval(expression, &context)
        .unwrap_or_else(|e| panic!("FEEL fails: {e}\n{expression}"));
    feel_to_json(&value)
}

fn example(path: &str) -> std::path::PathBuf {
    util::examples_dir().join(path)
}

/// Decide with the policy table of `example_dir`.
fn decide(example_dir: &str, role: &str, allow_staff: bool, opted_in: bool) -> (bool, String) {
    let dmn = std::fs::read(example(&format!(
        "{example_dir}/shared/rules/analysis-policy.dmn"
    )))
    .expect("the policy table");
    let input = json!({"role": role, "allowStaff": allow_staff, "optedIn": opted_in});
    let sutra_feel::FeelValue::Map(context) = json_to_feel(&input) else {
        unreachable!()
    };
    let out = sutra_dmn::DmnDecisionEngine::new()
        .evaluate("analysis-policy", &dmn, &context)
        .expect("the table evaluates");
    (
        feel_to_json(&out["analysed"]) == json!(true),
        feel_to_json(&out["analysisPolicy"])
            .as_str()
            .unwrap()
            .to_string(),
    )
}

#[test]
fn the_policy_analyses_external_parties_and_staff_only_when_allowed_and_opted_in() {
    for example_dir in ["video-meeting-assistant", "meeting-assistant"] {
        for staff in ["staff", "internal", "employee", "agent", "host"] {
            for (allow, opted) in [(false, false), (false, true), (true, false)] {
                assert_eq!(
                    decide(example_dir, staff, allow, opted),
                    (false, "staff-not-opted-in".to_string()),
                    "{example_dir}: {staff} allow={allow} opted={opted}"
                );
            }
            assert_eq!(
                decide(example_dir, staff, true, true),
                (true, "staff-opted-in".to_string())
            );
        }
        assert_eq!(
            decide(example_dir, "external", false, false),
            (true, "analysed".to_string())
        );
        for other in ["unknown", "guest", ""] {
            assert_eq!(
                decide(example_dir, other, true, true),
                (false, "role-not-analysed".to_string()),
                "{example_dir}: {other}"
            );
        }
    }
}

/// A `call-segment` message as the transport delivers it.
fn segment() -> Value {
    let media = |uri: &str, sha: &str, format: &str| {
        json!({"kind": "media", "uri": uri, "sha256": sha, "bytes": 1000, "format": format,
               "participant": "PA_sam", "startMs": 5000, "endMs": 10000, "sealed": true})
    };
    let mut frame = media(
        "s3://b/s/m/c-1/segments/p2/2-f1.jpg.sealed",
        "f1",
        "image/jpeg",
    );
    frame["atMs"] = json!(300);
    frame["requested"] = json!(true);
    json!({
        "callId": "c-1", "key": "c-1:2:2", "participantId": "PA_sam", "participantIndex": 2,
        "participant": "P2", "label": "Sam", "role": "external", "declaredRole": "external",
        "segment": 2, "startMs": 5000, "endMs": 10000, "speaking": true, "talkMs": 3200,
        "audioMs": 5000, "features": {"durationMs": 5000, "voicedRatio": 0.6},
        "flaggedMoments": [], "words": [{"speaker": "P1", "startMs": 0, "endMs": 900, "text": "Hello."}],
        "present": [{"participant": "P1", "role": "staff", "talkMs": 900},
                    {"participant": "P2", "role": "external", "talkMs": 3200}],
        "audio": media("s3://b/s/m/c-1/segments/p2/2.wav.sealed", "a1", "audio/wav"),
        "frames": [frame], "status": "complete"
    })
}

#[test]
fn the_segment_prompt_sends_the_media_as_references_and_names_pseudonyms_only() {
    let bpmn = example("video-meeting-assistant/shared/bpmn/segment-analysis.bpmn");
    let mut vars = json!({"segment": segment()});
    vars["promptContext"] = eval(&assignment(&bpmn, "promptContext"), &vars);
    let context = &vars["promptContext"];
    assert_eq!(context["speaking"], true, "{context}");
    assert_eq!(context["talkMs"], 3200, "{context}");
    assert_eq!(context["frames"], 1, "{context}");
    assert_eq!(
        context["segment"],
        json!({"index": 2, "startMs": 5000, "endMs": 10000})
    );
    let request = eval(&assignment(&bpmn, "requestBody"), &vars);
    assert_eq!(request["json"], true);
    let message = &request["messages"][0];
    assert_eq!(
        message["audio"],
        json!({"ref": "s3://b/s/m/c-1/segments/p2/2.wav.sealed", "sha256": "a1", "format": "audio/wav"})
    );
    assert_eq!(
        message["video"]["frames"],
        json!([{"ref": "s3://b/s/m/c-1/segments/p2/2-f1.jpg.sealed", "sha256": "f1",
                "format": "image/jpeg", "atMs": 300}])
    );
    let content = message["content"].as_str().unwrap();
    assert!(content.starts_with("Analyse participant P2 "), "{content}");
    assert!(
        !content.contains("Sam") && !content.contains("PA_sam"),
        "{content}"
    );
    assert!(
        request["system"]
            .as_str()
            .unwrap()
            .contains("Never identify"),
        "{request}"
    );
    // No frames: no video part at all (the channel refuses an empty frame list).
    vars["segment"]["frames"] = json!([]);
    let request = eval(&assignment(&bpmn, "requestBody"), &vars);
    assert!(request["messages"][0]["video"].is_null(), "{request}");
}

#[test]
fn the_fusion_keeps_what_was_asked_and_sent_and_reports_disagreement() {
    let bpmn = example("video-meeting-assistant/shared/bpmn/segment-analysis.bpmn");
    let answer = |reply: Value| json!({"json": reply, "provider": "gemini", "model": "m"});
    let mut vars = json!({"segment": segment()});
    vars["sent"] = eval(&assignment(&bpmn, "sent"), &vars);
    assert_eq!(
        vars["sent"],
        json!({"voice": true, "face": true, "body": true, "words": true})
    );

    vars["answer"] = answer(json!({
        "expression": "tense", "engagement": "high", "attention": "on P1",
        "bodyLanguage": ["arms crossed"],
        "emotion": {"label": "uneasy", "confidence": 0.7,
                    "modalities": {"voice": false, "face": true, "body": true, "words": false}},
        "disagreement": {"flagged": true, "detail": "a calm voice, a tense posture"},
        "reactions": [{"atMs": 300, "to": "P1", "cue": "frowns", "gender": "x"}],
        "age": 41, "identity": "someone"
    }));
    let fused = eval(&assignment(&bpmn, "fused"), &vars);
    let result = &fused["result"];
    assert_eq!(result["emotion"]["label"], "uneasy");
    assert_eq!(
        result["emotion"]["modalities"],
        json!({"voice": false, "face": true, "body": true, "words": false})
    );
    // The disagreement is kept as the model reported it, never averaged away.
    assert_eq!(
        result["disagreement"],
        json!({"flagged": true, "detail": "a calm voice, a tense posture"})
    );
    // Only the fields asked for: an inferred age or identity is dropped and listed.
    assert!(result.get("age").is_none() && result.get("identity").is_none());
    assert_eq!(fused["removedKeys"], json!(["age", "identity"]));
    assert_eq!(
        result["reactions"],
        json!([{"atMs": 300, "to": "P1", "cue": "frowns"}])
    );

    // A modality the model claims but that was not sent is false, not guessed; an emotion that
    // no sent modality supports is withheld.
    vars["segment"]["frames"] = json!([]);
    vars["segment"]["words"] = json!([]);
    vars["sent"] = eval(&assignment(&bpmn, "sent"), &vars);
    vars["answer"] = answer(json!({
        "expression": "not visible", "engagement": "low", "attention": "unknown",
        "emotion": {"label": "bored", "confidence": 0.9,
                    "modalities": {"voice": false, "face": true, "body": true, "words": true}},
        "disagreement": {"flagged": false, "detail": null}
    }));
    let fused = eval(&assignment(&bpmn, "fused"), &vars);
    assert_eq!(fused["unsupportedClaims"], json!(["face", "body", "words"]));
    assert!(fused["result"]["emotion"].is_null(), "{fused}");
    assert_eq!(fused["result"]["bodyLanguage"], json!([]));

    // The row: every field of the segment, its references, and the fused result.
    vars["segment"] = segment();
    vars["sent"] = eval(&assignment(&bpmn, "sent"), &vars);
    vars["fused"] = eval(&assignment(&bpmn, "fused"), &vars);
    vars["analysisPolicy"] = json!("analysed");
    let mut with_row = vars.clone();
    with_row["row"] = Value::Null;
    let row = eval(&assignment(&bpmn, "row"), &with_row);
    assert_eq!(row["segment"], 2, "{row}");
    assert_eq!(row["startMs"], 5000, "{row}");
    assert_eq!(row["talkMs"], 3200, "{row}");
    assert_eq!(row["audioSent"], true, "{row}");
    assert_eq!(row["frames"], 1, "{row}");
    assert_eq!(row["framesRequested"], 1, "{row}");
    assert_eq!(row["media"]["audio"]["sha256"], "a1", "{row}");
    assert_eq!(row["status"], "complete", "{row}");

    // An answer that is not JSON: no result, so the row reads as failed.
    vars["answer"] = json!({"json": null, "text": "sorry", "provider": "gemini"});
    let fused = eval(&assignment(&bpmn, "fused"), &vars);
    assert!(fused["result"].is_null());
}

#[test]
fn the_session_digest_names_each_participants_dominant_signals() {
    let bpmn = example("video-meeting-assistant/shared/bpmn/video-meeting.bpmn");
    let row = |index: u64, segment: u64, emotion: &str, engagement: &str, disagrees: bool| {
        json!({"participantIndex": index, "segment": segment, "status": "complete",
               "provider": "gemini", "model": "m",
               "result": {"emotion": {"label": emotion}, "engagement": engagement,
                          "disagreement": {"flagged": disagrees}}})
    };
    let participant = |index: u64, label: &str, role: &str, segments: u64| {
        json!({"participantId": format!("PA_{index}"), "participantIndex": index, "label": label,
               "role": role, "segmented": segments > 0, "segments": segments, "talkMs": 4000,
               "transcriptLines": 2, "video": {"regular": 16},
               "recording": {"uri": format!("s3://b/p{index}.wav.sealed"), "sha256": format!("h{index}")}})
    };
    let vars = json!({
        "callId": "c-1",
        "ended": {"participants": [participant(1, "Alex", "staff", 0), participant(2, "Sam", "external", 3)],
                  "segments": {"segmentMs": 5000, "delivered": 3}},
        "roster": [
            {"event": "joined", "participantIndex": 1, "analysed": false, "analysisPolicy": "staff-not-opted-in"},
            {"event": "joined", "participantIndex": 2, "analysed": true, "analysisPolicy": "analysed"},
        ],
    });
    let keys = eval(&assignment(&bpmn, "analysisKeys"), &vars);
    assert_eq!(keys, json!(["c-1:2:1", "c-1:2:2", "c-1:2:3"]));

    let mut vars = vars;
    vars["analysisKeys"] = keys;
    vars["rows"] = json!([
        row(2, 1, "interested", "high", false),
        row(2, 2, "uneasy", "high", true),
        row(2, 3, "interested", "medium", false),
    ]);
    vars["digests"] = eval(&assignment(&bpmn, "digests"), &vars);
    let participants = eval(&assignment(&bpmn, "participants"), &vars);
    let (alex, sam) = (&participants[0], &participants[1]);
    assert_eq!(alex["analysed"], false);
    assert_eq!(alex["analysisPolicy"], "staff-not-opted-in");
    assert_eq!(alex["segments"], 0);
    assert!(alex["emotion"].is_null());
    assert_eq!(sam["analysed"], true);
    assert_eq!(sam["segments"], 3);
    assert_eq!(sam["emotion"], "interested");
    assert_eq!(sam["engagement"], "high");
    assert_eq!(sam["disagreements"], 1);
    assert_eq!(sam["recordingSha256"], "h2");
    assert!(
        sam.get("mine").is_none(),
        "the helpers stay out of the record"
    );
    vars["participants"] = participants;
    let analysis = eval(&assignment(&bpmn, "analysis"), &vars);
    assert_eq!(analysis["results"], 3);
    assert_eq!(analysis["missing"], 0);
    assert_eq!(analysis["provider"], "gemini");
}
