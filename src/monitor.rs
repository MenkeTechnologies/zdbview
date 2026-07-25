//! The write-monitor screen: a `top` over the stores zdbview knows about.
//!
//! Sampling lives in [`crate::watch`]; this is the view and its keys, kept in one
//! place so the app and the file picker show the same screen instead of each
//! growing its own copy.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};
use ratatui::Frame;

use crate::app::{human_size, truncate};
use crate::store::Kind;
use crate::theme::Theme;
use crate::watch::{spark, Column, Sort, Watcher, ACTIVE_WINDOW};

/// What a key press asks the host to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Stay in the monitor.
    None,
    /// Leave it, back to whatever was underneath.
    Back,
    /// Quit the program.
    Quit,
    /// Open this file.
    Open(PathBuf),
    /// Walk this database's log, frame by frame.
    Frames(PathBuf),
}

/// Column widths, in display order. The click-to-sort hit test and the header
/// highlight both read this, so they cannot disagree with the table.
const WIDTHS: [u16; 7] = [6, 30, 8, 9, 10, 7, 10];

/// The monitor screen: the watched set, its ordering, the filter and the cursor.
pub struct Monitor {
    pub watcher: Watcher,
    pub sort: Sort,
    pub sel: usize,
    /// Active `/` filter over the path; empty lists everything.
    pub filter: String,
    /// True while the `/` prompt is open.
    pub typing: bool,
    /// A one-off message for the host to surface as a toast.
    pub note: Option<String>,
    /// What the bottom frame shows: the log's frames, or the tables those frames
    /// belong to.
    pub pane: Pane,
}

/// The bottom frame's two views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    /// The log's frames, newest first.
    #[default]
    Frames,
    /// Bytes written per table, attributed through the frames' page numbers.
    Tables,
}

impl Monitor {
    pub fn new(targets: impl IntoIterator<Item = (PathBuf, Kind)>) -> Self {
        Monitor {
            watcher: Watcher::new(targets),
            sort: Sort::default(),
            sel: 0,
            filter: String::new(),
            typing: false,
            note: None,
            pane: Pane::default(),
        }
    }

    /// Sample if the interval has elapsed; `true` when it did.
    pub fn tick(&mut self) -> bool {
        self.watcher.tick()
    }

    /// How many files are being watched.
    pub fn len(&self) -> usize {
        self.watcher.targets.len()
    }

    /// The rows on screen: filtered, then ordered by the active column.
    pub fn rows(&self) -> Vec<usize> {
        self.watcher.sorted_filtered(self.sort, &self.filter)
    }

    /// Sort by `column`, or invert when it is already the sorted one — the
    /// behaviour of clicking a column header.
    pub fn sort_by(&mut self, column: Column) {
        self.sort = if self.sort.column == column {
            self.sort.inverted()
        } else {
            self.sort.to(column)
        };
        self.sel = 0;
        self.note = Some(format!(
            "sorted by {} {}",
            column.label(),
            if self.sort.desc {
                "descending"
            } else {
                "ascending"
            }
        ));
    }

