//! Copy text to the system clipboard via the OSC 52 terminal escape sequence.
//!
//! OSC 52 asks the terminal emulator to set the clipboard, so it works both
//! locally and over SSH with no clipboard library or X/Wayland/pbcopy
//! dependency — the escape is written straight to the controlling terminal.

use std::io::Write;

/// Copy `text` to the clipboard. Best-effort: silently does nothing if there is
/// no controlling terminal (e.g. output redirected to a file).
pub fn copy(text: &str) -> bool {
    let seq = sequence(text);
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        if tty.write_all(seq.as_bytes()).is_ok() {
            let _ = tty.flush();
            return true;
        }
    }
    false
}

/// The escape a terminal reads as "set the clipboard to this": `OSC 52`, the
/// `c` selection, the payload in base64, terminated by BEL. Built apart from the
/// write so the bytes that go to the terminal can be checked without one.
fn sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Standard base64 (RFC 4648) with `=` padding.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{base64, sequence};

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// The escape has to be exactly what a terminal expects, byte for byte: a
    /// wrong introducer or a missing terminator leaves the payload printed on
    /// the screen instead of copied.
    #[test]
    fn the_osc_52_escape_is_shaped_the_way_terminals_read_it() {
        assert_eq!(sequence("foo"), "\u{1b}]52;c;Zm9v\u{7}");
        // `c` is the clipboard selection, not the primary one.
        assert!(sequence("x").starts_with("\u{1b}]52;c;"));
        assert!(sequence("x").ends_with('\u{7}'));
        // Empty text is still a well-formed escape — it clears the clipboard.
        assert_eq!(sequence(""), "\u{1b}]52;c;\u{7}");
    }

    /// A cell copied out of the grid can hold anything, and none of it may reach
    /// the terminal as itself: base64 is what keeps a newline or an escape from
    /// being read as more terminal input.
    #[test]
    fn nothing_in_the_payload_can_be_read_as_terminal_input() {
        let nasty = "line one\nline two\u{1b}]52;c;evil\u{7}\u{0}end";
        let seq = sequence(nasty);
        let body = seq
            .strip_prefix("\u{1b}]52;c;")
            .and_then(|s| s.strip_suffix('\u{7}'))
            .expect("the frame is intact");
        assert!(
            body.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)),
            "the payload is base64 and nothing else: {body}"
        );
        // Multi-byte text goes as its UTF-8 bytes, which is what the terminal
        // decodes back.
        assert_eq!(base64("é".as_bytes()), "w6k=");
    }
}
