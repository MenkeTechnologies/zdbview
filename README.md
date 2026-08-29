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

**The walk is paid for once, not on a timer.** A scan is saved to
`$XDG_CACHE_HOME/zdbview/scan` (or `~/.cache/zdbview/scan`) and kept until the
filesystem invalidates it — there is no expiry, because an index of a disk nobody
touched still describes that disk. The index records the mtime of every directory
that held a hit, and a start compares them: unchanged means no walk at all,
however old the index is.

What did change is walked again — **those directories, not the disk.** A machine
being worked on always has something moving (a cache written a minute ago), and
re-walking `/` for it would put the full walk back on every start, which is the
behavior keeping an index was meant to remove. A walk of everything happens on
the first run, when the previous one never finished, and when asked for. Quitting
mid-walk keeps what was found: an interrupted refresh re-flags only the
directories it was reading, so opening a file two seconds in never costs a walk
of the disk.

The one case this cannot see is a database created in a directory that never held
one; `r` in the picker and `--rescan` on the command line are for that. `R` walks
again after discarding the saved list. `--scan` roots neither read nor write the
saved scan, since they are not the default set.

**Where it looks** — everything, ending at `/`. The earlier roots exist for
ordering rather than reach: the producers keep their stores in their own home
directory (`~/.zshrs/scripts.rkyv`, `~/.zshrs/compsys.db`,
`~/.pythonrs/scripts.rkyv`), so the dot-directories of `$HOME`
(most-recently-touched first), the XDG cache and data directories, the working
directory, `$HOME`'s own files and `~/Library/Application Support` are read first
— they take milliseconds, so the rows that matter are on screen before the walk
has left `/bin`. Then `/` covers the rest of the machine at any depth. A file
reachable from two roots is still listed once.

What it stays out of is other disks: the walk descends only into what sits on the
same devices as `/` and `$HOME`, so a mounted USB disk, and above all an SMB or
NFS share, is never walked over the network. `/System/Volumes` is skipped for the
same reason — on macOS it is a second door onto the disk you are already walking,
and going through it walks everything twice. `/dev`, `/proc`, `/sys`, `/net`,
`/private/var/folders` and the Spotlight indexes are skipped as never containing
anything openable. VCS and package-manager caches, the rest of `~/Library`, and
Chromium profile stores are skipped as noise — that is what keeps the list about
your data. Measured on this machine, a full walk of `/` finds 553 files in ~96 s
(debug build) and loads back from the saved index in under a millisecond.
Finding a file is never a reason to stop: the number of hits is not capped, since
a cap there would cut the walk mid-root and lose every root after it. A 10-minute
clock and a 20M-entry count are hang backstops, not budgets.

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
- `:` — open the **SQL editor** (see below).
- `D` — the **database report** (see below).
- `A` — **column statistics** (see below).
- `e` on a blob cell opens the **hex editor**; text cells edit inline.
- `e` also works on the **detail screen**, on the field the arrows select there.

Rows are addressed by `rowid` where there is one, and by the **primary key**
otherwise — so a `WITHOUT ROWID` table is editable too, including one with a
composite key. A keyed row is matched on every key column, so editing `('a','two')`
cannot touch `('b','one')`. Two cases stay read-only and say so: a table with
neither a rowid nor a primary key, and a key column holding a blob (the key is
matched as text).
Identifiers are double-quoted with internal quotes doubled, and edited values are
bound as parameters, so schemas with spaces, keywords or quotes in their names
work unmodified.

A table whose virtual-table module this binary does not have is marked ` ⃰` in the
list rather than left to fail when selected — an FTS index built with a custom
tokenizer, for instance, cannot be read by any other program either.

## Large databases

Nothing the grid does waits on a scan of the whole table. Measured on Ableton's
`Live-files` index — 23 GB, a 300 MB WAL, 6.5M rows in `files` and 88.8M in
`ancestors`:

| | before | now |
|---|---|---|
| open, first page drawn | — | **4 ms** |
| first page under a filter | 4.16 s | **28 ms** |
| step to the next / previous page | grows with the page number | **21 / 36 ms** |
| last page (`G`) | 9.6 s | **1 ms** |
| exact row count behind a filter | 9.2 s, blocking | **1.6 s**, in the background |

Both columns are the development build on the same file, so the ratios are what to
read; a release build is faster on both sides.

Four things get it there.

**Queries run on their own threads.** A page, an exact count and an `n`/`N` search
each have a worker with its own read-only connection, so a scan the user cannot see
never blocks the render thread. Read-only also means browsing a database another
process is writing cannot make zdbview checkpoint its WAL.

**Nothing is cancelled by waiting for it.** Every request carries a generation, and
each connection runs a progress handler that abandons its statement as soon as its
generation is stale. Typing a filter therefore costs one query, not one per key —
no debounce interval to guess at.

**Totals are never scanned for.** The count a page needs is "is there another
page", which one extra row answers for free. Until an exact total arrives the title
says `501+`; the exact figure is counted in the background and fills in. `G` waits
for that count instead of freezing, since only an exact total says which page the
last one is.

**Paging steps by cursor, not by offset.** `LIMIT n OFFSET k` makes SQLite walk and
discard `k` matching rows, so on a filtered grid the cost of a page grows with how
far in it is — the first page of that 270k-match filter took 26 ms and
`OFFSET 269000` took 6.2 s. A step to the neighbouring page asks for the rows after
(or before) a known one instead, which is one index seek whatever the page number,
and the last page is read backwards from the end.

**The exact count uses every core.** One SQLite statement runs on one thread, so
the table is cut into rowid ranges and each is counted on its own connection —
6.1× faster than one statement on that 6.5M-row filter. The cuts are arithmetic:
finding balanced ones means walking the whole index, which cost 5.8 s of cold reads
on that file, more than the count it was meant to speed up. Since rowids are unique
integers, cutting far finer than the worker count and taking ranges one at a time
balances the work without reading anything.

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
| zshrs canonical shard | `ZSHS` | magic (or validated, unstamped) | `section/key`, `section[i]` |
| zshrs system shard | `ZSHS` | magic (second layout) | entry key |
| pythonrs bytecode cache | *(none)* | validated try-decode | source path |
| rubylang / arb script cache | *(none)* | validated try-decode | u64 content hash |

The canonical shard is the shell's whole captured state for one source root —
aliases, functions, options, bindings, paths — so its records address a section
and a key (`aliases/ll`, `path[0]`, `extras/<sub>/<key>`) rather than one flat
entry map; renames apply to the map sections, lists being positional.

