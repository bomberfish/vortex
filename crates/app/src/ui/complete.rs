//! Filter-bar completion: field names, then values for the field being typed.

use std::ops::Range;

/// One field the query language understands.
pub struct FieldDoc {
    /// Text inserted on accept (usually `name:`).
    pub insert: &'static str,
    /// One-line meaning.
    pub doc: &'static str,
    /// Known values offered after the operator.
    pub values: &'static [&'static str],
    /// A value that makes `insert + example` a valid query (checked in tests).
    #[cfg_attr(not(test), allow(dead_code))]
    pub example: &'static str,
}

const TYPES: &[&str] = &[
    "post",
    "like",
    "repost",
    "follow",
    "block",
    "profile",
    "list",
    "listitem",
    "listblock",
    "feedgen",
    "threadgate",
    "postgate",
    "starterpack",
    "labeler",
    "verification",
    "chat",
    "bsky",
];
/// The NSID a `type:` alias stands for, shown next to the suggestion.
fn type_nsid(field: &str, alias: &str) -> &'static str {
    if field != "type" {
        return "";
    }
    match alias {
        "post" => "app.bsky.feed.post",
        "like" => "app.bsky.feed.like",
        "repost" => "app.bsky.feed.repost",
        "follow" => "app.bsky.graph.follow",
        "block" => "app.bsky.graph.block",
        "profile" => "app.bsky.actor.profile",
        "list" => "app.bsky.graph.list",
        "listitem" => "app.bsky.graph.listitem",
        "listblock" => "app.bsky.graph.listblock",
        "feedgen" => "app.bsky.feed.generator",
        "threadgate" => "app.bsky.feed.threadgate",
        "postgate" => "app.bsky.feed.postgate",
        "starterpack" => "app.bsky.graph.starterpack",
        "labeler" => "app.bsky.labeler.service",
        "verification" => "app.bsky.graph.verification",
        "chat" => "chat.bsky.actor.declaration",
        "bsky" => "app.bsky.*",
        _ => "",
    }
}

const FLAGS: &[&str] = &[
    "media", "image", "video", "reply", "quote", "link", "external", "mention", "tag", "label", "lang", "alt", "embed",
    "gif", "facets", "text", "name", "subject", "created",
];

