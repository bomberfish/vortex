//! The processing stage: frames in, filtered events into the store.
//!
//! One dedicated OS thread drains the frame channel in batches. A batch is
//! whatever accumulated while the previous one was being processed, so the
//! stage self-tunes: at live-tail rates batches are a handful of frames and
//! latency is a single thread hop; during replay bursts batches grow, the
//! store lock is taken once per thousands of events, and decoding fans out
//! across the rayon pool (order-preserving). The channel is bounded, so if
//! even that cannot keep up, back-pressure reaches the socket instead of
//! memory growing.

use crate::decode::zstd_dict::Dictionaries;
use crate::decode::{Decoded, FrameDecoder, Wire};
use crate::event::Event;
use crate::filter::Filter;
use crate::stats::{Log, Stats};
use crate::store::Store;
use bytes::Bytes;
use rayon::prelude::*;
use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// Frames buffered between the socket and the pipeline before back-pressure.
pub const CHANNEL_CAPACITY: usize = 16_384;
/// Largest batch processed under one store lock.
const MAX_BATCH: usize = 8_192;
/// Batches at least this large are decoded in parallel.
const PAR_MIN: usize = 512;
const PAR_CHUNK: usize = 128;

pub struct FrameMsg {
    pub wire: Wire,
    pub is_text: bool,
    pub payload: Bytes,
}

pub enum Msg {
    Frame(FrameMsg),
    /// A connection went live. With `fresh_cursor_space` the previous
    /// connection's cursors are not comparable (different host, or the user
    /// asked for a different position), so replay dedup starts over.
    Connected { fresh_cursor_space: bool },
}

thread_local! {
    static DECODER: RefCell<Option<FrameDecoder>> = const { RefCell::new(None) };
}

struct FrameInfo {
    n_items: u32,
    witnessed_us: i64,
    wire: Wire,
    failed: bool,
}

#[derive(Default)]
struct ChunkOut {
    items: Vec<Decoded>,
    frames: Vec<FrameInfo>,
}

fn decode_chunk(frames: &[FrameMsg], dicts: &Arc<Dictionaries>) -> ChunkOut {
    DECODER.with(|slot| {
        let mut slot = slot.borrow_mut();
        // Decoders are per-thread and keyed to the dictionary set in use.
        if slot.as_ref().is_some_and(|d| !Arc::ptr_eq(d.dictionaries(), dicts)) {
            *slot = None;
        }
        let dec = slot.get_or_insert_with(|| FrameDecoder::new(dicts.clone()));
        let mut out = ChunkOut { items: Vec::with_capacity(frames.len()), frames: Vec::with_capacity(frames.len()) };
        for f in frames {
            let before = out.items.len();
            let res = dec.decode(f.wire, &f.payload, f.is_text, &mut out.items);
            out.frames.push(FrameInfo {
                n_items: (out.items.len() - before) as u32,
                witnessed_us: res.as_ref().map(|m| m.witnessed_us).unwrap_or(0),
                wire: f.wire,
                failed: res.is_err(),
            });
        }
        out
    })
}

struct Pipeline {
    store: Arc<Store>,
    stats: Arc<Stats>,
    log: Arc<Log>,
    dicts: Arc<Dictionaries>,
    filter: Arc<Filter>,
    epoch: u64,
    /// Highest cursor accepted on the current cursor space.
    floor: i64,
    events: Vec<Event>,
    verdicts: Vec<bool>,
    errors_logged: u32,
}

