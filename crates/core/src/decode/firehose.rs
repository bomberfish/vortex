//! `com.atproto.sync.subscribeRepos` frame → [`Event`]s.
//!
//! A firehose frame is two concatenated DAG-CBOR items: a header
//! (`{op, t}`) and a payload. Commit payloads carry their records as blocks
//! of an embedded CAR file, addressed by CID from the `ops` list.
//!
//! Rather than maintaining a second extraction path over CBOR, each op is
//! transcoded into the same JSON shape Jetstream v2 emits and handed to the
//! shared JSON frame decoder. That keeps one definition of every extracted
//! field and gives firehose events an inspectable, filterable JSON form.
//!
//! The JSON is assembled as bytes and checked for UTF-8 once per event,
//! instead of once per CBOR string: everything outside the record comes from
//! strings that were validated when they were read, and the record transcoder
//! only ever adds ASCII around the text it copies, so the event is valid
//! exactly when every string in its record is.

use super::car::Car;
use super::cbor::{check_utf8, cid_to_bytes, push_i64, Reader};
use super::frame::decode_json_frame;
use super::{DecodeError, Decoded, Notice, NoticeLevel};
use crate::event::EventBuilder;
use crate::json::write_string_bytes;

const TYPE_PREFIX: &str = "com.atproto.sync.subscribeRepos";

#[derive(Clone, Copy)]
struct RepoOp<'a> {
    action: &'a str,
    path: &'a str,
    cid: Option<&'a [u8]>,
}

/// Ops kept on the stack; almost every commit has exactly one.
const INLINE_OPS: usize = 4;

/// The ops of one commit, without a heap allocation in the common case.
struct Ops<'a> {
    inline: [RepoOp<'a>; INLINE_OPS],
    len: usize,
    spill: Vec<RepoOp<'a>>,
}

impl<'a> Ops<'a> {
    fn new() -> Self {
        Ops { inline: [RepoOp { action: "", path: "", cid: None }; INLINE_OPS], len: 0, spill: Vec::new() }
    }

    fn push(&mut self, op: RepoOp<'a>) {
        match self.inline.get_mut(self.len) {
            Some(slot) => *slot = op,
            None => self.spill.push(op),
        }
        self.len += 1;
    }

    fn iter(&self) -> impl Iterator<Item = &RepoOp<'a>> {
        self.inline[..self.len.min(INLINE_OPS)].iter().chain(&self.spill)
    }
}

/// Open `{"$type":"message","payload":{"$type":"…#kind","did":…,"seq":…,"time":…`
fn open_frame(out: &mut Vec<u8>, kind: &str, did: &str, seq: i64, time: &str) {
    out.clear();
    out.extend_from_slice(b"{\"$type\":\"message\",\"payload\":{\"$type\":\"");
    out.extend_from_slice(TYPE_PREFIX.as_bytes());
    out.push(b'#');
    out.extend_from_slice(kind.as_bytes());
    out.extend_from_slice(b"\",\"did\":");
    write_string_bytes(out, did.as_bytes());
    out.extend_from_slice(b",\"seq\":");
    push_i64(out, seq);
    out.extend_from_slice(b",\"time\":");
    write_string_bytes(out, time.as_bytes());
}

/// Decode one firehose frame, appending its events and notices to `out`.
/// `json` is scratch space for the transcoded event, reused across calls.
pub fn decode_firehose_frame(
    frame: &[u8],
    b: &mut EventBuilder,
    json: &mut Vec<u8>,
    out: &mut Vec<Decoded>,
) -> Result<(), DecodeError> {
    let mut r = Reader::new(frame);

    // Header: {op: 1 | -1, t: "#commit" | …}
    let Some(n) = r.map()? else {
        return Err(DecodeError::Cbor("frame header is not a map"));
    };
    let mut op = 1;
    let mut t = "";
    for _ in 0..n {
        // Keys are matched as bytes; only the ones that are not recognised
        // still need the UTF-8 check a text key is owed.
        match r.key_bytes()? {
            b"op" => op = r.int()?.unwrap_or(1),
            b"t" => t = r.str()?.unwrap_or(""),
            other => {
                check_utf8(other)?;
                r.skip()?
            }
        }
    }

    if op == -1 {
        let mut name = String::new();
        let mut message = String::new();
        if let Some(n) = r.map()? {
            for _ in 0..n {
                match r.key()? {
                    "error" => name = r.str()?.unwrap_or("").to_owned(),
                    "message" => message = r.str()?.unwrap_or("").to_owned(),
                    _ => r.skip()?,
                }
            }
        }
        out.push(Decoded::Notice(Notice { level: NoticeLevel::Error, name, message }));
        return Ok(());
    }

    match t {
        "#commit" => decode_commit(&mut r, b, json, out),
        "#identity" => decode_wrapped(&mut r, "identity", b, json, out),
        "#account" => decode_wrapped(&mut r, "account", b, json, out),
        "#sync" => decode_wrapped(&mut r, "sync", b, json, out),
        "#info" => {
            let mut name = String::new();
            let mut message = String::new();
            if let Some(n) = r.map()? {
                for _ in 0..n {
                    match r.key()? {
                        "name" => name = r.str()?.unwrap_or("").to_owned(),
                        "message" => message = r.str()?.unwrap_or("").to_owned(),
                        _ => r.skip()?,
                    }
                }
            }
            out.push(Decoded::Notice(Notice { level: NoticeLevel::Info, name, message }));
            Ok(())
        }
        // Unknown message types must be ignored per the event stream spec.
        _ => Ok(()),
    }
}

