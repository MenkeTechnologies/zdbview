//! Text fitted to a fixed width, shared by every screen that draws a column.
//!
//! Three copies of "take w-1 chars and add an ellipsis" had appeared — the grid,
//! the designers and the insert form — which is exactly how two of them end up
//! disagreeing about whether the ellipsis counts toward the width.

/// `s` cut to `max` characters, marking that it was cut. Counts characters, not
/// bytes, so a column of UTF-8 lines up.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn it_counts_characters_and_marks_what_it_cut() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab\u{2026}");
        // Multi-byte characters count once each.
        assert_eq!(truncate("\u{e9}\u{e9}\u{e9}", 3), "\u{e9}\u{e9}\u{e9}");
        assert_eq!(
            truncate("\u{e9}\u{e9}\u{e9}\u{e9}", 3),
            "\u{e9}\u{e9}\u{2026}"
        );
        assert_eq!(truncate("abc", 0), "\u{2026}");
    }
}
