//! Small text helpers shared across views.

use crate::theme::Theme;
use gpui::{HighlightStyle, SharedString, StyledText};
use std::ops::Range;

/// Byte ranges of the whole word "live" (any case) in `s`.
pub fn live_ranges(s: &str) -> Vec<Range<usize>> {
    let lower = s.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = lower[from..].find("live") {
        let start = from + i;
        let end = start + 4;
        let before = start == 0 || !b[start - 1].is_ascii_alphanumeric();
        let after = end == b.len() || !b[end].is_ascii_alphanumeric();
        if before && after {
            out.push(start..end);
        }
        from = end;
    }
    out
}

/// `text` with every "live" in red: the stream being live is the one state
/// worth catching at a glance.
pub fn live_text(text: impl Into<SharedString>, t: &Theme) -> StyledText {
    let text = text.into();
    let red = HighlightStyle { color: Some(t.red), ..Default::default() };
    let ranges = live_ranges(&text);
    StyledText::new(text).with_highlights(ranges.into_iter().map(|r| (r, red)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_word_live_only() {
        assert_eq!(live_ranges("live"), [0..4]);
        assert_eq!(live_ranges("connected to x live"), [15..19]);
        assert_eq!(live_ranges("Live: start from now · live"), [0..4, 24..28]);
        assert!(live_ranges("olive delivery lives alive").is_empty());
    }
}
