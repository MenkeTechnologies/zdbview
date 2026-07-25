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
zdbview                        # no args → recent files + a scan (cached)
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

## No args — recent files plus a scan

Running `zdbview` with no argument opens a picker of everything it can open:
recently used files first, then whatever a **background scan** finds, so a shard
does not have to be located by hand.

```
┌ zdbview — 127 files (3 recent, scanning… 127 found) ─────────────────────────┐
│ rkyv   scripts.rkyv                     608 B  /Users/me/.awkrs             │
│ rkyv   scripts.rkyv                     760 B  /Users/me/.zshrs             │
│ rkyv   scripts.compat.rkyv              1.3 K  /Users/me/.stryke            │
│ sqlite compsys.db                        187 M  /Users/me/.zshrs            │
└──────────────────────────────────────────────────────────────────────────────┘
j/k move · / search · Enter open · c scheme · h help · q quit  ·  awkrs script cache (AWKR)
```

The scan runs on its own thread, so the picker is usable immediately and fills in
as results arrive; the title counts what has been found. `/` **filters** the list
by path as you type (`zdbview — 1/127 files  /compsys` with everything else gone),
`Enter` keeps the filter, `Esc` clears it, and a second `Esc` quits. Recent rows show their age, scanned rows their size,
and the bottom line names the recognized format of the selected row.

**The walk does not repeat on every start.** A completed scan is saved to
`$XDG_CACHE_HOME/zdbview/scan` (or `~/.cache/zdbview/scan`) and reused for 24
hours, so later starts show the list immediately — measured here, 127 hits take
~550 ms to walk and ~0.4 ms to load back from the 12 KB cache. The title says how
old the saved list is (`scan 3h old · r rescans`); `r` walks again, `R` walks again
after discarding the saved list, and `--rescan` does the same from the command
line. Entries whose file has since disappeared are dropped on load, and `--scan`
roots neither read nor write the cache since they are not the default set.

**Where it looks** — the producers keep their stores in their own home directory
(`~/.zshrs/scripts.rkyv`, `~/.zshrs/compsys.db`, `~/.pythonrs/scripts.rkyv`), so
the default roots are the dot-directories of `$HOME` (most-recently-touched
first), the XDG cache and data directories, the working directory, and `$HOME`'s
own files. `~/Library`, VCS and package-manager caches, and Chromium profile
stores are skipped — that is what keeps the list about your data. The walk is
bounded (5 levels, 60k entries, 500 hits, 20 s) and stops as soon as you pick.

**What counts as a hit** — the SQLite header magic, or one of the rkyv shard
magics, or a `.rkyv` name (the header-less hash-keyed shards carry no magic).
Nothing else is offered, and files are only read when their extension makes them a
candidate. Ordering puts recognized shards first, then other rkyv archives, then
databases, newest-first within each group.

```sh
zdbview --scan ~/Library --scan /srv   # scan these instead of the defaults
zdbview --no-scan                      # recent files only
zdbview --rescan                       # ignore the saved scan and walk now
```

Recent files are recorded in `$XDG_CACHE_HOME/zdbview/recent` (or
`~/.cache/zdbview/recent`), most-recent-first. Paths are canonicalized, so the
same file reached through different relative paths or symlinks dedupes to one
entry that moves back to the front on re-open; the list is capped at 50 entries
and written via temp-file-plus-rename, so a concurrent reader never sees a
half-written list. A recent file the scan also finds stays a recent row.

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
| zshrs canonical shard | `ZSHS` | magic | `section/key`, `section[i]` |
| pythonrs bytecode cache | *(none)* | validated try-decode | source path |
| rubylang / arb script cache | *(none)* | validated try-decode | u64 content hash |

