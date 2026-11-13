//! The normalized event every source decodes into.
//!
//! An [`Event`] is a fixed-size header plus exactly one heap allocation (the
//! `blob`) holding the raw frame JSON and every extracted string back to back.
//! Filters read pre-extracted fields straight out of the blob; nothing is
//! re-parsed unless a JSON-path predicate asks for it. One allocation per
//! event keeps the ring buffer cheap to fill, evict, and scan in parallel.

use crate::did::Did;
use crate::nsid::Nsid;
use bitflags::bitflags;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Commit = 0,
    Identity = 1,
    Account = 2,
    Sync = 3,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Commit, Kind::Identity, Kind::Account, Kind::Sync];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Identity => "identity",
            Kind::Account => "account",
            Kind::Sync => "sync",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "commit" => Kind::Commit,
            "identity" => Kind::Identity,
            "account" => Kind::Account,
            "sync" => Kind::Sync,
            _ => return None,
        })
    }

    #[inline]
    pub fn mask(self) -> u8 {
        1 << self as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Op {
    None = 0,
    Create = 1,
    Update = 2,
    Delete = 3,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::None => "",
            Op::Create => "create",
            Op::Update => "update",
            Op::Delete => "delete",
        }
    }

    pub fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "create" => Op::Create,
            "update" => Op::Update,
            "delete" => Op::Delete,
            _ => return None,
        })
    }

    #[inline]
    pub fn mask(self) -> u8 {
        1 << self as u8
    }
}

bitflags! {
    /// Cheap boolean facts computed once at decode time.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Flags: u32 {
        /// Record has a `reply` reference.
        const REPLY    = 1 << 0;
        /// Embeds another record (quote post, starter pack, feed, list…).
        const QUOTE    = 1 << 1;
        const IMAGES   = 1 << 2;
        const VIDEO    = 1 << 3;
        /// External link card embed.
        const EXTERNAL = 1 << 4;
        /// Any link: facet link or external embed.
        const LINK     = 1 << 5;
        const MENTION  = 1 << 6;
        const TAG      = 1 << 7;
        /// Self-labels present.
        const LABELS   = 1 << 8;
        const LANGS    = 1 << 9;
        /// At least one non-empty alt text.
        const ALT      = 1 << 10;
        /// Any embed at all.
        const EMBED    = 1 << 11;
        /// Non-empty primary text.
        const TEXT     = 1 << 12;
        /// Commit carries a record body.
        const RECORD   = 1 << 13;
        /// Account event with `active: true`.
        const ACTIVE   = 1 << 14;
        /// Identity event carrying a handle.
        const HANDLE   = 1 << 15;
        /// External embed pointing at a GIF host.
        const GIF      = 1 << 16;
        const FACETS   = 1 << 17;
    }
}

/// String fields stored in the event blob, in storage order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Field {
    /// The frame exactly as received (after decompression/transcoding).
    Raw = 0,
    /// Primary text: post text, profile/list/feed description.
    Text,
    /// Display name / name / title.
    Name,
    Rkey,
    /// Target of a like/repost/follow/block/list item: an AT-URI or DID.
    Subject,
    /// Reply parent AT-URI.
    Parent,
    /// Reply root AT-URI.
    Root,
    /// Embedded (quoted) record AT-URI.
    Quote,
    /// `\n`-separated language tags.
    Langs,
    /// `\n`-separated lowercased hashtags.
    Tags,
    /// `\n`-separated mentioned DIDs.
    Mentions,
    /// `\n`-separated link URLs (facets and external embeds).
    Links,
    /// `\n`-separated self-label values.
    Labels,
    /// Alt texts, `\n`-joined.
    Alt,
    /// Identity handle or account status.
    Aux,
}

pub const FIELD_COUNT: usize = 15;

/// Separator for list-valued fields.
pub const LIST_SEP: char = '\n';