**Registering a magic yourself.** The header the recognized formats share is a
published convention, not a zdbview-private one — see
[`spec/rkyv-shard-header.md`](spec/rkyv-shard-header.md). Any producer can stamp
its own four-character tag and have zdbview name it without patching the binary:
list it in `$XDG_CONFIG_HOME/zdbview/magics` (or `~/.config/zdbview/magics`),
one `tag = display name` per line.

```text
# tag = display name
LUAR = luars bytecode cache (LUAR)
0x5045_524C = perlrs script cache (PERL)
```

A four-character tag is the little-endian u32 a producer stamping
`u32::from_be_bytes(*b"LUAR")` writes, so the bytes read in file order; the
`0x…` form (underscores allowed) sets the u32 directly for anything else. A
registered tag zdbview has no decoder for is still worth adding: the scan then
offers the file under its real name and the structural inspector opens it. An
entry for a tag zdbview already knows renames it in the UI; the decoder keyed on
that tag is unaffected. Unparseable lines are skipped, not fatal.

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
opcode enum, whose variant list grows every fusevm release, so no risk of
silently wrong output. bincode is
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
| `n` / `N` | next / previous match — inside the active filter, in display order |
| `Ctrl-f` / `Ctrl-b`, `PgUp` / `PgDn` | page by a screenful (every view) |
| `e` `a` `d` `:` | edit / add / delete / SQL (SQLite) |
| `W` / `Ctrl-s`, `R` | write / revert the unwritten changes (SQLite) |
| `E` | edit the cell **as bytes**, whatever it holds (SQLite) |
| `s` | sort by the cursor column (ascending → descending → off) |
| `<` / `>` | move the sort to the previous / next column |
| `a` `e` `r` `d` | create / hex-edit value / rename / delete record (rkyv) |
| `S` | schema: the objects, and the designers that edit them (SQLite) |
| `D` | database report: pragmas, integrity check, foreign-key lint, maintenance (SQLite) |
| `P` | edit the pragmas that decide how the file is written (SQLite) |
| `H` / `U` | hide the cursor column / show every hidden column (SQLite) |
| `f` | freeze the columns up to the cursor at the left edge (SQLite) |
| `#` | show the `rowid` as its own column (SQLite) |
| `m` / `M` | next / previous display format for the cursor column (SQLite) |
| `%` | custom display format for the cursor column (SQLite) |
| `!` | conditional formats for the cursor column (SQLite) |
| `r` | find and replace in the cursor column (SQLite) |
| `i` | insert a row value by value (SQLite) |
| `V` | save the current filter as a view (SQLite) |
| `z` / `Z` | clear the sorting / the filter (SQLite) |
| `L` | unlock a view for editing (SQLite) |
| `Ctrl-y` | copy the column's name (SQLite) |
| `Ctrl-p` | print the table through `lpr` (SQLite) |
| `Ctrl-n` | set the cell to `NULL` (SQLite) |
| `Ctrl-h` | copy the cell as a hex + ASCII dump (SQLite) |
| `Ctrl-e` | open the cell's bytes in an external application (SQLite) |
| `Ctrl-r` / `F5` | re-read the database (SQLite) |
| `A` | column statistics for the table, with a per-column frequency table (SQLite) |
| `F` | follow the foreign key under the cursor to the row it references (SQLite) |
| `Y` | copy the row as an `INSERT` (SQLite) |
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
binding in up to three columns, with the sections matching what is open — the
SQLite grid, the rkyv archive, the recent-files picker, the SQL editor, the hex
editor, the write monitor, the log walker, the schema, the pragmas, the column
statistics, or the database report. Any key or click closes it.

The box measures itself against what that screen lists: the key field is as wide
as the longest chord on it, the description field as wide as the longest
description, and only the columns the sections actually fill are drawn — so the
pragma screen's two sections get a narrow box while the SQLite grid gets a wide
one. Nothing is clipped unless the terminal itself is too small for the box.

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

## SQL editor (`:`)