The canonical shard is the shell's whole captured state for one source root —
aliases, functions, options, bindings, paths — so its records address a section
and a key (`aliases/ll`, `path[0]`, `extras/<sub>/<key>`) rather than one flat
entry map; renames apply to the map sections, lists being positional.

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
- `e` — **update** the selected record's value in a **hex editor** (see below).
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
| `j` / `k`, arrows | move; `←` / `→` change column |
| `gg` / `G` | jump to top / bottom |
| `Enter` | open detail (row / record); focus rows (table list) |
| `/` | **filter** the list/table as you type; `Enter` keeps it, `Esc` clears it |
| `n` / `N` | next / previous match of the filter pattern |
| `Ctrl-f` / `Ctrl-b`, `PgUp` / `PgDn` | page by a screenful (every view) |
| `e` `a` `d` `:` | edit / add / delete / SQL (SQLite) |
| `s` | sort by the cursor column (ascending → descending → off) |
| `<` / `>` | move the sort to the previous / next column |
| `a` `e` `r` `d` | create / hex-edit value / rename / delete record (rkyv) |
| `S` | schema view (SQLite) |
| `0` `1` `2` `3` | Records / Info / Strings / Hex (rkyv) |
| `v` | cycle value render (auto / hex / text / disasm) — detail screen |
| `y` | copy cell / value / key to clipboard (OSC 52) |
| `x` | export table (CSV) / records (JSON) to a file |
| `o` | back to the file list (open another file) |
| `c` | color-scheme chooser |
| `C` | palette editor |
| `h` / `?` | help overlay |
| mouse | wheel scrolls, click selects, right-click selects + opens detail |
| `q` | quit |
| `Esc` | back out of a nested screen; on the first level, back to the file list |

`h` is the help key, not a motion — columns move with `←` / `→`. The overlay keys
(`h`, `c`, `C`) work on every screen, including the recent-files picker.

## Color schemes

The scheme is carried from screen to screen — the picker hands it to the file it
opens and the file hands it back — so opening a file never re-reads it from disk
and cannot land on a different one. Writes to the prefs file are atomic
(temp-plus-rename), because a plain write is briefly empty and another instance
reading at that moment would fall back to the default scheme.

`c` opens the scheme chooser (ported from `iftoprs`): every scheme with a swatch
of its six palette colors, `j`/`k` or the wheel to cycle with live preview,
`Enter` to save, `Esc` to cancel and restore the previous scheme. `C` opens the
palette **editor** — `←`/`→` pick a slot, `↑`/`↓` adjust it by one, `PgUp`/`PgDn`
by sixteen, `Enter` saves the custom palette. Both persist to
`$XDG_CONFIG_HOME/zdbview/prefs` (or `~/.config/zdbview/prefs`) and load on
startup; the editor uses `C` rather than `e`, which edits data everywhere else.

Non-interactively, `--list-themes` prints every scheme with its token and swatch,
and `--theme <token>` overrides the saved scheme for one run:

```sh
zdbview --list-themes                  # tokens, names, palettes
zdbview data.db --theme blade_runner   # this run only, prefs untouched
```

## Help overlay and toasts

`h` (or `?`) opens the keyboard-shortcut overlay: a themed box listing every
binding in three columns, with the section matching what is open — SQLite, rkyv,
or the recent-files picker. Any key or click closes it.

Action results (copy, export, edit, delete, SQL, scheme saved) appear as a
transient **toast** centered above the status bar and dismiss themselves after
three seconds — a port of iftoprs's `StatusMsg` / `draw_status`. The same text
stays in the status bar so it remains readable after the toast fades.

Input prompts (search, SQL, cell/value edit, add/rename) have a movable text
cursor: `←`/`→`, `Home`/`End`, and the readline chords `Ctrl-a`/`Ctrl-e`
(line start/end), `Ctrl-w` (delete word), `Ctrl-u`/`Ctrl-k` (kill to
start/end). Mouse support and the cursor model are ported from `iftoprs`.

## Hex editor