pub struct Event {
    /// Local monotonic id, assigned by the store.
    pub id: u64,
    /// Source cursor: Jetstream/firehose `seq` (v1: `time_us`).
    pub seq: i64,
    /// Event time in unix microseconds.
    pub time_us: i64,
    /// Record `createdAt` in unix microseconds, or 0.
    pub created_us: i64,
    pub did: Did,
    pub collection: Option<Nsid>,
    pub flags: Flags,
    pub kind: Kind,
    pub op: Op,
    pub n_images: u8,
    pub n_langs: u8,
    pub n_tags: u8,
    pub n_mentions: u8,
    pub n_links: u8,
    /// Characters in `Text`, saturating.
    pub text_chars: u16,
    /// Byte span of the record object inside `Raw` (start, len); len 0 if none.
    rec: (u32, u32),
    ends: [u32; FIELD_COUNT],
    blob: Box<[u8]>,
}

impl Event {
    #[inline]
    pub fn field(&self, f: Field) -> &str {
        let i = f as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        let end = self.ends[i] as usize;
        // SAFETY: the blob is only ever assembled by `EventBuilder::finish`
        // from `&str` pieces, and `ends` are the cumulative piece lengths, so
        // every span is in bounds and lies on UTF-8 boundaries.
        unsafe { std::str::from_utf8_unchecked(self.blob.get_unchecked(start..end)) }
    }

    #[inline]
    pub fn raw(&self) -> &str {
        self.field(Field::Raw)
    }

    #[inline]
    pub fn text(&self) -> &str {
        self.field(Field::Text)
    }

    /// Items of a list-valued field.
    #[inline]
    pub fn list(&self, f: Field) -> impl Iterator<Item = &str> {
        let s = self.field(f);
        s.split(LIST_SEP).filter(|p| !p.is_empty())
    }

    /// The record object's raw JSON, if the event has one.
    #[inline]
    pub fn record_json(&self) -> Option<&str> {
        if self.rec.1 == 0 {
            return None;
        }
        let (s, l) = (self.rec.0 as usize, self.rec.1 as usize);
        self.raw().get(s..s + l)
    }

    #[inline]
    pub fn collection_str(&self) -> &'static str {
        self.collection.map(Nsid::as_str).unwrap_or("")
    }

    /// `at://did/collection/rkey` for commits.
    pub fn at_uri(&self) -> Option<String> {
        let c = self.collection?;
        let rkey = self.field(Field::Rkey);
        let mut s = String::with_capacity(8 + self.did.str_len() + c.as_str().len() + rkey.len());
        s.push_str("at://");
        self.did.push_to(&mut s);
        s.push('/');
        s.push_str(c.as_str());
        s.push('/');
        s.push_str(rkey);
        Some(s)
    }

    /// Approximate heap + inline footprint in bytes.
    #[inline]
    pub fn mem_size(&self) -> usize {
        std::mem::size_of::<Event>() + self.blob.len()
    }
}

impl Clone for Event {
    fn clone(&self) -> Self {
        Event {
            id: self.id,
            seq: self.seq,
            time_us: self.time_us,
            created_us: self.created_us,
            did: self.did.clone(),
            collection: self.collection,
            flags: self.flags,
            kind: self.kind,
            op: self.op,
            n_images: self.n_images,
            n_langs: self.n_langs,
            n_tags: self.n_tags,
            n_mentions: self.n_mentions,
            n_links: self.n_links,
            text_chars: self.text_chars,
            rec: self.rec,
            ends: self.ends,
            blob: self.blob.clone(),
        }
    }
}

impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Event")
            .field("id", &self.id)
            .field("seq", &self.seq)
            .field("kind", &self.kind)
            .field("op", &self.op)
            .field("did", &self.did)
            .field("collection", &self.collection)
            .field("rkey", &self.field(Field::Rkey))
            .field("flags", &self.flags)
            .field("text", &self.text())
            .finish()
    }
}

/// Reusable scratch state for assembling events without per-field allocation.
pub struct EventBuilder {
    pub seq: i64,
    pub time_us: i64,
    /// When the serving instance witnessed the event (portable cursor), or 0.
    pub witnessed_us: i64,
    pub created_us: i64,
    pub did: Option<Did>,
    pub collection: Option<Nsid>,
    pub kind: Option<Kind>,
    pub op: Op,
    pub flags: Flags,
    pub n_images: u8,
    pub n_langs: u8,
    pub n_tags: u8,
    pub n_mentions: u8,
    pub n_links: u8,
    pub rec: (u32, u32),
    text_rank: u8,
    name_rank: u8,
    /// Indexed by `Field as usize`; index 0 (Raw) is unused.
    fields: [String; FIELD_COUNT],
}

