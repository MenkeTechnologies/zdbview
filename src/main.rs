//! zdbview — terminal inspector and CRUD editor for rkyv archives and SQLite
//! databases.
//!
//! SQLite files are fully self-describing, so zdbview offers complete generic
//! CRUD: browse tables, edit any cell, insert/delete rows, run raw SQL.
//!
//! rkyv archives are NOT self-describing (the format stores no field names or
//! type tags — see <https://rkyv.org/format.html>). For an arbitrary archive
//! zdbview therefore provides a structural inspector: hex/ascii dump and the
//! embedded string runs. Typed field-name CRUD requires a supplied schema
//! descriptor (future work).

mod app;
mod browse;
mod clipboard;
mod ddl;
mod designer;
#[cfg(feature = "disasm")]
mod disasm;
mod export;
mod formats;
mod frames;
mod hexedit;
mod import;
mod input;
mod magics;
mod monitor;
mod mru;
mod overlay;
mod pragmas;
mod prefs;
mod project;
mod query;
mod recover;
mod rkyv_inspect;
mod scan;
mod sqledit;
mod sqlite;
mod store;
mod text;
mod theme;
mod wal;
mod watch;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{DefaultTerminal, Terminal};
use std::io;
use std::path::PathBuf;

use store::Store;

/// Help layout, ported from temprs (`tp --help`): banner, coloured usage line,
/// grouped options, then a footer of keys, examples and files.
const HELP_TEMPLATE: &str = "
{before-help}
{about}

