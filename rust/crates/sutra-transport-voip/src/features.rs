//! Acoustic features, computed in the transport with no model: deterministic and cheap.
//!
//! Per turn ([`analyze_turn`]): duration, voiced time and voiced-frame ratio (a speech-rate
//! proxy), an RMS energy contour in dBFS and an autocorrelation pitch contour in Hz, each
//! reduced to a [`ContourSummary`] (mean, min, max, standard deviation, slope per second).
//! Per call ([`CallAnalytics`]): talk time per party, silence and dead air, overtalk and
//! interruptions, fed one analysis frame at a time from the caller's audio clock.
//!
//! The values ride the inbound messages as JSON — a `call-turn` carries its
//! [`TurnFeatures`] in the `x-turn-features` header, a `call-hangup` carries the call's
//! [`CallFeatures`] in its body — so a flow can store them next to the transcript segment.

use serde::Serialize;

/// Root-mean-square amplitude of a frame (0..32767).
pub fn frame_rms(frame: &[i16]) -> f64 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    (sum / frame.len() as f64).sqrt()
}

/// RMS expressed in dBFS (full scale = 0 dB), floored at -100.
pub fn rms_dbfs(rms: f64) -> f64 {
    if rms <= 0.0 {
        return -100.0;
    }
    (20.0 * (rms / 32768.0).log10()).max(-100.0)
}

/// Pitch-estimation settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PitchConfig {
    pub min_hz: f64,
    pub max_hz: f64,
    /// Minimum normalised autocorrelation for a window to count as periodic.
    pub voicing_threshold: f64,
}

impl Default for PitchConfig {
    fn default() -> Self {
        PitchConfig {
            min_hz: 60.0,
            max_hz: 400.0,
            voicing_threshold: 0.5,
        }
    }
}

/// Estimate the fundamental frequency of `window` by normalised autocorrelation. Picks the
/// first local maximum within 90 % of the strongest peak (which avoids octave-down errors on
/// clean periodic input) and refines it by parabolic interpolation. `None` when the window is
/// too short, silent or not periodic enough.
pub fn estimate_pitch(window: &[i16], rate: u32, config: &PitchConfig) -> Option<f64> {
    let rate = f64::from(rate);
    let min_lag = (rate / config.max_hz).floor().max(2.0) as usize;
    let max_lag = (rate / config.min_hz).ceil() as usize;
    if window.len() < max_lag + 2 || min_lag >= max_lag {
        return None;
    }
    let x: Vec<f64> = window.iter().map(|s| f64::from(*s)).collect();
    let mean = x.iter().sum::<f64>() / x.len() as f64;
    let x: Vec<f64> = x.iter().map(|v| v - mean).collect();
    let correlation = |lag: usize| -> f64 {
        let n = x.len() - lag;
        let mut cross = 0.0;
        let mut energy_a = 0.0;
        let mut energy_b = 0.0;
        for i in 0..n {
            cross += x[i] * x[i + lag];
            energy_a += x[i] * x[i];
            energy_b += x[i + lag] * x[i + lag];
        }
        let denominator = (energy_a * energy_b).sqrt();
        if denominator <= f64::EPSILON {
            0.0
        } else {
            cross / denominator
        }
    };
    let lo = min_lag - 1;
    let hi = (max_lag + 1).min(x.len() - 1);
    let r: Vec<f64> = (lo..=hi).map(correlation).collect();
    let at = |lag: usize| r[lag - lo];
    let peak = (min_lag..=max_lag.min(hi - 1))
        .map(at)
        .fold(f64::NEG_INFINITY, f64::max);
    if peak < config.voicing_threshold {
        return None;
    }
    let lag = (min_lag..=max_lag.min(hi - 1))
        .find(|&lag| at(lag) >= at(lag - 1) && at(lag) >= at(lag + 1) && at(lag) >= 0.9 * peak)?;
    let (a, b, c) = (at(lag - 1), at(lag), at(lag + 1));
    let curvature = a - 2.0 * b + c;
    let delta = if curvature.abs() > f64::EPSILON {
        (0.5 * (a - c) / curvature).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    Some(rate / (lag as f64 + delta))
}

/// A contour reduced to summary statistics. `slope_per_s` is the least-squares slope over
/// time (units per second): positive = rising.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContourSummary {
    pub count: usize,
    pub mean: f64,
    pub min: f64,
    pub max: f64,
    pub stddev: f64,
    pub slope_per_s: f64,
}