impl Pipeline {
    fn flush(&mut self, frames: &mut Vec<FrameMsg>) {
        if frames.is_empty() {
            return;
        }
        let dicts = &self.dicts;
        let outs: Vec<ChunkOut> = if frames.len() >= PAR_MIN {
            frames.par_chunks(PAR_CHUNK).map(|c| decode_chunk(c, dicts)).collect()
        } else {
            vec![decode_chunk(frames, dicts)]
        };

        let wire_bytes: usize = frames.iter().map(|f| f.payload.len()).sum();
        let n_frames = frames.len();
        frames.clear();

        let mut decoded_bytes = 0usize;
        let mut errors = 0u64;
        let mut dups = 0u64;
        let mut witnessed = 0i64;
        for out in outs {
            let mut items = out.items.into_iter();
            for info in out.frames {
                if info.failed {
                    errors += 1;
                }
                // All events of one frame share a cursor (a firehose commit
                // yields one event per op), so dedup is decided per frame.
                let mut frame_is_dup = None;
                for item in items.by_ref().take(info.n_items as usize) {
                    match item {
                        Decoded::Event(e) => {
                            let dup = *frame_is_dup.get_or_insert_with(|| match info.wire {
                                // v1 cursors are timestamps and may collide.
                                Wire::JetstreamV1 => e.seq < self.floor,
                                _ => e.seq <= self.floor,
                            });
                            if dup {
                                dups += 1;
                                continue;
                            }
                            self.floor = self.floor.max(e.seq);
                            decoded_bytes += e.raw().len();
                            if info.witnessed_us != 0 {
                                witnessed = info.witnessed_us;
                            }
                            self.events.push(e);
                        }
                        Decoded::Notice(n) => {
                            let text = if n.message.is_empty() { n.name } else { format!("{}: {}", n.name, n.message) };
                            self.log.push(n.level, text);
                        }
                    }
                }
            }
        }

        let l = Ordering::Relaxed;
        self.stats.frames.fetch_add(n_frames as u64, l);
        self.stats.wire_bytes.fetch_add(wire_bytes as u64, l);
        self.stats.decoded_bytes.fetch_add(decoded_bytes as u64, l);
        if errors > 0 {
            self.stats.decode_errors.fetch_add(errors, l);
            if self.errors_logged < 5 {
                self.errors_logged += 1;
                self.log.error(format!("{errors} frame(s) failed to decode"));
            }
        }
        if dups > 0 {
            self.stats.duplicates.fetch_add(dups, l);
        }
        let Some(last) = self.events.last() else { return };
        self.stats.last_seq.store(last.seq, l);
        self.stats.last_time_us.store(last.time_us, l);
        if witnessed != 0 {
            self.stats.last_witnessed_us.store(witnessed, l);
        }
        self.stats.events.fetch_add(self.events.len() as u64, l);

        // Evaluate the filter outside the store lock.
        if self.store.epoch() != self.epoch {
            (self.filter, self.epoch) = self.store.filter();
        }
        let filter = &*self.filter;
        self.verdicts.clear();
        if self.events.len() >= PAR_MIN {
            self.events.par_iter().with_min_len(PAR_CHUNK).map(|e| filter.matches(e)).collect_into_vec(&mut self.verdicts);
        } else {
            self.verdicts.extend(self.events.iter().map(|e| filter.matches(e)));
        }
        let matched = self.store.append(&mut self.events, &self.verdicts, self.epoch);
        self.stats.matched.fetch_add(matched as u64, l);
    }

