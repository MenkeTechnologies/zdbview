//! rkyv (and generic binary) structural inspector.
//!
//! rkyv archives are not self-describing — the format stores no field names or
//! type tags (<https://rkyv.org/format.html>), so a generic reader cannot recover
//! the schema. What it CAN do without the Rust type is show the raw structure:
//! a hex/ascii dump and the runs of printable text embedded in the archive
//! (strings, keys, interned identifiers). Typed CRUD would require a supplied
//! schema descriptor and is deferred.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct RkyvStore {
    pub path: PathBuf,
    /// The whole archive. Shared rather than owned: every edit path and the
    /// background decoder need the bytes at the same time, and a 382 MB shard
    /// copied per operation is 382 MB per keystroke.
    pub bytes: Arc<[u8]>,
}

/// Most printable runs collected from one archive. Past this the list is no
/// longer something a person reads, and every entry costs a heap allocation.
const MAX_STRING_HITS: usize = 20_000;
/// Bytes scanned for printable runs. A 382 MB shard took 4.2 s to scan in full,
/// which is a stall the picker's file open cannot afford.
const STRINGS_SCAN_CAP: usize = 64 * 1024 * 1024;

/// The result of a bounded string extraction.
#[derive(Debug, Default)]
pub struct Strings {
    pub hits: Vec<StringHit>,
    /// A bound was hit, so this is not every run in the file.
    pub truncated: bool,
    /// How many bytes were scanned.
    pub scanned: usize,
}

/// A run of printable ASCII found in the archive, with its byte offset.
#[derive(Debug)]
pub struct StringHit {
    pub offset: usize,
    pub text: String,
}

impl RkyvStore {
    pub fn open(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            bytes: bytes.into(),
        })
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Extract runs of printable ASCII of at least `min_len` bytes, bounded so a
    /// huge archive cannot stall the UI: at most [`MAX_STRING_HITS`] runs from the
    /// first [`STRINGS_SCAN_CAP`] bytes. `truncated` says whether either bound
    /// was hit, which the Strings view reports.
    pub fn strings(&self, min_len: usize) -> Strings {
        let scan_end = self.bytes.len().min(STRINGS_SCAN_CAP);
        let mut hits = Vec::new();
        let mut start: Option<usize> = None;
        for (i, &b) in self.bytes[..scan_end].iter().enumerate() {
            if hits.len() >= MAX_STRING_HITS {
                break;
            }
            let printable = (0x20..0x7f).contains(&b);
            match (printable, start) {
                (true, None) => start = Some(i),
                (false, Some(s)) => {
                    if i - s >= min_len {
                        hits.push(StringHit {
                            offset: s,
                            text: String::from_utf8_lossy(&self.bytes[s..i]).into_owned(),
                        });
                    }
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            if hits.len() < MAX_STRING_HITS && scan_end - s >= min_len {
                hits.push(StringHit {
                    offset: s,
                    text: String::from_utf8_lossy(&self.bytes[s..scan_end]).into_owned(),
                });
            }
        }
        let truncated = hits.len() >= MAX_STRING_HITS || scan_end < self.bytes.len();
        Strings {
            hits,
            truncated,
            scanned: scan_end,
        }
    }

    /// One 16-byte `offset  hex bytes  |ascii|` line, `xxd` style. Formatting
    /// lives in `crate::hexedit` so every hex view in the app shares one layout.
    pub fn hex_row(&self, offset: usize) -> String {
        let end = (offset + 16).min(self.bytes.len());
        let chunk = &self.bytes[offset.min(self.bytes.len())..end];
        crate::hexedit::hex_dump_line(offset, chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::{RkyvStore, MAX_STRING_HITS};
    use std::sync::Arc;

    fn store(bytes: &[u8]) -> RkyvStore {
        RkyvStore {
            path: std::path::PathBuf::from("/tmp/in-memory"),
            bytes: Arc::from(bytes.to_vec()),
        }
    }

    #[test]
    fn runs_shorter_than_the_minimum_are_not_hits() {
        // "ab" (2) then "hello" (5), separated by bytes that are not printable.
        let s = store(b"\x00ab\x00hello\x00");
        let hits = s.strings(3).hits;
        assert_eq!(hits.len(), 1, "only the run that is long enough");
        assert_eq!(hits[0].text, "hello");
        assert_eq!(hits[0].offset, 4, "the offset is where the run starts");
        // Lower the bar and both runs count.
        assert_eq!(s.strings(2).hits.len(), 2);
    }

    #[test]
    fn a_run_that_reaches_the_end_of_the_file_is_still_a_hit() {
        // The scan closes a run when it meets a non-printable byte; a file that
        // ends mid-run has to be closed by the end of the file instead.
        let s = store(b"\x00trailing");
        let hits = s.strings(4).hits;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "trailing");
        assert_eq!(hits[0].offset, 1);
    }

    #[test]
    fn printable_is_space_through_tilde() {
        // The boundaries either side: 0x1f and 0x7f are not text, 0x20 and 0x7e
        // are. A tab and a newline are separators here, not characters.
        let s = store(b"\x1f\x20\x7e\x7f\x00a\tb\nc");
        let hits = s.strings(1).hits;
        let texts: Vec<&str> = hits.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, vec![" ~", "a", "b", "c"]);
    }

    #[test]
    fn too_many_runs_stops_the_scan_and_says_so() {
        // One more run than the list holds, each two bytes with a separator.
        let mut bytes = Vec::new();
        for _ in 0..MAX_STRING_HITS + 1 {
            bytes.extend_from_slice(b"ab\x00");
        }
        let s = store(&bytes);
        let got = s.strings(2);
        assert_eq!(got.hits.len(), MAX_STRING_HITS, "capped");
        assert!(got.truncated, "and the view is told the list is partial");

        // A file whose runs all fit is not reported as truncated.
        let small = store(b"\x00hello\x00world\x00");
        let got = small.strings(3);
        assert_eq!(got.hits.len(), 2);
        assert!(!got.truncated);
        assert_eq!(got.scanned, small.len());
    }

    #[test]
    fn a_hex_row_stops_at_the_end_of_the_file() {
        let s = store(b"0123456789");
        // A full row's worth is asked for; only what exists is rendered.
        let row = s.hex_row(0);
        assert!(row.contains("30 31 32 33"), "{row}");
        assert!(row.contains("0123456789"), "{row}");
        // Past the end: no panic, and nothing invented.
        let past = s.hex_row(64);
        assert!(!past.contains("30"), "{past}");
    }
}
