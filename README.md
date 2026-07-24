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
zdbview path/to/archive.rkyv   # rkyv   → structural inspection
zdbview --sqlite file          # force SQLite
zdbview --rkyv   file          # force rkyv/binary
```

## Install

```sh
git clone https://github.com/MenkeTechnologies/zdbview
cd zdbview && cargo build --release
install -m 755 target/release/zdbview /usr/local/bin/
```

SQLite is compiled in (`rusqlite`'s `bundled` feature), so there is no system
library to install. Tagged releases publish prebuilt binaries for macOS
(arm64 + x86_64) and Linux (glibc + static musl, arm64 + x86_64).

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

## rkyv — structural inspection

rkyv archives are **not self-describing**: the format stores no field names or
type tags (https://rkyv.org/format.html), so the schema cannot be recovered from
an arbitrary archive. Without the originating Rust type, zdbview shows the raw
structure instead:

- `1` **Info** — size and summary.
- `2` **Strings** — every run of printable text embedded in the archive, with
  byte offsets (keys, interned identifiers, string fields).
- `3` **Hex** — `xxd`-style hex/ascii dump of the raw bytes.

Typed field-name CRUD over a rkyv archive requires a supplied schema descriptor
and is planned separately.

## Keys

| Key | Action |
|-----|--------|
| `Tab` | switch focus (table list ↔ rows) |
| arrows / `hjkl` | move |
| `gg` / `G` | jump to top / bottom |
| `/` | search; `n` / `N` next / previous match |
| `Ctrl-f` / `Ctrl-b` | page forward / back (SQLite) |
| `e` `a` `d` `:` | edit / add / delete / SQL (SQLite) |
| `1` `2` `3` | Info / Strings / Hex (rkyv) |
| `q` / `Esc` | quit |

Search scans the loaded page across all columns (SQLite), the string list or raw
bytes (rkyv), or filenames (recent-files picker). Text searches are
case-insensitive and wrap; the hex byte search is case-sensitive and stops at the
end of the file.

## Man pages

```sh
man -l man/man1/zdbview.1        # the standard page
man -l man/man1/zdbviewall.1     # the all-in-one reference, zshall-style

# install
cp man/man1/zdbview*.1 /usr/local/share/man/man1/
```

## Zsh completion

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
