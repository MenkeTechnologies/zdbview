//! Recognized rkyv archive formats.
//!
//! rkyv archives are not self-describing, so decoding one to key/value requires
//! knowing its Rust type. This module carries faithful copies of the archive
//! types for the formats zdbview recognizes, detects each by its magic header,
//! validates with `rkyv::check_archived_root`, and yields `(key, value)`
//! records. Anything unrecognized falls back to the structural inspector.
//!
//! The copied types MUST stay byte-compatible with the producer: same rkyv
//! version (0.7), same features (`archive_le`, `size_32`), same field order and
//! types. A mismatch shows up as failed validation (returns `None`), not
//! silent corruption.

use rkyv::{Archive, Deserialize, Serialize};
use std::collections::HashMap;

// Family A — the shared zshrs script-cache template. Every host below uses an
// identical archive layout with only its magic (and header version field name)
// differing, so one `ScriptShard` type decodes them all.
const ZSHRS_MAGIC: u32 = 0x5A52_5343; // "ZRSC"
const STRYKE_MAGIC: u32 = 0x5354_5259; // "STRY"
const AWKRS_MAGIC: u32 = 0x4157_4B52; // "AWKR"
const VIMLRS_MAGIC: u32 = 0x5649_4D4C; // "VIML"

/// zshrs autoload cache magic ("ZRAL" little-endian).
const AUTOLOAD_MAGIC: u32 = 0x5A52_414C;
/// elisprs heap-image cache magic ("ELSP").
const ELISP_MAGIC: u32 = 0x454C_5350;

