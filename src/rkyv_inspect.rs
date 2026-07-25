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

pub struct RkyvStore {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
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
            bytes,
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