`e` on a record opens a full-screen **hex editor** over that record's value —
ported from [`zmax`](https://github.com/MenkeTechnologies/zmax)'s
`zmax-term/src/ui/hex.rs`. It opens pre-filled with the current bytes, so there
is nothing to retype:

```
 plugins/foo.zsh [+]  —  40 bytes  ·  cursor 0x00000004  ·  -- EDIT (hex) --
00000000  40 07 0e 15 1c 23 2a 31  38 3f 46 4d 54 5b 62 69 |@....#*18?FMT[bi|
00000010  70 77 7e 85 8c 93 9a a1  a8 af b6 bd c4 cb d2 d9 |pw~.............|
```

| Key | Action |
|-----|--------|
| `h` `l` `j` `k`, arrows | move a byte / a row; `0` `$` row start / end; `g` `G` first / last |
| `Ctrl-f` / `Ctrl-b` | page; the wheel scrolls, a click places the cursor |
| `i` / `R` | enter EDIT mode (the editor opens read-only) |
| `Tab` | switch the focused column between hex and ASCII |
| `0`-`9` `a`-`f` | in EDIT + hex: set the high nibble, then the low one, then advance |
| any printable | in EDIT + ASCII: overwrite the byte and advance |
| `o` / `O` | insert a `00` byte after / before the cursor |
| `x` | delete the byte under the cursor |
| `Ctrl-s` | write the value back into the archive |
| `Esc` | leave EDIT mode; `q` leaves the editor (twice when there are unsaved edits) |

The editor is modal — `h` and `c` are its own motions and hex digits there, not
the help and scheme keys. Saving goes through the same write-back as every other
record edit: deserialize, mutate, re-serialize byte-identically, atomic rename.

`o`/`O`/`x` are a zdbview addition: zmax edits a fixed-length file, while a
record's value can legitimately change length.

## Sorting

`s` sorts the row grid by the column under the cursor: ascending, then descending
on a second press, then back to the table's natural `rowid` order on a third.
`<` / `>` move the sort to the previous / next column, keeping the direction. The
sorted column is marked in its header (`qty ▲`) and in the pane title; selecting
another table clears the sort.

Sorting is done in SQL (`ORDER BY "col" ASC|DESC, rowid`), so it orders the
**whole table**, not just the loaded page — numeric columns sort numerically, and
the `rowid` tiebreaker keeps paging stable when the sort column has duplicates.
Search and the `(row N of M)` readout follow the displayed order, so `n` / `N`
step to the next match *on screen* rather than the next by `rowid`.

## Filtering

`/` **filters** the current list as you type, the same model as `iftoprs`: only
matching rows stay listed, the title carries the count (`tables 2/3  /user`,
`1/4 keys  /alpha`), `Enter` keeps the filter and `Esc` clears it and puts the
cursor back where `/` was pressed. Navigation walks the filtered list, so `j`/`k`
never land on a hidden row.

The list stays live while the pattern is being typed: `↑`/`↓`, `PgUp`/`PgDn`,
`Home`/`End` and `Ctrl-f`/`Ctrl-b` move through the matches with the prompt still
open, so you can narrow and pick in one go. `←`/`→` belong to the pattern's own
text cursor.

For SQLite the filter is a SQL `WHERE` across every column, so it covers the
**whole table** — the row count and paging follow the filter, not just the loaded
page. The table list, the rkyv Records and Strings views, and the file picker
filter the same way; the Hex view is unfiltered (bytes have no rows) and keeps
`/` as a byte search. Matching is case-insensitive substring; the hex byte search
is case-sensitive.

## Large archives

Opening a big shard used to block: a 382 MB `elisprs` heap image took ~4 s to
scan for strings and ~25 s for rkyv to validate, which reads as a hang. Now the
same file opens in **~213 ms**:

- rkyv validation for anything over 4 MB runs on a background thread. The
  structural view (Info / Strings / Hex) is usable immediately and the Records
  view appears when the decode lands, with a toast naming the format.
- String extraction is bounded to 20k runs from the first 64 MB. The Info line and
  the Strings title say so (`Strings (20000+)`) rather than implying the list is
  exhaustive.

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
