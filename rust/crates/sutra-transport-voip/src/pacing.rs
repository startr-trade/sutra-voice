//! Real-time paced playback.
//!
//! Audio to a call is never pre-buffered into the gateway: it is cut into fixed frames
//! (20 ms) and sent one frame per tick of a 20 ms timer. With nothing queued downstream,
//! barge-in is simply ceasing to send — which matters on a bridge that offers no flush, and is
//! required outright where the gateway forwards frames without pacing them itself.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};

/// Cut samples into `frame_ms` frames at `rate`; the last frame is zero-padded.
pub fn frames_of(samples: &[i16], rate: u32, frame_ms: u32) -> Vec<Vec<i16>> {
    let spf = ((rate as usize) * frame_ms.max(1) as usize / 1000).max(1);
    samples
        .chunks(spf)
        .map(|chunk| {
            let mut frame = chunk.to_vec();
            frame.resize(spf, 0);
            frame
        })
        .collect()
}

/// How a paced playback ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackOutcome {
    pub frames_sent: usize,
    pub frames_total: usize,
    /// Stopped early (barge-in, hang-up or a newer play cancelling this one).
    pub interrupted: bool,
}

impl PlaybackOutcome {
    pub fn played_ms(&self, frame_ms: u32) -> u64 {
        (self.frames_sent as u64) * u64::from(frame_ms)
    }
}

/// Send `frames` (already encoded) to `out`, one per `frame_ms` tick, until done, until
/// `cancelled()` answers true (checked before every frame) or until `out` closes. The first
/// frame goes immediately.
pub async fn pace<T, F>(
    frames: Vec<T>,
    frame_ms: u32,
    out: &mpsc::Sender<T>,
    mut cancelled: F,
) -> PlaybackOutcome
where
    F: FnMut() -> bool,
{
    let frames_total = frames.len();
    let mut ticker = interval(Duration::from_millis(u64::from(frame_ms.max(1))));
    // A late tick is caught up in a short burst (keeps the average real-time) rather than
    // skipped (which would drop audio).
    ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);
    let mut frames_sent = 0;
    for frame in frames {
        ticker.tick().await;
        if cancelled() {
            return PlaybackOutcome {
                frames_sent,
                frames_total,
                interrupted: true,
            };
        }
        if out.send(frame).await.is_err() {
            return PlaybackOutcome {
                frames_sent,
                frames_total,
                interrupted: true,
            };
        }
        frames_sent += 1;
    }
    PlaybackOutcome {
        frames_sent,
        frames_total,
        interrupted: false,
    }
}
