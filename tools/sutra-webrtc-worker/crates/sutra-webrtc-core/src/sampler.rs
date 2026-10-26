//! Per-participant video sampling: frames at the policy's rate, plus extra frames the engine
//! requests at moments its audio analysis flags — rate-limited, and outside the fps budget.
//! Time is milliseconds on any monotonic clock, so the logic is testable without one.

use std::collections::VecDeque;

use crate::protocol::VideoPolicy;

/// The most extra frames sent per participant in any one second.
pub const MAX_REQUESTS_PER_SEC: usize = 4;

/// Why a frame is sent now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    /// The policy's regular rate.
    Regular,
    /// The engine asked for it (sent with the `requested` flag).
    Requested,
}

/// One participant's sampler.
#[derive(Debug, Clone, Default)]
pub struct FrameSampler {
    interval_ms: Option<u64>,
    next_due_ms: u64,
    requested: bool,
    recent_requests: VecDeque<u64>,
}

impl FrameSampler {
    /// A sampler that sends nothing until a policy enables video.
    pub fn new() -> FrameSampler {
        FrameSampler::default()
    }

    /// Apply `policy` from `now_ms`: the first regular frame is due at once.
    pub fn set_policy(&mut self, policy: &VideoPolicy, now_ms: u64) {
        self.interval_ms = (policy.enabled && policy.fps > 0.0)
            .then(|| ((1000.0 / policy.fps).round() as u64).max(1));
        self.next_due_ms = now_ms;
        if self.interval_ms.is_none() {
            self.requested = false;
        }
    }

    pub fn enabled(&self) -> bool {
        self.interval_ms.is_some()
    }

    /// The engine asked for a frame now. Answers whether the request is accepted (video is
    /// enabled and fewer than [`MAX_REQUESTS_PER_SEC`] were accepted in the last second).
    pub fn request(&mut self, now_ms: u64) -> bool {
        if !self.enabled() {
            return false;
        }
        while self
            .recent_requests
            .front()
            .is_some_and(|at| now_ms.saturating_sub(*at) >= 1000)
        {
            self.recent_requests.pop_front();
        }
        if self.recent_requests.len() >= MAX_REQUESTS_PER_SEC {
            return false;
        }
        self.recent_requests.push_back(now_ms);
        self.requested = true;
        true
    }

    /// Whether a frame is to be sent at `now_ms`, and why. A requested frame does not move the
    /// regular schedule.
    pub fn poll(&mut self, now_ms: u64) -> Option<SampleKind> {
        if std::mem::take(&mut self.requested) {
            return Some(SampleKind::Requested);
        }
        let interval = self.interval_ms?;
        if now_ms < self.next_due_ms {
            return None;
        }
        self.next_due_ms += interval;
        if self.next_due_ms <= now_ms {
            // Fell behind (no frame was available for a while): no burst to catch up.
            self.next_due_ms = now_ms + interval;
        }
        Some(SampleKind::Regular)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(fps: f64) -> VideoPolicy {
        VideoPolicy {
            enabled: true,
            fps,
            ..VideoPolicy::default()
        }
    }

    /// Poll every `step` ms over `[from, to)`; count what is sent.
    fn run(sampler: &mut FrameSampler, from: u64, to: u64, step: u64) -> (usize, usize) {
        let (mut regular, mut requested) = (0, 0);
        let mut t = from;
        while t < to {
            match sampler.poll(t) {
                Some(SampleKind::Regular) => regular += 1,
                Some(SampleKind::Requested) => requested += 1,
                None => {}
            }
            t += step;
        }
        (regular, requested)
    }

    #[test]
    fn nothing_is_sampled_before_a_policy() {
        let mut sampler = FrameSampler::new();
        assert_eq!(run(&mut sampler, 0, 5000, 50), (0, 0));
        assert!(!sampler.request(100), "no extra frames without video");
    }

    #[test]
    fn the_rate_follows_the_policy() {
        let mut sampler = FrameSampler::new();
        sampler.set_policy(&policy(1.0), 0);
        assert_eq!(run(&mut sampler, 0, 10_000, 50), (10, 0));
        sampler.set_policy(&policy(0.5), 10_000);
        assert_eq!(run(&mut sampler, 10_000, 20_000, 50), (5, 0));
        sampler.set_policy(&policy(4.0), 20_000);
        assert_eq!(run(&mut sampler, 20_000, 22_000, 10), (8, 0));
        sampler.set_policy(
            &VideoPolicy {
                enabled: false,
                ..policy(1.0)
            },
            22_000,
        );
        assert_eq!(run(&mut sampler, 22_000, 30_000, 50), (0, 0));
    }

    #[test]
    fn a_stall_does_not_burst() {
        let mut sampler = FrameSampler::new();
        sampler.set_policy(&policy(1.0), 0);
        assert_eq!(sampler.poll(0), Some(SampleKind::Regular));
        // No polls for 5 s (no frame decoded), then one: a single frame, not five.
        assert_eq!(sampler.poll(5_000), Some(SampleKind::Regular));
        assert_eq!(sampler.poll(5_050), None);
        assert_eq!(sampler.poll(6_000), Some(SampleKind::Regular));
    }

    #[test]
    fn requests_are_extra_and_rate_limited() {
        let mut sampler = FrameSampler::new();
        sampler.set_policy(&policy(1.0), 0);
        assert_eq!(sampler.poll(0), Some(SampleKind::Regular));
        // Six requests within one second: four accepted.
        let accepted = (0..6).filter(|i| sampler.request(100 + i * 10)).count();
        assert_eq!(accepted, MAX_REQUESTS_PER_SEC);
        assert_eq!(sampler.poll(200), Some(SampleKind::Requested));
        assert_eq!(sampler.poll(210), None, "requests coalesce into one frame");
        // The regular schedule is untouched by the extra frame.
        assert_eq!(sampler.poll(1_000), Some(SampleKind::Regular));
        // A second later the window has room again.
        assert!(sampler.request(1_200));
        assert_eq!(sampler.poll(1_210), Some(SampleKind::Requested));
    }
}
