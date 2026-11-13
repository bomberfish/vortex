//! Jetstream JSON frame → [`Event`].
//!
//! One walker handles both wire shapes, since they only differ in nesting:
//!
//! * v2 (`network.bsky.jetstream.subscribeEvents`, `xrpc.v1.json` framing):
//!   `{"$type":"message","payload":{"$type":"…#commit","did":…,"seq":…,"time":…,"operation":…,"collection":…,"rkey":…,"record":{…}}}`
//! * v1 (legacy `/subscribe`):
//!   `{"did":…,"time_us":…,"kind":"commit","commit":{"operation":…,"collection":…,"rkey":…,"record":{…}}}`
//!
//! The firehose decoder transcodes its CBOR into the v2 shape and reuses this.
//!
//! Record feature extraction is deliberately schema-agnostic: it keys off
//! well-known field names and checks value types as it goes, so records from
//! unknown lexicons degrade to "fewer extracted fields" rather than errors.

use super::{DecodeError, Decoded, Notice, NoticeLevel};
use crate::did::Did;
use crate::event::{url_host, EventBuilder, Field, Flags, Kind, Op};
use crate::json::{Result as JsonResult, Scanner, Str, ValueKind};
use crate::nsid::Nsid;
use crate::time::parse_rfc3339_us;

#[derive(Default)]
struct Envelope {
    is_error: bool,
    is_info: bool,
    error: String,
    name: String,
    message: String,
}

/// Decode one text frame. `b` is reset first and reused across calls.
pub fn decode_json_frame(src: &str, b: &mut EventBuilder) -> Result<Decoded, DecodeError> {
    b.reset();
    let mut env = Envelope::default();
    let mut s = Scanner::new(src);
    if !s.object()? {
        return Err(DecodeError::Malformed("frame is not an object"));
    }
    walk_envelope(&mut s, b, &mut env, 0)?;

    if env.is_error {
        return Ok(Decoded::Notice(Notice { level: NoticeLevel::Error, name: env.error, message: env.message }));
    }
    if env.is_info {
        return Ok(Decoded::Notice(Notice { level: NoticeLevel::Info, name: env.name, message: env.message }));
    }

    if b.kind.is_none() && (b.op != Op::None || b.collection.is_some()) {
        b.kind = Some(Kind::Commit);
    }
    let Some(kind) = b.kind else {
        return Err(DecodeError::Malformed("frame has no recognizable kind"));
    };
    if b.did.is_none() {
        return Err(DecodeError::Malformed("event has no did"));
    }
    if kind == Kind::Commit {
        if b.collection.is_none() {
            return Err(DecodeError::Malformed("commit has no collection"));
        }
    } else {
        b.op = Op::None;
        b.collection = None;
    }
    if b.seq == 0 {
        // v1 has no seq; its cursor is the microsecond timestamp.
        b.seq = b.time_us;
    }
    match b.finish(src) {
        Some(e) => Ok(Decoded::Event(e)),
        None => Err(DecodeError::Malformed("event too large")),
    }
}

fn type_kind(ty: &str) -> Option<&str> {
    ty.rsplit_once('#').map(|(_, k)| k)
}

