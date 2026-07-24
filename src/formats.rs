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

// strykelang's native v4 entry adds `program_blob` before `chunk_blob`; its
// older/compat shards use the 5-field template above. Both carry the STRY magic.
#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct StrykeEntry {
    mtime_secs: i64,
    mtime_nsecs: i64,
    binary_mtime_at_cache: i64,
    cached_at_secs: i64,
    program_blob: Vec<u8>,
    chunk_blob: Vec<u8>,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct StrykeShard {
    header: ShardHeader,
    entries: HashMap<String, StrykeEntry>,
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
    /// Human-readable key shown in the UI. NOT necessarily unique (pythonrs
    /// stores many records under the same source path).
    pub key: String,
    /// Stable, unique identity used for deletion. For the map formats this is
    /// the map key; for the hash-keyed formats it is the u64 content hash in
    /// hex, so deleting never affects a different record with the same display
    /// key.
    pub del_key: String,
    /// Raw value bytes (the entry's blob) for hex display.
    pub value: Vec<u8>,
    /// Decoded scalar fields of the value struct (name, rendered value).
    pub fields: Vec<(String, String)>,
}

/// Which concrete archive type backs a `Decoded`, so a record can be deleted
/// and the shard re-serialized with the matching writer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FormatKind {
    Script,
    Stryke,
    Autoload,
    Elisp,
    Python,
    Hash,
}

/// A fully decoded, recognized archive.
pub struct Decoded {
    /// Human name of the detected format.
    pub format: String,
    /// The concrete archive type (for write-back).
    pub kind: FormatKind,
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

    // strykelang shares the STRY magic across two layouts: try the native v4
    // (with program_blob) first, then fall back to the 5-field compat template.
    if contains_u32_le(bytes, STRYKE_MAGIC) {
        if let Ok(s) = rkyv::check_archived_root::<StrykeShard>(bytes) {
            if u32::from(s.header.magic) == STRYKE_MAGIC {
                return Some(decode_stryke(s));
            }
        }
        if let Ok(s) = rkyv::check_archived_root::<ScriptShard>(bytes) {
            if u32::from(s.header.magic) == STRYKE_MAGIC {
                return Some(decode_script(s, "strykelang script cache (STRY, compat)"));
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
            del_key: k.as_str().to_string(),
            value: v.chunk_blob.as_slice().to_vec(),
            fields: vec![
                ("mtime_secs".into(), i64::from(v.mtime_secs).to_string()),
                ("mtime_nsecs".into(), i64::from(v.mtime_nsecs).to_string()),
                (
                    "binary_mtime_at_cache".into(),
                    i64::from(v.binary_mtime_at_cache).to_string(),
                ),
                (
                    "cached_at_secs".into(),
                    i64::from(v.cached_at_secs).to_string(),
                ),
                ("chunk_blob_len".into(), v.chunk_blob.len().to_string()),
            ],
        })
        .collect();
    records.sort_by(|a, b| a.key.cmp(&b.key));
    Decoded {
        format: name.into(),
        kind: FormatKind::Script,
        header: header_fields(&s.header),
        records,
    }
}

fn decode_stryke(s: &ArchivedStrykeShard) -> Decoded {
    let mut records: Vec<KvRecord> = s
        .entries
        .iter()
        .map(|(k, v)| KvRecord {
            key: k.as_str().to_string(),
            del_key: k.as_str().to_string(),
            value: v.chunk_blob.as_slice().to_vec(),
            fields: vec![
                ("mtime_secs".into(), i64::from(v.mtime_secs).to_string()),
                (
                    "binary_mtime_at_cache".into(),
                    i64::from(v.binary_mtime_at_cache).to_string(),
                ),
                (
                    "cached_at_secs".into(),
                    i64::from(v.cached_at_secs).to_string(),
                ),
                ("program_blob_len".into(), v.program_blob.len().to_string()),
                ("chunk_blob_len".into(), v.chunk_blob.len().to_string()),
            ],
        })
        .collect();
    records.sort_by(|a, b| a.key.cmp(&b.key));
    Decoded {
        format: "strykelang script cache (STRY)".into(),
        kind: FormatKind::Stryke,
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
            del_key: k.as_str().to_string(),
            value: v.heap.as_slice().to_vec(),
            fields: vec![
                ("mtime_ns".into(), i64::from(v.mtime_ns).to_string()),
                (
                    "binary_mtime_at_cache".into(),
                    i64::from(v.binary_mtime_at_cache).to_string(),
                ),
                (
                    "cached_at_secs".into(),
                    i64::from(v.cached_at_secs).to_string(),
                ),
                ("forms".into(), v.forms.len().to_string()),
                ("heap_len".into(), v.heap.len().to_string()),
                (
                    "oclosure_meta_len".into(),
                    v.oclosure_meta.len().to_string(),
                ),
            ],
        })
        .collect();
    records.sort_by(|a, b| a.key.cmp(&b.key));
    Decoded {
        format: "elisprs heap-image cache (ELSP)".into(),
        kind: FormatKind::Elisp,
        header: vec![
            (
                "magic".into(),
                format!("{:#010x}", u32::from(s.header.magic)),
            ),
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
            (
                "schema_key".into(),
                s.header.schema_key.as_str().to_string(),
            ),
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
            del_key: format!("{:#018x}", u64::from(e.key)),
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
        kind: FormatKind::Python,
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
            del_key: format!("{:#018x}", u64::from(e.key)),
            value: e.blob.as_slice().to_vec(),
            fields: vec![("blob_len".into(), e.blob.len().to_string())],
        })
        .collect();
    Decoded {
        format: "hash-keyed script cache (rubylang / arb, no header)".into(),
        kind: FormatKind::Hash,
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
            del_key: k.as_str().to_string(),
            value: v.chunk_blob.as_slice().to_vec(),
            fields: vec![
                (
                    "binary_mtime_at_cache".into(),
                    i64::from(v.binary_mtime_at_cache).to_string(),
                ),
                (
                    "cached_at_secs".into(),
                    i64::from(v.cached_at_secs).to_string(),
                ),
                ("chunk_blob_len".into(), v.chunk_blob.len().to_string()),
            ],
        })
        .collect();
    records.sort_by(|a, b| a.key.cmp(&b.key));
    Decoded {
        format: "zshrs autoload cache (ZRAL)".into(),
        kind: FormatKind::Autoload,
        header: header_fields(&s.header),
        records,
    }
}

