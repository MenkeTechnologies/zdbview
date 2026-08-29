# The rkyv shard header

Version 1 · status: stable · this document is the normative definition.

rkyv archives are not self-describing. The format stores no field names and no
type tags (<https://rkyv.org/format.html>), so an archive on disk cannot say what
it is: decoding one requires already knowing its Rust type. Every producer that
writes an rkyv cache therefore has to solve the same problem — how does a reader
that did not write the file know which type to cast it to?

This is the convention that answers it. It costs one struct at the front of the
archive and makes a shard identifiable by any tool, in any language, without
parsing the payload.

The convention is deliberately small. It is not a container format, it does not
frame or compress the payload, and it takes no position on what a producer
stores. It defines a tag, a version, and enough provenance to refuse a shard
that was written by a mismatched build.

## The header

A conforming archive's root type begins with a header struct:

```rust
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
struct ShardHeader {
    magic: u32,            // producer tag, big-endian ASCII: u32::from_be_bytes(*b"ZRSC")
    format_version: u32,   // this producer's schema version, starting at 1
    producer_version: String, // the writing binary's version, e.g. "0.31.2"
    pointer_width: u32,    // 32 or 64 — the writer's usize width
    built_at_secs: u64,    // unix seconds when the shard was written
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
struct Shard {
    header: ShardHeader,   // first field, always
    entries: /* producer's own type */,
}
```

Serialized with rkyv 0.7 and the `validation`, `archive_le` and `size_32`
features.

That is the whole requirement. Everything below is what the fields mean and how
a reader is expected to behave.

### Field rules

| Field | Rule |
|-------|------|
| `magic` | Four printable ASCII characters read big-endian, so the bytes appear in file order. Unique per producer; see *Registering a tag*. |
| `format_version` | Bumped by the producer whenever the layout of `entries` changes in a way an older reader would misread. Starts at 1. |
| `producer_version` | The writing binary's own version string. Advisory: it explains a mismatch, it does not decide one. |
| `pointer_width` | 32 or 64. A shard whose width differs from the reader's is not portable and must be rejected, not repaired. |
| `built_at_secs` | Unix seconds. Used for staleness decisions, never for correctness. |

Because rkyv stores no names, the contract is **field order and field types**,
not spelling. A producer that calls the third field `schema_key` and gives it a
different meaning is still wire-compatible so long as it is a `String` in that
position — but readers will print it under whatever name they know, so producers
should keep the names above unless there is a reason not to.

A producer that needs more metadata appends fields **after** `built_at_secs`,
never between. Appending changes the layout, so it is a `format_version` bump.

### Reader behaviour

A conforming reader:

1. Locates the four `magic` bytes to guess the type. rkyv writes its root last,
   so the header may sit near the end of a large file; a reader that scans only
   the first N bytes must also scan the tail before concluding "unknown".
2. Validates with `rkyv::check_archived_root` **before** trusting any field. The
   magic scan is a hint; validation is the decision.
3. Compares `magic` and `format_version` against what it can decode, and refuses
   anything else. A mismatch is reported, never guessed around: a wrong cast
   surfaces as failed validation, which is the property this convention exists
   to preserve.
4. Treats an unknown-but-registered tag as a named opaque archive rather than an
   error — it still knows *what* the file is, which is the point of the tag.

## Registering a tag

Tags are four printable ASCII characters, chosen by the producer, and are
first-come. There is no central authority and nothing to apply for: pick a tag
that names your producer, write it in your README next to the magic constant,
and it is yours by publication. Practical advice — use the producer's short
name (`ZRSC`, `LUAR`, `PERL`), keep it upper case, and grep a reader's registry
before claiming one.

Tags known at the time of writing:

| Tag | Producer |
|-----|----------|
| `ZRSC` | zshrs script cache |
| `ZRAL` | zshrs autoload cache |
| `ZSHS` | zshrs canonical state shard |
| `STRY` | strykelang script cache |
| `AWKR` | awkrs script cache |
| `VIML` | vimlrs script cache |
| `ELSP` | elisprs heap image |

A reader must not hard-code that list as the closed set. The tag space belongs
to producers, so a conforming reader lets its user add tags it has never heard
of — which is what makes publication sufficient for registration. zdbview does
this with a plain-text file, `$XDG_CONFIG_HOME/zdbview/magics`:

```text
# tag = display name
LUAR = luars bytecode cache (LUAR)
0x5045_524C = perlrs script cache (PERL)
```

## Header-less archives

Some producers write no header. They are still readable — a reader can attempt a
type and let rkyv validation reject it — but they are not identifiable, they
cannot be versioned, and every reader that supports them pays for it in
try-decode attempts against every candidate type. Stamping a header is four
fields and removes all three costs.

## Conformance checklist

- [ ] The root type's first field is a header with the five fields, in order.
- [ ] `magic` is four printable ASCII characters, big-endian, published in the
      producer's own documentation.
- [ ] `format_version` starts at 1 and is bumped on every layout change.
- [ ] Serialized with rkyv 0.7 + `validation`, `archive_le`, `size_32`.
- [ ] Readers validate before reading, and reject on tag or version mismatch.
- [ ] Readers accept user-registered tags they do not ship.

## Implementations

Producers: zshrs, strykelang, awkrs, vimlrs, elisprs.
Readers: [zdbview](https://github.com/MenkeTechnologies/zdbview) — detects every
tag above, decodes the ones it has types for, and names the rest from its user
registry.