`:` opens a multi-line SQL editor with Tab completion and a transcript, ported from
[`zmax`](https://github.com/MenkeTechnologies/zmax)'s REPL panel
(`zmax-term/src/ui/repl.rs`). The old `:` was one line that reported only an
affected-row count, so a `SELECT` looked like it had done nothing.

```
┌ SQL — 3 statements · Tab completes · ^j newline · Enter runs · Esc back ───────┐
│     a                            b                                            │
│     1                            y                                            │
│     11 rows                                                                    │
│     1 row changed                                                              │
│    ┌ su ──────────┐              biggest                                       │
│    │SUM           │              9                                             │
│    │SUBSTR        │                                                            │
│    └──────────────┘missing_table                                               │
└────────────────────────────────────────────────────────────────────────────────┘
┌ input · 9 chars ──────────────────────────────────────────────────────────────┐
│sql> select su                                                                 │
└────────────────────────────────────────────────────────────────────────────────┘
```

| Key | Action |
|-----|--------|
| `Enter` | run the statement |
| `Ctrl-j` (or `Alt-Enter`) | insert a newline — statements may span lines |
| `Tab` / `Shift-Tab` | complete the word before the cursor; walk the candidates |
| `Alt-e` / `F5` | query plan instead of the result (`EXPLAIN QUERY PLAN`) |
| `↑` `↓` / `Ctrl-p` `Ctrl-n` | browse history, with the in-progress line stashed |
| `Ctrl-g` | clear the input; `Ctrl-l` clears the transcript |
| `Ctrl-a` `Ctrl-e` `Ctrl-b` `Ctrl-f` `Ctrl-w` `Ctrl-u` `Ctrl-k` | readline motions and kills |
| `PgUp` / `PgDn` | scroll the transcript |
| `Esc` | back to the data; the transcript and history survive |

A `SELECT` (or `PRAGMA`, `EXPLAIN`, a CTE) comes back as a grid of rows with a
count; anything else reports what it changed and reloads the grid behind it; a
refusal shows SQLite's own message. Results are capped at 500 rows per statement,
and the cap is stated when it bites. Every statement is timed, like the shell's
`.timer`, and `Alt-e` (or `F5`, where the terminal eats Meta) shows the plan
instead of running the query — the shell's `.eqp`, drawn as the same tree:

```
sql> select p.name from parent p join child c on c.parent_id = p.id order by p.name
     QUERY PLAN
     |--SCAN c
     |--SEARCH p USING INTEGER PRIMARY KEY (rowid=?)
     `--USE TEMP B-TREE FOR ORDER BY
```

Completion has no LSP to ask, so it reads the statement instead: what SQL allows at
the cursor is what it offers. Measured against `~/.zshrs/compsys.db`:

| typed | offers |
|-------|--------|
| *(nothing)* | `SELECT`, `INSERT INTO`, `UPDATE`, `DELETE FROM`, `CREATE TABLE` … |
| `select * from ` | the tables, and only the tables |
| `select * from autoloads where na` | **`name`** (the scoped table's column), then `named_dirs` |
| `select * from autoloads a where a.` | `name`, `source`, `offset`, `size`, `body` — the alias resolved |
| `select ` | columns; with no table named yet, from any table |
| `pragma jour` | `journal_mode` |
| `select * from nosuch.` | nothing, rather than a guess |

`FROM`/`JOIN`/`UPDATE`/`INTO` bring tables into scope in the order they are named,
aliases included, so a join offers the left table's columns before the right one's.
Inside a clause the scoped columns lead, then what continues the clause (`AND`,
`ORDER BY`, …), then table names, then the functions. A prefix matching exactly one
candidate is taken without a menu.

### Tabs, files and results

The rest of DB Browser's Execute SQL tab, on the Alt chords so every printable
key stays part of the statement:

| Key | Does | DB Browser |
|-----|------|------------|
| `Alt-t` / `Alt-w` | new tab / close it | Open tab / Close tab |
| `Alt-[` / `Alt-]` | previous / next tab | the tab bar |
| `Alt-o` / `Alt-s` | open a `.sql` file into the tab / save the tab to one | Open SQL file / Save SQL file |
| `Alt-l` | run only the line the cursor is on | Execute current line |
| `Alt-;` | comment or uncomment the cursor's line | Toggle comment |
| `Alt-f` / `Alt-r` | find / replace inside the statement | Find / Find and Replace |
| `Alt-x` | export the last result set (CSV or JSON, by extension) | Export to CSV / JSON |
| `Alt-v` | save the statement that produced the result as a view | Save results as view |
| `Alt-W` | word wrap the transcript | Word Wrap |
| `Esc` | stop a running statement, else back to the data | Stop the execution |

Each tab keeps its own statement, transcript and file; the tab strip is the
pane's title, and a tab loaded from a file is named after it. A tab that came
from a file saves back to it without asking again.

**Stopping a statement.** A statement runs on the thread that draws, so nothing
else can watch the keyboard while it does. SQLite's progress handler is the one
place that gets control back periodically, so that is where the key is read: it
polls without blocking every few thousand VM instructions, and `Esc` (or
`Ctrl-c`) aborts the statement. The transcript then says it was stopped rather
than that it failed:

```
sql> SELECT count(*) FROM huge
     stopped after 2.184s
```

### Dot-commands

A line starting with `.` is a dot-command, as in the shell — `Tab` completes them,
and `.help` lists exactly what is implemented:

| Command | Does |
|---------|------|
| `.tables` / `.schema [T]` / `.indexes [T]` | the objects, their `CREATE` statements, a table's indexes with their columns |
| `.dump [T]` | schema and data as replayable SQL (the same writer as `--export sql`) |
| `.databases` / `.attach FILE ALIAS` / `.detach ALIAS` | attached databases, so a statement can join across files |
| `.mode list\|csv\|tsv\|markdown\|line\|insert\|json` | how a result set is rendered; `list` is the grid |
| `.headers on\|off` | column names in redirected output |
| `.output FILE` / `.once FILE` | send results to a file — every result until reset, or just the next one |
| `.timer on\|off` / `.eqp on\|off` | statement timing; plan before each statement |
| `.import FILE TABLE` | load a CSV or TSV (the same reader as `--import`) |
| `.read FILE` | run a file's statements, each one as if typed |
| `.backup FILE` | `VACUUM INTO` a copy |
| `.expert` | index advice for the statement just run (see below) |
| `.recover [FILE]` | salvage rows page by page into a script (see below) |
| `.vacuum` / `.analyze` / `.reindex` | the maintenance statements |
| `.quit` | leave zdbview |

`.expert` is **not** `sqlite3_expert` — that builds candidate indexes and re-plans
against them, and rusqlite exposes no binding for it. This reads the plan the
planner actually produced and the columns the statement compares on, then names an
index for each table the planner chose to scan in full:

```
sql> SELECT id FROM big WHERE note = 'note 5'
sql> .expert
     big: full scan — CREATE INDEX "big_note" ON "big"("note");
```

A column is attributed to a table only when no other table in the statement has a
column by that name, so an ambiguous join reports the scan without guessing at the
index.

History persists to `$XDG_CACHE_HOME/zdbview/sql_history` (200 statements, newlines
escaped so one statement stays one line), written temp-plus-rename like the other
appdata.

## Writing and reverting (`W` / `R`)

Every edit — a cell, a row, an import, a schema change, a statement typed into
the editor — is buffered until it is written, which is how DB Browser works:
changes belong to the session, not to the file, until `W` (or `Ctrl-s`) commits
them. `R` throws them all away and puts the database back to what it was when the
first unwritten edit was made. Both are one savepoint, so a schema edit and a
hundred cell edits revert together.

The status line carries a marker while anything is unwritten, and leaving the
file — `q`, `Esc`, `o` — asks first, because closing the connection would roll
the savepoint back:

```
● unwritten changes (W write · R revert)
unwritten changes: w = write, r = revert, any = stay
```

Two consequences worth knowing:

* **The grid reads differently while changes are pending.** Pages normally come
  from reader threads on their own connections, and no connection can see
  another's open transaction, so an unwritten edit would still show its old
  value. While anything is pending the page is fetched on the store's own
  connection instead; when it is written, paging goes back off the render thread.
* **`VACUUM`, `ANALYZE`, `REINDEX` and `--backup` refuse to run** with unwritten
  changes. They rewrite the whole file and cannot run inside a transaction, so
  they say to write or revert first rather than failing with SQLite's wording.

The one-shot command-line paths (`--import`) write before exiting: there is no
user there to ask.

## What the grid shows

The Browse Data settings DB Browser keeps per table, kept the same way — going
back to a table finds it as it was left.

| Key | Does | DB Browser |
|-----|------|------------|
| `H` | hide the cursor column (the last visible one cannot be hidden) | Hide columns |
| `U` | show every hidden column again | Show all columns |
| `f` | freeze the columns up to the cursor; again inside that span unfreezes | Freeze columns |
| `#` | show the `rowid` as a leading read-only column | Show rowid column |
| `m` / `M` | step the cursor column's display format | Column display format |
| `%` | type a display format of your own, `%1` standing for the column | Custom |

A wide table scrolls sideways under the cursor, and the frozen columns stay at
the left edge while it does. A frozen column is marked `▏` in its header and a
formatted one `ƒ`, since neither setting is otherwise visible.

### Display formats

The nineteen formats DB Browser applies, and the SQL each one runs — plus a
custom expression, where `%1` stands for the column:

| Format | SQL |
|--------|-----|
| default | the column |
| decimal number / exponent notation | `printf('%d', c)` / `printf('%e', c)` |
| hex blob / hex number / octal number | `hex(c)` / `printf('%x', c)` / `printf('%o', c)` |
| round number | `round(c)` |
| lower case / upper case | `lower(c)` / `upper(c)` |
| date as dd/mm/yyyy | `strftime('%d/%m/%Y', c)` |
| julian day to date | `datetime(c)` |
| unix epoch to date / to local time | `datetime(c, 'unixepoch')` (`, 'localtime'`) |
| apple NSDate to date | `datetime(c + 978307200, 'unixepoch')` |
| java epoch (ms) to date | `datetime(c / 1000, 'unixepoch')` |
| webkit / chromium epoch to date / to local time | `datetime(c / 1000000 - 11644473600, 'unixepoch')` (`, 'localtime'`) |
| windows DATE to date | `datetime((c - 25569) * 86400, 'unixepoch')` |
| text as latin-1 / windows-1252 | `hex(c)`, decoded here — see below |

**A format is SQL, not a rendering rule** — which is how DB Browser does it, and
what makes `hex blob` possible at all: the grid receives cells that are already
display strings, so a blob has become `<blob 12 bytes>` long before anything
could format it. Asking SQLite for `hex(col)` gets the bytes; asking the string
does not. The column keeps its own name in the result, so sorting, filtering,
paging and editing all still address the raw value — sorting a hex-formatted
integer column gives `1, 2, a`, not the `1, 10, 2` that sorting the displayed
strings would.

The last two are DB Browser's *Set encoding*: they read the bytes already stored
as ISO-8859-1 or Windows-1252 rather than UTF-8. SQLite has no codecs, so the
value comes back as `hex()` and is decoded on this side — which is also the only
way to read bytes that are not valid UTF-8 at all. Windows-1252 differs from
Latin-1 only in `0x80`–`0x9F`, where it has the curly quotes and dashes that turn
up in text pasted out of Word, and Latin-1 has control characters.

DB Browser's *SpatiaLite Geometry to SVG* is the one format not implemented: it
needs the SpatiaLite extension loaded to mean anything.

## Files, extensions and projects

The File menu, on the command line:

```sh
zdbview --new fresh.db          # create an empty database and open it
zdbview --memory                # a database that never reaches a disk
zdbview data.db --readonly      # a read-only connection: every edit is refused
zdbview data.db --import-sql schema.sql   # run a script in, and exit
zdbview data.db --load-extension ./ext.dylib   # load an extension first
zdbview --project session.zdbp  # open the database a project names, as it was left
```

`--readonly` opens a different connection rather than checking a flag at the
edit, so no key handler can write through it by accident. `--import-sql` runs the
whole script inside one savepoint: a script that fails half way leaves the
database exactly as it was. `--new` refuses to overwrite an existing file, and
forces a page out so the file it made is a database rather than an empty file.

In the editor, `.load FILE` loads an extension — the shell's own command, and DB
Browser's "Load Extension". Extension loading is off in the C library by default
and is enabled only for the duration of the call.

`O` on the database report runs `PRAGMA optimize` — DB Browser's "Optimize" — and
lists what SQLite decided was worth doing, which is usually `ANALYZE` on the
indexes that have changed enough to matter.

`Ctrl-p` prints the table (under its filter and sort) by piping it to `lpr`,
which is what a terminal program has instead of a print dialog.

### Projects

`.project save FILE` and `.project open FILE` in the editor write and read a
project: everything about a session that is not in the database — what each
grid is set to show, what is filtered and sorted, and the statements left in the
editor's tabs. `--project FILE` opens the database it names with those settings
already applied.

DB Browser writes XML; zdbview writes one directive per line, tab-separated, so
a project can be read and edited by a person:

```
zdbview-project	1
db	/path/to/file.db
table	orders
hidden	orders	note
frozen	orders	1
format	orders	total	hex-number
rule	orders	total	> 100	red	bold
filter	orders	tag:keep
sort	orders	total	desc
sql	SELECT * FROM orders WHERE total > 100
```

A directive this version does not know is skipped rather than refused, so a
project written by a later one still opens.

## Conditional formats (`!`)

DB Browser's Conditional Formats, for the column under the cursor: rules that
paint a cell when its value matches. `a` adds a rule, `Enter` edits its
condition, `c` cycles the colour, `b` toggles bold, `d` drops it, and `J`/`K`
reorder — **the first rule that matches wins**, so the order is part of the
meaning.

```
▸  1. > 100                          red      bold
   2. > 10                           yellow
   3. null                           gray
```

A condition is DB Browser's filter vocabulary: an operator and a value (`> 5`,
`= done`, `<> 0`, `like a%`), `null` / `not null`, or bare text meaning
"contains". A comparison is numeric when both sides read as numbers and textual
otherwise — so `> 9` matches `10`, which comparing the strings would not.