    fn run(mut self, mut rx: mpsc::Receiver<Msg>) {
        let mut frames: Vec<FrameMsg> = Vec::with_capacity(1024);
        while let Some(first) = rx.blocking_recv() {
            let started = Instant::now();
            let mut next = Some(first);
            loop {
                let msg = match next.take() {
                    Some(m) => m,
                    None => match rx.try_recv() {
                        Ok(m) => m,
                        Err(_) => break,
                    },
                };
                match msg {
                    Msg::Frame(f) => {
                        frames.push(f);
                        if frames.len() >= MAX_BATCH {
                            self.flush(&mut frames);
                        }
                    }
                    Msg::Connected { fresh_cursor_space } => {
                        self.flush(&mut frames);
                        if fresh_cursor_space {
                            self.floor = i64::MIN;
                        }
                    }
                }
            }
            self.flush(&mut frames);
            self.stats.queue_depth.store(rx.len(), Ordering::Relaxed);
            self.stats.busy_ns.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }
}

/// Start the pipeline thread. It runs until every sender is dropped.
pub fn spawn(
    store: Arc<Store>,
    stats: Arc<Stats>,
    log: Arc<Log>,
    dicts: Arc<Dictionaries>,
) -> (mpsc::Sender<Msg>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (filter, epoch) = store.filter();
    let pipeline = Pipeline {
        store,
        stats,
        log,
        dicts,
        filter,
        epoch,
        floor: i64::MIN,
        events: Vec::with_capacity(1024),
        verdicts: Vec::with_capacity(1024),
        errors_logged: 0,
    };
    let handle = std::thread::Builder::new()
        .name("vortex-pipeline".into())
        .spawn(move || pipeline.run(rx))
        .expect("spawn pipeline thread");
    (tx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::zstd_dict::BUNDLED_V2_DICT;
    use crate::store::StoreConfig;

    struct Rig {
        store: Arc<Store>,
        stats: Arc<Stats>,
        log: Arc<Log>,
        tx: Option<mpsc::Sender<Msg>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Rig {
        fn new() -> Rig {
            let store = Arc::new(Store::new(StoreConfig::default()));
            let stats = Arc::new(Stats::default());
            let log = Arc::new(Log::default());
            let (tx, handle) = spawn(store.clone(), stats.clone(), log.clone(), Arc::new(Dictionaries::default()));
            Rig { store, stats, log, tx: Some(tx), handle: Some(handle) }
        }

        fn send(&self, msg: Msg) {
            self.tx.as_ref().unwrap().blocking_send(msg).ok().unwrap();
        }

        fn text(&self, json: String) {
            self.send(Msg::Frame(FrameMsg { wire: Wire::JetstreamV2, is_text: true, payload: Bytes::from(json) }));
        }

        /// Close the channel and wait for everything to be processed.
        fn finish(&mut self) {
            self.tx = None;
            self.handle.take().unwrap().join().unwrap();
        }

        fn seqs(&self) -> Vec<i64> {
            let v = self.store.view();
            (0..v.len()).map(|i| v.row(i).unwrap().seq).collect()
        }
    }

    fn frame(seq: i64, collection: &str) -> String {
        format!(
            r#"{{"$type":"message","payload":{{"$type":"network.bsky.jetstream.subscribeEvents#commit","did":"did:plc:7e6kocyzb77xkncplrkoojej","seq":{seq},"time":"2026-08-13T06:47:43.000000Z","witnessedAt":"2026-08-13T06:47:44.000000Z","operation":"create","collection":"{collection}","rkey":"k","rev":"r","record":{{"text":"event {seq}"}}}}}}"#
        )
    }

    #[test]
    fn frames_flow_into_the_store_in_order() {
        let mut rig = Rig::new();
        for seq in 1..=50 {
            rig.text(frame(seq, "app.bsky.feed.post"));
        }
        rig.finish();
        assert_eq!(rig.seqs(), (1..=50).collect::<Vec<_>>());
        let s = rig.stats.snapshot();
        assert_eq!((s.frames, s.events, s.matched, s.decode_errors), (50, 50, 50, 0));
        assert_eq!(s.last_seq, 50);
        assert_eq!(rig.stats.last_witnessed_us.load(Ordering::Relaxed), 1_786_603_664_000_000);
        assert!(s.decoded_bytes > 0 && s.wire_bytes == s.decoded_bytes);
    }

    #[test]
    fn large_batches_take_the_parallel_path_and_stay_ordered() {
        let mut rig = Rig::new();
        let mut enc = zstd::bulk::Compressor::with_dictionary(1, BUNDLED_V2_DICT).unwrap();
        // Hold the store so a big batch accumulates behind the first append.
        let n = 6_000;
        {
            let _hold = rig.store.view();
            for seq in 1..=n {
                let packed = enc.compress(frame(seq, "app.bsky.feed.post").as_bytes()).unwrap();
                rig.send(Msg::Frame(FrameMsg { wire: Wire::JetstreamV2, is_text: false, payload: Bytes::from(packed) }));
            }
        }
        rig.finish();
        assert_eq!(rig.seqs(), (1..=n).collect::<Vec<_>>());
        let s = rig.stats.snapshot();
        assert_eq!(s.events, n as u64);
        assert!(s.wire_bytes < s.decoded_bytes, "compressed frames should be smaller");
    }

    #[test]
    fn replayed_events_are_dropped_until_a_fresh_cursor_space() {
        let mut rig = Rig::new();
        for seq in [1, 2, 3] {
            rig.text(frame(seq, "app.bsky.feed.post"));
        }
        // Reconnect to the same host: the cursor is inclusive, so 3 repeats.
        rig.send(Msg::Connected { fresh_cursor_space: false });
        for seq in [3, 4, 5] {
            rig.text(frame(seq, "app.bsky.feed.post"));
        }
        // New host: seq numbering is unrelated, nothing may be dropped.
        rig.send(Msg::Connected { fresh_cursor_space: true });
        for seq in [2, 3] {
            rig.text(frame(seq, "app.bsky.feed.post"));
        }
        rig.finish();
        assert_eq!(rig.seqs(), [1, 2, 3, 4, 5, 2, 3]);
        assert_eq!(rig.stats.snapshot().duplicates, 1);
    }

    #[test]
    fn filter_applies_to_incoming_events_and_tracks_changes() {
        let mut rig = Rig::new();
        rig.store.set_filter(Filter::parse("type:like").unwrap());
        for seq in 1..=10 {
            rig.text(frame(seq, if seq % 2 == 0 { "app.bsky.feed.like" } else { "app.bsky.feed.post" }));
        }
        rig.finish();
        assert_eq!(rig.seqs(), [2, 4, 6, 8, 10]);
        assert_eq!(rig.stats.snapshot().matched, 5);
        assert_eq!(rig.store.view().ring_len(), 10);
    }

    #[test]
    fn bad_frames_and_notices_are_reported_not_fatal() {
        let mut rig = Rig::new();
        rig.text("{not json".into());
        rig.send(Msg::Frame(FrameMsg { wire: Wire::JetstreamV2, is_text: false, payload: Bytes::from_static(b"not zstd") }));
        rig.text(r#"{"$type":"message","payload":{"$type":"network.bsky.jetstream.subscribeEvents#info","name":"OutdatedCursor","message":"resumed from 9"}}"#.into());
        rig.text(frame(9, "app.bsky.feed.post"));
        rig.finish();
        assert_eq!(rig.seqs(), [9]);
        assert_eq!(rig.stats.snapshot().decode_errors, 2);
        let log: Vec<String> = rig.log.snapshot().into_iter().map(|e| e.text).collect();
        assert!(log.iter().any(|l| l == "OutdatedCursor: resumed from 9"), "{log:?}");
        assert!(log.iter().any(|l| l.contains("failed to decode")), "{log:?}");
    }
}
