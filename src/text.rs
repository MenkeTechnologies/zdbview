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

/// `s` cut to `max` characters from the *front*, so the end survives. Keys that
/// share a long prefix — absolute paths in a cache shard, for one — are told
/// apart by their tail, and cutting the tail makes every row read the same.
pub fn truncate_start(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let mut out = String::from("\u{2026}");
    out.extend(s.chars().skip(n - max.saturating_sub(1)));
    out
}

#[cfg(test)]
mod tests {
    use super::{truncate, truncate_start};

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

    #[test]
    fn cutting_from_the_front_keeps_the_tail() {
        assert_eq!(truncate_start("abc", 3), "abc");
        assert_eq!(truncate_start("abcd", 3), "\u{2026}cd");
        assert_eq!(
            truncate_start("\u{e9}\u{e9}\u{e9}\u{e9}", 3),
            "\u{2026}\u{e9}\u{e9}"
        );
        assert_eq!(truncate_start("abc", 0), "\u{2026}");
    }
}