The rules are evaluated on what the grid is showing, so a column with a display
format is matched on the formatted value. DB Browser's font and alignment
settings have no meaning in a terminal grid; colour and bold do.

## Working on the data

The rest of DB Browser's Browse Data tab:

| Key | Does | DB Browser |
|-----|------|------------|
| `r` | find and replace in the cursor column | Find and Replace |
| `i` | insert a row value by value, `n` leaving one `NULL` | Insert Values |
| `V` | save the current filter as a view | Save filter as view |
| `z` / `Z` | clear the sorting / the filter | Clear sorting / Clear all filters |
| `L` | unlock a view for editing | Unlock view editing |
| `Ctrl-y` | copy the column's name | Copy column name |
| `Ctrl-n` | set the cell to `NULL` | Set to NULL |
| `Ctrl-h` | copy the cell as a hex + ASCII dump | Copy with hex/ASCII |
| `Ctrl-e` | open the cell's bytes in an external application | Open in external application |
| `Ctrl-r` / `F5` | re-read the database | Refresh |
| `R` (schema screen) | row counts per table and view | Row counts |

**Find and replace** asks in two steps, because a terminal has one prompt line:
what to find, then what to put there. Between them it counts, so a term that
matches nothing never reaches the second question:

```
find in note: cat
2 rows of note contain "cat"
replace "cat" with: bird
replaced "cat" with "bird" in 2 rows of note — R reverts
```

