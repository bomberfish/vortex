//! Network sources: where frames come from and how to ask for them.

mod http;
mod session;

pub(crate) use session::{run, Cmd, Ctx};

use crate::decode::Wire;
use crate::filter::Pushdown;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// Jetstream `network.bsky.jetstream.subscribeEvents` (recommended).
    JetstreamV2,
    /// Jetstream legacy `/subscribe`.
    JetstreamV1,
    /// Relay firehose `com.atproto.sync.subscribeRepos`.
    Firehose,
}

impl SourceKind {
    pub const ALL: [SourceKind; 3] = [SourceKind::JetstreamV2, SourceKind::JetstreamV1, SourceKind::Firehose];

    pub fn wire(self) -> Wire {
        match self {
            SourceKind::JetstreamV2 => Wire::JetstreamV2,
            SourceKind::JetstreamV1 => Wire::JetstreamV1,
            SourceKind::Firehose => Wire::Firehose,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SourceKind::JetstreamV2 => "jetstream",
            SourceKind::JetstreamV1 => "jetstream-v1",
            SourceKind::Firehose => "firehose",
        }
    }

    pub fn parse(s: &str) -> Option<SourceKind> {
        Some(match s {
            "jetstream" | "jetstream-v2" | "v2" => SourceKind::JetstreamV2,
            "jetstream-v1" | "v1" => SourceKind::JetstreamV1,
            "firehose" | "relay" => SourceKind::Firehose,
            _ => return None,
        })
    }

