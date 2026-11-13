//! The welcome screen: logo, the common ways in, and configuration.

use super::palette::Cmd;
use super::*;

/// 8×8 Bayer matrix for ordered dithering.
const BAYER: [[u8; 8]; 8] = [
    [0, 32, 8, 40, 2, 34, 10, 42],
    [48, 16, 56, 24, 50, 18, 58, 26],
    [12, 44, 4, 36, 14, 46, 6, 38],
    [60, 28, 52, 20, 62, 30, 54, 22],
    [3, 35, 11, 43, 1, 33, 9, 41],
    [51, 19, 59, 27, 49, 17, 57, 25],
    [15, 47, 7, 39, 13, 45, 5, 37],
    [63, 31, 55, 23, 61, 29, 53, 21],
];

/// Seconds per full turn of the cloud bands.
const TURN_SECS: f32 = 24.0;

/// Cloud density of a hurricane seen from above at normalised `(x, y)` in
/// [-1, 1] (y down): two logarithmic spiral bands, a bright eyewall, a clear
/// eye. `phase` (radians) turns the bands counter-clockwise on screen, arms
/// trailing, as a northern-hemisphere storm does.
pub(super) fn hurricane(x: f32, y: f32, phase: f32) -> f32 {
    let r = (x * x + y * y).sqrt();
    if r > 1.0 {
        return 0.0;
    }
    let theta = y.atan2(x);
    let eye = 0.11;
    if r < eye {
        return 0.0;
    }
    let arms = 0.5 + 0.5 * (2.0 * (theta + phase) - 5.5 * (r + 0.02).ln()).cos();
    let falloff = (1.0 - r).powf(0.7);
    let wall = (-((r - 0.16) / 0.05).powi(2)).exp();
    (arms.powf(1.6) * falloff * 1.15 + wall * 0.95).clamp(0.0, 1.0)
}

/// Which cells of an `n`×`n` grid are lit, and whether each is a dense cell
/// (drawn brighter): ordered dithering of [`hurricane`].
pub(super) fn logo_cells(n: usize, phase: f32) -> Vec<(usize, usize, bool)> {
    let mut out = Vec::new();
    for cy in 0..n {
        for cx in 0..n {
            let x = (cx as f32 + 0.5) / n as f32 * 2.0 - 1.0;
            let y = (cy as f32 + 0.5) / n as f32 * 2.0 - 1.0;
            let d = hurricane(x, y, phase);
            let threshold = (BAYER[cy % 8][cx % 8] as f32 + 0.5) / 64.0;
            if d > threshold {
                out.push((cx, cy, d > 0.62));
            }
        }
    }
    out
}

impl Vortex {
    pub(super) fn render_welcome(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        const N: usize = 56;
        let phase = self.started.elapsed().as_secs_f32() / TURN_SECS * std::f32::consts::TAU;
        let cells = logo_cells(N, phase);
        let (dim, bright) = (t.fg_dim, t.accent);
        let cell = px(2.);
        let logo = canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, _, window, _| {
                for &(x, y, dense) in &cells {
                    let origin = point(bounds.left() + cell * x as f32, bounds.top() + cell * y as f32);
                    window.paint_quad(fill(Bounds::new(origin, size(cell, cell)), if dense { bright } else { dim }));
                }
            },
        )
        .size(cell * N as f32)
        .flex_none();