fn walk_envelope(s: &mut Scanner, b: &mut EventBuilder, env: &mut Envelope, depth: u8) -> JsonResult<()> {
    while let Some(key) = s.key()? {
        match key {
            "$type" => {
                if let Some(v) = s.str()? {
                    match v.raw() {
                        "message" => {}
                        "error" => env.is_error = true,
                        ty => match type_kind(ty) {
                            Some("info") => env.is_info = true,
                            Some(k) => {
                                if let Some(k) = Kind::parse(k) {
                                    b.kind = Some(k);
                                }
                            }
                            None => {}
                        },
                    }
                }
            }
            "payload" | "commit" if depth == 0 => {
                if s.object()? {
                    walk_envelope(s, b, env, depth + 1)?;
                }
            }
            "did" => {
                if let Some(v) = s.str()? {
                    b.did = Some(Did::parse(&v.to_cow()));
                }
            }
            "seq" => {
                if let Some(v) = s.i64()? {
                    b.seq = v;
                }
            }
            "time" => {
                if let Some(v) = s.str()? {
                    if let Some(us) = parse_rfc3339_us(v.raw()) {
                        b.time_us = us;
                    }
                }
            }
            "witnessedAt" => {
                if let Some(v) = s.str()? {
                    b.witnessed_us = parse_rfc3339_us(v.raw()).unwrap_or(0);
                }
            }
            "time_us" => {
                if let Some(v) = s.i64()? {
                    b.time_us = v;
                }
            }
            "kind" => {
                if let Some(v) = s.str()? {
                    if let Some(k) = Kind::parse(v.raw()) {
                        b.kind = Some(k);
                    }
                }
            }
            "operation" => {
                if let Some(v) = s.str()? {
                    b.op = Op::parse(v.raw()).unwrap_or(Op::None);
                }
            }
            "collection" => {
                if let Some(v) = s.str()? {
                    if !v.raw().is_empty() {
                        b.collection = Some(Nsid::intern(&v.to_cow()));
                    }
                }
            }
            "rkey" => {
                if let Some(v) = s.str()? {
                    let buf = b.buf(Field::Rkey);
                    buf.clear();
                    v.push_to(buf);
                }
            }
            "record" => walk_record(s, b)?,
            "identity" => {
                b.kind.get_or_insert(Kind::Identity);
                if s.object()? {
                    while let Some(k) = s.key()? {
                        if k == "handle" {
                            if let Some(v) = s.str()? {
                                let buf = b.buf(Field::Aux);
                                buf.clear();
                                v.push_to(buf);
                                if !buf.is_empty() {
                                    b.flags |= Flags::HANDLE;
                                }
                            }
                        } else {
                            s.skip()?;
                        }
                    }
                }
            }
            "account" => {
                b.kind.get_or_insert(Kind::Account);
                if s.object()? {
                    while let Some(k) = s.key()? {
                        match k {
                            "active" => {
                                if s.bool()? == Some(true) {
                                    b.flags |= Flags::ACTIVE;
                                }
                            }
                            "status" => {
                                if let Some(v) = s.str()? {
                                    let buf = b.buf(Field::Aux);
                                    buf.clear();
                                    v.push_to(buf);
                                }
                            }
                            _ => s.skip()?,
                        }
                    }
                }
            }
            "sync" => {
                b.kind.get_or_insert(Kind::Sync);
                s.skip()?;
            }
            "error" => {
                if let Some(v) = s.str()? {
                    v.push_to(&mut env.error);
                }
            }
            "name" => {
                if let Some(v) = s.str()? {
                    v.push_to(&mut env.name);
                }
            }
            "message" => {
                if let Some(v) = s.str()? {
                    v.push_to(&mut env.message);
                }
            }
            _ => s.skip()?,
        }
    }
    Ok(())
}

#[inline]
fn bump(n: &mut u8) {
    *n = n.saturating_add(1);
}

fn push_item(b: &mut EventBuilder, field: Field, v: Str, lowercase: bool) -> bool {
    let (start, buf) = b.begin_item(field);
    v.push_to(buf);
    b.end_item(field, start, lowercase)
}

