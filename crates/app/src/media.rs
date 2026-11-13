//! Media and content labels found in AT Protocol records.
//!
//! Pure functions over record JSON: which images, videos, link cards and
//! other blobs a record carries, where to load a preview from, and which
//! labels (self-applied or from labelers) apply. The UI decides what to load
//! and what to hide.

use serde_json::Value;

const CDN: &str = "https://cdn.bsky.app/img";
const VIDEO: &str = "https://video.bsky.app/watch";

/// Where to load bytes from. Blobs in non-Bluesky records are fetched from
/// the owner's PDS, which first needs the DID resolved.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Src {
    Url(String),
    Blob { did: String, cid: String },
}

impl Src {
    /// A stable cache key.
    pub fn key(&self) -> String {
        match self {
            Src::Url(u) => u.clone(),
            Src::Blob { did, cid } => format!("blob:{did}/{cid}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    /// An animated GIF from a link (e.g. Tenor), shown inline.
    Gif,
    Video,
    /// An external link with an optional thumbnail.
    Link,
    Audio,
    /// A blob of some other type: offered for download only.
    File,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Media {
    pub kind: Kind,
    /// Small preview to show inline (thumbnail, avatar, video poster).
    pub preview: Option<Src>,
    /// Larger version for the viewer (images, GIFs).
    pub full: Option<Src>,
    /// What "open" does: the link, the playable video, the raw blob.
    pub open: Option<Src>,
    /// Alt text, or a link card's title.
    pub alt: String,
    /// A link card's description, or the blob's MIME type.
    pub detail: String,
    /// Width / height, when the record says.
    pub aspect: Option<f32>,
    /// What this is, for display: "image", "avatar", "banner", "video"…
    pub role: &'static str,
}

/// The CID of a blob reference (`{"$type":"blob","ref":{"$link":…}}` or the
/// legacy `{"cid":…,"mimeType":…}` form) and its MIME type.
pub fn blob_cid(v: &Value) -> Option<(&str, &str)> {
    let mime = v["mimeType"].as_str().unwrap_or("");
    if let Some(cid) = v["ref"]["$link"].as_str() {
        return Some((cid, mime));
    }
    if v["$type"].is_null() {
        if let Some(cid) = v["cid"].as_str() {
            if !mime.is_empty() {
                return Some((cid, mime));
            }
        }
    }
    None
}

fn cdn(kind: &str, did: &str, cid: &str) -> Src {
    Src::Url(format!("{CDN}/{kind}/plain/{did}/{cid}@jpeg"))
}

fn aspect(v: &Value) -> Option<f32> {
    let w = v["width"].as_f64()?;
    let h = v["height"].as_f64()?;
    (w > 0.0 && h > 0.0).then_some((w / h) as f32)
}

/// Media in a record owned by `did`. `collection` decides how well-known
/// fields (avatars, embeds) are read; anything else falls back to a scan for
/// blob references.
pub fn extract(did: &str, collection: &str, record: &Value) -> Vec<Media> {
    let mut out = Vec::new();
    match collection {
        "app.bsky.actor.profile" => {
            if let Some((cid, _)) = blob_cid(&record["avatar"]) {
                out.push(image(did, cid, "avatar", "avatar", "avatar_thumbnail"));
            }
            if let Some((cid, _)) = blob_cid(&record["banner"]) {
                out.push(image(did, cid, "banner", "banner", "banner"));
            }
            return out;
        }
        "app.bsky.graph.list" | "app.bsky.feed.generator" | "app.bsky.labeler.service" => {
            if let Some((cid, _)) = blob_cid(&record["avatar"]) {
                out.push(image(did, cid, "avatar", "avatar", "avatar_thumbnail"));
            }
        }
        _ => {}
    }
    embed(did, &record["embed"], &mut out);
    if out.is_empty() && !collection.starts_with("app.bsky.") {
        scan_blobs(did, record, &mut out);
    }
    out
}

fn image(did: &str, cid: &str, role: &'static str, full: &str, thumb: &str) -> Media {
    Media {
        kind: Kind::Image,
        preview: Some(cdn(thumb, did, cid)),
        full: Some(cdn(full, did, cid)),
        open: Some(cdn(full, did, cid)),
        alt: String::new(),
        detail: String::new(),
        aspect: None,
        role,
    }
}

fn embed(did: &str, e: &Value, out: &mut Vec<Media>) {
    match e["$type"].as_str().unwrap_or("") {
        "app.bsky.embed.images" => {
            for i in e["images"].as_array().into_iter().flatten() {
                let Some((cid, _)) = blob_cid(&i["image"]) else { continue };
                out.push(Media {
                    alt: i["alt"].as_str().unwrap_or("").to_owned(),
                    aspect: aspect(&i["aspectRatio"]),
                    ..image(did, cid, "image", "feed_fullsize", "feed_thumbnail")
                });
            }
        }
        "app.bsky.embed.video" => {
            if let Some((cid, _)) = blob_cid(&e["video"]) {
                out.push(Media {
                    kind: Kind::Video,
                    preview: Some(Src::Url(format!("{VIDEO}/{did}/{cid}/thumbnail.jpg"))),
                    full: None,
                    open: Some(Src::Url(format!("{VIDEO}/{did}/{cid}/playlist.m3u8"))),
                    alt: e["alt"].as_str().unwrap_or("").to_owned(),
                    detail: String::new(),
                    aspect: aspect(&e["aspectRatio"]),
                    role: "video",
                });
            }
        }
        "app.bsky.embed.external" => {
            let x = &e["external"];
            let uri = x["uri"].as_str().unwrap_or("");
            let thumb = blob_cid(&x["thumb"]).map(|(cid, _)| cdn("feed_thumbnail", did, cid));
            let gif = is_gif(uri);
            out.push(Media {
                kind: if gif { Kind::Gif } else { Kind::Link },
                preview: if gif { Some(Src::Url(uri.to_owned())) } else { thumb },
                full: gif.then(|| Src::Url(uri.to_owned())),
                open: (!uri.is_empty()).then(|| Src::Url(uri.to_owned())),
                alt: x["title"].as_str().unwrap_or("").to_owned(),
                detail: x["description"].as_str().unwrap_or("").to_owned(),
                aspect: None,
                role: if gif { "gif" } else { "link" },
            });
        }
        "app.bsky.embed.recordWithMedia" => embed(did, &e["media"], out),
        _ => {}
    }
}

/// Links that are animated GIFs (Tenor and plain `.gif` URLs).
fn is_gif(uri: &str) -> bool {
    let path = uri.split(['?', '#']).next().unwrap_or("");
    path.ends_with(".gif") || (uri.contains("media.tenor.com/") && path.ends_with(".gif"))
}

/// Every blob reference anywhere in a record, for lexicons vortex doesn't
/// know. Images get an inline preview; other types can be opened.
fn scan_blobs(did: &str, v: &Value, out: &mut Vec<Media>) {
    if out.len() >= 16 {
        return;
    }
    if let Some((cid, mime)) = blob_cid(v) {
        let src = Src::Blob { did: did.to_owned(), cid: cid.to_owned() };
        let kind = match mime.split('/').next() {
            Some("image") => Kind::Image,
            Some("video") => Kind::Video,
            Some("audio") => Kind::Audio,
            _ => Kind::File,
        };
        out.push(Media {
            kind,
            preview: (kind == Kind::Image).then(|| src.clone()),
            full: (kind == Kind::Image).then(|| src.clone()),
            open: Some(src),
            alt: String::new(),
            detail: mime.to_owned(),
            aspect: None,
            role: match kind {
                Kind::Image => "image",
                Kind::Video => "video",
                Kind::Audio => "audio",
                _ => "file",
            },
        });
        return;
    }
    match v {
        Value::Object(m) => m.values().for_each(|c| scan_blobs(did, c, out)),
        Value::Array(a) => a.iter().for_each(|c| scan_blobs(did, c, out)),
        _ => {}
    }
}

/// An avatar URL from the AppView, at thumbnail size.
pub fn avatar_thumb(url: &str) -> String {
    url.replacen("/img/avatar/plain/", "/img/avatar_thumbnail/plain/", 1)
}

// ---- labels ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Informational (e.g. `!no-unauthenticated`).
    Info,
    /// A warning from a labeler (`!warn`, `spam`, …).
    Warn,
    /// Sexual content or nudity.
    Adult,
    /// Violence, gore, graphic imagery.
    Graphic,
    /// A labeler asks clients to hide the content.
    Hide,
}

impl Severity {
    /// Whether media under this label should be hidden until revealed.
    pub fn hides_media(self) -> bool {
        self >= Severity::Adult || self == Severity::Warn
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub val: String,
    /// `None` for a self-label set by the author; else the labeler's DID.
    pub src: Option<String>,
}

impl Label {
    pub fn severity(&self) -> Severity {
        severity(&self.val)
    }
}

pub fn severity(val: &str) -> Severity {
    match val {
        "porn" | "sexual" | "nudity" | "sexual-figurative" => Severity::Adult,
        "graphic-media" | "gore" | "corpse" | "self-harm" => Severity::Graphic,
        "!hide" | "!takedown" => Severity::Hide,
        "!no-unauthenticated" | "!no-promote" => Severity::Info,
        _ => Severity::Warn,
    }
}

/// Self-labels on a record (`labels.values[].val`).
pub fn self_labels(record: &Value) -> Vec<Label> {
    record["labels"]["values"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["val"].as_str())
        .map(|val| Label { val: val.to_owned(), src: None })
        .collect()
}

/// Labels on an AppView view (post or profile): self-labels arrive here too,
/// with the subject's own DID as `src`. Negations are dropped.
pub fn view_labels(view: &Value, subject_did: &str) -> Vec<Label> {
    view["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|l| !l["neg"].as_bool().unwrap_or(false))
        .filter_map(|l| {
            let val = l["val"].as_str()?.to_owned();
            let src = l["src"].as_str().filter(|s| *s != subject_did).map(str::to_owned);
            Some(Label { val, src })
        })
        .collect()
}

/// Union of label lists, keeping the first of duplicates (self before
/// labeler, by the order given).
pub fn merge(lists: impl IntoIterator<Item = Vec<Label>>) -> Vec<Label> {
    let mut out: Vec<Label> = Vec::new();
    for l in lists.into_iter().flatten() {
        if !out.iter().any(|o| o.val == l.val && o.src == l.src) {
            out.push(l);
        }
    }
    out.sort_by_key(|l| std::cmp::Reverse(l.severity()));
    out
}

/// The strongest label that hides media, if any.
pub fn media_gate(labels: &[Label]) -> Option<&Label> {
    labels.iter().filter(|l| l.severity().hides_media()).max_by_key(|l| l.severity())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DID: &str = "did:plc:abc";

    #[test]
    fn images_video_links_and_gifs() {
        let post = json!({"embed": {"$type": "app.bsky.embed.images", "images": [
            {"alt": "a cat", "image": {"$type": "blob", "ref": {"$link": "bafyimg"}, "mimeType": "image/jpeg"},
             "aspectRatio": {"width": 4, "height": 3}}]}});
        let m = extract(DID, "app.bsky.feed.post", &post);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].kind, Kind::Image);
        assert_eq!(m[0].alt, "a cat");
        assert_eq!(m[0].aspect, Some(4.0 / 3.0));
        assert_eq!(
            m[0].preview,
            Some(Src::Url("https://cdn.bsky.app/img/feed_thumbnail/plain/did:plc:abc/bafyimg@jpeg".into()))
        );

        let video = json!({"embed": {"$type": "app.bsky.embed.recordWithMedia", "media": {
            "$type": "app.bsky.embed.video", "video": {"$type": "blob", "ref": {"$link": "bafyvid"}, "mimeType": "video/mp4"}}}});
        let m = extract(DID, "app.bsky.feed.post", &video);
        assert_eq!(m[0].kind, Kind::Video);
        assert_eq!(m[0].open, Some(Src::Url("https://video.bsky.app/watch/did:plc:abc/bafyvid/playlist.m3u8".into())));

        let gif = json!({"embed": {"$type": "app.bsky.embed.external", "external": {
            "uri": "https://media.tenor.com/abc/cat.gif?hh=200&ww=200", "title": "Cat GIF", "description": "ALT: a cat"}}});
        assert_eq!(extract(DID, "app.bsky.feed.post", &gif)[0].kind, Kind::Gif);

        let link = json!({"embed": {"$type": "app.bsky.embed.external", "external": {
            "uri": "https://example.com/story", "title": "Story", "description": "d",
            "thumb": {"$type": "blob", "ref": {"$link": "bafythumb"}, "mimeType": "image/jpeg"}}}});
        let m = extract(DID, "app.bsky.feed.post", &link);
        assert_eq!((m[0].kind, m[0].alt.as_str()), (Kind::Link, "Story"));
        assert!(m[0].preview.is_some());
    }

    #[test]
    fn profiles_and_unknown_lexicons() {
        let profile = json!({
            "avatar": {"$type": "blob", "ref": {"$link": "bafyav"}, "mimeType": "image/png"},
            "banner": {"$type": "blob", "ref": {"$link": "bafyban"}, "mimeType": "image/jpeg"}});
        let m = extract(DID, "app.bsky.actor.profile", &profile);
        assert_eq!(m.iter().map(|m| m.role).collect::<Vec<_>>(), ["avatar", "banner"]);

        let other = json!({"track": {"cover": {"$type": "blob", "ref": {"$link": "bafycover"}, "mimeType": "image/webp"},
            "audio": {"$type": "blob", "ref": {"$link": "bafyaudio"}, "mimeType": "audio/mpeg"}}});
        let m = extract(DID, "fm.example.track", &other);
        let kinds: Vec<_> = m.iter().map(|m| m.kind).collect();
        assert!(kinds.contains(&Kind::Image) && kinds.contains(&Kind::Audio), "{kinds:?}");
        assert!(m.iter().all(|m| matches!(m.open, Some(Src::Blob { .. }))));
        assert!(extract(DID, "app.bsky.feed.post", &json!({"text": "hi"})).is_empty());
    }

    #[test]
    fn labels_gate_media() {
        let rec = json!({"labels": {"$type": "com.atproto.label.defs#selfLabels", "values": [{"val": "nudity"}]}});
        let selfl = self_labels(&rec);
        assert_eq!(selfl, [Label { val: "nudity".into(), src: None }]);
        let view = json!({"labels": [
            {"src": DID, "val": "nudity"},
            {"src": "did:plc:labeler", "val": "graphic-media"},
            {"src": "did:plc:labeler", "val": "porn", "neg": true}]});
        let all = merge([selfl, view_labels(&view, DID)]);
        assert_eq!(all.len(), 2, "{all:?}");
        assert_eq!(all[0].val, "graphic-media", "strongest first");
        assert_eq!(media_gate(&all).unwrap().val, "graphic-media");
        assert!(media_gate(&[Label { val: "!no-unauthenticated".into(), src: None }]).is_none());
        assert!(severity("spam").hides_media());
    }

    #[test]
    fn avatar_thumbnails() {
        assert_eq!(
            avatar_thumb("https://cdn.bsky.app/img/avatar/plain/did:plc:x/bafy@jpeg"),
            "https://cdn.bsky.app/img/avatar_thumbnail/plain/did:plc:x/bafy@jpeg"
        );
    }
}