\x1b[33m  USAGE:\x1b[0m {usage}

{all-args}
{after-help}";

/// Inner width of the banner's status box.
const BOX_W: usize = 54;

/// The `zdbview` block letters plus a status box, built at runtime so the box
/// stays square whatever the version string's length is.
fn banner() -> String {
    const ART: &str = concat!(
        "\x1b[36m ███████╗██████╗ ██████╗ ██╗   ██╗██╗███████╗██╗    ██╗\x1b[0m\n",
        "\x1b[36m ╚══███╔╝██╔══██╗██╔══██╗██║   ██║██║██╔════╝██║    ██║\x1b[0m\n",
        "\x1b[35m   ███╔╝ ██║  ██║██████╔╝██║   ██║██║█████╗  ██║ █╗ ██║\x1b[0m\n",
        "\x1b[35m  ███╔╝  ██║  ██║██╔══██╗╚██╗ ██╔╝██║██╔══╝  ██║███╗██║\x1b[0m\n",
        "\x1b[31m ███████╗██████╔╝██████╔╝ ╚████╔╝ ██║███████╗╚███╔███╔╝\x1b[0m\n",
        "\x1b[31m ╚══════╝╚═════╝ ╚═════╝   ╚═══╝  ╚═╝╚══════╝ ╚══╝╚══╝\x1b[0m\n",
    );
    let status = format!(
        " rkyv + sqlite  //  magic detection  //  v{}",
        env!("CARGO_PKG_VERSION")
    );
    let rule = "─".repeat(BOX_W);
    format!(
        "{ART}\x1b[36m ┌{rule}┐\x1b[0m\n\
         \x1b[36m │\x1b[0m{status:<BOX_W$}\x1b[36m│\x1b[0m\n\
         \x1b[36m └{rule}┘\x1b[0m\n\
         \x1b[35m  >> BOTH HALVES OF THE CACHE // ONE BINARY <<\x1b[0m"
    )
}

const AFTER_HELP: &str = concat!(
    "\x1b[36m  ── KEYS ───────────────────────────────────────────────\x1b[0m\n",
    "\x1b[32m  //\x1b[0m j/k ←/→ move   Tab focus   Enter detail   / search (n/N)\n",
    "\x1b[32m  //\x1b[0m e a d : edit/add/delete/SQL   s sort   S schema  (SQLite)\n",
    "\x1b[32m  //\x1b[0m D database report   A column stats   Y row as INSERT  (SQLite)\n",
    "\x1b[32m  //\x1b[0m 0 1 2 3 views   a e r d record CRUD   e hex editor  (rkyv)\n",
    "\x1b[32m  //\x1b[0m v value render   y copy (OSC 52)   x export to file\n",
    "\x1b[32m  //\x1b[0m c scheme   C palette   o file list   h/? help   q quit\n",
    "\n",
    "\x1b[36m  ── EXAMPLES ───────────────────────────────────────────\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview                        \x1b[90mrecent files + saved scan\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview ~/.zshrs/scripts.rkyv  \x1b[90mrecords, or structural\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview data.db --export json  \x1b[90mdump every table\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview data.db --export sql   \x1b[90mreplayable .dump\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview d.db --import r.csv --table t  \x1b[90mload a CSV\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview --rescan               \x1b[90mwalk again now\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview --list-themes          \x1b[90mpreview the schemes\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview --formats              \x1b[90mrkyv magics it knows\x1b[0m\n",
    "\n",
    "\x1b[36m  ── FILES ──────────────────────────────────────────────\x1b[0m\n",
    "\x1b[32m  //\x1b[0m $XDG_CACHE_HOME/zdbview/recent  \x1b[90mrecent-files list\x1b[0m\n",
    "\x1b[32m  //\x1b[0m $XDG_CACHE_HOME/zdbview/scan    \x1b[90msaved scan results\x1b[0m\n",
    "\x1b[32m  //\x1b[0m $XDG_CONFIG_HOME/zdbview/prefs  \x1b[90mscheme + palette\x1b[0m\n",
    "\x1b[32m  //\x1b[0m $XDG_CONFIG_HOME/zdbview/magics \x1b[90mextra rkyv tags\x1b[0m\n",
    "\n",
    "\x1b[36m  ── SYSTEM ─────────────────────────────────────────────\x1b[0m\n",
    "\x1b[35m  v",
    env!("CARGO_PKG_VERSION"),
    " \x1b[0m// \x1b[33mratatui + crossterm // rkyv 0.7 // bundled sqlite\x1b[0m\n",
    "\x1b[35m  The magic decides the backend, never the file name.\x1b[0m\n",
    "\x1b[33m  >>> OPEN THE SHARD. EDIT THE BYTES. WRITE IT BACK. <<<\x1b[0m\n",
    "\x1b[36m ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░\x1b[0m"
);

/// Option-group headings; clap appends the colon.
const H_POSITIONAL: &str = "\x1b[36m  ── FILE\x1b[0m";
const H_BACKEND: &str = "\x1b[36m  ── BACKEND\x1b[0m";
const H_OUTPUT: &str = "\x1b[36m  ── OUTPUT\x1b[0m";
const H_DISCOVERY: &str = "\x1b[36m  ── DISCOVERY\x1b[0m";
const H_APPEARANCE: &str = "\x1b[36m  ── APPEARANCE\x1b[0m";
const H_GENERAL: &str = "\x1b[36m  ── GENERAL\x1b[0m";

#[derive(Parser)]
#[command(
    name = "zdbview",
    version,
    about = "\x1b[0m  A terminal inspector and CRUD editor for rkyv archives and SQLite databases.",
    help_template = HELP_TEMPLATE,
    before_help = banner(),
    after_help = AFTER_HELP,
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Cli {
    #[arg(
        value_name = "FILE",
        help_heading = H_POSITIONAL,
        help = "\x1b[32m//\x1b[0m File to open; omit for the picker (recent + scan)"
    )]
    file: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with = "rkyv",
        help_heading = H_BACKEND,
        help = "\x1b[32m//\x1b[0m Force the SQLite backend, skipping detection"
    )]
    sqlite: bool,
    #[arg(
        long,
        help_heading = H_BACKEND,
        help = "\x1b[32m//\x1b[0m Force the rkyv/binary backend, skipping detection"
    )]
    rkyv: bool,
    #[arg(
        long,
        value_name = "FORMAT",
        help_heading = H_OUTPUT,
        help = "\x1b[32m//\x1b[0m Dump to stdout and exit: json csv tsv markdown insert line sql"
    )]
    export: Option<String>,
    #[arg(
        long,
        value_name = "TABLE",
        help_heading = H_OUTPUT,
        help = "\x1b[32m//\x1b[0m Restrict --export to one table"
    )]
    table: Option<String>,
    #[arg(
        long,
        value_name = "FILE",
        help_heading = H_OUTPUT,
        help = "\x1b[32m//\x1b[0m Copy the database with VACUUM INTO and exit"
    )]
    backup: Option<PathBuf>,
    #[arg(
        long,
        value_name = "FILE",
        help_heading = H_OUTPUT,
        help = "\x1b[32m//\x1b[0m Load a CSV (or TSV) into --table and exit"
    )]
    import: Option<PathBuf>,
    #[arg(
        long,
        help_heading = H_OUTPUT,
        help = "\x1b[32m//\x1b[0m Salvage rows page by page to stdout as SQL, and exit"
    )]
    recover: bool,
    #[arg(
        long,
        value_name = "FILE",
        help_heading = H_BACKEND,
        help = "\x1b[32m//\x1b[0m Create an empty database and open it"
    )]
    new: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with = "new",
        help_heading = H_BACKEND,
        help = "\x1b[32m//\x1b[0m Open a database that lives only in memory"
    )]
    memory: bool,
    #[arg(
        long,
        help_heading = H_BACKEND,
        help = "\x1b[32m//\x1b[0m Open read-only: every edit is refused"
    )]
    readonly: bool,
    #[arg(
        long,
        value_name = "FILE",
        help_heading = H_OUTPUT,
        help = "\x1b[32m//\x1b[0m Run a file of SQL into the database and exit"
    )]
    import_sql: Option<PathBuf>,
    #[arg(
        long,
        value_name = "FILE",
        help_heading = H_BACKEND,
        help = "\x1b[32m//\x1b[0m Load a SQLite extension before opening (repeatable)"
    )]
    load_extension: Vec<PathBuf>,
    #[arg(
        long,
        value_name = "FILE",
        help_heading = H_BACKEND,
        help = "\x1b[32m//\x1b[0m Open the database a project names, with its settings"
    )]
    project: Option<PathBuf>,
    #[arg(
        long,
        value_name = "NAME",
        help_heading = H_APPEARANCE,
        help = "\x1b[32m//\x1b[0m Colour scheme for this run (see --list-themes)"
    )]
    theme: Option<String>,
    #[arg(
        long,
        help_heading = H_APPEARANCE,
        help = "\x1b[32m//\x1b[0m Preview every scheme with its palette and exit"
    )]
    list_themes: bool,
    #[arg(
        long,
        help_heading = H_GENERAL,
        help = "\x1b[32m//\x1b[0m List every rkyv magic this build knows and exit"
    )]
    formats: bool,
    #[arg(
        long,
        value_name = "DIR",
        help_heading = H_DISCOVERY,
        help = "\x1b[32m//\x1b[0m Scan DIR instead of the default roots (repeatable)"
    )]
    scan: Vec<PathBuf>,
    #[arg(
        long,
        conflicts_with = "scan",
        help_heading = H_DISCOVERY,
        help = "\x1b[32m//\x1b[0m Skip the scan; list only recent files"
    )]
    no_scan: bool,
    #[arg(
        long,
        conflicts_with = "no_scan",
        help_heading = H_DISCOVERY,
        help = "\x1b[32m//\x1b[0m Walk again now, ignoring the saved scan"
    )]
    rescan: bool,
    #[arg(
        short,
        long,
        action = clap::ArgAction::Help,
        help_heading = H_GENERAL,
        help = "\x1b[32m//\x1b[0m Print this help"
    )]
    help: Option<bool>,
    #[arg(
        short = 'V',
        long,
        action = clap::ArgAction::Version,
        help_heading = H_GENERAL,
        help = "\x1b[32m//\x1b[0m Print the version"
    )]
    version: Option<bool>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.list_themes {
        list_themes();
        return Ok(());
    }

    if cli.formats {
        list_formats();
        return Ok(());
    }

    // `--export` is non-interactive: dump to stdout without touching the TUI.
    if let Some(fmt) = &cli.export {
        return run_export(&cli, fmt);
    }
    // `--backup` copies the database with VACUUM INTO, the one consistent way to
    // copy a file that may have writers.
    if let Some(dest) = &cli.backup {
        return run_backup(&cli, dest);
    }
    if let Some(src) = &cli.import {
        return run_import(&cli, src);
    }
    // `--import-sql` replays a script, the other half of `--export sql`.
    if let Some(src) = &cli.import_sql {
        return run_import_sql(&cli, src);
    }
    // `--recover` reads pages, never opening the database, so it works on a file
    // SQLite itself refuses.
    if cli.recover {
        return run_recover(&cli);
    }

    // `--new` makes the file before anything tries to detect its type.
    if let Some(path) = &cli.new {
        sqlite::SqliteStore::create(path)?;
        println!("created {}", path.display());
    }

    // Resolve --theme before touching the terminal so a typo prints plainly
    // instead of after an alternate-screen round trip.
    let theme = cli.theme.as_deref().map(parse_theme).transpose()?;

    // Manual terminal setup (ported from iftoprs) so mouse capture is enabled
    // in the same sequence as entering the alternate screen — `ratatui::init()`
    // does not capture the mouse, so scroll/click events never arrive.
    let mut terminal = setup_terminal()?;
    let res = run(&cli, &mut terminal, theme);
    restore_terminal(&mut terminal);
    res
}

