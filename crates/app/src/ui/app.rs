//! The main window.
//!
//! The view never waits on the data path: a 30 Hz tick compares cheap version
//! counters and only then re-renders; rendering takes the store lock with a
//! short timeout and falls back to the previous frame's rows if a writer
//! (ingest batch, re-filter) is holding it.

use super::input::{InputEvent, TextInput};
use super::rows::{
    clock, fmt_bytes, fmt_count, fmt_duration_us, fmt_rate, json_lines, row_data, short_collection, web_url, JsonLine,
    RowData,
};
use crate::config::{ConfigFile, SavedFilter, BUNDLED_FONT};
use crate::theme::{resolve_palette, theme_names, Theme};
use gpui::{
    actions, canvas, div, fill, point, prelude::*, px, size, uniform_list, App, Bounds, ClipboardItem, Context, Div,
    DragMoveEvent, Entity, FocusHandle, Focusable, Hsla, KeyBinding, MouseButton, MouseDownEvent, Pixels,
    ScrollEvent, ScrollStrategy, SharedString, StyledText, UniformListScrollHandle, Window, WindowControlArea,
};
use std::sync::Arc;
use std::time::Duration;
use vortex_core::decode::NoticeLevel;
use vortex_core::engine::Engine;
use vortex_core::event::{Event, Field, Kind};
use vortex_core::filter::{Filter, FIELD_HELP};
use vortex_core::net::{ConnState, ConnStatus, SourceKind, StartFrom};
use vortex_core::nsid::Nsid;
use vortex_core::stats::{RateSampler, Rates, Snapshot};
use vortex_core::store::{FacetCount, RefilterStats};
use vortex_core::time::{format_rfc3339_us, now_us};

actions!(
    vortex,
    [
        FocusFilter,
        SelectNext,
        SelectPrev,
        SelectFirst,
        SelectLast,
        PageDown,
        PageUp,
        ToggleFollow,
        Deselect,
        ClearBuffer,
        ToggleSettings,
        ToggleHelp,
        ToggleSidebar,
        ToggleDetail,
        ToggleConnect,
        FontBigger,
        FontSmaller,
        NextTheme,
        CopyJson,
        CopyUri,
        OpenInBrowser,
        Quit,
    ]
);

const ROOT_CONTEXT: &str = "Vortex";
const LIST_CONTEXT: &str = "EventList";

pub fn bind_keys(cx: &mut App) {
    let root = Some(ROOT_CONTEXT);
    let list = Some(LIST_CONTEXT);
    cx.bind_keys([
        // Anywhere in the window.
        KeyBinding::new("cmd-f", FocusFilter, root),
        KeyBinding::new("cmd-l", FocusFilter, root),
        KeyBinding::new("cmd-k", ClearBuffer, root),
        KeyBinding::new("cmd-,", ToggleSettings, root),
        KeyBinding::new("cmd-b", ToggleSidebar, root),
        KeyBinding::new("cmd-i", ToggleDetail, root),
        KeyBinding::new("cmd-=", FontBigger, root),
        KeyBinding::new("cmd--", FontSmaller, root),
        KeyBinding::new("cmd-t", NextTheme, root),
        KeyBinding::new("cmd-enter", ToggleConnect, root),
        KeyBinding::new("cmd-q", Quit, None),
        // Only while the list has focus, so typing in the filter is unaffected.
        KeyBinding::new("/", FocusFilter, list),
        KeyBinding::new("j", SelectNext, list),
        KeyBinding::new("down", SelectNext, list),
        KeyBinding::new("k", SelectPrev, list),
        KeyBinding::new("up", SelectPrev, list),
        KeyBinding::new("g", SelectFirst, list),
        KeyBinding::new("home", SelectFirst, list),
        KeyBinding::new("shift-g", SelectLast, list),
        KeyBinding::new("end", SelectLast, list),
        KeyBinding::new("pagedown", PageDown, list),
        KeyBinding::new("pageup", PageUp, list),
        KeyBinding::new("ctrl-d", PageDown, list),
        KeyBinding::new("ctrl-u", PageUp, list),
        KeyBinding::new("space", ToggleFollow, list),
        KeyBinding::new("f", ToggleFollow, list),
        KeyBinding::new("escape", Deselect, list),
        KeyBinding::new("?", ToggleHelp, list),
        KeyBinding::new("c", CopyJson, list),
        KeyBinding::new("u", CopyUri, list),
        KeyBinding::new("o", OpenInBrowser, list),
        KeyBinding::new("enter", OpenInBrowser, list),
    ]);
}

/// How long the render path will wait for the store before reusing the
/// previous frame.
const VIEW_TIMEOUT: Duration = Duration::from_millis(2);
const TICK: Duration = Duration::from_millis(33);
const FILTER_DEBOUNCE: Duration = Duration::from_millis(140);
/// Height of the combined title/top bar, tall enough for the traffic lights.
const TITLEBAR_HEIGHT: f32 = 26.;
const TRAFFIC_LIGHTS_WIDTH: f32 = 70.;
const REWINDS: [(&str, u64); 5] = [("live", 0), ("1m", 60), ("5m", 300), ("30m", 1800), ("2h", 7200)];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Sidebar,
    Detail,
}

/// Payload of a pane-resize drag.
#[derive(Clone)]
struct ResizeDrag(Pane);

/// Invisible view shown under the cursor while resizing.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

struct FacetRow {
    nsid: &'static str,
    count: FacetCount,
}

/// The selected event, prepared for display.
struct Detail {
    event: Event,
    lines: Arc<Vec<JsonLine>>,
}

pub struct Vortex {
    engine: Arc<Engine>,
    cfg: ConfigFile,
    cfg_dirty: bool,
    focus: FocusHandle,
    filter_input: Entity<TextInput>,
    font_input: Entity<TextInput>,

    filter_error: Option<String>,
    filter_gen: u64,
    refilter: Option<RefilterStats>,

    scroll: UniformListScrollHandle,
    detail_scroll: UniformListScrollHandle,
    follow: bool,
    /// A downward scroll happened; re-check whether we are back at the tail.
    recheck_tail: bool,
    selected: Option<u64>,
    detail: Option<Detail>,
    row_cache: Vec<RowData>,
    row_cache_start: usize,

    view_len: usize,
    view_generation: u64,
    dropped_front: u64,
    buffered: usize,
    buffered_bytes: usize,
    span_us: i64,

    sampler: RateSampler,
    rates: Rates,
    snap: Snapshot,
    facets: Vec<FacetRow>,
    kinds: [FacetCount; 4],
    status: ConnStatus,
    seen_store: u64,
    seen_log: u64,
    seen_status: u64,
    ticks: u64,

    rewind_secs: u64,
    show_settings: bool,
    show_help: bool,
    measured_font: Option<(SharedString, Pixels)>,
    /// Window corner style last pushed to the platform.
    applied_square: Option<bool>,
}

