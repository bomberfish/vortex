//! Frames drawn per second and the CPU time each frame takes.
//!
//! GPUI only redraws when something changes, so this counts frames actually
//! drawn (≤ the 30 Hz data tick while streaming), not a refresh-rate ceiling.
//!
//! Frame time runs from the start of the view's `render` to a probe element
//! painted last in the tree, so it covers building the element tree, layout,
//! prepaint and paint, everything on the CPU before GPU submission.

use gpui::{canvas, prelude::*};
use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct FpsMeter {
    frames: VecDeque<Instant>,
    /// Exponential moving average of frame time.
    frame: Duration,
    /// Written by the probe when the previous frame finished painting.
    probe: Rc<Cell<Option<Duration>>>,
}

impl FpsMeter {
    /// Call at the start of `render`; returns the frame's start time.
    pub fn begin(&mut self) -> Instant {
        if let Some(took) = self.probe.take() {
            self.frame = if self.frame.is_zero() { took } else { (self.frame * 7 + took) / 8 };
        }
        let now = Instant::now();
        self.frames.push_back(now);
        while self.frames.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1)) {
            self.frames.pop_front();
        }
        now
    }

    /// A zero-size element that records the frame time when painted; add it
    /// as the root's last child.
    pub fn probe(&self, start: Instant) -> impl IntoElement {
        let cell = self.probe.clone();
        canvas(|_, _, _| (), move |_, _, _, _| cell.set(Some(start.elapsed()))).absolute().size_0()
    }

    /// Frames in the last second.
    pub fn fps(&self) -> usize {
        let now = Instant::now();
        self.frames.iter().filter(|t| now.duration_since(**t) <= Duration::from_secs(1)).count()
    }

    pub fn frame_ms(&self) -> f64 {
        self.frame.as_secs_f64() * 1e3
    }

    pub fn reset(&mut self) {
        self.frames.clear();
        self.frame = Duration::ZERO;
        self.probe.set(None);
    }
}
