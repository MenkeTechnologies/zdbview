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
const TEXRS_MAGIC: u32 = 0x5445_5852; // "TEXR"

/// zshrs autoload cache magic ("ZRAL" little-endian).
const AUTOLOAD_MAGIC: u32 = 0x5A52_414C;
/// elisprs heap-image cache magic ("ELSP").
const ELISP_MAGIC: u32 = 0x454C_5350;
/// zshrs canonical-state shard magic ("ZSHS") — the daemon's per-source images
/// under `~/.zshrs/images/`, one per zinit plugin/snippet plus the system shard.
/// These are the most numerous archives the shell writes, and unlike the script
/// caches their root is not one flat map but ~20 sections (see [`CanonicalShard`]).
const CANONICAL_MAGIC: u32 = 0x5A53_4853;

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

// ---- Family D: the zshrs canonical shard (ZSHS) -----------------------------
// Faithful copy of `zshrs/daemon/shard.rs`'s `ShardHeader` + `CanonicalShard`,
// which the daemon writes with `rkyv::to_bytes::<_, 4096>` under the same rkyv
// 0.7 + `archive_le` + `size_32` pins this crate uses. Field ORDER and types
// must track that file; a drift shows up as failed validation, never as a
// silently wrong decode.
//
// This is the shell's whole captured state for one source root — aliases,
// functions, options, bindings, paths — so it is many small sections rather than
// one entry map, which is why it gets its own record projection below.

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct CanonicalHeader {
    magic: u32,
    format_version: u32,
    generation: u64,
    built_at_ns: u64,
    slug: String,
    source_root: String,
    entry_count: u32,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct CanonicalShard {
    header: CanonicalHeader,
    aliases: HashMap<String, String>,
    global_aliases: HashMap<String, String>,
    suffix_aliases: HashMap<String, String>,
    functions: HashMap<String, String>,
    autoload_functions: HashMap<String, String>,
    setopts: Vec<String>,
    unsetopts: Vec<String>,
    bindkeys: HashMap<String, String>,
    named_dirs: HashMap<String, String>,
    compdef: HashMap<String, String>,
    zstyle: Vec<(String, String)>,
    zmodload: Vec<String>,
    env_exports: HashMap<String, String>,
    params: HashMap<String, String>,
    path: Vec<String>,
    fpath: Vec<String>,
    manpath: Vec<String>,
    plugins: Vec<(String, String)>,
    sourced_files: Vec<String>,
    extras: HashMap<String, HashMap<String, String>>,
}

/// The daemon's OTHER ZSHS type: `zshrs/daemon/shard.rs`'s `Shard`, a flat
/// blob map under the same header and the same magic. The `*-system.rkyv` image
/// is written as this, not as a [`CanonicalShard`], so the ZSHS arm has to try
/// both — exactly like the two STRY layouts above.
#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
struct SystemShard {
    header: CanonicalHeader,
    entries: HashMap<String, Vec<u8>>,
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
    Canonical,
    System,
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

/// Every magic zdbview ships with its display name — the one place they are
/// listed, used both by [`try_decode`] and by the startup scan's cheap header
/// sniff. Producers outside this list register their own tag through
/// [`crate::magics`]; this table is the built-in half of that registry.
pub const BUILTIN_MAGICS: &[(u32, &str)] = &[
    (ZSHRS_MAGIC, "zshrs script cache (ZRSC)"),
    (AWKRS_MAGIC, "awkrs script cache (AWKR)"),
    (VIMLRS_MAGIC, "vimlrs script cache (VIML)"),
    (TEXRS_MAGIC, "texrs bytecode cache (TEXR)"),
    (STRYKE_MAGIC, "strykelang script cache (STRY)"),
    (AUTOLOAD_MAGIC, "zshrs autoload cache (ZRAL)"),
    (ELISP_MAGIC, "elisprs heap-image cache (ELSP)"),
    (CANONICAL_MAGIC, "zshrs canonical shard (ZSHS)"),
];

/// Display name for a known magic, from the registry (built-ins plus whatever
/// the user registered).
fn magic_name(magic: u32) -> &'static str {
    crate::magics::name_of(magic).unwrap_or("rkyv archive")
}

/// The format named by whichever known magic appears in `bytes`, if any. This is
/// a *sniff*, not a decode: it says the bytes carry a producer's header, which is
/// enough for the scan to offer the file. Opening it still validates properly
/// through [`try_decode`].
pub fn magic_in(bytes: &[u8]) -> Option<&'static str> {
    crate::magics::all()
        .iter()
        .find(|(m, _)| contains_u32_le(bytes, *m))
        .map(|(_, n)| *n)
}

