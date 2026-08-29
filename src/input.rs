//! Single-line text editing, shared by every prompt in zdbview.
//!
//! The motions were ported from iftoprs's filter prompt and lived inside
//! [`crate::app`] while the grid's modal prompts were the only editors. The
//! schema designers edit text too — a column name, a default, a `CHECK` body —
//! and a second implementation of "where does Ctrl-W stop" is exactly the kind of
//! drift that makes two prompts behave differently, so they live here and both
//! callers use them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Move the cursor one char left (UTF-8-safe). Ported from iftoprs
/// `FilterState::left`.
pub fn left(buf: &str, cur: usize) -> usize {
    if cur > 0 {
        buf[..cur]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    } else {
        0
    }
}

/// Move the cursor one char right (UTF-8-safe). Ported from iftoprs
/// `FilterState::right`.
pub fn right(buf: &str, cur: usize) -> usize {
    if cur < buf.len() {
        buf[cur..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| cur + i)
            .unwrap_or(buf.len())
    } else {
        buf.len()
    }
}

/// Delete the word before the cursor (Ctrl+W). Ported from iftoprs
/// `FilterState::delete_word` — skips trailing whitespace, then the word,
/// stepping by real UTF-8 widths. Returns the new cursor position.
pub fn delete_word(buf: &mut String, cur: usize) -> usize {
    let s = &buf[..cur];
    let trimmed = s.trim_end();
    let word_start = match trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
    {
        Some((i, c)) => i + c.len_utf8(),
        None => 0,
    };
    buf.drain(word_start..cur);
    word_start
}

/// A line being edited, with its cursor. What a designer field holds while it is
/// open for editing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub buf: String,
    /// Byte offset of the cursor within `buf`.
    pub cur: usize,
}

/// What a key did to the line.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Edit {
    /// The line took the key.
    Took,
    /// Enter: the caller should commit the line.
    Commit,
    /// Esc: the caller should discard it.
    Cancel,
    /// Nothing here handles this key; the caller may.
    Pass,
}

impl Line {
    /// A line holding `text`, with the cursor at its end — where a field opens.
    pub fn at_end(text: &str) -> Self {
        Line {
            buf: text.to_string(),
            cur: text.len(),
        }
    }