/// Enter raw mode + alternate screen with mouse capture, matching iftoprs.
fn setup_terminal() -> Result<DefaultTerminal> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

/// Reverse of `setup_terminal`; best-effort so a partial teardown still leaves
/// the terminal usable.
fn restore_terminal(terminal: &mut DefaultTerminal) {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();
}

/// Print every scheme as `token`, display name and a 6-cell palette swatch
/// (port of iftoprs's `--list-colors`).
fn list_themes() {
    use theme::{Theme, ThemeName};
    const RST: &str = "\x1b[0m";
    const B_CYAN: &str = "\x1b[1;36m";
    const B_GREEN: &str = "\x1b[1;32m";
    const B_MAGENTA: &str = "\x1b[1;35m";
    const B_YELLOW: &str = "\x1b[1;33m";

    println!(
        "\n{B_CYAN}  ── COLOR SCHEMES ({}) ────────────────────────{RST}\n",
        ThemeName::ALL.len()
    );
    for &name in ThemeName::ALL {
        let swatch: String = Theme::swatch(name)
            .iter()
            .map(|(color, _)| match color {
                ratatui::style::Color::Indexed(n) => format!("\x1b[48;5;{n}m   {RST}"),
                _ => "   ".to_string(),
            })
            .collect();
        println!(
            "  {B_GREEN}{token:<14}{RST} {B_MAGENTA}{display:<14}{RST} {swatch}",
            token = name.token(),
            display = name.display(),
        );
    }
    println!("\n  {B_YELLOW}Use:{RST}    zdbview FILE {B_GREEN}--theme neon_sprawl{RST}");
    println!("  {B_YELLOW}In TUI:{RST} press {B_GREEN}t{RST} for the chooser, {B_GREEN}e{RST} for the palette editor\n");
}

/// What `--export` accepts: the sqlite3 shell's output modes, plus `sql` for
/// `.dump`. Named here rather than inside the function because the completion,
/// the man page and the tests all have to agree with it.
const EXPORT_FORMATS: &[&str] = &["json", "csv", "tsv", "markdown", "insert", "line", "sql"];

/// `--formats`: every rkyv magic the registry resolves — the tags this build
/// ships and whatever the user registered on top of them — so "does zdbview know
/// my producer" is answerable without opening a file. A tag is printed as its
/// four characters when they are printable, since that is how a producer writes
/// it, plus the u32 the bytes actually carry.
fn list_formats() {
    const RST: &str = "\x1b[0m";
    const DIM: &str = "\x1b[90m";
    const B_GREEN: &str = "\x1b[1;32m";
    const B_YELLOW: &str = "\x1b[1;33m";

    println!("  {B_YELLOW}rkyv magics{RST}  {DIM}(tag, u32, name){RST}");
    for (magic, name) in magics::all() {
        let bytes = magic.to_be_bytes();
        let tag = if bytes.iter().all(|b| b.is_ascii_graphic()) {
            String::from_utf8_lossy(&bytes).to_string()
        } else {
            "    ".to_string()
        };
        let shipped = formats::BUILTIN_MAGICS.iter().any(|(m, _)| m == magic);
        println!(
            "  {B_GREEN}{tag}{RST}  {DIM}{magic:#010x}{RST}  {name}{}",
            if shipped {
                String::new()
            } else {
                format!("  {DIM}(registered){RST}")
            }
        );
    }
    match magics::registry_path() {
        Some(p) => println!("\n  {DIM}register more in{RST} {}", p.display()),
        None => println!("\n  {DIM}no config directory: set XDG_CONFIG_HOME or HOME{RST}"),
    }
}

/// Resolve `--theme` to a scheme, erroring with the valid tokens on a typo.
fn parse_theme(token: &str) -> Result<theme::ThemeName> {
    theme::ThemeName::from_token(token).ok_or_else(|| {
        anyhow!(
            "unknown theme '{}' — run --list-themes for the {} valid names",
            token,
            theme::ThemeName::ALL.len()
        )
    })
}

fn run_export(cli: &Cli, fmt: &str) -> Result<()> {
    let fmt = fmt.to_lowercase();
    if !EXPORT_FORMATS.contains(&fmt.as_str()) {
        return Err(anyhow!(
            "--export expects one of {}, got '{}'",
            EXPORT_FORMATS.join(" "),
            fmt
        ));
    }
    let file = cli
        .file
        .clone()
        .ok_or_else(|| anyhow!("--export requires a file argument"))?;
    let kind = store::detect(&file, cli.sqlite, cli.rkyv)?;
    let (store, _) = store::Store::open(&file, kind)?;
    print!("{}", export_store(&store, &fmt, cli.table.as_deref())?);
    Ok(())
}

