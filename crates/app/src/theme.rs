//! Color schemes and UI metrics.
//!
//! A [`Palette`] is sixteen semantic colors; every pixel of the UI is drawn
//! from one. Built-in palettes follow well-known schemes; users can override
//! individual colors or define whole themes in `config.toml`.

use crate::config::Config;
use gpui::{px, rgb, App, Global, Hsla, Pixels, SharedString};
use std::collections::BTreeMap;

/// Names of the palette slots, in the order they are documented.
#[allow(dead_code)]
pub const COLOR_NAMES: [&str; 16] = [
    "bg", "bg_alt", "bg_hover", "bg_sel", "border", "fg", "fg_dim", "fg_faint", "accent", "red", "green", "yellow",
    "blue", "purple", "aqua", "orange",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Window background.
    pub bg: u32,
    /// Bars, headers, panels.
    pub bg_alt: u32,
    pub bg_hover: u32,
    /// Selected row.
    pub bg_sel: u32,
    pub border: u32,
    pub fg: u32,
    /// Secondary text.
    pub fg_dim: u32,
    /// Placeholders, punctuation.
    pub fg_faint: u32,
    /// Focus and active toggles.
    pub accent: u32,
    pub red: u32,
    pub green: u32,
    pub yellow: u32,
    pub blue: u32,
    pub purple: u32,
    pub aqua: u32,
    pub orange: u32,
}

impl Palette {
    fn slot(&mut self, name: &str) -> Option<&mut u32> {
        Some(match name {
            "bg" => &mut self.bg,
            "bg_alt" => &mut self.bg_alt,
            "bg_hover" => &mut self.bg_hover,
            "bg_sel" => &mut self.bg_sel,
            "border" => &mut self.border,
            "fg" => &mut self.fg,
            "fg_dim" => &mut self.fg_dim,
            "fg_faint" => &mut self.fg_faint,
            "accent" => &mut self.accent,
            "red" => &mut self.red,
            "green" => &mut self.green,
            "yellow" => &mut self.yellow,
            "blue" => &mut self.blue,
            "purple" => &mut self.purple,
            "aqua" => &mut self.aqua,
            "orange" => &mut self.orange,
            _ => return None,
        })
    }

    /// Apply `name = "#rrggbb"` overrides; returns problems found.
    pub fn apply(&mut self, overrides: &BTreeMap<String, String>) -> Vec<String> {
        let mut problems = Vec::new();
        for (name, value) in overrides {
            if name == "base" {
                continue;
            }
            match (self.slot(name), parse_hex(value)) {
                (Some(slot), Some(c)) => *slot = c,
                (None, _) => problems.push(format!("unknown color '{name}'")),
                (_, None) => problems.push(format!("bad color '{value}' for '{name}'")),
            }
        }
        problems
    }
}

/// Parse `#rgb`, `#rrggbb` or the same without `#`.
pub fn parse_hex(s: &str) -> Option<u32> {
    let h = s.trim().trim_start_matches('#');
    match h.len() {
        6 => u32::from_str_radix(h, 16).ok(),
        3 => {
            let v = u32::from_str_radix(h, 16).ok()?;
            let (r, g, b) = ((v >> 8) & 15, (v >> 4) & 15, v & 15);
            Some((r * 17) << 16 | (g * 17) << 8 | b * 17)
        }
        _ => None,
    }
}

macro_rules! palette {
    ($($f:ident: $v:expr),* $(,)?) => { Palette { $($f: $v),* } };
}

