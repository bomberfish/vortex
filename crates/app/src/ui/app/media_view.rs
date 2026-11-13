//! Images, video and content labels: a bounded image cache fed by background
//! fetches, label chips, the media block shown for an event or record,
//! avatars, and a full-window viewer (with video playback when built with the
//! `video` feature).
//!
//! Content labels gate media: anything under an adult, graphic, hide or warn
//! label is replaced by a placeholder naming the label until clicked (or
//! `media.reveal_labeled` is set). For posts, media waits until the AppView's
//! view of the post has loaded, since labeler labels arrive there, so nothing
//! flashes before its labels are known.

use super::lookup::Fetch;
use super::*;
use crate::atp::Json;
use crate::media::{self, Kind, Label, Media, Severity, Src};
use gpui::{img, AnyElement, Image, ImageFormat, ObjectFit, StyledImage};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

/// Largest download accepted for one image.
const IMAGE_LIMIT: u64 = 25 << 20;
/// Decoded-image cache budget (encoded bytes, which track decoded size well
/// enough for eviction).
const CACHE_BUDGET: usize = 192 << 20;

pub(super) enum ImgFetch {
    Pending,
    /// The image and its pixel size, when the header could be read.
    Ready(Arc<Image>, Option<(u32, u32)>),
    Failed(String),
}

/// The image cache: entries plus insertion order for eviction.
#[derive(Default)]
pub(super) struct Images {
    map: HashMap<String, (ImgFetch, usize)>,
    order: VecDeque<String>,
    bytes: usize,
}

impl Images {
    fn get(&self, key: &str) -> Option<&ImgFetch> {
        self.map.get(key).map(|(f, _)| f)
    }

    fn insert(&mut self, key: String, f: ImgFetch, size: usize) {
        if let Some((_, old)) = self.map.insert(key.clone(), (f, size)) {
            self.bytes -= old;
        } else {
            self.order.push_back(key);
        }
        self.bytes += size;
        while self.bytes > CACHE_BUDGET {
            let Some(k) = self.order.pop_front() else { break };
            if let Some((_, s)) = self.map.remove(&k) {
                self.bytes -= s;
            }
        }
    }
}

/// What the full-window viewer is showing.
pub(super) struct Viewer {
    items: Vec<Media>,
    ix: usize,
    labels: Vec<Label>,
    #[cfg(feature = "video")]
    video: Option<(usize, VideoState)>,
}

#[cfg(feature = "video")]
enum VideoState {
    Loading,
    Ready(gpui_video_player::Video),
    Failed(String),
}

/// Image format from the content type, else from magic bytes.
fn sniff(bytes: &[u8], content_type: Option<&str>) -> Option<ImageFormat> {
    if let Some(f) =
        content_type.map(|c| c.split(';').next().unwrap_or("").trim()).and_then(ImageFormat::from_mime_type)
    {
        return Some(f);
    }
    match bytes {
        [0xFF, 0xD8, 0xFF, ..] => Some(ImageFormat::Jpeg),
        [0x89, b'P', b'N', b'G', ..] => Some(ImageFormat::Png),
        [b'G', b'I', b'F', b'8', ..] => Some(ImageFormat::Gif),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(ImageFormat::Webp),
        [b'B', b'M', ..] => Some(ImageFormat::Bmp),
        _ => None,
    }
}