It works on the cursor column, over the rows the active filter leaves, and the
match is a substring (`replace()`), so a row that does not contain the term is
not touched. Like every write it lands in the edit buffer, so a replace that went
wrong is one `R` away.

**Insert Values** (`i`) exists next to `a` for one reason: a column starts `NULL`
and stays `NULL` unless something is typed, so `NULL` and the empty string stay
apart. `n` puts a column back to `NULL` after typing.

**Save filter as view** (`V`) writes the filter's patterns into the statement as
literals, since a view cannot carry parameters — a quote in the pattern is
escaped rather than ending the string:

```
CREATE VIEW "keepers" AS SELECT * FROM "t" WHERE (CAST("tag" AS TEXT) LIKE '%keep%' ESCAPE '\')
```

**Unlocking a view** reports what SQLite would do rather than pretending the lock
is zdbview's to lift: a view can only be written to through `INSTEAD OF`
triggers, so `L` says whether the view has them, and editing a view without them
is refused with that reason.

## Editable pragmas (`P`)

`D` prints the pragmas that describe a file. `P` edits the other kind: the
eighteen settings DB Browser puts in its Edit Pragmas tab, which decide how the
database is written rather than what is in it.

```
  pragma                     value          what it does
▸ auto_vacuum                none           reclaim freed pages on commit, or only on incremental_vacuum
  automatic_index            on             let the planner build a transient index for a scan it would repeat
  case_sensitive_like        —              make LIKE case-sensitive for ASCII (no query form: shows what this session set)
  journal_mode               wal            how the rollback journal is kept; wal lets a reader and a writer overlap
  synchronous                full           how hard a commit is flushed — off risks the file on a power loss
```

`j`/`k` select and `g`/`G` and `PgUp`/`PgDn` move further, `Space` (or `←`/`→`)
cycles a flag or a named set, `Enter` (or `e`) types a number, `P` (or `p`) and
`Esc` go back. A value is shown as the word SQLite means by it
— `synchronous` reads back as `2` and is displayed `full`.

**Every change is read back.** SQLite accepts a pragma it will not apply rather
than raising: `journal_mode` will not change inside a transaction,
`max_page_count` clamps to the pages already in use, an unrecognised value is
ignored outright. The screen shows what the database reports afterwards, and says
so when that is not what was asked for:

```
max_page_count stayed 2: SQLite would not take 1
page_size = 8192 — takes effect on the next VACUUM
```

`case_sensitive_like` has no query form at all — it can be set but never read —
so it shows `—` until this session sets it, rather than a value invented for the
display. Pragmas cannot be applied over unwritten changes, so the screen says to
write or revert first.

## Schema editing (`S`)

`S` lists every object with its `CREATE` statement — the objects DB Browser puts
in its Database Structure tab — and the cursor selects one:

| Key | Does |
|-----|------|
| `j` / `k`, `g` / `G`, `PgUp` / `PgDn` | select an object |
| `Enter` / `e` | open the designer for a table or an index |
| `a` | new table |
| `i` | new index on the selected table |
| `d` | drop the object (asks first) |
| `y` | copy its `CREATE` statement — DB Browser's "Copy Create Statement" |
| `R` | count the rows in every table and view, and press again to drop them — DB Browser's "Row counts" |

Both designers are grids of fields with the SQL they will run shown underneath,
so the statement is visible before it is applied:

| Key | Does |
|-----|------|
| arrows, `Tab` | move by row and by field |
| `Enter` / `e` | edit the field under the cursor |
| `Space` | toggle the flag under the cursor |
| `a` / `d` | add / drop a column |
| `J` / `K` | move the column down / up |
| `W` | write the change |
| `Esc` | cancel, touching nothing |

The table designer sets everything SQLite lets a column carry: type, `PRIMARY
KEY`, `AUTOINCREMENT`, `NOT NULL`, `UNIQUE`, `DEFAULT`, `COLLATE`, `CHECK` and
`REFERENCES`, plus `WITHOUT ROWID` and `STRICT` on the table. Turning
`AUTOINCREMENT` on makes the column the table's single `INTEGER` key, because
that is the only form SQLite accepts.

### What the definition is read from

The object's own `CREATE` statement, not the pragmas. `PRAGMA table_info` reports
a column's name, type, `NOT NULL`, default and key and nothing else — it cannot
see `COLLATE`, `CHECK`, `UNIQUE`, `REFERENCES` or a generated expression — so a
rebuild driven by the pragmas would quietly drop all five. The statement is
parsed instead, and anything the model does not cover is carried through
verbatim rather than lost.

### What each edit costs

Only four schema edits are native `ALTER TABLE` in SQLite: renaming the table,
renaming a column, appending a column and dropping one. When the edit is one of
those, that is what runs:

```
ALTER TABLE "t" RENAME COLUMN "a" TO "a2"
ALTER TABLE "t" ADD COLUMN "b" INTEGER
```

Anything else — a changed type, a changed constraint, a reordered column, an
appended `NOT NULL` column with no default (which `ADD COLUMN` rejects) — is the
rebuild SQLite documents and DB Browser performs:

```
CREATE TABLE "zdbview_rebuild_tmp" (…)      the new shape
INSERT INTO "zdbview_rebuild_tmp" (…) SELECT … FROM "t"
DROP VIEW "v"                                see below
DROP TABLE "t"
ALTER TABLE "zdbview_rebuild_tmp" RENAME TO "t"
CREATE INDEX … / CREATE TRIGGER … / CREATE VIEW …
```

The whole rebuild is one transaction, run with foreign keys off and
`PRAGMA foreign_key_check` before the commit, so an edit that would orphan a
child row is refused and leaves the database exactly as it was.

The views and triggers come down before the swap because they have to: SQLite
validates every object in the schema during `ALTER TABLE … RENAME TO`, and one
still pointing at the table the rebuild has just dropped fails the rename with
`error in view v: no such table`. They are recreated afterwards, following the
table's new name and any renamed columns — an index through the parser, so its
table and columns are re-derived; a trigger or view by identifier rewriting,
which leaves string literals alone. An index whose column no longer exists is
not recreated.

## Database report (`D`)

