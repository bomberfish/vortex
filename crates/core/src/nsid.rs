//! Interned collection NSIDs.
//!
//! The set of collections on the network is small (hundreds, occasionally a
//! few thousand), while every commit event names one. Interning turns the
//! per-event collection into an 8-byte pointer with pointer equality, and
//! gives each collection a dense index for facet counters.

use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::fmt;
use std::sync::LazyLock;

pub struct NsidEntry {
    name: Box<str>,
    index: u32,
}

/// An interned NSID. Copy, 8 bytes, compared by pointer.
#[derive(Clone, Copy)]
pub struct Nsid(&'static NsidEntry);

/// Hard cap on distinct interned names so junk NSIDs cannot grow memory
/// without bound; anything past it shares one overflow entry.
const MAX_INTERNED: usize = 1 << 16;
const OVERFLOW_NAME: &str = "(other)";

struct Interner {
    map: FxHashMap<&'static str, Nsid>,
    list: Vec<Nsid>,
}

static INTERNER: LazyLock<RwLock<Interner>> =
    LazyLock::new(|| RwLock::new(Interner { map: FxHashMap::default(), list: Vec::new() }));

thread_local! {
    /// Per-thread front cache so the hot decode path never touches the lock.
    static LOCAL: RefCell<FxHashMap<&'static str, Nsid>> = RefCell::new(FxHashMap::default());
}

impl Nsid {
    pub fn intern(name: &str) -> Nsid {
        LOCAL.with(|local| {
            if let Some(&n) = local.borrow().get(name) {
                return n;
            }
            let n = intern_global(name);
            if n.as_str() == name {
                local.borrow_mut().insert(n.as_str(), n);
            }
            n
        })
    }

    /// Look up without interning.
    pub fn get(name: &str) -> Option<Nsid> {
        INTERNER.read().map.get(name).copied()
    }

    #[inline]
    pub fn as_str(self) -> &'static str {
        &self.0.name
    }

    /// Dense index, stable for the life of the process.
    #[inline]
    pub fn index(self) -> u32 {
        self.0.index
    }

    /// Number of interned names so far.
    pub fn count() -> usize {
        INTERNER.read().list.len()
    }

    pub fn by_index(index: u32) -> Option<Nsid> {
        INTERNER.read().list.get(index as usize).copied()
    }

    /// Snapshot of every interned NSID, in index order.
    pub fn all() -> Vec<Nsid> {
        INTERNER.read().list.clone()
    }
}

fn intern_global(name: &str) -> Nsid {
    if let Some(&n) = INTERNER.read().map.get(name) {
        return n;
    }
    let mut w = INTERNER.write();
    if let Some(&n) = w.map.get(name) {
        return n;
    }
    let name = if w.list.len() >= MAX_INTERNED - 1 {
        if let Some(&n) = w.map.get(OVERFLOW_NAME) {
            return n;
        }
        OVERFLOW_NAME
    } else {
        name
    };
    let entry: &'static NsidEntry =
        Box::leak(Box::new(NsidEntry { name: name.into(), index: w.list.len() as u32 }));
    let n = Nsid(entry);
    w.map.insert(&entry.name, n);
    w.list.push(n);
    n
}

impl PartialEq for Nsid {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}
impl Eq for Nsid {}

impl std::hash::Hash for Nsid {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u32(self.0.index)
    }
}

impl fmt::Debug for Nsid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl fmt::Display for Nsid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_stable() {
        let a = Nsid::intern("app.bsky.feed.post");
        let b = Nsid::intern("app.bsky.feed.post");
        let c = Nsid::intern("app.bsky.feed.like");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.as_str(), "app.bsky.feed.post");
        assert_eq!(Nsid::by_index(a.index()), Some(a));
        assert_eq!(Nsid::get("app.bsky.feed.post"), Some(a));
        assert_eq!(std::mem::size_of::<Option<Nsid>>(), 8);
    }

    #[test]
    fn interning_across_threads_agrees() {
        let a = Nsid::intern("test.threads.one");
        let b = std::thread::spawn(|| Nsid::intern("test.threads.one")).join().unwrap();
        assert_eq!(a, b);
    }
}