        let connected = self.status.state != ConnState::Idle;
        let item = |id: &'static str,
                    glyph: Icon,
                    label: String,
                    keys: &'static str,
                    cmd: Cmd,
                    t: &Theme,
                    cx: &mut Context<Self>| {
            let hover = t.bg_hover;
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(t.ch)
                .h(t.row * 1.2)
                .px(t.pad)
                .rounded(t.radius)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(div().flex_none().w(t.ch * 2.).flex().items_center().child(icon(glyph, t.fg_dim, t.font_size)))
                .child(div().flex_1().text_color(t.fg).child(label))
                .child(div().flex_none().text_color(t.fg_faint).child(keys))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| this.run_cmd(cmd.clone(), window, cx)),
                )
        };
        let section = |title: &'static str, t: &Theme| {
            div()
                .flex()
                .items_center()
                .gap(t.ch)
                .pt(t.pad * 3.)
                .pb(t.pad)
                .text_color(t.fg_faint)
                .child(title)
                .child(div().flex_1().h(px(1.)).bg(t.border))
        };
        let status = if connected {
            format!("live · {}/s · {} buffered", fmt_rate(self.rates.events_per_s), fmt_count(self.buffered as u64))
        } else {
            "not connected".to_owned()
        };
        let show_on_launch = self.cfg.config.ui.welcome;

        div().flex().flex_1().min_w_0().h_full().justify_center().overflow_hidden().child(
            div()
                .id("welcome")
                .flex()
                .flex_col()
                .w(t.ch * 64.)
                .pt(t.row * 3.)
                .overflow_y_scroll()
                .child(
                    div().flex().items_center().gap(t.ch * 3.).child(logo).child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(t.font_size * 1.6)
                                    .line_height(t.row * 1.5)
                                    .text_color(t.fg)
                                    .child("vortex"),
                            )
                            .child(div().text_color(t.fg_dim).child("AT Protocol stream command center"))
                            .child(div().text_color(t.fg_faint).truncate().child(live_text(status, t))),
                    ),
                )
                .child(section("get started", t))
                .child(item(
                    "w-stream",
                    Icon::Play,
                    if connected { "watch the stream".into() } else { "connect to Jetstream".into() },
                    if connected { "esc" } else { "⌘↵" },
                    if connected { Cmd::FocusFilter } else { Cmd::ToggleConnect },
                    t,
                    cx,
                ))
                .child(item("w-replay", Icon::Replay, "replay the last 5 minutes".into(), "", Cmd::Replay(300), t, cx))
                .child(item(
                    "w-firehose",
                    Icon::Stream,
                    "watch the relay firehose".into(),
                    "",
                    Cmd::Source(SourceKind::Firehose),
                    t,
                    cx,
                ))
                .child(
                    div()
                        .id("w-inspect")
                        .flex()
                        .items_center()
                        .gap(t.ch)
                        .h(t.row * 1.2)
                        .px(t.pad)
                        .rounded(t.radius)
                        .cursor_pointer()
                        .hover({
                            let h = t.bg_hover;
                            move |s| s.bg(h)
                        })
                        .child(div().flex_none().w(t.ch * 2.).flex().items_center().child(icon(
                            Icon::Search,
                            t.fg_dim,
                            t.font_size,
                        )))
                        .child(div().flex_1().text_color(t.fg).child("inspect an account or record…"))
                        .child(div().flex_none().text_color(t.fg_faint).child("⌘P"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| this.open_palette(window, cx)),
                        ),
                )
                .child(item("w-help", Icon::Help, "filter syntax & keys".into(), "?", Cmd::Help, t, cx))
                .child(section("configure", t))
                .child(item("w-settings", Icon::Gear, "settings".into(), "⌘,", Cmd::Settings, t, cx))
                .child(
                    item(
                        "w-theme",
                        Icon::Theme,
                        format!("theme: {}", self.cfg.config.ui.theme),
                        "⌘T",
                        Cmd::NextTheme,
                        t,
                        cx,
                    )
                    .tooltip(tip(
                        "theme",
                        Some("⌘T ⌘⇧T"),
                        Some("click for the next theme, right-click for the previous one"),
                    ))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, _, window, cx| this.run_cmd(Cmd::PrevTheme, window, cx)),
                    ),
                )
                .child(item("w-config", Icon::File, "open config.toml".into(), "", Cmd::OpenConfig, t, cx))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .pt(t.pad * 4.)
                        .text_color(t.fg_faint)
                        .child(
                            div()
                                .id("w-onlaunch")
                                .cursor_pointer()
                                .child(format!("{} show on launch", if show_on_launch { "[x]" } else { "[ ]" }))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.cfg.config.ui.welcome ^= true;
                                        this.cfg_dirty = true;
                                        cx.notify();
                                    }),
                                ),
                        )
                        .child("⌘P commands · esc close"),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_has_an_eye_and_bands() {
        let cells = logo_cells(56, 0.0);
        let lit = |x: usize, y: usize| cells.iter().any(|&(cx, cy, _)| cx == x && cy == y);
        // The eye is clear, the corners are empty, and a fair share is lit.
        assert!(!lit(28, 28) && !lit(27, 27));
        assert!(!lit(0, 0) && !lit(55, 55));
        let share = cells.len() as f32 / (56.0 * 56.0);
        assert!((0.15..0.6).contains(&share), "{share}");
        assert!(cells.iter().any(|c| c.2), "some dense cells");
        // It turns: a quarter turn later the pattern differs but the eye stays.
        let later = logo_cells(56, std::f32::consts::FRAC_PI_2);
        assert_ne!(cells, later);
        assert!(!later.iter().any(|&(x, y, _)| (x, y) == (28, 28)));
    }
}
