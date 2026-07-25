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

#[derive(Parser)]
#[command(
    name = "zdbview",
    version,
    about = "Terminal inspector and CRUD editor for rkyv archives and SQLite databases"
)]
struct Cli {
    /// File to open. With no file, shows a picker of recent files plus the
    /// results of a scan for databases and rkyv shards.
    file: Option<PathBuf>,
    /// Force treating the file as a SQLite database
    #[arg(long, conflicts_with = "rkyv")]
    sqlite: bool,
    /// Force treating the file as a rkyv archive
    #[arg(long)]
    rkyv: bool,
    /// Non-interactively dump the file's contents and exit (json | csv).
    #[arg(long, value_name = "FORMAT")]
    export: Option<String>,
    /// Color scheme for this run, by token (see --list-themes). Overrides the
    /// saved preference without replacing it.
    #[arg(long, value_name = "NAME")]
    theme: Option<String>,
    /// Preview every built-in color scheme with its palette and exit.
    #[arg(long)]
    list_themes: bool,
    /// Extra directory to scan for databases and rkyv shards when no file is
    /// given (repeatable). Replaces the default roots.
    #[arg(long, value_name = "DIR")]
    scan: Vec<PathBuf>,
    /// Skip the startup scan and list only recently opened files.
    #[arg(long, conflicts_with = "scan")]
    no_scan: bool,
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
                let v = s.rows(&table, total.max(1), 0, None)?;
                Ok(export::rows_to_csv(&v.columns, &v.rows))
            } else {
                // JSON: object of { table_name: [rows...] } for every table.
                let mut out = String::from("{");
                for (i, t) in s.tables.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let total = s.count(t)?;
                    let v = s.rows(t, total.max(1), 0, None)?;
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
    // Resolve the file to open: explicit argument, or a pick from the recent
    // files plus whatever the background scan finds.
    let file = match &cli.file {
        Some(f) => f.clone(),
        None => {
            let recent: Vec<mru::Entry> = mru::load()
                .into_iter()
                .filter(|e| e.path.exists())
                .collect();
            let scan = if cli.no_scan {
                None
            } else {
                let roots = if cli.scan.is_empty() {
                    scan::default_roots()
                } else {
                    // Explicit roots are taken at face value, walked as deeply
                    // as a producer's own directory.
                    cli.scan
                        .iter()
                        .map(|p| scan::Root::new(p.clone(), scan::DEEP))
                        .collect()
                };
                Some(scan::spawn(roots))
            };
            match app::pick_mru(terminal, &recent, theme, scan)? {
                Some(p) => p,
                None => return Ok(()), // user quit the picker
            }
        }
    };

    let kind = store::detect(&file, cli.sqlite, cli.rkyv)?;
    let (store, actual) = store::Store::open(&file, kind)?;
    mru::record(&file, actual);
    app::App::new(store, theme).run(terminal)
}
