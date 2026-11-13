//! Command palette (⌘P): every command by name, and an address bar for the
//! inspector (paste a handle, DID, at:// URI, bsky.app or pdsls link).

use super::*;
use crate::atp::{parse_target, AtTarget};

#[derive(Clone)]
pub(super) enum Cmd {
    Inspect(AtTarget),
    ToggleConnect,
    Source(SourceKind),
    Replay(u64),
    ClearBuffer,
    FocusFilter,
    SavedFilter(String),
    ClearFilter,
    Sidebar,
    Detail,
    Settings,
    Help,
    NextTheme,
    PrevTheme,
    Theme(String),
    OpenConfig,
    Welcome,
    FpsMeter,
    ServerFilter,
    Compression,
    InspectSelected,
    Quit,
}

impl Cmd {
    /// Whether running this from the welcome screen should reveal the event
    /// view. Overlays (settings) and preferences leave the welcome screen up.
    pub(super) fn leaves_welcome(&self) -> bool {
        !matches!(
            self,
            Cmd::Settings
                | Cmd::NextTheme
                | Cmd::PrevTheme
                | Cmd::Theme(_)
                | Cmd::OpenConfig
                | Cmd::Welcome
                | Cmd::FpsMeter
                | Cmd::ServerFilter
                | Cmd::Compression
                | Cmd::Quit
        )
    }
}

pub(super) struct Entry {
    pub label: String,
    pub keys: &'static str,
    pub cmd: Cmd,
}