/// Built-in schemes. The first is the default.
pub const BUILTIN: &[(&str, Palette)] = &[
    (
        "kanagawa",
        palette!(bg: 0x1f1f28, bg_alt: 0x16161d, bg_hover: 0x2a2a37, bg_sel: 0x2d4f67, border: 0x363646,
                 fg: 0xdcd7ba, fg_dim: 0xc8c093, fg_faint: 0x727169, accent: 0x7e9cd8,
                 red: 0xe46876, green: 0x98bb6c, yellow: 0xe6c384, blue: 0x7e9cd8, purple: 0x957fb8,
                 aqua: 0x7aa89f, orange: 0xffa066),
    ),
    (
        "gruvbox-dark",
        palette!(bg: 0x1d2021, bg_alt: 0x282828, bg_hover: 0x32302f, bg_sel: 0x45403d, border: 0x3c3836,
                 fg: 0xebdbb2, fg_dim: 0xa89984, fg_faint: 0x7c6f64, accent: 0xfabd2f,
                 red: 0xfb4934, green: 0xb8bb26, yellow: 0xfabd2f, blue: 0x83a598, purple: 0xd3869b,
                 aqua: 0x8ec07c, orange: 0xfe8019),
    ),
    (
        "gruvbox-light",
        palette!(bg: 0xfbf1c7, bg_alt: 0xf2e5bc, bg_hover: 0xebdbb2, bg_sel: 0xd5c4a1, border: 0xd5c4a1,
                 fg: 0x3c3836, fg_dim: 0x665c54, fg_faint: 0x928374, accent: 0xb57614,
                 red: 0x9d0006, green: 0x79740e, yellow: 0xb57614, blue: 0x076678, purple: 0x8f3f71,
                 aqua: 0x427b58, orange: 0xaf3a03),
    ),
    (
        "catppuccin-mocha",
        palette!(bg: 0x1e1e2e, bg_alt: 0x181825, bg_hover: 0x313244, bg_sel: 0x45475a, border: 0x313244,
                 fg: 0xcdd6f4, fg_dim: 0xa6adc8, fg_faint: 0x6c7086, accent: 0xcba6f7,
                 red: 0xf38ba8, green: 0xa6e3a1, yellow: 0xf9e2af, blue: 0x89b4fa, purple: 0xcba6f7,
                 aqua: 0x94e2d5, orange: 0xfab387),
    ),
    (
        "tokyo-night",
        palette!(bg: 0x1a1b26, bg_alt: 0x16161e, bg_hover: 0x292e42, bg_sel: 0x33467c, border: 0x292e42,
                 fg: 0xc0caf5, fg_dim: 0xa9b1d6, fg_faint: 0x565f89, accent: 0x7aa2f7,
                 red: 0xf7768e, green: 0x9ece6a, yellow: 0xe0af68, blue: 0x7aa2f7, purple: 0xbb9af7,
                 aqua: 0x7dcfff, orange: 0xff9e64),
    ),
    (
        "nord",
        palette!(bg: 0x2e3440, bg_alt: 0x3b4252, bg_hover: 0x434c5e, bg_sel: 0x4c566a, border: 0x434c5e,
                 fg: 0xd8dee9, fg_dim: 0xb4bccb, fg_faint: 0x7b88a1, accent: 0x88c0d0,
                 red: 0xbf616a, green: 0xa3be8c, yellow: 0xebcb8b, blue: 0x81a1c1, purple: 0xb48ead,
                 aqua: 0x8fbcbb, orange: 0xd08770),
    ),
    (
        "dracula",
        palette!(bg: 0x282a36, bg_alt: 0x21222c, bg_hover: 0x343746, bg_sel: 0x44475a, border: 0x44475a,
                 fg: 0xf8f8f2, fg_dim: 0xbfc0d0, fg_faint: 0x6272a4, accent: 0xbd93f9,
                 red: 0xff5555, green: 0x50fa7b, yellow: 0xf1fa8c, blue: 0x8be9fd, purple: 0xbd93f9,
                 aqua: 0x8be9fd, orange: 0xffb86c),
    ),
    (
        "solarized-dark",
        palette!(bg: 0x002b36, bg_alt: 0x073642, bg_hover: 0x0a4352, bg_sel: 0x14505f, border: 0x0d4a59,
                 fg: 0x93a1a1, fg_dim: 0x839496, fg_faint: 0x586e75, accent: 0x268bd2,
                 red: 0xdc322f, green: 0x859900, yellow: 0xb58900, blue: 0x268bd2, purple: 0x6c71c4,
                 aqua: 0x2aa198, orange: 0xcb4b16),
    ),
    (
        "one-dark",
        palette!(bg: 0x282c34, bg_alt: 0x21252b, bg_hover: 0x2c313a, bg_sel: 0x3e4451, border: 0x3b4048,
                 fg: 0xabb2bf, fg_dim: 0x8b92a0, fg_faint: 0x5c6370, accent: 0x61afef,
                 red: 0xe06c75, green: 0x98c379, yellow: 0xe5c07b, blue: 0x61afef, purple: 0xc678dd,
                 aqua: 0x56b6c2, orange: 0xd19a66),
    ),
    (
        // Inspired by the Bloomberg Terminal: amber on black, blue selection.
        "bloomberg",
        palette!(bg: 0x000000, bg_alt: 0x0d0d0d, bg_hover: 0x1c1c1c, bg_sel: 0x0b3a7a, border: 0x2b2b2b,
                 fg: 0xffa028, fg_dim: 0xc27a1e, fg_faint: 0x7a7a7a, accent: 0xffd400,
                 red: 0xff433d, green: 0x3ddc84, yellow: 0xffd400, blue: 0x4d9bff, purple: 0xc77dff,
                 aqua: 0x4af6c3, orange: 0xfb8b1e),
    ),
];

pub const DEFAULT_THEME: &str = BUILTIN[0].0;

fn builtin(name: &str) -> Option<Palette> {
    BUILTIN.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
}

/// Every selectable theme name: built-ins, then user-defined.
pub fn theme_names(cfg: &Config) -> Vec<String> {
    let mut names: Vec<String> = BUILTIN.iter().map(|(n, _)| n.to_string()).collect();
    names.extend(cfg.themes.keys().filter(|k| builtin(k).is_none()).cloned());
    names
}