/// `--recover`: salvage what the file still holds, as a replayable script. Reads
/// pages directly, so a corrupt or truncated database is still readable.
fn run_recover(cli: &Cli) -> Result<()> {
    let file = cli
        .file
        .clone()
        .ok_or_else(|| anyhow!("--recover requires a file argument"))?;
    let found = recover::recover(&file)?;
    let (tables, rows, orphans) = (found.tables.len(), found.rows.len(), found.orphans());
    print!("{}", recover::to_sql(&found));
    // The summary goes to stderr so the script on stdout stays replayable.
    eprintln!(
        "recovered {} row{} across {} table{}{}",
        rows,
        if rows == 1 { "" } else { "s" },
        tables,
        if tables == 1 { "" } else { "s" },
        if orphans > 0 {
            format!(", {orphans} in lost_and_found")
        } else {
            String::new()
        }
    );
    Ok(())
}

/// `--import`: load a CSV or TSV into an existing table, the shell's `.import`.
/// The separator follows the file's extension, since that is what distinguishes
/// the two formats.
fn run_import(cli: &Cli, src: &std::path::Path) -> Result<()> {
    let file = cli
        .file
        .clone()
        .ok_or_else(|| anyhow!("--import requires a file argument (the database)"))?;
    let table = cli
        .table
        .clone()
        .ok_or_else(|| anyhow!("--import requires --table to say where the rows go"))?;
    let kind = store::detect(&file, cli.sqlite, cli.rkyv)?;
    if kind != store::Kind::Sqlite {
        return Err(anyhow!("--import only applies to a SQLite database"));
    }
    let text =
        std::fs::read_to_string(src).with_context(|| format!("cannot read {}", src.display()))?;
    let sep = match src.extension().and_then(|e| e.to_str()) {
        Some("tsv") | Some("tab") => '\t',
        _ => ',',
    };
    let csv = import::parse(&text, sep)?;
    let (store, _) = store::Store::open(&file, kind)?;
    let n = match &store {
        Store::Sqlite(s) => {
            let n = s.import_rows(&table, &csv.header, &csv.rows)?;
            // A write from the TUI is buffered until the user writes it; a
            // one-shot command has no user to ask, so it commits before exiting.
            s.write_changes()?;
            n
        }
        _ => return Err(anyhow!("not a SQLite database")),
    };
    println!("imported {} rows into {}", n, table);
    Ok(())
}

/// `--backup`: copy the database with `VACUUM INTO`, the shell's `.backup`.
fn run_backup(cli: &Cli, dest: &std::path::Path) -> Result<()> {
    let file = cli
        .file
        .clone()
        .ok_or_else(|| anyhow!("--backup requires a file argument"))?;
    let kind = store::detect(&file, cli.sqlite, cli.rkyv)?;
    if kind != store::Kind::Sqlite {
        return Err(anyhow!("--backup only applies to a SQLite database"));
    }
    if dest.exists() {
        return Err(anyhow!("{} already exists", dest.display()));
    }
    let (store, _) = store::Store::open(&file, kind)?;
    match &store {
        Store::Sqlite(s) => s.backup_to(dest)?,
        _ => return Err(anyhow!("not a SQLite database")),
    }
    let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    println!("wrote {} ({} bytes)", dest.display(), size);
    Ok(())
}

fn export_store(store: &Store, fmt: &str, only: Option<&str>) -> Result<String> {
    match store {
        Store::Sqlite(s) => {
            // `sql` is the shell's `.dump`: schema and data for the whole database
            // or one table.
            if fmt == "sql" {
                return s.dump(only);
            }
            // Which tables to write: the named one, else the first for the
            // row-oriented modes, else all of them for JSON.
            let tables: Vec<String> = match only {
                Some(t) => {
                    if !s.tables.iter().any(|x| x == t) {
                        return Err(anyhow!("no such table: {t}"));
                    }
                    vec![t.to_string()]
                }
                None if fmt == "json" => s.tables.clone(),
                None => s
                    .tables
                    .first()
                    .cloned()
                    .map(|t| vec![t])
                    .ok_or_else(|| anyhow!("no tables to export"))?,
            };
            let page = |t: &str| -> Result<crate::sqlite::RowsView> {
                let total = s.count(t)?;
                s.rows(&crate::sqlite::PageQuery::all(t, total.max(1)))
            };
            match fmt {
                "csv" => {
                    let v = page(&tables[0])?;
                    Ok(export::rows_to_csv(&v.columns, &v.rows))
                }
                "tsv" => {
                    let v = page(&tables[0])?;
                    Ok(export::rows_to_tsv(&v.columns, &v.rows))
                }
                "markdown" => {
                    let v = page(&tables[0])?;
                    Ok(export::rows_to_markdown(&v.columns, &v.rows))
                }
                "line" => {
                    let v = page(&tables[0])?;
                    Ok(export::rows_to_lines(&v.columns, &v.rows))
                }
                // Read as literals, not as the strings the grid shows: `insert`
                // mode is meant to round-trip, and a display string describes a
                // blob rather than carrying it.
                "insert" => {
                    let mut out = String::new();
                    for t in &tables {
                        let columns = s.columns(t)?;
                        let literals = s.literal_rows(t)?;
                        out.push_str(&export::rows_to_inserts_exact(t, &columns, &literals));
                    }
                    Ok(out)
                }
                // JSON: object of { table_name: [rows...] }.
                _ => {
                    let mut out = String::from("{");
                    for (i, t) in tables.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        let v = page(t)?;
                        out.push_str(&export::json_escape(t));
                        out.push(':');
                        out.push_str(&export::rows_to_json(&v.columns, &v.rows));
                    }
                    out.push('}');
                    out.push('\n');
                    Ok(out)
                }
            }
        }
        Store::Rkyv(r) => {
            let d = formats::try_decode(&r.bytes)
                .ok_or_else(|| anyhow!("unrecognized rkyv archive — nothing to export"))?;
            let recs: Vec<export::RecordExport> = d
                .records
                .iter()
                .map(|rec| export::RecordExport {
                    key: &rec.key,
                    fields: &rec.fields,
                    value: &rec.value,
                })
                .collect();
            let mut out = export::records_to_json(&recs);
            out.push('\n');
            Ok(out)
        }
    }
}