impl Vortex {
    /// Fetch and cache an image once.
    pub(super) fn request_image(&mut self, src: &Src, cx: &mut Context<Self>) {
        let key = src.key();
        if self.images.get(&key).is_some() {
            return;
        }
        self.images.insert(key.clone(), ImgFetch::Pending, 0);
        let client = self.client.clone();
        let src = src.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let url = match &src {
                        Src::Url(u) => u.clone(),
                        Src::Blob { did, cid } => client.blob_url(did, cid)?,
                    };
                    let (bytes, ct) = client.get_bytes(&url, IMAGE_LIMIT)?;
                    let format = sniff(&bytes, ct.as_deref()).ok_or("not an image")?;
                    let dims = image::ImageReader::new(std::io::Cursor::new(&bytes))
                        .with_guessed_format()
                        .ok()
                        .and_then(|r| r.into_dimensions().ok());
                    Ok::<_, String>((Arc::new(Image::from_bytes(format, bytes)), dims))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((image, dims)) => {
                        let size = image.bytes.len();
                        this.images.insert(key, ImgFetch::Ready(image, dims), size);
                    }
                    Err(e) => this.images.insert(key, ImgFetch::Failed(e), 0),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// An image element for `src`, or a placeholder while it loads.
    pub(super) fn image_el(&self, src: &Src, fit: ObjectFit, t: &Theme) -> AnyElement {
        match self.images.get(&src.key()) {
            Some(ImgFetch::Ready(image, _)) => img(image.clone()).size_full().object_fit(fit).into_any_element(),
            Some(ImgFetch::Failed(e)) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(t.fg_faint)
                .child(e.clone())
                .into_any_element(),
            _ => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(t.fg_faint)
                .child("…")
                .into_any_element(),
        }
    }

    /// A URL that `open` can hand to the browser, if it is known yet.
    fn src_url(&self, src: &Src) -> Option<String> {
        match src {
            Src::Url(u) => Some(u.clone()),
            Src::Blob { did, cid } => self
                .lookup_ok(&format!("ident:{did}"))
                .and_then(|v| v["pds"].as_str().map(str::to_owned))
                .map(|pds| format!("{pds}/xrpc/com.atproto.sync.getBlob?did={did}&cid={cid}")),
        }
    }

    pub(super) fn media_reveal(&self, owner: &str) -> bool {
        self.cfg.config.media.reveal_labeled || self.revealed.contains(owner)
    }

    /// Request what the visible media needs: the selected event's previews,
    /// avatars of looked-up accounts, and the inspector's images.
    pub(super) fn prepare_media(&mut self, cx: &mut Context<Self>) {
        if !self.cfg.config.media.load {
            return;
        }
        let mut wanted: Vec<Src> = Vec::new();
        let mut owners_to_resolve: Vec<String> = Vec::new();
        // Selected event: its media (when labels allow) and avatars.
        if let Some(d) = &self.detail {
            let e = &d.event;
            let did = e.did.to_string();
            let collection = e.collection_str().to_owned();
            if let Some(record) = e.record_json().and_then(|r| serde_json::from_str::<Value>(r).ok()) {
                let uri = e.at_uri().unwrap_or_default();
                let (view, pending) = self.post_view_for(&collection, &uri);
                let labels = media::merge([
                    media::self_labels(&record),
                    view.map(|v| media::view_labels(v, &did)).unwrap_or_default(),
                ]);
                let gated = media::media_gate(&labels).is_some() && !self.media_reveal(&uri);
                if !gated && !pending {
                    for m in media::extract(&did, &collection, &record) {
                        if let Some(p) = m.preview {
                            if matches!(p, Src::Blob { .. }) {
                                owners_to_resolve.push(did.clone());
                            }
                            wanted.push(p);
                        }
                    }
                }
            }
            wanted.extend(self.context_avatars(e));
        }
        if let Some(v) = &self.viewer {
            if let Some(m) = v.items.get(v.ix) {
                wanted.extend(m.full.clone().or(m.preview.clone()));
            }
        }
        wanted.extend(self.inspector_media_sources());
        for did in owners_to_resolve {
            self.request_identity(&did, cx);
        }
        for src in wanted {
            self.request_image(&src, cx);
        }
    }

    /// The AppView view of a post (for its labels), and whether it is still
    /// loading. Non-posts have no view and never wait.
    pub(super) fn post_view_for(&self, collection: &str, uri: &str) -> (Option<&Json>, bool) {
        if collection != "app.bsky.feed.post" || uri.is_empty() {
            return (None, false);
        }
        match self.lookup(&format!("post:{uri}")) {
            Some(Fetch::Done(Ok(v))) => (Some(v), false),
            Some(Fetch::Done(Err(_))) => (None, false),
            _ => (None, true),
        }
    }

    /// Avatar sources for the selected event's author and referenced accounts.
    fn context_avatars(&self, e: &Event) -> Vec<Src> {
        let mut out = Vec::new();
        let mut push = |p: &Value| {
            if let Some(src) = avatar_src(p) {
                out.push(src);
            }
        };
        if let Some(p) = self.lookup_ok(&format!("profile:{}", e.did)) {
            push(p);
        }
        for (_, r) in super::lookup::event_refs(e) {
            match r {
                super::lookup::Ref::Post(u) => {
                    if let Some(p) = self.lookup_ok(&format!("post:{u}")) {
                        push(&p["author"]);
                    }
                }
                super::lookup::Ref::Actor(d) => {
                    if let Some(p) = self.lookup_ok(&format!("profile:{d}")) {
                        push(p);
                    }
                }
                super::lookup::Ref::Record(_) => {}
            }
        }
        out
    }

    /// A square avatar for a profile view (`p` has `avatar`, `did`, `labels`).
    /// Hidden behind a marker when the profile carries a media-hiding label.
    pub(super) fn avatar(&self, p: &Value, size: Pixels, t: &Theme) -> Div {
        let frame = div().flex_none().size(size).rounded(t.radius).overflow_hidden().bg(t.bg_hover);
        if !self.cfg.config.media.load {
            return frame;
        }
        let did = p["did"].as_str().unwrap_or("");
        let labels = media::view_labels(p, did);
        if media::media_gate(&labels).is_some() && !self.media_reveal(did) {
            return frame.flex().items_center().justify_center().child(icon(Icon::Warn, t.orange, size * 0.6));
        }
        match avatar_src(p) {
            Some(src) => frame.child(self.image_el(&src, ObjectFit::Cover, t)),
            None => frame,
        }
    }

    /// Label chips: strongest first, colored by severity, with the source in
    /// the tooltip.
    pub(super) fn label_chips(&self, labels: &[Label], id: &str, t: &Theme) -> Div {
        let mut row = div().flex().flex_wrap().items_center().gap(t.pad);
        for (i, l) in labels.iter().enumerate() {
            let color = match l.severity() {
                Severity::Adult | Severity::Graphic | Severity::Hide => t.red,
                Severity::Warn => t.orange,
                Severity::Info => t.fg_faint,
            };
            let from = match &l.src {
                None => "self-label: set by the author".to_owned(),
                Some(src) => match self.lookup_ok(&format!("ident:{src}")).and_then(|v| v["handle"].as_str()) {
                    Some(h) => format!("from labeler @{h}"),
                    None => format!("from labeler {src}"),
                },
            };
            let what = match l.severity() {
                Severity::Adult => "adult content; media hidden until revealed",
                Severity::Graphic => "graphic content; media hidden until revealed",
                Severity::Hide => "a labeler asks clients to hide this; media hidden until revealed",
                Severity::Warn => "warning; media hidden until revealed",
                Severity::Info => "informational",
            };
            let body = format!("{from}. {what}");
            row = row.child(
                div()
                    .id(SharedString::from(format!("{id}-label-{i}")))
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .px(t.pad)
                    .rounded(t.radius)
                    .border_1()
                    .border_color(color)
                    .text_color(color)
                    .child(icon(Icon::Warn, color, t.font_size * 0.85))
                    .child(l.val.clone())
                    .tooltip(tip(format!("label: {}", l.val), None, Some(&body))),
            );
        }
        row
    }

    /// Labels and media for a record: chips, then image/video tiles, link
    /// cards and other blobs. `None` when there is nothing to show.
    /// `view` is the AppView's view of the record (for labeler labels) and
    /// `pending` whether it is still loading.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn media_block(
        &self,
        owner: &str,
        did: &str,
        collection: &str,
        record: &Value,
        view: Option<&Json>,
        pending: bool,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let labels =
            media::merge([media::self_labels(record), view.map(|v| media::view_labels(v, did)).unwrap_or_default()]);
        let items = media::extract(did, collection, record);
        if items.is_empty() && labels.is_empty() {
            return None;
        }
        let gate = media::media_gate(&labels).filter(|_| !self.media_reveal(owner)).cloned();
        let mut out = div().flex().flex_col().gap(t.pad).pt(t.pad);
        if !labels.is_empty() {
            out = out.child(self.label_chips(&labels, owner, t));
        }
        let visual: Vec<usize> =
            (0..items.len()).filter(|i| matches!(items[*i].kind, Kind::Image | Kind::Gif | Kind::Video)).collect();
        if !visual.is_empty() {
            let h = t.row * 7.;
            let single_w =
                (visual.len() == 1).then(|| items[visual[0]].aspect.map(|a| h * a.clamp(0.4, 3.0))).flatten();
            let mut tiles = div().flex().gap(px(2.)).h(h);
            for &i in &visual {
                let m = &items[i];
                let mut tile = div()
                    .id(SharedString::from(format!("{owner}-media-{i}")))
                    .relative()
                    .h(h)
                    .min_w_0()
                    .rounded(t.radius)
                    .overflow_hidden()
                    .bg(t.bg_hover)
                    .cursor_pointer();
                tile = match single_w {
                    Some(w) => tile.flex_none().w(w).max_w_full(),
                    None => tile.flex_1(),
                };
                if let Some(g) = &gate {
                    // Hidden: say why, reveal on click.
                    let owner = owner.to_owned();
                    tile = tile
                        .border_1()
                        .border_color(t.red)
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(2.))
                        .child(icon(Icon::Warn, t.red, t.font_size))
                        .child(div().text_color(t.red).child(g.val.clone()))
                        .child(div().text_color(t.fg_faint).child("click to show"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.revealed.insert(owner.clone());
                                cx.notify();
                            }),
                        );
                } else if pending {
                    tile = tile.flex().items_center().justify_center().text_color(t.fg_faint).child("checking labels…");
                } else if !self.cfg.config.media.load {
                    tile = tile.flex().items_center().justify_center().text_color(t.fg_faint).child(m.role);
                } else {
                    if let Some(p) = &m.preview {
                        tile = tile.child(self.image_el(p, ObjectFit::Cover, t));
                    }
                    if m.kind != Kind::Image {
                        let badge = if m.kind == Kind::Video { "video" } else { "gif" };
                        tile = tile.child(
                            div()
                                .absolute()
                                .left(t.pad)
                                .bottom(t.pad)
                                .flex()
                                .items_center()
                                .gap(px(3.))
                                .px(t.pad)
                                .rounded(t.radius)
                                .bg(t.bg)
                                .text_color(t.fg)
                                .child(icon(Icon::Play, t.fg, t.font_size * 0.85))
                                .child(badge),
                        );
                    }
                    if !m.alt.is_empty() {
                        tile = tile.tooltip(tip(format!("{} · alt text", m.role), None, Some(&m.alt)));
                    }
                    let (items2, labels2) = (items.clone(), labels.clone());
                    tile = tile.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.open_viewer(items2.clone(), i, labels2.clone(), window, cx)
                        }),
                    );
                }
                tiles = tiles.child(tile);
            }
            out = out.child(tiles);
        }
        for (i, m) in items.iter().enumerate() {
            match m.kind {
                Kind::Link => out = out.child(self.link_card(owner, i, m, gate.is_some() || pending, t, cx)),
                Kind::Audio | Kind::File => out = out.child(self.blob_row(owner, i, m, t, cx)),
                _ => {}
            }
        }
        Some(out)
    }

    fn link_card(&self, owner: &str, i: usize, m: &Media, hide_thumb: bool, t: &Theme, cx: &mut Context<Self>) -> Div {
        let url = m.open.clone().and_then(|s| self.src_url(&s)).unwrap_or_default();
        let host = vortex_core::event::url_host(&url).to_owned();
        let thumb = (!hide_thumb && self.cfg.config.media.load).then(|| m.preview.clone()).flatten();
        let hover = t.bg_hover;
        let open = url.clone();
        div().child(
            div()
                .id(SharedString::from(format!("{owner}-link-{i}")))
                .flex()
                .gap(t.ch)
                .p(t.pad)
                .rounded(t.radius)
                .border_1()
                .border_color(t.border)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .tooltip(tip("link", None, Some(&url)))
                .on_mouse_down(MouseButton::Left, cx.listener(move |_, _, _, cx| cx.open_url(&open)))
                .children(thumb.map(|src| {
                    div()
                        .flex_none()
                        .size(t.row * 3.5)
                        .rounded(t.radius)
                        .overflow_hidden()
                        .bg(t.bg_hover)
                        .child(self.image_el(&src, ObjectFit::Cover, t))
                }))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .flex_1()
                        .child(div().text_color(t.fg).truncate().child(m.alt.clone()))
                        .child(div().text_color(t.fg_dim).max_h(t.row * 2.).overflow_hidden().child(m.detail.clone()))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(3.))
                                .text_color(t.fg_faint)
                                .child(icon(Icon::Link, t.fg_faint, t.font_size * 0.85))
                                .child(host),
                        ),
                ),
        )
    }

    fn blob_row(&self, owner: &str, i: usize, m: &Media, t: &Theme, cx: &mut Context<Self>) -> Div {
        let url = m.open.as_ref().and_then(|s| self.src_url(s));
        let hover = t.bg_hover;
        div().child(
            div()
                .id(SharedString::from(format!("{owner}-blob-{i}")))
                .flex()
                .items_center()
                .gap(t.ch)
                .px(t.pad)
                .rounded(t.radius)
                .border_1()
                .border_color(t.border)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(icon(if m.kind == Kind::Audio { Icon::Play } else { Icon::File }, t.fg_dim, t.font_size))
                .child(div().text_color(t.fg).child(m.role))
                .child(div().text_color(t.fg_faint).child(m.detail.clone()))
                .child(div().text_color(t.blue).child(if url.is_some() { "open" } else { "…" }))
                .tooltip(tip(m.role, None, Some("open the blob from the account's PDS")))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_, _, _, cx| {
                        if let Some(u) = &url {
                            cx.open_url(u);
                        }
                    }),
                ),
        )
    }

    /// Image sources the inspector page shows: the repo's avatar and banner,
    /// or a record's media.
    fn inspector_media_sources(&self) -> Vec<Src> {
        let Some(i) = &self.inspector else { return Vec::new() };
        let t = &i.target;
        let mut out = Vec::new();
        match (&t.collection, &t.rkey) {
            (None, _) => {
                if let Some(p) = self.lookup_ok(&format!("profile:{}", t.repo)) {
                    out.extend(avatar_src(p));
                    if let Some(b) = p["banner"].as_str() {
                        out.push(Src::Url(b.to_owned()));
                    }
                }
            }
            (Some(coll), Some(_)) => {
                let uri = t.uri();
                if let (Some(rec), Some(did)) = (self.lookup_ok(&format!("record:{uri}")), self.inspector_did()) {
                    let (view, pending) = self.post_view_for_inspector(coll, &uri);
                    let labels = media::merge([
                        media::self_labels(&rec["value"]),
                        view.map(|v| media::view_labels(v, &did)).unwrap_or_default(),
                    ]);
                    if !pending && (media::media_gate(&labels).is_none() || self.media_reveal(&uri)) {
                        out.extend(media::extract(&did, coll, &rec["value"]).into_iter().filter_map(|m| m.preview));
                    }
                }
            }
            _ => {}
        }
        out
    }

    pub(super) fn inspector_did(&self) -> Option<String> {
        let repo = &self.inspector.as_ref()?.target.repo;
        if repo.starts_with("did:") {
            return Some(repo.clone());
        }
        self.lookup_ok(&format!("ident:{repo}")).and_then(|v| v["did"].as_str().map(str::to_owned))
    }

    /// Like [`post_view_for`], for the inspector, whose post views are keyed
    /// by the URI as browsed (handle or DID).
    pub(super) fn post_view_for_inspector(&self, collection: &str, uri: &str) -> (Option<&Json>, bool) {
        if collection != "app.bsky.feed.post" {
            return (None, false);
        }
        match self.lookup(&format!("postview:{uri}")) {
            Some(Fetch::Done(Ok(v))) => (Some(v), false),
            Some(Fetch::Done(Err(_))) => (None, false),
            _ => (None, true),
        }
    }

    // ---- viewer ---------------------------------------------------------

    pub(super) fn open_viewer(
        &mut self,
        items: Vec<Media>,
        ix: usize,
        labels: Vec<Label>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only images, GIFs and videos are viewable; keep their order.
        let visual: Vec<Media> =
            items.iter().filter(|m| matches!(m.kind, Kind::Image | Kind::Gif | Kind::Video)).cloned().collect();
        let target = &items[ix];
        let ix = visual.iter().position(|m| m == target).unwrap_or(0);
        self.viewer = Some(Viewer {
            items: visual,
            ix,
            labels,
            #[cfg(feature = "video")]
            video: None,
        });
        window.focus(&self.viewer_focus, cx);
        self.viewer_changed(cx);
    }

    /// Debug builds: the `view:<query>` hook, once the selection's labels are in.
    pub(super) fn debug_open_viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = &self.detail else { return };
        let e = &d.event;
        let Some(record) = e.record_json().and_then(|r| serde_json::from_str::<Value>(r).ok()) else { return };
        let did = e.did.to_string();
        let owner = e.at_uri().unwrap_or_else(|| format!("event:{}", e.id));
        let (view, pending) = self.post_view_for(e.collection_str(), &owner);
        if pending {
            return;
        }
        let labels =
            media::merge([media::self_labels(&record), view.map(|v| media::view_labels(v, &did)).unwrap_or_default()]);
        let items = media::extract(&did, e.collection_str(), &record);
        self.debug_view = false;
        if let Some(ix) = items.iter().position(|m| matches!(m.kind, Kind::Image | Kind::Gif | Kind::Video)) {
            self.revealed.insert(owner);
            self.open_viewer(items, ix, labels, window, cx);
        }
    }

    pub(super) fn close_viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.viewer = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(super) fn viewer_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(v) = &mut self.viewer else { return };
        let n = v.items.len() as isize;
        if n > 1 {
            v.ix = (v.ix as isize + delta).rem_euclid(n) as usize;
            self.viewer_changed(cx);
        }
    }

    /// Load what the current item needs (full image, or the video).
    fn viewer_changed(&mut self, cx: &mut Context<Self>) {
        let Some(v) = &self.viewer else { return };
        let Some(m) = v.items.get(v.ix).cloned() else { return };
        let ix = v.ix;
        if let Some(src) = m.full.clone().or(m.preview.clone()) {
            self.request_image(&src, cx);
        }
        #[cfg(feature = "video")]
        if m.kind == Kind::Video {
            self.load_video(ix, &m, cx);
        } else if let Some(v) = &mut self.viewer {
            v.video = None;
        }
        #[cfg(not(feature = "video"))]
        let _ = ix;
        cx.notify();
    }

    #[cfg(feature = "video")]
    fn load_video(&mut self, ix: usize, m: &Media, cx: &mut Context<Self>) {
        let Some(v) = &mut self.viewer else { return };
        if matches!(&v.video, Some((i, _)) if *i == ix) {
            return;
        }
        v.video = Some((ix, VideoState::Loading));
        let Some(src) = m.open.clone() else { return };
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            // Opening a stream blocks until it starts playing; keep it off the
            // UI thread.
            let result = cx
                .background_executor()
                .spawn(async move {
                    let url = match &src {
                        Src::Url(u) => u.clone(),
                        Src::Blob { did, cid } => client.blob_url(did, cid)?,
                    };
                    let url = gpui_video_player::Url::parse(&url).map_err(|e| e.to_string())?;
                    gpui_video_player::Video::new(&url).map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(v) = &mut this.viewer {
                    if matches!(&v.video, Some((i, VideoState::Loading)) if *i == ix) {
                        v.video = Some((
                            ix,
                            match result {
                                Ok(video) => VideoState::Ready(video),
                                Err(e) => VideoState::Failed(e),
                            },
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn viewer_toggle_pause(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "video")]
        if let Some(Viewer { video: Some((_, VideoState::Ready(v))), .. }) = &self.viewer {
            v.set_paused(!v.paused());
            if v.eos() {
                let _ = v.seek(std::time::Duration::ZERO, false);
                v.set_paused(false);
            }
        }
        cx.notify();
    }

    pub(super) fn viewer_toggle_mute(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "video")]
        if let Some(Viewer { video: Some((_, VideoState::Ready(v))), .. }) = &self.viewer {
            v.set_muted(!v.muted());
        }
        cx.notify();
    }

    pub(super) fn viewer_open_external(&mut self, cx: &mut Context<Self>) {
        let Some(v) = &self.viewer else { return };
        let Some(m) = v.items.get(v.ix) else { return };
        if let Some(url) = m.open.as_ref().and_then(|s| self.src_url(s)) {
            cx.open_url(&url);
        }
    }

    pub(super) fn render_viewer(&self, window: &Window, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let v = self.viewer.as_ref()?;
        let m = v.items.get(v.ix)?;
        let vp = window.viewport_size();
        // The caption below takes up to ~8 rows (counter, 3 of alt text,
        // labels, hints); the image fills whatever the column leaves.
        let (max_w, max_h) = (vp.width - px(80.), vp.height - t.row * 12.);
        let stage: AnyElement = match m.kind {
            #[cfg(feature = "video")]
            Kind::Video => match &v.video {
                Some((_, VideoState::Ready(video))) => {
                    let (w, h) = video.size();
                    let (w, h) = (w.max(1) as f32, h.max(1) as f32);
                    let scale = (max_w / px(w)).min(max_h / px(h));
                    gpui_video_player::video(video.clone())
                        .id("viewer-video")
                        .size(px(w * scale), px(h * scale))
                        .into_any_element()
                }
                Some((_, VideoState::Failed(e))) => div()
                    .text_color(t.orange)
                    .child(format!("video: {e} · o opens it in the browser"))
                    .into_any_element(),
                _ => div().text_color(t.fg_faint).child("loading video…").into_any_element(),
            },
            _ => match m.full.as_ref().or(m.preview.as_ref()) {
                Some(src) => {
                    // Explicit size: fit the image's aspect into the stage.
                    let dims = match self.images.get(&src.key()) {
                        Some(ImgFetch::Ready(_, Some((w, h)))) => Some(*w as f32 / (*h).max(1) as f32),
                        _ => None,
                    };
                    let (w, h) = match dims.or(m.aspect) {
                        Some(a) if a > max_w / max_h => (max_w, max_w / a),
                        Some(a) => (max_h * a, max_h),
                        None => (max_w, max_h),
                    };
                    div().w(w).h(h).child(self.image_el(src, ObjectFit::Contain, t)).into_any_element()
                }
                None => div().text_color(t.fg_faint).child("no preview · o opens it").into_any_element(),
            },
        };
        let mut playing = String::new();
        #[cfg(feature = "video")]
        if let Some((_, VideoState::Ready(video))) = &v.video {
            let (p, d) = (video.position().as_secs(), video.duration().as_secs());
            playing = format!(
                " · {}:{:02} / {}:{:02}{}{}",
                p / 60,
                p % 60,
                d / 60,
                d % 60,
                if video.paused() { " · paused" } else { "" },
                if video.muted() { " · muted" } else { "" }
            );
        }
        let hint = match m.kind {
            Kind::Video if cfg!(feature = "video") => "space pause · m mute · ← → · o open · esc close",
            _ => "← → · o open · esc close",
        };
        let mut backdrop = t.bg;
        backdrop.a = 0.94;
        let caption = div()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(2.))
            .max_w(t.ch * 100.)
            .child(div().text_color(t.fg_dim).child(format!("{}/{} · {}{playing}", v.ix + 1, v.items.len(), m.role)))
            .when(!m.alt.is_empty(), |d| {
                d.child(div().text_color(t.fg).max_h(t.row * 3.).overflow_hidden().child(m.alt.clone()))
            })
            .when(!v.labels.is_empty(), |d| d.child(self.label_chips(&v.labels, "viewer", t)))
            .child(div().text_color(t.fg_faint).child(hint));
        Some(
            deferred(
                div()
                    .id("viewer")
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(t.pad * 2.)
                    .py(t.row * 2.)
                    .bg(backdrop)
                    .font_family(t.font.clone())
                    .text_size(t.font_size)
                    .line_height(t.row)
                    .key_context(VIEWER_CONTEXT)
                    .track_focus(&self.viewer_focus)
                    .occlude()
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_viewer(window, cx)))
                    .child(
                        div()
                            .id("viewer-stage")
                            .flex_none()
                            .h(max_h)
                            .w(max_w)
                            .flex()
                            .items_center()
                            .justify_center()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(stage),
                    )
                    .child(caption),
            )
            .with_priority(3),
        )
    }
}

/// A profile view's avatar at thumbnail size.
fn avatar_src(p: &Value) -> Option<Src> {
    p["avatar"].as_str().filter(|s| !s.is_empty()).map(|u| Src::Url(media::avatar_thumb(u)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_formats() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0], None), Some(ImageFormat::Jpeg));
        assert_eq!(sniff(b"\x89PNG\r\n", None), Some(ImageFormat::Png));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 ", None), Some(ImageFormat::Webp));
        assert_eq!(sniff(b"whatever", Some("image/webp")), Some(ImageFormat::Webp));
        assert_eq!(sniff(b"<html>", Some("text/html")), None);
    }
}