fn walk_record(s: &mut Scanner, b: &mut EventBuilder) -> JsonResult<()> {
    s.peek();
    let start = s.pos();
    if !s.object()? {
        return Ok(());
    }
    b.flags |= Flags::RECORD;
    while let Some(key) = s.key()? {
        match key {
            "text" => {
                if let Some(v) = s.str()? {
                    if let Some(buf) = b.text_slot(3) {
                        v.push_to(buf);
                    }
                }
            }
            "description" => {
                if let Some(v) = s.str()? {
                    if let Some(buf) = b.text_slot(2) {
                        v.push_to(buf);
                    }
                }
            }
            "displayName" => {
                if let Some(v) = s.str()? {
                    if let Some(buf) = b.name_slot(3) {
                        v.push_to(buf);
                    }
                }
            }
            "name" => {
                if let Some(v) = s.str()? {
                    if let Some(buf) = b.name_slot(2) {
                        v.push_to(buf);
                    }
                }
            }
            "title" => {
                if let Some(v) = s.str()? {
                    if let Some(buf) = b.name_slot(1) {
                        v.push_to(buf);
                    }
                }
            }
            "createdAt" => {
                if let Some(v) = s.str()? {
                    b.created_us = parse_rfc3339_us(v.raw()).unwrap_or(0);
                }
            }
            "langs" => {
                if s.array()? {
                    while s.elem()? {
                        if let Some(v) = s.str()? {
                            if push_item(b, Field::Langs, v, false) {
                                bump(&mut b.n_langs);
                                b.flags |= Flags::LANGS;
                            }
                        }
                    }
                }
            }
            "tags" => {
                if s.array()? {
                    while s.elem()? {
                        if let Some(v) = s.str()? {
                            if push_item(b, Field::Tags, v, true) {
                                bump(&mut b.n_tags);
                                b.flags |= Flags::TAG;
                            }
                        }
                    }
                }
            }
            "reply" => {
                if s.object()? {
                    b.flags |= Flags::REPLY;
                    while let Some(k) = s.key()? {
                        let field = match k {
                            "parent" => Field::Parent,
                            "root" => Field::Root,
                            _ => {
                                s.skip()?;
                                continue;
                            }
                        };
                        if s.object()? {
                            while let Some(k) = s.key()? {
                                if k == "uri" {
                                    if let Some(v) = s.str()? {
                                        let buf = b.buf(field);
                                        buf.clear();
                                        v.push_to(buf);
                                    }
                                } else {
                                    s.skip()?;
                                }
                            }
                        }
                    }
                }
            }
            "embed" => walk_embed(s, b, 0)?,
            "facets" => walk_facets(s, b)?,
            "labels" => {
                if s.object()? {
                    while let Some(k) = s.key()? {
                        if k == "values" && s.kind()? == ValueKind::Array {
                            s.array()?;
                            while s.elem()? {
                                if s.object()? {
                                    while let Some(k) = s.key()? {
                                        if k == "val" {
                                            if let Some(v) = s.str()? {
                                                if push_item(b, Field::Labels, v, false) {
                                                    b.flags |= Flags::LABELS;
                                                }
                                            }
                                        } else {
                                            s.skip()?;
                                        }
                                    }
                                }
                            }
                        } else {
                            s.skip()?;
                        }
                    }
                }
            }
            "subject" => match s.kind()? {
                ValueKind::String => {
                    if let Some(v) = s.str()? {
                        let buf = b.buf(Field::Subject);
                        buf.clear();
                        v.push_to(buf);
                    }
                }
                ValueKind::Object => {
                    s.object()?;
                    while let Some(k) = s.key()? {
                        if k == "uri" {
                            if let Some(v) = s.str()? {
                                let buf = b.buf(Field::Subject);
                                buf.clear();
                                v.push_to(buf);
                            }
                        } else {
                            s.skip()?;
                        }
                    }
                }
                _ => s.skip()?,
            },
            _ => s.skip()?,
        }
    }
    b.rec = (start as u32, (s.pos() - start) as u32);
    Ok(())
}

fn is_gif_host(url: &str) -> bool {
    let host = url_host(url);
    host.len() < 64 && {
        let h = host.to_ascii_lowercase();
        h.ends_with("tenor.com") || h.ends_with("giphy.com")
    }
}

