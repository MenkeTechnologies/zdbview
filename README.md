```
███████╗██████╗ ██████╗ ██╗   ██╗██╗███████╗██╗    ██╗
╚══███╔╝██╔══██╗██╔══██╗██║   ██║██║██╔════╝██║    ██║
  ███╔╝ ██║  ██║██████╔╝██║   ██║██║█████╗  ██║ █╗ ██║
 ███╔╝  ██║  ██║██╔══██╗╚██╗ ██╔╝██║██╔══╝  ██║███╗██║
███████╗██████╔╝██████╔╝ ╚████╔╝ ██║███████╗╚███╔███╔╝
╚══════╝╚═════╝ ╚═════╝   ╚═══╝  ╚═╝╚══════╝ ╚══╝╚══╝
```

<p align="center">
  <a href="https://github.com/MenkeTechnologies/zdbview/actions/workflows/ci.yml"><img src="https://github.com/MenkeTechnologies/zdbview/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/MenkeTechnologies/zdbview/actions/workflows/release.yml"><img src="https://github.com/MenkeTechnologies/zdbview/actions/workflows/release.yml/badge.svg" alt="Release"></a>
  <a href="https://crates.io/crates/zdbview"><img src="https://img.shields.io/crates/v/zdbview.svg" alt="crates.io"></a>
  <a href="https://docs.rs/zdbview"><img src="https://docs.rs/zdbview/badge.svg" alt="docs.rs"></a>
  <a href="https://github.com/MenkeTechnologies/zdbview/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="license"></a>
</p>

