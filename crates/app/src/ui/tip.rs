//! Hover tooltips: a title, an optional key hint, an optional explanation,
//! and optionally an icon legend. Used to explain terse labels without
//! spending screen space.
//!
//! GPUI lays tooltips out at their minimum content size, which squeezes text
//! to its longest word (or lets it overflow the box). So each tooltip gets an
//! explicit width computed from its text, capped so long explanations wrap.

use super::icons::{icon, Icon};
use super::text::live_text;
use crate::theme::Theme;
use gpui::{div, prelude::*, px, AnyView, App, Context, Pixels, SharedString, Window};

/// Widest a tooltip gets, in character cells; longer text wraps.
const MAX_COLS: f32 = 56.0;

pub struct Tip {
    title: SharedString,
    keys: Option<SharedString>,
    body: Option<SharedString>,
    legend: Vec<(Icon, SharedString)>,
}

impl Tip {
    /// Content width in character cells: the widest of the title row, the
    /// body (up to the cap) and the legend rows.
    fn cols(&self) -> f32 {
        let n = |s: &str| s.chars().count() as f32;
        let head = n(&self.title) + self.keys.as_deref().map_or(0.0, |k| n(k) + 2.0);
        let body = self.body.as_deref().map_or(0.0, n);
        let legend = self.legend.iter().map(|(_, l)| n(l) + 2.0).fold(0.0, f32::max);
        head.max(legend).max(body.min(MAX_COLS))
    }

    fn width(&self, t: &Theme) -> Pixels {
        // Content, horizontal padding, border, and a cell of slack for
        // rounding in glyph advances.
        t.ch * (self.cols() + 1.0) + t.pad * 3.0 + px(2.)
    }
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::get(cx);
        div()
            .flex()
            .flex_col()
            .w(self.width(t))
            .px(t.pad * 1.5)
            .py(t.pad)
            .gap(px(1.))
            .bg(t.bg_alt)
            .border_1()
            .border_color(t.border)
            .rounded(t.radius)
            .shadow_md()
            .font_family(t.font.clone())
            .text_size(t.font_size)
            .line_height(t.row)
            .child(
                div()
                    .flex()
                    .gap(t.ch * 2.)
                    .justify_between()
                    .whitespace_nowrap()
                    .child(div().text_color(t.fg).child(live_text(self.title.clone(), t)))
                    .children(self.keys.clone().map(|k| div().flex_none().text_color(t.yellow).child(k))),
            )
            .children(self.body.clone().map(|b| div().w_full().text_color(t.fg_dim).child(live_text(b, t))))
            .children(self.legend.iter().map(|(i, label)| {
                div()
                    .flex()
                    .items_center()
                    .gap(t.ch)
                    .whitespace_nowrap()
                    .child(icon(*i, t.fg, t.font_size))
                    .child(div().text_color(t.fg_dim).child(label.clone()))
            }))
    }
}

/// A tooltip builder for `.tooltip(...)`.
pub fn tip(
    title: impl Into<SharedString>,
    keys: Option<&'static str>,
    body: Option<&str>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let title = title.into();
    let keys: Option<SharedString> = keys.map(Into::into);
    let body: Option<SharedString> = body.map(|b| SharedString::from(b.to_owned()));
    move |_, cx| {
        let tip = Tip { title: title.clone(), keys: keys.clone(), body: body.clone(), legend: Vec::new() };
        cx.new(|_| tip).into()
    }
}

/// A tooltip that explains a set of icons, one per line.
pub fn tip_legend(
    title: &'static str,
    items: &'static [(Icon, &'static str)],
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_, cx| {
        let legend = items.iter().map(|(i, l)| (*i, SharedString::from(*l))).collect();
        cx.new(|_| Tip { title: title.into(), keys: None, body: None, legend }).into()
    }
}

/// Every tooltip variant at once, for checking layout without hovering
/// (debug builds only; see the `tips` debug hook).
#[cfg(debug_assertions)]
pub fn sample(title: &'static str, keys: Option<&'static str>, body: Option<&'static str>) -> Tip {
    Tip { title: title.into(), keys: keys.map(Into::into), body: body.map(Into::into), legend: Vec::new() }
}

#[cfg(debug_assertions)]
pub fn sample_legend(title: &'static str, items: &'static [(Icon, &'static str)]) -> Tip {
    Tip { title: title.into(), keys: None, body: None, legend: items.iter().map(|(i, l)| (*i, (*l).into())).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_follows_the_text() {
        let short = sample("tail", Some("space"), None);
        assert_eq!(short.cols(), 4.0 + 5.0 + 2.0);
        let long =
            sample("srv", None, Some("this part of the query is sent to the server; other events never arrive at all"));
        assert_eq!(long.cols(), MAX_COLS, "long bodies wrap at the cap");
        let mid = sample("lag", None, Some("now minus the newest event's server time"));
        assert_eq!(mid.cols(), 40.0);
    }
}