fn walk_embed(s: &mut Scanner, b: &mut EventBuilder, depth: u8) -> JsonResult<()> {
    if !s.object()? {
        return Ok(());
    }
    b.flags |= Flags::EMBED;
    while let Some(key) = s.key()? {
        match key {
            "images" => {
                if s.array()? {
                    while s.elem()? {
                        bump(&mut b.n_images);
                        b.flags |= Flags::IMAGES;
                        if s.object()? {
                            while let Some(k) = s.key()? {
                                if k == "alt" {
                                    if let Some(v) = s.str()? {
                                        if push_item(b, Field::Alt, v, false) {
                                            b.flags |= Flags::ALT;
                                        }
                                    }
                                } else {
                                    s.skip()?;
                                }
                            }
                        }
                    }
                }
            }
            "video" => {
                if s.kind()? == ValueKind::Object {
                    b.flags |= Flags::VIDEO;
                }
                s.skip()?;
            }
            "alt" => {
                if let Some(v) = s.str()? {
                    if push_item(b, Field::Alt, v, false) {
                        b.flags |= Flags::ALT;
                    }
                }
            }
            "external" => {
                if s.object()? {
                    b.flags |= Flags::EXTERNAL;
                    while let Some(k) = s.key()? {
                        if k == "uri" {
                            if let Some(v) = s.str()? {
                                let (start, buf) = b.begin_item(Field::Links);
                                v.push_to(buf);
                                let gif = is_gif_host(&buf[start..]);
                                if b.end_item(Field::Links, start, false) {
                                    bump(&mut b.n_links);
                                    b.flags |= Flags::LINK;
                                    if gif {
                                        b.flags |= Flags::GIF;
                                    }
                                }
                            }
                        } else {
                            s.skip()?;
                        }
                    }
                }
            }
            "record" => walk_embed_record(s, b, 0)?,
            "media" if depth < 4 => walk_embed(s, b, depth + 1)?,
            _ => s.skip()?,
        }
    }
    Ok(())
}

/// `app.bsky.embed.record` is `{record:{uri,cid}}`; `recordWithMedia` nests
/// one level deeper (`{record:{record:{uri,cid}},media}`).
fn walk_embed_record(s: &mut Scanner, b: &mut EventBuilder, depth: u8) -> JsonResult<()> {
    if !s.object()? {
        return Ok(());
    }
    while let Some(key) = s.key()? {
        match key {
            "uri" => {
                if let Some(v) = s.str()? {
                    let buf = b.buf(Field::Quote);
                    if buf.is_empty() {
                        v.push_to(buf);
                        b.flags |= Flags::QUOTE;
                    }
                }
            }
            "record" if depth < 4 => walk_embed_record(s, b, depth + 1)?,
            _ => s.skip()?,
        }
    }
    Ok(())
}

