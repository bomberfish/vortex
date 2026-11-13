//! The event buffer the UI renders from.
//!
//! Two tiers:
//!
//! * `ring` — every retained event (matching or not), bounded by count and
//!   bytes. Keeping non-matching events is what makes changing the filter
//!   instant: the new filter is simply re-run over the ring, in parallel.
//! * `pinned` — matching events that aged out of the ring. With a narrow
//!   filter on a busy stream the ring turns over in minutes; without this
//!   tier matches would vanish with it.
//!
//! The view (what the list shows) is `pinned ++ matching events in ring`.
//! Ids are assigned contiguously, so ring lookups by id are index arithmetic.
//!
//! Writers hold the lock once per batch; garbage produced by eviction is
//! dropped after the lock is released. Readers that must never stall (the
//! render path) use [`Store::try_view`].

use crate::event::{Event, Kind};
use crate::filter::Filter;
use parking_lot::{RwLock, RwLockReadGuard};
use rayon::prelude::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreConfig {
    /// Max events kept in the ring.
    pub ring_events: usize,
    /// Max bytes kept in the ring.
    pub ring_bytes: usize,
    /// Max matching events kept after they leave the ring.
    pub pinned_events: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig { ring_events: 300_000, ring_bytes: 512 << 20, pinned_events: 100_000 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FacetCount {
    /// Events currently in the ring.
    pub seen: u32,
    /// Events currently in the view.
    pub matched: u32,
}

/// Live histograms over the buffer, maintained incrementally.
#[derive(Debug, Clone, Default)]
pub struct Facets {
    /// Indexed by `Nsid::index()`.
    pub collections: Vec<FacetCount>,
    /// Indexed by `Kind as usize`.
    pub kinds: [FacetCount; 4],
}

impl Facets {
    #[inline]
    fn slot(&mut self, e: &Event) -> (&mut FacetCount, &mut FacetCount) {
        let kind = &mut self.kinds[e.kind as usize];
        let idx = e.collection.map(|c| c.index() as usize + 1).unwrap_or(0);
        if self.collections.len() <= idx {
            self.collections.resize(idx + 1, FacetCount::default());
        }
        (kind, &mut self.collections[idx])
    }

    #[inline]
    fn seen(&mut self, e: &Event, delta: i32) {
        let (k, c) = self.slot(e);
        k.seen = k.seen.wrapping_add_signed(delta);
        c.seen = c.seen.wrapping_add_signed(delta);
    }

    #[inline]
    fn matched(&mut self, e: &Event, delta: i32) {
        let (k, c) = self.slot(e);
        k.matched = k.matched.wrapping_add_signed(delta);
        c.matched = c.matched.wrapping_add_signed(delta);
    }

    /// Counts for a collection by its `Nsid::index()`.
    pub fn collection(&self, nsid_index: u32) -> FacetCount {
        self.collections.get(nsid_index as usize + 1).copied().unwrap_or_default()
    }

    pub fn kind(&self, k: Kind) -> FacetCount {
        self.kinds[k as usize]
    }

    fn clear_matched(&mut self) {
        for c in &mut self.collections {
            c.matched = 0;
        }
        for k in &mut self.kinds {
            k.matched = 0;
        }
    }
}

struct Inner {
    cfg: StoreConfig,
    ring: VecDeque<Event>,
    pinned: VecDeque<Event>,
    /// Ids of matching events in `ring`, ascending.
    matched: VecDeque<u64>,
    next_id: u64,
    ring_bytes: usize,
    pinned_bytes: usize,
    filter: Arc<Filter>,
    epoch: u64,
    facets: Facets,
    /// View rows removed from the front since the last rebuild.
    dropped_front: u64,
    /// Bumps whenever the view is rebuilt wholesale (filter change, clear).
    generation: u64,
}

impl Inner {
    #[inline]
    fn ring_base(&self) -> u64 {
        self.ring.front().map(|e| e.id).unwrap_or(self.next_id)
    }

    /// Evict until within limits. Evicted non-matches and overflowed pins are
    /// moved to `garbage` so their memory is released outside the lock.
    fn enforce_limits(&mut self, garbage: &mut Vec<Event>) {
        while self.ring.len() > self.cfg.ring_events || self.ring_bytes > self.cfg.ring_bytes {
            let Some(e) = self.ring.pop_front() else { break };
            let size = e.mem_size();
            self.ring_bytes -= size;
            self.facets.seen(&e, -1);
            if self.matched.front() == Some(&e.id) {
                self.matched.pop_front();
                self.pinned_bytes += size;
                self.pinned.push_back(e);
            } else {
                garbage.push(e);
            }
        }
        while self.pinned.len() > self.cfg.pinned_events {
            let Some(e) = self.pinned.pop_front() else { break };
            self.pinned_bytes -= e.mem_size();
            self.facets.matched(&e, -1);
            self.dropped_front += 1;
            garbage.push(e);
        }
    }
}

pub struct Store {
    inner: RwLock<Inner>,
    /// Bumps on every visible change; cheap for the UI to poll.
    version: AtomicU64,
    /// Mirrors `Inner::epoch` so writers can detect a filter change lock-free.
    epoch: AtomicU64,
}

/// What a refilter did.
#[derive(Debug, Clone, Copy, Default)]
pub struct RefilterStats {
    pub scanned: usize,
    pub matched: usize,
    pub elapsed: Duration,
}

impl Store {
    pub fn new(cfg: StoreConfig) -> Self {
        Store {
            inner: RwLock::new(Inner {
                cfg,
                ring: VecDeque::new(),
                pinned: VecDeque::new(),
                matched: VecDeque::new(),
                next_id: 1,
                ring_bytes: 0,
                pinned_bytes: 0,
                filter: Arc::new(Filter::all()),
                epoch: 0,
                facets: Facets::default(),
                dropped_front: 0,
                generation: 0,
            }),
            version: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    #[inline]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// The current filter and the epoch it belongs to.
    pub fn filter(&self) -> (Arc<Filter>, u64) {
        let g = self.inner.read();
        (g.filter.clone(), g.epoch)
    }

    /// Append a batch. `verdicts[i]` is whether `events[i]` matched the filter
    /// of `epoch`; if the filter changed in the meantime the batch is
    /// re-evaluated here. Drains `events`. Returns how many matched.
    pub fn append(&self, events: &mut Vec<Event>, verdicts: &[bool], epoch: u64) -> usize {
        if events.is_empty() {
            return 0;
        }
        debug_assert_eq!(events.len(), verdicts.len());
        let mut garbage = Vec::new();
        let mut n_matched = 0;
        {
            let mut g = self.inner.write();
            let stale = g.epoch != epoch;
            let filter = stale.then(|| g.filter.clone());
            g.ring.reserve(events.len());
            for (i, mut e) in events.drain(..).enumerate() {
                let hit = match &filter {
                    Some(f) => f.matches(&e),
                    None => verdicts.get(i).copied().unwrap_or(false),
                };
                e.id = g.next_id;
                g.next_id += 1;
                g.ring_bytes += e.mem_size();
                g.facets.seen(&e, 1);
                if hit {
                    g.matched.push_back(e.id);
                    g.facets.matched(&e, 1);
                    n_matched += 1;
                }
                g.ring.push_back(e);
            }
            g.enforce_limits(&mut garbage);
        }
        self.version.fetch_add(1, Ordering::Release);
        drop(garbage);
        n_matched
    }

    /// Install a new filter and rebuild the view from retained history.
    pub fn set_filter(&self, filter: Filter) -> RefilterStats {
        let started = std::time::Instant::now();
        let filter = Arc::new(filter);
        let mut garbage = Vec::new();
        let stats;
        {
            let mut g = self.inner.write();
            g.epoch += 1;
            g.generation += 1;
            g.dropped_front = 0;
            g.filter = filter.clone();

            // Old pins only survive if they match the new filter too.
            let pinned = std::mem::take(&mut g.pinned);
            let keep: Vec<bool> = pinned.par_iter().map(|e| filter.matches(e)).collect();
            g.pinned_bytes = 0;
            let mut kept = VecDeque::with_capacity(pinned.len());
            for (e, keep) in pinned.into_iter().zip(keep) {
                if keep {
                    g.pinned_bytes += e.mem_size();
                    kept.push_back(e);
                } else {
                    garbage.push(e);
                }
            }
            g.pinned = kept;

            let (a, b) = g.ring.as_slices();
            let scan = |s: &[Event]| -> Vec<u64> {
                s.par_iter().with_min_len(2048).filter(|e| filter.matches(e)).map(|e| e.id).collect()
            };
            let (mut ma, mb) = rayon::join(|| scan(a), || scan(b));
            ma.extend(mb);
            let scanned = g.ring.len() + g.pinned.len() + garbage.len();
            g.matched = VecDeque::from(ma);

            // Rebuild matched-side facets from scratch.
            let inner = &mut *g;
            inner.facets.clear_matched();
            for e in &inner.pinned {
                inner.facets.matched(e, 1);
            }
            let base = inner.ring.front().map(|e| e.id).unwrap_or(inner.next_id);
            for &id in &inner.matched {
                inner.facets.matched(&inner.ring[(id - base) as usize], 1);
            }
            g.enforce_limits(&mut garbage);
            self.epoch.store(g.epoch, Ordering::Release);
            stats = RefilterStats { scanned, matched: g.pinned.len() + g.matched.len(), elapsed: started.elapsed() };
        }
        self.version.fetch_add(1, Ordering::Release);
        drop(garbage);
        RefilterStats { elapsed: started.elapsed(), ..stats }
    }

    pub fn clear(&self) {
        let (ring, pinned);
        {
            let mut g = self.inner.write();
            ring = std::mem::take(&mut g.ring);
            pinned = std::mem::take(&mut g.pinned);
            g.matched.clear();
            g.ring_bytes = 0;
            g.pinned_bytes = 0;
            g.facets = Facets::default();
            g.dropped_front = 0;
            g.generation += 1;
        }
        self.version.fetch_add(1, Ordering::Release);
        drop((ring, pinned));
    }

    pub fn config(&self) -> StoreConfig {
        self.inner.read().cfg
    }

    pub fn set_config(&self, cfg: StoreConfig) {
        let mut garbage = Vec::new();
        {
            let mut g = self.inner.write();
            g.cfg = cfg;
            g.enforce_limits(&mut garbage);
        }
        self.version.fetch_add(1, Ordering::Release);
        drop(garbage);
    }

    /// Blocking read access.
    pub fn view(&self) -> View<'_> {
        View { g: self.inner.read() }
    }

    /// Read access that gives up after `timeout` instead of stalling behind a
    /// long writer (e.g. a refilter). For the render path.
    pub fn try_view(&self, timeout: Duration) -> Option<View<'_>> {
        self.inner.try_read_for(timeout).map(|g| View { g })
    }
}

/// A consistent read-only snapshot of the store (holds the read lock).
pub struct View<'a> {
    g: RwLockReadGuard<'a, Inner>,
}

impl View<'_> {
    /// Number of rows in the view.
    #[inline]
    pub fn len(&self) -> usize {
        self.g.pinned.len() + self.g.matched.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Row `i` of the view, oldest first.
    #[inline]
    pub fn row(&self, i: usize) -> Option<&Event> {
        let g = &*self.g;
        if i < g.pinned.len() {
            return g.pinned.get(i);
        }
        let id = *g.matched.get(i - g.pinned.len())?;
        g.ring.get((id - g.ring_base()) as usize)
    }

    /// Find an event by id anywhere in the buffer (matching or not).
    pub fn get(&self, id: u64) -> Option<&Event> {
        let g = &*self.g;
        let base = g.ring_base();
        if id >= base {
            return g.ring.get((id - base) as usize);
        }
        let i = g.pinned.partition_point(|e| e.id < id);
        g.pinned.get(i).filter(|e| e.id == id)
    }

    /// The view row showing event `id`, if it is in the view.
    pub fn index_of(&self, id: u64) -> Option<usize> {
        let g = &*self.g;
        if id >= g.ring_base() {
            let i = g.matched.partition_point(|&m| m < id);
            return (g.matched.get(i) == Some(&id)).then_some(g.pinned.len() + i);
        }
        let i = g.pinned.partition_point(|e| e.id < id);
        (g.pinned.get(i).map(|e| e.id) == Some(id)).then_some(i)
    }

    /// Events held in the ring (matching or not).
    pub fn ring_len(&self) -> usize {
        self.g.ring.len()
    }

    pub fn pinned_len(&self) -> usize {
        self.g.pinned.len()
    }

    /// Bytes held by buffered events.
    pub fn bytes(&self) -> usize {
        self.g.ring_bytes + self.g.pinned_bytes
    }

    pub fn facets(&self) -> &Facets {
        &self.g.facets
    }

    /// Rows evicted from the front of the view since the last rebuild; lets a
    /// scrolled-back list keep its position as old rows fall away.
    pub fn dropped_front(&self) -> u64 {
        self.g.dropped_front
    }

    pub fn generation(&self) -> u64 {
        self.g.generation
    }

    pub fn filter(&self) -> &Arc<Filter> {
        &self.g.filter
    }

    /// Time span covered by the ring, in microseconds.
    pub fn ring_span_us(&self) -> i64 {
        match (self.g.ring.front(), self.g.ring.back()) {
            (Some(a), Some(b)) => b.time_us - a.time_us,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::frame::decode_json_frame;
    use crate::decode::Decoded;
    use crate::event::EventBuilder;
    use crate::nsid::Nsid;

    fn ev(seq: i64, collection: &str, text: &str) -> Event {
        let src = format!(
            r#"{{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":{seq},"kind":"commit","commit":{{"operation":"create","collection":"{collection}","rkey":"k{seq}","record":{{"text":"{text}"}}}}}}"#
        );
        match decode_json_frame(&src, &mut EventBuilder::new()).unwrap() {
            Decoded::Event(e) => e,
            other => panic!("{other:?}"),
        }
    }

    /// Append `n` events alternating post/like; posts say "even N".
    fn feed(store: &Store, from: i64, n: i64) {
        let (filter, epoch) = store.filter();
        let mut events: Vec<Event> = (from..from + n)
            .map(|i| {
                if i % 2 == 0 {
                    ev(i, "app.bsky.feed.post", &format!("even {i}"))
                } else {
                    ev(i, "app.bsky.feed.like", "")
                }
            })
            .collect();
        let verdicts: Vec<bool> = events.iter().map(|e| filter.matches(e)).collect();
        store.append(&mut events, &verdicts, epoch);
    }

    fn rows(store: &Store) -> Vec<i64> {
        let v = store.view();
        (0..v.len()).map(|i| v.row(i).unwrap().seq).collect()
    }

    fn small(ring: usize, pinned: usize) -> Store {
        Store::new(StoreConfig { ring_events: ring, ring_bytes: usize::MAX, pinned_events: pinned })
    }

    #[test]
    fn append_and_view_everything() {
        let s = small(100, 100);
        let v0 = s.version();
        feed(&s, 0, 10);
        assert!(s.version() > v0);
        assert_eq!(rows(&s), (0..10).collect::<Vec<_>>());
        let v = s.view();
        assert_eq!((v.len(), v.ring_len(), v.pinned_len()), (10, 10, 0));
        assert!(v.bytes() > 0);
        // Ids are contiguous from 1 and resolvable.
        assert_eq!(v.row(0).unwrap().id, 1);
        assert_eq!(v.get(10).unwrap().seq, 9);
        assert!(v.get(11).is_none());
        assert_eq!(v.index_of(4), Some(3));
    }

    #[test]
    fn refilter_rebuilds_view_from_history() {
        let s = small(100, 100);
        feed(&s, 0, 20);
        let st = s.set_filter(Filter::parse("type:post").unwrap());
        assert_eq!((st.scanned, st.matched), (20, 10));
        assert_eq!(rows(&s), (0..20).step_by(2).collect::<Vec<_>>());
        // Non-matching events are still retained and come back.
        s.set_filter(Filter::parse("type:like").unwrap());
        assert_eq!(rows(&s), (1..20).step_by(2).collect::<Vec<_>>());
        s.set_filter(Filter::all());
        assert_eq!(rows(&s).len(), 20);
        // New events are filtered on the way in.
        s.set_filter(Filter::parse("\"even 2\"").unwrap());
        feed(&s, 20, 10);
        assert_eq!(rows(&s), [2, 20, 22, 24, 26, 28]);
    }

    #[test]
    fn matches_survive_ring_eviction_via_pins() {
        let s = small(10, 4);
        s.set_filter(Filter::parse("type:post").unwrap());
        feed(&s, 0, 30);
        let v = s.view();
        assert_eq!(v.ring_len(), 10);
        // Ring holds 20..30 (5 posts); pins hold the 4 newest older posts.
        assert_eq!(v.pinned_len(), 4);
        drop(v);
        assert_eq!(rows(&s), [12, 14, 16, 18, 20, 22, 24, 26, 28]);
        let v = s.view();
        // 10 posts left the ring; 4 are pinned, so 6 fell off the front.
        assert_eq!(v.dropped_front(), 6);
        // Pinned rows resolve by id and index.
        let first = v.row(0).unwrap();
        assert_eq!(v.index_of(first.id), Some(0));
        assert_eq!(v.get(first.id).unwrap().seq, 12);
        let ring_row = v.row(5).unwrap();
        assert_eq!(v.index_of(ring_row.id), Some(5));
        // Evicted non-matches are gone.
        assert!(v.get(2).is_none());
        assert_eq!(v.index_of(2), None);
    }

    #[test]
    fn refilter_drops_pins_that_no_longer_match() {
        let s = small(10, 100);
        s.set_filter(Filter::parse("type:post").unwrap());
        feed(&s, 0, 30);
        assert_eq!(s.view().pinned_len(), 10);
        s.set_filter(Filter::parse("\"even 4\" OR \"even 24\"").unwrap());
        assert_eq!(rows(&s), [4, 24]);
        assert_eq!(s.view().pinned_len(), 1);
        s.set_filter(Filter::parse("type:like").unwrap());
        assert_eq!(rows(&s), [21, 23, 25, 27, 29]);
        assert_eq!(s.view().pinned_len(), 0);
        assert_eq!(s.view().dropped_front(), 0);
    }

    #[test]
    fn stale_epoch_is_reevaluated() {
        let s = small(100, 100);
        let (filter, epoch) = s.filter();
        let mut events: Vec<Event> = (0..6).map(|i| ev(i, "app.bsky.feed.like", "")).collect();
        let verdicts: Vec<bool> = events.iter().map(|e| filter.matches(e)).collect();
        assert!(verdicts.iter().all(|v| *v));
        // Filter changes after the batch was evaluated but before append.
        s.set_filter(Filter::parse("type:post").unwrap());
        assert_eq!(s.append(&mut events, &verdicts, epoch), 0);
        assert!(rows(&s).is_empty());
        assert_eq!(s.view().ring_len(), 6);
        assert_ne!(s.epoch(), epoch);
    }

    #[test]
    fn facets_track_ring_and_view() {
        let s = small(10, 2);
        s.set_filter(Filter::parse("type:post").unwrap());
        feed(&s, 0, 30);
        let post = Nsid::intern("app.bsky.feed.post").index();
        let like = Nsid::intern("app.bsky.feed.like").index();
        let v = s.view();
        assert_eq!(v.facets().collection(post), FacetCount { seen: 5, matched: 7 });
        assert_eq!(v.facets().collection(like), FacetCount { seen: 5, matched: 0 });
        assert_eq!(v.facets().kind(Kind::Commit), FacetCount { seen: 10, matched: 7 });
        drop(v);
        s.set_filter(Filter::all());
        let v = s.view();
        assert_eq!(v.facets().collection(like).matched, 5);
        assert_eq!(v.facets().collection(post).matched, 7);
        drop(v);
        s.clear();
        let v = s.view();
        assert_eq!(v.facets().kind(Kind::Commit), FacetCount::default());
        assert!(v.is_empty());
    }

    #[test]
    fn byte_limit_and_reconfigure() {
        let s = Store::new(StoreConfig { ring_events: 1000, ring_bytes: 4000, pinned_events: 0 });
        feed(&s, 0, 100);
        let v = s.view();
        assert!(v.bytes() <= 4000);
        assert!(v.ring_len() < 100 && v.ring_len() > 0);
        // pinned_events = 0: everything that leaves the ring is dropped.
        assert_eq!(v.pinned_len(), 0);
        drop(v);
        s.set_config(StoreConfig { ring_events: 3, ring_bytes: usize::MAX, pinned_events: 0 });
        assert_eq!(rows(&s), [97, 98, 99]);
        let gen = s.view().generation();
        s.clear();
        assert!(s.view().generation() > gen);
        feed(&s, 200, 2);
        assert_eq!(rows(&s), [200, 201]);
    }

    #[test]
    fn try_view_does_not_block_behind_a_writer() {
        let s = Arc::new(small(100, 100));
        feed(&s, 0, 4);
        let guard = s.inner.write();
        assert!(s.try_view(Duration::from_millis(1)).is_none());
        drop(guard);
        assert_eq!(s.try_view(Duration::from_millis(1)).unwrap().len(), 4);
    }
}