    pub fn default_hosts(self) -> &'static [&'static str] {
        match self {
            SourceKind::JetstreamV2 => &["jetstream.us-east.bsky.network", "jetstream.us-west.bsky.network"],
            SourceKind::JetstreamV1 => &["jetstream2.us-east.bsky.network", "jetstream1.us-west.bsky.network"],
            SourceKind::Firehose => &["relay1.us-east.bsky.network", "relay1.us-west.bsky.network", "bsky.network"],
        }
    }

    /// Whether the server can filter for us.
    pub fn supports_pushdown(self) -> bool {
        !matches!(self, SourceKind::Firehose)
    }

    /// Whether dictionary-zstd compression is available.
    pub fn supports_compression(self) -> bool {
        !matches!(self, SourceKind::Firehose)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartFrom {
    /// The live tip.
    Live,
    /// This far in the past (Jetstream only; bounded by server retention).
    Rewind(Duration),
    /// An explicit cursor: a seq, or unix microseconds for Jetstream.
    Cursor(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceConfig {
    pub kind: SourceKind,
    /// Hosts tried in order on failure. A bare hostname means `wss://`;
    /// `ws://host:port` works for self-hosted instances.
    pub hosts: Vec<String>,
    pub compress: bool,
    pub start: StartFrom,
}

impl SourceConfig {
    pub fn new(kind: SourceKind) -> Self {
        SourceConfig {
            kind,
            hosts: kind.default_hosts().iter().map(|h| h.to_string()).collect(),
            compress: kind.supports_compression(),
            start: StartFrom::Live,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Idle,
    Connecting,
    Live,
    /// Waiting to retry after a failure.
    Waiting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnStatus {
    pub state: ConnState,
    pub host: String,
    pub error: Option<String>,
    /// Summary of the server-side filter in effect, empty if none.
    pub server_filter: String,
    pub attempt: u32,
}

impl Default for ConnStatus {
    fn default() -> Self {
        ConnStatus { state: ConnState::Idle, host: String::new(), error: None, server_filter: String::new(), attempt: 0 }
    }
}

#[derive(Default)]
pub(crate) struct StatusCell {
    status: Mutex<ConnStatus>,
    version: AtomicU64,
}

impl StatusCell {
    pub(crate) fn get(&self) -> ConnStatus {
        self.status.lock().clone()
    }

    pub(crate) fn update(&self, f: impl FnOnce(&mut ConnStatus)) {
        f(&mut self.status.lock());
        self.version.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

/// Longest query string we are willing to send. Proxies commonly cap request
/// lines around 8 KiB; a filter that does not fit is applied locally instead.
const MAX_QUERY_BYTES: usize = 6_000;

fn push_param(q: &mut String, key: &str, value: &str) {
    q.push(if q.is_empty() { '?' } else { '&' });
    q.push_str(key);
    q.push('=');
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' | b'*' => q.push(b as char),
            _ => {
                q.push('%');
                q.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0').to_ascii_uppercase());
                q.push(char::from_digit((b & 15) as u32, 16).unwrap_or('0').to_ascii_uppercase());
            }
        }
    }
}

/// What was actually asked of the server (pushdown may be partially dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    /// The part of the pushdown that made it into the URL.
    pub applied: Pushdown,
}

/// Build the subscription URL for a source.
pub fn build_request(
    kind: SourceKind,
    host: &str,
    cursor: Option<i64>,
    pushdown: &Pushdown,
    zstd_dict_id: Option<u32>,
) -> Request {
    let base = if host.contains("://") { host.trim_end_matches('/').to_owned() } else { format!("wss://{host}") };
    let mut q = String::new();
    let mut applied = Pushdown::default();
    match kind {
        SourceKind::JetstreamV2 | SourceKind::JetstreamV1 => {
            let v2 = kind == SourceKind::JetstreamV2;
            let (coll_key, did_key) = if v2 { ("collections", "dids") } else { ("wantedCollections", "wantedDids") };
            if v2 {
                if let Some(kinds) = &pushdown.kinds {
                    for k in kinds {
                        push_param(&mut q, "kinds", k.as_str());
                    }
                    applied.kinds = Some(kinds.clone());
                }
            }
            if let Some(colls) = &pushdown.collections {
                for c in colls {
                    push_param(&mut q, coll_key, c);
                }
                applied.collections = Some(colls.clone());
            }
            if let Some(dids) = &pushdown.dids {
                // All or nothing: a partial DID list would drop wanted events.
                let mut with_dids = q.clone();
                for d in dids {
                    push_param(&mut with_dids, did_key, d);
                }
                if with_dids.len() <= MAX_QUERY_BYTES {
                    q = with_dids;
                    applied.dids = Some(dids.clone());
                }
            }
            if let Some(c) = cursor {
                push_param(&mut q, "cursor", &c.to_string());
            }
            match (v2, zstd_dict_id) {
                (true, Some(id)) => push_param(&mut q, "zstdDictionary", &id.to_string()),
                (false, Some(_)) => push_param(&mut q, "compress", "true"),
                _ => {}
            }
            let path = if v2 { "/xrpc/network.bsky.jetstream.subscribeEvents" } else { "/subscribe" };
            Request { url: format!("{base}{path}{q}"), applied }
        }
        SourceKind::Firehose => {
            if let Some(c) = cursor {
                push_param(&mut q, "cursor", &c.to_string());
            }
            Request { url: format!("{base}/xrpc/com.atproto.sync.subscribeRepos{q}"), applied }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Kind;

    fn pd(kinds: Option<Vec<Kind>>, colls: Option<&[&str]>, dids: Option<Vec<String>>) -> Pushdown {
        Pushdown { kinds, collections: colls.map(|c| c.iter().map(|s| s.to_string()).collect()), dids }
    }

    #[test]
    fn v2_urls() {
        let r = build_request(SourceKind::JetstreamV2, "jetstream.us-east.bsky.network", None, &Pushdown::default(), None);
        assert_eq!(r.url, "wss://jetstream.us-east.bsky.network/xrpc/network.bsky.jetstream.subscribeEvents");
        assert!(r.applied.is_empty());

        let p = pd(Some(vec![Kind::Commit]), Some(&["app.bsky.feed.post", "app.bsky.graph.*"]), Some(vec!["did:plc:abc".into()]));
        let r = build_request(SourceKind::JetstreamV2, "h", Some(42), &p, Some(20260811));
        assert_eq!(
            r.url,
            "wss://h/xrpc/network.bsky.jetstream.subscribeEvents?kinds=commit&collections=app.bsky.feed.post&collections=app.bsky.graph.*&dids=did:plc:abc&cursor=42&zstdDictionary=20260811"
        );
        assert_eq!(r.applied, p);
    }

    #[test]
    fn v1_and_firehose_urls() {
        let p = pd(Some(vec![Kind::Commit]), Some(&["app.bsky.feed.like"]), Some(vec!["did:web:a%3Ab".into()]));
        let r = build_request(SourceKind::JetstreamV1, "ws://localhost:6008/", Some(1_700_000_000_000_000), &p, Some(1));
        assert_eq!(
            r.url,
            "ws://localhost:6008/subscribe?wantedCollections=app.bsky.feed.like&wantedDids=did:web:a%253Ab&cursor=1700000000000000&compress=true"
        );
        // v1 has no kinds parameter.
        assert_eq!(r.applied.kinds, None);

        let r = build_request(SourceKind::Firehose, "bsky.network", Some(7), &p, None);
        assert_eq!(r.url, "wss://bsky.network/xrpc/com.atproto.sync.subscribeRepos?cursor=7");
        assert!(r.applied.is_empty());
    }

    #[test]
    fn oversized_did_lists_fall_back_to_local_filtering() {
        let dids: Vec<String> = (0..5000).map(|i| format!("did:plc:{i:024}")).collect();
        let p = pd(None, Some(&["app.bsky.feed.post"]), Some(dids));
        let r = build_request(SourceKind::JetstreamV2, "h", None, &p, None);
        assert!(r.url.len() < 200, "{}", r.url.len());
        assert_eq!(r.applied.dids, None);
        assert!(r.applied.collections.is_some());

        let few: Vec<String> = (0..100).map(|i| format!("did:plc:{i:024}")).collect();
        let r = build_request(SourceKind::JetstreamV2, "h", None, &pd(None, None, Some(few.clone())), None);
        assert_eq!(r.applied.dids, Some(few));
    }

    #[test]
    fn source_kind_round_trips() {
        for k in SourceKind::ALL {
            assert_eq!(SourceKind::parse(k.label()), Some(k));
            assert!(!k.default_hosts().is_empty());
        }
    }
}