    /// Column under a click, from the same widths the table is built with.
    pub fn column_at(&self, x: u16, area: Rect) -> Option<Column> {
        // One border column, then each field in order.
        let mut edge = area.x + 1;
        for (i, w) in WIDTHS.iter().enumerate() {
            if x >= edge && x < edge + w {
                return Column::ALL.get(i).copied();
            }
            // ratatui puts one space between columns.
            edge += w + 1;
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.watcher.targets.is_empty()
    }

    /// A screenful for paging, from the rect the host draws into.
    pub fn page_rows(area: Rect) -> usize {
        // Two borders and the header row.
        (area.height as usize).saturating_sub(3).max(1)
    }

    /// Handle one key. `page` is a screenful, from whatever the host rendered.
    pub fn on_key(&mut self, code: KeyCode, page: usize) -> Action {
        let last = self.rows().len().saturating_sub(1);
        // While `/` is open the keys belong to the filter, and the list narrows
        // as it is typed.
        if self.typing {
            match crate::app::filter_prompt_key(code, &mut self.filter, &mut self.sel, last, page) {
                crate::app::Prompt::Open => {}
                crate::app::Prompt::Accept => self.typing = false,
                crate::app::Prompt::Cancel => {
                    self.typing = false;
                    self.filter.clear();
                    self.sel = 0;
                }
            }
            return Action::None;
        }
        let w = &mut self.watcher;
        match code {
            // Esc clears an applied filter first, then leaves.
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.sel = 0;
            }
            KeyCode::Esc | KeyCode::Char('w') => return Action::Back,
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char('/') => {
                self.typing = true;
                self.filter.clear();
                self.sel = 0;
            }
            // `F` opens the log itself: every frame, and the rows each one wrote.
            KeyCode::Char('F') => {
                let picked = self.rows().get(self.sel).map(|&i| {
                    (
                        self.watcher.targets[i].path.clone(),
                        self.watcher.targets[i].kind,
                    )
                });
                if let Some((path, kind)) = picked {
                    if kind == Kind::Sqlite {
                        return Action::Frames(path);
                    }
                    self.note = Some(format!(
                        "{} is an rkyv archive — no log to walk",
                        crate::app::truncate(name_of(&path), 24)
                    ));
                }
            }
            // `t` swaps the bottom frame between the raw log and what the log says
            // about tables.
            KeyCode::Char('t') => {
                self.pane = match self.pane {
                    Pane::Frames => Pane::Tables,
                    Pane::Tables => Pane::Frames,
                }
            }
            // Sorting by column, as htop does it: `<` / `>` (and F6) pick the
            // column, `I` inverts, and picking the sorted column inverts too.
            KeyCode::Char('>') | KeyCode::F(6) => {
                let next = self.sort.column.next();
                self.sort_by(next);
            }
            KeyCode::Char('<') => {
                let prev = self.sort.column.prev();
                self.sort_by(prev);
            }
            KeyCode::Char('I') => {
                self.sort = self.sort.inverted();
                self.note = Some(format!(
                    "sorted by {} {}",
                    self.sort.column.label(),
                    if self.sort.desc {
                        "descending"
                    } else {
                        "ascending"
                    }
                ));
            }
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::PageDown => self.sel = (self.sel + page).min(last),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(page),
            KeyCode::Char('g') => self.sel = 0,
            KeyCode::Char('G') => self.sel = last,
            // `s` is the same as `>`, for the muscle memory of the other screens.
            KeyCode::Char('s') => {
                let next = self.sort.column.next();
                self.sort_by(next);
            }
            KeyCode::Char('p') => {
                w.paused = !w.paused;
                self.note = Some(if w.paused { "paused" } else { "resumed" }.into());
            }
            // `+` / `-` change how often it samples.
            KeyCode::Char('+') | KeyCode::Char('=') => {
                w.interval = (w.interval / 2).max(Duration::from_millis(100));
                self.note = Some(format!("sampling every {}ms", w.interval.as_millis()));
            }
            KeyCode::Char('-') => {
                w.interval = (w.interval * 2).min(Duration::from_secs(5));
                self.note = Some(format!("sampling every {}ms", w.interval.as_millis()));
            }
            // Enter opens the selected file, which is the point of watching it.
            KeyCode::Enter => {
                if let Some(path) = self
                    .rows()
                    .get(self.sel)
                    .map(|&i| self.watcher.targets[i].path.clone())
                {
                    return Action::Open(path);
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Mouse handling: the header row sorts by the column under the pointer (as
    /// clicking a header does in htop), a row selects, and the wheel scrolls.
    pub fn on_mouse(&mut self, m: MouseEvent, area: Rect) -> Action {
        let rows = self.rows();
        let last = rows.len().saturating_sub(1);
        match m.kind {
            MouseEventKind::ScrollDown => self.sel = (self.sel + 1).min(last),
            MouseEventKind::ScrollUp => self.sel = self.sel.saturating_sub(1),
            MouseEventKind::Down(_) => {
                // Border, then the header row, then the body.
                let header_row = area.y + 1;
                if m.row == header_row {
                    if let Some(c) = self.column_at(m.column, area) {
                        self.sort_by(c);
                    }
                } else if m.row > header_row {
                    let idx = (m.row - header_row - 1) as usize;
                    if idx <= last {
                        self.sel = idx;
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    /// One row per watched store, ordered by activity, with a sparkline of the
    /// last samples.
    pub fn render(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let w = &self.watcher;
        let rows = self.rows();
        // Scale every sparkline against the busiest sample on screen, so the
        // bars are comparable between rows.
        let peak = w
            .targets
            .iter()
            .flat_map(|t| t.history.iter().copied())
            .max()
            .unwrap_or(0);

        // The sorted column carries the arrow and the accent, so which column is
        // in charge is visible without reading the title.
        let head = |c: Column| -> Cell {
            if self.sort.column == c {
                Cell::from(format!("{}{}", c.label(), self.sort.arrow()))
                    .style(Style::default().fg(t.accent).add_modifier(Modifier::BOLD))
            } else {
                Cell::from(c.label()).style(Style::default().fg(t.primary))
            }
        };
        let header = Row::new(vec![
            head(Column::Kind),
            head(Column::Name),
            head(Column::Size),
            head(Column::Written),
            head(Column::Rate),
            head(Column::Last),
            Cell::from("activity").style(Style::default().fg(t.primary)),
        ])
        .style(Style::default().add_modifier(Modifier::BOLD));

        let body = rows.iter().map(|&i| {
            let tg = &w.targets[i];
            let hot = tg.active(ACTIVE_WINDOW);
            // A file being written stands out; the rest stay quiet.
            let name_style = if hot {
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.dim)
            };
            let rate = tg.rate(w.interval);
            let bars: String = tg
                .history
                .iter()
                .rev()
                .take(24)
                .rev()
                .map(|&s| spark(s, peak))
                .collect();
            Row::new(vec![
                Cell::from(match tg.kind {
                    Kind::Sqlite => "sqlite",
                    Kind::Rkyv => "rkyv",
                })
                .style(Style::default().fg(if tg.kind == Kind::Sqlite {
                    t.primary
                } else {
                    t.alt
                })),
                Cell::from(truncate(tg.name(), 30)).style(name_style),
                Cell::from(human_size(tg.size)).style(Style::default().fg(t.dim)),
                Cell::from(if tg.written > 0 {
                    human_size(tg.written)
                } else {
                    "—".into()
                })
                .style(Style::default().fg(if tg.written > 0 {
                    t.label
                } else {
                    t.dim
                })),
                Cell::from(if rate >= 1.0 {
                    format!("{}/s", human_size(rate as u64))
                } else {
                    "—".into()
                })
                .style(Style::default().fg(if hot { t.accent } else { t.dim })),
                Cell::from(match tg.last_write {
                    Some(at) => format!("{:.1}s", at.elapsed().as_secs_f64()),
                    None => "—".into(),
                })
                .style(Style::default().fg(t.dim)),
                Cell::from(bars).style(Style::default().fg(if hot { t.accent } else { t.label })),
            ])
        });

        // Same widths the click hit test uses.
        let widths: Vec<Constraint> = WIDTHS
            .iter()
            .enumerate()
            .map(|(i, &w)| {
                if i + 1 == WIDTHS.len() {
                    Constraint::Min(w)
                } else {
                    Constraint::Length(w)
                }
            })
            .collect();
        let mut st = TableState::default();
        st.select(Some(self.sel.min(rows.len().saturating_sub(1))));
        // While the `/` prompt is open the title carries it, since the table has
        // no other line to put it on.
        let title = if self.typing {
            format!(" filter /{}_ · {} match(es) ", self.filter, rows.len())
        } else {
            let counted = if self.filter.is_empty() {
                format!("{} files", w.targets.len())
            } else {
                format!("{}/{} files  /{}", rows.len(), w.targets.len(), self.filter)
            };
            format!(
                " writes — {}, {} active, {} in {}s at {}/s · {}{}{} ",
                counted,
                w.active_count(ACTIVE_WINDOW),
                human_size(w.total_written()),
                w.elapsed().as_secs(),
                human_size(w.total_rate() as u64),
                self.sort.column.label(),
                self.sort.arrow(),
                if w.paused { " · PAUSED" } else { "" },
            )
        };
        // The screen is two frames: the watched set above, and the selected
        // file's write-ahead log below — what it is about to become, before a
        // checkpoint folds those pages into the database itself.
        let (top, bottom) = split_for_wal(area);
        f.render_stateful_widget(
            Table::new(body, widths)
                .header(header)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(t.accent))
                        .title(title),
                )
                .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            top,
            &mut st,
        );
        if let Some(bottom) = bottom {
            let selected = rows.get(self.sel.min(rows.len().saturating_sub(1)));
            let picked = selected.map(|&i| &w.targets[i]);
            match self.pane {
                Pane::Frames => {
                    self.render_wal(f, bottom, t, picked.map(|tg| (tg.path.as_path(), tg.kind)))
                }
                Pane::Tables => self.render_tables(f, bottom, t, picked),
            }
        }
    }

    /// Bytes written per table, which no other SQLite tool shows live: a WAL frame
    /// carries the page it rewrote, and the page map read from the database file
    /// says which table's b-tree that page belongs to. Totals are since the monitor
    /// opened, so they answer "what is being written right now", not "what is big".
    fn render_tables(
        &self,
        f: &mut Frame,
        area: Rect,
        t: &Theme,
        sel: Option<&crate::watch::Target>,
    ) {
        let block = |title: String| {
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.alt))
                .title(title)
        };
        let Some(target) = sel else {
            f.render_widget(block(" tables — nothing selected ".into()), area);
            return;
        };
        if target.kind != Kind::Sqlite {
            f.render_widget(
                block(format!(
                    " tables — {} is an rkyv archive, no page map ",
                    truncate(target.name(), 28)
                )),
                area,
            );
            return;
        }
        if target.by_table.is_empty() {
            f.render_widget(
                block(format!(
                    " tables — no writes attributed yet for {} (needs journal_mode=WAL) ",
                    truncate(target.name(), 28)
                )),
                area,
            );
            return;
        }

        let total: u64 = target.by_table.iter().map(|(_, b)| *b).sum();
        let header = Row::new(vec![
            Cell::from("table"),
            Cell::from("written"),
            Cell::from("share"),
            Cell::from(""),
        ])
        .style(Style::default().fg(t.label).add_modifier(Modifier::BOLD));
        let width = (area.width as usize).saturating_sub(40).clamp(4, 40);
        let body = target.by_table.iter().map(|(name, bytes)| {
            let share = if total == 0 {
                0.0
            } else {
                *bytes as f64 / total as f64
            };
            let bar = "#".repeat((share * width as f64).round() as usize);
            Row::new(vec![
                Cell::from(truncate(name, 28)),
                Cell::from(human_size(*bytes)),
                Cell::from(format!("{:>4.0}%", share * 100.0)),
                Cell::from(bar),
            ])
            .style(Style::default().fg(t.primary))
        });
        f.render_widget(
            Table::new(
                body,
                [
                    Constraint::Length(28),
                    Constraint::Length(10),
                    Constraint::Length(6),
                    Constraint::Min(4),
                ],
            )
            .header(header)
            .block(block(format!(
                " tables — {} across {} object{} of {} · t for frames ",
                human_size(total),
                target.by_table.len(),
                if target.by_table.len() == 1 { "" } else { "s" },
                truncate(target.name(), 24),
            ))),
            area,
        );
    }

    /// The WAL frame: the tail of the selected database's `-wal`, newest last.
    ///
    /// Only frame headers are read, so this costs the same on a 190 MB log as on
    /// an empty one. A frame whose salts no longer match the header is drawn dim
    /// — it belongs to a log a checkpoint already folded away, and everything
    /// below that point is a ghost rather than pending work.
    fn render_wal(&self, f: &mut Frame, area: Rect, t: &Theme, sel: Option<(&Path, Kind)>) {
        let block = |title: String| {
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.alt))
                .title(title)
        };

        let Some((path, kind)) = sel else {
            f.render_widget(block(" wal — nothing selected ".into()), area);
            return;
        };
        if kind != Kind::Sqlite {
            f.render_widget(
                block(format!(
                    " wal — {} is an rkyv archive, no log ",
                    truncate(name_of(path), 28)
                )),
                area,
            );
            return;
        }
        let Some(tail) = crate::wal::read_tail(path, Self::wal_rows(area)) else {
            f.render_widget(
                block(format!(
                    " wal — none for {} (journal_mode is not WAL, or it is checkpointed away) ",
                    truncate(name_of(path), 28)
                )),
                area,
            );
            return;
        };

        let header = Row::new(vec![
            Cell::from("frame"),
            Cell::from("page"),
            Cell::from("table"),
            Cell::from("commit"),
            Cell::from("db pages"),
        ])
        .style(Style::default().fg(t.label).add_modifier(Modifier::BOLD));
        // Which table each rewritten page belongs to, from the map the watcher
        // already built for this target.
        let owners = self
            .watcher
            .targets
            .iter()
            .find(|tg| tg.path == path)
            .map(|tg| tg.owners_snapshot())
            .unwrap_or_default();

        let body = tail.frames.iter().rev().map(|fr| {
            let style = if !fr.live {
                Style::default().fg(t.dim)
            } else if fr.commit {
                Style::default().fg(t.accent)
            } else {
                Style::default().fg(t.label)
            };
            Row::new(vec![
                Cell::from(fr.index.to_string()),
                Cell::from(fr.page.to_string()),
                Cell::from(truncate(
                    owners.get(&fr.page).map(String::as_str).unwrap_or("—"),
                    20,
                )),
                Cell::from(if !fr.live {
                    "stale".to_string()
                } else if fr.commit {
                    "commit".to_string()
                } else {
                    "—".to_string()
                }),
                Cell::from(if fr.db_size > 0 {
                    fr.db_size.to_string()
                } else {
                    "—".into()
                }),
            ])
            .style(style)
        });

        let pending = tail.uncommitted();
        let title = format!(
            " wal — {} live of {} frames · {} commits · {} pages pending · {}{} · seq {} · {} pages · salt {:08x}/{:08x} · {} ",
            tail.live_frames,
            tail.total_frames,
            tail.commits,
            tail.distinct_pages(),
            human_size(tail.size),
            if pending > 0 {
                format!(" · {pending} uncommitted")
            } else {
                String::new()
            },
            tail.checkpoint_seq,
            human_size(tail.page_size as u64),
            // The salts are what make older frames stale, so they belong beside
            // the checkpoint sequence they move with.
            tail.salt1,
            tail.salt2,
            truncate(name_of(&tail.path), 24),
        );
        f.render_widget(
            Table::new(
                body,
                [
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Length(20),
                    Constraint::Length(8),
                    Constraint::Min(8),
                ],
            )
            .header(header)
            .block(block(title)),
            area,
        );
    }

    /// How many WAL frames fit in the bottom frame (borders + header row).
    fn wal_rows(area: Rect) -> usize {
        (area.height as usize).saturating_sub(3).max(1)
    }
}

/// The file's own name, for a frame title.
fn name_of(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
}

/// Give the WAL frame the bottom third, floor 6 rows, and drop it entirely when
/// the terminal is too short for both to be readable.
pub fn split_for_wal(area: Rect) -> (Rect, Option<Rect>) {
    const MIN_WAL: u16 = 6;
    const MIN_TOP: u16 = 6;
    if area.height < MIN_TOP + MIN_WAL {
        return (area, None);
    }
    let wal_h = (area.height / 3).max(MIN_WAL);
    let top_h = area.height - wal_h;
    (
        Rect {
            height: top_h,
            ..area
        },
        Some(Rect {
            y: area.y + top_h,
            height: wal_h,
            ..area
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeName;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::io::Write;

    fn scratch(ext: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "zdbview_mon_{}_{}.{}",
            std::process::id(),
            seq,
            ext
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn rows_at(m: &Monitor, w: u16, h: u16) -> Vec<String> {
        let theme = Theme::from_name(ThemeName::NeonSprawl);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| m.render(f, f.area(), &theme)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn sample(m: &mut Monitor) {
        m.watcher.interval = Duration::ZERO;
        assert!(m.tick());
        m.watcher.interval = crate::watch::DEFAULT_INTERVAL;
    }

    /// The screen the app and the picker share: keys in, actions out, so both
    /// hosts behave the same without either owning a copy.
    #[test]
    fn keys_map_to_actions_the_host_can_act_on() {
        let a = scratch("rkyv");
        let b = scratch("db");
        std::fs::write(&a, b"aaaa").unwrap();
        std::fs::write(&b, b"SQLite format 3\0").unwrap();
        let mut m = Monitor::new([(a.clone(), Kind::Rkyv), (b.clone(), Kind::Sqlite)]);
        assert_eq!(m.len(), 2);
        assert!(!m.is_empty());

        // Motions stay inside the list.
        assert_eq!(m.on_key(KeyCode::Down, 5), Action::None);
        assert_eq!(m.sel, 1);
        m.on_key(KeyCode::Down, 5);
        assert_eq!(m.sel, 1, "clamped at the last row");
        m.on_key(KeyCode::Up, 5);
        assert_eq!(m.sel, 0);
        m.on_key(KeyCode::PageDown, 5);
        assert_eq!(m.sel, 1);
        m.on_key(KeyCode::Char('G'), 5);
        assert_eq!(m.sel, 1);
        m.on_key(KeyCode::Char('g'), 5);
        assert_eq!(m.sel, 0);

        // Sort, pause and interval each leave a note for the host to surface.
        let before = m.sort;
        m.on_key(KeyCode::Char('s'), 5);
        assert_ne!(m.sort, before);
        assert!(m.note.take().unwrap().starts_with("sorted by"));
        m.on_key(KeyCode::Char('p'), 5);
        assert!(m.watcher.paused);
        assert_eq!(m.note.take().as_deref(), Some("paused"));
        m.on_key(KeyCode::Char('p'), 5);
        assert!(!m.watcher.paused);
        let iv = m.watcher.interval;
        m.on_key(KeyCode::Char('+'), 5);
        assert!(m.watcher.interval < iv);
        assert!(m.note.take().unwrap().contains("ms"));
        m.on_key(KeyCode::Char('-'), 5);
        assert_eq!(m.watcher.interval, iv);

        // Enter names the file under the cursor; w/Esc leave; q quits.
        m.sort = Sort::by(Column::Name);
        m.sel = 0;
        let first = m.watcher.sorted(Sort::by(Column::Name))[0];
        let expected = m.watcher.targets[first].path.clone();
        assert_eq!(m.on_key(KeyCode::Enter, 5), Action::Open(expected));
        assert_eq!(m.on_key(KeyCode::Char('w'), 5), Action::Back);
        assert_eq!(m.on_key(KeyCode::Esc, 5), Action::Back);
        assert_eq!(m.on_key(KeyCode::Char('q'), 5), Action::Quit);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn render_shows_totals_columns_and_live_bytes() {
        let f = scratch("rkyv");
        std::fs::write(&f, b"x").unwrap();
        let mut m = Monitor::new([(f.clone(), Kind::Rkyv)]);
        let r = rows_at(&m, 110, 8);
        assert!(r[0].contains("writes —"), "header missing: {:?}", r[0]);
        assert!(r[0].contains("1 files"));
        assert!(r[0].contains("0 active"), "nothing written yet");
        assert!(r[1].contains("activity"), "column header missing");
        assert!(
            r.iter().any(|l| l.contains("rkyv")),
            "the watched row is not listed"
        );

        // Write to it, sample, and the row must show bytes, a rate and bars.
        {
            let mut fh = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
            fh.write_all(&[b'z'; 8192]).unwrap();
        }
        sample(&mut m);
        let r = rows_at(&m, 110, 8);
        assert!(r[0].contains("1 active"), "the write was not noticed");
        assert!(
            r.iter().any(|l| l.contains("8.0 K")),
            "bytes missing: {r:#?}"
        );
        assert!(
            r.iter().any(|l| l.contains('█')),
            "no sparkline for a peak sample"
        );
        // Pausing is stated on screen, not just in a toast.
        m.watcher.paused = true;
        assert!(rows_at(&m, 110, 8)[0].contains("PAUSED"));
        let _ = std::fs::remove_file(&f);
    }

    /// `/` filters the monitor's rows, and the arrows still work while typing.
    #[test]
    fn slash_filters_the_watched_list() {
        let a = scratch("rkyv");
        let b = scratch("db");
        std::fs::write(&a, b"aaaa").unwrap();
        std::fs::write(&b, b"bbbb").unwrap();
        let mut m = Monitor::new([(a.clone(), Kind::Rkyv), (b.clone(), Kind::Sqlite)]);
        assert_eq!(m.rows().len(), 2);

        m.on_key(KeyCode::Char('/'), 5);
        assert!(m.typing, "the prompt must open");
        // Every scratch name contains the extension, so filter on that.
        for c in ".db".chars() {
            m.on_key(KeyCode::Char(c), 5);
        }
        assert_eq!(m.filter, ".db");
        let rows = m.rows();
        assert_eq!(rows.len(), 1, "only the database is listed");
        assert_eq!(m.watcher.targets[rows[0]].path, b);

        // The table says what is filtered, and the prompt is visible while typing.
        let r = rows_at(&m, 110, 8);
        assert!(r[0].contains("filter /.db_"), "prompt missing: {:?}", r[0]);
        m.on_key(KeyCode::Enter, 5);
        assert!(!m.typing);
        let r = rows_at(&m, 110, 8);
        assert!(r[0].contains("1/2 files"), "count missing: {:?}", r[0]);
        assert!(r[0].contains("/.db"));

        // Esc clears the filter, a second Esc leaves.
        assert_eq!(m.on_key(KeyCode::Esc, 5), Action::None);
        assert!(m.filter.is_empty());
        assert_eq!(m.rows().len(), 2);
        assert_eq!(m.on_key(KeyCode::Esc, 5), Action::Back);

        // Cancelling the prompt drops what was typed.
        m.on_key(KeyCode::Char('/'), 5);
        m.on_key(KeyCode::Char('z'), 5);
        assert_eq!(m.on_key(KeyCode::Esc, 5), Action::None);
        assert!(!m.typing && m.filter.is_empty());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// Sorting is by column with a direction, as in htop: `<`/`>` pick it, `I`
    /// inverts, and the header marks which one is active.
    #[test]
    fn columns_sort_and_the_header_says_which() {
        let a = scratch("rkyv");
        let b = scratch("db");
        std::fs::write(&a, [b'a'; 10]).unwrap();
        std::fs::write(&b, [b'b'; 5000]).unwrap();
        let mut m = Monitor::new([(a.clone(), Kind::Rkyv), (b.clone(), Kind::Sqlite)]);
        assert_eq!(m.sort.column, Column::Last, "recent writes by default");

        // `>` walks forward, `<` back, both re-selecting the top row.
        m.sel = 1;
        m.on_key(KeyCode::Char('>'), 5);
        assert_eq!(m.sort.column, Column::Kind);
        assert_eq!(m.sel, 0, "a new sort starts at the top");
        assert!(m.note.take().unwrap().contains("kind"));
        m.on_key(KeyCode::Char('<'), 5);
        assert_eq!(m.sort.column, Column::Last);
        // F6 is htop's key for the same thing.
        m.on_key(KeyCode::F(6), 5);
        assert_eq!(m.sort.column, Column::Kind);

        // Sort by size and check the order really changes.
        while m.sort.column != Column::Size {
            m.on_key(KeyCode::Char('>'), 5);
        }
        m.note.take();
        let biggest = m.rows()[0];
        assert_eq!(m.watcher.targets[biggest].path, b, "largest first");
        // `I` inverts it.
        m.on_key(KeyCode::Char('I'), 5);
        assert!(!m.sort.desc);
        assert_eq!(m.watcher.targets[m.rows()[0]].path, a, "smallest first");
        assert!(m.note.take().unwrap().contains("ascending"));

        // The header marks the sorted column with an arrow.
        let r = rows_at(&m, 110, 8);
        assert!(r[1].contains("size▲"), "header not marked: {:?}", r[1]);
        assert!(r[0].contains("size▲"), "title not marked: {:?}", r[0]);
        m.on_key(KeyCode::Char('I'), 5);
        assert!(rows_at(&m, 110, 8)[1].contains("size▼"));
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// Clicking a column header sorts by it, and clicking it again inverts —
    /// which is what "sort on columns" means in htop.
    #[test]
    fn clicking_a_header_sorts_by_that_column() {
        let a = scratch("rkyv");
        std::fs::write(&a, b"x").unwrap();
        let mut m = Monitor::new([(a.clone(), Kind::Rkyv)]);
        let area = Rect::new(0, 0, 110, 10);

        // Column edges follow WIDTHS: kind at x=1, file at 8, size at 39.
        assert_eq!(m.column_at(1, area), Some(Column::Kind));
        assert_eq!(m.column_at(8, area), Some(Column::Name));
        assert_eq!(m.column_at(39, area), Some(Column::Size));
        assert_eq!(m.column_at(0, area), None, "the border is not a column");

        let click = |col: u16, row: u16| MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        // Header row is y=1.
        m.on_mouse(click(39, 1), area);
        assert_eq!(m.sort.column, Column::Size);
        assert!(m.sort.desc);
        m.on_mouse(click(39, 1), area);
        assert!(!m.sort.desc, "clicking the sorted column inverts it");
        assert!(m.note.take().is_some());

        // A click in the body selects instead of sorting.
        let before = m.sort;
        m.on_mouse(click(39, 2), area);
        assert_eq!(m.sort, before);
        assert_eq!(m.sel, 0);
        let _ = std::fs::remove_file(&a);
    }

    /// A narrow or short terminal must not panic the table.
    #[test]
    fn render_survives_a_tiny_terminal() {
        let f = scratch("rkyv");
        std::fs::write(&f, b"x").unwrap();
        let m = Monitor::new([(f.clone(), Kind::Rkyv)]);
        for (w, h) in [(20u16, 4u16), (8, 3), (1, 1), (200, 60)] {
            rows_at(&m, w, h);
        }
        assert_eq!(Monitor::page_rows(Rect::new(0, 0, 80, 24)), 21);
        assert_eq!(Monitor::page_rows(Rect::new(0, 0, 80, 2)), 1, "never zero");
        let _ = std::fs::remove_file(&f);
    }

    /// Watching nothing is not a crash, just an empty table.
    #[test]
    fn an_empty_watch_set_renders() {
        let m = Monitor::new(Vec::new());
        assert!(m.is_empty());
        let r = rows_at(&m, 80, 6);
        assert!(r[0].contains("0 files"));
    }
}

#[cfg(test)]
mod wal_frame_tests {
    use super::*;
    use crate::theme::{Theme, ThemeName};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen(m: &Monitor, w: u16, h: u16) -> Vec<String> {
        let theme = Theme::from_name(ThemeName::NeonSprawl);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| m.render(f, f.area(), &theme)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn wal_db(name: &str, rows: usize) -> (PathBuf, rusqlite::Connection) {
        let p =
            std::env::temp_dir().join(format!("zdbview_monwal_{}_{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(crate::wal::wal_path(&p));
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        for i in 0..rows {
            conn.execute("INSERT INTO t (v) VALUES (?1)", [format!("v{i}")])
                .unwrap();
        }
        (p, conn)
    }

    /// The screen is two frames: the watched set, and the selected database's
    /// log underneath it.
    #[test]
    fn the_wal_frame_lists_frames_for_the_selected_database() {
        let (db, _conn) = wal_db("listed", 4);
        let m = Monitor::new([(db, Kind::Sqlite)]);

        let s = screen(&m, 110, 24);
        let all = s.join("\n");
        assert!(
            all.contains("writes —"),
            "the write table is still the top frame"
        );
        assert!(all.contains("wal —"), "the wal frame is drawn: {all}");
        assert!(
            all.contains("commits"),
            "its title carries the commit count"
        );
        assert!(
            all.contains("frame") && all.contains("page"),
            "the frame table has its columns: {all}"
        );
        assert!(all.contains("commit"), "commit frames are marked");
    }

    /// An rkyv archive has no write-ahead log, and the frame says so rather than
    /// sitting empty.
    #[test]
    fn an_rkyv_target_explains_why_there_is_no_log() {
        let p =
            std::env::temp_dir().join(format!("zdbview_monwal_{}_arch.rkyv", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        let m = Monitor::new([(p, Kind::Rkyv)]);

        let all = screen(&m, 110, 24).join("\n");
        assert!(all.contains("rkyv archive, no log"), "{all}");
    }

    /// A journal_mode=delete database has no `-wal` at all.
    #[test]
    fn a_non_wal_database_says_the_log_is_absent() {
        let p =
            std::env::temp_dir().join(format!("zdbview_monwal_{}_plain.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute("CREATE TABLE t (a)", []).unwrap();
        drop(conn);
        let m = Monitor::new([(p, Kind::Sqlite)]);

        let all = screen(&m, 110, 24).join("\n");
        assert!(all.contains("wal — none"), "{all}");
    }

    /// On a short terminal the log frame is dropped rather than squeezing the
    /// table into unreadability.
    #[test]
    fn a_short_terminal_keeps_only_the_write_table() {
        // Both frames need a floor of 6 rows each, so 12 is the threshold.
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let (top, bottom) = split_for_wal(area);
        let bottom = bottom.expect("24 rows fits both frames");
        assert_eq!(top.height + bottom.height, area.height, "no rows are lost");
        assert_eq!(
            bottom.y,
            top.y + top.height,
            "the log frame sits directly below"
        );
        assert!(bottom.height >= 6, "the log frame keeps its floor");

        let exact = Rect { height: 12, ..area };
        assert!(
            split_for_wal(exact).1.is_some(),
            "12 rows is exactly enough"
        );

        let tiny = Rect { height: 11, ..area };
        assert!(
            split_for_wal(tiny).1.is_none(),
            "below that the log frame is dropped"
        );
        assert_eq!(
            split_for_wal(tiny).0.height,
            11,
            "and the table keeps the whole area"
        );
    }
}