/// The two terminal-driven steps of a session, behind a trait so the loop that
/// sequences them can be tested without a terminal.
trait Session {
    /// Ask the picker for a file. `None` means the user quit it.
    fn pick(&mut self, scheme: theme::Theme) -> Result<Option<app::Picked>>;
    /// Open a file and run the app over it, reporting the scheme it ended on and
    /// any file it wants opened next.
    fn open(
        &mut self,
        scheme: theme::Theme,
        file: PathBuf,
    ) -> Result<(app::Outcome, theme::Theme, Option<PathBuf>)>;
}

/// The real session: a picker and an app over the terminal.
struct TerminalSession<'a> {
    cli: &'a Cli,
    terminal: &'a mut DefaultTerminal,
    /// A project to apply to the first database opened, from `--project`.
    project: Option<project::Project>,
}

impl Session for TerminalSession<'_> {
    fn pick(&mut self, scheme: theme::Theme) -> Result<Option<app::Picked>> {
        pick(self.cli, self.terminal, scheme)
    }

    fn open(
        &mut self,
        scheme: theme::Theme,
        file: PathBuf,
    ) -> Result<(app::Outcome, theme::Theme, Option<PathBuf>)> {
        // The project applies to the database it named, which is the first one
        // opened; after that the session is the user's.
        open_and_run(self.cli, self.terminal, scheme, file, self.project.take())
    }
}

fn run(cli: &Cli, terminal: &mut DefaultTerminal, theme: Option<theme::ThemeName>) -> Result<()> {
    // The scheme is carried from screen to screen instead of being re-read from
    // prefs at every hop: another instance writing prefs must not change what
    // this one is showing.
    let scheme = app::resolve_theme(theme);
    // `--new` opens the database it just made; `--memory` opens one that never
    // reaches a disk, so it has no path to hand the loop and runs on its own.
    if cli.memory {
        let store = store::Store::Sqlite(sqlite::SqliteStore::open_memory()?);
        for ext in &cli.load_extension {
            if let store::Store::Sqlite(s) = &store {
                s.load_extension(ext)?;
            }
        }
        let mut app = app::App::with_theme(store, scheme);
        app.run(terminal)?;
        return Ok(());
    }
    // `--project` says which database to open and how to show it.
    let project = match &cli.project {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            Some(
                project::Project::parse(&text)
                    .ok_or_else(|| anyhow!("{} is not a zdbview project", path.display()))?,
            )
        }
        None => None,
    };
    let first = cli
        .file
        .clone()
        .or_else(|| cli.new.clone())
        .or_else(|| project.as_ref().map(|p| p.database.clone()));
    let mut session = TerminalSession {
        cli,
        terminal,
        project,
    };
    drive(&mut session, first, scheme)
}

/// Sequence a whole session: an explicit FILE opens first, then each round either
/// takes the file the write monitor handed over or asks the picker, and the scheme
/// each screen ended on is carried to the next instead of being re-read from
/// prefs. `o` or Esc inside a file lead back to the picker, so the explicit-file
/// case falls into the loop rather than exiting.
fn drive(
    session: &mut dyn Session,
    first: Option<PathBuf>,
    mut scheme: theme::Theme,
) -> Result<()> {
    // A file the write monitor asked to open, which skips the picker.
    let mut pending: Option<PathBuf> = None;

    if let Some(file) = first {
        let (outcome, used, next) = session.open(scheme, file)?;
        if outcome == app::Outcome::Quit {
            return Ok(());
        }
        scheme = used;
        pending = next;
    }
    loop {
        let file = match pending.take() {
            Some(f) => f,
            None => match session.pick(scheme)? {
                Some(picked) => {
                    scheme = picked.theme;
                    picked.path
                }
                None => return Ok(()), // user quit the picker
            },
        };
        let (outcome, used, next) = session.open(scheme, file)?;
        if outcome == app::Outcome::Quit {
            return Ok(());
        }
        scheme = used;
        pending = next;
    }
}

/// Show the picker: recent files, plus the saved scan rows, plus whatever a walk
/// started here adds to them.
fn pick(
    cli: &Cli,
    terminal: &mut DefaultTerminal,
    scheme: theme::Theme,
) -> Result<Option<app::Picked>> {
    let recent: Vec<mru::Entry> = mru::load()
        .into_iter()
        .filter(|e| e.path.exists())
        .collect();

    // Explicit --scan roots are not the default set, so they neither read nor
    // write the saved scan.
    let custom_roots = !cli.scan.is_empty();
    let roots: Vec<scan::Root> = if custom_roots {
        cli.scan
            .iter()
            .map(|p| scan::Root::new(p.clone(), scan::DEEP))
            .collect()
    } else {
        scan::default_roots()
    };

    let saved = if cli.no_scan || cli.rescan || custom_roots {
        None
    } else {
        scan::load_cache()
    };
    // The saved rows are shown either way; what the check decides is how much of
    // the disk is walked behind them. An index the filesystem has not
    // invalidated means no walk at all, however old it is; one whose watched
    // directories have moved means a walk of those directories, not of `/`; an
    // index left unfinished means the full walk again.
    let (cached, cache_age, todo, refresh) = match saved {
        Some(c) => {
            let age = c.age();
            let todo = c.refresh_roots(&roots);
            (c.hits, Some(age), todo, c.complete)
        }
        None if cli.no_scan => (Vec::new(), None, Vec::new(), false),
        None => (Vec::new(), None, roots.clone(), false),
    };

    app::pick_mru(
        terminal,
        app::Picker {
            recent: &recent,
            theme: scheme,
            cached,
            cache_age,
            scan: (!todo.is_empty()).then(|| scan::spawn(todo.clone())),
            walking: todo,
            refresh,
            // What `r` walks: the whole default set, never the refresh subset.
            roots,
            persist: !custom_roots,
        },
    )
}

