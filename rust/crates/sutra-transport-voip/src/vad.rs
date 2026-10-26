//! Energy voice-activity detection and turn segmentation.
//!
//! Audio arrives in whatever chunk size the gateway sends; [`TurnDetector`] re-frames it into
//! fixed analysis frames (20 ms by default), classifies each frame voiced/unvoiced by RMS
//! energy, and segments speech into turns: a turn starts after `min-speech-ms` of continuous
//! voiced audio (the frames that triggered it are kept as pre-roll) and ends after
//! `silence-ms` of unvoiced audio, or forcibly at `max-turn-ms`.

use crate::features::frame_rms;

/// Turn-detection settings. Defaults: 20 ms frames, 700 ms end-of-turn silence, 30 s
/// maximum turn, 60 ms minimum speech, RMS threshold 500 (about -36 dBFS).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadConfig {
    pub frame_ms: u32,
    pub silence_ms: u32,
    pub max_turn_ms: u32,
    pub min_speech_ms: u32,
    /// Linear RMS (0..32767) at or above which a frame counts as voiced.
    pub energy_threshold: f64,
}

impl Default for VadConfig {
    fn default() -> Self {
        VadConfig {
            frame_ms: 20,
            silence_ms: 700,
            max_turn_ms: 30_000,
            min_speech_ms: 60,
            energy_threshold: 500.0,
        }
    }
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEndReason {
    /// `silence-ms` of unvoiced audio followed the speech.
    Silence,
    /// The turn reached `max-turn-ms`.
    MaxLength,
    /// The stream ended mid-turn.
    Flush,
}

impl TurnEndReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnEndReason::Silence => "silence",
            TurnEndReason::MaxLength => "max-length",
            TurnEndReason::Flush => "flush",
        }
    }
}

/// What one analysis frame produced.
#[derive(Debug, Clone, PartialEq)]
pub enum VadEvent {
    /// Speech began (the barge-in trigger while playback is running).
    SpeechStarted,
    /// A turn completed; `samples` is its audio with the trailing silence trimmed.
    TurnEnded {
        samples: Vec<i16>,
        reason: TurnEndReason,
    },
}

/// One classified analysis frame, reported alongside any event it produced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameInfo {
    pub voiced: bool,
    /// Whether the detector is inside a turn after this frame.
    pub in_turn: bool,
}

/// The streaming turn segmenter.
#[derive(Debug, Clone)]
pub struct TurnDetector {
    config: VadConfig,
    samples_per_frame: usize,
    partial: Vec<i16>,
    preroll: Vec<i16>,
    preroll_ms: u32,
    in_turn: bool,
    turn: Vec<i16>,
    turn_ms: u32,
    silence_run_ms: u32,
}

impl TurnDetector {
    pub fn new(config: VadConfig, rate: u32) -> TurnDetector {
        let frame_ms = config.frame_ms.max(1);
        let samples_per_frame = ((rate as usize) * frame_ms as usize / 1000).max(1);
        TurnDetector {
            config: VadConfig { frame_ms, ..config },
            samples_per_frame,
            partial: Vec::new(),
            preroll: Vec::new(),
            preroll_ms: 0,
            in_turn: false,
            turn: Vec::new(),
            turn_ms: 0,
            silence_run_ms: 0,
        }
    }

    pub fn config(&self) -> &VadConfig {
        &self.config
    }

    pub fn samples_per_frame(&self) -> usize {
        self.samples_per_frame
    }

    pub fn in_turn(&self) -> bool {
        self.in_turn
    }

    /// Feed audio of any length. `on_frame` is called once per completed analysis frame with
    /// its classification and the event it produced, in order.
    pub fn push(&mut self, samples: &[i16], mut on_frame: impl FnMut(FrameInfo, Option<VadEvent>)) {
        self.partial.extend_from_slice(samples);
        let spf = self.samples_per_frame;
        let mut offset = 0;
        while self.partial.len() - offset >= spf {
            let frame: Vec<i16> = self.partial[offset..offset + spf].to_vec();
            offset += spf;
            let (info, event) = self.process_frame(&frame);
            on_frame(info, event);
        }
        self.partial.drain(..offset);
    }

    /// Convenience over [`Self::push`] collecting only the events.
    pub fn push_collect(&mut self, samples: &[i16]) -> Vec<VadEvent> {
        let mut events = Vec::new();
        self.push(samples, |_, event| events.extend(event));
        events
    }

    /// End the stream: a turn in progress is emitted with [`TurnEndReason::Flush`].
    pub fn flush(&mut self) -> Option<VadEvent> {
        self.partial.clear();
        if !self.in_turn {
            return None;
        }
        Some(self.finish(TurnEndReason::Flush))
    }

    fn process_frame(&mut self, frame: &[i16]) -> (FrameInfo, Option<VadEvent>) {
        let voiced = frame_rms(frame) >= self.config.energy_threshold;
        let frame_ms = self.config.frame_ms;
        let mut event = None;
        if !self.in_turn {
            if voiced {
                self.preroll.extend_from_slice(frame);
                self.preroll_ms += frame_ms;
                if self.preroll_ms >= self.config.min_speech_ms.max(frame_ms) {
                    self.in_turn = true;
                    self.turn = std::mem::take(&mut self.preroll);
                    self.turn_ms = self.preroll_ms;
                    self.preroll_ms = 0;
                    self.silence_run_ms = 0;
                    event = Some(VadEvent::SpeechStarted);
                }
            } else {
                self.preroll.clear();
                self.preroll_ms = 0;
            }
        } else {
            self.turn.extend_from_slice(frame);
            self.turn_ms += frame_ms;
            if voiced {
                self.silence_run_ms = 0;
            } else {
                self.silence_run_ms += frame_ms;
            }
            if self.silence_run_ms >= self.config.silence_ms {
                event = Some(self.finish(TurnEndReason::Silence));
            } else if self.turn_ms >= self.config.max_turn_ms {
                event = Some(self.finish(TurnEndReason::MaxLength));
            }
        }
        (
            FrameInfo {
                voiced,
                in_turn: self.in_turn,
            },
            event,
        )
    }

    fn finish(&mut self, reason: TurnEndReason) -> VadEvent {
        let mut samples = std::mem::take(&mut self.turn);
        if reason == TurnEndReason::Silence {
            let trailing =
                (self.silence_run_ms / self.config.frame_ms) as usize * self.samples_per_frame;
            samples.truncate(samples.len().saturating_sub(trailing));
        }
        self.in_turn = false;
        self.turn_ms = 0;
        self.silence_run_ms = 0;
        VadEvent::TurnEnded { samples, reason }
    }
}
