//! Rich views of records: post text with clickable mentions, links and tags;
//! post and profile cards (with quotes, media and labels); what a record
//! refers to; TID timestamps; and a field summary that works for any lexicon.

use super::lookup::{Fetch, Ref};
use super::*;
use crate::atp::{parse_target, AtTarget};
use gpui::{relative, AnyElement, ElementId, HighlightStyle, InteractiveText};
use serde_json::Value;
use std::ops::Range;
use vortex_core::time::parse_rfc3339_us;

/// What a span of post text links to.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Facet {
    Mention(String),
    Link(String),
    Tag(String),
}

/// A record's facets as byte ranges of `text`, sorted and non-overlapping.
/// Facets that don't land on character boundaries (or overlap an earlier
/// one) are malformed and skipped.
pub(super) fn facet_spans(text: &str, facets: &Value) -> Vec<(Range<usize>, Facet)> {
    let mut out = Vec::new();
    for f in facets.as_array().into_iter().flatten() {
        let (Some(s), Some(e)) = (f["index"]["byteStart"].as_u64(), f["index"]["byteEnd"].as_u64()) else {
            continue;
        };
        let (s, e) = (s as usize, e as usize);
        if s >= e || e > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            continue;
        }
        let Some(feat) = f["features"].as_array().and_then(|a| a.first()) else { continue };
        let facet = match feat["$type"].as_str().unwrap_or("") {
            "app.bsky.richtext.facet#mention" => feat["did"].as_str().map(|d| Facet::Mention(d.to_owned())),
            "app.bsky.richtext.facet#link" => feat["uri"].as_str().map(|u| Facet::Link(u.to_owned())),
            "app.bsky.richtext.facet#tag" => feat["tag"].as_str().map(|t| Facet::Tag(t.to_owned())),
            _ => None,
        };
        if let Some(facet) = facet {
            out.push((s..e, facet));
        }
    }
    out.sort_by_key(|(r, _)| r.start);
    let mut end = 0;
    out.retain(|(r, _)| {
        let keep = r.start >= end;
        if keep {
            end = r.end;
        }
        keep
    });
    out
}

const TID_ALPHABET: &[u8] = b"234567abcdefghijklmnopqrstuvwxyz";

/// The time (µs since the epoch) and clock id encoded in a TID record key,
/// if `rkey` is one.
pub(super) fn tid(rkey: &str) -> Option<(i64, u16)> {
    let b = rkey.as_bytes();
    if b.len() != 13 {
        return None;
    }
    let digit = |c: u8| TID_ALPHABET.iter().position(|&a| a == c).map(|d| d as u64);
    // 13 × 5 bits = 65: the first digit carries the always-zero top bit.
    if digit(b[0])? >= 16 {
        return None;
    }
    let mut v: u64 = 0;
    for &c in b {
        v = v << 5 | digit(c)?;
    }
    let us = (v >> 10) as i64;
    // 2023-01-01 .. 2100-01-01: anything else is a coincidence.
    (1_672_531_200_000_000..4_102_444_800_000_000).contains(&us).then_some((us, (v & 0x3ff) as u16))
}

/// `2026-11-13 07:51:47 · 3.2m ago`
pub(super) fn when(us: i64) -> String {
    let s = format_rfc3339_us(us);
    format!("{} {} · {} ago", &s[..10], &s[11..19], fmt_duration_us(now_us() - us).trim_start_matches('-'))
}

