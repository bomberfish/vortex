//! Derive the server-side filter implied by a query.
//!
//! Jetstream filters by kind, collection and DID before anything is sent. A
//! query's pushdown is the tightest such filter that is still a *necessary
//! condition* of the query — everything the local filter could match still
//! arrives, while bandwidth and decode work drop by orders of magnitude for
//! narrow queries. The local filter always runs on what does arrive, so an
//! over-wide pushdown is merely slower, never wrong.

use super::node::{DidSet, Node};
use crate::event::Kind;
use std::collections::BTreeSet;

/// Jetstream's caps (`subscribeEvents` lexicon).
const MAX_COLLECTIONS: usize = 100;
const MAX_DIDS: usize = 10_000;

const COMMIT: u8 = 1 << Kind::Commit as u8;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pushdown {
    /// `None`: every kind.
    pub kinds: Option<Vec<Kind>>,
    /// Exact NSIDs or `prefix.*`. `None`: every collection.
    pub collections: Option<Vec<String>>,
    /// `None`: every repo.
    pub dids: Option<Vec<String>>,
}

impl Pushdown {
    pub fn is_empty(&self) -> bool {
        self.kinds.is_none() && self.collections.is_none() && self.dids.is_none()
    }

    /// Short human summary, e.g. `commit · 2 collections · 1 did`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(k) = &self.kinds {
            parts.push(k.iter().map(|k| k.as_str()).collect::<Vec<_>>().join("+"));
        }
        if let Some(c) = &self.collections {
            parts.push(match c.as_slice() {
                [one] => one.clone(),
                many => format!("{} collections", many.len()),
            });
        }
        if let Some(d) = &self.dids {
            parts.push(if d.len() == 1 { "1 did".into() } else { format!("{} dids", d.len()) });
        }
        parts.join(" · ")
    }
}

fn valid_nsid_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Conservative syntax check so we never send something the server rejects
/// (a rejected filter fails the whole connection).
fn valid_collection_pattern(p: &str) -> bool {
    let (body, wildcard) = match p.strip_suffix(".*") {
        Some(prefix) => (prefix, true),
        None => (p, false),
    };
    let segments: Vec<&str> = body.split('.').collect();
    let min = if wildcard { 2 } else { 3 };
    p.len() <= 317
        && segments.len() >= min
        && segments.iter().all(|s| valid_nsid_segment(s))
        && (wildcard || segments.last().is_some_and(|s| s.bytes().all(|b| b.is_ascii_alphanumeric())))
}

fn valid_did(d: &str) -> bool {
    let mut parts = d.splitn(3, ':');
    parts.next() == Some("did")
        && parts.next().is_some_and(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_lowercase()))
        && parts.next().is_some_and(|id| {
            !id.is_empty()
                && id.len() <= 2000
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'%' | b'-'))
                && !id.ends_with([':', '%'])
        })
}

/// Collections every matching *commit* must belong to; `None` = unconstrained.
/// A node that cannot match commits at all constrains them vacuously.
fn required_collections(n: &Node) -> Option<BTreeSet<String>> {
    if n.kinds() & COMMIT == 0 {
        return Some(BTreeSet::new());
    }
    match n {
        Node::Collection(set) if set.regexes.is_empty() => Some(set.patterns.iter().map(|p| p.to_string()).collect()),
        Node::And(children) => children.iter().filter_map(required_collections).min_by_key(BTreeSet::len),
        Node::Or(children) => {
            let mut all = BTreeSet::new();
            for c in children.iter() {
                all.extend(required_collections(c)?);
            }
            Some(all)
        }
        _ => None,
    }
}

/// DIDs every matching event must come from; `None` = unconstrained.
fn required_dids(n: &Node) -> Option<BTreeSet<String>> {
    match n {
        Node::False => Some(BTreeSet::new()),
        Node::Did(set) => Some(match set {
            DidSet::One(d) => std::iter::once(d.to_string()).collect(),
            DidSet::Many(_) => set.iter().map(|d| d.to_string()).collect(),
        }),
        Node::And(children) => {
            // Each child's set is necessary, so their intersection is too.
            let mut acc: Option<BTreeSet<String>> = None;
            for set in children.iter().filter_map(required_dids) {
                acc = Some(match acc {
                    Some(a) => a.intersection(&set).cloned().collect(),
                    None => set,
                });
            }
            acc
        }
        Node::Or(children) => {
            let mut all = BTreeSet::new();
            for c in children.iter() {
                all.extend(required_dids(c)?);
            }
            Some(all)
        }
        _ => None,
    }
}

pub(crate) fn derive(root: &Node) -> Pushdown {
    let mask = root.kinds();
    // A query that can match nothing gets no pushdown: there is no valid way
    // to ask the server for "nothing", and it will be obvious locally.
    if mask == 0 {
        return Pushdown::default();
    }
    let kinds = (mask != super::node::ALL_KINDS)
        .then(|| Kind::ALL.into_iter().filter(|k| mask & k.mask() != 0).collect::<Vec<_>>());

    let collections = required_collections(root)
        // The server rejects a collection filter when kinds excludes commit.
        .filter(|_| mask & COMMIT != 0)
        .filter(|set| !set.is_empty() && set.len() <= MAX_COLLECTIONS)
        .filter(|set| set.iter().all(|p| valid_collection_pattern(p)))
        .map(|set| set.into_iter().collect());

    let dids = required_dids(root)
        .map(|set| set.into_iter().filter(|d| valid_did(d)).collect::<Vec<_>>())
        .filter(|list| !list.is_empty() && list.len() <= MAX_DIDS);

    Pushdown { kinds, collections, dids }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_pattern_validation() {
        for ok in ["app.bsky.feed.post", "app.bsky.*", "app.bsky.feed.*", "com.example.fooBar", "a-b.c.d"] {
            assert!(valid_collection_pattern(ok), "{ok}");
        }
        for bad in ["post", "app.*", "app.bsky", "app..post", "app.bsky.feed.po st", "*.post", "app.bsky.*.post", ""] {
            assert!(!valid_collection_pattern(bad), "{bad}");
        }
    }

    #[test]
    fn did_validation() {
        for ok in ["did:plc:z72i7hdynmk6r22z27h6tvur", "did:web:example.com", "did:web:localhost%3A8080"] {
            assert!(valid_did(ok), "{ok}");
        }
        for bad in ["did:plc:", "plc:abc", "did::abc", "did:PLC:abc", "did:web:a b", "did:web:x:"] {
            assert!(!valid_did(bad), "{bad}");
        }
    }
}
