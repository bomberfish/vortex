//! The two non-repo streams pdsls also offers, mapped onto [`Event`]s:
//!
//! - **Spacedust** (`spacedust.microcosm.blue/subscribe`): links between
//!   records as they're made. Each becomes a commit event for the linking
//!   record (its DID, collection and rkey), with the link target as the
//!   subject and the link's path (`app.bsky.feed.like:subject.uri`) in
//!   [`Field::Aux`]. The `link` object is the event's record.
//! - **PLC** (`plc.directory/export/stream`): `did:plc` operations. Each
//!   becomes an identity event: create (no `prev`), update, or delete for a
//!   tombstone, with the handle in [`Field::Aux`] and the PDS endpoint in
//!   [`Field::Links`]. The `operation` object is the event's record.
//!
//! [`Event`]: crate::event::Event

use super::{DecodeError, Decoded, Notice, NoticeLevel};
use crate::did::Did;
use crate::event::{EventBuilder, Field, Flags, Kind, Op};
use crate::json::{Result as JsonResult, Scanner};
use crate::nsid::Nsid;
use crate::time::{now_us, parse_rfc3339_us};

pub fn decode_spacedust_frame(src: &str, b: &mut EventBuilder) -> Result<Decoded, DecodeError> {
    b.reset();
    let mut s = Scanner::new(src);
    if !s.object()? {
        return Err(DecodeError::Malformed("frame is not an object"));
    }
    let mut kind = String::new();
    let mut saw_link = false;
    while let Some(k) = s.key()? {
        match k {
            "kind" => {
                if let Some(v) = s.str()? {
                    kind = v.to_cow().into_owned();
                }
            }
            "link" => {
                s.peek();
                let start = s.pos();
                let raw = s.raw()?;
                b.rec = (start as u32, raw.len() as u32);
                walk_link(raw, b)?;
                saw_link = true;
            }
            _ => s.skip()?,
        }
    }
    if kind != "link" || !saw_link {
        let name = if kind.is_empty() { "message".to_owned() } else { kind };
        return Ok(Decoded::Notice(Notice { level: NoticeLevel::Info, name, message: one_line(src) }));
    }
    if b.did.is_none() || b.collection.is_none() {
        return Err(DecodeError::Malformed("link has no source record"));
    }
    b.kind = Some(Kind::Commit);
    // Spacedust has no cursor or timestamps: events are stamped on arrival
    // and never deduplicated (see the pipeline).
    b.time_us = now_us();
    b.seq = 0;
    b.finish(src).map(Decoded::Event).ok_or(DecodeError::Malformed("event too large"))
}

fn walk_link(raw: &str, b: &mut EventBuilder) -> JsonResult<()> {
    let mut s = Scanner::new(raw);
    if !s.object()? {
        return Ok(());
    }
    while let Some(k) = s.key()? {
        match k {
            "operation" => {
                if let Some(v) = s.str()? {
                    b.op = Op::parse(&v.to_cow()).unwrap_or(Op::None);
                }
            }
            "source" => {
                if let Some(v) = s.str()? {
                    v.push_to(b.buf(Field::Aux));
                }
            }
            "source_record" => {
                if let Some(v) = s.str()? {
                    let uri = v.to_cow();
                    let mut parts = uri.trim_start_matches("at://").splitn(3, '/');
                    if let Some(did) = parts.next().filter(|d| d.starts_with("did:")) {
                        b.did = Some(Did::parse(did));
                    }
                    if let Some(coll) = parts.next().filter(|c| !c.is_empty()) {
                        b.collection = Some(Nsid::intern(coll));
                    }
                    if let Some(rkey) = parts.next() {
                        b.buf(Field::Rkey).push_str(rkey);
                    }
                }
            }
            "subject" => {
                if let Some(v) = s.str()? {
                    v.push_to(b.buf(Field::Subject));
                }
            }
            _ => s.skip()?,
        }
    }
    Ok(())
}