impl Default for EventBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBuilder {
    pub fn new() -> Self {
        EventBuilder {
            seq: 0,
            time_us: 0,
            witnessed_us: 0,
            created_us: 0,
            did: None,
            collection: None,
            kind: None,
            op: Op::None,
            flags: Flags::empty(),
            n_images: 0,
            n_langs: 0,
            n_tags: 0,
            n_mentions: 0,
            n_links: 0,
            rec: (0, 0),
            text_rank: 0,
            name_rank: 0,
            fields: Default::default(),
        }
    }

    pub fn reset(&mut self) {
        self.seq = 0;
        self.time_us = 0;
        self.witnessed_us = 0;
        self.created_us = 0;
        self.did = None;
        self.collection = None;
        self.kind = None;
        self.op = Op::None;
        self.flags = Flags::empty();
        self.n_images = 0;
        self.n_langs = 0;
        self.n_tags = 0;
        self.n_mentions = 0;
        self.n_links = 0;
        self.rec = (0, 0);
        self.text_rank = 0;
        self.name_rank = 0;
        for f in &mut self.fields {
            f.clear();
        }
    }

    /// Mutable access to a scalar string field's buffer.
    #[inline]
    pub fn buf(&mut self, f: Field) -> &mut String {
        debug_assert!(f != Field::Raw);
        &mut self.fields[f as usize]
    }

    #[inline]
    pub fn get(&self, f: Field) -> &str {
        &self.fields[f as usize]
    }

    /// Buffer for the primary text if `rank` beats what is already there
    /// (so `text` wins over `description` regardless of key order).
    #[inline]
    pub fn text_slot(&mut self, rank: u8) -> Option<&mut String> {
        if rank > self.text_rank {
            self.text_rank = rank;
            let b = &mut self.fields[Field::Text as usize];
            b.clear();
            Some(b)
        } else {
            None
        }
    }

    #[inline]
    pub fn name_slot(&mut self, rank: u8) -> Option<&mut String> {
        if rank > self.name_rank {
            self.name_rank = rank;
            let b = &mut self.fields[Field::Name as usize];
            b.clear();
            Some(b)
        } else {
            None
        }
    }

    /// Begin a new item in a list field and return the buffer to append to.
    /// Call [`EventBuilder::end_item`] afterwards.
    #[inline]
    pub fn begin_item(&mut self, f: Field) -> (usize, &mut String) {
        let b = &mut self.fields[f as usize];
        if !b.is_empty() {
            b.push(LIST_SEP);
        }
        (b.len(), b)
    }

    /// Finish the item started at `start`: neutralise embedded separators and
    /// drop the item if it turned out empty. Returns whether an item was kept.
    pub fn end_item(&mut self, f: Field, start: usize, lowercase: bool) -> bool {
        let b = &mut self.fields[f as usize];
        if b.len() == start {
            if start > 0 {
                b.pop();
            }
            return false;
        }
        let item = &b[start..];
        let needs_sep_fix = item.contains(LIST_SEP);
        let needs_lower = lowercase && item.chars().any(|c| c.is_uppercase());
        if needs_sep_fix || needs_lower {
            let fixed: String = item
                .chars()
                .map(|c| if c == LIST_SEP { ' ' } else { c })
                .flat_map(|c| {
                    let (a, b) = if lowercase {
                        let mut it = c.to_lowercase();
                        (it.next(), it.next())
                    } else {
                        (Some(c), None)
                    };
                    a.into_iter().chain(b)
                })
                .collect();
            b.truncate(start);
            b.push_str(&fixed);
        }
        true
    }

