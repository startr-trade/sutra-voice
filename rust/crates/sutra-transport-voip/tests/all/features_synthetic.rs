use sutra_transport_voip::features::*;

use super::support::{silence, tone};

#[test]
fn pitch_of_pure_tones_is_found_within_two_percent() {
    let config = PitchConfig::default();
    for (hz, rate) in [(100.0, 8000), (200.0, 8000), (310.0, 16000), (120.0, 16000)] {
        let window = tone(hz, 40, rate, 8000.0);
        let estimate = estimate_pitch(&window, rate, &config).unwrap();
        assert!(
            (estimate - hz).abs() / hz < 0.02,
            "{hz} Hz at {rate}: {estimate}"
        );
    }
    assert_eq!(estimate_pitch(&silence(40, 8000), 8000, &config), None);
    // Deterministic pseudo-noise is not periodic.
    let mut state = 12345u32;
    let noise: Vec<i16> = (0..320)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
            ((state >> 16) as i16) / 4
        })
        .collect();
    assert_eq!(estimate_pitch(&noise, 8000, &config), None);
}

#[test]
fn turn_features_of_tone_half_silence() {
    let mut samples = tone(200.0, 500, 8000, 8000.0);
    samples.extend(silence(500, 8000));
    let features = analyze_turn(&samples, 8000, 20, 500.0, &PitchConfig::default());
    assert_eq!(features.duration_ms, 1000);
    assert_eq!(features.voiced_ms, 500);
    assert!((features.voiced_ratio - 0.5).abs() < 1e-9);
    let pitch = features.pitch_hz.unwrap();
    assert!((pitch.mean - 200.0).abs() < 4.0, "{pitch:?}");
    assert!(pitch.stddev < 3.0);
    let energy = features.energy_dbfs.unwrap();
    // A 8000-amplitude sine has RMS 5657 → about -15.2 dBFS; silence floors at -100.
    assert!((energy.max - -15.24).abs() < 0.2, "{energy:?}");
    assert_eq!(energy.min, -100.0);
    assert!(energy.slope_per_s < 0.0, "energy falls into silence");
    // Same input, same output.
    assert_eq!(
        features,
        analyze_turn(&samples, 8000, 20, 500.0, &PitchConfig::default())
    );
}

#[test]
fn rising_pitch_has_a_positive_slope() {
    let mut samples = Vec::new();
    for step in 0..10 {
        samples.extend(tone(120.0 + 15.0 * step as f64, 100, 8000, 8000.0));
    }
    let features = analyze_turn(&samples, 8000, 20, 500.0, &PitchConfig::default());
    let pitch = features.pitch_hz.unwrap();
    assert!(pitch.slope_per_s > 100.0, "{pitch:?}");
    assert!(pitch.min < 140.0 && pitch.max > 240.0, "{pitch:?}");
}

#[test]
fn call_analytics_accounts_talk_silence_dead_air_and_overtalk() {
    let mut analytics = CallAnalytics::new(20, 2000);
    // 1 s caller, 3 s silence (dead air), 1 s agent, 0.5 s both, 1 s silence (not dead air).
    for _ in 0..50 {
        analytics.on_frame(true, false);
    }
    for _ in 0..150 {
        analytics.on_frame(false, false);
    }
    for _ in 0..50 {
        analytics.on_frame(false, true);
    }
    for _ in 0..25 {
        analytics.on_frame(true, true);
    }
    analytics.on_interruption();
    analytics.on_turn();
    for _ in 0..50 {
        analytics.on_frame(false, false);
    }
    let summary = analytics.summary();
    assert_eq!(summary.observed_ms, 6500);
    assert_eq!(summary.caller_talk_ms, 1500);
    assert_eq!(summary.agent_talk_ms, 1500);
    assert_eq!(summary.overtalk_ms, 500);
    assert_eq!(summary.silence_ms, 4000);
    assert_eq!(summary.dead_air_ms, 3000);
    assert_eq!(summary.dead_air_events, 1);
    assert_eq!(summary.longest_silence_ms, 3000);
    assert_eq!(summary.interruptions, 1);
    assert_eq!(summary.turns, 1);
    assert!((summary.caller_talk_ratio - 0.5).abs() < 1e-9);
    let json = serde_json::to_value(&summary).unwrap();
    assert_eq!(json["deadAirMs"], 3000);
}
