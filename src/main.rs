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
mod clipboard;
#[cfg(feature = "disasm")]
mod disasm;
mod export;
mod formats;
mod hexedit;
mod mru;
mod overlay;
mod prefs;
mod rkyv_inspect;
mod scan;
mod sqlite;
mod store;
mod theme;

use anyhow::{anyhow, Result};
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
    "\x1b[32m  //\x1b[0m 0 1 2 3 views   a e r d record CRUD   e hex editor  (rkyv)\n",
    "\x1b[32m  //\x1b[0m v value render   y copy (OSC 52)   x export to file\n",
    "\x1b[32m  //\x1b[0m c scheme   C palette   o file list   h/? help   q quit\n",
    "\n",
    "\x1b[36m  ── EXAMPLES ───────────────────────────────────────────\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview                        \x1b[90mrecent files + saved scan\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview ~/.zshrs/scripts.rkyv  \x1b[90mrecords, or structural\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview data.db --export json  \x1b[90mdump every table\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview --rescan               \x1b[90mwalk again now\x1b[0m\n",
    "\x1b[32m  //\x1b[0m zdbview --list-themes          \x1b[90mpreview the schemes\x1b[0m\n",
    "\n",
    "\x1b[36m  ── FILES ──────────────────────────────────────────────\x1b[0m\n",
    "\x1b[32m  //\x1b[0m $XDG_CACHE_HOME/zdbview/recent  \x1b[90mrecent-files list\x1b[0m\n",
    "\x1b[32m  //\x1b[0m $XDG_CACHE_HOME/zdbview/scan    \x1b[90msaved scan results\x1b[0m\n",
    "\x1b[32m  //\x1b[0m $XDG_CONFIG_HOME/zdbview/prefs  \x1b[90mscheme + palette\x1b[0m\n",
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
        help = "\x1b[32m//\x1b[0m Dump contents to stdout and exit: json | csv"
    )]
    export: Option<String>,
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

    // `--export` is non-interactive: dump to stdout without touching the TUI.
    if let Some(fmt) = &cli.export {
        return run_export(&cli, fmt);
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
    if fmt != "json" && fmt != "csv" {
        return Err(anyhow!("--export expects 'json' or 'csv', got '{}'", fmt));
    }
    let file = cli
        .file
        .clone()
        .ok_or_else(|| anyhow!("--export requires a file argument"))?;
    let kind = store::detect(&file, cli.sqlite, cli.rkyv)?;
    let (store, _) = store::Store::open(&file, kind)?;
    print!("{}", export_store(&store, &fmt)?);
    Ok(())
}

fn export_store(store: &Store, fmt: &str) -> Result<String> {
    match store {
        Store::Sqlite(s) => {
            if fmt == "csv" {
                let table = s
                    .tables
                    .first()
                    .cloned()
                    .ok_or_else(|| anyhow!("no tables to export"))?;
                let total = s.count(&table)?;
                let v = s.rows(&table, total.max(1), 0, None, "")?;
                Ok(export::rows_to_csv(&v.columns, &v.rows))
            } else {
                // JSON: object of { table_name: [rows...] } for every table.
                let mut out = String::from("{");
                for (i, t) in s.tables.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let total = s.count(t)?;
                    let v = s.rows(t, total.max(1), 0, None, "")?;
                    out.push_str(&export::json_escape(t));
                    out.push(':');
                    out.push_str(&export::rows_to_json(&v.columns, &v.rows));
                }
                out.push('}');
                out.push('\n');
                Ok(out)
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

fn run(cli: &Cli, terminal: &mut DefaultTerminal, theme: Option<theme::ThemeName>) -> Result<()> {
    // The scheme is carried from screen to screen instead of being re-read from
    // prefs at every hop: another instance writing prefs must not change what
    // this one is showing.
    let mut scheme = app::resolve_theme(theme);

    // An explicit FILE opens straight away; `o` or Esc there still lead to the
    // picker, so fall through to the loop instead of exiting.
    if let Some(file) = &cli.file {
        let (outcome, used) = open_and_run(cli, terminal, scheme, file.clone())?;
        if outcome == app::Outcome::Quit {
            return Ok(());
        }
        scheme = used;
    }
    loop {
        let picked = match pick(cli, terminal, scheme)? {
            Some(p) => p,
            None => return Ok(()), // user quit the picker
        };
        scheme = picked.theme;
        let (outcome, used) = open_and_run(cli, terminal, scheme, picked.path)?;
        if outcome == app::Outcome::Quit {
            return Ok(());
        }
        scheme = used;
    }
}

/// Show the picker: recent files, plus scan rows taken from appdata while they
/// are still fresh, else from a walk started here.
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
        scan::load_cache().filter(|c| c.fresh())
    };
    // A fresh saved scan is used as-is, so no walk runs on this start.
    let (cached, cache_age, walk) = match saved {
        Some(c) => {
            let age = c.age();
            (c.hits, Some(age), false)
        }
        None => (Vec::new(), None, !cli.no_scan),
    };

    app::pick_mru(
        terminal,
        app::Picker {
            recent: &recent,
            theme: scheme,
            cached,
            cache_age,
            scan: walk.then(|| scan::spawn(roots.clone())),
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
) -> Result<(app::Outcome, theme::Theme)> {
    let kind = store::detect(&file, cli.sqlite, cli.rkyv)?;
    let (store, actual) = store::Store::open(&file, kind)?;
    mru::record(&file, actual);
    let mut app = app::App::with_theme(store, scheme);
    let outcome = app.run(terminal)?;
    Ok((outcome, app.theme()))
}
