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
}