/// The known format whose display name is `name`, so a cached scan result can be
/// restored to the same `&'static str` it was saved from.
pub fn magic_label(name: &str) -> Option<&'static str> {
    crate::magics::by_name(name)
}

/// Detect the archive format and decode it to key/value records. Magic-bearing
/// formats are matched by their header magic; the header-less hash-keyed shards
/// are attempted last, gated by rkyv validation. `None` → structural fallback.
pub fn try_decode(bytes: &[u8]) -> Option<Decoded> {
    // Family A — shared script-cache template, one type, per-host magic + name.
    for magic in [ZSHRS_MAGIC, AWKRS_MAGIC, VIMLRS_MAGIC, TEXRS_MAGIC] {
        if contains_u32_le(bytes, magic) {
            if let Ok(s) = rkyv::check_archived_root::<ScriptShard>(bytes) {
                if u32::from(s.header.magic) == magic {
                    return Some(decode_script(s, magic_name(magic)));
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

    // Family D — the zshrs ZSHS images. Two layouts share the magic: the
    // per-source canonical shard (many sections) and the flat blob `Shard` the
    // system image uses. Try the richer one first, then the flat one.
    if contains_u32_le(bytes, CANONICAL_MAGIC) {
        if let Ok(s) = rkyv::check_archived_root::<CanonicalShard>(bytes) {
            if u32::from(s.header.magic) == CANONICAL_MAGIC {
                return Some(decode_canonical(s));
            }
        }
        if let Ok(s) = rkyv::check_archived_root::<SystemShard>(bytes) {
            if u32::from(s.header.magic) == CANONICAL_MAGIC {
                return Some(decode_system(s));
            }
        }
    }

    // The recorder writes a canonical shard with the magic field left at ZERO
    // (`*-recorder.rkyv`), so the pre-filter above never sees it. Validating the
    // full type is itself a strong check — every field, every relative pointer —
    // so an unstamped shard is accepted when it validates AND carries a slug,
    // rather than being left on the structural view for a missing u32.
    if let Ok(s) = rkyv::check_archived_root::<CanonicalShard>(bytes) {
        let magic = u32::from(s.header.magic);
        if (magic == 0 || magic == CANONICAL_MAGIC) && !s.header.slug.as_str().is_empty() {
            return Some(decode_canonical(s));
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
        FormatKind::Canonical => canonical_edit(bytes, del_key, CanonicalOp::Delete),
        FormatKind::System => edit_shard::<SystemShard, _>(bytes, |s| {
            s.entries.remove(del_key);
        }),
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
        FormatKind::Canonical => {
            canonical_edit(bytes, del_key, CanonicalOp::Set(canonical_text(value)?))
        }
        // System-shard values are opaque blobs, so they take the bytes verbatim.
        FormatKind::System => edit_shard::<SystemShard, _>(bytes, move |s| {
            if let Some(e) = s.entries.get_mut(del_key) {
                *e = value;
            }
        }),
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
        FormatKind::System => {
            let key = key.to_string();
            edit_shard::<SystemShard, _>(bytes, move |s| {
                s.entries.insert(key, value);
            })
        }
        // A canonical address names its slot, so an add is "create this slot",
        // then set it when a value came with it.
        FormatKind::Canonical => {
            let created = canonical_edit(bytes, key, CanonicalOp::Add)?;
            if value.is_empty() {
                Ok(created)
            } else {
                canonical_edit(&created, key, CanonicalOp::Set(canonical_text(value)?))
            }
        }
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
        FormatKind::Canonical => canonical_edit(bytes, del_key, CanonicalOp::Rename(new_key)),
        FormatKind::System => edit_shard::<SystemShard, _>(bytes, move |s| {
            if let Some(v) = s.entries.remove(del_key) {
                s.entries.insert(new_key, v);
            }
        }),
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

/// Serialize a header-less hash-keyed shard holding `entries`. Test-only
/// fixture, for the formats whose key is a content hash rather than a name.
#[cfg(test)]
pub(crate) fn test_hash_shard_bytes(entries: &[(u64, &[u8])]) -> Vec<u8> {
    let shard = HashShard {
        entries: entries
            .iter()
            .map(|(key, blob)| HashEntry {
                key: *key,
                blob: blob.to_vec(),
            })
            .collect(),
    };
    rkyv::to_bytes::<_, 4096>(&shard).unwrap().to_vec()
}

/// Serialize a zshrs script shard holding `entries`. Test-only fixture.
#[cfg(test)]
pub(crate) fn test_script_shard_bytes_many(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut map = HashMap::new();
    for (key, blob) in entries {
        map.insert(
            key.clone(),
            ScriptEntry {
                mtime_secs: 1,
                mtime_nsecs: 2,
                binary_mtime_at_cache: 3,
                cached_at_secs: 4,
                chunk_blob: blob.clone(),
            },
        );
    }
    let shard = ScriptShard {
        header: ShardHeader {
            magic: ZSHRS_MAGIC,
            format_version: 1,
            zshrs_version: "9.9.9".into(),
            pointer_width: 8,
            built_at_secs: 5,
        },
        entries: map,
    };
    rkyv::to_bytes::<_, 256>(&shard)
        .expect("serialize test shard")
        .to_vec()
}

/// Serialize a one-entry zshrs script shard. Test-only fixture, kept next to the
/// archive types so tests elsewhere in the crate don't need them made public.
#[cfg(test)]
pub(crate) fn test_script_shard_bytes(key: &str, blob: &[u8]) -> Vec<u8> {
    let mut entries = HashMap::new();
    entries.insert(
        key.to_string(),
        ScriptEntry {
            mtime_secs: 1,
            mtime_nsecs: 2,
            binary_mtime_at_cache: 3,
            cached_at_secs: 4,
            chunk_blob: blob.to_vec(),
        },
    );
    let shard = ScriptShard {
        header: ShardHeader {
            magic: ZSHRS_MAGIC,
            format_version: 1,
            zshrs_version: "9.9.9".into(),
            pointer_width: 8,
            built_at_secs: 5,
        },
        entries,
    };
    rkyv::to_bytes::<_, 256>(&shard)
        .expect("serialize test shard")
        .to_vec()
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

    /// The README's promise, in its strongest form: an edited shard is not
    /// merely readable by the producing host, it is byte for byte the shard that
    /// host would have written itself. Anything less and a cache the shell
    /// mmaps could differ from one it built.
    #[test]
    fn an_edit_writes_exactly_what_the_producer_would_have_written() {
        let entry = |k: &str, v: &[u8]| (k.to_string(), v.to_vec());

        // set_value: the same shard with one blob replaced.
        let before =
            test_script_shard_bytes_many(&[entry("/tmp/a.sh", b"one"), entry("/tmp/b.sh", b"two")]);
        let expected =
            test_script_shard_bytes_many(&[entry("/tmp/a.sh", b"ONE"), entry("/tmp/b.sh", b"two")]);
        let edited = set_value(&before, FormatKind::Script, "/tmp/a.sh", b"ONE".to_vec()).unwrap();
        assert_eq!(
            edited, expected,
            "an edited shard is the shard, not a copy of it"
        );

        // A no-op edit is a no-op on the bytes too, so writing back a record
        // nobody changed cannot invalidate a cache.
        let same = set_value(&before, FormatKind::Script, "/tmp/a.sh", b"one".to_vec()).unwrap();
        assert_eq!(same, before);

        // delete: the shard as if the record had never been written.
        let three = test_script_shard_bytes_many(&[
            entry("/tmp/a.sh", b"one"),
            entry("/tmp/b.sh", b"two"),
            entry("/tmp/c.sh", b"three"),
        ]);
        let without_b = test_script_shard_bytes_many(&[
            entry("/tmp/a.sh", b"one"),
            entry("/tmp/c.sh", b"three"),
        ]);
        assert_eq!(
            delete_record(&three, FormatKind::Script, "/tmp/b.sh").unwrap(),
            without_b
        );

        // rename: the same entry under a different key.
        let renamed =
            test_script_shard_bytes_many(&[entry("/tmp/z.sh", b"one"), entry("/tmp/b.sh", b"two")]);
        assert_eq!(
            rename_record(&before, FormatKind::Script, "/tmp/a.sh", "/tmp/z.sh").unwrap(),
            renamed
        );
    }

    /// Editing is a decode-mutate-encode round trip, so it has to survive being
    /// repeated: a shard edited ten times must still be one the decoder reads,
    /// with only the last value in it.
    #[test]
    fn repeated_edits_do_not_accumulate_anything() {
        let mut bytes = test_script_shard_bytes_many(&[
            ("/tmp/a.sh".to_string(), b"v0".to_vec()),
            ("/tmp/b.sh".to_string(), b"keep".to_vec()),
        ]);
        for i in 1..=10 {
            bytes = set_value(
                &bytes,
                FormatKind::Script,
                "/tmp/a.sh",
                format!("v{i}").into_bytes(),
            )
            .unwrap();
        }
        // The shard is rebuilt each time, never appended to, so ten edits leave
        // exactly the shard a single write of the last value would have.
        assert_eq!(
            bytes,
            test_script_shard_bytes_many(&[
                ("/tmp/a.sh".to_string(), b"v10".to_vec()),
                ("/tmp/b.sh".to_string(), b"keep".to_vec()),
            ]),
            "ten edits leave no residue"
        );

        let decoded = try_decode(&bytes).expect("still a shard");
        assert_eq!(decoded.records.len(), 2);
        let a = decoded
            .records
            .iter()
            .find(|r| r.key == "/tmp/a.sh")
            .expect("the edited record");
        assert_eq!(a.value, b"v10", "only the last write survives");
        let b = decoded
            .records
            .iter()
            .find(|r| r.key == "/tmp/b.sh")
            .expect("the untouched record");
        assert_eq!(b.value, b"keep", "and the one nobody touched is untouched");
    }

    #[test]
    fn garbage_is_not_recognized() {
        assert!(try_decode(&[0u8; 64]).is_none());
        assert!(try_decode(b"not an archive at all, just plain text bytes").is_none());
    }
}

/// Project a canonical shard into `section/key` records.
///
/// The shard is ~20 sections of three shapes, and each becomes records the same
/// way so one flat list can carry all of them:
///
///   * maps (`aliases`, `functions`, `params`, …) → `section/<key>`
///   * lists (`path`, `setopts`, `sourced_files`, …) → `section[<index>]`
///   * pair lists (`zstyle`, `plugins`) → `section[<index>]` with the pair joined
///   * `extras` (map of maps) → `extras/<subsystem>/<key>`
///
/// The record's `del_key` is that same address, which is what the edit path
/// parses back into (section, key-or-index) — see [`canonical_edit`].
fn decode_canonical(s: &ArchivedCanonicalShard) -> Decoded {
    let mut records: Vec<KvRecord> = Vec::new();

    let mut push = |key: String, text: &str, kind: &str| {
        records.push(KvRecord {
            key: key.clone(),
            del_key: key,
            value: text.as_bytes().to_vec(),
            fields: vec![
                ("section".into(), kind.to_string()),
                ("len".into(), text.len().to_string()),
            ],
        });
    };

    macro_rules! map_section {
        ($field:ident) => {
            for (k, v) in s.$field.iter() {
                push(
                    format!("{}/{}", stringify!($field), k.as_str()),
                    v.as_str(),
                    stringify!($field),
                );
            }
        };
    }
    macro_rules! list_section {
        ($field:ident) => {
            for (i, v) in s.$field.iter().enumerate() {
                push(
                    format!("{}[{i}]", stringify!($field)),
                    v.as_str(),
                    stringify!($field),
                );
            }
        };
    }
    macro_rules! pair_section {
        ($field:ident) => {
            for (i, pair) in s.$field.iter().enumerate() {
                push(
                    format!("{}[{i}]", stringify!($field)),
                    &format!("{} {}", pair.0.as_str(), pair.1.as_str()),
                    stringify!($field),
                );
            }
        };
    }

    map_section!(aliases);
    map_section!(global_aliases);
    map_section!(suffix_aliases);
    map_section!(functions);
    map_section!(autoload_functions);
    map_section!(bindkeys);
    map_section!(named_dirs);
    map_section!(compdef);
    map_section!(env_exports);
    map_section!(params);
    list_section!(setopts);
    list_section!(unsetopts);
    list_section!(zmodload);
    list_section!(path);
    list_section!(fpath);
    list_section!(manpath);
    list_section!(sourced_files);
    pair_section!(zstyle);
    pair_section!(plugins);
    for (sub, entries) in s.extras.iter() {
        for (k, v) in entries.iter() {
            push(
                format!("extras/{}/{}", sub.as_str(), k.as_str()),
                v.as_str(),
                "extras",
            );
        }
    }

    records.sort_by(|a, b| a.key.cmp(&b.key));
    Decoded {
        format: "zshrs canonical shard (ZSHS)".into(),
        kind: FormatKind::Canonical,
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
                "generation".into(),
                u64::from(s.header.generation).to_string(),
            ),
            (
                "built_at_ns".into(),
                u64::from(s.header.built_at_ns).to_string(),
            ),
            ("slug".into(), s.header.slug.as_str().to_string()),
            (
                "source_root".into(),
                s.header.source_root.as_str().to_string(),
            ),
            (
                "entry_count".into(),
                u32::from(s.header.entry_count).to_string(),
            ),
        ],
        records,
    }
}

/// What a canonical-shard edit does to the addressed slot.
enum CanonicalOp {
    Set(String),
    Delete,
    /// Re-key a map entry (lists are positional, so they refuse this).
    Rename(String),
    /// Insert a new empty slot at the address.
    Add,
}

/// Apply `op` to the slot named by `addr` in a canonical shard.
///
/// Addresses are exactly what [`decode_canonical`] emits: `section/<key>` for the
/// maps, `section[<index>]` for the lists and pair-lists, `extras/<sub>/<key>`
/// for the nested map. An address naming a section that does not exist, or an
/// index past the end, is refused rather than silently creating something the
/// shell would not recognize.
fn canonical_edit(bytes: &[u8], addr: &str, op: CanonicalOp) -> Result<Vec<u8>, String> {
    // `extras/<sub>/<key>` has two separators, so it is matched before the
    // one-separator map form.
    if let Some(rest) = addr.strip_prefix("extras/") {
        let (sub, key) = rest
            .split_once('/')
            .ok_or("extras address needs <sub>/<key>")?;
        let (sub, key) = (sub.to_string(), key.to_string());
        return edit_shard::<CanonicalShard, _>(bytes, move |s| match op {
            CanonicalOp::Set(v) => {
                s.extras.entry(sub).or_default().insert(key, v);
            }
            CanonicalOp::Add => {
                s.extras.entry(sub).or_default().entry(key).or_default();
            }
            CanonicalOp::Delete => {
                if let Some(m) = s.extras.get_mut(&sub) {
                    m.remove(&key);
                }
            }
            CanonicalOp::Rename(new) => {
                // The new name may be typed bare or as a full address; anything
                // naming a different subsystem is refused rather than moved.
                let prefix = format!("extras/{sub}/");
                let new = new
                    .strip_prefix(prefix.as_str())
                    .unwrap_or(new.as_str())
                    .to_string();
                if new.contains('/') {
                    return;
                }
                if let Some(m) = s.extras.get_mut(&sub) {
                    if let Some(v) = m.remove(&key) {
                        m.insert(new, v);
                    }
                }
            }
        });
    }

    if let Some((section, key)) = addr.split_once('/') {
        let (section, key) = (section.to_string(), key.to_string());
        return edit_shard::<CanonicalShard, _>(bytes, move |s| {
            let map = match section.as_str() {
                "aliases" => &mut s.aliases,
                "global_aliases" => &mut s.global_aliases,
                "suffix_aliases" => &mut s.suffix_aliases,
                "functions" => &mut s.functions,
                "autoload_functions" => &mut s.autoload_functions,
                "bindkeys" => &mut s.bindkeys,
                "named_dirs" => &mut s.named_dirs,
                "compdef" => &mut s.compdef,
                "env_exports" => &mut s.env_exports,
                "params" => &mut s.params,
                _ => return,
            };
            match op {
                CanonicalOp::Set(v) => {
                    map.insert(key, v);
                }
                CanonicalOp::Add => {
                    map.entry(key).or_default();
                }
                CanonicalOp::Delete => {
                    map.remove(&key);
                }
                CanonicalOp::Rename(new) => {
                    // Accept `newkey` or `section/newkey`; a different section is
                    // refused, since moving an entry across sections would change
                    // what the shell does with it.
                    let prefix = format!("{section}/");
                    let new = new
                        .strip_prefix(prefix.as_str())
                        .unwrap_or(new.as_str())
                        .to_string();
                    if new.contains('/') {
                        return;
                    }
                    if let Some(v) = map.remove(&key) {
                        map.insert(new, v);
                    }
                }
            }
        });
    }

    // `section[index]`
    let (section, idx) = addr
        .strip_suffix(']')
        .and_then(|a| a.split_once('['))
        .ok_or_else(|| format!("unknown canonical address: {addr}"))?;
    let idx: usize = idx.parse().map_err(|_| format!("bad index in {addr}"))?;
    let section = section.to_string();
    edit_shard::<CanonicalShard, _>(bytes, move |s| {
        // Pair lists carry two strings per row; the record renders them
        // space-joined, so a Set splits on the first space.
        if let Some(pairs) = match section.as_str() {
            "zstyle" => Some(&mut s.zstyle),
            "plugins" => Some(&mut s.plugins),
            _ => None,
        } {
            match op {
                CanonicalOp::Set(v) => {
                    if let Some(slot) = pairs.get_mut(idx) {
                        let (a, b) = v.split_once(' ').unwrap_or((v.as_str(), ""));
                        *slot = (a.to_string(), b.to_string());
                    }
                }
                CanonicalOp::Add => pairs.push((String::new(), String::new())),
                CanonicalOp::Delete => {
                    if idx < pairs.len() {
                        pairs.remove(idx);
                    }
                }
                CanonicalOp::Rename(_) => {}
            }
            return;
        }
        let list = match section.as_str() {
            "setopts" => &mut s.setopts,
            "unsetopts" => &mut s.unsetopts,
            "zmodload" => &mut s.zmodload,
            "path" => &mut s.path,
            "fpath" => &mut s.fpath,
            "manpath" => &mut s.manpath,
            "sourced_files" => &mut s.sourced_files,
            _ => return,
        };
        match op {
            CanonicalOp::Set(v) => {
                if let Some(slot) = list.get_mut(idx) {
                    *slot = v;
                }
            }
            CanonicalOp::Add => list.push(String::new()),
            CanonicalOp::Delete => {
                if idx < list.len() {
                    list.remove(idx);
                }
            }
            CanonicalOp::Rename(_) => {}
        }
    })
}

/// A canonical shard's values are text, so an edit arrives as UTF-8 (the hex
/// entry form still works — it is decoded to bytes before this point).
fn canonical_text(value: Vec<u8>) -> Result<String, String> {
    String::from_utf8(value).map_err(|_| "canonical shard values are text, not binary".to_string())
}

#[cfg(test)]
mod canonical_tests {
    use super::*;

    /// A shard shaped like the daemon's: one entry in each of the three section
    /// kinds plus a nested extras subsystem.
    fn shard() -> CanonicalShard {
        let mut aliases = HashMap::new();
        aliases.insert("ll".to_string(), "ls -la".to_string());
        let mut extras = HashMap::new();
        let mut sub = HashMap::new();
        sub.insert("_git".to_string(), "#compdef git".to_string());
        extras.insert("autoload_completion".to_string(), sub);
        CanonicalShard {
            header: CanonicalHeader {
                magic: CANONICAL_MAGIC,
                format_version: 1,
                generation: 7,
                built_at_ns: 42,
                slug: "zinit-plugin-example".into(),
                source_root: "/tmp/example".into(),
                entry_count: 3,
            },
            aliases,
            global_aliases: HashMap::new(),
            suffix_aliases: HashMap::new(),
            functions: HashMap::new(),
            autoload_functions: HashMap::new(),
            setopts: vec!["extended_glob".into()],
            unsetopts: Vec::new(),
            bindkeys: HashMap::new(),
            named_dirs: HashMap::new(),
            compdef: HashMap::new(),
            zstyle: vec![(":completion:*".into(), "menu select".into())],
            zmodload: Vec::new(),
            env_exports: HashMap::new(),
            params: HashMap::new(),
            path: vec!["/usr/bin".into()],
            fpath: Vec::new(),
            manpath: Vec::new(),
            plugins: Vec::new(),
            sourced_files: Vec::new(),
            extras,
        }
    }

    fn bytes() -> Vec<u8> {
        rkyv::to_bytes::<_, 4096>(&shard())
            .expect("serialize")
            .into_vec()
    }

    #[test]
    fn the_zshs_magic_is_recognized_and_projected_into_records() {
        let d = try_decode(&bytes()).expect("recognized");
        assert_eq!(d.format, "zshrs canonical shard (ZSHS)");
        assert!(matches!(d.kind, FormatKind::Canonical));

        let keys: Vec<&str> = d.records.iter().map(|r| r.key.as_str()).collect();
        assert!(
            keys.contains(&"aliases/ll"),
            "map entries are section/key: {keys:?}"
        );
        assert!(
            keys.contains(&"path[0]"),
            "list entries are section[index]: {keys:?}"
        );
        assert!(
            keys.contains(&"zstyle[0]"),
            "pair lists are indexed too: {keys:?}"
        );
        assert!(
            keys.contains(&"extras/autoload_completion/_git"),
            "extras nest one level deeper: {keys:?}"
        );

        let alias = d.records.iter().find(|r| r.key == "aliases/ll").unwrap();
        assert_eq!(alias.value, b"ls -la");
        assert!(d
            .header
            .iter()
            .any(|(k, v)| k == "slug" && v == "zinit-plugin-example"));
    }

    #[test]
    fn a_map_entry_round_trips_through_edit_and_delete() {
        let edited = set_value(
            &bytes(),
            FormatKind::Canonical,
            "aliases/ll",
            b"ls -A".to_vec(),
        )
        .expect("set");
        let d = try_decode(&edited).expect("still valid");
        let alias = d.records.iter().find(|r| r.key == "aliases/ll").unwrap();
        assert_eq!(alias.value, b"ls -A", "the shell reads the new value");

        let renamed = rename_record(&edited, FormatKind::Canonical, "aliases/ll", "aliases/l")
            .expect("rename");
        let d = try_decode(&renamed).expect("valid");
        assert!(d.records.iter().any(|r| r.key == "aliases/l"));
        assert!(!d.records.iter().any(|r| r.key == "aliases/ll"));

        let deleted = delete_record(&renamed, FormatKind::Canonical, "aliases/l").expect("delete");
        let d = try_decode(&deleted).expect("valid");
        assert!(!d.records.iter().any(|r| r.key.starts_with("aliases/")));
    }

    #[test]
    fn a_list_slot_is_addressed_by_index() {
        let edited = set_value(
            &bytes(),
            FormatKind::Canonical,
            "path[0]",
            b"/opt/bin".to_vec(),
        )
        .expect("set");
        let d = try_decode(&edited).expect("valid");
        let row = d.records.iter().find(|r| r.key == "path[0]").unwrap();
        assert_eq!(row.value, b"/opt/bin");

        let deleted = delete_record(&edited, FormatKind::Canonical, "path[0]").expect("delete");
        let d = try_decode(&deleted).expect("valid");
        assert!(
            !d.records.iter().any(|r| r.key.starts_with("path[")),
            "the row is gone"
        );
    }

    #[test]
    fn an_unknown_section_is_refused_rather_than_inventing_state() {
        let before = bytes();
        let after = set_value(&before, FormatKind::Canonical, "nosuch/key", b"x".to_vec())
            .expect("re-serializes");
        let d = try_decode(&after).expect("valid");
        assert!(
            !d.records.iter().any(|r| r.key.starts_with("nosuch")),
            "an address the shell has no section for must not create one"
        );
    }
}

/// Project the flat ZSHS `Shard` (the `*-system.rkyv` image): one record per
/// entry, the value being the raw blob the shell stored under that key.
fn decode_system(s: &ArchivedSystemShard) -> Decoded {
    let mut records: Vec<KvRecord> = s
        .entries
        .iter()
        .map(|(k, v)| KvRecord {
            key: k.as_str().to_string(),
            del_key: k.as_str().to_string(),
            value: v.as_slice().to_vec(),
            fields: vec![("blob_len".into(), v.len().to_string())],
        })
        .collect();
    records.sort_by(|a, b| a.key.cmp(&b.key));
    Decoded {
        format: "zshrs system shard (ZSHS)".into(),
        kind: FormatKind::System,
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
                "generation".into(),
                u64::from(s.header.generation).to_string(),
            ),
            (
                "built_at_ns".into(),
                u64::from(s.header.built_at_ns).to_string(),
            ),
            ("slug".into(), s.header.slug.as_str().to_string()),
            (
                "source_root".into(),
                s.header.source_root.as_str().to_string(),
            ),
            (
                "entry_count".into(),
                u32::from(s.header.entry_count).to_string(),
            ),
        ],
        records,
    }
}

#[cfg(test)]
mod system_shard_tests {
    use super::*;

    /// The daemon's OTHER ZSHS layout: a flat blob map, which is what the
    /// `*-system.rkyv` image is written as.
    fn system_bytes() -> Vec<u8> {
        let mut entries: HashMap<String, Vec<u8>> = HashMap::new();
        entries.insert("cmd:ls".into(), b"/bin/ls".to_vec());
        entries.insert("cmd:grep".into(), vec![0x00, 0xff, 0x10]);
        let shard = SystemShard {
            header: CanonicalHeader {
                magic: CANONICAL_MAGIC,
                format_version: 1,
                generation: 3,
                built_at_ns: 9,
                slug: "system".into(),
                source_root: "/".into(),
                entry_count: 2,
            },
            entries,
        };
        rkyv::to_bytes::<_, 4096>(&shard)
            .expect("serialize")
            .into_vec()
    }

    /// Both ZSHS layouts share a magic, so the arm must try the canonical shard
    /// first and fall through to the flat one instead of giving up — which is
    /// what left the 43 MB system image on the structural view.
    #[test]
    fn the_flat_system_layout_is_reached_through_the_same_magic() {
        let d = try_decode(&system_bytes()).expect("recognized");
        assert_eq!(d.format, "zshrs system shard (ZSHS)");
        assert!(matches!(d.kind, FormatKind::System));
        assert_eq!(d.records.len(), 2);
        let row = d.records.iter().find(|r| r.key == "cmd:ls").unwrap();
        assert_eq!(row.value, b"/bin/ls");
    }

    #[test]
    fn system_values_are_binary_and_round_trip() {
        // A blob, not text: the edit path must take the bytes verbatim.
        let edited = set_value(
            &system_bytes(),
            FormatKind::System,
            "cmd:grep",
            vec![1, 2, 3],
        )
        .expect("set");
        let d = try_decode(&edited).expect("valid");
        assert_eq!(
            d.records
                .iter()
                .find(|r| r.key == "cmd:grep")
                .unwrap()
                .value,
            vec![1, 2, 3]
        );

        let renamed =
            rename_record(&edited, FormatKind::System, "cmd:grep", "cmd:rg").expect("rename");
        let d = try_decode(&renamed).expect("valid");
        assert!(d.records.iter().any(|r| r.key == "cmd:rg"));

        let deleted = delete_record(&renamed, FormatKind::System, "cmd:rg").expect("delete");
        let d = try_decode(&deleted).expect("valid");
        assert_eq!(d.records.len(), 1);
    }

    /// The recorder writes a canonical shard with the magic left at zero. It
    /// still validates as the full type, so it decodes rather than falling back.
    #[test]
    fn an_unstamped_canonical_shard_is_still_decoded() {
        let mut aliases = HashMap::new();
        aliases.insert("g".to_string(), "git".to_string());
        let shard = CanonicalShard {
            header: CanonicalHeader {
                magic: 0,
                format_version: 1,
                generation: 1,
                built_at_ns: 1,
                slug: "recorder".into(),
                source_root: "/tmp".into(),
                entry_count: 1,
            },
            aliases,
            global_aliases: HashMap::new(),
            suffix_aliases: HashMap::new(),
            functions: HashMap::new(),
            autoload_functions: HashMap::new(),
            setopts: Vec::new(),
            unsetopts: Vec::new(),
            bindkeys: HashMap::new(),
            named_dirs: HashMap::new(),
            compdef: HashMap::new(),
            zstyle: Vec::new(),
            zmodload: Vec::new(),
            env_exports: HashMap::new(),
            params: HashMap::new(),
            path: Vec::new(),
            fpath: Vec::new(),
            manpath: Vec::new(),
            plugins: Vec::new(),
            sourced_files: Vec::new(),
            extras: HashMap::new(),
        };
        let bytes = rkyv::to_bytes::<_, 4096>(&shard)
            .expect("serialize")
            .into_vec();

        let d = try_decode(&bytes).expect("an unstamped shard still decodes");
        assert!(d
            .header
            .iter()
            .any(|(k, v)| k == "magic" && v == "0x00000000"));
        assert!(d.records.iter().any(|r| r.key == "aliases/g"));
    }
}
