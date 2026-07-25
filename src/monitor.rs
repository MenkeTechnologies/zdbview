//! The write-monitor screen: a `top` over the stores zdbview knows about.
//!
//! Sampling lives in [`crate::watch`]; this is the view and its keys, kept in one
//! place so the app and the file picker show the same screen instead of each
//! growing its own copy.

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};
use ratatui::Frame;

use crate::app::{human_size, truncate};
use crate::store::Kind;
use crate::theme::Theme;
use crate::watch::{spark, Order, Watcher, ACTIVE_WINDOW};

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
}

/// The monitor screen: the watched set, its ordering, and the cursor.
pub struct Monitor {
    pub watcher: Watcher,
    pub order: Order,
    pub sel: usize,
    /// A one-off message for the host to surface as a toast.
    pub note: Option<String>,
}

impl Monitor {
    pub fn new(targets: impl IntoIterator<Item = (PathBuf, Kind)>) -> Self {
        Monitor {
            watcher: Watcher::new(targets),
            order: Order::Recent,
            sel: 0,
            note: None,
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
        let w = &mut self.watcher;
        let last = w.targets.len().saturating_sub(1);
        match code {
            KeyCode::Esc | KeyCode::Char('w') => return Action::Back,
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::PageDown => self.sel = (self.sel + page).min(last),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(page),
            KeyCode::Char('g') => self.sel = 0,
            KeyCode::Char('G') => self.sel = last,
            // `s` cycles the ordering, like a `top` does.
            KeyCode::Char('s') => {
                self.order = self.order.next();
                self.note = Some(format!("sorted by {}", self.order.label()));
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
                if let Some(path) = w
                    .sorted(self.order)
                    .get(self.sel)
                    .map(|&i| w.targets[i].path.clone())
                {
                    return Action::Open(path);
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
        let order = self.order;
        let rows = w.sorted(order);
        // Scale every sparkline against the busiest sample on screen, so the
        // bars are comparable between rows.
        let peak = w
            .targets
            .iter()
            .flat_map(|t| t.history.iter().copied())
            .max()
            .unwrap_or(0);

        let header = Row::new(vec![
            Cell::from("kind"),
            Cell::from("file"),
            Cell::from("size"),
            Cell::from("written"),
            Cell::from("rate"),
            Cell::from("last"),
            Cell::from("activity"),
        ])
        .style(Style::default().fg(t.primary).add_modifier(Modifier::BOLD));

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

        let widths = [
            Constraint::Length(6),
            Constraint::Length(30),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Min(10),
        ];
        let mut st = TableState::default();
        st.select(Some(self.sel.min(rows.len().saturating_sub(1))));
        let title = format!(
            " writes — {} files, {} active, {} in {}s at {}/s · sort {}{} ",
            w.targets.len(),
            w.active_count(ACTIVE_WINDOW),
            human_size(w.total_written()),
            w.elapsed().as_secs(),
            human_size(w.total_rate() as u64),
            order.label(),
            if w.paused { " · PAUSED" } else { "" },
        );
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
            area,
            &mut st,
        );
    }
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

    fn rows(m: &Monitor, w: u16, h: u16) -> Vec<String> {
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
        let order = m.order;
        m.on_key(KeyCode::Char('s'), 5);
        assert_ne!(m.order, order);
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
        m.order = Order::Name;
        m.sel = 0;
        let first = m.watcher.sorted(Order::Name)[0];
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
        let r = rows(&m, 110, 8);
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
        let r = rows(&m, 110, 8);
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
        assert!(rows(&m, 110, 8)[0].contains("PAUSED"));
        let _ = std::fs::remove_file(&f);
    }

    /// A narrow or short terminal must not panic the table.
    #[test]
    fn render_survives_a_tiny_terminal() {
        let f = scratch("rkyv");
        std::fs::write(&f, b"x").unwrap();
        let m = Monitor::new([(f.clone(), Kind::Rkyv)]);
        for (w, h) in [(20u16, 4u16), (8, 3), (1, 1), (200, 60)] {
            rows(&m, w, h);
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
        let r = rows(&m, 80, 6);
        assert!(r[0].contains("0 files"));
    }
}