pub const FIELDS: &[FieldDoc] = &[
    FieldDoc { insert: "type:", doc: "record type: post like follow … or an NSID", values: TYPES, example: "post" },
    FieldDoc {
        insert: "kind:",
        doc: "commit · identity · account · sync",
        values: &["commit", "identity", "account", "sync"],
        example: "commit",
    },
    FieldDoc {
        insert: "op:",
        doc: "create · update · delete",
        values: &["create", "update", "delete"],
        example: "create",
    },
    FieldDoc { insert: "has:", doc: "has a feature: media reply link tag …", values: FLAGS, example: "media" },
    FieldDoc {
        insert: "is:",
        doc: "is a reply / quote / root post, or active account",
        values: &["reply", "quote", "root", "active", "create", "update", "delete"],
        example: "reply",
    },
    FieldDoc {
        insert: "embed:",
        doc: "embed type",
        values: &["image", "video", "external", "record", "media", "gif", "none"],
        example: "video",
    },
    FieldDoc {
        insert: "lang:",
        doc: "post language (en matches en-US)",
        values: &["en", "ja", "pt", "de", "es", "fr", "ko", "zh", "it", "nl", "tr", "ru"],
        example: "en",
    },
    FieldDoc { insert: "text:", doc: "post text contains (case-insensitive)", values: &[], example: "cats" },
    FieldDoc { insert: "word:", doc: "whole word in post text", values: &[], example: "cat" },
    FieldDoc { insert: "name:", doc: "display name contains", values: &[], example: "bob" },
    FieldDoc { insert: "alt:", doc: "image alt text contains", values: &[], example: "sunset" },
    FieldDoc { insert: "tag:", doc: "hashtag (also #tag)", values: &[], example: "art" },
    FieldDoc {
        insert: "did:",
        doc: "author DID · did:@file loads a list",
        values: &[],
        example: "did:plc:z72i7hdynmk6r22z27h6tvur",
    },
    FieldDoc { insert: "mention:", doc: "mentions this DID", values: &[], example: "did:plc:z72i7hdynmk6r22z27h6tvur" },
    FieldDoc {
        insert: "subject:",
        doc: "like / follow / block target contains (a DID works)",
        values: &[],
        example: "did:plc:z72i7hdynmk6r22z27h6tvur",
    },
    FieldDoc {
        insert: "reply-to:",
        doc: "parent post AT-URI contains",
        values: &[],
        example: "did:plc:z72i7hdynmk6r22z27h6tvur",
    },
    FieldDoc {
        insert: "root:",
        doc: "thread root AT-URI contains",
        values: &[],
        example: "did:plc:z72i7hdynmk6r22z27h6tvur",
    },
    FieldDoc {
        insert: "quote:",
        doc: "quoted post AT-URI contains",
        values: &[],
        example: "did:plc:z72i7hdynmk6r22z27h6tvur",
    },
    FieldDoc { insert: "link:", doc: "link URL contains", values: &[], example: "youtube" },
    FieldDoc {
        insert: "domain:",
        doc: "link host (subdomains match)",
        values: &["github.com", "youtube.com", "youtu.be", "nytimes.com"],
        example: "github.com",
    },
    FieldDoc {
        insert: "label:",
        doc: "self-label",
        values: &["porn", "sexual", "nudity", "graphic-media", "!no-unauthenticated"],
        example: "porn",
    },
    FieldDoc {
        insert: "collection:",
        doc: "NSID, globs ok: app.bsky.graph.*",
        values: &[],
        example: "app.bsky.feed.post",
    },
    FieldDoc { insert: "handle:", doc: "identity events: new handle", values: &[], example: "bob.bsky.social" },
    FieldDoc {
        insert: "status:",
        doc: "account events: status",
        values: &["takendown", "suspended", "deleted", "deactivated", "desynchronized", "throttled"],
        example: "takendown",
    },
    FieldDoc { insert: "active:", doc: "account events: active or not", values: &["true", "false"], example: "false" },
    FieldDoc { insert: "uri:", doc: "AT-URI contains", values: &[], example: "app.bsky.feed.post" },
    FieldDoc { insert: "rkey:", doc: "record key", values: &["self"], example: "self" },
    FieldDoc { insert: "raw:", doc: "raw frame contains", values: &[], example: "bsky" },
    FieldDoc { insert: "len>", doc: "post text length in characters", values: &["100", "280"], example: "200" },
    FieldDoc { insert: "images>=", doc: "number of images", values: &["2", "4"], example: "2" },
    FieldDoc { insert: "tags>=", doc: "number of hashtags", values: &[], example: "3" },
    FieldDoc { insert: "mentions>=", doc: "number of mentions", values: &[], example: "1" },
    FieldDoc { insert: "links>=", doc: "number of links", values: &[], example: "1" },
    FieldDoc { insert: "langs>", doc: "number of declared languages", values: &[], example: "1" },
    FieldDoc { insert: "bytes>", doc: "frame size in bytes", values: &["10000"], example: "10000" },
    FieldDoc { insert: "time>", doc: "event time: 2026-01-01T00:00Z", values: &[], example: "2026-01-01T00:00Z" },
    FieldDoc { insert: "created>", doc: "record createdAt", values: &[], example: "2026-01-01T00:00Z" },
    FieldDoc {
        insert: "skew>",
        doc: "createdAt − event time: skew>1h, skew<10m",
        values: &["10m", "1h", "1d"],
        example: "1h",
    },
    FieldDoc { insert: "seq>", doc: "cursor / sequence number", values: &[], example: "0" },
    FieldDoc { insert: "sample:", doc: "keep a deterministic fraction", values: &["1%", "10%", "50%"], example: "10%" },
    FieldDoc {
        insert: "exists:",
        doc: "JSON path exists: record.embed",
        values: &["record.embed", "record.reply", "record.labels"],
        example: "record.embed",
    },
    FieldDoc { insert: "record.", doc: "any record field: record.embed.$type:…", values: &[], example: "text:hi" },
    FieldDoc { insert: "json.", doc: "any frame field: json.commit.rev:…", values: &[], example: "did:x" },
];

#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    /// What replaces the token.
    pub insert: String,
    /// Shown left, highlighted.
    pub label: String,
    /// Shown right, dim.
    pub doc: String,
}