`D` opens what `sqlite3` prints across four dot-commands. The pragmas are read
when the screen opens; the two checks and the lint run on a keypress, because both
walk the whole file:

| Key | Runs | Shell equivalent |
|-----|------|------------------|
| *(open)* | page size/count, freelist, encoding, journal mode, synchronous, auto-vacuum, schema/user version, application id, foreign keys, derived data size, object counts | `.dbinfo` |
| `i` | `PRAGMA integrity_check` | `.intck` |
| `Q` | `PRAGMA quick_check` | `.intck` (cheap) |
| `f` | foreign keys with no index to serve them | `.lint fkey-indexes` |
| `v` | `VACUUM` — rewrite the file, reclaiming free pages (reports the size change) | `.vacuum` / DB4S "Compact Database" |
| `z` | `ANALYZE` — write `sqlite_stat1` so the planner has statistics | `.analyze` |
| `r` | `REINDEX` — rebuild every index | — |
| `O` | `PRAGMA optimize` — whatever SQLite decides is worth doing, listed | DB4S "Optimize" |

Every result is appended below the pragmas, so the screen grows as checks run.
`j` / `k` and `PgUp` / `PgDn` scroll it, and `g` / `G` jump to the top and the
bottom — the bottom being where the newest output lands.

The lint reads `foreign_key_list`, `index_list` and `index_info` per table and
reports a key only when no index *starts* with the child column, which is the
condition that makes every parent-row change scan the child table. An index on
some other column of the same table does not count.

## Following a foreign key (`F`)