fn decode_commit(
    r: &mut Reader,
    b: &mut EventBuilder,
    json: &mut Vec<u8>,
    out: &mut Vec<Decoded>,
) -> Result<(), DecodeError> {
    let Some(n) = r.map()? else {
        return Err(DecodeError::Cbor("commit payload is not a map"));
    };
    let mut seq = 0;
    let mut repo = "";
    let mut rev = "";
    let mut time = "";
    let mut blocks: &[u8] = &[];
    let mut ops = Ops::new();
    for _ in 0..n {
        match r.key_bytes()? {
            b"seq" => seq = r.int()?.unwrap_or(0),
            b"repo" => repo = r.str()?.unwrap_or(""),
            b"rev" => rev = r.str()?.unwrap_or(""),
            b"time" => time = r.str()?.unwrap_or(""),
            b"blocks" => blocks = r.bytes()?.unwrap_or(&[]),
            b"ops" => {
                if let Some(count) = r.array()? {
                    for _ in 0..count {
                        let Some(fields) = r.map()? else { continue };
                        let mut op = RepoOp { action: "", path: "", cid: None };
                        for _ in 0..fields {
                            match r.key_bytes()? {
                                b"action" => op.action = r.str()?.unwrap_or(""),
                                b"path" => op.path = r.str()?.unwrap_or(""),
                                b"cid" => op.cid = r.cid()?,
                                other => {
                                    check_utf8(other)?;
                                    r.skip()?
                                }
                            }
                        }
                        ops.push(op);
                    }
                }
            }
            other => {
                check_utf8(other)?;
                r.skip()?
            }
        }
    }
    if repo.is_empty() {
        return Err(DecodeError::Malformed("commit has no repo"));
    }

    // A commit without usable blocks (tooBig, or a malformed archive) still
    // yields its ops, just without record bodies.
    let car = if blocks.is_empty() { None } else { Car::parse(blocks).ok() };

    let mut first_err = None;
    for op in ops.iter() {
        let Some((collection, rkey)) = op.path.split_once('/') else { continue };
        open_frame(json, "commit", repo, seq, time);
        json.extend_from_slice(b",\"operation\":");
        write_string_bytes(json, op.action.as_bytes());
        json.extend_from_slice(b",\"collection\":");
        write_string_bytes(json, collection.as_bytes());
        json.extend_from_slice(b",\"rkey\":");
        write_string_bytes(json, rkey.as_bytes());
        json.extend_from_slice(b",\"rev\":");
        write_string_bytes(json, rev.as_bytes());
        // Where the record starts, so it can be cut off again.
        let mut record_at = None;
        if let Some(cid) = op.cid {
            json.extend_from_slice(b",\"cid\":\"");
            cid_to_bytes(cid, json);
            json.push(b'"');
            if let Some(block) = car.as_ref().and_then(|c| c.get(cid)) {
                let mark = json.len();
                json.extend_from_slice(b",\"record\":");
                // A record that fails to transcode is dropped, not the op.
                if Reader::new(block).to_json_unvalidated(json).is_ok() {
                    record_at = Some(mark);
                } else {
                    json.truncate(mark);
                }
            }
        }
        json.extend_from_slice(b"}}");
        let decoded = match std::str::from_utf8(json) {
            Ok(text) => decode_json_frame(text, b),
            // Only the record can hold unvalidated text, and a record with a
            // string that is not UTF-8 has failed to transcode.
            Err(_) => match record_at {
                Some(mark) => {
                    json.truncate(mark);
                    json.extend_from_slice(b"}}");
                    match std::str::from_utf8(json) {
                        Ok(text) => decode_json_frame(text, b),
                        Err(_) => Err(DecodeError::Utf8),
                    }
                }
                None => Err(DecodeError::Utf8),
            },
        };
        match decoded {
            Ok(d) => out.push(d),
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `#identity`, `#account` and `#sync`: wrap the payload verbatim under a key
/// named after the kind, exactly as Jetstream v2 does.
fn decode_wrapped(
    r: &mut Reader,
    kind: &str,
    b: &mut EventBuilder,
    json: &mut Vec<u8>,
    out: &mut Vec<Decoded>,
) -> Result<(), DecodeError> {
    // First pass: pull the envelope fields; second pass: transcode the map.
    let start = r.pos();
    let Some(n) = r.map()? else {
        return Err(DecodeError::Cbor("payload is not a map"));
    };
    let mut seq = 0;
    let mut did = "";
    let mut time = "";
    for _ in 0..n {
        match r.key()? {
            "seq" => seq = r.int()?.unwrap_or(0),
            "did" => did = r.str()?.unwrap_or(""),
            "time" => time = r.str()?.unwrap_or(""),
            _ => r.skip()?,
        }
    }
    let end = r.pos();
    if did.is_empty() {
        return Err(DecodeError::Malformed("event has no did"));
    }
    open_frame(json, kind, did, seq, time);
    json.extend_from_slice(b",\"");
    json.extend_from_slice(kind.as_bytes());
    json.extend_from_slice(b"\":");
    // These events are rare, so they take the self-validating transcoder and
    // report its exact error. The envelope so far is built from `&str`s, so
    // borrowing the scratch buffer as a `String` cannot fail.
    let mut text = String::from_utf8(std::mem::take(json)).unwrap_or_default();
    let mut again = Reader::new(r.slice(start, end));
    let decoded = again.to_json(&mut text).and_then(|()| {
        text.push_str("}}");
        decode_json_frame(&text, b)
    });
    *json = text.into_bytes();
    out.push(decoded?);
    Ok(())
}

/// The decoder exactly as it was before the byte-oriented rewrite: a `String`
/// scratch buffer, per-string UTF-8 validation, a heap-allocated op list and
/// the reference transcoder. Kept as the oracle for the differential tests.
#[cfg(test)]
mod reference {
    use super::super::car::Car;
    use super::super::cbor::{cid_to_string, reference::to_json, Reader};
    use super::super::frame::decode_json_frame;
    use super::super::{DecodeError, Decoded, Notice, NoticeLevel};
    use super::TYPE_PREFIX;
    use crate::event::EventBuilder;
    use crate::json::reference::write_string;

    struct RepoOp<'a> {
        action: &'a str,
        path: &'a str,
        cid: Option<&'a [u8]>,
    }

    fn push_int(out: &mut String, v: i64) {
        use std::fmt::Write;
        let _ = write!(out, "{v}");
    }

    fn open_frame(out: &mut String, kind: &str, did: &str, seq: i64, time: &str) {
        out.clear();
        out.push_str("{\"$type\":\"message\",\"payload\":{\"$type\":\"");
        out.push_str(TYPE_PREFIX);
        out.push('#');
        out.push_str(kind);
        out.push_str("\",\"did\":");
        write_string(out, did);
        out.push_str(",\"seq\":");
        push_int(out, seq);
        out.push_str(",\"time\":");
        write_string(out, time);
    }

    pub fn decode_firehose_frame(
        frame: &[u8],
        b: &mut EventBuilder,
        json: &mut String,
        out: &mut Vec<Decoded>,
    ) -> Result<(), DecodeError> {
        let mut r = Reader::new(frame);

        let Some(n) = r.map()? else {
            return Err(DecodeError::Cbor("frame header is not a map"));
        };
        let mut op = 1;
        let mut t = "";
        for _ in 0..n {
            match r.key()? {
                "op" => op = r.int()?.unwrap_or(1),
                "t" => t = r.str()?.unwrap_or(""),
                _ => r.skip()?,
            }
        }

        if op == -1 {
            let mut name = String::new();
            let mut message = String::new();
            if let Some(n) = r.map()? {
                for _ in 0..n {
                    match r.key()? {
                        "error" => name = r.str()?.unwrap_or("").to_owned(),
                        "message" => message = r.str()?.unwrap_or("").to_owned(),
                        _ => r.skip()?,
                    }
                }
            }
            out.push(Decoded::Notice(Notice { level: NoticeLevel::Error, name, message }));
            return Ok(());
        }

        match t {
            "#commit" => decode_commit(&mut r, b, json, out),
            "#identity" => decode_wrapped(&mut r, "identity", b, json, out),
            "#account" => decode_wrapped(&mut r, "account", b, json, out),
            "#sync" => decode_wrapped(&mut r, "sync", b, json, out),
            "#info" => {
                let mut name = String::new();
                let mut message = String::new();
                if let Some(n) = r.map()? {
                    for _ in 0..n {
                        match r.key()? {
                            "name" => name = r.str()?.unwrap_or("").to_owned(),
                            "message" => message = r.str()?.unwrap_or("").to_owned(),
                            _ => r.skip()?,
                        }
                    }
                }
                out.push(Decoded::Notice(Notice { level: NoticeLevel::Info, name, message }));
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn decode_commit(
        r: &mut Reader,
        b: &mut EventBuilder,
        json: &mut String,
        out: &mut Vec<Decoded>,
    ) -> Result<(), DecodeError> {
        let Some(n) = r.map()? else {
            return Err(DecodeError::Cbor("commit payload is not a map"));
        };
        let mut seq = 0;
        let mut repo = "";
        let mut rev = "";
        let mut time = "";
        let mut blocks: &[u8] = &[];
        let mut ops: Vec<RepoOp> = Vec::new();
        for _ in 0..n {
            match r.key()? {
                "seq" => seq = r.int()?.unwrap_or(0),
                "repo" => repo = r.str()?.unwrap_or(""),
                "rev" => rev = r.str()?.unwrap_or(""),
                "time" => time = r.str()?.unwrap_or(""),
                "blocks" => blocks = r.bytes()?.unwrap_or(&[]),
                "ops" => {
                    if let Some(count) = r.array()? {
                        ops.reserve(count.min(256));
                        for _ in 0..count {
                            let Some(fields) = r.map()? else { continue };
                            let mut op = RepoOp { action: "", path: "", cid: None };
                            for _ in 0..fields {
                                match r.key()? {
                                    "action" => op.action = r.str()?.unwrap_or(""),
                                    "path" => op.path = r.str()?.unwrap_or(""),
                                    "cid" => op.cid = r.cid()?,
                                    _ => r.skip()?,
                                }
                            }
                            ops.push(op);
                        }
                    }
                }
                _ => r.skip()?,
            }
        }
        if repo.is_empty() {
            return Err(DecodeError::Malformed("commit has no repo"));
        }

        let car = if blocks.is_empty() { None } else { Car::parse(blocks).ok() };

        let mut first_err = None;
        for op in &ops {
            let Some((collection, rkey)) = op.path.split_once('/') else { continue };
            open_frame(json, "commit", repo, seq, time);
            json.push_str(",\"operation\":");
            write_string(json, op.action);
            json.push_str(",\"collection\":");
            write_string(json, collection);
            json.push_str(",\"rkey\":");
            write_string(json, rkey);
            json.push_str(",\"rev\":");
            write_string(json, rev);
            if let Some(cid) = op.cid {
                json.push_str(",\"cid\":\"");
                cid_to_string(cid, json);
                json.push('"');
                if let Some(block) = car.as_ref().and_then(|c| c.get(cid)) {
                    let mark = json.len();
                    json.push_str(",\"record\":");
                    if to_json(&mut Reader::new(block), json).is_err() {
                        json.truncate(mark);
                    }
                }
            }
            json.push_str("}}");
            match decode_json_frame(json, b) {
                Ok(d) => out.push(d),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn decode_wrapped(
        r: &mut Reader,
        kind: &str,
        b: &mut EventBuilder,
        json: &mut String,
        out: &mut Vec<Decoded>,
    ) -> Result<(), DecodeError> {
        let start = r.pos();
        let Some(n) = r.map()? else {
            return Err(DecodeError::Cbor("payload is not a map"));
        };
        let mut seq = 0;
        let mut did = "";
        let mut time = "";
        for _ in 0..n {
            match r.key()? {
                "seq" => seq = r.int()?.unwrap_or(0),
                "did" => did = r.str()?.unwrap_or(""),
                "time" => time = r.str()?.unwrap_or(""),
                _ => r.skip()?,
            }
        }
        let end = r.pos();
        if did.is_empty() {
            return Err(DecodeError::Malformed("event has no did"));
        }
        open_frame(json, kind, did, seq, time);
        json.push_str(",\"");
        json.push_str(kind);
        json.push_str("\":");
        let mut again = Reader::new(r.slice(start, end));
        to_json(&mut again, json)?;
        json.push_str("}}");
        out.push(decode_json_frame(json, b)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::car::testenc::{car, fake_cid};
    use super::super::cbor::testenc as e;
    use super::*;
    use crate::event::{Event, Field, Flags, Kind, Op};

    fn header(t: &str) -> Vec<u8> {
        let mut b = Vec::new();
        e::map(&mut b, 2);
        e::text(&mut b, "op");
        e::uint(&mut b, 1);
        e::text(&mut b, "t");
        e::text(&mut b, t);
        b
    }

    fn post_record(text: &str) -> Vec<u8> {
        let mut b = Vec::new();
        e::map(&mut b, 4);
        e::text(&mut b, "text");
        e::text(&mut b, text);
        e::text(&mut b, "$type");
        e::text(&mut b, "app.bsky.feed.post");
        e::text(&mut b, "langs");
        e::array(&mut b, 1);
        e::text(&mut b, "en");
        e::text(&mut b, "createdAt");
        e::text(&mut b, "2026-08-13T06:47:44.859Z");
        b
    }

    fn decode(frame: &[u8]) -> Vec<Decoded> {
        let mut out = Vec::new();
        decode_firehose_frame(frame, &mut EventBuilder::new(), &mut Vec::new(), &mut out).unwrap();
        out
    }

    fn events(frame: &[u8]) -> Vec<Event> {
        decode(frame)
            .into_iter()
            .map(|d| match d {
                Decoded::Event(e) => e,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn commit_frame(with_blocks: bool) -> Vec<u8> {
        let (c1, c2) = (fake_cid(1), fake_cid(2));
        let blocks = car(&[
            (fake_cid(9), vec![0xa0]),
            (c1.clone(), post_record("hello \"firehose\"")),
            (c2.clone(), post_record("second")),
        ]);
        let mut b = header("#commit");
        e::map(&mut b, 6);
        e::text(&mut b, "ops");
        e::array(&mut b, 3);
        for (action, path, cid) in [
            ("create", "app.bsky.feed.post/3aaa", Some(&c1)),
            ("update", "app.bsky.feed.post/3bbb", Some(&c2)),
            ("delete", "app.bsky.feed.like/3ccc", None),
        ] {
            e::map(&mut b, 3);
            e::text(&mut b, "cid");
            match cid {
                Some(c) => e::cid(&mut b, c),
                None => e::null(&mut b),
            }
            e::text(&mut b, "path");
            e::text(&mut b, path);
            e::text(&mut b, "action");
            e::text(&mut b, action);
        }
        e::text(&mut b, "rev");
        e::text(&mut b, "3rev");
        e::text(&mut b, "seq");
        e::uint(&mut b, 4242);
        e::text(&mut b, "repo");
        e::text(&mut b, "did:plc:7e6kocyzb77xkncplrkoojej");
        e::text(&mut b, "time");
        e::text(&mut b, "2026-08-13T06:47:43.959Z");
        e::text(&mut b, "blocks");
        e::bytes(&mut b, if with_blocks { &blocks } else { &[] });
        b
    }

    #[test]
    fn commit_yields_one_event_per_op() {
        let evs = events(&commit_frame(true));
        assert_eq!(evs.len(), 3);
        let a = &evs[0];
        assert_eq!((a.kind, a.op, a.seq), (Kind::Commit, Op::Create, 4242));
        assert_eq!(a.did.to_string(), "did:plc:7e6kocyzb77xkncplrkoojej");
        assert_eq!(a.collection_str(), "app.bsky.feed.post");
        assert_eq!(a.field(Field::Rkey), "3aaa");
        assert_eq!(a.text(), "hello \"firehose\"");
        assert_eq!(a.time_us, 1_786_603_663_959_000);
        assert_eq!(a.list(Field::Langs).collect::<Vec<_>>(), ["en"]);
        assert!(a.flags.contains(Flags::RECORD | Flags::TEXT));
        let v: serde_json::Value = serde_json::from_str(a.raw()).unwrap();
        assert_eq!(v["payload"]["$type"], "com.atproto.sync.subscribeRepos#commit");
        assert_eq!(v["payload"]["record"]["text"], "hello \"firehose\"");
        assert!(v["payload"]["cid"].as_str().unwrap().starts_with("bafyrei"));
        assert_eq!(evs[1].op, Op::Update);
        assert_eq!(evs[1].text(), "second");
        let d = &evs[2];
        assert_eq!((d.op, d.collection_str(), d.field(Field::Rkey)), (Op::Delete, "app.bsky.feed.like", "3ccc"));
        assert!(d.record_json().is_none());
    }

    #[test]
    fn commit_without_blocks_keeps_ops() {
        let evs = events(&commit_frame(false));
        assert_eq!(evs.len(), 3);
        assert!(evs.iter().all(|e| e.record_json().is_none()));
    }

    #[test]
    fn identity_and_account() {
        let mut b = header("#identity");
        e::map(&mut b, 4);
        e::text(&mut b, "did");
        e::text(&mut b, "did:plc:7e6kocyzb77xkncplrkoojej");
        e::text(&mut b, "seq");
        e::uint(&mut b, 7);
        e::text(&mut b, "time");
        e::text(&mut b, "2026-08-13T06:47:43.959Z");
        e::text(&mut b, "handle");
        e::text(&mut b, "alice.test");
        let evs = events(&b);
        assert_eq!(evs.len(), 1);
        assert_eq!((evs[0].kind, evs[0].seq), (Kind::Identity, 7));
        assert_eq!(evs[0].field(Field::Aux), "alice.test");

        let mut b = header("#account");
        e::map(&mut b, 5);
        e::text(&mut b, "did");
        e::text(&mut b, "did:plc:7e6kocyzb77xkncplrkoojej");
        e::text(&mut b, "seq");
        e::uint(&mut b, 8);
        e::text(&mut b, "time");
        e::text(&mut b, "2026-08-13T06:47:43.959Z");
        e::text(&mut b, "active");
        e::bool(&mut b, false);
        e::text(&mut b, "status");
        e::text(&mut b, "suspended");
        let evs = events(&b);
        assert_eq!(evs[0].kind, Kind::Account);
        assert_eq!(evs[0].field(Field::Aux), "suspended");
        assert!(!evs[0].flags.contains(Flags::ACTIVE));
    }

    #[test]
    fn error_info_and_unknown_frames() {
        let mut b = Vec::new();
        e::map(&mut b, 1);
        e::text(&mut b, "op");
        e::head(&mut b, 1, 0); // -1
        e::map(&mut b, 2);
        e::text(&mut b, "error");
        e::text(&mut b, "FutureCursor");
        e::text(&mut b, "message");
        e::text(&mut b, "nope");
        match &decode(&b)[..] {
            [Decoded::Notice(n)] => {
                assert_eq!(n.level, NoticeLevel::Error);
                assert_eq!(n.name, "FutureCursor");
            }
            other => panic!("{other:?}"),
        }

        let mut b = header("#info");
        e::map(&mut b, 1);
        e::text(&mut b, "name");
        e::text(&mut b, "OutdatedCursor");
        assert!(matches!(&decode(&b)[..], [Decoded::Notice(n)] if n.level == NoticeLevel::Info));

        let mut b = header("#somethingNew");
        e::map(&mut b, 0);
        assert!(decode(&b).is_empty());
    }

    #[test]
    fn truncation_never_panics() {
        let frame = commit_frame(true);
        for cut in 0..frame.len() {
            let mut out = Vec::new();
            let _ = decode_firehose_frame(&frame[..cut], &mut EventBuilder::new(), &mut Vec::new(), &mut out);
        }
    }

    // ---- differential tests against the reference decoder ----

    /// Everything observable about a decoded item.
    fn fingerprint(d: &Decoded) -> String {
        const FIELDS: [Field; 15] = [
            Field::Raw,
            Field::Text,
            Field::Name,
            Field::Rkey,
            Field::Subject,
            Field::Parent,
            Field::Root,
            Field::Quote,
            Field::Langs,
            Field::Tags,
            Field::Mentions,
            Field::Links,
            Field::Labels,
            Field::Alt,
            Field::Aux,
        ];
        match d {
            Decoded::Notice(n) => format!("notice {:?} {:?} {:?}", n.level, n.name, n.message),
            Decoded::Event(e) => {
                let fields: Vec<&str> = FIELDS.iter().map(|&f| e.field(f)).collect();
                format!(
                    "{} {} {} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
                    e.seq,
                    e.time_us,
                    e.created_us,
                    e.did,
                    e.collection,
                    e.flags,
                    e.kind,
                    e.op,
                    (e.n_images, e.n_langs, e.n_tags, e.n_mentions, e.n_links, e.text_chars),
                    e.record_json(),
                    e.at_uri(),
                    fields
                )
            }
        }
    }

    /// Both decoders, each with its own long-lived scratch state like the
    /// pipeline's per-thread decoder.
    struct Pair {
        new_builder: EventBuilder,
        new_json: Vec<u8>,
        old_builder: EventBuilder,
        old_json: String,
        events: usize,
        records: usize,
        errors: usize,
    }

    impl Pair {
        fn new() -> Self {
            Pair {
                new_builder: EventBuilder::new(),
                new_json: Vec::new(),
                old_builder: EventBuilder::new(),
                old_json: String::new(),
                events: 0,
                records: 0,
                errors: 0,
            }
        }

        fn check(&mut self, frame: &[u8]) {
            let mut want = Vec::new();
            let want_res = reference::decode_firehose_frame(frame, &mut self.old_builder, &mut self.old_json, &mut want)
                .map_err(|e| e.to_string());
            let mut got = Vec::new();
            let got_res =
                decode_firehose_frame(frame, &mut self.new_builder, &mut self.new_json, &mut got).map_err(|e| e.to_string());
            assert_eq!(got_res, want_res, "{frame:x?}");
            self.records += got.iter().filter(|d| matches!(d, Decoded::Event(e) if e.record_json().is_some())).count();
            let want: Vec<String> = want.iter().map(fingerprint).collect();
            let got: Vec<String> = got.iter().map(fingerprint).collect();
            assert_eq!(got, want, "{frame:x?}");
            self.events += got.len();
            self.errors += got_res.is_err() as usize;
        }
    }

    fn random_did(rng: &mut fastrand::Rng) -> String {
        match rng.u8(..12) {
            0 => "did:web:example.com".into(),
            1 => String::new(),
            2 => e::random_text(rng),
            _ => {
                let id: String = (0..24).map(|_| *rng.choice(b"abcdefghijklmnopqrstuvwxyz234567").unwrap() as char).collect();
                format!("did:plc:{id}")
            }
        }
    }

    fn random_time(rng: &mut fastrand::Rng) -> String {
        match rng.u8(..12) {
            0 => String::new(),
            1 => e::random_text(rng),
            // Shapes the timestamp parser might or might not accept; the
            // escaped forms matter because the JSON carries them escaped.
            2 => "2026-08-13T06:47:43.959Z\n".into(),
            3 => "2026-08-13".into(),
            4 => "2026-08-13 06:47:43\t".into(),
            5 => "2026-08-13T06:47:43.959+02:00".into(),
            6 => "2026-08-13T06:47\\".into(),
            _ => format!("2026-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", rng.u8(1..13), rng.u8(1..29), rng.u8(..24), rng.u8(..60), rng.u8(..60), rng.u16(..1000)),
        }
    }

    fn random_uri(rng: &mut fastrand::Rng) -> String {
        match rng.u8(..5) {
            0 => format!("https://{}/{}", *rng.choice(&["example.com", "media.tenor.com", "GIPHY.com", "youtu.be"]).unwrap(), rng.u32(..)),
            1 => e::random_text(rng),
            _ => format!("at://{}/app.bsky.feed.post/3k{}", random_did(rng), rng.u32(..)),
        }
    }

    fn strong_ref(rng: &mut fastrand::Rng, b: &mut Vec<u8>) {
        e::map(b, 2);
        e::text(b, "cid");
        e::text(b, "bafyreigh2akiscaildc");
        e::text(b, "uri");
        e::text(b, &random_uri(rng));
    }

    /// A record shaped like the lexicons the extractor knows about, with
    /// fields present, absent and occasionally of the wrong type.
    fn random_record(rng: &mut fastrand::Rng, hostile: bool) -> Vec<u8> {
        let mut b = Vec::new();
        if rng.u8(..5) == 0 {
            e::random_value(rng, &mut b, 0, hostile);
            return b;
        }
        let mut fields: Vec<Vec<u8>> = Vec::new();
        let mut field = |key: &str, value: Vec<u8>| {
            let mut f = Vec::new();
            e::text(&mut f, key);
            f.extend(value);
            fields.push(f);
        };
        let text = |s: &str| {
            let mut v = Vec::new();
            e::text(&mut v, s);
            v
        };
        let ty = *rng
            .choice(&["app.bsky.feed.post", "app.bsky.feed.like", "app.bsky.graph.follow", "app.bsky.actor.profile", "com.example.thing"])
            .unwrap();
        field("$type", text(ty));
        for key in ["text", "description", "displayName", "name", "title"] {
            if rng.u8(..3) == 0 {
                field(key, text(&e::random_text(rng)));
            }
        }
        if rng.bool() {
            field("createdAt", text(&random_time(rng)));
        }
        for key in ["langs", "tags"] {
            if rng.u8(..3) == 0 {
                let n = rng.u64(..4);
                let mut v = Vec::new();
                e::array(&mut v, n);
                for _ in 0..n {
                    let item = if rng.u8(..4) == 0 { e::random_text(rng) } else { (*rng.choice(&["en", "ja", "pt-BR", "Art", "ÉTÉ"]).unwrap()).into() };
                    e::text(&mut v, &item);
                }
                field(key, v);
            }
        }
        if rng.u8(..3) == 0 {
            let mut v = Vec::new();
            e::map(&mut v, 2);
            e::text(&mut v, "root");
            strong_ref(rng, &mut v);
            e::text(&mut v, "parent");
            strong_ref(rng, &mut v);
            field("reply", v);
        }
        if rng.u8(..3) == 0 {
            let mut v = Vec::new();
            match rng.u8(..3) {
                0 => e::text(&mut v, &random_uri(rng)),
                1 => strong_ref(rng, &mut v),
                _ => e::cid(&mut v, &fake_cid(rng.u8(..))),
            }
            field("subject", v);
        }
        if rng.u8(..3) == 0 {
            let mut v = Vec::new();
            match rng.u8(..5) {
                0 => {
                    let n = rng.u64(..4);
                    e::map(&mut v, 2);
                    e::text(&mut v, "$type");
                    e::text(&mut v, "app.bsky.embed.images");
                    e::text(&mut v, "images");
                    e::array(&mut v, n);
                    for _ in 0..n {
                        e::map(&mut v, 2);
                        e::text(&mut v, "alt");
                        e::text(&mut v, &e::random_text(rng));
                        e::text(&mut v, "image");
                        e::map(&mut v, 4);
                        e::text(&mut v, "$type");
                        e::text(&mut v, "blob");
                        e::text(&mut v, "ref");
                        e::cid(&mut v, &fake_cid(rng.u8(..)));
                        e::text(&mut v, "mimeType");
                        e::text(&mut v, "image/jpeg");
                        e::text(&mut v, "size");
                        e::uint(&mut v, rng.u64(..5_000_000));
                    }
                }
                1 => {
                    e::map(&mut v, 1);
                    e::text(&mut v, "external");
                    e::map(&mut v, 2);
                    e::text(&mut v, "uri");
                    e::text(&mut v, &random_uri(rng));
                    e::text(&mut v, "title");
                    e::text(&mut v, &e::random_text(rng));
                }
                2 => {
                    e::map(&mut v, 1);
                    e::text(&mut v, "record");
                    strong_ref(rng, &mut v);
                }
                3 => {
                    e::map(&mut v, 2);
                    e::text(&mut v, "video");
                    e::map(&mut v, 1);
                    e::text(&mut v, "ref");
                    e::cid(&mut v, &fake_cid(3));
                    e::text(&mut v, "alt");
                    e::text(&mut v, &e::random_text(rng));
                }
                _ => {
                    e::map(&mut v, 2);
                    e::text(&mut v, "record");
                    e::map(&mut v, 1);
                    e::text(&mut v, "record");
                    strong_ref(rng, &mut v);
                    e::text(&mut v, "media");
                    e::map(&mut v, 1);
                    e::text(&mut v, "images");
                    e::array(&mut v, 0);
                }
            }
            field("embed", v);
        }
        if rng.u8(..3) == 0 {
            let n = rng.u64(..3);
            let mut v = Vec::new();
            e::array(&mut v, n);
            for _ in 0..n {
                e::map(&mut v, 2);
                e::text(&mut v, "index");
                e::map(&mut v, 2);
                e::text(&mut v, "byteEnd");
                e::uint(&mut v, rng.u64(..300));
                e::text(&mut v, "byteStart");
                e::uint(&mut v, rng.u64(..300));
                e::text(&mut v, "features");
                e::array(&mut v, 1);
                e::map(&mut v, 2);
                e::text(&mut v, "$type");
                let (feature, key, value) = match rng.u8(..3) {
                    0 => ("app.bsky.richtext.facet#link", "uri", random_uri(rng)),
                    1 => ("app.bsky.richtext.facet#mention", "did", random_did(rng)),
                    _ => ("app.bsky.richtext.facet#tag", "tag", e::random_text(rng)),
                };
                e::text(&mut v, feature);
                e::text(&mut v, key);
                e::text(&mut v, &value);
            }
            field("facets", v);
        }
        if rng.u8(..6) == 0 {
            let mut v = Vec::new();
            e::map(&mut v, 1);
            e::text(&mut v, "values");
            e::array(&mut v, 1);
            e::map(&mut v, 1);
            e::text(&mut v, "val");
            e::text(&mut v, "porn");
            field("labels", v);
        }
        if rng.u8(..6) == 0 {
            let mut v = Vec::new();
            e::random_value(rng, &mut v, 2, hostile);
            field(&e::random_text(rng), v);
        }
        if hostile && rng.u8(..10) == 0 {
            // A string that is not UTF-8, somewhere in the record.
            let mut v = Vec::new();
            e::head(&mut v, 3, 3);
            v.extend(b"\xe3\x81\"");
            field("text", v);
        }
        rng.shuffle(&mut fields);
        e::map(&mut b, fields.len() as u64);
        for f in fields {
            b.extend(f);
        }
        b
    }

    fn random_frame(rng: &mut fastrand::Rng, hostile: bool) -> Vec<u8> {
        let kind = rng.u8(..20);
        if kind == 0 {
            // Error frame.
            let mut b = Vec::new();
            e::map(&mut b, 1);
            e::text(&mut b, "op");
            e::head(&mut b, 1, 0);
            e::map(&mut b, 2);
            e::text(&mut b, "error");
            e::text(&mut b, &e::random_text(rng));
            e::text(&mut b, "message");
            e::text(&mut b, &e::random_text(rng));
            return b;
        }
        if kind < 5 {
            let t = *rng.choice(&["#identity", "#account", "#sync", "#info", "#future"]).unwrap();
            let mut b = header(t);
            let mut fields: Vec<Vec<u8>> = Vec::new();
            let mut field = |key: &str, value: Vec<u8>| {
                let mut f = Vec::new();
                e::text(&mut f, key);
                f.extend(value);
                fields.push(f);
            };
            let text = |s: &str| {
                let mut v = Vec::new();
                e::text(&mut v, s);
                v
            };
            field("did", text(&random_did(rng)));
            field("time", text(&random_time(rng)));
            let mut v = Vec::new();
            e::uint(&mut v, rng.u64(..1 << 40));
            field("seq", v);
            for key in ["handle", "status", "name", "message", "rev"] {
                if rng.bool() {
                    field(key, text(&e::random_text(rng)));
                }
            }
            if rng.bool() {
                let mut v = Vec::new();
                e::bool(&mut v, rng.bool());
                field("active", v);
            }
            if rng.u8(..4) == 0 {
                let mut v = Vec::new();
                e::bytes(&mut v, &(0..rng.usize(..300)).map(|_| rng.u8(..)).collect::<Vec<_>>());
                field("blocks", v);
            }
            if rng.u8(..4) == 0 {
                let mut v = Vec::new();
                e::random_value(rng, &mut v, 1, hostile);
                field(&e::random_text(rng), v);
            }
            rng.shuffle(&mut fields);
            e::map(&mut b, fields.len() as u64);
            for f in fields {
                b.extend(f);
            }
            return b;
        }

        // Commit.
        let n_ops = match rng.u8(..20) {
            0 => 0,
            1 => rng.usize(5..40),
            2..=5 => rng.usize(2..5),
            _ => 1,
        };
        let mut blocks: Vec<(Vec<u8>, Vec<u8>)> = vec![(fake_cid(250), vec![0xa0])];
        let mut ops = Vec::new();
        e::array(&mut ops, n_ops as u64);
        for i in 0..n_ops {
            let cid = fake_cid(i as u8);
            let mut fields: Vec<Vec<u8>> = Vec::new();
            let mut f = Vec::new();
            e::text(&mut f, "action");
            let action = match rng.u8(..10) {
                0 => e::random_text(rng),
                1 => "delete".into(),
                2 => "update".into(),
                _ => "create".to_owned(),
            };
            e::text(&mut f, &action);
            fields.push(f);
            let mut f = Vec::new();
            e::text(&mut f, "path");
            let path = match rng.u8(..14) {
                0 => "no-slash".into(),
                1 => "/".into(),
                2 => format!("/{}", e::random_text(rng)),
                3 => format!("{}/{}", e::random_text(rng), e::random_text(rng)),
                4 => "app.bsky.feed.post/a/b\"c".into(),
                _ => format!(
                    "{}/3k{}",
                    *rng.choice(&["app.bsky.feed.post", "app.bsky.feed.like", "app.bsky.graph.follow", "com.example.thing"]).unwrap(),
                    rng.u32(..)
                ),
            };
            e::text(&mut f, &path);
            fields.push(f);
            if rng.u8(..10) != 0 {
                let mut f = Vec::new();
                e::text(&mut f, "cid");
                if action == "delete" || rng.u8(..10) == 0 {
                    e::null(&mut f);
                } else {
                    e::cid(&mut f, &cid);
                    // Most blocks are present; some records are missing.
                    if rng.u8(..10) != 0 {
                        blocks.push((cid.clone(), random_record(rng, hostile)));
                    }
                }
                fields.push(f);
            }
            if rng.u8(..6) == 0 {
                let mut f = Vec::new();
                e::text(&mut f, "prev");
                e::null(&mut f);
                fields.push(f);
            }
            rng.shuffle(&mut fields);
            if rng.u8(..30) == 0 {
                // Not a map at all.
                e::uint(&mut ops, 7);
            } else {
                e::map(&mut ops, fields.len() as u64);
                for f in fields {
                    ops.extend(f);
                }
            }
        }
        rng.shuffle(&mut blocks);
        let mut archive = car(&blocks);
        match rng.u8(..25) {
            0 => archive.clear(),
            1 => archive.truncate(rng.usize(..archive.len())),
            2 => archive.extend([0xff; 12]),
            _ => {}
        }

        let mut fields: Vec<Vec<u8>> = Vec::new();
        let mut field = |key: &str, value: Vec<u8>| {
            let mut f = Vec::new();
            e::text(&mut f, key);
            f.extend(value);
            fields.push(f);
        };
        let text = |s: &str| {
            let mut v = Vec::new();
            e::text(&mut v, s);
            v
        };
        field("ops", ops);
        if rng.u8(..30) != 0 {
            field("repo", text(&random_did(rng)));
        }
        field("rev", text(&if rng.u8(..8) == 0 { e::random_text(rng) } else { "3lzx2abcdefgh".into() }));
        field("time", text(&random_time(rng)));
        let mut v = Vec::new();
        match rng.u8(..12) {
            0 => e::uint(&mut v, u64::MAX),
            1 => e::head(&mut v, 1, rng.u64(..1000)),
            2 => e::text(&mut v, "12"),
            3 => e::uint(&mut v, 0),
            _ => e::uint(&mut v, rng.u64(..1 << 40)),
        }
        field("seq", v);
        let mut v = Vec::new();
        if rng.u8(..30) == 0 {
            e::null(&mut v);
        } else {
            e::bytes(&mut v, &archive);
        }
        field("blocks", v);
        // Fields the decoder does not read.
        let mut v = Vec::new();
        e::bool(&mut v, false);
        field("tooBig", v);
        let mut v = Vec::new();
        e::cid(&mut v, &fake_cid(200));
        field("commit", v);
        let mut v = Vec::new();
        e::array(&mut v, 0);
        field("blobs", v);
        if rng.u8(..6) == 0 {
            let mut v = Vec::new();
            e::random_value(rng, &mut v, 1, hostile);
            field(&e::random_text(rng), v);
        }
        if hostile && rng.u8(..20) == 0 {
            // A key that is not UTF-8.
            let mut f = Vec::new();
            e::head(&mut f, 3, 2);
            f.extend([0xc3, 0x28]);
            e::null(&mut f);
            fields.push(f);
        }
        rng.shuffle(&mut fields);
        let mut b = header("#commit");
        e::map(&mut b, fields.len() as u64);
        for f in fields {
            b.extend(f);
        }
        b
    }

    #[test]
    fn differential_well_formed_frames() {
        let mut rng = fastrand::Rng::with_seed(0xf12e);
        let mut pair = Pair::new();
        for _ in 0..6_000 {
            pair.check(&random_frame(&mut rng, false));
        }
        // The generator must actually reach the interesting paths.
        assert!(pair.events > 4_000, "{}", pair.events);
        assert!(pair.records > 2_000, "{}", pair.records);
    }

    #[test]
    fn differential_hostile_frames() {
        let mut rng = fastrand::Rng::with_seed(0xdead);
        let mut pair = Pair::new();
        for _ in 0..6_000 {
            let mut frame = random_frame(&mut rng, true);
            match rng.u8(..5) {
                0 => frame.truncate(rng.usize(..frame.len())),
                1 => {
                    for _ in 0..rng.usize(1..4) {
                        let i = rng.usize(..frame.len());
                        frame[i] = rng.u8(..);
                    }
                }
                2 => {
                    let i = rng.usize(..frame.len());
                    frame[i] ^= 1 << rng.u8(..8);
                }
                _ => {}
            }
            pair.check(&frame);
        }
        assert!(pair.events > 1_000, "{}", pair.events);
        assert!(pair.errors > 500, "{}", pair.errors);
    }

    #[test]
    fn differential_fixed_frames() {
        let mut pair = Pair::new();
        pair.check(&commit_frame(true));
        pair.check(&commit_frame(false));
        // Every truncation and every single-byte corruption of a commit.
        let frame = commit_frame(true);
        for cut in 0..frame.len() {
            pair.check(&frame[..cut]);
        }
        for i in 0..frame.len() {
            for flip in [0x01, 0x80, 0xff] {
                let mut f = frame.clone();
                f[i] ^= flip;
                pair.check(&f);
            }
        }
    }

    #[test]
    fn record_with_invalid_utf8_is_dropped_but_op_is_kept() {
        let (c1, c2) = (fake_cid(1), fake_cid(2));
        let mut bad = Vec::new();
        e::map(&mut bad, 2);
        e::text(&mut bad, "$type");
        e::text(&mut bad, "app.bsky.feed.post");
        e::text(&mut bad, "text");
        e::head(&mut bad, 3, 4);
        bad.extend(b"ab\xff\"");
        let blocks = car(&[(c1.clone(), bad), (c2.clone(), post_record("good"))]);
        let mut b = header("#commit");
        e::map(&mut b, 5);
        e::text(&mut b, "ops");
        e::array(&mut b, 2);
        for (path, cid) in [("app.bsky.feed.post/3aaa", &c1), ("app.bsky.feed.post/3bbb", &c2)] {
            e::map(&mut b, 3);
            e::text(&mut b, "cid");
            e::cid(&mut b, cid);
            e::text(&mut b, "path");
            e::text(&mut b, path);
            e::text(&mut b, "action");
            e::text(&mut b, "create");
        }
        e::text(&mut b, "seq");
        e::uint(&mut b, 1);
        e::text(&mut b, "repo");
        e::text(&mut b, "did:plc:7e6kocyzb77xkncplrkoojej");
        e::text(&mut b, "time");
        e::text(&mut b, "2026-08-13T06:47:43.959Z");
        e::text(&mut b, "blocks");
        e::bytes(&mut b, &blocks);

        Pair::new().check(&b);
        let evs = events(&b);
        assert_eq!(evs.len(), 2);
        assert!(evs[0].record_json().is_none());
        assert!(evs[0].raw().ends_with("\"}}"), "{}", evs[0].raw());
        assert_eq!(evs[0].field(Field::Rkey), "3aaa");
        assert_eq!(evs[1].text(), "good");
        for e in &evs {
            serde_json::from_str::<serde_json::Value>(e.raw()).unwrap();
        }
    }

    #[test]
    fn many_ops_spill_past_the_inline_slots() {
        for n in [0usize, 1, INLINE_OPS - 1, INLINE_OPS, INLINE_OPS + 1, 300] {
            let mut blocks = Vec::new();
            let mut b = header("#commit");
            e::map(&mut b, 4);
            e::text(&mut b, "ops");
            e::array(&mut b, n as u64);
            for i in 0..n {
                let cid = fake_cid(i as u8);
                e::map(&mut b, 3);
                e::text(&mut b, "cid");
                e::cid(&mut b, &cid);
                e::text(&mut b, "path");
                e::text(&mut b, &format!("app.bsky.feed.post/r{i}"));
                e::text(&mut b, "action");
                e::text(&mut b, "create");
                if i < 256 {
                    blocks.push((cid, post_record(&format!("post {i}"))));
                }
            }
            e::text(&mut b, "seq");
            e::uint(&mut b, 9);
            e::text(&mut b, "repo");
            e::text(&mut b, "did:plc:7e6kocyzb77xkncplrkoojej");
            e::text(&mut b, "blocks");
            e::bytes(&mut b, &car(&blocks));
            Pair::new().check(&b);
            let evs = events(&b);
            assert_eq!(evs.len(), n);
            for (i, ev) in evs.iter().enumerate() {
                assert_eq!(ev.field(Field::Rkey), format!("r{i}"));
            }
        }
    }

    /// Replay a capture made with `vortex-bench-firehose capture` through
    /// both decoders. Opt-in, since captures are large and not checked in:
    ///
    /// ```text
    /// VORTEX_FIREHOSE_CAPTURE=target/firehose.capture cargo test --release -p vortex-core differential_capture
    /// ```
    #[test]
    fn differential_capture() {
        let Ok(path) = std::env::var("VORTEX_FIREHOSE_CAPTURE") else { return };
        let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let mut pair = Pair::new();
        let (mut pos, mut frames) = (0, 0);
        while pos + 4 <= data.len() {
            let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            let Some(frame) = data.get(pos..pos + len) else { break };
            pair.check(frame);
            pos += len;
            frames += 1;
        }
        eprintln!("{path}: {frames} frames, {} events, {} with records, {} errors", pair.events, pair.records, pair.errors);
        assert!(frames > 0);
    }
}