/// Open `file` and run the app over it in `scheme`, reporting the scheme it ended
/// on so the next screen keeps it.
fn open_and_run(
    cli: &Cli,
    terminal: &mut DefaultTerminal,
    scheme: theme::Theme,
    file: PathBuf,
    project: Option<project::Project>,
) -> Result<(app::Outcome, theme::Theme, Option<PathBuf>)> {
    let kind = store::detect(&file, cli.sqlite, cli.rkyv)?;
    // `--readonly` is a different connection, not a flag checked at the edit: a
    // reader cannot be made to write by a bug in a key handler.
    let (store, actual) = if cli.readonly && kind == store::Kind::Sqlite {
        (
            store::Store::Sqlite(sqlite::SqliteStore::open_readonly(&file)?),
            store::Kind::Sqlite,
        )
    } else {
        store::Store::open(&file, kind)?
    };
    if let store::Store::Sqlite(s) = &store {
        for ext in &cli.load_extension {
            s.load_extension(ext)?;
        }
    }
    mru::record(&file, actual);
    let mut app = app::App::with_theme(store, scheme);
    if let Some(p) = project {
        app.apply_project(&p);
    }
    let outcome = app.run(terminal)?;
    Ok((outcome, app.theme(), app.open_next()))
}

/// `--import-sql`: run a file of SQL into the database, DB Browser's "Import
/// from SQL file". The whole script is one savepoint, so a script that fails
/// half way leaves nothing behind.
fn run_import_sql(cli: &Cli, src: &std::path::Path) -> Result<()> {
    let file = cli
        .file
        .clone()
        .ok_or_else(|| anyhow!("--import-sql requires a file argument (the database)"))?;
    let kind = store::detect(&file, cli.sqlite, cli.rkyv)?;
    if kind != store::Kind::Sqlite {
        return Err(anyhow!("--import-sql only applies to a SQLite database"));
    }
    let script =
        std::fs::read_to_string(src).with_context(|| format!("cannot read {}", src.display()))?;
    let (store, _) = store::Store::open(&file, kind)?;
    match &store {
        Store::Sqlite(s) => {
            let n = s.import_sql(&script)?;
            // A one-shot command has no user to ask, so it writes before exiting.
            s.write_changes()?;
            println!(
                "ran {} into {}, {n} row(s) changed",
                src.display(),
                file.display()
            );
        }
        _ => return Err(anyhow!("not a SQLite database")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        drive, export_store, run_backup, run_export, run_import, run_import_sql, run_recover,
        store, Cli, Session, EXPORT_FORMATS,
    };
    use crate::app::{Outcome, Picked};
    use crate::theme::{Theme, ThemeName};
    use clap::Parser;
    use std::path::Path;
    use std::path::PathBuf;

    /// What the loop did, with the two terminal steps scripted.
    #[derive(Default)]
    struct Fake {
        /// Files the picker will hand over, in order; `None` ends the session.
        picks: Vec<Option<Picked>>,
        /// What each open reports: outcome, the scheme it ended on, the next file.
        opens: Vec<(Outcome, Theme, Option<PathBuf>)>,
        /// Every open, as (file, scheme it was given).
        opened: Vec<(PathBuf, ThemeName)>,
        /// The scheme the picker was called with, once per call.
        picked_with: Vec<ThemeName>,
    }

    impl Session for Fake {
        fn pick(&mut self, scheme: Theme) -> anyhow::Result<Option<Picked>> {
            self.picked_with.push(scheme.name);
            Ok(self.picks.remove(0))
        }

        fn open(
            &mut self,
            scheme: Theme,
            file: PathBuf,
        ) -> anyhow::Result<(Outcome, Theme, Option<PathBuf>)> {
            self.opened.push((file, scheme.name));
            Ok(self.opens.remove(0))
        }
    }

    fn theme(name: ThemeName) -> Theme {
        Theme::from_name(name)
    }

    /// A database with one of everything the writers have to quote.
    fn fixture(name: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("zdbview_cli_{}_{seq}_{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (a TEXT, raw BLOB, n REAL)")
            .unwrap();
        conn.execute(
            "INSERT INTO t VALUES (?1, ?2, ?3)",
            rusqlite::params!["it's here", vec![0x00u8, 0xff], 1.5f64],
        )
        .unwrap();
        drop(conn);
        path
    }

    fn cli(args: &[&str]) -> Cli {
        let mut all = vec!["zdbview"];
        all.extend_from_slice(args);
        Cli::parse_from(all)
    }

    /// Every `--export` format, over the store rather than over stdout. The blob is
    /// the case that separates them: only the two SQL forms can carry it.
    #[test]
    fn export_formats_render_what_they_claim() {
        let path = fixture("export.db");
        let kind = store::detect(&path, false, false).unwrap();
        let (st, _) = store::Store::open(&path, kind).unwrap();

        let csv = export_store(&st, "csv", None).unwrap();
        assert!(csv.starts_with("a,raw,n\n"), "{csv}");
        assert!(
            csv.contains("<blob 2 bytes>"),
            "the grid describes a blob: {csv}"
        );

        let tsv = export_store(&st, "tsv", None).unwrap();
        assert!(tsv.contains('\t') && !tsv.contains(",<blob"), "{tsv}");

        let md = export_store(&st, "markdown", None).unwrap();
        assert!(md.lines().nth(1).unwrap().starts_with("|--"), "{md}");

        let line = export_store(&st, "line", None).unwrap();
        assert!(line.contains("  a = it's here"), "{line}");

        // `insert` reads literals, so the blob survives as bytes and the quote is
        // doubled — this is what the display-string path used to get wrong.
        let ins = export_store(&st, "insert", None).unwrap();
        assert!(ins.contains("x'00ff'"), "insert must carry the blob: {ins}");
        assert!(ins.contains("'it''s here'"), "{ins}");
        assert!(!ins.contains("<blob"), "no descriptions in SQL: {ins}");

        let dump = export_store(&st, "sql", None).unwrap();
        assert!(
            dump.contains("CREATE TABLE t") && dump.contains("x'00ff'"),
            "{dump}"
        );

        let json = export_store(&st, "json", None).unwrap();
        assert!(json.starts_with("{\"t\":[{"), "{json}");

        // `--table` narrows, and an unknown name is an error rather than nothing.
        assert!(export_store(&st, "csv", Some("t")).is_ok());
        assert!(export_store(&st, "csv", Some("nope"))
            .unwrap_err()
            .to_string()
            .contains("no such table"));
        let _ = std::fs::remove_file(path);
    }

    /// Every accepted format has to render, or a name in the list is a promise
    /// the binary does not keep. Looping over the list rather than naming them
    /// means a format added later is exercised without anyone remembering to.
    #[test]
    fn every_accepted_export_format_produces_output() {
        let path = fixture("all_formats.db");
        let kind = store::detect(&path, false, false).unwrap();
        let (st, _) = store::Store::open(&path, kind).unwrap();
        for fmt in EXPORT_FORMATS {
            let out = export_store(&st, fmt, None)
                .unwrap_or_else(|e| panic!("--export {fmt} failed: {e}"));
            assert!(!out.trim().is_empty(), "--export {fmt} wrote nothing");
            // Every mode carries the row's text; only the shape differs.
            assert!(
                out.contains("it's here") || out.contains("it''s here"),
                "--export {fmt} lost the data: {out}"
            );
        }
        drop(st);
        let _ = std::fs::remove_file(&path);
    }

    /// The list lives in three places a user meets it: the flag itself, the zsh
    /// completion and the man page. They drift silently, so they are compared.
    #[test]
    fn the_completion_and_the_man_page_list_the_same_formats() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

        let completion = std::fs::read_to_string(root.join("completions/_zdbview")).unwrap();
        let line = completion
            .lines()
            .find(|l| l.contains("--export["))
            .expect("the completion offers --export");
        let offered = line
            .rsplit_once(":format:(")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(list, _)| list)
            .expect("with a value list");
        assert_eq!(
            offered.split_whitespace().collect::<Vec<_>>(),
            EXPORT_FORMATS.to_vec(),
            "the completion offers exactly what the flag accepts, in the same order"
        );

        // The man page's own --export paragraph, not the page at large: every
        // format has to be named where a reader looks for it.
        let man = std::fs::read_to_string(root.join("man/man1/zdbview.1")).unwrap();
        let para = man
            .split_once(".BI \\-\\-export")
            .map(|(_, rest)| rest.split("\n.TP\n").next().unwrap_or(rest))
            .expect("the man page documents --export");
        for fmt in EXPORT_FORMATS {
            assert!(
                para.contains(fmt),
                "the man page's --export paragraph does not name {fmt}"
            );
        }
    }

    /// `--export sql` is advertised as a replayable dump, so it has to replay:
    /// into an empty database, through the same `--import-sql` path a user would
    /// use, reproducing the schema and every value including the blob, the real
    /// and the embedded quote.
    #[test]
    fn a_sql_dump_replays_into_an_empty_database() {
        let src = fixture("dump_src.db");
        let kind = store::detect(&src, false, false).unwrap();
        let (st, _) = store::Store::open(&src, kind).unwrap();
        let dump = export_store(&st, "sql", None).unwrap();
        drop(st);

        let script =
            std::env::temp_dir().join(format!("zdbview_cli_replay_{}.sql", std::process::id()));
        std::fs::write(&script, &dump).unwrap();
        // An empty database, made the way `--new` makes one.
        let dest =
            std::env::temp_dir().join(format!("zdbview_cli_replay_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dest);
        crate::sqlite::SqliteStore::create(&dest).unwrap();

        run_import_sql(
            &cli(&[
                dest.to_str().unwrap(),
                "--import-sql",
                script.to_str().unwrap(),
            ]),
            &script,
        )
        .unwrap();

        let conn = rusqlite::Connection::open(&dest).unwrap();
        let (a, raw, n): (String, Vec<u8>, f64) = conn
            .query_row("SELECT a, raw, n FROM t", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(a, "it's here", "the quote survived the round trip");
        assert_eq!(
            raw,
            vec![0x00, 0xff],
            "and the blob is bytes, not a description"
        );
        assert_eq!(n, 1.5);
        drop(conn);

        for p in [src, dest, script] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// The four non-interactive entry points refuse what they cannot do, with a
    /// message that says which requirement was missed.
    #[test]
    fn the_cli_entry_points_report_what_they_need() {
        let db = fixture("entry.db");
        let db_arg = db.to_str().unwrap();
        let text =
            std::env::temp_dir().join(format!("zdbview_cli_text_{}.txt", std::process::id()));
        std::fs::write(&text, "not a database, just words\n".repeat(6)).unwrap();
        let text_arg = text.to_str().unwrap();

        // --export
        let err = run_export(&cli(&[db_arg, "--export", "yaml"]), "yaml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--export expects one of"), "{err}");
        let err = run_export(&cli(&["--export", "csv"]), "csv")
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires a file"), "{err}");

        // --backup: needs a file, refuses a non-database, refuses to overwrite.
        let err = run_backup(&cli(&["--backup", "/tmp/x.db"]), Path::new("/tmp/x.db"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires a file"), "{err}");
        let dest = std::env::temp_dir().join(format!("zdbview_cli_bk_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dest);
        run_backup(&cli(&[db_arg, "--backup", dest.to_str().unwrap()]), &dest).unwrap();
        assert!(dest.exists(), "the copy is written");
        let err = run_backup(&cli(&[db_arg, "--backup", dest.to_str().unwrap()]), &dest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");

        // --import: needs a file, a table, and a real database.
        let csv = std::env::temp_dir().join(format!("zdbview_cli_in_{}.csv", std::process::id()));
        std::fs::write(&csv, "a\nfrom the file\n").unwrap();
        let csv_arg = csv.to_str().unwrap();
        let err = run_import(&cli(&[db_arg, "--import", csv_arg]), &csv)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--table"), "{err}");
        let err = run_import(
            &cli(&[text_arg, "--import", csv_arg, "--table", "t", "--sqlite"]),
            &csv,
        )
        .unwrap_err()
        .to_string();
        assert!(!err.is_empty(), "a text file is not importable into: {err}");
        run_import(&cli(&[db_arg, "--import", csv_arg, "--table", "t"]), &csv).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM t WHERE a = 'from the file'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "the row landed");
        drop(conn);

        // --import-sql: needs a file, refuses what is not a database, and runs
        // the script into a real one.
        let script =
            std::env::temp_dir().join(format!("zdbview_cli_script_{}.sql", std::process::id()));
        std::fs::write(&script, "INSERT INTO t (a) VALUES ('from the script');\n").unwrap();
        let script_arg = script.to_str().unwrap();
        let err = run_import_sql(&cli(&["--import-sql", script_arg]), &script)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires a file"), "{err}");
        let err = run_import_sql(
            &cli(&[text_arg, "--import-sql", script_arg, "--sqlite"]),
            &script,
        )
        .unwrap_err()
        .to_string();
        assert!(!err.is_empty(), "a text file is not a database: {err}");
        run_import_sql(&cli(&[db_arg, "--import-sql", script_arg]), &script).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM t WHERE a = 'from the script'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "the script's row landed and was written");
        drop(conn);

        // --recover: needs a file, and refuses what is not a database.
        let err = run_recover(&cli(&["--recover"])).unwrap_err().to_string();
        assert!(err.contains("requires a file"), "{err}");
        let err = run_recover(&cli(&[text_arg, "--recover", "--sqlite"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a SQLite database"), "{err}");
        // And on a real one it succeeds.
        run_recover(&cli(&[db_arg, "--recover"])).unwrap();

        for p in [db, text, dest, csv, script] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// An explicit FILE opens first and quitting there ends the session without
    /// ever showing the picker.
    #[test]
    fn an_explicit_file_that_quits_never_reaches_the_picker() {
        let mut f = Fake {
            opens: vec![(Outcome::Quit, theme(ThemeName::NeonSprawl), None)],
            ..Default::default()
        };
        drive(
            &mut f,
            Some(PathBuf::from("/tmp/a.db")),
            theme(ThemeName::NeonSprawl),
        )
        .unwrap();
        assert_eq!(f.opened.len(), 1);
        assert!(f.picked_with.is_empty(), "the picker must not open");
    }

    /// `o` inside a file (Reopen) goes to the picker, and the scheme the file
    /// ended on is what the picker and the next file are given — prefs are not
    /// re-read, so a concurrent instance cannot change this one's colors.
    #[test]
    fn reopen_carries_the_scheme_into_the_picker_and_the_next_file() {
        let mut f = Fake {
            opens: vec![
                // The first file ends on a different scheme than it started with.
                (Outcome::Reopen, theme(ThemeName::AcidRain), None),
                (Outcome::Quit, theme(ThemeName::AcidRain), None),
            ],
            picks: vec![Some(Picked {
                path: PathBuf::from("/tmp/b.rkyv"),
                theme: theme(ThemeName::SynthWave),
            })],
            ..Default::default()
        };
        drive(
            &mut f,
            Some(PathBuf::from("/tmp/a.db")),
            theme(ThemeName::NeonSprawl),
        )
        .unwrap();

        assert_eq!(
            f.picked_with,
            vec![ThemeName::AcidRain],
            "the file's scheme"
        );
        assert_eq!(
            f.opened,
            vec![
                (PathBuf::from("/tmp/a.db"), ThemeName::NeonSprawl),
                // The picker's own scheme change wins for the next file.
                (PathBuf::from("/tmp/b.rkyv"), ThemeName::SynthWave),
            ]
        );
    }

    /// The write monitor hands a file over directly, which skips the picker for
    /// that round only.
    #[test]
    fn a_file_from_the_monitor_skips_the_picker_once() {
        let mut f = Fake {
            opens: vec![
                (
                    Outcome::Reopen,
                    theme(ThemeName::NeonSprawl),
                    Some(PathBuf::from("/tmp/watched.db")),
                ),
                (Outcome::Reopen, theme(ThemeName::NeonSprawl), None),
                (Outcome::Quit, theme(ThemeName::NeonSprawl), None),
            ],
            picks: vec![Some(Picked {
                path: PathBuf::from("/tmp/from_picker.db"),
                theme: theme(ThemeName::NeonSprawl),
            })],
            ..Default::default()
        };
        drive(
            &mut f,
            Some(PathBuf::from("/tmp/a.db")),
            theme(ThemeName::NeonSprawl),
        )
        .unwrap();

        let files: Vec<&str> = f.opened.iter().map(|(p, _)| p.to_str().unwrap()).collect();
        assert_eq!(
            files,
            ["/tmp/a.db", "/tmp/watched.db", "/tmp/from_picker.db"],
            "the handed-over file comes before the picker is asked"
        );
        assert_eq!(f.picked_with.len(), 1, "asked once, after the handover");
    }

    /// With no FILE the picker leads, and quitting it ends the session.
    #[test]
    fn quitting_the_picker_ends_a_session_with_no_file() {
        let mut f = Fake {
            picks: vec![None],
            ..Default::default()
        };
        drive(&mut f, None, theme(ThemeName::NeonSprawl)).unwrap();
        assert!(f.opened.is_empty(), "nothing was opened");
        assert_eq!(f.picked_with.len(), 1);
    }
}