impl Vortex {
    pub fn new(engine: Arc<Engine>, cfg: ConfigFile, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter_input = cx.new(|cx| TextInput::new("filter  ·  type:post lang:en has:media  ·  ? for help", cx));
        let font_input = cx.new(|cx| TextInput::new("font family", cx));
        let query = cfg.config.filter.query.clone();
        filter_input.update(cx, |i, cx| i.set_text(&query, cx));
        font_input.update(cx, |i, cx| i.set_text(&cfg.config.ui.font, cx));

        cx.subscribe_in(&filter_input, window, |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::Change => this.filter_changed(false, cx),
            InputEvent::Submit => {
                this.filter_changed(true, cx);
                window.focus(&this.focus, cx);
            }
            InputEvent::Cancel => window.focus(&this.focus, cx),
        })
        .detach();
        cx.subscribe_in(&font_input, window, |this, input, ev: &InputEvent, window, cx| match ev {
            InputEvent::Submit => {
                let font = input.read(cx).text().trim().to_owned();
                this.cfg.config.ui.font = if font.is_empty() { BUNDLED_FONT.into() } else { font };
                this.config_changed(cx);
                window.focus(&this.focus, cx);
            }
            InputEvent::Cancel => window.focus(&this.focus, cx),
            InputEvent::Change => {}
        })
        .detach();

        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(TICK).await;
            if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                break;
            }
        })
        .detach();

        let mut this = Vortex {
            engine,
            cfg,
            cfg_dirty: false,
            focus: cx.focus_handle(),
            filter_input,
            font_input,
            filter_error: None,
            filter_gen: 0,
            refilter: None,
            scroll: UniformListScrollHandle::new(),
            detail_scroll: UniformListScrollHandle::new(),
            follow: true,
            recheck_tail: false,
            selected: None,
            detail: None,
            row_cache: Vec::new(),
            row_cache_start: 0,
            view_len: 0,
            view_generation: 0,
            dropped_front: 0,
            buffered: 0,
            buffered_bytes: 0,
            span_us: 0,
            sampler: RateSampler::new(48),
            rates: Rates::default(),
            snap: Snapshot::default(),
            facets: Vec::new(),
            kinds: Default::default(),
            status: ConnStatus::default(),
            seen_store: 0,
            seen_log: 0,
            seen_status: 0,
            ticks: 0,
            rewind_secs: 0,
            show_settings: false,
            show_help: false,
            measured_font: None,
            applied_square: None,
        };
        this.apply_config(cx);
        if let Some(e) = this.cfg.error.clone() {
            this.engine.log.error(format!("config: {e}"));
        }
        this.filter_changed(true, cx);
        if this.cfg.config.source.autoconnect {
            this.connect(cx);
        }
        window.focus(&this.focus, cx);
        this
    }

    // ---- periodic work -------------------------------------------------

    fn tick(&mut self, cx: &mut Context<Self>) {
        self.ticks += 1;
        let mut dirty = false;
        let (store, log, status) = (self.engine.store.version(), self.engine.log.version(), self.engine.status_version());
        if (store, log, status) != (self.seen_store, self.seen_log, self.seen_status) {
            (self.seen_store, self.seen_log, self.seen_status) = (store, log, status);
            self.status = self.engine.status();
            dirty = true;
        }
        if self.ticks % 8 == 0 {
            self.snap = self.engine.stats.snapshot();
            self.rates = self.sampler.sample(self.snap);
            self.refresh_facets();
            dirty = true;
        }
        if self.ticks % 30 == 0 {
            if self.cfg_dirty {
                self.cfg_dirty = false;
                self.cfg.save();
            } else if self.cfg.reload_if_changed() {
                match self.cfg.error.clone() {
                    Some(e) => self.engine.log.error(format!("config: {e}")),
                    None => {
                        self.engine.log.info("config reloaded");
                        self.apply_config(cx);
                    }
                }
                dirty = true;
            }
        }
        #[cfg(debug_assertions)]
        if self.ticks == 150 {
            self.debug_hook(cx);
        }
        if dirty {
            cx.notify();
        }
    }

    /// Debug builds only: `VORTEX_DEBUG=select|settings|badfilter` drives the
    /// UI into a state a few seconds after launch, for visual checks.
    #[cfg(debug_assertions)]
    fn debug_hook(&mut self, cx: &mut Context<Self>) {
        match std::env::var("VORTEX_DEBUG").as_deref() {
            Ok("select") => {
                let id = {
                    let view = self.engine.store.view();
                    (0..view.len()).rev().filter_map(|i| view.row(i)).find(|e| e.text().len() > 40).map(|e| e.id)
                };
                self.follow = false;
                self.select(id, cx);
            }
            Ok("settings") => self.show_settings = true,
            Ok("badfilter") => self.set_query("type:post lnag:en (has:media", cx),
            _ => {}
        }
        cx.notify();
    }

    fn refresh_facets(&mut self) {
        let Some(view) = self.engine.store.try_view(VIEW_TIMEOUT) else { return };
        let facets = view.facets();
        let mut rows: Vec<FacetRow> = Nsid::all()
            .into_iter()
            .map(|n| FacetRow { nsid: n.as_str(), count: facets.collection(n.index()) })
            .filter(|r| r.count.seen > 0 || r.count.matched > 0)
            .collect();
        rows.sort_by(|a, b| b.count.seen.cmp(&a.count.seen).then(a.nsid.cmp(b.nsid)));
        rows.truncate(80);
        self.facets = rows;
        self.kinds = Kind::ALL.map(|k| facets.kind(k));
        self.buffered = view.ring_len() + view.pinned_len();
        self.buffered_bytes = view.bytes();
        self.span_us = view.ring_span_us();
    }

    // ---- configuration -------------------------------------------------

    /// Push the current config into the theme, the store and the engine.
    fn apply_config(&mut self, cx: &mut Context<Self>) {
        let c = &self.cfg.config;
        let (palette, problems) = resolve_palette(c);
        for p in problems {
            self.engine.log.error(p);
        }
        let known = cx.text_system().all_font_names();
        let font: SharedString = if known.iter().any(|f| f == &c.ui.font) {
            c.ui.font.clone().into()
        } else {
            let similar: Vec<&String> = known.iter().filter(|f| f.contains("Berkeley")).collect();
            eprintln!("vortex: font '{}' not found (similar: {similar:?})", c.ui.font);
            self.engine.log.error(format!("font '{}' not found, using {BUNDLED_FONT}", c.ui.font));
            BUNDLED_FONT.into()
        };
        cx.set_global(Theme::build(c, palette, font));
        self.measured_font = None;
        self.engine.store.set_config(c.buffer.to_store());
        self.engine.set_pushdown_enabled(c.source.pushdown);
        cx.refresh_windows();
    }

    /// A setting was changed from the UI: apply now, write the file shortly.
    fn config_changed(&mut self, cx: &mut Context<Self>) {
        self.cfg_dirty = true;
        self.apply_config(cx);
        cx.notify();
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let start = match self.rewind_secs {
            0 => StartFrom::Live,
            s => StartFrom::Rewind(Duration::from_secs(s)),
        };
        self.engine.connect(self.cfg.config.source.to_source(start));
        self.follow = true;
        cx.notify();
    }

    fn reconnect_fresh(&mut self, cx: &mut Context<Self>) {
        self.engine.clear();
        self.select(None, cx);
        self.connect(cx);
    }

    // ---- filter ----------------------------------------------------------

    fn filter_changed(&mut self, immediate: bool, cx: &mut Context<Self>) {
        self.filter_gen += 1;
        let gen = self.filter_gen;
        let text = self.filter_input.read(cx).text().to_owned();
        // Validate on the spot so errors track typing; apply after a pause.
        match Filter::parse(&text) {
            Err(e) => {
                self.filter_error = Some(e.message.clone());
                self.filter_input.update(cx, |i, cx| i.set_error_range(Some(e.span.clone()), cx));
                cx.notify();
                return;
            }
            Ok(_) => {
                self.filter_error = None;
                self.filter_input.update(cx, |i, cx| i.set_error_range(None, cx));
            }
        }
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            if !immediate {
                cx.background_executor().timer(FILTER_DEBOUNCE).await;
            }
            if this.update(cx, |this, _| this.filter_gen != gen).unwrap_or(true) {
                return;
            }
            // Compile and re-filter the buffer off the UI thread.
            let query = text.clone();
            let stats = cx
                .background_executor()
                .spawn(async move { Filter::parse(&query).ok().map(|f| engine.set_filter(f)) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.filter_gen == gen {
                    this.refilter = stats;
                    this.follow = true;
                    if this.cfg.config.filter.query != text {
                        this.cfg.config.filter.query = text;
                        this.cfg_dirty = true;
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn set_query(&mut self, query: &str, cx: &mut Context<Self>) {
        self.filter_input.update(cx, |i, cx| i.set_text(query, cx));
        self.filter_changed(true, cx);
    }

    /// Toggle `value` in the comma list of a `field:` term (`-field:` when
    /// `exclude`), adding or removing the term as needed.
    fn toggle_term(&mut self, field: &str, value: &str, exclude: bool, cx: &mut Context<Self>) {
        let text = self.filter_input.read(cx).text().to_owned();
        self.set_query(&toggle_in_query(&text, field, value, exclude), cx);
    }

    // ---- selection -------------------------------------------------------

    fn select(&mut self, id: Option<u64>, cx: &mut Context<Self>) {
        self.selected = id;
        self.detail = id.and_then(|id| {
            let view = self.engine.store.view();
            let event = view.get(id)?.clone();
            drop(view);
            let lines = Arc::new(json_lines(event.raw(), 4000, 2000));
            Some(Detail { event, lines })
        });
        if self.detail.is_none() {
            self.selected = None;
        }
        cx.notify();
    }

    /// Move the selection by `delta` rows (or to an end with `usize::MAX`).
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let target = {
            let view = self.engine.store.view();
            let len = view.len();
            if len == 0 {
                return;
            }
            let current = self.selected.and_then(|id| view.index_of(id));
            let ix = match current {
                Some(i) => i.saturating_add_signed(delta).min(len - 1),
                None if delta < 0 => len - 1,
                None => self.top_row(cx).min(len - 1),
            };
            view.row(ix).map(|e| (ix, e.id))
        };
        if let Some((ix, id)) = target {
            self.follow = false;
            self.select(Some(id), cx);
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    /// Index of the first visible row.
    fn top_row(&self, cx: &App) -> usize {
        let row = Theme::get(cx).row;
        let y = self.scroll.0.borrow().base_handle.offset().y;
        (-y / row).max(0.0) as usize
    }

    fn page_rows(&self, cx: &App) -> isize {
        let row = Theme::get(cx).row;
        let h = self.scroll.0.borrow().base_handle.bounds().size.height;
        ((h / row) as isize - 2).max(1)
    }

    fn copy(&self, text: String, what: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.engine.log.info(format!("copied {what}"));
    }

    // ---- small view helpers ------------------------------------------------

    fn chip(&self, label: impl Into<SharedString>, active: bool, t: &Theme) -> Div {
        let hover = t.bg_hover;
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(t.row)
            .px(t.pad)
            .rounded(t.radius)
            .cursor_pointer()
            .when(active, |d| d.bg(t.accent).text_color(t.bg))
            .when(!active, |d| d.text_color(t.fg_dim).hover(move |s| s.bg(hover)))
            .child(label.into())
    }

    fn stat(&self, value: impl Into<SharedString>, unit: &'static str, color: Hsla, t: &Theme) -> Div {
        div()
            .flex()
            .flex_none()
            .items_center()
            .child(div().text_color(color).child(value.into()))
            .child(div().text_color(t.fg_faint).child(unit))
    }

    fn sep(&self, t: &Theme) -> Div {
        div().flex_none().w(px(1.)).h(t.row * 0.7).bg(t.border)
    }

    fn section(&self, title: &'static str, t: &Theme) -> Div {
        div().flex_none().h(t.row).px(t.pad).flex().items_center().text_color(t.fg_faint).child(title)
    }

    // ---- regions ---------------------------------------------------------

    fn render_top_bar(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> Div {
        let src = &self.cfg.config.source;
        let kind = src.kind();
        let (dot, state_text): (Hsla, SharedString) = match self.status.state {
            ConnState::Live => (t.green, self.status.host.clone().into()),
            ConnState::Connecting => (t.yellow, format!("… {}", self.status.host).into()),
            ConnState::Waiting => (t.red, self.status.error.clone().unwrap_or_else(|| "retrying".into()).into()),
            ConnState::Idle => (t.fg_faint, "off".into()),
        };
        let lag = if self.status.state == ConnState::Live && self.snap.last_time_us > 0 {
            fmt_duration_us(now_us() - self.snap.last_time_us)
        } else {
            "–".into()
        };
        let history: Vec<f32> = self.sampler.history.iter().copied().collect();
        let matched: Vec<f32> = self.sampler.matched_history.iter().copied().collect();
        let (bar_color, match_color) = (t.fg_faint, t.accent);

        let mut bar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(t.pad)
            .px(t.pad)
            .min_h(px(TITLEBAR_HEIGHT))
            .bg(t.bg_alt)
            .border_b_1()
            .border_color(t.border)
            // Doubles as the window title bar.
            .window_control_area(WindowControlArea::Drag)
            .when(cfg!(target_os = "macos") && !window.is_fullscreen(), |d| d.pl(px(TRAFFIC_LIGHTS_WIDTH)))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(t.pad)
                    .px(t.pad)
                    .h(t.row)
                    .rounded(t.radius)
                    .cursor_pointer()
                    .hover({
                        let h = t.bg_hover;
                        move |s| s.bg(h)
                    })
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.toggle_connect(&ToggleConnect, window, cx)))
                    .child(div().text_color(dot).child("●"))
                    .child(div().max_w(t.ch * 40.).truncate().text_color(t.fg_dim).child(state_text)),
            )
            .child(self.sep(t));

        for k in SourceKind::ALL {
            let label = match k {
                SourceKind::JetstreamV2 => "jetstream",
                SourceKind::JetstreamV1 => "v1",
                SourceKind::Firehose => "firehose",
            };
            bar = bar.child(self.chip(label, k == kind, t).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.cfg.config.source.kind() != k {
                        this.cfg.config.source.kind = k.label().into();
                        this.cfg.config.source.hosts.clear();
                        this.cfg_dirty = true;
                        this.reconnect_fresh(cx);
                    }
                }),
            ));
        }
        bar = bar.child(self.sep(t));
        if kind != SourceKind::Firehose {
            for (label, secs) in REWINDS {
                bar = bar.child(self.chip(label, self.rewind_secs == secs, t).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.rewind_secs = secs;
                        this.reconnect_fresh(cx);
                    }),
                ));
            }
            bar = bar
                .child(self.sep(t))
                .child(self.chip("zstd", src.compress, t).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.cfg.config.source.compress ^= true;
                        this.cfg_dirty = true;
                        this.connect(cx);
                    }),
                ))
                .child(self.chip("srv", src.pushdown, t).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.cfg.config.source.pushdown ^= true;
                        this.config_changed(cx);
                    }),
                ));
        }

        bar.child(div().flex_1())
            .child(self.stat(fmt_rate(self.rates.events_per_s), "/s", t.fg, t))
            .child(self.stat(fmt_rate(self.rates.matched_per_s), "✓/s", t.accent, t))
            .child(self.stat(fmt_bytes(self.rates.wire_bytes_per_s), "/s", t.fg_dim, t))
            .child(self.stat(lag, " lag", t.fg_dim, t))
            .child(self.stat(format!("{:.0}%", self.rates.pipeline_load * 100.0), " cpu", t.fg_dim, t))
            .child(self.sep(t))
            .child(self.stat(fmt_count(self.buffered as u64), " ev", t.fg_dim, t))
            .child(self.stat(fmt_bytes(self.buffered_bytes as f64), "", t.fg_dim, t))
            .child(self.stat(fmt_duration_us(self.span_us), "", t.fg_dim, t))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds: Bounds<Pixels>, _, window, _| {
                        let peak = history.iter().copied().fold(1.0f32, f32::max);
                        let n = history.len().max(1) as f32;
                        let w = bounds.size.width / n;
                        for (i, (total, hit)) in history.iter().zip(matched.iter()).enumerate() {
                            let x = bounds.left() + w * i as f32;
                            for (v, color) in [(*total, bar_color), (*hit, match_color)] {
                                let h = bounds.size.height * (v / peak).clamp(0.0, 1.0);
                                if h > px(0.5) {
                                    window.paint_quad(fill(
                                        Bounds::new(point(x, bounds.bottom() - h), size((w - px(1.)).max(px(1.)), h)),
                                        color,
                                    ));
                                }
                            }
                        }
                    },
                )
                .w(t.ch * 16.)
                .h(t.row * 0.8)
                .flex_none(),
            )
            .child(self.sep(t))
            .child(self.chip("?", self.show_help, t).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.toggle_help(&ToggleHelp, window, cx)),
            ))
            .child(self.chip("⚙", self.show_settings, t).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.toggle_settings(&ToggleSettings, window, cx)),
            ))
    }

    fn render_filter_bar(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> Div {
        let focused = self.filter_input.focus_handle(cx).is_focused(window);
        let mut right = div().flex().flex_none().items_center().gap(t.pad * 2.);
        if !self.status.server_filter.is_empty() {
            right = right.child(self.stat(self.status.server_filter.clone(), "", t.aqua, t).child(div().text_color(t.fg_faint).child(" srv")));
        }
        if let Some(r) = &self.refilter {
            right = right.child(self.stat(fmt_duration_us(r.elapsed.as_micros() as i64), "", t.fg_faint, t));
        }
        right = right.child(self.stat(fmt_count(self.view_len as u64), " rows", t.fg, t)).child(
            self.chip(if self.follow { "▼ tail" } else { "▽ tail" }, self.follow, t).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.toggle_follow(&ToggleFollow, window, cx)),
            ),
        );

        let mut bar = div().flex().flex_col().flex_none().bg(t.bg).border_b_1().border_color(t.border).child(
            div()
                .flex()
                .items_center()
                .gap(t.pad)
                .px(t.pad)
                .py(t.pad * 0.5)
                .child(div().flex_none().text_color(if focused { t.accent } else { t.fg_faint }).child("/"))
                .child(self.filter_input.clone())
                .child(right),
        );
        if let Some(err) = &self.filter_error {
            bar = bar.child(div().px(t.pad).pb(t.pad * 0.5).text_color(t.red).truncate().child(err.clone()));
        }
        bar
    }

    fn render_sidebar(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let query = self.filter_input.read(cx).text().to_owned();
        let count_cell = |c: FacetCount, t: &Theme| {
            div()
                .flex()
                .flex_none()
                .gap(t.ch)
                .child(div().text_color(if c.matched > 0 { t.accent } else { t.fg_faint }).child(fmt_count(c.matched as u64)))
                .child(div().w(t.ch * 5.).flex().justify_end().text_color(t.fg_dim).child(fmt_count(c.seen as u64)))
        };
        let facet_row = |label: SharedString, color: Hsla, active: bool, c: FacetCount, t: &Theme| {
            let hover = t.bg_hover;
            div()
                .flex()
                .flex_none()
                .items_center()
                .h(t.row)
                .px(t.pad)
                .gap(t.ch)
                .cursor_pointer()
                .when(active, |d| d.bg(t.bg_sel))
                .hover(move |s| s.bg(hover))
                .child(div().flex_1().min_w_0().truncate().text_color(color).child(label))
                .child(count_cell(c, t))
        };

        let mut kinds = div().flex().flex_col().flex_none();
        for k in Kind::ALL {
            let c = self.kinds[k as usize];
            if c.seen == 0 && c.matched == 0 && k != Kind::Commit {
                continue;
            }
            let name = k.as_str();
            let active = query_has(&query, "kind", name);
            kinds = kinds.child(facet_row(name.into(), t.fg, active, c, t).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    this.toggle_term("kind", name, ev.modifiers.alt || ev.modifiers.shift, cx)
                }),
            ));
        }

        let mut collections = div().id("facet-collections").flex().flex_col().flex_1().min_h_0().overflow_y_scroll();
        for row in &self.facets {
            let nsid = row.nsid;
            let active = query_has(&query, "collection", nsid);
            collections = collections.child(
                facet_row(short_collection(nsid).into(), t.hue(nsid), active, row.count, t).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        this.toggle_term("collection", nsid, ev.modifiers.alt || ev.modifiers.shift, cx)
                    }),
                ),
            );
        }

        let mut saved = div().id("saved-filters").flex().flex_col().flex_none().max_h(t.row * 9.).overflow_y_scroll();
        for (i, s) in self.cfg.config.filter.saved.iter().enumerate() {
            let q = s.query.clone();
            let hover = t.bg_hover;
            saved = saved.child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(t.row)
                    .px(t.pad)
                    .gap(t.ch)
                    .hover(move |s| s.bg(hover))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .cursor_pointer()
                            .text_color(if q == query { t.accent } else { t.fg_dim })
                            .child(s.name.clone())
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.set_query(&q, cx))),
                    )
                    .child(div().flex_none().cursor_pointer().text_color(t.fg_faint).child("×").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if i < this.cfg.config.filter.saved.len() {
                                this.cfg.config.filter.saved.remove(i);
                                this.cfg_dirty = true;
                                cx.notify();
                            }
                        }),
                    )),
            );
        }

        div()
            .flex()
            .flex_col()
            .flex_none()
            .h_full()
            .w(px(self.cfg.config.ui.sidebar_width.clamp(120., 600.)))
            .bg(t.bg_alt)
            .child(self.section("kind", t))
            .child(kinds)
            .child(self.section("collection", t))
            .child(collections)
            .child(
                self.section("saved", t).justify_between().child(
                    div().cursor_pointer().text_color(t.fg_dim).child("+").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            let q = this.filter_input.read(cx).text().trim().to_owned();
                            let saved = &mut this.cfg.config.filter.saved;
                            if !q.is_empty() && !saved.iter().any(|s| s.query == q) {
                                saved.push(SavedFilter { name: q.clone(), query: q });
                                this.cfg_dirty = true;
                                cx.notify();
                            }
                        }),
                    ),
                ),
            )
            .child(saved)
    }

    fn render_rows(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) -> Vec<Div> {
        let t = Theme::get(cx).clone();
        // Refresh from the store if it is free; otherwise reuse last frame.
        if let Some(view) = self.engine.store.try_view(VIEW_TIMEOUT) {
            self.row_cache.clear();
            self.row_cache_start = range.start;
            self.row_cache.extend(range.clone().map_while(|i| view.row(i)).map(row_data));
        }
        let selected = self.selected;
        range
            .map(|i| {
                let row = i.checked_sub(self.row_cache_start).and_then(|o| self.row_cache.get(o));
                let Some(r) = row else {
                    return div().h(t.row).w_full();
                };
                let id = r.id;
                let is_sel = selected == Some(id);
                let hover = t.bg_hover;
                div()
                    .flex()
                    .items_center()
                    .h(t.row)
                    .w_full()
                    .px(t.pad)
                    .gap(t.ch)
                    .when(is_sel, |d| d.bg(t.bg_sel))
                    .when(!is_sel, |d| d.hover(move |s| s.bg(hover)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.follow = false;
                            this.select(Some(id), cx);
                            window.focus(&this.focus, cx);
                        }),
                    )
                    .child(div().flex_none().w(t.ch * 12.5).text_color(t.fg_faint).child(r.time.clone()))
                    .child(div().flex_none().w(t.ch).text_color(r.tone.color(&t)).child(r.glyph))
                    .child(
                        div()
                            .flex_none()
                            .w(t.ch * 18.)
                            .truncate()
                            .text_color(if r.collection.is_empty() { t.fg_dim } else { t.hue(r.collection) })
                            .child(r.what.clone()),
                    )
                    .child(div().flex_none().w(t.ch * 16.).truncate().text_color(t.fg_faint).child(r.did.clone()))
                    .child(div().flex_none().w(t.ch * 4.).truncate().text_color(t.fg_dim).child(r.marks.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(if r.text_is_ref { t.fg_faint } else { t.fg })
                            .child(r.text.clone()),
                    )
            })
            .collect()
    }

    fn render_list(&mut self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .h(t.row)
            .px(t.pad)
            .gap(t.ch)
            .bg(t.bg_alt)
            .text_color(t.fg_faint)
            .child(div().flex_none().w(t.ch * 12.5).child("utc"))
            .child(div().flex_none().w(t.ch))
            .child(div().flex_none().w(t.ch * 18.).child("type"))
            .child(div().flex_none().w(t.ch * 16.).child("did"))
            .child(div().flex_none().w(t.ch * 4.))
            .child(div().flex_1().child("text"));

        let list = uniform_list("events", self.view_len, cx.processor(|this, range, _window, cx| this.render_rows(range, cx)))
            .track_scroll(&self.scroll)
            .flex_1()
            .w_full();

        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .key_context(LIST_CONTEXT)
            .track_focus(&self.focus)
            .on_scroll(cx.listener(|this, ev: &ScrollEvent, window, cx| {
                let dy = ev.delta.pixel_delta(window.line_height()).y;
                if dy > px(0.) {
                    this.follow = false;
                } else if dy < px(0.) {
                    this.recheck_tail = true;
                }
                cx.notify();
            }))
            .child(header)
            .child(list);
        if self.view_len == 0 {
            let hint = match (self.status.state, self.snap.events) {
                (ConnState::Idle, _) => "disconnected — click ● to connect",
                (_, 0) => "waiting for events…",
                _ => "no events match",
            };
            body = body.child(
                div().absolute().top(t.row * 3.).left_0().right_0().flex().justify_center().text_color(t.fg_faint).child(hint),
            );
        }
        body
    }

    fn render_detail(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let pane = div()
            .flex()
            .flex_col()
            .flex_none()
            .h_full()
            .w(px(self.cfg.config.ui.detail_width.clamp(160., 1600.)))
            .bg(t.bg_alt);

        if self.show_help || self.detail.is_none() {
            let mut help = div().id("help").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().p(t.pad).gap(px(1.));
            for (syntax, meaning) in FIELD_HELP {
                help = help.child(
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_color(t.blue).child(*syntax))
                        .child(div().pl(t.ch * 2.).pb(t.pad * 0.5).text_color(t.fg_dim).child(*meaning)),
                );
            }
            let keys = [
                ("/", "filter"),
                ("j k ↑ ↓", "move"),
                ("g G", "first / last"),
                ("space", "tail on/off"),
                ("c u o", "copy json / copy uri / open"),
                ("⌘K", "clear"),
                ("⌘B ⌘I", "sidebar / detail"),
                ("⌘T ⌘= ⌘-", "theme / font size"),
                ("⌘,", "settings"),
            ];
            help = help.child(div().h(t.row));
            for (k, v) in keys {
                help = help.child(
                    div()
                        .flex()
                        .gap(t.ch)
                        .child(div().w(t.ch * 11.).flex_none().text_color(t.yellow).child(k))
                        .child(div().text_color(t.fg_dim).child(v)),
                );
            }
            return pane.child(self.section("filter syntax", t)).child(help);
        }
        let Some(d) = &self.detail else { return pane };
        let e = &d.event;
        let uri = e.at_uri();

        let mut head = div().flex().flex_col().flex_none().p(t.pad).gap(px(1.)).border_b_1().border_color(t.border);
        let title = if e.collection.is_some() {
            format!("{} {}", e.op.as_str(), e.collection_str())
        } else {
            e.kind.as_str().to_owned()
        };
        head = head
            .child(div().text_color(t.hue(e.collection_str())).truncate().child(title))
            .child(div().text_color(t.fg_dim).truncate().child(uri.clone().unwrap_or_else(|| e.did.to_string())));
        let mut when = format!("{}  seq {}", format_rfc3339_us(e.time_us), e.seq);
        if e.created_us != 0 {
            when.push_str(&format!("  created {}", fmt_duration_us(e.created_us - e.time_us)));
        }
        head = head.child(div().text_color(t.fg_faint).truncate().child(when));
        if !e.text().is_empty() {
            head = head.child(
                div()
                    .id("detail-text")
                    .py(t.pad)
                    .max_h(t.row * 10.)
                    .overflow_y_scroll()
                    .text_color(t.fg)
                    .child(SharedString::from(e.text().to_owned())),
            );
        }
        let mut meta = Vec::new();
        for (label, field) in [("lang", Field::Langs), ("tag", Field::Tags), ("link", Field::Links), ("label", Field::Labels)] {
            let items: Vec<&str> = e.list(field).collect();
            if !items.is_empty() {
                meta.push(format!("{label} {}", items.join(" ")));
            }
        }
        for (label, field) in [("re", Field::Parent), ("quote", Field::Quote), ("→", Field::Subject), ("alt", Field::Alt)] {
            if !e.field(field).is_empty() {
                meta.push(format!("{label} {}", e.field(field).replace('\n', " ⏎ ")));
            }
        }
        for m in meta {
            head = head.child(div().text_color(t.fg_dim).truncate().child(m));
        }

        let did = e.did.to_string();
        let collection = e.collection_str();
        let mut actions = div().flex().flex_wrap().gap(t.pad).pt(t.pad);
        actions = actions
            .child(self.chip("json", false, t).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.copy_json(&CopyJson, window, cx)),
            ))
            .child(self.chip("uri", false, t).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.copy_uri(&CopyUri, window, cx)),
            ))
            .child(self.chip("open", false, t).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.open_in_browser(&OpenInBrowser, window, cx)),
            ))
            .child(self.chip("+did", false, t).on_mouse_down(MouseButton::Left, {
                let did = did.clone();
                cx.listener(move |this, _, _, cx| this.toggle_term("did", &did, false, cx))
            }))
            .child(self.chip("-did", false, t).on_mouse_down(MouseButton::Left, {
                let did = did.clone();
                cx.listener(move |this, _, _, cx| this.toggle_term("did", &did, true, cx))
            }));
        if !collection.is_empty() {
            actions = actions
                .child(self.chip("+type", false, t).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.toggle_term("collection", collection, false, cx)),
                ))
                .child(self.chip("-type", false, t).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.toggle_term("collection", collection, true, cx)),
                ));
        }
        head = head.child(actions);

        let lines = d.lines.clone();
        let theme = t.clone();
        let json = uniform_list("json", lines.len(), move |range, _window, _cx| {
            range
                .map(|i| {
                    let line = &lines[i];
                    let highlights = line.spans.iter().map(|(r, tok)| (r.clone(), tok.style(&theme)));
                    div()
                        .h(theme.row)
                        .px(theme.pad)
                        .whitespace_nowrap()
                        .text_color(theme.fg)
                        .child(StyledText::new(line.text.clone()).with_highlights(highlights))
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.detail_scroll)
        .flex_1()
        .w_full();

        pane.child(head).child(json)
    }

    fn render_settings(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let ui = &self.cfg.config.ui;
        let stepper = |label: &'static str,
                       value: String,
                       t: &Theme,
                       cx: &mut Context<Self>,
                       apply: fn(&mut Vortex, f32)|
         -> Div {
            div()
                .flex()
                .items_center()
                .h(t.row)
                .gap(t.ch)
                .child(div().w(t.ch * 12.).text_color(t.fg_dim).child(label))
                .child(self.chip("−", false, t).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        apply(this, -1.0);
                        this.config_changed(cx);
                    }),
                ))
                .child(div().w(t.ch * 6.).flex().justify_center().text_color(t.fg).child(value))
                .child(self.chip("+", false, t).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        apply(this, 1.0);
                        this.config_changed(cx);
                    }),
                ))
        };

        let mut themes = div().flex().flex_wrap().gap(px(2.));
        for name in theme_names(&self.cfg.config) {
            let active = name == ui.theme;
            let n = name.clone();
            themes = themes.child(self.chip(name, active, t).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.cfg.config.ui.theme = n.clone();
                    this.config_changed(cx);
                }),
            ));
        }

        let path = self.cfg.path.clone();
        div()
            .absolute()
            .top(px(TITLEBAR_HEIGHT + 2.))
            .right(t.pad)
            .w(t.ch * 46.)
            .flex()
            .flex_col()
            .gap(t.pad)
            .p(t.pad * 2.)
            .bg(t.bg_alt)
            .border_1()
            .border_color(t.border)
            .rounded(t.radius)
            .occlude()
            .child(div().text_color(t.fg_faint).child("theme"))
            .child(themes)
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(t.row)
                    .gap(t.ch)
                    .child(div().w(t.ch * 12.).flex_none().text_color(t.fg_dim).child("font"))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .px(t.pad)
                            .bg(t.bg)
                            .border_1()
                            .border_color(t.border)
                            .rounded(t.radius)
                            .child(self.font_input.clone()),
                    ),
            )
            .child(stepper("font size", format!("{:.0}", ui.font_size), t, cx, |v, d| {
                v.cfg.config.ui.font_size = (v.cfg.config.ui.font_size + d).clamp(7.0, 40.0)
            }))
            .child(stepper("line height", format!("{:.2}", ui.line_height), t, cx, |v, d| {
                v.cfg.config.ui.line_height = (v.cfg.config.ui.line_height + d * 0.05).clamp(1.0, 2.5)
            }))
            .child(stepper("padding", format!("{:.0}", ui.padding), t, cx, |v, d| {
                v.cfg.config.ui.padding = (v.cfg.config.ui.padding + d).clamp(0.0, 16.0)
            }))
            .child(stepper("radius", format!("{:.0}", ui.radius), t, cx, |v, d| {
                v.cfg.config.ui.radius = (v.cfg.config.ui.radius + d).clamp(0.0, 12.0)
            }))
            .when(cfg!(target_os = "macos"), |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .h(t.row)
                        .gap(t.ch)
                        .child(div().w(t.ch * 12.).flex_none().text_color(t.fg_dim).child("corners"))
                        .children([("round", false), ("square", true)].map(|(label, sq)| {
                            self.chip(label, ui.square_corners == sq, t).on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.cfg.config.ui.square_corners = sq;
                                    this.config_changed(cx);
                                }),
                            )
                        })),
                )
            })
            .child(stepper("buffer", fmt_count(self.cfg.config.buffer.events as u64), t, cx, |v, d| {
                let e = v.cfg.config.buffer.events;
                v.cfg.config.buffer.events = if d > 0.0 { (e * 2).min(8_000_000) } else { (e / 2).max(25_000) };
            }))
            .child(stepper("buffer MiB", self.cfg.config.buffer.megabytes.to_string(), t, cx, |v, d| {
                let m = v.cfg.config.buffer.megabytes;
                v.cfg.config.buffer.megabytes = if d > 0.0 { (m * 2).min(32_768) } else { (m / 2).max(64) };
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(t.ch)
                    .child(self.chip("open config.toml", false, t).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.cfg.save();
                            cx.open_with_system(&path);
                        }),
                    ))
                    .child(div().text_color(t.fg_faint).child("colors · themes")),
            )
    }

    fn render_status_bar(&self, t: &Theme) -> Div {
        let last = self.engine.log.last();
        let (text, color): (SharedString, Hsla) = match (&self.cfg.error, last) {
            (Some(e), _) => (format!("config.toml: {e}").into(), t.red),
            (None, Some(entry)) => (
                format!("{} {}", clock(entry.time_us), entry.text).into(),
                if entry.level == NoticeLevel::Error { t.orange } else { t.fg_faint },
            ),
            (None, None) => ("".into(), t.fg_faint),
        };
        let mut right = String::new();
        if self.snap.decode_errors > 0 {
            right.push_str(&format!("{} decode errors  ", self.snap.decode_errors));
        }
        if self.snap.queue_depth > 1000 {
            right.push_str(&format!("queue {}  ", fmt_count(self.snap.queue_depth as u64)));
        }
        right.push_str(&format!("{} total", fmt_count(self.snap.events)));
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(t.row)
            .px(t.pad)
            .gap(t.pad * 2.)
            .bg(t.bg_alt)
            .border_t_1()
            .border_color(t.border)
            .child(div().flex_1().min_w_0().truncate().text_color(color).child(text))
            .child(div().flex_none().text_color(t.fg_faint).child(right))
    }

    fn resize_handle(&self, pane: Pane, t: &Theme) -> impl IntoElement {
        let hover = t.accent;
        div()
            .id(match pane {
                Pane::Sidebar => "resize-sidebar",
                Pane::Detail => "resize-detail",
            })
            .flex_none()
            .w(px(3.))
            .h_full()
            .bg(t.border)
            .cursor_col_resize()
            .hover(move |s| s.bg(hover))
            .on_drag(ResizeDrag(pane), |_, _, _, cx| cx.new(|_| DragGhost))
    }

    // ---- actions ---------------------------------------------------------

    fn focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.filter_input.focus_handle(cx);
        window.focus(&handle, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(self.page_rows(cx), cx);
    }

    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-self.page_rows(cx), cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(isize::MIN / 2, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(isize::MAX / 2, cx);
        self.follow = true;
    }

    fn toggle_follow(&mut self, _: &ToggleFollow, _: &mut Window, cx: &mut Context<Self>) {
        self.follow = !self.follow;
        cx.notify();
    }

    fn deselect(&mut self, _: &Deselect, _: &mut Window, cx: &mut Context<Self>) {
        if self.show_settings || self.show_help {
            self.show_settings = false;
            self.show_help = false;
            cx.notify();
        } else {
            self.select(None, cx);
        }
    }

    fn clear_buffer(&mut self, _: &ClearBuffer, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.clear();
        self.select(None, cx);
        self.follow = true;
    }

    fn toggle_settings(&mut self, _: &ToggleSettings, _: &mut Window, cx: &mut Context<Self>) {
        self.show_settings = !self.show_settings;
        cx.notify();
    }

    fn toggle_help(&mut self, _: &ToggleHelp, _: &mut Window, cx: &mut Context<Self>) {
        self.show_help = !self.show_help;
        if self.show_help {
            self.cfg.config.ui.detail = true;
        }
        cx.notify();
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.cfg.config.ui.sidebar ^= true;
        self.cfg_dirty = true;
        cx.notify();
    }

    fn toggle_detail(&mut self, _: &ToggleDetail, _: &mut Window, cx: &mut Context<Self>) {
        self.cfg.config.ui.detail ^= true;
        self.cfg_dirty = true;
        cx.notify();
    }

    fn toggle_connect(&mut self, _: &ToggleConnect, _: &mut Window, cx: &mut Context<Self>) {
        if self.engine.is_connected() {
            self.engine.disconnect();
            cx.notify();
        } else {
            self.connect(cx);
        }
    }

    fn font_bigger(&mut self, _: &FontBigger, _: &mut Window, cx: &mut Context<Self>) {
        self.cfg.config.ui.font_size = (self.cfg.config.ui.font_size + 1.0).min(40.0);
        self.config_changed(cx);
    }

    fn font_smaller(&mut self, _: &FontSmaller, _: &mut Window, cx: &mut Context<Self>) {
        self.cfg.config.ui.font_size = (self.cfg.config.ui.font_size - 1.0).max(7.0);
        self.config_changed(cx);
    }

    fn next_theme(&mut self, _: &NextTheme, _: &mut Window, cx: &mut Context<Self>) {
        let names = theme_names(&self.cfg.config);
        let i = names.iter().position(|n| *n == self.cfg.config.ui.theme).map(|i| i + 1).unwrap_or(0);
        self.cfg.config.ui.theme = names[i % names.len()].clone();
        self.engine.log.info(format!("theme {}", self.cfg.config.ui.theme));
        self.config_changed(cx);
    }

    fn copy_json(&mut self, _: &CopyJson, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = &self.detail {
            self.copy(vortex_core::json::pretty(d.event.raw(), 2), "json", cx);
        }
    }

    fn copy_uri(&mut self, _: &CopyUri, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = &self.detail {
            self.copy(d.event.at_uri().unwrap_or_else(|| d.event.did.to_string()), "uri", cx);
        }
    }

    fn open_in_browser(&mut self, _: &OpenInBrowser, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(url) = self.detail.as_ref().and_then(|d| web_url(&d.event)) {
            cx.open_url(&url);
        }
    }

    fn quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        self.cfg.save();
        cx.quit();
    }
}