pub fn decode_plc_frame(src: &str, b: &mut EventBuilder) -> Result<Decoded, DecodeError> {
    b.reset();
    let mut s = Scanner::new(src);
    if !s.object()? {
        return Err(DecodeError::Malformed("frame is not an object"));
    }
    let mut ty = String::new();
    let mut op_raw = None;
    while let Some(k) = s.key()? {
        match k {
            "seq" => b.seq = s.i64()?.unwrap_or(0),
            "type" => {
                if let Some(v) = s.str()? {
                    ty = v.to_cow().into_owned();
                }
            }
            "did" => {
                if let Some(v) = s.str()? {
                    b.did = Some(Did::parse(&v.to_cow()));
                }
            }
            "createdAt" => {
                if let Some(us) = s.str()?.and_then(|v| parse_rfc3339_us(&v.to_cow())) {
                    b.time_us = us;
                    b.created_us = us;
                }
            }
            "operation" => {
                s.peek();
                let start = s.pos();
                op_raw = Some((start, s.raw()?));
            }
            _ => s.skip()?,
        }
    }
    if !ty.is_empty() && ty != "sequenced_op" {
        return Ok(Decoded::Notice(Notice { level: NoticeLevel::Info, name: ty, message: one_line(src) }));
    }
    let Some((start, raw)) = op_raw else {
        return Err(DecodeError::Malformed("plc event has no operation"));
    };
    if b.did.is_none() {
        return Err(DecodeError::Malformed("event has no did"));
    }
    b.rec = (start as u32, raw.len() as u32);
    walk_plc_op(raw, b)?;
    b.kind = Some(Kind::Identity);
    if b.time_us == 0 {
        b.time_us = now_us();
    }
    b.finish(src).map(Decoded::Event).ok_or(DecodeError::Malformed("event too large"))
}

fn walk_plc_op(raw: &str, b: &mut EventBuilder) -> JsonResult<()> {
    let mut s = Scanner::new(raw);
    if !s.object()? {
        return Ok(());
    }
    let (mut tombstone, mut genesis) = (false, false);
    let mut handle = String::new();
    let mut pds = String::new();
    while let Some(k) = s.key()? {
        match k {
            "type" => {
                if let Some(v) = s.str()? {
                    tombstone = v.raw() == "plc_tombstone";
                }
            }
            "prev" => {
                genesis = s.peek() == Some(b'n');
                s.skip()?;
            }
            "alsoKnownAs" => {
                if s.array()? {
                    while s.elem()? {
                        match s.str()? {
                            Some(v) if handle.is_empty() => handle = v.to_cow().trim_start_matches("at://").to_owned(),
                            _ => {}
                        }
                    }
                }
            }
            // Legacy (`create`) operations.
            "handle" => {
                if let Some(v) = s.str()? {
                    if handle.is_empty() {
                        handle = v.to_cow().into_owned();
                    }
                }
            }
            "service" => {
                if let Some(v) = s.str()? {
                    pds = v.to_cow().into_owned();
                }
            }
            "services" => {
                if s.object()? {
                    while let Some(svc) = s.key()? {
                        if svc == "atproto_pds" && s.object()? {
                            while let Some(f) = s.key()? {
                                if let ("endpoint", Some(v)) = (f, s.str()?) {
                                    pds = v.to_cow().into_owned();
                                }
                            }
                        } else if svc != "atproto_pds" {
                            s.skip()?;
                        }
                    }
                }
            }
            _ => s.skip()?,
        }
    }
    b.op = if tombstone {
        Op::Delete
    } else if genesis {
        Op::Create
    } else {
        Op::Update
    };
    if !handle.is_empty() {
        b.buf(Field::Aux).push_str(&handle);
        b.flags |= Flags::HANDLE;
    }
    if !pds.is_empty() {
        b.buf(Field::Links).push_str(&pds);
        b.n_links = 1;
    }
    Ok(())
}