    /// Apply one key. The chords match the grid's prompts exactly, because they
    /// are the same code.
    pub fn on_key(&mut self, key: KeyEvent) -> Edit {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.cur = self.cur.min(self.buf.len());
        match key.code {
            KeyCode::Esc => return Edit::Cancel,
            KeyCode::Enter => return Edit::Commit,
            KeyCode::Left => self.cur = left(&self.buf, self.cur),
            KeyCode::Right => self.cur = right(&self.buf, self.cur),
            KeyCode::Home => self.cur = 0,
            KeyCode::End => self.cur = self.buf.len(),
            KeyCode::Char('a') if ctrl => self.cur = 0,
            KeyCode::Char('e') if ctrl => self.cur = self.buf.len(),
            KeyCode::Char('b') if ctrl => self.cur = left(&self.buf, self.cur),
            KeyCode::Char('f') if ctrl => self.cur = right(&self.buf, self.cur),
            KeyCode::Char('w') if ctrl => self.cur = delete_word(&mut self.buf, self.cur),
            KeyCode::Char('u') if ctrl => {
                self.buf.drain(..self.cur);
                self.cur = 0;
            }
            KeyCode::Char('k') if ctrl => self.buf.truncate(self.cur),
            KeyCode::Backspace => {
                if self.cur > 0 {
                    let p = left(&self.buf, self.cur);
                    self.buf.drain(p..self.cur);
                    self.cur = p;
                }
            }
            KeyCode::Delete => {
                if self.cur < self.buf.len() {
                    let n = right(&self.buf, self.cur);
                    self.buf.drain(self.cur..n);
                }
            }
            KeyCode::Char(c) => {
                self.buf.insert(self.cur, c);
                self.cur += c.len_utf8();
            }
            _ => return Edit::Pass,
        }
        Edit::Took
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn typing_and_killing_agree_with_the_prompt_motions() {
        let mut l = Line::default();
        for c in "one two".chars() {
            assert_eq!(l.on_key(key(c)), Edit::Took);
        }
        assert_eq!(l.buf, "one two");
        assert_eq!(l.on_key(ctrl('w')), Edit::Took);
        assert_eq!(l.buf, "one ");
        assert_eq!(l.on_key(ctrl('u')), Edit::Took);
        assert_eq!(l.buf, "");
    }

    #[test]
    fn the_cursor_steps_by_characters_not_bytes() {
        let mut l = Line::at_end("aé");
        assert_eq!(l.cur, 3);
        l.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::empty()));
        assert_eq!(l.cur, 1, "one char back over a two-byte char");
        l.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()));
        assert_eq!(l.buf, "é");
    }

    #[test]
    fn enter_and_esc_are_reported_rather_than_swallowed() {
        let mut l = Line::at_end("x");
        assert_eq!(
            l.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())),
            Edit::Commit
        );
        assert_eq!(
            l.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            Edit::Cancel
        );
        assert_eq!(
            l.on_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::empty())),
            Edit::Pass
        );
    }

    /// Ctrl-W stops at the start of the word before the cursor, having first
    /// skipped whatever whitespace sits between — the behaviour the shell has,
    /// and the one both prompts share because they run this code.
    #[test]
    fn ctrl_w_eats_one_word_and_the_space_before_the_cursor() {
        let mut line = Line::at_end("select name from t");
        assert_eq!(line.on_key(ctrl('w')), Edit::Took);
        assert_eq!(line.buf, "select name from ");
        assert_eq!(line.cur, line.buf.len());

        // Trailing whitespace goes with the word, not instead of it.
        let mut line = Line::at_end("select name    ");
        line.on_key(ctrl('w'));
        assert_eq!(line.buf, "select ");

        // From the middle, only what is behind the cursor is touched.
        let mut line = Line {
            buf: "alpha beta gamma".into(),
            cur: "alpha beta".len(),
        };
        line.on_key(ctrl('w'));
        assert_eq!(line.buf, "alpha  gamma");
        assert_eq!(line.cur, "alpha ".len());

        // Nothing behind the cursor is nothing to delete.
        let mut line = Line {
            buf: "word".into(),
            cur: 0,
        };
        line.on_key(ctrl('w'));
        assert_eq!(line.buf, "word");
        assert_eq!(line.cur, 0);

        // A line of only spaces empties rather than looping.
        let mut line = Line::at_end("    ");
        line.on_key(ctrl('w'));
        assert_eq!(line.buf, "");
    }

    /// Ctrl-U cuts back to the start, Ctrl-K forward to the end, and Home/End
    /// (and their Ctrl-A/Ctrl-E spellings) put the cursor at either edge.
    #[test]
    fn the_line_kills_in_both_directions_from_where_the_cursor_is() {
        let mut line = Line {
            buf: "keep this half".into(),
            cur: "keep ".len(),
        };
        line.on_key(ctrl('u'));
        assert_eq!((line.buf.as_str(), line.cur), ("this half", 0));

        let mut line = Line {
            buf: "keep this half".into(),
            cur: "keep ".len(),
        };
        line.on_key(ctrl('k'));
        assert_eq!((line.buf.as_str(), line.cur), ("keep ", 5));

        let mut line = Line::at_end("abc");
        line.on_key(ctrl('a'));
        assert_eq!(line.cur, 0);
        line.on_key(ctrl('e'));
        assert_eq!(line.cur, 3);
        line.on_key(KeyEvent::from(KeyCode::Home));
        assert_eq!(line.cur, 0);
        line.on_key(KeyEvent::from(KeyCode::End));
        assert_eq!(line.cur, 3);
    }

    /// Delete takes the character in front of the cursor, Backspace the one
    /// behind it, and neither runs off the end of the line.
    #[test]
    fn delete_and_backspace_stop_at_the_edges() {
        let mut line = Line {
            buf: "abc".into(),
            cur: 1,
        };
        line.on_key(KeyEvent::from(KeyCode::Delete));
        assert_eq!((line.buf.as_str(), line.cur), ("ac", 1), "forward");
        line.on_key(KeyEvent::from(KeyCode::Backspace));
        assert_eq!((line.buf.as_str(), line.cur), ("c", 0), "backward");

        // At either edge the key is taken and nothing happens.
        line.cur = 0;
        line.on_key(KeyEvent::from(KeyCode::Backspace));
        assert_eq!(line.buf, "c");
        line.cur = line.buf.len();
        line.on_key(KeyEvent::from(KeyCode::Delete));
        assert_eq!(line.buf, "c");
    }

    /// Multi-byte text is edited by characters. A cursor left inside one would
    /// panic on the next insert, so every motion has to land on a boundary.
    #[test]
    fn multi_byte_text_is_never_split_by_a_motion() {
        let mut line = Line::at_end("héllo→");
        // The arrow is three bytes; one Left steps over all of it.
        line.on_key(KeyEvent::from(KeyCode::Left));
        assert_eq!(line.cur, "héllo".len());
        line.on_key(KeyEvent::from(KeyCode::Left));
        assert_eq!(line.cur, "héll".len());

        // Inserting there keeps the string valid, which is the point.
        line.on_key(key('x'));
        assert_eq!(line.buf, "héllxo→");

        // Backspace over a two-byte character removes the whole character.
        let mut line = Line::at_end("é");
        line.on_key(KeyEvent::from(KeyCode::Backspace));
        assert_eq!(line.buf, "");
        assert_eq!(line.cur, 0);

        // And a cursor past the end is clamped rather than panicking.
        let mut line = Line {
            buf: "ab".into(),
            cur: 99,
        };
        assert_eq!(line.on_key(key('c')), Edit::Took);
        assert_eq!(line.buf, "abc");
    }

    /// A key the line has no use for is handed back, so the caller can act on it
    /// rather than having it silently swallowed.
    #[test]
    fn an_unhandled_key_is_passed_back_to_the_caller() {
        let mut line = Line::at_end("x");
        assert_eq!(line.on_key(KeyEvent::from(KeyCode::Tab)), Edit::Pass);
        assert_eq!(line.on_key(KeyEvent::from(KeyCode::F(5))), Edit::Pass);
        assert_eq!(line.on_key(KeyEvent::from(KeyCode::Up)), Edit::Pass);
        assert_eq!(line.buf, "x", "and the line is untouched");
    }
}