<p align="center">
  <code>[ SYSTEM://STORE_INSPECTOR ]</code><br>
  <code>⟦ ONE BINARY, BOTH HALVES OF THE CACHE ⟧</code><br><br>
  <strong>Terminal inspector and CRUD editor for rkyv archives and SQLite databases</strong><br>
  <em>Built in Rust with <a href="https://github.com/ratatui/ratatui">ratatui</a> + <a href="https://github.com/crossterm-rs/crossterm">crossterm</a></em>
</p>

### [`Read the Docs`](https://menketechnologies.github.io/zdbview/) &middot; [`Engineering Report`](https://menketechnologies.github.io/zdbview/report.html)

---

# zdbview

Terminal inspector and CRUD editor for **rkyv archives** and **SQLite databases**.

One binary opens either kind of file. The file type is detected from the SQLite
header magic (authoritative — a `.db` name whose bytes are not a SQLite header is
treated as binary), falling back to the extension only for files too short to
carry a header.

```
zdbview                        # no args → pick from recently opened files
zdbview path/to/file.db        # SQLite → full CRUD
zdbview path/to/archive.rkyv   # rkyv   → full CRUD if recognized, else structural
zdbview --sqlite file          # force SQLite
zdbview --rkyv   file          # force rkyv/binary
```

## Install

```sh
brew tap MenkeTechnologies/menketech
brew install zdbview
```

The formula installs the binary, both man pages and the zsh completion, and is
bumped automatically by this repo's `Release` workflow on every `v*` tag.

From [crates.io](https://crates.io/crates/zdbview) — binary only, no man pages
or completion:

```sh
cargo install zdbview
```

From source:

```sh
git clone https://github.com/MenkeTechnologies/zdbview
cd zdbview && cargo build --release
install -m 755 target/release/zdbview /usr/local/bin/
```

SQLite is compiled in (`rusqlite`'s `bundled` feature), so there is no system
library to install. Tagged releases publish prebuilt binaries for macOS
(arm64 + x86_64) and Linux (glibc + static musl, arm64 + x86_64). Homebrew
covers the glibc targets; the static musl tarballs — for Alpine, distroless and
other non-glibc hosts — are attached to the same GitHub release, with their
sha256 sums listed at the bottom of the formula.

## Recent files (no args)

Every opened file is recorded to `$XDG_CACHE_HOME/zdbview/recent` (or
`~/.cache/zdbview/recent`), most-recent-first. Running `zdbview` with no argument
shows that list as a picker — `j`/`k` to move, `Enter` to open, `q` to quit.

Paths are canonicalized, so the same file reached through different relative
paths or symlinks dedupes to one entry that moves back to the front on re-open.
The list is capped at 50 entries and written via temp-file-plus-rename, so a
concurrent reader never sees a half-written list.

## SQLite — full generic CRUD

SQLite files are self-describing, so every operation works on any database:

- Browse tables (with row counts) and paginated rows.
- `e` — edit the selected cell in place.
- `a` — insert a row using column defaults.
- `d` — delete the selected row (confirm with `y`).
- `:` — run an arbitrary SQL statement.

Rows are addressed by `rowid`; `WITHOUT ROWID` tables are listed read-only.
Identifiers are double-quoted with internal quotes doubled, and edited values are
bound as parameters, so schemas with spaces, keywords or quotes in their names
work unmodified.

## rkyv — auto-detected key/value CRUD + structural inspection

rkyv archives are **not self-describing**: the format stores no field names or
type tags (https://rkyv.org/format.html), so the schema cannot be recovered from
an *unknown* archive. zdbview handles this with a **format registry**: known
formats are detected by their magic header and decoded to real key/value with a
faithfully-copied, byte-compatible archive type; anything unrecognized falls
back to a raw structural view.

- `0` **Records** — key/value table for a recognized archive: keys on the left,
  the selected value's decoded scalar fields plus a hex dump on the right.
  Searchable by key. This view is the default whenever a format is recognized.
- `1` **Info** — file size, and (when recognized) the detected format name and
  decoded header fields.
- `2` **Strings** — every run of printable text embedded in the archive, with
  byte offsets.
- `3` **Hex** — `xxd`-style hex/ascii dump of the raw bytes.

**Recognized formats** (all fusevm-host script/heap caches, rkyv 0.7):

| Format | Magic | Detect | Key |
|--------|-------|--------|-----|
| zshrs script cache | `ZRSC` | magic | script path |
| zshrs autoload cache | `ZRAL` | magic | function name |
| strykelang script cache | `STRY` | magic (native v4 + compat) | script path |
| awkrs script cache | `AWKR` | magic | script path |
| vimlrs script cache | `VIML` | magic | script path |
| elisprs heap-image cache | `ELSP` | magic | script path |
| pythonrs bytecode cache | *(none)* | validated try-decode | source path |
| rubylang / arb script cache | *(none)* | validated try-decode | u64 content hash |

Magic-bearing formats are matched by their header; the header-less hash-keyed
shards (pythonrs, rubylang, arb) are attempted last and gated by rkyv
validation, so an unrelated archive falls through to the structural view rather
than mis-decoding. A format mismatch always surfaces as failed validation →
structural fallback, never silent corruption. Adding a format is one registry
entry: copy its archive type (same rkyv version and features as the producer)
and map its magic (or add a validated try-decode for header-less formats).

### Full CRUD (write-back)

Recognized archives are editable in place, not just readable. In the Records
view:

- `a` — **create** a record (prompt for a key; inserted with an empty value).
- `e` — **update** the selected record's value (type text, or `0x<hex>` for
  binary).
- `r` — **rename** a record's key (map-keyed formats).
- `d` — **delete** the record (confirm with `y`).

Every edit deserializes the shard, mutates it, and re-serializes it, then writes
the file back atomically (temp + rename). Re-serialization is **byte-identical**
to what the producing host writes, so the host reads the edited shard normally —
verified by round-tripping every real cache. Edits target a record's stable
identity (map key, or the u64 content hash for the header-less formats), so an
update or delete touches exactly one entry even when several share a display key
(pythonrs stores many records under `<string>`/`<stdin>`). Rename is offered only
for the map-keyed formats; the header-less formats key by a content hash and have
no renameable key.

### Bytecode disassembly (`disasm` feature — on by default)

The script-cache value blobs are `bincode`-encoded `fusevm::Chunk`. The value
pane's **disasm** render mode (`v` cycles to it) decodes the blob and lists ops,
constants, and names using the real `fusevm` types — no vendored copy of the
267-variant opcode enum, so no risk of silently wrong output. bincode is
version-sensitive: disassembly is correct only when the linked `fusevm` version
matches the one that encoded the cache; a mismatch fails loudly (`invalid
variant` / `unexpected EOF`) and you fall back to hex. On by default (`fusevm`
is published on crates.io); disable with `--no-default-features`.

## Keys

| Key | Action |
|-----|--------|
| `Tab` | switch focus (table list ↔ rows) |
| arrows / `hjkl` | move |
| `gg` / `G` | jump to top / bottom |
| `Enter` | open detail (row / record); focus rows (table list) |
| `/` | search; `n` / `N` next / previous match |
| `Ctrl-f` / `Ctrl-b` | page forward / back (SQLite) |
| `e` `a` `d` `:` | edit / add / delete / SQL (SQLite) |
| `a` `e` `r` `d` | create / update-value / rename / delete record (rkyv) |
| `S` | schema view (SQLite) |
| `0` `1` `2` `3` | Records / Info / Strings / Hex (rkyv) |
| `v` | cycle value render (auto / hex / text / disasm) — detail screen |
| `y` | copy cell / value / key to clipboard (OSC 52) |
| `x` | export table (CSV) / records (JSON) to a file |
| `t` | theme chooser (31 schemes) |
| `?` | help overlay |
| mouse | wheel scrolls, click selects, right-click selects + opens detail |
| `q` | quit |
| `Esc` | back out of a nested screen (quits on the main screen) |

## Themes

`t` opens a chooser of **31 color schemes** (ported from `iftoprs`); `j`/`k` or
the wheel cycle with live preview, `Enter` saves, `e` opens a palette **editor**
(adjust the 6 base colors, `Enter` saves a custom scheme). The selection
persists to `$XDG_CONFIG_HOME/zdbview/prefs` (or `~/.config/zdbview/prefs`) and
loads on startup.

Input prompts (search, SQL, cell/value edit, add/rename) have a movable text
cursor: `←`/`→`, `Home`/`End`, and the readline chords `Ctrl-a`/`Ctrl-e`
(line start/end), `Ctrl-w` (delete word), `Ctrl-u`/`Ctrl-k` (kill to
start/end). Mouse support and the cursor model are ported from `iftoprs`.

SQLite search is **whole-table** (SQL-backed across every column), not limited to
the loaded page; rkyv search scans record keys, the string list, or raw bytes.
Text searches are case-insensitive and wrap; the hex byte search is
case-sensitive.

## Detail view

`Enter` on a row or record opens a full-screen detail: every field untruncated on
top, and a scrollable value pane below. `v` cycles how the value bytes render —
**auto** (text if it looks textual, else hex), **hex** (`xxd` style), or **text**
(UTF-8). `y` copies the value; the blob is shown raw, so this is how you read a
cell or record value that's too wide for the grid.

## Export

Interactive `x` writes the current table (CSV) or all records (JSON) to a file in
the working directory. Non-interactively:

```sh
zdbview data.db --export json      # object of { table: [rows...] }, all tables
zdbview data.db --export csv       # first table as CSV
zdbview cache.rkyv --export json   # recognized records, value blob as hex
```

## Man pages

Installed by the Homebrew formula — `man zdbview` works straight after
`brew install`. From a source checkout:

```sh
man -l man/man1/zdbview.1        # the standard page
man -l man/man1/zdbviewall.1     # the all-in-one reference, zshall-style

# install
cp man/man1/zdbview*.1 /usr/local/share/man/man1/
```

## Zsh completion

The Homebrew formula drops `_zdbview` into Homebrew's `zsh_completion` dir,
which is already on `fpath`. From a source checkout:

```sh
fpath=(/path/to/zdbview/completions $fpath)
autoload -Uz compinit && compinit
```

## Build

```
cargo build
cargo test
```

## License

MIT — see [LICENSE](LICENSE).