impl Vortex {
    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = true;
        self.palette_ix = 0;
        self.show_settings = false;
        self.palette_input.update(cx, |i, cx| i.set_text("", cx));
        window.focus(&self.palette_input.focus_handle(cx), cx);
        cx.notify();
    }

    pub(super) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Entries matching the palette text, best first.
    pub(super) fn palette_entries(&self, cx: &App) -> Vec<Entry> {
        let query = self.palette_input.read(cx).text().trim().to_owned();
        let mut out = Vec::new();
        if let Some(target) = parse_target(&query) {
            out.push(Entry { label: format!("inspect {}", target.uri()), keys: "↵", cmd: Cmd::Inspect(target) });
        }
        let connected = self.status.state != ConnState::Idle;
        let mut all = vec![
            Entry {
                label: if connected { "disconnect".into() } else { "connect".into() },
                keys: "⌘↵",
                cmd: Cmd::ToggleConnect,
            },
            Entry { label: "source: jetstream".into(), keys: "", cmd: Cmd::Source(SourceKind::JetstreamV2) },
            Entry {
                label: "source: jetstream v1 (legacy)".into(),
                keys: "",
                cmd: Cmd::Source(SourceKind::JetstreamV1),
            },
            Entry { label: "source: relay firehose".into(), keys: "", cmd: Cmd::Source(SourceKind::Firehose) },
        ];
        for (label, secs) in REWINDS {
            let label = if secs == 0 { "live: start from now".to_owned() } else { format!("replay the last {label}") };
            all.push(Entry { label, keys: "", cmd: Cmd::Replay(secs) });
        }
        all.extend([
            Entry { label: "inspect selected event".into(), keys: "i", cmd: Cmd::InspectSelected },
            Entry { label: "focus filter".into(), keys: "/", cmd: Cmd::FocusFilter },
            Entry { label: "clear filter".into(), keys: "", cmd: Cmd::ClearFilter },
            Entry { label: "clear buffer".into(), keys: "⌘K", cmd: Cmd::ClearBuffer },
            Entry { label: "toggle sidebar".into(), keys: "⌘B", cmd: Cmd::Sidebar },
            Entry { label: "toggle detail pane".into(), keys: "⌘I", cmd: Cmd::Detail },
            Entry { label: "toggle server filter (srv)".into(), keys: "", cmd: Cmd::ServerFilter },
            Entry { label: "toggle compression (zstd)".into(), keys: "", cmd: Cmd::Compression },
            Entry { label: "toggle fps meter".into(), keys: "", cmd: Cmd::FpsMeter },
            Entry { label: "settings".into(), keys: "⌘,", cmd: Cmd::Settings },
            Entry { label: "filter syntax & keys".into(), keys: "?", cmd: Cmd::Help },
            Entry { label: "next theme".into(), keys: "⌘T", cmd: Cmd::NextTheme },
            Entry { label: "previous theme".into(), keys: "⌘⇧T", cmd: Cmd::PrevTheme },
            Entry { label: "open config.toml".into(), keys: "", cmd: Cmd::OpenConfig },
            Entry { label: "welcome".into(), keys: "", cmd: Cmd::Welcome },
            Entry { label: "quit".into(), keys: "⌘Q", cmd: Cmd::Quit },
        ]);
        for s in &self.cfg.config.filter.saved {
            all.push(Entry {
                label: format!("saved filter: {}", s.name),
                keys: "",
                cmd: Cmd::SavedFilter(s.query.clone()),
            });
        }
        for name in theme_names(&self.cfg.config) {
            all.push(Entry { label: format!("theme: {name}"), keys: "", cmd: Cmd::Theme(name) });
        }
        let words: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_owned).collect();
        out.extend(all.into_iter().filter(|e| {
            let l = e.label.to_lowercase();
            words.iter().all(|w| l.contains(w.as_str()))
        }));
        out
    }

    pub(super) fn run_palette(&mut self, ix: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self.palette_entries(cx);
        let Some(e) = entries.into_iter().nth(ix.unwrap_or(self.palette_ix)) else { return };
        self.close_palette(window, cx);
        self.run_cmd(e.cmd, window, cx);
    }

    pub(super) fn run_cmd(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        if cmd.leaves_welcome() {
            self.show_welcome = false;
        }
        match cmd {
            Cmd::Inspect(t) => self.inspect(t, cx),
            Cmd::ToggleConnect => self.toggle_connect(&ToggleConnect, window, cx),
            Cmd::Source(k) => self.switch_source(k, cx),
            Cmd::Replay(secs) => {
                self.rewind_secs = secs;
                self.reconnect_fresh(cx);
            }
            Cmd::ClearBuffer => self.clear_buffer(&ClearBuffer, window, cx),
            Cmd::FocusFilter => self.focus_filter(&FocusFilter, window, cx),
            Cmd::SavedFilter(q) => self.set_query(&q, cx),
            Cmd::ClearFilter => self.set_query("", cx),
            Cmd::Sidebar => self.toggle_sidebar(&ToggleSidebar, window, cx),
            Cmd::Detail => self.toggle_detail(&ToggleDetail, window, cx),
            Cmd::Settings => self.toggle_settings(&ToggleSettings, window, cx),
            Cmd::Help => self.toggle_help(&ToggleHelp, window, cx),
            Cmd::NextTheme => self.next_theme(&NextTheme, window, cx),
            Cmd::PrevTheme => self.prev_theme(&PrevTheme, window, cx),
            Cmd::Theme(name) => {
                self.cfg.config.ui.theme = name;
                self.config_changed(cx);
            }
            Cmd::OpenConfig => {
                self.cfg.save();
                cx.open_with_system(&self.cfg.path.clone());
            }
            Cmd::Welcome => self.show_welcome = true,
            Cmd::FpsMeter => {
                self.cfg.config.ui.fps_meter ^= true;
                self.fps.reset();
                self.config_changed(cx);
            }
            Cmd::ServerFilter => {
                self.cfg.config.source.pushdown ^= true;
                self.config_changed(cx);
            }
            Cmd::Compression => {
                self.cfg.config.source.compress ^= true;
                self.cfg_dirty = true;
                self.connect(cx);
            }
            Cmd::InspectSelected => self.inspect_selected(&InspectSelected, window, cx),
            Cmd::Quit => self.quit(&Quit, window, cx),
        }
        cx.notify();
    }

    pub(super) fn switch_source(&mut self, k: SourceKind, cx: &mut Context<Self>) {
        if self.cfg.config.source.kind() != k {
            self.cfg.config.source.kind = k.label().into();
            self.cfg.config.source.hosts.clear();
            self.cfg_dirty = true;
        }
        self.reconnect_fresh(cx);
    }

    pub(super) fn render_palette(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.palette_entries(cx);
        let selected = self.palette_ix.min(entries.len().saturating_sub(1));
        let mut list = div().id("palette-list").flex().flex_col().max_h(t.row * 14.).overflow_y_scroll();
        for (i, e) in entries.iter().enumerate().take(200) {
            let hover = t.bg_hover;
            let is_inspect = matches!(e.cmd, Cmd::Inspect(_));
            list = list.child(
                div()
                    .id(("pal", i))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(t.ch * 2.)
                    .h(t.row)
                    .px(t.pad * 2.)
                    .cursor_pointer()
                    .when(i == selected, |d| d.bg(t.bg_sel))
                    .when(i != selected, |d| d.hover(move |s| s.bg(hover)))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(if is_inspect { t.blue } else { t.fg })
                            .child(live_text(e.label.clone(), t)),
                    )
                    .child(div().flex_none().text_color(t.yellow).child(e.keys))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| this.run_palette(Some(i), window, cx)),
                    ),
            );
        }
        if entries.is_empty() {
            list = list.child(
                div()
                    .px(t.pad * 2.)
                    .h(t.row)
                    .text_color(t.fg_faint)
                    .child("no match: try a handle, DID, at:// URI or a command"),
            );
        }
        deferred(
            // Deferred elements paint after the root's text style is popped;
            // without restating it the input lays out with one line height
            // and paints with another, so text and cursor drift apart.
            div()
                .font_family(t.font.clone())
                .text_size(t.font_size)
                .line_height(t.row)
                .absolute()
                .top(px(TITLEBAR_HEIGHT) + t.row * 2.)
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .w(t.ch * 76.)
                        .flex()
                        .flex_col()
                        .bg(t.bg_alt)
                        .border_1()
                        .border_color(t.border)
                        .rounded(t.radius)
                        .shadow_lg()
                        .occlude()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(t.pad)
                                .px(t.pad * 2.)
                                .py(t.pad)
                                .border_b_1()
                                .border_color(t.border)
                                .child(div().flex_none().text_color(t.accent).child("›"))
                                .child(self.palette_input.clone()),
                        )
                        .child(list.py(t.pad * 0.5))
                        .child(
                            div()
                                .px(t.pad * 2.)
                                .py(t.pad * 0.5)
                                .border_t_1()
                                .border_color(t.border)
                                .text_color(t.fg_faint)
                                .child("↵ run · ↑↓ choose · esc close · paste a handle or link to inspect"),
                        ),
                ),
        )
        .with_priority(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlays_and_preferences_keep_the_welcome_screen() {
        for cmd in
            [Cmd::Settings, Cmd::NextTheme, Cmd::PrevTheme, Cmd::Theme("nord".into()), Cmd::OpenConfig, Cmd::FpsMeter]
        {
            assert!(!cmd.leaves_welcome());
        }
        for cmd in
            [Cmd::Help, Cmd::Source(SourceKind::Firehose), Cmd::Replay(300), Cmd::ToggleConnect, Cmd::FocusFilter]
        {
            assert!(cmd.leaves_welcome());
        }
    }
}