fn one_line(s: &str) -> String {
    s.chars().take(200).collect::<String>().replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(d: Decoded) -> crate::event::Event {
        match d {
            Decoded::Event(e) => e,
            Decoded::Notice(n) => panic!("notice: {n:?}"),
        }
    }

    #[test]
    fn spacedust_links() {
        let src = r#"{"kind":"link","origin":"live","link":{"operation":"create","source":"app.bsky.feed.like:subject.uri","source_record":"at://did:plc:dxgvtmp6htz5gxbexd2tgccb/app.bsky.feed.like/3n26kbfx7y42m","source_rev":"3n26kbfxgsu2m","subject":"at://did:plc:ybzpvop7xhl7dxeitckhgn72/app.bsky.feed.post/3n26e5bc4ek2q"}}"#;
        let mut b = EventBuilder::new();
        let e = event(decode_spacedust_frame(src, &mut b).unwrap());
        assert_eq!(e.kind, Kind::Commit);
        assert_eq!(e.op, Op::Create);
        assert_eq!(e.did.to_string(), "did:plc:dxgvtmp6htz5gxbexd2tgccb");
        assert_eq!(e.collection_str(), "app.bsky.feed.like");
        assert_eq!(e.field(Field::Rkey), "3n26kbfx7y42m");
        assert_eq!(e.field(Field::Subject), "at://did:plc:ybzpvop7xhl7dxeitckhgn72/app.bsky.feed.post/3n26e5bc4ek2q");
        assert_eq!(e.field(Field::Aux), "app.bsky.feed.like:subject.uri");
        assert!(e.record_json().unwrap().starts_with(r#"{"operation":"create""#));
        assert!(e.time_us > 0);
        let other = decode_spacedust_frame(r#"{"kind":"hello"}"#, &mut b).unwrap();
        assert!(matches!(other, Decoded::Notice(_)));
    }

    #[test]
    fn plc_operations() {
        let src = r#"{"seq":136297949,"type":"sequenced_op","did":"did:plc:c2nyhfzkp2irdan643gj55ux","operation":{"sig":"x","prev":null,"type":"plc_operation","services":{"atproto_pds":{"type":"AtprotoPersonalDataServer","endpoint":"https://pds.example.com"}},"alsoKnownAs":["at://alice.example.com","at://other.example"],"rotationKeys":["did:key:z1"],"verificationMethods":{"atproto":"did:key:z2"}},"cid":"bafy","createdAt":"2026-11-13T08:17:23.469Z"}"#;
        let mut b = EventBuilder::new();
        let e = event(decode_plc_frame(src, &mut b).unwrap());
        assert_eq!(e.kind, Kind::Identity);
        assert_eq!(e.op, Op::Create);
        assert_eq!(e.seq, 136297949);
        assert_eq!(e.field(Field::Aux), "alice.example.com");
        assert_eq!(e.field(Field::Links), "https://pds.example.com");
        assert!(e.record_json().unwrap().starts_with(r#"{"sig""#));
        assert!(crate::time::format_rfc3339_us(e.time_us).starts_with("2026-11-13T08:17:23.469"));

        let update = src.replace(r#""prev":null"#, r#""prev":"bafyprev""#);
        assert_eq!(event(decode_plc_frame(&update, &mut b).unwrap()).op, Op::Update);
        let tomb = r#"{"seq":5,"type":"sequenced_op","did":"did:plc:c2nyhfzkp2irdan643gj55ux","operation":{"sig":"x","prev":"bafy","type":"plc_tombstone"},"cid":"c","createdAt":"2026-11-13T08:17:23.469Z"}"#;
        assert_eq!(event(decode_plc_frame(tomb, &mut b).unwrap()).op, Op::Delete);
        // Hiding a spammy PDS hides its identity events and nothing else.
        let spam = event(decode_plc_frame(&src.replace("pds.example.com", "pds.spam.example"), &mut b).unwrap());
        let hosts = vec!["pds.spam.example".to_owned()];
        let f = crate::filter::Filter::parse("").unwrap().excluding_pds(&hosts);
        assert!(!f.matches(&spam), "{f:?} {:?} {:?}", spam.field(Field::Links), spam.flags);
        assert!(f.matches(&event(decode_plc_frame(src, &mut b).unwrap())));
        assert!(!f.is_all());
        let f = crate::filter::Filter::parse("kind:identity").unwrap().excluding_pds(&[]);
        assert!(f.matches(&spam));
        let legacy = r#"{"seq":6,"did":"did:plc:c2nyhfzkp2irdan643gj55ux","operation":{"type":"create","handle":"bob.bsky.social","service":"https://bsky.social","prev":null},"createdAt":"2023-01-01T00:00:00Z"}"#;
        let e = event(decode_plc_frame(legacy, &mut b).unwrap());
        assert_eq!((e.field(Field::Aux), e.field(Field::Links)), ("bob.bsky.social", "https://bsky.social"));
    }
}