fn header_fields(h: &ArchivedShardHeader) -> Vec<(String, String)> {
    vec![
        ("magic".into(), format!("{:#010x}", u32::from(h.magic))),
        (
            "format_version".into(),
            u32::from(h.format_version).to_string(),
        ),
        ("version".into(), h.zshrs_version.as_str().to_string()),
        (
            "pointer_width".into(),
            u32::from(h.pointer_width).to_string(),
        ),
        (
            "built_at_secs".into(),
            u64::from(h.built_at_secs).to_string(),
        ),
    ]
}

/// Whether `val` appears anywhere in `bytes` as little-endian u32 — a cheap
/// pre-filter before attempting full validation.
fn contains_u32_le(bytes: &[u8], val: u32) -> bool {
    let needle = val.to_le_bytes();
    bytes.windows(4).any(|w| w == needle)
}

// ---- write-back: delete a record and re-serialize the shard ----------------

/// Deserialize `bytes` as `T`, mutate it with `f`, and re-serialize. The
/// resulting bytes are byte-identical in format to what the producing host
/// writes, so the host reads the edited shard normally.
fn edit_shard<T, F>(bytes: &[u8], f: F) -> Result<Vec<u8>, String>
where
    T: Archive + rkyv::Serialize<rkyv::ser::serializers::AllocSerializer<4096>>,
    T::Archived: rkyv::Deserialize<T, rkyv::Infallible>
        + for<'a> rkyv::CheckBytes<rkyv::validation::validators::DefaultValidator<'a>>,
    F: FnOnce(&mut T),
{
    let archived = rkyv::check_archived_root::<T>(bytes).map_err(|e| e.to_string())?;
    let mut owned: T = archived
        .deserialize(&mut rkyv::Infallible)
        .map_err(|_| "deserialize failed".to_string())?;
    f(&mut owned);
    rkyv::to_bytes::<_, 4096>(&owned)
        .map(|b| b.into_vec())
        .map_err(|e| e.to_string())
}