/// Resolve the configured palette: theme (built-in or user, optionally based
/// on another) with `[colors]` overrides on top.
pub fn resolve_palette(cfg: &Config) -> (Palette, Vec<String>) {
    let mut problems = Vec::new();
    let name = cfg.ui.theme.as_str();
    let mut palette = match (cfg.themes.get(name), builtin(name)) {
        (Some(user), built) => {
            let base = user.get("base").and_then(|b| builtin(b)).or(built).unwrap_or(BUILTIN[0].1);
            let mut p = base;
            problems.extend(p.apply(user).into_iter().map(|e| format!("theme {name}: {e}")));
            p
        }
        (None, Some(p)) => p,
        (None, None) => {
            problems.push(format!("unknown theme '{name}', using {DEFAULT_THEME}"));
            BUILTIN[0].1
        }
    };
    problems.extend(palette.apply(&cfg.colors).into_iter().map(|e| format!("colors: {e}")));
    (palette, problems)
}

/// The resolved look, installed as a GPUI global.
#[derive(Debug, Clone)]
pub struct Theme {
    pub bg: Hsla,
    pub bg_alt: Hsla,
    pub bg_hover: Hsla,
    pub bg_sel: Hsla,
    pub border: Hsla,
    pub fg: Hsla,
    pub fg_dim: Hsla,
    pub fg_faint: Hsla,
    pub accent: Hsla,
    pub red: Hsla,
    pub green: Hsla,
    pub yellow: Hsla,
    pub blue: Hsla,
    pub purple: Hsla,
    pub aqua: Hsla,
    pub orange: Hsla,
    pub font: SharedString,
    pub font_size: Pixels,
    /// Height of one text row.
    pub row: Pixels,
    /// Base padding unit.
    pub pad: Pixels,
    pub radius: Pixels,
    /// Advance width of one character cell (monospace assumption).
    pub ch: Pixels,
}

impl Global for Theme {}

impl Theme {
    pub fn build(cfg: &Config, palette: Palette, font: SharedString) -> Theme {
        let c = |v: u32| Hsla::from(rgb(v));
        let size = cfg.ui.font_size.clamp(7.0, 40.0);
        let row = (size * cfg.ui.line_height.clamp(1.0, 2.5)).round();
        Theme {
            bg: c(palette.bg),
            bg_alt: c(palette.bg_alt),
            bg_hover: c(palette.bg_hover),
            bg_sel: c(palette.bg_sel),
            border: c(palette.border),
            fg: c(palette.fg),
            fg_dim: c(palette.fg_dim),
            fg_faint: c(palette.fg_faint),
            accent: c(palette.accent),
            red: c(palette.red),
            green: c(palette.green),
            yellow: c(palette.yellow),
            blue: c(palette.blue),
            purple: c(palette.purple),
            aqua: c(palette.aqua),
            orange: c(palette.orange),
            font,
            font_size: px(size),
            row: px(row),
            pad: px(cfg.ui.padding.clamp(0.0, 16.0)),
            radius: px(cfg.ui.radius.clamp(0.0, 12.0)),
            // Refined after the first text measurement.
            ch: px(size * 0.6),
        }
    }

    pub fn get(cx: &App) -> &Theme {
        cx.global::<Theme>()
    }

    /// A stable hue for a string, used to tint collections so that the eye
    /// can group rows without reading them.
    pub fn hue(&self, s: &str) -> Hsla {
        let hues = [self.blue, self.aqua, self.green, self.yellow, self.orange, self.purple, self.red];
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for b in s.bytes() {
            h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
        hues[(h % hues.len() as u64) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("#1d2021"), Some(0x1d2021));
        assert_eq!(parse_hex("FABD2F"), Some(0xfabd2f));
        assert_eq!(parse_hex("#fa0"), Some(0xffaa00));
        assert_eq!(parse_hex("#12345"), None);
        assert_eq!(parse_hex("nothex"), None);
    }

    #[test]
    fn every_builtin_names_every_slot() {
        for (name, p) in BUILTIN {
            let mut p = *p;
            for slot in COLOR_NAMES {
                assert!(p.slot(slot).is_some(), "{name}.{slot}");
            }
            assert_ne!(p.bg, p.fg, "{name}");
        }
    }

    #[test]
    fn resolution_layers_overrides() {
        let mut cfg = Config::default();
        assert_eq!(resolve_palette(&cfg).0, BUILTIN[0].1);

        cfg.colors.insert("accent".into(), "#ff00ff".into());
        cfg.colors.insert("nope".into(), "#000000".into());
        cfg.colors.insert("fg".into(), "zzz".into());
        let (p, problems) = resolve_palette(&cfg);
        assert_eq!(p.accent, 0xff00ff);
        assert_eq!(p.fg, BUILTIN[0].1.fg);
        assert_eq!(problems.len(), 2);

        cfg.colors.clear();
        cfg.ui.theme = "mine".into();
        cfg.themes.insert(
            "mine".into(),
            [("base".to_string(), "nord".to_string()), ("bg".to_string(), "#000".to_string())].into(),
        );
        let (p, problems) = resolve_palette(&cfg);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(p.bg, 0);
        assert_eq!(p.fg, builtin("nord").unwrap().fg);
        assert!(theme_names(&cfg).contains(&"mine".to_string()));

        cfg.ui.theme = "missing".into();
        let (p, problems) = resolve_palette(&cfg);
        assert_eq!(p, BUILTIN[0].1);
        assert_eq!(problems.len(), 1);
    }
}