/// Split a query into whitespace-separated terms, keeping quoted phrases and
/// parenthesised groups intact (good enough to find `field:a,b` terms).
fn split_terms(query: &str) -> Vec<&str> {
    let mut terms = Vec::new();
    let mut start = None;
    let mut quote = false;
    let mut depth = 0u32;
    for (i, c) in query.char_indices() {
        match c {
            '"' => quote = !quote,
            '(' if !quote => depth += 1,
            ')' if !quote => depth = depth.saturating_sub(1),
            _ => {}
        }
        if c.is_whitespace() && !quote && depth == 0 {
            if let Some(s) = start.take() {
                terms.push(&query[s..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        terms.push(&query[s..]);
    }
    terms
}

fn term_values<'a>(term: &'a str, field: &str, exclude: bool) -> Option<Vec<&'a str>> {
    let body = if exclude { term.strip_prefix('-')? } else { term };
    let values = body.strip_prefix(field)?.strip_prefix(':')?;
    Some(values.split(',').filter(|v| !v.is_empty()).collect())
}

/// True if the query has an including `field:` term listing `value`.
fn query_has(query: &str, field: &str, value: &str) -> bool {
    split_terms(query).iter().any(|t| term_values(t, field, false).is_some_and(|v| v.contains(&value)))
}

fn toggle_in_query(query: &str, field: &str, value: &str, exclude: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut done = false;
    for term in split_terms(query) {
        match term_values(term, field, exclude) {
            Some(mut values) if !done => {
                done = true;
                match values.iter().position(|v| *v == value) {
                    Some(i) => {
                        values.remove(i);
                    }
                    None => values.push(value),
                }
                if !values.is_empty() {
                    out.push(format!("{}{field}:{}", if exclude { "-" } else { "" }, values.join(",")));
                }
            }
            _ => out.push(term.to_owned()),
        }
    }
    if !done {
        out.push(format!("{}{field}:{value}", if exclude { "-" } else { "" }));
    }
    out.join(" ")
}

impl Focusable for Vortex {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Vortex {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let square = self.cfg.config.ui.square_corners;
        if self.applied_square != Some(square) {
            self.applied_square = Some(square);
            crate::corners::set_square(window, square);
        }
        // Measure the character cell once per font/size so columns line up.
        let key = {
            let t = Theme::get(cx);
            (t.font.clone(), t.font_size)
        };
        if self.measured_font.as_ref() != Some(&key) {
            let style = window.text_style();
            let run = gpui::TextRun {
                len: 10,
                font: gpui::Font { family: key.0.clone(), ..style.font() },
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window.text_system().shape_line("0000000000".into(), key.1, &[run], None);
            let ch = line.width / 10.;
            if ch > px(1.) {
                let mut theme = Theme::get(cx).clone();
                theme.ch = ch;
                cx.set_global(theme);
            }
            self.measured_font = Some(key);
        }
        let t = Theme::get(cx).clone();

        // Sync with the store; if it is busy keep last frame's numbers.
        if let Some(view) = self.engine.store.try_view(VIEW_TIMEOUT) {
            let generation = view.generation();
            let dropped = view.dropped_front();
            if generation != self.view_generation {
                self.view_generation = generation;
                self.dropped_front = dropped;
            } else if dropped != self.dropped_front {
                // Rows fell off the front; hold a scrolled-back view in place.
                let delta = dropped - self.dropped_front;
                self.dropped_front = dropped;
                if !self.follow {
                    let handle = self.scroll.0.borrow().base_handle.clone();
                    let off = handle.offset();
                    handle.set_offset(point(off.x, (off.y + t.row * delta as f32).min(px(0.))));
                }
            }
            self.view_len = view.len();
        }
        if self.recheck_tail {
            self.recheck_tail = false;
            if self.scroll.is_scrolled_to_end() == Some(true) {
                self.follow = true;
            }
        }
        if self.follow && self.view_len > 0 {
            self.scroll.scroll_to_item(self.view_len - 1, ScrollStrategy::Bottom);
        }

        let ui = self.cfg.config.ui.clone();
        let mut main = div().flex().flex_1().min_h_0().w_full();
        if ui.sidebar {
            main = main.child(self.render_sidebar(&t, cx)).child(self.resize_handle(Pane::Sidebar, &t));
        }
        main = main.child(self.render_list(&t, cx));
        if ui.detail {
            main = main.child(self.resize_handle(Pane::Detail, &t)).child(self.render_detail(&t, cx));
        }

        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.bg)
            .text_color(t.fg)
            .font_family(t.font.clone())
            .text_size(t.font_size)
            .line_height(t.row)
            .key_context(ROOT_CONTEXT)
            .on_action(cx.listener(Self::focus_filter))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::toggle_follow))
            .on_action(cx.listener(Self::deselect))
            .on_action(cx.listener(Self::clear_buffer))
            .on_action(cx.listener(Self::toggle_settings))
            .on_action(cx.listener(Self::toggle_help))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_detail))
            .on_action(cx.listener(Self::toggle_connect))
            .on_action(cx.listener(Self::font_bigger))
            .on_action(cx.listener(Self::font_smaller))
            .on_action(cx.listener(Self::next_theme))
            .on_action(cx.listener(Self::copy_json))
            .on_action(cx.listener(Self::copy_uri))
            .on_action(cx.listener(Self::open_in_browser))
            .on_action(cx.listener(Self::quit))
            .on_drag_move(cx.listener(|this, ev: &DragMoveEvent<ResizeDrag>, window, cx| {
                let x = ev.event.position.x;
                match ev.drag(cx).0 {
                    Pane::Sidebar => this.cfg.config.ui.sidebar_width = (x / px(1.)).clamp(120., 600.),
                    Pane::Detail => {
                        this.cfg.config.ui.detail_width = ((window.viewport_size().width - x) / px(1.)).clamp(160., 1600.)
                    }
                }
                this.cfg_dirty = true;
                cx.notify();
            }))
            .child(self.render_top_bar(&t, window, cx))
            .child(self.render_filter_bar(&t, window, cx))
            .child(main)
            .child(self.render_status_bar(&t));
        if self.show_settings {
            root = root.child(self.render_settings(&t, cx));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_splitting_respects_quotes_and_groups() {
        assert_eq!(split_terms(r#"a "b c" (d OR e) f:g,h"#), ["a", "\"b c\"", "(d OR e)", "f:g,h"]);
        assert!(split_terms("   ").is_empty());
    }

    #[test]
    fn toggling_terms() {
        let t = toggle_in_query;
        assert_eq!(t("", "collection", "a.b.c", false), "collection:a.b.c");
        assert_eq!(t("hello", "collection", "a.b.c", false), "hello collection:a.b.c");
        assert_eq!(t("hello collection:a.b.c", "collection", "x.y.z", false), "hello collection:a.b.c,x.y.z");
        assert_eq!(t("hello collection:a.b.c,x.y.z lang:en", "collection", "a.b.c", false), "hello collection:x.y.z lang:en");
        assert_eq!(t("collection:a.b.c", "collection", "a.b.c", false), "");
        // Excluding terms are tracked separately.
        assert_eq!(t("collection:a.b.c", "collection", "x.y.z", true), "collection:a.b.c -collection:x.y.z");
        assert_eq!(t("-collection:x.y.z", "collection", "x.y.z", true), "");
        assert_eq!(t("\"a b\" kind:commit", "kind", "identity", false), "\"a b\" kind:commit,identity");
        // DIDs contain colons; only the field prefix is split off.
        assert_eq!(t("", "did", "did:plc:abc", false), "did:did:plc:abc");
        assert_eq!(t("did:did:plc:abc", "did", "did:plc:abc", false), "");
    }

    #[test]
    fn query_membership() {
        assert!(query_has("x collection:a.b.c,d.e.f", "collection", "d.e.f"));
        assert!(!query_has("x -collection:a.b.c", "collection", "a.b.c"));
        assert!(!query_has("\"collection:a.b.c\"", "collection", "a.b.c"));
        assert!(Filter::parse(&toggle_in_query("type:post", "did", "did:plc:7e6kocyzb77xkncplrkoojej", true)).is_ok());
    }
}