/// What a record refers to, labelled for display.
pub(super) fn record_refs(collection: &str, v: &Value) -> Vec<(&'static str, Ref)> {
    let as_ref = |u: &str| {
        if u.starts_with("did:") {
            Ref::Actor(u.to_owned())
        } else if u.contains("/app.bsky.feed.post/") {
            Ref::Post(u.to_owned())
        } else {
            Ref::Record(u.to_owned())
        }
    };
    let mut out = Vec::new();
    let parent = v["reply"]["parent"]["uri"].as_str();
    if let Some(p) = parent {
        out.push(("reply to", as_ref(p)));
    }
    if let Some(r) = v["reply"]["root"]["uri"].as_str().filter(|r| Some(*r) != parent) {
        out.push(("thread", as_ref(r)));
    }
    if let Some(q) = v["embed"]["record"]["uri"].as_str().or(v["embed"]["record"]["record"]["uri"].as_str()) {
        out.push(("quotes", as_ref(q)));
    }
    if let Some(s) = v["subject"]["uri"].as_str().or(v["subject"].as_str()) {
        let name = match collection {
            "app.bsky.feed.like" => "liked",
            "app.bsky.feed.repost" => "reposted",
            "app.bsky.graph.follow" => "follows",
            "app.bsky.graph.block" => "blocks",
            "app.bsky.graph.listitem" => "member",
            "app.bsky.graph.verification" => "verifies",
            _ => "subject",
        };
        out.push((name, as_ref(s)));
    }
    if let Some(l) = v["list"].as_str() {
        out.push(("list", as_ref(l)));
    }
    if let Some(p) = v["pinnedPost"]["uri"].as_str() {
        out.push(("pinned", as_ref(p)));
    }
    if let Some(p) = v["post"].as_str() {
        out.push(("post", as_ref(p)));
    }
    out
}

/// Top-level fields of a record, each summarised on one line.
pub(super) fn field_rows(v: &Value) -> Vec<(String, String)> {
    let Some(obj) = v.as_object() else { return Vec::new() };
    obj.iter().filter(|(k, _)| *k != "$type").map(|(k, v)| (k.clone(), summarize(v))).collect()
}

fn summarize(v: &Value) -> String {
    match v {
        Value::String(s) => {
            let one: String = s.replace('\n', " ⏎ ").chars().take(200).collect();
            if s.chars().count() > 200 {
                format!("{one}…")
            } else {
                one
            }
        }
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => match a.len() {
            0 => "[]".into(),
            1 => format!("[{}]", summarize(&a[0])),
            n => format!("{n} items: {}, …", summarize(&a[0])),
        },
        Value::Object(o) => {
            if o.get("$type").and_then(Value::as_str) == Some("blob") {
                let size = o.get("size").and_then(Value::as_u64).unwrap_or(0);
                let mime = o.get("mimeType").and_then(Value::as_str).unwrap_or("?");
                return format!("blob {mime} {}", fmt_bytes(size as f64));
            }
            if let Some(uri) = o.get("uri").and_then(Value::as_str) {
                return uri.to_owned();
            }
            if let Some(t) = o.get("$type").and_then(Value::as_str) {
                return format!("{t} {{…}}");
            }
            let keys: Vec<&str> = o.keys().map(String::as_str).take(5).collect();
            format!("{{{}{}}}", keys.join(", "), if o.len() > 5 { ", …" } else { "" })
        }
    }
}

/// The record a post view (or an embed's view record) quotes, if any.
fn embedded_view(p: &Value) -> Option<&Value> {
    let embed = if p["embed"].is_object() { &p["embed"] } else { p["embeds"].get(0)? };
    match embed["$type"].as_str()? {
        "app.bsky.embed.record#view" => Some(&embed["record"]),
        "app.bsky.embed.recordWithMedia#view" => Some(&embed["record"]["record"]),
        _ => None,
    }
}

impl Vortex {
    /// Post text with mentions (inspect), links (open) and tags (filter)
    /// highlighted and clickable.
    pub(super) fn rich_text(
        &self,
        id: impl Into<ElementId>,
        text: &str,
        facets: &Value,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let spans = facet_spans(text, facets);
        let color = |f: &Facet| match f {
            Facet::Mention(_) => t.blue,
            Facet::Link(_) => t.aqua,
            Facet::Tag(_) => t.purple,
        };
        let highlights: Vec<(Range<usize>, HighlightStyle)> = spans
            .iter()
            .map(|(r, f)| (r.clone(), HighlightStyle { color: Some(color(f)), ..Default::default() }))
            .collect();
        let styled = StyledText::new(SharedString::from(text.to_owned())).with_highlights(highlights);
        if spans.is_empty() {
            return styled.into_any_element();
        }
        let ranges = spans.iter().map(|(r, _)| r.clone()).collect();
        let actions: Vec<Facet> = spans.into_iter().map(|(_, f)| f).collect();
        let weak = cx.entity().downgrade();
        InteractiveText::new(id, styled)
            .on_click(ranges, move |ix, _, cx| {
                let Some(f) = actions.get(ix).cloned() else { return };
                match f {
                    Facet::Link(u) => cx.open_url(&u),
                    Facet::Mention(did) => {
                        let _ = weak.update(cx, |this, cx| this.inspect(AtTarget::repo(did), cx));
                    }
                    Facet::Tag(tag) => {
                        let _ = weak.update(cx, |this, cx| {
                            this.set_query(&format!("tag:{tag}"), cx);
                            this.close_inspector(cx);
                        });
                    }
                }
            })
            .into_any_element()
    }