impl ContourSummary {
    /// Summarise `(time_s, value)` points; `None` when empty.
    pub fn of(points: &[(f64, f64)]) -> Option<ContourSummary> {
        if points.is_empty() {
            return None;
        }
        let n = points.len() as f64;
        let mean = points.iter().map(|p| p.1).sum::<f64>() / n;
        let variance = points.iter().map(|p| (p.1 - mean).powi(2)).sum::<f64>() / n;
        let min = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let max = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        let mean_t = points.iter().map(|p| p.0).sum::<f64>() / n;
        let sxx: f64 = points.iter().map(|p| (p.0 - mean_t).powi(2)).sum();
        let sxy: f64 = points.iter().map(|p| (p.0 - mean_t) * (p.1 - mean)).sum();
        let slope = if sxx > f64::EPSILON { sxy / sxx } else { 0.0 };
        Some(ContourSummary {
            count: points.len(),
            mean: round3(mean),
            min: round3(min),
            max: round3(max),
            stddev: round3(variance.sqrt()),
            slope_per_s: round3(slope),
        })
    }
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// One turn's acoustic features.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnFeatures {
    pub duration_ms: u64,
    pub voiced_ms: u64,
    /// Voiced frames / all frames — a speech-rate proxy (dense speech scores high, halting
    /// speech with pauses scores low).
    pub voiced_ratio: f64,
    /// Per-frame RMS energy in dBFS.
    pub energy_dbfs: Option<ContourSummary>,
    /// Per-frame pitch in Hz over the periodic frames.
    pub pitch_hz: Option<ContourSummary>,
    /// Milliseconds of this turn spoken while Sutra audio was playing.
    pub overtalk_ms: u64,
    /// Whether this turn barged in on playback.
    pub interrupted_playback: bool,
}

/// Analyse one turn. Frames of `frame_ms` are classified voiced at `energy_threshold`
/// (linear RMS); pitch is estimated over a two-frame window at each voiced frame.
pub fn analyze_turn(
    samples: &[i16],
    rate: u32,
    frame_ms: u32,
    energy_threshold: f64,
    pitch: &PitchConfig,
) -> TurnFeatures {
    let spf = ((rate as usize) * frame_ms.max(1) as usize / 1000).max(1);
    let frames = samples.len() / spf;
    let mut voiced_frames = 0usize;
    let mut energy = Vec::with_capacity(frames);
    let mut pitch_points = Vec::new();
    for index in 0..frames {
        let frame = &samples[index * spf..(index + 1) * spf];
        let rms = frame_rms(frame);
        let t = (index * frame_ms as usize) as f64 / 1000.0;
        energy.push((t, rms_dbfs(rms)));
        if rms >= energy_threshold {
            voiced_frames += 1;
            let end = ((index + 2) * spf).min(samples.len());
            let start = end.saturating_sub(2 * spf);
            if let Some(hz) = estimate_pitch(&samples[start..end], rate, pitch) {
                pitch_points.push((t, hz));
            }
        }
    }
    TurnFeatures {
        duration_ms: (samples.len() as u64 * 1000) / u64::from(rate.max(1)),
        voiced_ms: (voiced_frames * frame_ms as usize) as u64,
        voiced_ratio: if frames == 0 {
            0.0
        } else {
            round3(voiced_frames as f64 / frames as f64)
        },
        energy_dbfs: ContourSummary::of(&energy),
        pitch_hz: ContourSummary::of(&pitch_points),
        overtalk_ms: 0,
        interrupted_playback: false,
    }
}