/// Delete the record identified by `del_key` (the record's stable identity, not
/// its display key) from a recognized shard and return the re-serialized bytes.
/// For the map formats `del_key` is the map key; for the hash-keyed formats it
/// is the `0x…` hex of the entry's u64, so exactly one record is removed even
/// when several share a display key.
pub fn delete_record(bytes: &[u8], kind: FormatKind, del_key: &str) -> Result<Vec<u8>, String> {
    match kind {
        FormatKind::Script => edit_shard::<ScriptShard, _>(bytes, |s| {
            s.entries.remove(del_key);
        }),
        FormatKind::Stryke => edit_shard::<StrykeShard, _>(bytes, |s| {
            s.entries.remove(del_key);
        }),
        FormatKind::Autoload => edit_shard::<AutoloadShard, _>(bytes, |s| {
            s.entries.remove(del_key);
        }),
        FormatKind::Elisp => edit_shard::<ElispShard, _>(bytes, |s| {
            s.entries.remove(del_key);
        }),
        FormatKind::Python => {
            let want = parse_hex_u64(del_key)?;
            edit_shard::<PyShard, _>(bytes, |s| s.entries.retain(|e| e.key != want))
        }
        FormatKind::Hash => {
            let want = parse_hex_u64(del_key)?;
            edit_shard::<HashShard, _>(bytes, |s| s.entries.retain(|e| e.key != want))
        }
    }
}

fn parse_hex_u64(s: &str) -> Result<u64, String> {
    s.strip_prefix("0x")
        .and_then(|h| u64::from_str_radix(h, 16).ok())
        .ok_or_else(|| format!("bad delete key: {}", s))
}