/// Completions for the token ending at `cursor`, plus the byte range it
/// replaces. `nsids` are collections seen in the stream, most common first.
pub fn complete(text: &str, cursor: usize, nsids: &[&str]) -> (Range<usize>, Vec<Completion>) {
    const MAX: usize = 8;
    let cursor = cursor.min(text.len());
    let before = &text[..cursor];
    // Inside an open quote: free text, nothing to suggest.
    if before.matches('"').count() % 2 == 1 {
        return (cursor..cursor, Vec::new());
    }
    let mut start = before.rfind(|c: char| c.is_whitespace() || c == '(' || c == ')').map_or(0, |i| i + 1);
    let mut token = &before[start..];
    if let Some(rest) = token.strip_prefix(['-', '!']) {
        start += 1;
        token = rest;
    }
    if token.is_empty() || token.starts_with('#') {
        return (cursor..cursor, Vec::new());
    }

    // `field<op>value`: complete the value (after the last comma).
    if let Some(op_at) = token.find([':', '=', '~', '<', '>', '!']) {
        let field_name = token[..op_at].to_ascii_lowercase();
        let ops_end = op_at
            + token[op_at..]
                .find(|c: char| !matches!(c, ':' | '=' | '~' | '<' | '>' | '!'))
                .unwrap_or(token.len() - op_at);
        let value = &token[ops_end..];
        let value_start = value.rfind(',').map_or(0, |i| i + 1);
        let prefix = value[value_start..].to_ascii_lowercase();
        let range = start + ops_end + value_start..cursor;
        let Some(field) =
            FIELDS.iter().find(|f| f.insert.trim_end_matches([':', '=', '~', '<', '>', '!']) == field_name)
        else {
            return (range, Vec::new());
        };
        let mut out: Vec<Completion> = field
            .values
            .iter()
            .filter(|v| v.starts_with(&prefix) && **v != prefix)
            .map(|v| Completion {
                insert: (*v).to_owned(),
                label: (*v).to_owned(),
                doc: type_nsid(&field_name, v).to_owned(),
            })
            .collect();
        if matches!(field_name.as_str(), "type" | "collection") {
            for n in nsids {
                if out.len() >= MAX {
                    break;
                }
                if n.starts_with(&prefix) && *n != prefix && !out.iter().any(|c| c.insert == *n) {
                    out.push(Completion { insert: (*n).to_owned(), label: (*n).to_owned(), doc: String::new() });
                }
            }
        }
        out.truncate(MAX);
        return (range, out);
    }

    // A bare word: offer field names it could be the start of.
    let lower = token.to_ascii_lowercase();
    let out: Vec<Completion> = FIELDS
        .iter()
        .filter(|f| f.insert.starts_with(&lower) && f.insert.len() > lower.len())
        .take(MAX)
        .map(|f| Completion { insert: f.insert.to_owned(), label: f.insert.to_owned(), doc: f.doc.to_owned() })
        .collect();
    (start..cursor, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_core::filter::Filter;

    #[test]
    fn every_suggestion_parses() {
        for f in FIELDS {
            let q = format!("{}{}", f.insert, f.example);
            assert!(Filter::parse(&q).is_ok(), "{q}: {:?}", Filter::parse(&q).err());
            for v in f.values {
                let q = format!("{}{v}", f.insert);
                assert!(Filter::parse(&q).is_ok(), "{q}: {:?}", Filter::parse(&q).err());
            }
        }
    }

    #[test]
    fn fields_then_values() {
        let (r, c) = complete("lang:en ty", 10, &[]);
        assert_eq!(r, 8..10);
        assert_eq!(c[0].insert, "type:");
        let (r, c) = complete("type:po", 7, &[]);
        assert_eq!(r, 5..7);
        assert_eq!(c.iter().map(|c| c.insert.as_str()).collect::<Vec<_>>(), ["post", "postgate"]);
        let (r, c) = complete("-has:media,vi", 13, &[]);
        assert_eq!((r, c[0].insert.as_str()), (11..13, "video"));
        let (_, c) = complete("type:app.bsky.f", 15, &["app.bsky.feed.like", "app.bsky.graph.follow"]);
        assert_eq!(c[0].insert, "app.bsky.feed.like");
        assert!(complete("\"type", 5, &[]).1.is_empty());
        assert!(complete("#ar", 3, &[]).1.is_empty());
        assert!(complete("len>", 4, &[]).1.iter().any(|c| c.insert == "200" || c.insert == "100"));
        // Every type alias is documented with the NSID it expands to.
        for v in TYPES {
            assert!(!type_nsid("type", v).is_empty(), "{v}");
            let q = format!("type:{v}");
            let nsid = type_nsid("type", v);
            if !nsid.ends_with('*') {
                assert!(Filter::parse(&q).unwrap().pushdown().summary().contains(nsid), "{q}");
            }
        }
    }
}