    /// Assemble the event. `raw` is the frame text; the builder is left ready
    /// for [`EventBuilder::reset`].
    pub fn finish(&mut self, raw: &str) -> Option<Event> {
        let did = self.did.take()?;
        let kind = self.kind?;
        let text = &self.fields[Field::Text as usize];
        let text_chars = text.chars().count().min(u16::MAX as usize) as u16;
        if !text.is_empty() {
            self.flags |= Flags::TEXT;
        }

        let total = raw.len() + self.fields[1..].iter().map(String::len).sum::<usize>();
        if total > u32::MAX as usize {
            return None;
        }
        let mut blob = Vec::with_capacity(total);
        let mut ends = [0u32; FIELD_COUNT];
        blob.extend_from_slice(raw.as_bytes());
        ends[0] = blob.len() as u32;
        for i in 1..FIELD_COUNT {
            blob.extend_from_slice(self.fields[i].as_bytes());
            ends[i] = blob.len() as u32;
        }
        debug_assert_eq!(blob.len(), total);

        Some(Event {
            id: 0,
            seq: self.seq,
            time_us: self.time_us,
            created_us: self.created_us,
            did,
            collection: self.collection,
            flags: self.flags,
            kind,
            op: self.op,
            n_images: self.n_images,
            n_langs: self.n_langs,
            n_tags: self.n_tags,
            n_mentions: self.n_mentions,
            n_links: self.n_links,
            text_chars,
            rec: self.rec,
            ends,
            blob: blob.into_boxed_slice(),
        })
    }
}

/// The DID portion of an AT-URI (`at://did/...`) or of a bare DID.
pub fn uri_did(s: &str) -> &str {
    let rest = s.strip_prefix("at://").unwrap_or(s);
    rest.split('/').next().unwrap_or("")
}

/// The host of a URL, without scheme, userinfo, port, or path.
pub fn url_host(url: &str) -> &str {
    let rest = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority.rsplit('@').next().unwrap_or(authority);
    host.split(':').next().unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_assembles_fields() {
        let mut b = EventBuilder::new();
        b.did = Some(Did::parse("did:plc:mcc5yghbk7tixqryac2yxku7"));
        b.kind = Some(Kind::Commit);
        b.op = Op::Create;
        b.collection = Some(Nsid::intern("app.bsky.feed.post"));
        b.buf(Field::Rkey).push_str("3abc");
        b.text_slot(1).unwrap().push_str("desc");
        b.text_slot(2).unwrap().push_str("héllo");
        assert!(b.text_slot(1).is_none());
        for t in ["Rust", "", "ÉCOLE", "a\nb"] {
            let (start, buf) = b.begin_item(Field::Tags);
            buf.push_str(t);
            b.end_item(Field::Tags, start, true);
        }
        let raw = r#"{"x":{"record":{"a":1}}}"#;
        b.rec = (15, 7);
        let e = b.finish(raw).unwrap();
        assert_eq!(e.raw(), raw);
        assert_eq!(e.text(), "héllo");
        assert_eq!(e.text_chars, 5);
        assert!(e.flags.contains(Flags::TEXT));
        assert_eq!(e.field(Field::Rkey), "3abc");
        assert_eq!(e.list(Field::Tags).collect::<Vec<_>>(), ["rust", "école", "a b"]);
        assert_eq!(e.field(Field::Aux), "");
        assert_eq!(e.record_json(), Some(r#"{"a":1}"#));
        assert_eq!(e.at_uri().unwrap(), "at://did:plc:mcc5yghbk7tixqryac2yxku7/app.bsky.feed.post/3abc");
        let c = e.clone();
        assert_eq!(c.text(), e.text());
    }

    #[test]
    fn builder_requires_did_and_kind() {
        let mut b = EventBuilder::new();
        assert!(b.finish("{}").is_none());
        b.did = Some(Did::parse("did:web:x"));
        assert!(b.finish("{}").is_none());
    }

    #[test]
    fn helpers() {
        assert_eq!(uri_did("at://did:plc:abc/app.bsky.feed.post/3k"), "did:plc:abc");
        assert_eq!(uri_did("did:plc:abc"), "did:plc:abc");
        assert_eq!(url_host("https://Sub.Example.com:8443/a?b#c"), "Sub.Example.com");
        assert_eq!(url_host("https://user:pw@example.com"), "example.com");
        assert_eq!(url_host("example.com/path"), "example.com");
    }

    #[test]
    fn header_stays_compact() {
        assert!(std::mem::size_of::<Event>() <= 176, "{}", std::mem::size_of::<Event>());
    }
}
