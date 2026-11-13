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

use super::car::Car;
use super::cbor::{cid_to_string, Reader};
use super::frame::decode_json_frame;
use super::{DecodeError, Decoded, Notice, NoticeLevel};
use crate::event::EventBuilder;
use crate::json::write_string;

const TYPE_PREFIX: &str = "com.atproto.sync.subscribeRepos";

struct RepoOp<'a> {
    action: &'a str,
    path: &'a str,
    cid: Option<&'a [u8]>,
}

fn push_int(out: &mut String, v: i64) {
    use std::fmt::Write;
    let _ = write!(out, "{v}");
}

/// Open `{"$type":"message","payload":{"$type":"…#kind","did":…,"seq":…,"time":…`
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

    // Header: {op: 1 | -1, t: "#commit" | …}
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
        // Unknown message types must be ignored per the event stream spec.
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

    // A commit without usable blocks (tooBig, or a malformed archive) still
    // yields its ops, just without record bodies.
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
                // A record that fails to transcode is dropped, not the op.
                if Reader::new(block).to_json(json).is_err() {
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

/// `#identity`, `#account` and `#sync`: wrap the payload verbatim under a key
/// named after the kind, exactly as Jetstream v2 does.
fn decode_wrapped(
    r: &mut Reader,
    kind: &str,
    b: &mut EventBuilder,
    json: &mut String,
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
    json.push_str(",\"");
    json.push_str(kind);
    json.push_str("\":");
    let mut again = Reader::new(r.slice(start, end));
    again.to_json(json)?;
    json.push_str("}}");
    out.push(decode_json_frame(json, b)?);
    Ok(())
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
        decode_firehose_frame(frame, &mut EventBuilder::new(), &mut String::new(), &mut out).unwrap();
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
            let _ = decode_firehose_frame(&frame[..cut], &mut EventBuilder::new(), &mut String::new(), &mut out);
        }
    }
}