/// For the header-less formats, turn a create/rename key string into a u64: an
/// explicit `0x…` hex is used as-is; otherwise the string is hashed (a real
/// record, though the host indexes by content hash so it is not a cache hit).
fn key_to_u64(s: &str) -> u64 {
    if let Ok(v) = parse_hex_u64(s) {
        return v;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Replace the value blob of the record identified by `del_key`, preserving all
/// other fields, and return the re-serialized shard.
pub fn set_value(
    bytes: &[u8],
    kind: FormatKind,
    del_key: &str,
    value: Vec<u8>,
) -> Result<Vec<u8>, String> {
    match kind {
        FormatKind::Script => edit_shard::<ScriptShard, _>(bytes, |s| {
            if let Some(e) = s.entries.get_mut(del_key) {
                e.chunk_blob = value;
            }
        }),
        FormatKind::Stryke => edit_shard::<StrykeShard, _>(bytes, |s| {
            if let Some(e) = s.entries.get_mut(del_key) {
                e.chunk_blob = value;
            }
        }),
        FormatKind::Autoload => edit_shard::<AutoloadShard, _>(bytes, |s| {
            if let Some(e) = s.entries.get_mut(del_key) {
                e.chunk_blob = value;
            }
        }),
        FormatKind::Elisp => edit_shard::<ElispShard, _>(bytes, |s| {
            if let Some(e) = s.entries.get_mut(del_key) {
                e.heap = value;
            }
        }),
        FormatKind::Python => {
            let want = parse_hex_u64(del_key)?;
            edit_shard::<PyShard, _>(bytes, move |s| {
                if let Some(e) = s.entries.iter_mut().find(|e| e.key == want) {
                    e.blob = value;
                }
            })
        }
        FormatKind::Hash => {
            let want = parse_hex_u64(del_key)?;
            edit_shard::<HashShard, _>(bytes, move |s| {
                if let Some(e) = s.entries.iter_mut().find(|e| e.key == want) {
                    e.blob = value;
                }
            })
        }
    }
}

/// Insert a new record with `key` and `value` (metadata zeroed) and return the
/// re-serialized shard. For the map formats `key` is the map key; for the
/// header-less formats it is a `0x…` hex u64 or a string that is hashed.
pub fn add_record(
    bytes: &[u8],
    kind: FormatKind,
    key: &str,
    value: Vec<u8>,
) -> Result<Vec<u8>, String> {
    match kind {
        FormatKind::Script => edit_shard::<ScriptShard, _>(bytes, |s| {
            s.entries.insert(
                key.to_string(),
                ScriptEntry {
                    mtime_secs: 0,
                    mtime_nsecs: 0,
                    binary_mtime_at_cache: 0,
                    cached_at_secs: 0,
                    chunk_blob: value,
                },
            );
        }),
        FormatKind::Stryke => edit_shard::<StrykeShard, _>(bytes, |s| {
            s.entries.insert(
                key.to_string(),
                StrykeEntry {
                    mtime_secs: 0,
                    mtime_nsecs: 0,
                    binary_mtime_at_cache: 0,
                    cached_at_secs: 0,
                    program_blob: Vec::new(),
                    chunk_blob: value,
                },
            );
        }),
        FormatKind::Autoload => edit_shard::<AutoloadShard, _>(bytes, |s| {
            s.entries.insert(
                key.to_string(),
                AutoloadEntry {
                    binary_mtime_at_cache: 0,
                    cached_at_secs: 0,
                    chunk_blob: value,
                },
            );
        }),
        FormatKind::Elisp => edit_shard::<ElispShard, _>(bytes, |s| {
            s.entries.insert(
                key.to_string(),
                ElispEntry {
                    mtime_ns: 0,
                    binary_mtime_at_cache: 0,
                    cached_at_secs: 0,
                    forms: Vec::new(),
                    heap: value,
                    oclosure_meta: Vec::new(),
                },
            );
        }),
        FormatKind::Python => {
            let k = key_to_u64(key);
            edit_shard::<PyShard, _>(bytes, move |s| {
                s.entries.push(PyEntry {
                    key: k,
                    verify: 0,
                    source: key.to_string(),
                    blob: value,
                });
            })
        }
        FormatKind::Hash => {
            let k = key_to_u64(key);
            edit_shard::<HashShard, _>(bytes, move |s| {
                s.entries.push(HashEntry {
                    key: k,
                    blob: value,
                });
            })
        }
    }
}

/// Rename a record's key (map formats only; the header-less formats key by a
/// content hash and have no renameable key).
pub fn rename_record(
    bytes: &[u8],
    kind: FormatKind,
    del_key: &str,
    new_key: &str,
) -> Result<Vec<u8>, String> {
    let new_key = new_key.to_string();
    match kind {
        FormatKind::Script => edit_shard::<ScriptShard, _>(bytes, move |s| {
            if let Some(v) = s.entries.remove(del_key) {
                s.entries.insert(new_key, v);
            }
        }),
        FormatKind::Stryke => edit_shard::<StrykeShard, _>(bytes, move |s| {
            if let Some(v) = s.entries.remove(del_key) {
                s.entries.insert(new_key, v);
            }
        }),
        FormatKind::Autoload => edit_shard::<AutoloadShard, _>(bytes, move |s| {
            if let Some(v) = s.entries.remove(del_key) {
                s.entries.insert(new_key, v);
            }
        }),
        FormatKind::Elisp => edit_shard::<ElispShard, _>(bytes, move |s| {
            if let Some(v) = s.entries.remove(del_key) {
                s.entries.insert(new_key, v);
            }
        }),
        FormatKind::Python | FormatKind::Hash => {
            Err("rename is not supported for header-less hash-keyed formats".into())
        }
    }
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
        assert!(d.header.iter().any(|(k, v)| k == "version" && v == "9.9.9"));
    }

    #[test]
    fn hash_shard_roundtrips() {
        let shard = HashShard {
            entries: vec![
                HashEntry {
                    key: 0xAABB,
                    blob: vec![9, 9, 9],
                },
                HashEntry {
                    key: 0xCCDD,
                    blob: vec![7],
                },
            ],
        };
        let bytes = rkyv::to_bytes::<_, 256>(&shard).expect("serialize");
        let d = try_decode(&bytes[..]).expect("recognized");
        assert!(d.format.contains("hash-keyed"));
        assert_eq!(d.records.len(), 2);
        assert_eq!(d.records[0].value, vec![9, 9, 9]);
    }

    #[test]
    fn delete_record_removes_and_reserializes() {
        let mut entries = HashMap::new();
        for name in ["/a.sh", "/b.sh"] {
            entries.insert(
                name.to_string(),
                ScriptEntry {
                    mtime_secs: 1,
                    mtime_nsecs: 2,
                    binary_mtime_at_cache: 3,
                    cached_at_secs: 4,
                    chunk_blob: vec![0xaa],
                },
            );
        }
        let shard = ScriptShard {
            header: ShardHeader {
                magic: ZSHRS_MAGIC,
                format_version: 1,
                zshrs_version: "1.0".into(),
                pointer_width: 8,
                built_at_secs: 0,
            },
            entries,
        };
        let bytes = rkyv::to_bytes::<_, 256>(&shard).expect("serialize");

        // Delete one key; the re-serialized shard must still decode, minus it.
        let new_bytes = delete_record(&bytes, FormatKind::Script, "/a.sh").expect("delete");
        let d = try_decode(&new_bytes).expect("still valid");
        assert_eq!(d.records.len(), 1);
        assert_eq!(d.records[0].key, "/b.sh");
        // Header is preserved.
        assert!(d.header.iter().any(|(k, v)| k == "version" && v == "1.0"));
    }

    #[test]
    fn python_delete_removes_exactly_one_of_duplicate_sources() {
        // Two records share the same display source but have distinct u64 keys.
        let shard = PyShard {
            entries: vec![
                PyEntry {
                    key: 1,
                    verify: 0,
                    source: "<string>".into(),
                    blob: vec![0xa1],
                },
                PyEntry {
                    key: 2,
                    verify: 0,
                    source: "<string>".into(),
                    blob: vec![0xa2],
                },
            ],
        };
        let bytes = rkyv::to_bytes::<_, 256>(&shard).expect("serialize");
        let d = try_decode(&bytes).expect("recognized");
        assert_eq!(d.records.len(), 2);
        assert!(d.records.iter().all(|r| r.key == "<string>"));

        // Delete only the first by its unique del_key — the second must survive.
        let target = d.records.iter().find(|r| r.value == vec![0xa1]).unwrap();
        let new_bytes = delete_record(&bytes, FormatKind::Python, &target.del_key).unwrap();
        let after = try_decode(&new_bytes).expect("valid");
        assert_eq!(after.records.len(), 1, "exactly one removed");
        assert_eq!(after.records[0].value, vec![0xa2]);
    }

    #[test]
    fn full_crud_roundtrip_on_map_format() {
        // Start from an empty-ish script shard, then Create, Update, Rename,
        // Delete — each re-serialization must stay a valid, decodable shard.
        let shard = ScriptShard {
            header: ShardHeader {
                magic: ZSHRS_MAGIC,
                format_version: 1,
                zshrs_version: "1.0".into(),
                pointer_width: 8,
                built_at_secs: 0,
            },
            entries: HashMap::new(),
        };
        let mut bytes = rkyv::to_bytes::<_, 256>(&shard).unwrap().into_vec();

        // Create
        bytes = add_record(&bytes, FormatKind::Script, "/x.sh", vec![1, 2, 3]).unwrap();
        let d = try_decode(&bytes).unwrap();
        assert_eq!(d.records.len(), 1);
        assert_eq!(d.records[0].value, vec![1, 2, 3]);

        // Update value
        bytes = set_value(&bytes, FormatKind::Script, "/x.sh", vec![9, 9]).unwrap();
        let d = try_decode(&bytes).unwrap();
        assert_eq!(d.records[0].value, vec![9, 9]);

        // Rename key
        bytes = rename_record(&bytes, FormatKind::Script, "/x.sh", "/y.sh").unwrap();
        let d = try_decode(&bytes).unwrap();
        assert_eq!(d.records[0].key, "/y.sh");
        assert_eq!(d.records[0].value, vec![9, 9]);

        // Delete
        bytes = delete_record(&bytes, FormatKind::Script, "/y.sh").unwrap();
        assert_eq!(try_decode(&bytes).unwrap().records.len(), 0);
    }

    #[test]
    fn hash_delete_by_hex_key() {
        let shard = HashShard {
            entries: vec![
                HashEntry {
                    key: 0x10,
                    blob: vec![1],
                },
                HashEntry {
                    key: 0x20,
                    blob: vec![2],
                },
            ],
        };
        let bytes = rkyv::to_bytes::<_, 256>(&shard).expect("serialize");
        let key = format!("{:#018x}", 0x10u64);
        let new_bytes = delete_record(&bytes, FormatKind::Hash, &key).expect("delete");
        let d = try_decode(&new_bytes).expect("valid");
        assert_eq!(d.records.len(), 1);
    }

    #[test]
    fn garbage_is_not_recognized() {
        assert!(try_decode(&[0u8; 64]).is_none());
        assert!(try_decode(b"not an archive at all, just plain text bytes").is_none());
    }
}
