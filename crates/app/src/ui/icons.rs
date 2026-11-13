//! A small pixel icon set: square bitmaps (7×7, or finer where a shape needs
//! it) with one-cell strokes, drawn as quads snapped to whole device pixels so
//! they stay crisp at text size.

use gpui::{canvas, fill, point, prelude::*, px, size, Bounds, Hsla, Pixels};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Dot,
    Play,
    Replay,
    Stream,
    Search,
    Help,
    Gear,
    Theme,
    File,
    Tail,
    Reply,
    Quote,
    Image,
    Video,
    Link,
    Heart,
    Repost,
    Check,
    Left,
    Right,
    Up,
    Close,
    Warn,
}

impl Icon {
    pub const ALL: [Icon; 23] = [
        Icon::Dot,
        Icon::Play,
        Icon::Replay,
        Icon::Stream,
        Icon::Search,
        Icon::Help,
        Icon::Gear,
        Icon::Theme,
        Icon::File,
        Icon::Tail,
        Icon::Reply,
        Icon::Quote,
        Icon::Image,
        Icon::Video,
        Icon::Link,
        Icon::Heart,
        Icon::Repost,
        Icon::Check,
        Icon::Left,
        Icon::Right,
        Icon::Up,
        Icon::Close,
        Icon::Warn,
    ];

    /// A square grid of cells; `#` is lit. Most icons are 7×7.
    fn rows(self) -> &'static [&'static str] {
        match self {
            Icon::Dot => &[".......", ".......", "..###..", "..###..", "..###..", ".......", "......."],
            Icon::Play => &["#......", "###....", "#####..", "#######", "#####..", "###....", "#......"],
            Icon::Replay => &["..#....", ".######", "..#...#", "#.....#", "#.....#", "#######", "......."],
            Icon::Stream => &[".......", "######.", ".......", ".######", ".......", "####...", "......."],
            Icon::Search => &["#####..", "#...#..", "#...#..", "#...#..", "#####..", "....##.", ".....##"],
            Icon::Help => &[".#####.", ".#...#.", ".....#.", "...###.", "...#...", ".......", "...#..."],
            Icon::Gear => &[
                "....###....",
                ".##.###.##.",
                ".#########.",
                "..#######..",
                "####...####",
                "####...####",
                "####...####",
                "..#######..",
                ".#########.",
                ".##.###.##.",
                "....###....",
            ],
            Icon::Theme => &["#######", "#..####", "#..####", "#..####", "#..####", "#..####", "#######"],
            Icon::File => &["####...", "#..##..", "#...##.", "#.##.#.", "#....#.", "#.##.#.", "######."],
            Icon::Tail => &["...#...", "...#...", "#######", ".#####.", "..###..", "...#...", "#######"],
            Icon::Reply => &["..#....", ".##....", "######.", ".##..#.", "..#..#.", ".....#.", "......."],
            Icon::Quote => &[".......", "##..##.", "##..##.", ".#...#.", "#...#..", ".......", "......."],
            Icon::Image => &[".......", "#######", "#.....#", "#..#..#", "#.###.#", "#######", "......."],
            Icon::Video => &[".......", "#######", "#.#...#", "#.###.#", "#.#...#", "#######", "......."],
            Icon::Link => &["####...", "#..#...", "#.#####", "####..#", "...#..#", "...####", "......."],
            Icon::Heart => &[".......", ".##.##.", "#######", "#######", ".#####.", "..###..", "...#..."],
            Icon::Repost => &["....#..", "######.", "#...#..", "#.....#", "..#...#", ".######", "..#...."],
            Icon::Check => &[".......", ".....##", "....##.", "#..##..", "####...", ".##....", "......."],
            Icon::Left => &[".......", "..#....", ".##....", "#######", ".##....", "..#....", "......."],
            Icon::Right => &[".......", "....#..", "....##.", "#######", "....##.", "....#..", "......."],
            Icon::Up => &["...#...", "..###..", ".#####.", "...#...", "...#...", "...#...", "...#..."],
            Icon::Close => &[".......", "##...##", ".##.##.", "..###..", ".##.##.", "##...##", "......."],
            Icon::Warn => &["...#...", "..###..", "..#.#..", ".##.##.", ".##.##.", "#######", "###.###"],
        }
    }

    /// Lit cells merged into horizontal runs: `(row, first column, length)`.
    fn runs(self) -> Vec<(u8, u8, u8)> {
        let mut out = Vec::new();
        for (y, row) in self.rows().iter().enumerate() {
            let b = row.as_bytes();
            let mut x = 0;
            while x < b.len() {
                if b[x] == b'#' {
                    let start = x;
                    while x < b.len() && b[x] == b'#' {
                        x += 1;
                    }
                    out.push((y as u8, start as u8, (x - start) as u8));
                } else {
                    x += 1;
                }
            }
        }
        out
    }
}

/// An icon in a `size`×`size` box. Cells are whole device pixels, so the
/// drawn glyph may be slightly smaller than the box; it is centred.
pub fn icon(kind: Icon, color: Hsla, size_px: Pixels) -> impl IntoElement {
    let runs = kind.runs();
    let n = kind.rows().len();
    canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, _, window, _| {
            let scale = window.scale_factor();
            let box_dev = (bounds.size.width.min(bounds.size.height) / px(1.)) * scale;
            let cell_dev = (box_dev / n as f32).floor().max(1.0);
            let cell = px(cell_dev / scale);
            let glyph = cell * n as f32;
            let snap = |v: Pixels| px(((v / px(1.)) * scale).round() / scale);
            let ox = snap(bounds.left() + (bounds.size.width - glyph) / 2.0);
            let oy = snap(bounds.top() + (bounds.size.height - glyph) / 2.0);
            for &(row, col, len) in &runs {
                let origin = point(ox + cell * col as f32, oy + cell * row as f32);
                window.paint_quad(fill(Bounds::new(origin, size(cell * len as f32, cell)), color));
            }
        },
    )
    .size(size_px)
    .flex_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmaps_are_square_and_not_empty() {
        for i in Icon::ALL {
            let rows = i.rows();
            for r in rows {
                assert_eq!(r.len(), rows.len(), "{i:?}");
                assert!(r.bytes().all(|b| b == b'#' || b == b'.'), "{i:?}");
            }
            assert!(!i.runs().is_empty(), "{i:?}");
        }
    }
}
