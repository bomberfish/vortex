//! Lock-free counters shared between the network, the pipeline and the UI,
//! plus a sampler that turns them into rates.

use crate::decode::NoticeLevel;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

#[derive(Default)]
pub struct Stats {
    /// Websocket data messages received.
    pub frames: AtomicU64,
    /// Bytes as received off the socket (compressed if compression is on).
    pub wire_bytes: AtomicU64,
    /// Bytes after decompression / transcoding.
    pub decoded_bytes: AtomicU64,
    /// Events decoded (after dedup).
    pub events: AtomicU64,
    /// Events that passed the local filter.
    pub matched: AtomicU64,
    pub decode_errors: AtomicU64,
    /// Events dropped as replays after a reconnect.
    pub duplicates: AtomicU64,
    /// Cursor of the newest event processed.
    pub last_seq: AtomicI64,
    /// Event time of the newest event processed (unix µs).
    pub last_time_us: AtomicI64,
    /// Portable resume timestamp of the newest event processed (unix µs).
    pub last_witnessed_us: AtomicI64,
    /// Frames waiting between the network and the pipeline.
    pub queue_depth: AtomicUsize,
    /// Nanoseconds the pipeline thread has spent working (not waiting).
    pub busy_ns: AtomicU64,
    /// Successful connections so far.
    pub connects: AtomicU64,
}

impl Stats {
    pub fn snapshot(&self) -> Snapshot {
        let l = Ordering::Relaxed;
        Snapshot {
            frames: self.frames.load(l),
            wire_bytes: self.wire_bytes.load(l),
            decoded_bytes: self.decoded_bytes.load(l),
            events: self.events.load(l),
            matched: self.matched.load(l),
            decode_errors: self.decode_errors.load(l),
            duplicates: self.duplicates.load(l),
            last_seq: self.last_seq.load(l),
            last_time_us: self.last_time_us.load(l),
            queue_depth: self.queue_depth.load(l),
            busy_ns: self.busy_ns.load(l),
        }
    }