// ---- faithful copies of the zshrs shard archive types ----------------------

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct ShardHeader {
    magic: u32,
    format_version: u32,
    zshrs_version: String,
    pointer_width: u32,
    built_at_secs: u64,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct ScriptEntry {
    mtime_secs: i64,
    mtime_nsecs: i64,
    binary_mtime_at_cache: i64,
    cached_at_secs: i64,
    chunk_blob: Vec<u8>,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct ScriptShard {
    header: ShardHeader,
    entries: HashMap<String, ScriptEntry>,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct AutoloadEntry {
    binary_mtime_at_cache: i64,
    cached_at_secs: i64,
    chunk_blob: Vec<u8>,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct AutoloadShard {
    header: ShardHeader,
    entries: HashMap<String, AutoloadEntry>,
}

// ---- Family B: elisprs heap-image cache (ELSP) -----------------------------
// Distinct header (schema_key instead of a version string, and a different
// field order) and a distinct entry (forms/heap/oclosure blobs).

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct ElispHeader {
    magic: u32,
    format_version: u32,
    pointer_width: u32,
    built_at_secs: u64,
    schema_key: String,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct ElispEntry {
    mtime_ns: i64,
    binary_mtime_at_cache: i64,
    cached_at_secs: i64,
    forms: Vec<Vec<u8>>,
    heap: Vec<u8>,
    oclosure_meta: Vec<u8>,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct ElispShard {
    header: ElispHeader,
    entries: HashMap<String, ElispEntry>,
}

// ---- Family C: header-less, hash-keyed shards (no magic) --------------------
// pythonrs keeps a source path + a verify hash; rubylang and arb share an
// identical minimal (key, blob) layout and cannot be told apart structurally.

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct PyEntry {
    key: u64,
    verify: u64,
    source: String,
    blob: Vec<u8>,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct PyShard {
    entries: Vec<PyEntry>,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct HashEntry {
    key: u64,
    blob: Vec<u8>,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct HashShard {
    entries: Vec<HashEntry>,
}

// ---- decoded output --------------------------------------------------------

/// One key/value record extracted from a recognized archive.
pub struct KvRecord {
    pub key: String,
    /// Raw value bytes (the entry's blob) for hex display.
    pub value: Vec<u8>,
    /// Decoded scalar fields of the value struct (name, rendered value).
    pub fields: Vec<(String, String)>,
}

/// A fully decoded, recognized archive.
pub struct Decoded {
    /// Human name of the detected format.
    pub format: String,
    /// Header/metadata fields.
    pub header: Vec<(String, String)>,
    /// Records, sorted by key.
    pub records: Vec<KvRecord>,
}

/// Detect the archive format and decode it to key/value records. Magic-bearing
/// formats are matched by their header magic; the header-less hash-keyed shards
/// are attempted last, gated by rkyv validation. `None` → structural fallback.
pub fn try_decode(bytes: &[u8]) -> Option<Decoded> {
    // Family A — shared script-cache template, one type, per-host magic + name.
    for (magic, name) in [
        (ZSHRS_MAGIC, "zshrs script cache (ZRSC)"),
        (STRYKE_MAGIC, "strykelang script cache (STRY)"),
        (AWKRS_MAGIC, "awkrs script cache (AWKR)"),
        (VIMLRS_MAGIC, "vimlrs script cache (VIML)"),
    ] {
        if contains_u32_le(bytes, magic) {
            if let Ok(s) = rkyv::check_archived_root::<ScriptShard>(bytes) {
                if u32::from(s.header.magic) == magic {
                    return Some(decode_script(s, name));
                }
            }
        }
    }

    // zshrs autoload cache.
    if contains_u32_le(bytes, AUTOLOAD_MAGIC) {
        if let Ok(s) = rkyv::check_archived_root::<AutoloadShard>(bytes) {
            if u32::from(s.header.magic) == AUTOLOAD_MAGIC {
                return Some(decode_autoload(s));
            }
        }
    }

    // Family B — elisprs heap-image cache.
    if contains_u32_le(bytes, ELISP_MAGIC) {
        if let Ok(s) = rkyv::check_archived_root::<ElispShard>(bytes) {
            if u32::from(s.header.magic) == ELISP_MAGIC {
                return Some(decode_elisp(s));
            }
        }
    }

    // Family C — header-less hash-keyed shards, distinguished only by whether a
    // source string is present. Try the richer pythonrs layout first; ruby/arb
    // share the minimal layout and are labeled generically.
    if let Ok(s) = rkyv::check_archived_root::<PyShard>(bytes) {
        if plausible_hash_shard(s.entries.len(), bytes.len()) {
            return Some(decode_python(s));
        }
    }
    if let Ok(s) = rkyv::check_archived_root::<HashShard>(bytes) {
        if plausible_hash_shard(s.entries.len(), bytes.len()) {
            return Some(decode_hash(s));
        }
    }

    None
}

/// Guard against a random buffer validating as an empty/degenerate hash shard:
/// require at least one entry and a non-trivial file.
fn plausible_hash_shard(entries: usize, bytes_len: usize) -> bool {
    entries >= 1 && bytes_len >= 16
}

fn decode_script(s: &ArchivedScriptShard, name: &str) -> Decoded {
    let mut records: Vec<KvRecord> = s
        .entries
        .iter()
        .map(|(k, v)| KvRecord {
            key: k.as_str().to_string(),
            value: v.chunk_blob.as_slice().to_vec(),
            fields: vec![
                ("mtime_secs".into(), i64::from(v.mtime_secs).to_string()),
                ("mtime_nsecs".into(), i64::from(v.mtime_nsecs).to_string()),
                (
                    "binary_mtime_at_cache".into(),
                    i64::from(v.binary_mtime_at_cache).to_string(),
                ),
                ("cached_at_secs".into(), i64::from(v.cached_at_secs).to_string()),
                ("chunk_blob_len".into(), v.chunk_blob.len().to_string()),
            ],
        })
        .collect();
    records.sort_by(|a, b| a.key.cmp(&b.key));
    Decoded {
        format: name.into(),
        header: header_fields(&s.header),
        records,
    }
}

fn decode_elisp(s: &ArchivedElispShard) -> Decoded {
    let mut records: Vec<KvRecord> = s
        .entries
        .iter()
        .map(|(k, v)| KvRecord {
            key: k.as_str().to_string(),
            value: v.heap.as_slice().to_vec(),
            fields: vec![
                ("mtime_ns".into(), i64::from(v.mtime_ns).to_string()),
                (
                    "binary_mtime_at_cache".into(),
                    i64::from(v.binary_mtime_at_cache).to_string(),
                ),
                ("cached_at_secs".into(), i64::from(v.cached_at_secs).to_string()),
                ("forms".into(), v.forms.len().to_string()),
                ("heap_len".into(), v.heap.len().to_string()),
                ("oclosure_meta_len".into(), v.oclosure_meta.len().to_string()),
            ],
        })
        .collect();
    records.sort_by(|a, b| a.key.cmp(&b.key));
    Decoded {
        format: "elisprs heap-image cache (ELSP)".into(),
        header: vec![
            ("magic".into(), format!("{:#010x}", u32::from(s.header.magic))),
            (
                "format_version".into(),
                u32::from(s.header.format_version).to_string(),
            ),
            (
                "pointer_width".into(),
                u32::from(s.header.pointer_width).to_string(),
            ),
            (
                "built_at_secs".into(),
                u64::from(s.header.built_at_secs).to_string(),
            ),
            ("schema_key".into(), s.header.schema_key.as_str().to_string()),
        ],
        records,
    }
}

fn decode_python(s: &ArchivedPyShard) -> Decoded {
    let mut records: Vec<KvRecord> = s
        .entries
        .iter()
        .map(|e| KvRecord {
            key: e.source.as_str().to_string(),
            value: e.blob.as_slice().to_vec(),
            fields: vec![
                ("key".into(), format!("{:#018x}", u64::from(e.key))),
                ("verify".into(), format!("{:#018x}", u64::from(e.verify))),
                ("blob_len".into(), e.blob.len().to_string()),
            ],
        })
        .collect();
    records.sort_by(|a, b| a.key.cmp(&b.key));
    Decoded {
        format: "pythonrs bytecode cache (no header)".into(),
        header: vec![("entries".into(), records.len().to_string())],
        records,
    }
}

fn decode_hash(s: &ArchivedHashShard) -> Decoded {
    let records: Vec<KvRecord> = s
        .entries
        .iter()
        .map(|e| KvRecord {
            key: format!("{:#018x}", u64::from(e.key)),
            value: e.blob.as_slice().to_vec(),
            fields: vec![("blob_len".into(), e.blob.len().to_string())],
        })
        .collect();
    Decoded {
        format: "hash-keyed script cache (rubylang / arb, no header)".into(),
        header: vec![("entries".into(), records.len().to_string())],
        records,
    }
}

fn decode_autoload(s: &ArchivedAutoloadShard) -> Decoded {
    let mut records: Vec<KvRecord> = s
        .entries
        .iter()
        .map(|(k, v)| KvRecord {
            key: k.as_str().to_string(),
            value: v.chunk_blob.as_slice().to_vec(),
            fields: vec![
                (
                    "binary_mtime_at_cache".into(),
                    i64::from(v.binary_mtime_at_cache).to_string(),
                ),
                ("cached_at_secs".into(), i64::from(v.cached_at_secs).to_string()),
                ("chunk_blob_len".into(), v.chunk_blob.len().to_string()),
            ],
        })
        .collect();
    records.sort_by(|a, b| a.key.cmp(&b.key));
    Decoded {
        format: "zshrs autoload cache (ZRAL)".into(),
        header: header_fields(&s.header),
        records,
    }
}

fn header_fields(h: &ArchivedShardHeader) -> Vec<(String, String)> {
    vec![
        ("magic".into(), format!("{:#010x}", u32::from(h.magic))),
        ("format_version".into(), u32::from(h.format_version).to_string()),
        ("version".into(), h.zshrs_version.as_str().to_string()),
        ("pointer_width".into(), u32::from(h.pointer_width).to_string()),
        ("built_at_secs".into(), u64::from(h.built_at_secs).to_string()),
    ]
}

/// Whether `val` appears anywhere in `bytes` as little-endian u32 — a cheap
/// pre-filter before attempting full validation.
fn contains_u32_le(bytes: &[u8], val: u32) -> bool {
    let needle = val.to_le_bytes();
    bytes.windows(4).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_shard_roundtrips_through_try_decode() {
        let mut entries = HashMap::new();
        entries.insert(
            "/tmp/a.sh".to_string(),
            ScriptEntry {
                mtime_secs: 111,
                mtime_nsecs: 222,
                binary_mtime_at_cache: 333,
                cached_at_secs: 444,
                chunk_blob: vec![1, 2, 3, 4],
            },
        );
        let shard = ScriptShard {
            header: ShardHeader {
                magic: ZSHRS_MAGIC,
                format_version: 1,
                zshrs_version: "9.9.9".into(),
                pointer_width: 8,
                built_at_secs: 555,
            },
            entries,
        };
        let bytes = rkyv::to_bytes::<_, 256>(&shard).expect("serialize");
        let d = try_decode(&bytes[..]).expect("recognized");
        assert_eq!(d.format, "zshrs script cache (ZRSC)");
        assert_eq!(d.records.len(), 1);
        assert_eq!(d.records[0].key, "/tmp/a.sh");
        assert_eq!(d.records[0].value, vec![1, 2, 3, 4]);
        assert!(d
            .header
            .iter()
            .any(|(k, v)| k == "version" && v == "9.9.9"));
    }

    #[test]
    fn hash_shard_roundtrips() {
        let shard = HashShard {
            entries: vec![
                HashEntry { key: 0xAABB, blob: vec![9, 9, 9] },
                HashEntry { key: 0xCCDD, blob: vec![7] },
            ],
        };
        let bytes = rkyv::to_bytes::<_, 256>(&shard).expect("serialize");
        let d = try_decode(&bytes[..]).expect("recognized");
        assert!(d.format.contains("hash-keyed"));
        assert_eq!(d.records.len(), 2);
        assert_eq!(d.records[0].value, vec![9, 9, 9]);
    }

    #[test]
    fn garbage_is_not_recognized() {
        assert!(try_decode(&[0u8; 64]).is_none());
        assert!(try_decode(b"not an archive at all, just plain text bytes").is_none());
    }
}
