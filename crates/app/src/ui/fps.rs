//! Frames drawn per second and the time spent building each frame.
//!
//! GPUI only redraws when something changes, so this counts frames actually
//! drawn (≤ the 30 Hz data tick while streaming), not a refresh-rate ceiling.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct FpsMeter {
    frames: VecDeque<Instant>,
    /// Exponential moving average of frame build time.
    build: Duration,
}

impl FpsMeter {
    /// Call at the start of a frame; returns the start time for `end`.
    pub fn begin(&mut self) -> Instant {
        let now = Instant::now();
        self.frames.push_back(now);
        while self.frames.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1)) {
            self.frames.pop_front();
        }
        now
    }

    pub fn end(&mut self, start: Instant) {
        let took = start.elapsed();
        self.build = if self.build.is_zero() { took } else { (self.build * 7 + took) / 8 };
    }

    /// Frames in the last second (counting only recent ones).
    pub fn fps(&self) -> usize {
        let now = Instant::now();
        self.frames.iter().filter(|t| now.duration_since(**t) <= Duration::from_secs(1)).count()
    }

    pub fn build_ms(&self) -> f64 {
        self.build.as_secs_f64() * 1e3
    }

    pub fn reset(&mut self) {
        self.frames.clear();
        self.build = Duration::ZERO;
    }
}