`F` on a foreign-key column jumps to the row it references — the one piece of
navigation a schema gives you for free, and what DB Browser and Datasette both
link. It reads `PRAGMA foreign_key_list`, resolves the parent column (the pragma
leaves it empty when the key targets the parent's primary key), finds the parent
row, and lands on it with the cursor on the key column. A key with no matching
parent row, a `NULL` key and a column that is not a key each say so instead of
moving.

## Column statistics (`A`)

`A` describes every column of the current table — what VisiData's `describe` and
sqlite-utils' `analyze-tables` report:

```
  column           type       rows  nulls  distinct  numeric  longest       min       max      mean
  id            INTEGER      46716      0     46716    46716        5         1     46716   23358.5
  name             TEXT      46716      0     46716        0       51      _0ad     _zzuf         —
  body             BLOB      46716      0     46713        0   552636   <blob …   <blob …         —
```

`numeric` is the count of cells SQLite actually stores as a number, which is the
only way to see that a column declared `INTEGER` is holding text — a declared type
is an affinity hint, not a constraint. `mean` covers those numeric cells only, so a
text column is not averaged into nonsense.

`Enter` on a column shows its most common values with bars, VisiData's frequency
sheet — one glance says whether a column is skewed:

```
  a                                    2  ########################
  b                                    1  ############
```

Every column costs one pass over the table, and on a real database a wide blob
column costs over a second of it (measured: 1.6s for `count(DISTINCT body)` across
46,716 rows of `~/.zshrs/compsys.db`). So the pass runs on its own connection on a
background thread — the screen opens saying `analyzing …` and fills in when the
work lands, and the UI never blocks.

## Editing blobs

A blob cell has no text form, so `e` on one opens the **hex editor** over its bytes
instead of the line editor, and `^s` writes them back as a blob parameter. Editing
a blob as a string would replace the bytes with their own description. Text,
integer and real cells still edit inline.

`E` forces the hex editor on **any** cell, which is the only way to put binary into
one that is not already a blob: a string starts as its own bytes, a number as its
digits, a `NULL` as nothing at all, and `^s` writes the result back as a blob.

## Import

`--import` loads a CSV (or TSV, by extension) into an existing table — the shell's
`.import`:

```sh
zdbview data.db --import rows.csv --table people
# imported 3 rows into people
```

The header row names the columns, so a file whose columns are ordered differently
from the table still lands correctly, and a header naming a column the table does
not have is an error rather than a silent drop. The whole file goes in one
transaction: a row with the wrong field count rolls all of it back. Values are
inserted as text and left to SQLite's affinity rules, which is what the shell does.
The reader is RFC 4180 — quoted fields may hold the separator, newlines and doubled
quotes, and CRLF or a missing final newline are both accepted.

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

## Search inside a filter

`/` narrows the list; `n` / `N` then step through matches **within** what is
listed. For the SQLite grid that means the filter travels into the SQL: the search
statement carries both the search `LIKE` and the filter `LIKE`, and the ordinal
that decides which page to jump to counts only listed rows. Without that, a search
under an active filter lands on a hidden row and scrolls to the page it would have
been on unfiltered.

Both halves of that are full scans in the worst case — finding the match, then
counting to it — so a grid search runs on a query thread and the title says
`· searching` while it does. The ordinal is counted on every core, the same way the
total is.

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
page. Such a filter cannot use an index, so it is a full scan by construction; what
keeps it interactive is that the page is fetched without one and the count that does
need one runs in the background (see [Large databases](#large-databases)). A term of
the form `col:value` restricts that term to one column — DB Browser's filter row —
and terms are ANDed:

| Typed | Keeps |
|-------|-------|
| `zshrs` | rows where any column contains `zshrs` |
| `cwd:zshrs` | rows whose `cwd` contains `zshrs` |
| `cwd:zshrs line:echo` | both conditions, each on its own column |
| `12:30` | rows containing `12:30` — `12` is not a column, so the token stays one plain term |

Unknown column names stay plain terms, so a filter that happens to contain a colon
keeps working, and each term binds its own parameter rather than being pasted into
the SQL. The table list, the rkyv Records and Strings views, and the file picker
filter the same way; the Hex view is unfiltered (bytes have no rows) and keeps
`/` as a byte search. Matching is case-insensitive substring; the hex byte search
is case-sensitive.

## Write monitor (`w`)

`w` opens a `top` for stores: every shard and database zdbview knows about — the
open file, the recent list and the saved scan — with what is being written to it
right now. It works from the file picker as well as from an open file; it is one
screen shared by both, so the keys and columns are identical.

```
┌ writes — 130 files, 2 active, 70 K in 6s at 29 K/s · sort written ─────────────┐
│kind   file                           size     written   rate       last  activity│
│rkyv   scripts.rkyv                   62 K     62 K      26 K/s     0.0s  ▂▃▄▆▇█ │
│sqlite compsys.db                     187 M    8.8 K     2.9 K/s    0.0s  ▁▁▁▁▁▁ │
│sqlite catalog.db                     48 M     —         —          —            │
└────────────────────────────────────────────────────────────────────────────────┘
```

| Key | Action |
|-----|--------|
| `<` / `>` / `F6` | move the sort to the previous / next column (`s` does `>`) |
| `I` | invert the sort direction |
| click a header | sort by that column; click it again to invert |
| `/` | filter the watched list by path — `Enter` keeps it, `Esc` clears it |
| `p` | pause and resume sampling |
| `+` / `-` | sample faster / slower (100 ms … 5 s, 500 ms default) |
| `Enter` | open the selected file |
| `t` | swap the bottom frame: the log's frames ↔ **bytes written per table** |
| `F` | **walk the log**: every frame, and the rows each one wrote |
| `j` `k`, `PgUp` `PgDn`, `g` `G` | move |
| `w` / `Esc` | back |

Detection is by polling `stat`, not a filesystem notification API: no extra
dependency, identical behaviour on macOS and Linux, and a few hundred `stat` calls
per tick cost less than a frame. The trade is sub-tick resolution — a write that
lands and is undone inside one interval is invisible.

Two things it gets right that a naive size watch does not:

- **SQLite writes land in the `-wal` sidecar first**, often growing it by megabytes
  while the database file itself is untouched until a checkpoint. The sidecar is
  sampled alongside and its growth attributed to the database, so a busy database
  does not read as idle.
- **rkyv shards are rewritten atomically** (temp file plus rename), so they can
  shrink. A shrink counts as activity but adds no bytes to the total, and a
  checkpoint that moves bytes from WAL to database is not counted twice.

`written` is bytes seen since the monitor opened, `rate` averages the last four
samples so one quiet tick does not read as "stopped", and `activity` is the last 24
samples scaled against the busiest row on screen.

Sorting is by column with a direction, the way htop does it — the sorted column
carries an arrow in its header (`size▼`) and the title repeats it. Each column's
two directions are exact mirrors, and the path breaks ties so rows do not shuffle
between samples. Text columns start ascending, numeric ones descending.

### The WAL frame

Under the table sits a second frame: the selected database's write-ahead log,
newest frame last. A `-wal` holds transactions that are already durable but not
yet folded into the database file, so this is the file's immediate future — the
pages it is about to become.

| Column | Meaning |
|--------|---------|
| `frame` | position in the log |
| `page` | which database page this frame carries |
| `table` | which table or index owns that page (see below) |
| `commit` | `commit` ends a transaction; `stale` means a checkpoint has since rolled the salts, so the frame belongs to a log that is already folded away |
| `db pages` | database size in pages after a commit frame |

The title line counts live frames against total, commits, distinct pages pending
checkpoint, log size, the checkpoint sequence number, and any frames written
since the last commit (an open transaction). Only frame *headers* are read — 24
bytes and a seek each — so tailing a 190 MB log costs what tailing an empty one
does. Selecting an rkyv archive, or a database in `journal_mode=delete`, says so
in the frame rather than sitting blank. Below 12 rows of terminal the frame is
dropped so the table stays readable.

### Which table is being written (`t`)

`t` swaps the bottom frame for a per-table breakdown:

```
┌ tables — 1.1 M across 3 objects of history.db · t for frames ─────────────────┐
│table                        written    rate         share                      │
│history                      724 K      148 K/s        64%  ################    │
│index history_ts_idx         352 K      72 K/s         31%  ########            │
│sqlite_schema                 12 K      —               1%                      │
└────────────────────────────────────────────────────────────────────────────────┘
```

This is the thing no other SQLite tool shows: **not how big a table is, but which
one is being written to right now.** It works because a WAL frame header carries
the page number it rewrote, and a page can be traced to the b-tree that owns it:

1. Every table's and index's b-tree is walked from its root page, straight out of
   the file, giving a page → owner map (`recover::page_owners`).
2. The database file alone is the *past* in WAL mode — a table created but not yet
   checkpointed is not in its schema at all — so the log's newest image of each
   page is applied over the file before walking. Without that, every page reads as
   unmapped.
3. Each sample reads only the frame headers written since the last one, so the cost
   does not grow with the log's length.
4. The map is re-read when the log restarts (a checkpoint moves pages) or when a
   frame names a page the map does not cover (the file grew).

`written` is the total since the monitor opened; `rate` is what the *last* sample
attributed, so a table that has stopped reads as idle while its total stays. That
pair is the answer to "which table is being written fastest right now".

Indexes are named as indexes, and a page the map cannot place is shown as
`page N (unmapped)` rather than being folded into a table it might not belong to.
A database in rollback-journal mode gets no breakdown: a journal records the pages
it is *about to* change, not the ones it did.

### Walking the log (`F`)

`F` opens the log itself. A frame does not merely say that a page changed — it
carries the whole page image, so the rows that write put there can be decoded and
shown:

```
┌ 412 frames · 37 commits ────┐┌ what this frame wrote — j/k step · [ ] commits ──┐
│frame  page   what           ││page 6  table leaf  history                       │
│412    2      commit         ││commit — the database was 41 pages after this write│
│411    6      history        ││                                                  │
│410    9      index hist…    ││rowid 118                                         │
│409    6      history        ││              line  git push --force-with-lease    │
│…                            ││             ts_ns  1784952602119000000            │
└─────────────────────────────┘└──────────────────────────────────────────────────┘
```

`j`/`k` step one frame (down is *back* in time), `[` and `]` jump a whole
transaction, `g`/`G` go to the newest and oldest frame. Frames belonging to a log a
checkpoint has already folded away are marked `stale`.

`/` filters the log — by a table or index name, by a page number, or by `commit` /
`stale` — and stepping then walks only what is listed, which is what makes a log
with tens of thousands of frames usable. The title carries the count
(`412/9184 frames`), `Esc` clears the filter and a second `Esc` leaves.

The right pane decodes that one page image with the same record reader the recovery
pass uses, labelling values with the table's column names. A value that continues
onto an overflow page says so rather than showing a truncated string — the rest of
it is in a different frame. Pages that are not table leaves (interior nodes,
index pages, overflow) are named as such instead of pretending to hold rows.

This is the only place in zdbview that reads frame *payloads*, so it is on a
keypress and decodes one frame at a time: a 190 MB log holds tens of thousands of
frames and each payload is a whole page.

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
the working directory. Non-interactively, `--export` takes the sqlite3 shell's
output modes, and `--table` restricts it to one table:

```sh
zdbview data.db --export json               # object of { table: [rows...] }, all tables
zdbview data.db --export csv                # first table as CSV
zdbview data.db --export tsv                # .mode tabs (tabs/newlines escaped)
zdbview data.db --export markdown           # .mode markdown, columns padded
zdbview data.db --export line               # .mode line, one column per line
zdbview data.db --export insert             # .mode insert, one INSERT per row (type-exact)
zdbview data.db --export sql                # .dump — schema and data, replayable
zdbview data.db --export csv --table users  # just that table
zdbview cache.rkyv --export json            # recognized records, value blob as hex
```

`--export sql` is `.dump`: every `CREATE` followed by its rows, wrapped in
`PRAGMA foreign_keys=OFF` / `BEGIN` / `COMMIT`. It and `insert` mode both read
values from the database rather than from the grid, so a blob comes out as `x'…'`
and a real keeps its decimal point — the display string for a blob is a
*description* of it (`<blob 2 bytes>`), which is useless in SQL. Everything that
writes SQL uses the exact form; `csv`, `tsv`, `markdown` and `line` show what the
grid shows. Virtual tables are the case that makes a naive dump
unreplayable: `CREATE VIRTUAL TABLE` runs the module's constructor and builds the
shadow tables, which the dump would then try to create a second time. Like the
shell, zdbview registers the virtual table by writing its `sqlite_schema` row
under `PRAGMA writable_schema=ON` and emits the shadow tables itself with
`CREATE TABLE IF NOT EXISTS`, so an FTS5 index survives the round trip and still
answers `MATCH` queries. Verified line-for-line against `sqlite3 .dump` on a
database with an FTS5 table: identical content, differing only in the order
`sqlite_master` is walked.

## Recovery

`--recover` (and `.recover` in the editor) salvages what a file still holds by
reading **pages**, never opening the database — so it works on a file SQLite
refuses. It walks every page rather than only the ones reachable from
`sqlite_master`, which is what brings back rows a corrupt b-tree root has
orphaned:

```sh
zdbview broken.db --recover > recovered.sql
# recovered 400 rows across 1 table
sqlite3 fixed.db < recovered.sql
```

Measured against a 400-row database with its table's root page zeroed — SQLite
answers `database disk image is malformed` and refuses the rows — `sqlite3
.recover` emits 400 inserts and so does this, and replaying the script produces
data identical to the original. Truncated to 20 of its 35 pages, both recover the
same 226 rows.

The parser reads the parts of the [file format](https://sqlite.org/fileformat2.html)
that hold data: the 100-byte header (page size, reserved bytes, page count), table
b-tree pages, cells with their payload and rowid, the overflow chain when a value
does not fit on a page, and the record's serial types. A 20 KB text value spanning
overflow pages comes back whole.

Pages that cannot be attributed to a table — because the schema is damaged, or
because more than one table has that column count — go to a `lost_and_found` table
with the page number each row came from, as the shell's `.recover` does. Every
assumption the pass made is written into the script as a comment:

```sql
-- t: root page 2 is not a readable table page, so its rows are recovered from unreachable pages instead
-- 33 pages unreachable from any root matched t by column count alone
```

A `WITHOUT ROWID` table keeps its rows in an **index b-tree** rather than a table
b-tree, so recovering it needs three page kinds, not one: table leaves (cells carry
a rowid), index leaves (the key record *is* the row), and index interiors — which,
unlike table interiors, carry key payloads of their own, so some rows live there.
Index cells also keep much less of their payload on the page than table cells do
(`((usable-12)*64/255)-23` against `usable-35`), and using the wrong formula
decodes a short record. Measured on a 300-row keyed table with its root zeroed:
`sqlite3 .recover` brings back 298 rows and so does this, replaying to
`key-0000`…`key-0299`.

Rows recovered from an index leaf carry no rowid, so their `INSERT` does not name
`_rowid_` — naming it for a keyed table is an error and the replay would stop.
Ordinary indexes are skipped rather than read as data: their entries duplicate
columns of a table already being recovered, and emitting them would invent rows.
The script replays each `CREATE INDEX` instead. The file is never modified.

## Backup

`--backup` copies a database with `VACUUM INTO`, which is the shell's `.backup`
and the only way to copy a file that may have writers without stopping them. An
existing target is an error rather than an overwrite:

```sh
zdbview ~/.zshrs/history.db --backup /tmp/history-copy.db
# wrote /tmp/history-copy.db (36864 bytes)
```

## What is ported from DB Browser for SQLite, and what is not

The list below is the action set taken out of DB Browser for SQLite 3.13.1
itself — the `setObjectName` strings in its binary — rather than from its
documentation, so it is what the program actually has.

**Ported.** New / in-memory / read-only databases, attach and detach, write and
revert changes, compact, export to CSV / JSON / SQL, import from CSV and from a
SQL script, projects, recent files. Create and modify table, create index, delete
object, copy CREATE statement, row counts, refresh. Browse Data: per-column
filters, sorting, hide / show / freeze columns, the rowid column, display formats
including a custom expression, conditional formats, find and replace, insert
values, set to NULL, copy column name, copy with hex/ASCII, open in external
application, save filter as view, unlock view editing, clear filters and sorting.
Edit Pragmas. Execute SQL: tabs, open and save `.sql`, execute all, execute
current line, stop, toggle comment, find and replace, word wrap, export results,
save results as view. Integrity check, quick check, foreign-key check, optimize,
load extension, print.

**Not ported, and why:**

| DB Browser feature | Why not |
|--------------------|---------|
| SQLCipher encryption | Needs a SQLCipher build of SQLite in place of the bundled one, which changes what every release binary links against |
| dbhub.io remote (push, fetch, clone, commits) | A network client for a third-party service |
| Rich-text cell formatting (bold, italic, colours, alignment) | A terminal grid has no rich text; conditional formats cover colour |
| Image and PDF preview of a blob cell, print image | No image surface in a terminal — `Ctrl-e` hands the bytes to an application that has one |
| Plot pane | Same reason |
| Import CSV from the clipboard | A terminal can write the system clipboard (OSC 52) but not read it |
| Print the database structure or the SQL transcript | Only the table prints; the rest is what `.schema` and the transcript already show |
| SpatiaLite Geometry to SVG display format | Needs the SpatiaLite extension loaded to mean anything |
| Select whole column | Selection is a mouse-drag idea; `Ctrl-y` copies the name and `x` exports the data |
| Docks, toolbars, "What's this", drag-and-drop SQL options | Qt window furniture with nothing behind it |

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