    /// Forget stream position (used when the user starts a fresh session).
    pub fn reset_position(&self) {
        self.last_seq.store(0, Ordering::Relaxed);
        self.last_time_us.store(0, Ordering::Relaxed);
        self.last_witnessed_us.store(0, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub frames: u64,
    pub wire_bytes: u64,
    pub decoded_bytes: u64,
    pub events: u64,
    pub matched: u64,
    pub decode_errors: u64,
    pub duplicates: u64,
    pub last_seq: i64,
    pub last_time_us: i64,
    pub queue_depth: usize,
    pub busy_ns: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Rates {
    pub events_per_s: f64,
    pub matched_per_s: f64,
    pub wire_bytes_per_s: f64,
    pub decoded_bytes_per_s: f64,
    /// Fraction of wall time the pipeline thread was busy (0..1).
    pub pipeline_load: f64,
}

/// Turns counter snapshots into smoothed rates and a short history.
pub struct RateSampler {
    prev: Snapshot,
    prev_at: Instant,
    pub rates: Rates,
    /// Recent events/s samples, oldest first.
    pub history: VecDeque<f32>,
    /// Recent matched/s samples, oldest first.
    pub matched_history: VecDeque<f32>,
    capacity: usize,
}

impl RateSampler {
    pub fn new(history: usize) -> Self {
        RateSampler {
            prev: Snapshot::default(),
            prev_at: Instant::now(),
            rates: Rates::default(),
            history: VecDeque::with_capacity(history),
            matched_history: VecDeque::with_capacity(history),
            capacity: history,
        }
    }

    /// Feed a new snapshot; call at a steady cadence (e.g. 4×/s).
    pub fn sample(&mut self, now: Snapshot) -> Rates {
        let at = Instant::now();
        let dt = at.duration_since(self.prev_at).as_secs_f64();
        if dt < 1e-3 {
            return self.rates;
        }
        let per_s = |a: u64, b: u64| a.saturating_sub(b) as f64 / dt;
        let inst = Rates {
            events_per_s: per_s(now.events, self.prev.events),
            matched_per_s: per_s(now.matched, self.prev.matched),
            wire_bytes_per_s: per_s(now.wire_bytes, self.prev.wire_bytes),
            decoded_bytes_per_s: per_s(now.decoded_bytes, self.prev.decoded_bytes),
            pipeline_load: (now.busy_ns.saturating_sub(self.prev.busy_ns) as f64 / 1e9 / dt).min(1.0),
        };
        // Light smoothing so the readout does not jitter.
        let a = 0.5;
        let mix = |old: f64, new: f64| old + (new - old) * a;
        self.rates = Rates {
            events_per_s: mix(self.rates.events_per_s, inst.events_per_s),
            matched_per_s: mix(self.rates.matched_per_s, inst.matched_per_s),
            wire_bytes_per_s: mix(self.rates.wire_bytes_per_s, inst.wire_bytes_per_s),
            decoded_bytes_per_s: mix(self.rates.decoded_bytes_per_s, inst.decoded_bytes_per_s),
            pipeline_load: mix(self.rates.pipeline_load, inst.pipeline_load),
        };
        for (h, v) in [(&mut self.history, inst.events_per_s), (&mut self.matched_history, inst.matched_per_s)] {
            if h.len() == self.capacity {
                h.pop_front();
            }
            h.push_back(v as f32);
        }
        self.prev = now;
        self.prev_at = at;
        self.rates
    }
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub time_us: i64,
    pub level: NoticeLevel,
    pub text: String,
}

/// A small bounded activity log (connection lifecycle, server notices).
pub struct Log {
    entries: Mutex<VecDeque<LogEntry>>,
    version: AtomicU64,
}

impl Default for Log {
    fn default() -> Self {
        Log { entries: Mutex::new(VecDeque::new()), version: AtomicU64::new(0) }
    }
}

const LOG_CAPACITY: usize = 500;

impl Log {
    pub fn push(&self, level: NoticeLevel, text: impl Into<String>) {
        let mut g = self.entries.lock();
        if g.len() == LOG_CAPACITY {
            g.pop_front();
        }
        g.push_back(LogEntry { time_us: crate::time::now_us(), level, text: text.into() });
        drop(g);
        self.version.fetch_add(1, Ordering::Release);
    }

    pub fn info(&self, text: impl Into<String>) {
        self.push(NoticeLevel::Info, text);
    }

    pub fn error(&self, text: impl Into<String>) {
        self.push(NoticeLevel::Error, text);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.entries.lock().iter().cloned().collect()
    }

    pub fn last(&self) -> Option<LogEntry> {
        self.entries.lock().back().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampler_computes_rates() {
        let stats = Stats::default();
        let mut s = RateSampler::new(4);
        s.sample(stats.snapshot());
        for _ in 0..6 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            stats.events.fetch_add(200, Ordering::Relaxed);
            stats.matched.fetch_add(20, Ordering::Relaxed);
            s.sample(stats.snapshot());
        }
        // ~200 events per ~20ms ≈ 10k/s; allow generous scheduling slack.
        assert!(s.rates.events_per_s > 2_000.0 && s.rates.events_per_s < 12_000.0, "{}", s.rates.events_per_s);
        assert!(s.rates.matched_per_s < s.rates.events_per_s);
        assert_eq!(s.history.len(), 4);
    }

    #[test]
    fn log_is_bounded() {
        let log = Log::default();
        for i in 0..LOG_CAPACITY + 10 {
            log.info(format!("entry {i}"));
        }
        let snap = log.snapshot();
        assert_eq!(snap.len(), LOG_CAPACITY);
        assert_eq!(snap[0].text, "entry 10");
        assert_eq!(log.last().unwrap().text, format!("entry {}", LOG_CAPACITY + 9));
        assert_eq!(log.version(), (LOG_CAPACITY + 10) as u64);
    }
}