/// The whole call's interaction features.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallFeatures {
    /// Milliseconds of caller audio observed.
    pub observed_ms: u64,
    pub caller_talk_ms: u64,
    /// Milliseconds Sutra audio was playing.
    pub agent_talk_ms: u64,
    /// caller / (caller + agent); 0 when nobody spoke.
    pub caller_talk_ratio: f64,
    /// Neither party audible.
    pub silence_ms: u64,
    /// Silence in runs of at least the dead-air threshold.
    pub dead_air_ms: u64,
    pub dead_air_events: u64,
    pub longest_silence_ms: u64,
    /// Both parties at once.
    pub overtalk_ms: u64,
    /// Barge-ins (caller speech that stopped playback).
    pub interruptions: u64,
    pub turns: u64,
}

/// Running per-call accounting, advanced one analysis frame at a time.
#[derive(Debug, Clone)]
pub struct CallAnalytics {
    frame_ms: u64,
    dead_air_threshold_ms: u64,
    observed_ms: u64,
    caller_talk_ms: u64,
    agent_talk_ms: u64,
    overtalk_ms: u64,
    silence_ms: u64,
    dead_air_ms: u64,
    dead_air_events: u64,
    longest_silence_ms: u64,
    silence_run_ms: u64,
    interruptions: u64,
    turns: u64,
}

/// Default dead-air threshold: a silence of two seconds or more.
pub const DEFAULT_DEAD_AIR_MS: u64 = 2000;

impl CallAnalytics {
    pub fn new(frame_ms: u32, dead_air_threshold_ms: u64) -> CallAnalytics {
        CallAnalytics {
            frame_ms: u64::from(frame_ms.max(1)),
            dead_air_threshold_ms,
            observed_ms: 0,
            caller_talk_ms: 0,
            agent_talk_ms: 0,
            overtalk_ms: 0,
            silence_ms: 0,
            dead_air_ms: 0,
            dead_air_events: 0,
            longest_silence_ms: 0,
            silence_run_ms: 0,
            interruptions: 0,
            turns: 0,
        }
    }

    /// Account one frame of the caller's clock.
    pub fn on_frame(&mut self, caller_voiced: bool, agent_playing: bool) {
        let ms = self.frame_ms;
        self.observed_ms += ms;
        if caller_voiced {
            self.caller_talk_ms += ms;
        }
        if agent_playing {
            self.agent_talk_ms += ms;
        }
        if caller_voiced && agent_playing {
            self.overtalk_ms += ms;
        }
        if !caller_voiced && !agent_playing {
            self.silence_ms += ms;
            self.silence_run_ms += ms;
            self.longest_silence_ms = self.longest_silence_ms.max(self.silence_run_ms);
        } else {
            self.close_silence_run();
        }
    }

    pub fn on_interruption(&mut self) {
        self.interruptions += 1;
    }

    pub fn on_turn(&mut self) {
        self.turns += 1;
    }

    fn close_silence_run(&mut self) {
        if self.silence_run_ms >= self.dead_air_threshold_ms && self.silence_run_ms > 0 {
            self.dead_air_ms += self.silence_run_ms;
            self.dead_air_events += 1;
        }
        self.silence_run_ms = 0;
    }

    /// The features so far (a silence run still open counts as if it ended now).
    pub fn summary(&self) -> CallFeatures {
        let mut finished = self.clone();
        finished.close_silence_run();
        let talk = finished.caller_talk_ms + finished.agent_talk_ms;
        CallFeatures {
            observed_ms: finished.observed_ms,
            caller_talk_ms: finished.caller_talk_ms,
            agent_talk_ms: finished.agent_talk_ms,
            caller_talk_ratio: if talk == 0 {
                0.0
            } else {
                round3(finished.caller_talk_ms as f64 / talk as f64)
            },
            silence_ms: finished.silence_ms,
            dead_air_ms: finished.dead_air_ms,
            dead_air_events: finished.dead_air_events,
            longest_silence_ms: finished.longest_silence_ms,
            overtalk_ms: finished.overtalk_ms,
            interruptions: finished.interruptions,
            turns: finished.turns,
        }
    }
}
