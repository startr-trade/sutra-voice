use sutra_transport_voip::vad::*;

use super::support::{silence, tone};

fn speech(ms: u32) -> Vec<i16> {
    tone(180.0, ms, 8000, 8000.0)
}

#[test]
fn defaults_are_700_ms_silence_and_30_s_turns() {
    let config = VadConfig::default();
    assert_eq!(config.silence_ms, 700);
    assert_eq!(config.max_turn_ms, 30_000);
    assert_eq!(config.frame_ms, 20);
}

#[test]
fn speech_then_silence_makes_one_turn_with_trailing_silence_trimmed() {
    let mut detector = TurnDetector::new(VadConfig::default(), 8000);
    let mut audio = silence(300, 8000);
    audio.extend(speech(1000));
    audio.extend(silence(800, 8000));
    // Feed in odd-sized chunks, as a gateway would.
    let mut events = Vec::new();
    for chunk in audio.chunks(137) {
        events.extend(detector.push_collect(chunk));
    }
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], VadEvent::SpeechStarted);
    let VadEvent::TurnEnded { samples, reason } = &events[1] else {
        panic!("no turn")
    };
    assert_eq!(*reason, TurnEndReason::Silence);
    // 1000 ms of speech (no leading silence, trailing 700 ms trimmed).
    assert_eq!(samples.len(), 8000);
    assert!(!detector.in_turn());
}

#[test]
fn short_pauses_stay_in_one_turn_and_long_pauses_split() {
    let mut detector = TurnDetector::new(VadConfig::default(), 8000);
    let mut audio = speech(400);
    audio.extend(silence(400, 8000));
    audio.extend(speech(400));
    audio.extend(silence(900, 8000));
    audio.extend(speech(400));
    audio.extend(silence(900, 8000));
    let turns = detector
        .push_collect(&audio)
        .into_iter()
        .filter(|e| matches!(e, VadEvent::TurnEnded { .. }))
        .count();
    assert_eq!(turns, 2);
}

#[test]
fn max_turn_length_forces_a_turn_end() {
    let config = VadConfig {
        max_turn_ms: 1000,
        ..VadConfig::default()
    };
    let mut detector = TurnDetector::new(config, 8000);
    let events = detector.push_collect(&speech(2500));
    let reasons: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            VadEvent::TurnEnded { reason, samples } => Some((*reason, samples.len())),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasons,
        vec![
            (TurnEndReason::MaxLength, 8000),
            (TurnEndReason::MaxLength, 8000)
        ]
    );
    // The rest is still a turn in progress; flushing ends it.
    let VadEvent::TurnEnded { reason, .. } = detector.flush().unwrap() else {
        panic!()
    };
    assert_eq!(reason, TurnEndReason::Flush);
}

#[test]
fn clicks_shorter_than_min_speech_do_not_start_a_turn() {
    let mut detector = TurnDetector::new(VadConfig::default(), 8000);
    let mut audio = speech(40); // below the 60 ms minimum
    audio.extend(silence(500, 8000));
    assert!(detector.push_collect(&audio).is_empty());
    assert!(detector.flush().is_none());
}

#[test]
fn barge_in_trigger_is_reported_on_the_frame_speech_starts() {
    // The transport stops playback on SpeechStarted; it must fire within min-speech-ms.
    let mut detector = TurnDetector::new(VadConfig::default(), 8000);
    let mut frames_until_start = None;
    let mut frame = 0;
    detector.push(&speech(200), |info, event| {
        frame += 1;
        assert!(info.voiced);
        if event == Some(VadEvent::SpeechStarted) && frames_until_start.is_none() {
            frames_until_start = Some(frame);
        }
    });
    assert_eq!(frames_until_start, Some(3)); // 60 ms at 20 ms frames
}