fn walk_facets(s: &mut Scanner, b: &mut EventBuilder) -> JsonResult<()> {
    if !s.array()? {
        return Ok(());
    }
    while s.elem()? {
        if !s.object()? {
            continue;
        }
        b.flags |= Flags::FACETS;
        while let Some(key) = s.key()? {
            if key != "features" || s.kind()? != ValueKind::Array {
                s.skip()?;
                continue;
            }
            s.array()?;
            while s.elem()? {
                if !s.object()? {
                    continue;
                }
                while let Some(k) = s.key()? {
                    match k {
                        "did" => {
                            if let Some(v) = s.str()? {
                                if push_item(b, Field::Mentions, v, false) {
                                    bump(&mut b.n_mentions);
                                    b.flags |= Flags::MENTION;
                                }
                            }
                        }
                        "uri" => {
                            if let Some(v) = s.str()? {
                                if push_item(b, Field::Links, v, false) {
                                    bump(&mut b.n_links);
                                    b.flags |= Flags::LINK;
                                }
                            }
                        }
                        "tag" => {
                            if let Some(v) = s.str()? {
                                if push_item(b, Field::Tags, v, true) {
                                    bump(&mut b.n_tags);
                                    b.flags |= Flags::TAG;
                                }
                            }
                        }
                        _ => s.skip()?,
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    fn ev(src: &str) -> Event {
        let mut b = EventBuilder::new();
        match decode_json_frame(src, &mut b).unwrap() {
            Decoded::Event(e) => e,
            other => panic!("expected event, got {other:?}"),
        }
    }

    #[test]
    fn v2_commit_post_with_everything() {
        let src = r##"{"$type":"message","payload":{"$type":"network.bsky.jetstream.subscribeEvents#commit","did":"did:plc:7e6kocyzb77xkncplrkoojej","seq":24664288881,"time":"2026-08-13T06:47:43.959305Z","witnessedAt":"2026-08-13T06:47:43.959305Z","operation":"create","collection":"app.bsky.feed.post","rkey":"3msx2efqdxs27","rev":"3msx2efqjtc27","cid":"bafyreigwn","record":{"$type":"app.bsky.feed.post","createdAt":"2026-08-13T06:47:44.859Z","langs":["en","ja"],"text":"Hello \"world\" #Rust\nline two","tags":["Extra"],"reply":{"parent":{"cid":"c1","uri":"at://did:plc:parent/app.bsky.feed.post/p1"},"root":{"cid":"c2","uri":"at://did:plc:root/app.bsky.feed.post/r1"}},"facets":[{"index":{"byteStart":0,"byteEnd":5},"features":[{"$type":"app.bsky.richtext.facet#tag","tag":"Rust"}]},{"features":[{"$type":"app.bsky.richtext.facet#mention","did":"did:plc:mentioned"},{"$type":"app.bsky.richtext.facet#link","uri":"https://example.com/a"}]}],"embed":{"$type":"app.bsky.embed.recordWithMedia","record":{"$type":"app.bsky.embed.record","record":{"cid":"qc","uri":"at://did:plc:quoted/app.bsky.feed.post/q1"}},"media":{"$type":"app.bsky.embed.images","images":[{"alt":"a cat","image":{"$type":"blob","ref":{"$link":"bafk"},"mimeType":"image/jpeg","size":1}},{"alt":"","image":{}}]}},"labels":{"$type":"com.atproto.label.defs#selfLabels","values":[{"val":"porn"}]}}}}"##;
        let e = ev(src);
        assert_eq!(e.kind, Kind::Commit);
        assert_eq!(e.op, Op::Create);
        assert_eq!(e.seq, 24664288881);
        assert_eq!(e.time_us, 1_786_603_663_959_305);
        assert_eq!(e.created_us, 1_786_603_664_859_000);
        assert_eq!(e.did.to_string(), "did:plc:7e6kocyzb77xkncplrkoojej");
        assert_eq!(e.collection_str(), "app.bsky.feed.post");
        assert_eq!(e.field(Field::Rkey), "3msx2efqdxs27");
        assert_eq!(e.text(), "Hello \"world\" #Rust\nline two");
        assert_eq!(e.list(Field::Langs).collect::<Vec<_>>(), ["en", "ja"]);
        assert_eq!(e.list(Field::Tags).collect::<Vec<_>>(), ["extra", "rust"]);
        assert_eq!(e.list(Field::Mentions).collect::<Vec<_>>(), ["did:plc:mentioned"]);
        assert_eq!(e.list(Field::Links).collect::<Vec<_>>(), ["https://example.com/a"]);
        assert_eq!(e.list(Field::Labels).collect::<Vec<_>>(), ["porn"]);
        assert_eq!(e.field(Field::Parent), "at://did:plc:parent/app.bsky.feed.post/p1");
        assert_eq!(e.field(Field::Root), "at://did:plc:root/app.bsky.feed.post/r1");
        assert_eq!(e.field(Field::Quote), "at://did:plc:quoted/app.bsky.feed.post/q1");
        assert_eq!(e.field(Field::Alt), "a cat");
        assert_eq!((e.n_images, e.n_langs, e.n_tags, e.n_mentions, e.n_links), (2, 2, 2, 1, 1));
        for f in [
            Flags::REPLY, Flags::QUOTE, Flags::IMAGES, Flags::LINK, Flags::MENTION, Flags::TAG, Flags::LABELS,
            Flags::LANGS, Flags::ALT, Flags::EMBED, Flags::TEXT, Flags::RECORD, Flags::FACETS,
        ] {
            assert!(e.flags.contains(f), "{f:?}");
        }
        assert!(!e.flags.intersects(Flags::VIDEO | Flags::EXTERNAL | Flags::GIF));
        let rec = e.record_json().unwrap();
        assert!(rec.starts_with(r#"{"$type":"app.bsky.feed.post""#) && rec.ends_with("}}"));
        let _: serde_json::Value = serde_json::from_str(rec).unwrap();
        assert_eq!(e.raw(), src);
    }

    #[test]
    fn v2_like_delete_identity_account_sync() {
        let like = ev(r#"{"$type":"message","payload":{"$type":"network.bsky.jetstream.subscribeEvents#commit","did":"did:plc:7e6kocyzb77xkncplrkoojej","seq":5,"time":"2026-08-13T06:47:43.959305Z","operation":"create","collection":"app.bsky.feed.like","rkey":"3k","rev":"r","cid":"c","record":{"$type":"app.bsky.feed.like","createdAt":"2026-08-13T06:47:44.859Z","subject":{"cid":"b","uri":"at://did:plc:gdcz3s2rofr4bbom5bx3kqrr/app.bsky.feed.post/3mjf"}}}}"#);
        assert_eq!(like.field(Field::Subject), "at://did:plc:gdcz3s2rofr4bbom5bx3kqrr/app.bsky.feed.post/3mjf");
        assert!(!like.flags.contains(Flags::TEXT));

        let del = ev(r#"{"$type":"message","payload":{"$type":"network.bsky.jetstream.subscribeEvents#commit","did":"did:web:example.com","seq":6,"time":"2026-08-13T06:47:43.959305Z","operation":"delete","collection":"app.bsky.graph.follow","rkey":"3k","rev":"r"}}"#);
        assert_eq!(del.op, Op::Delete);
        assert!(del.record_json().is_none());
        assert!(!del.flags.contains(Flags::RECORD));
        assert_eq!(del.did.to_string(), "did:web:example.com");

        let id = ev(r#"{"$type":"message","payload":{"$type":"network.bsky.jetstream.subscribeEvents#identity","did":"did:plc:7e6kocyzb77xkncplrkoojej","seq":7,"time":"2026-08-13T06:47:43.959305Z","identity":{"did":"did:plc:7e6kocyzb77xkncplrkoojej","handle":"alice.example","seq":99,"time":"2026-08-13T06:47:43.000Z"}}}"#);
        assert_eq!(id.kind, Kind::Identity);
        assert_eq!(id.seq, 7);
        assert_eq!(id.field(Field::Aux), "alice.example");
        assert!(id.flags.contains(Flags::HANDLE));
        assert!(id.collection.is_none());

        let acct = ev(r#"{"$type":"message","payload":{"$type":"network.bsky.jetstream.subscribeEvents#account","did":"did:plc:7e6kocyzb77xkncplrkoojej","seq":8,"time":"2026-08-13T06:47:43.959305Z","account":{"active":false,"did":"x","seq":1,"status":"takendown","time":"t"}}}"#);
        assert_eq!(acct.kind, Kind::Account);
        assert_eq!(acct.field(Field::Aux), "takendown");
        assert!(!acct.flags.contains(Flags::ACTIVE));

        let sync = ev(r#"{"$type":"message","payload":{"$type":"network.bsky.jetstream.subscribeEvents#sync","did":"did:plc:7e6kocyzb77xkncplrkoojej","seq":9,"time":"2026-08-13T06:47:43.959305Z","sync":{"blocks":{"$bytes":"AAAA"},"did":"x","rev":"r","seq":1,"time":"t"}}}"#);
        assert_eq!(sync.kind, Kind::Sync);
    }

    #[test]
    fn notices() {
        let mut b = EventBuilder::new();
        match decode_json_frame(r#"{"$type":"error","error":"ConsumerTooSlow","message":"too slow"}"#, &mut b).unwrap() {
            Decoded::Notice(n) => {
                assert_eq!(n.level, NoticeLevel::Error);
                assert_eq!((n.name.as_str(), n.message.as_str()), ("ConsumerTooSlow", "too slow"));
            }
            other => panic!("{other:?}"),
        }
        match decode_json_frame(r#"{"$type":"message","payload":{"$type":"network.bsky.jetstream.subscribeEvents#info","name":"OutdatedCursor","message":"resumed from 12"}}"#, &mut b).unwrap() {
            Decoded::Notice(n) => {
                assert_eq!(n.level, NoticeLevel::Info);
                assert_eq!(n.name, "OutdatedCursor");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn v1_golden_corpus() {
        let corpus = include_str!("golden_v1.jsonl");
        let mut kinds = Vec::new();
        for line in corpus.lines().filter(|l| !l.is_empty()) {
            let e = ev(line);
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(e.did.to_string(), v["did"].as_str().unwrap());
            assert_eq!(e.time_us, v["time_us"].as_i64().unwrap());
            assert_eq!(e.seq, e.time_us);
            assert_eq!(e.kind.as_str(), v["kind"].as_str().unwrap());
            if e.kind == Kind::Commit {
                let c = &v["commit"];
                assert_eq!(e.op.as_str(), c["operation"].as_str().unwrap());
                assert_eq!(e.collection_str(), c["collection"].as_str().unwrap());
                assert_eq!(e.field(Field::Rkey), c["rkey"].as_str().unwrap());
                match c.get("record") {
                    Some(rec) => {
                        let got: serde_json::Value = serde_json::from_str(e.record_json().unwrap()).unwrap();
                        assert_eq!(&got, rec);
                        if let Some(t) = rec.get("text").and_then(|t| t.as_str()) {
                            assert_eq!(e.text(), t);
                        }
                        if let Some(sub) = rec.get("subject") {
                            let want = sub.as_str().or_else(|| sub["uri"].as_str()).unwrap();
                            assert_eq!(e.field(Field::Subject), want);
                        }
                    }
                    None => assert!(e.record_json().is_none()),
                }
            }
            if e.kind == Kind::Identity {
                assert_eq!(e.field(Field::Aux), v["identity"]["handle"].as_str().unwrap_or(""));
            }
            if e.kind == Kind::Account {
                assert_eq!(e.flags.contains(Flags::ACTIVE), v["account"]["active"].as_bool().unwrap());
            }
            kinds.push(e.kind);
        }
        assert_eq!(kinds.len(), 10);
        assert!(kinds.contains(&Kind::Identity) && kinds.contains(&Kind::Account));
    }

    #[test]
    fn foreign_lexicons_with_clashing_field_types() {
        let e = ev(r#"{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":1,"kind":"commit","commit":{"rev":"r","operation":"create","collection":"com.example.thing","rkey":"self","record":{"text":42,"embed":"nope","facets":{"a":1},"langs":"en","labels":[1,2],"subject":[1],"reply":"x","tags":[1,"ok",null],"name":{"first":"a"},"title":"T","createdAt":123},"cid":"c"}}"#);
        assert_eq!(e.text(), "");
        assert_eq!(e.field(Field::Name), "T");
        assert_eq!(e.list(Field::Tags).collect::<Vec<_>>(), ["ok"]);
        assert_eq!(e.created_us, 0);
        assert!(!e.flags.intersects(Flags::EMBED | Flags::REPLY | Flags::LANGS | Flags::LABELS));
    }

    #[test]
    fn external_and_video_embeds() {
        let e = ev(r#"{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":1,"kind":"commit","commit":{"operation":"create","collection":"app.bsky.feed.post","rkey":"a","record":{"text":"gif","embed":{"$type":"app.bsky.embed.external","external":{"uri":"https://media.tenor.com/x.gif?hh=1","title":"t","description":"d"}}}}}"#);
        assert!(e.flags.contains(Flags::EXTERNAL | Flags::LINK | Flags::GIF | Flags::EMBED));
        assert_eq!(e.n_links, 1);
        let e = ev(r#"{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":1,"kind":"commit","commit":{"operation":"create","collection":"app.bsky.feed.post","rkey":"a","record":{"text":"vid","embed":{"$type":"app.bsky.embed.video","video":{"$type":"blob"},"alt":"clip"}}}}"#);
        assert!(e.flags.contains(Flags::VIDEO | Flags::ALT));
        assert_eq!(e.field(Field::Alt), "clip");
    }

    #[test]
    fn malformed_frames_are_errors_not_panics() {
        let mut b = EventBuilder::new();
        for src in [
            "",
            "[]",
            "{",
            r#"{"did":"did:plc:x"}"#,
            r#"{"kind":"commit","time_us":1}"#,
            r#"{"did":"d","time_us":1,"kind":"commit","commit":{"operation":"create"}}"#,
            r#"{"did":"d","time_us":1,"kind":"commit","commit":{"record":{"text":"unterminated"#,
        ] {
            assert!(decode_json_frame(src, &mut b).is_err(), "{src}");
        }
        // The builder is reusable after a failure.
        let ok = r#"{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":5,"kind":"identity","identity":{"handle":"h.test"}}"#;
        assert!(matches!(decode_json_frame(ok, &mut b), Ok(Decoded::Event(_))));
    }
}
