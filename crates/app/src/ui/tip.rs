//! Hover tooltips: a title, an optional key hint, and an optional line of
//! explanation. Used to explain terse labels without spending screen space.

use super::icons::{icon, Icon};
use crate::theme::Theme;
use gpui::{div, prelude::*, AnyView, App, Context, SharedString, Window};

pub struct Tip {
    title: SharedString,
    keys: Option<SharedString>,
    body: Option<SharedString>,
    legend: Vec<(Icon, SharedString)>,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::get(cx);
        div()
            .flex()
            .flex_col()
            .max_w(t.ch * 56.)
            .px(t.pad * 1.5)
            .py(t.pad)
            .gap(px1())
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
                    .child(div().text_color(t.fg).child(self.title.clone()))
                    .children(self.keys.clone().map(|k| div().text_color(t.yellow).child(k))),
            )
            .children(self.body.clone().map(|b| div().text_color(t.fg_dim).child(b)))
            .children(self.legend.iter().map(|(i, label)| {
                div()
                    .flex()
                    .items_center()
                    .gap(t.ch)
                    .child(icon(*i, t.fg, t.font_size))
                    .child(div().text_color(t.fg_dim).child(label.clone()))
            }))
    }
}

fn px1() -> gpui::Pixels {
    gpui::px(1.)
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
