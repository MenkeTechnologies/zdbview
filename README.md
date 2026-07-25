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
- `:` — open the **SQL editor** (see below).
- `D` — the **database report** (see below).
- `A` — **column statistics** (see below).
- `e` on a blob cell opens the **hex editor**; text cells edit inline.

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
| zshrs canonical shard | `ZSHS` | magic (or validated, unstamped) | `section/key`, `section[i]` |
| zshrs system shard | `ZSHS` | magic (second layout) | entry key |
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
| `D` | database report: pragmas, integrity check, foreign-key lint, maintenance (SQLite) |
| `A` | column statistics for the table, with a per-column frequency table (SQLite) |
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

History persists to `$XDG_CACHE_HOME/zdbview/sql_history` (200 statements, newlines
escaped so one statement stays one line), written temp-plus-rename like the other
appdata.

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

The lint reads `foreign_key_list`, `index_list` and `index_info` per table and
reports a key only when no index *starts* with the child column, which is the
condition that makes every parent-row change scan the child table. An index on
some other column of the same table does not count.

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
| `commit` | `commit` ends a transaction; `stale` means a checkpoint has since rolled the salts, so the frame belongs to a log that is already folded away |
| `db pages` | database size in pages after a commit frame |

The title line counts live frames against total, commits, distinct pages pending
checkpoint, log size, the checkpoint sequence number, and any frames written
since the last commit (an open transaction). Only frame *headers* are read — 24
bytes and a seek each — so tailing a 190 MB log costs what tailing an empty one
does. Selecting an rkyv archive, or a database in `journal_mode=delete`, says so
in the frame rather than sitting blank. Below 12 rows of terminal the frame is
dropped so the table stays readable.

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
zdbview data.db --export insert             # .mode insert, one INSERT per row
zdbview data.db --export sql                # .dump — schema and data, replayable
zdbview data.db --export csv --table users  # just that table
zdbview cache.rkyv --export json            # recognized records, value blob as hex
```

`--export sql` is `.dump`: every `CREATE` followed by its rows, wrapped in
`PRAGMA foreign_keys=OFF` / `BEGIN` / `COMMIT`, and it reads values from the
database rather than from the grid, so a blob comes out as `x'…'` and a real keeps
its decimal point. Virtual tables are the case that makes a naive dump
unreplayable: `CREATE VIRTUAL TABLE` runs the module's constructor and builds the
shadow tables, which the dump would then try to create a second time. Like the
shell, zdbview registers the virtual table by writing its `sqlite_schema` row
under `PRAGMA writable_schema=ON` and emits the shadow tables itself with
`CREATE TABLE IF NOT EXISTS`, so an FTS5 index survives the round trip and still
answers `MATCH` queries. Verified line-for-line against `sqlite3 .dump` on a
database with an FTS5 table: identical content, differing only in the order
`sqlite_master` is walked.

## Backup

`--backup` copies a database with `VACUUM INTO`, which is the shell's `.backup`
and the only way to copy a file that may have writers without stopping them. An
existing target is an error rather than an overwrite:

```sh
zdbview ~/.zshrs/history.db --backup /tmp/history-copy.db
# wrote /tmp/history-copy.db (36864 bytes)
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