    /// A post as a card: author, time, text, media, the post it quotes and
    /// its counts. `p` is a post view (`record`) or an embed's view record
    /// (`value`). `pending` means its labels are still loading.
    pub(super) fn post_card(&self, p: &Value, pending: bool, depth: usize, t: &Theme, cx: &mut Context<Self>) -> Div {
        let uri = p["uri"].as_str().unwrap_or("").to_owned();
        let author = &p["author"];
        let did = author["did"].as_str().unwrap_or("").to_owned();
        let record = if p["record"].is_object() { &p["record"] } else { &p["value"] };
        let hover = t.bg_hover;
        let handle = author["handle"].as_str().unwrap_or("?");
        let name = author["displayName"].as_str().filter(|s| !s.is_empty()).unwrap_or(handle);
        let time = record["createdAt"].as_str().and_then(parse_rfc3339_us).map(when).unwrap_or_default();
        let target = parse_target(&uri);
        let nested = depth > 0;

        let byline = div()
            .flex()
            .gap(t.ch)
            .min_w_0()
            .child(div().flex_none().max_w(relative(0.5)).truncate().text_color(t.fg).child(name.to_owned()))
            .child(div().flex_1().min_w_0().truncate().text_color(t.blue).child(format!("@{handle}")));
        let mut head = div()
            .id(SharedString::from(format!("card{depth}-{uri}")))
            .flex()
            .items_center()
            .gap(t.ch)
            .min_w_0()
            .rounded(t.radius)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .tooltip(tip("post", None, Some("click to inspect")))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if let Some(t) = target.clone() {
                        this.inspect(t, cx);
                    }
                }),
            )
            .child(self.avatar(author, if nested { t.row } else { t.row * 2. }, t));
        head = if nested {
            head.child(byline.flex_1())
        } else {
            head.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(byline)
                    .child(div().truncate().text_color(t.fg_faint).child(time.clone())),
            )
        };

        let mut card = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(t.pad)
            .min_w_0()
            .rounded(t.radius)
            .border_1()
            .border_color(t.border)
            .child(head);
        let text = record["text"].as_str().unwrap_or("");
        if !text.is_empty() {
            card = card.child(div().text_color(t.fg).when(nested, |d| d.max_h(t.row * 6.).overflow_hidden()).child(
                self.rich_text(SharedString::from(format!("text{depth}-{uri}")), text, &record["facets"], t, cx),
            ));
        }
        let rows = if nested { 4. } else { 6. };
        if let Some(block) = self.media_block(&uri, &did, "app.bsky.feed.post", record, Some(p), pending, rows, t, cx) {
            card = card.child(block);
        }

        // The quoted record: from the view when there is one, else by lookup.
        if depth < 2 {
            match embedded_view(p) {
                Some(q) if q["$type"] == "app.bsky.embed.record#viewRecord" => {
                    card = card.child(self.post_card(q, false, depth + 1, t, cx));
                }
                Some(q) => {
                    let note = match q["$type"].as_str().unwrap_or("") {
                        "app.bsky.embed.record#viewNotFound" => "quoted post not found".to_owned(),
                        "app.bsky.embed.record#viewBlocked" => "quoted post is blocked".to_owned(),
                        "app.bsky.embed.record#viewDetached" => "quote detached by its author".to_owned(),
                        ty => format!(
                            "{} · {}",
                            q["displayName"].as_str().or(q["name"].as_str()).unwrap_or(""),
                            ty.rsplit('.').next().unwrap_or(ty)
                        ),
                    };
                    card = card.child(div().text_color(t.fg_faint).truncate().child(note));
                }
                None => {
                    let quoted = record["embed"]["record"]["uri"]
                        .as_str()
                        .or(record["embed"]["record"]["record"]["uri"].as_str());
                    if let Some(q) = quoted {
                        card = card.child(self.ref_card(&Ref::Post(q.to_owned()), depth + 1, t, cx));
                    }
                }
            }
        }
        if p.get("likeCount").is_some() {
            card = card.child(super::lookup::post_counts(p, t).text_color(t.fg_faint));
        }
        card
    }

    /// An account as a card: avatar, names, counts, bio and labels.
    pub(super) fn profile_card(&self, p: &Value, t: &Theme, cx: &mut Context<Self>) -> Div {
        let did = p["did"].as_str().unwrap_or("").to_owned();
        let hover = t.bg_hover;
        let target = AtTarget::repo(did.clone());
        let mut card =
            div().flex().flex_col().gap(px(2.)).p(t.pad).min_w_0().rounded(t.radius).border_1().border_color(t.border);
        card = card.child(
            div()
                .id(SharedString::from(format!("pcard-{did}")))
                .flex()
                .items_center()
                .gap(t.ch)
                .min_w_0()
                .rounded(t.radius)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .tooltip(tip("account", None, Some("click to inspect this repo")))
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.inspect(target.clone(), cx)))
                .child(self.avatar(p, t.row * 2., t))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(super::lookup::profile_line(p, t))
                        .child(div().truncate().text_color(t.fg_faint).child(super::lookup::profile_stats(p))),
                ),
        );
        if let Some(bio) = p["description"].as_str().filter(|s| !s.is_empty()) {
            card = card.child(div().text_color(t.fg_dim).max_h(t.row * 4.).overflow_hidden().child(bio.to_owned()));
        }
        let labels = crate::media::view_labels(p, &did);
        if !labels.is_empty() {
            card = card.child(self.label_chips(&labels, &format!("pcard-{did}"), t));
        }
        card
    }

    /// Whatever a reference points at, as a card once it has loaded.
    pub(super) fn ref_card(&self, r: &Ref, depth: usize, t: &Theme, cx: &mut Context<Self>) -> Div {
        let (key, uri) = match r {
            Ref::Post(u) => (format!("post:{u}"), u.clone()),
            Ref::Actor(d) => (format!("profile:{d}"), d.clone()),
            Ref::Record(u) => (String::new(), u.clone()),
        };
        match (r, self.lookup(&key)) {
            (Ref::Post(_), Some(Fetch::Done(Ok(p)))) => self.post_card(p, false, depth, t, cx),
            (Ref::Actor(_), Some(Fetch::Done(Ok(p)))) => self.profile_card(p, t, cx),
            (_, Some(Fetch::Done(Err(e)))) => div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(div().text_color(t.fg_dim).truncate().child(uri))
                .child(div().text_color(t.orange).truncate().child(e.clone())),
            (Ref::Record(_), _) => {
                let hover = t.bg_hover;
                let target = parse_target(&uri);
                div().child(
                    div()
                        .id(SharedString::from(format!("ref-{uri}")))
                        .truncate()
                        .text_color(t.fg_dim)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                if let Some(t) = target.clone() {
                                    this.inspect(t, cx);
                                }
                            }),
                        )
                        .child(uri),
                )
            }
            _ => div().text_color(t.fg_faint).truncate().child(format!("… {uri}")),
        }
    }

    /// Fetch what the inspected record refers to, once it has loaded.
    pub(super) fn fetch_record_refs(&mut self, cx: &mut Context<Self>) {
        let Some(i) = &self.inspector else { return };
        let (Some(coll), Some(_)) = (i.target.collection.clone(), &i.target.rkey) else { return };
        let repo = i.target.repo.clone();
        let Some(v) = self.lookup_ok(&format!("record:{}", i.target.uri())).cloned() else { return };
        self.request_profile(&repo, cx);
        for (_, r) in record_refs(&coll, &v["value"]) {
            match r {
                Ref::Post(u) => self.request_post(&u, cx),
                Ref::Actor(d) => self.request_profile(&d, cx),
                Ref::Record(_) => {}
            }
        }
    }

    /// The inspector's reading of a record: when and by whom, what it
    /// says and shows, what it points at, and every field.
    pub(super) fn record_info(&self, uri: &str, coll: &str, rkey: &str, t: &Theme, cx: &mut Context<Self>) -> Div {
        let Some(rec) = self.lookup_ok(&format!("record:{uri}")) else { return div() };
        let value = &rec["value"];
        let kv = |k: &'static str, v: Div, t: &Theme| {
            div()
                .flex()
                .gap(t.ch)
                .min_w_0()
                .child(div().flex_none().w(t.ch * 9.).text_color(t.fg_faint).child(k))
                .child(v.flex_1().min_w_0())
        };
        let mut out = div().flex().flex_col().gap(t.pad).min_w_0();

        // Facts.
        let mut facts = div().flex().flex_col().min_w_0();
        facts = facts.child(kv("type", div().text_color(t.hue(coll)).truncate().child(coll.to_owned()), t));
        if let Some(us) = value["createdAt"].as_str().and_then(parse_rfc3339_us) {
            facts = facts.child(kv("created", div().text_color(t.fg_dim).truncate().child(when(us)), t));
        }
        let rkey_text = match tid(rkey) {
            Some((us, clock)) => {
                format!("{rkey} · tid {} · clock {clock}", format_rfc3339_us(us)[..19].replace('T', " "))
            }
            None => rkey.to_owned(),
        };
        facts = facts.child(kv("rkey", div().text_color(t.fg_dim).truncate().child(rkey_text), t));
        facts = facts.child(kv(
            "cid",
            div().text_color(t.fg_faint).truncate().child(rec["cid"].as_str().unwrap_or("–").to_owned()),
            t,
        ));
        let postview = self.lookup_ok(&format!("postview:{uri}"));
        if let Some(ix) = postview.and_then(|p| p["indexedAt"].as_str()).and_then(parse_rfc3339_us) {
            facts = facts.child(kv("indexed", div().text_color(t.fg_faint).truncate().child(when(ix)), t));
        }
        if let Some(langs) = value["langs"].as_array().filter(|l| !l.is_empty()) {
            let l: Vec<&str> = langs.iter().filter_map(Value::as_str).collect();
            facts = facts.child(kv("langs", div().text_color(t.fg_dim).child(l.join(" ")), t));
        }
        if let Some(tags) = value["tags"].as_array().filter(|l| !l.is_empty()) {
            let l: Vec<String> = tags.iter().filter_map(Value::as_str).map(|s| format!("#{s}")).collect();
            facts = facts.child(kv("tags", div().text_color(t.purple).child(l.join(" ")), t));
        }
        out = out.child(facts);

        // Author.
        let repo = uri.trim_start_matches("at://").split('/').next().unwrap_or("");
        let profile = self.lookup_ok(&format!("profile:{repo}"));
        let mut by =
            div().flex().gap(t.ch).min_w_0().child(div().flex_none().w(t.ch * 9.).text_color(t.fg_faint).child("by"));
        by = match profile {
            Some(p) => by.child(self.profile_card(p, t, cx).flex_1()),
            None => by.child(div().text_color(t.fg_faint).child("…")),
        };
        out = out.child(by);

        // What it points at, then the record itself.
        let is_post = coll == "app.bsky.feed.post";
        for (name, r) in record_refs(coll, value) {
            if is_post && name == "quotes" {
                continue; // shown inside the post card
            }
            out = out.child(
                div()
                    .flex()
                    .gap(t.ch)
                    .min_w_0()
                    .child(div().flex_none().w(t.ch * 9.).text_color(t.fg_faint).child(name))
                    .child(self.ref_card(&r, 0, t, cx).flex_1()),
            );
        }
        if is_post {
            let did = profile.and_then(|p| p["did"].as_str()).unwrap_or(repo).to_owned();
            let (_, pending) = self.post_view_for_inspector(coll, uri);
            let synth;
            let p: &Value = match postview {
                Some(v) => v,
                None => {
                    let author = profile.map(|p| (**p).clone()).unwrap_or_else(|| serde_json::json!({ "did": did }));
                    synth = serde_json::json!({ "uri": uri, "author": author, "record": value });
                    &synth
                }
            };
            out = out.child(
                div()
                    .flex()
                    .gap(t.ch)
                    .min_w_0()
                    .child(div().flex_none().w(t.ch * 9.).text_color(t.fg_faint).child("post"))
                    .child(self.post_card(p, pending, 0, t, cx).flex_1()),
            );
        } else if let Some(did) = self.inspector_did() {
            if let Some(block) = self.media_block(uri, &did, coll, value, None, false, 6., t, cx) {
                out = out.child(block);
            }
            if let Some(d) = value["description"].as_str().filter(|s| !s.is_empty()) {
                out = out.child(div().text_color(t.fg_dim).child(self.rich_text(
                    "rec-desc",
                    d,
                    &value["descriptionFacets"],
                    t,
                    cx,
                )));
            }
        }

        // Every field, for any lexicon.
        let mut fields = div().flex().flex_col().min_w_0().pt(t.pad).border_t_1().border_color(t.border);
        fields = fields.child(div().text_color(t.fg_faint).child("fields"));
        for (k, v) in field_rows(value) {
            fields = fields.child(
                div()
                    .flex()
                    .gap(t.ch)
                    .min_w_0()
                    .child(div().flex_none().w(t.ch * 14.).truncate().text_color(t.blue).child(k))
                    .child(div().flex_1().min_w_0().text_color(t.fg_dim).max_h(t.row * 3.).overflow_hidden().child(v)),
            );
        }
        out.child(fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn facets_are_byte_ranges_on_char_boundaries() {
        let text = "héllo @bob.test #rust https://x.y";
        let f = json!([
            {"index": {"byteStart": 7, "byteEnd": 16}, "features": [{"$type": "app.bsky.richtext.facet#mention", "did": "did:plc:bob"}]},
            {"index": {"byteStart": 17, "byteEnd": 22}, "features": [{"$type": "app.bsky.richtext.facet#tag", "tag": "rust"}]},
            {"index": {"byteStart": 23, "byteEnd": 34}, "features": [{"$type": "app.bsky.richtext.facet#link", "uri": "https://x.y"}]},
            {"index": {"byteStart": 2, "byteEnd": 3}, "features": [{"$type": "app.bsky.richtext.facet#tag", "tag": "bad"}]},
            {"index": {"byteStart": 8, "byteEnd": 10}, "features": [{"$type": "app.bsky.richtext.facet#tag", "tag": "overlap"}]}
        ]);
        let spans = facet_spans(text, &f);
        assert_eq!(spans.len(), 3, "{spans:?}");
        assert_eq!(&text[spans[0].0.clone()], "@bob.test");
        assert_eq!(spans[0].1, Facet::Mention("did:plc:bob".into()));
        assert_eq!(&text[spans[1].0.clone()], "#rust");
        assert_eq!(&text[spans[2].0.clone()], "https://x.y");
    }

    #[test]
    fn tids_decode() {
        // 3jzfcijpj2z2a is the example TID in the atproto spec.
        let (us, _) = tid("3jzfcijpj2z2a").unwrap();
        assert!(format_rfc3339_us(us).starts_with("2023-"), "{}", format_rfc3339_us(us));
        assert_eq!(tid("self"), None);
        assert_eq!(tid("zzzzzzzzzzzzz"), None);
    }

    #[test]
    fn refs_and_fields() {
        let v = json!({
            "text": "hi",
            "reply": {"parent": {"uri": "at://did:plc:a/app.bsky.feed.post/p"}, "root": {"uri": "at://did:plc:a/app.bsky.feed.post/r"}},
            "embed": {"$type": "app.bsky.embed.record", "record": {"uri": "at://did:plc:b/app.bsky.feed.post/q"}},
            "avatar": {"$type": "blob", "mimeType": "image/png", "size": 2048}
        });
        let names: Vec<&str> = record_refs("app.bsky.feed.post", &v).iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["reply to", "thread", "quotes"]);
        let f = field_rows(&v);
        assert!(f.iter().any(|(k, s)| k == "avatar" && s.starts_with("blob image/png")));
        let like = json!({"subject": {"uri": "at://did:plc:a/app.bsky.feed.post/p"}});
        assert!(matches!(record_refs("app.bsky.feed.like", &like)[0], ("liked", Ref::Post(_))));
    }
}
