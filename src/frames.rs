//! The write-ahead log as a history you can step through.
//!
//! A WAL frame carries a whole page image, which means the log holds not just
//! *that* something was written but *what* — the state of that page as of that
//! write. Walking backwards through the frames and decoding each page's cells
//! therefore shows the rows a write contained, and the rows it replaced.
//!
//! Nothing else reads a log this way: `sqlite3` exposes a checkpoint and a frame
//! count, DB Browser and the GUI tools show the database after the fact, and
//! `sqlite3_analyzer` is a static report. This is the only view here that reads
//! frame payloads rather than frame headers, so it is deliberately on a keypress
//! rather than in the sampling loop.

use crate::recover::{decode_page_image, PageKind, Value};
use crate::theme::Theme;
use crate::wal;
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What a keypress asked the host to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// Back to the write monitor.
    Back,
    Quit,
}

/// One frame with what it turned out to hold.
#[derive(Debug, Clone)]
pub struct Decoded {
    pub frame: wal::Frame,
    /// The table or index whose b-tree this page belongs to, when the page map
    /// knows it.
    pub owner: Option<String>,
    pub kind: PageKind,
    pub rows: Vec<(Option<i64>, Vec<Value>)>,
    /// A value continued onto an overflow page, which is a different frame.
    pub overflowed: bool,
}

/// The log of one database, newest frame first.
pub struct FrameView {
    pub db: PathBuf,
    /// Frames newest first, which is the order they are stepped through.
    pub frames: Vec<wal::Frame>,
    pub sel: usize,
    /// The selected frame, decoded. Decoding is per-frame and on demand: a log can
    /// hold tens of thousands of them and each payload is a whole page.
    pub current: Option<Decoded>,
    owners: HashMap<u32, String>,
    /// Column names per table, so a decoded row can be labelled.
    columns: HashMap<String, Vec<String>>,
    /// Active filter over what a frame is: an owner name, or `commit`. A log can
    /// hold tens of thousands of frames, and the question is usually about one
    /// table or one transaction.
    pub filter: String,
    /// True while the `/` prompt is open.
    pub typing: bool,
    pub note: String,
}

impl FrameView {
    /// Read the log's frame headers and the page map. `columns` labels the values
    /// of a decoded row; it comes from the host, which already has the schema.
    pub fn open(db: &Path, columns: HashMap<String, Vec<String>>) -> Option<Self> {
        let tail = wal::read_frames_after(db, 0, None)?;
        if tail.frames.is_empty() {
            return None;
        }
        let owners = crate::recover::page_owners(db).unwrap_or_default();
        let mut frames = tail.frames;
        frames.reverse();
        let mut view = FrameView {
            db: db.to_path_buf(),
            frames,
            sel: 0,
            current: None,
            owners,
            columns,
            filter: String::new(),
            typing: false,
            note: String::new(),
        };
        view.decode();
        Some(view)
    }

    /// Decode the selected frame's page image.
    fn decode(&mut self) {
        let Some(frame) = self.frames.get(self.sel).cloned() else {
            self.current = None;
            return;
        };
        let Some((page, image)) = wal::frame_page(&self.db, frame.index) else {
            self.note = format!("frame {} could not be read", frame.index);
            self.current = None;
            return;
        };
        let decoded = decode_page_image(&image, page);
        self.note = String::new();
        self.current = Some(Decoded {
            owner: self.owners.get(&page).cloned(),
            kind: decoded.kind,
            rows: decoded.rows,
            overflowed: decoded.overflowed,
            frame,
        });
    }

    /// Indices of the frames the filter leaves listed, newest first. A frame matches
    /// on its owner's name, on `commit`, or on its page number.
    pub fn visible(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.frames.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.frames
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                let owner = self
                    .owners
                    .get(&f.page)
                    .cloned()
                    .unwrap_or_default()
                    .to_lowercase();
                owner.contains(&needle)
                    || f.page.to_string().contains(&needle)
                    || (f.commit && "commit".contains(&needle))
                    || (!f.live && "stale".contains(&needle))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Column names for the selected frame's table, when it is known.
    fn current_columns(&self) -> Option<&Vec<String>> {
        let owner = self.current.as_ref()?.owner.as_ref()?;
        self.columns.get(owner)
    }

    pub fn on_key(&mut self, code: KeyCode, page: usize) -> Action {
        // While `/` is open the keys belong to the filter, and the list narrows as
        // it is typed — the same prompt the rest of the app uses.
        if self.typing {
            let mut sel = 0usize;
            let visible = self.visible();
            let last = visible.len().saturating_sub(1);
            match crate::app::filter_prompt_key(code, &mut self.filter, &mut sel, last, page) {
                crate::app::Prompt::Open => {}
                crate::app::Prompt::Accept => self.typing = false,
                crate::app::Prompt::Cancel => {
                    self.typing = false;
                    self.filter.clear();
                }
            }
            // Keep the cursor on something that is still listed.
            let visible = self.visible();
            if !visible.contains(&self.sel) {
                self.sel = visible.first().copied().unwrap_or(0);
            }
            self.decode();
            return Action::None;
        }
        let visible = self.visible();
        let last = self.frames.len().saturating_sub(1);
        // Stepping moves through the listed frames, not through the log's own
        // numbering, so a filter narrows what j/k walk.
        let pos = visible.iter().position(|&i| i == self.sel).unwrap_or(0);
        let at = |p: usize| visible.get(p.min(visible.len().saturating_sub(1))).copied();
        match code {
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                return Action::None;
            }
            KeyCode::Esc | KeyCode::Char('F') => return Action::Back,
            KeyCode::Char('/') => {
                self.typing = true;
                self.filter.clear();
                return Action::None;
            }
            KeyCode::Char('q') => return Action::Quit,
            // Down is back in time: the list is newest first.
            KeyCode::Down | KeyCode::Char('j') => self.sel = at(pos + 1).unwrap_or(self.sel),
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = at(pos.saturating_sub(1)).unwrap_or(self.sel)
            }
            KeyCode::PageDown => self.sel = at(pos + page).unwrap_or(self.sel),
            KeyCode::PageUp => self.sel = at(pos.saturating_sub(page)).unwrap_or(self.sel),
            KeyCode::Char('g') | KeyCode::Home => self.sel = at(0).unwrap_or(0),
            KeyCode::Char('G') | KeyCode::End => self.sel = visible.last().copied().unwrap_or(last),
            // `[` and `]` step to the previous / next commit, which is a whole
            // transaction rather than one page.
            KeyCode::Char(']') => {
                if let Some(i) = self
                    .frames
                    .iter()
                    .enumerate()
                    .skip(self.sel + 1)
                    .find(|(_, f)| f.commit)
                    .map(|(i, _)| i)
                {
                    self.sel = i;
                }
            }
            KeyCode::Char('[') => {
                if let Some(i) = self.frames[..self.sel]
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.commit)
                    .map(|(i, _)| i)
                    .next_back()
                {
                    self.sel = i;
                }
            }
            _ => return Action::None,
        }
        self.decode();
        Action::None
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, t: &Theme) {
        // Frame list on the left, the selected frame's rows on the right.
        let list_w = 34.min(area.width / 2);
        let left = Rect {
            width: list_w,
            ..area
        };
        let right = Rect {
            x: area.x + list_w,
            width: area.width.saturating_sub(list_w),
            ..area
        };

        let visible = self.visible();
        let commits = visible.iter().filter(|&&i| self.frames[i].commit).count();
        let header = Row::new(vec![
            Cell::from("frame"),
            Cell::from("page"),
            Cell::from("what"),
        ])
        .style(Style::default().fg(t.label).add_modifier(Modifier::BOLD));
        let rows = visible.iter().map(|&i| {
            let fr = &self.frames[i];
            let style = if i == self.sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if !fr.live {
                Style::default().fg(t.dim)
            } else if fr.commit {
                Style::default().fg(t.accent)
            } else {
                Style::default().fg(t.primary)
            };
            Row::new(vec![
                Cell::from(fr.index.to_string()),
                Cell::from(fr.page.to_string()),
                Cell::from(match (fr.live, fr.commit) {
                    (false, _) => "stale".to_string(),
                    (true, true) => "commit".to_string(),
                    (true, false) => self
                        .owners
                        .get(&fr.page)
                        .cloned()
                        .unwrap_or_else(|| "—".into()),
                }),
            ])
            .style(style)
        });
        f.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Length(7),
                    Constraint::Length(7),
                    Constraint::Min(6),
                ],
            )
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.accent))
                    .title(if self.typing {
                        format!(" /{}", self.filter)
                    } else if self.filter.is_empty() {
                        format!(" {} frames · {commits} commits ", self.frames.len())
                    } else {
                        format!(
                            " {}/{} frames · {commits} commits · /{} ",
                            visible.len(),
                            self.frames.len(),
                            self.filter
                        )
                    }),
            ),
            left,
        );

        let mut lines: Vec<Line> = Vec::new();
        match &self.current {
            None => lines.push(Line::from(Span::styled(
                if self.note.is_empty() {
                    "nothing decoded".to_string()
                } else {
                    self.note.clone()
                },
                Style::default().fg(t.alt),
            ))),
            Some(d) => {
                let owner = d.owner.clone().unwrap_or_else(|| "unmapped".into());
                lines.push(Line::from(vec![
                    Span::styled("page ", Style::default().fg(t.dim)),
                    Span::styled(d.frame.page.to_string(), Style::default().fg(t.primary)),
                    Span::styled("  ", Style::default()),
                    Span::styled(d.kind.label(), Style::default().fg(t.label)),
                    Span::styled("  ", Style::default()),
                    Span::styled(owner, Style::default().fg(t.accent)),
                ]));
                if d.frame.commit {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "commit — the database was {} pages after this write",
                            d.frame.db_size
                        ),
                        Style::default().fg(t.label),
                    )));
                }
                if d.overflowed {
                    lines.push(Line::from(Span::styled(
                        "a value continues on an overflow page, which is a different frame",
                        Style::default().fg(t.alt),
                    )));
                }
                lines.push(Line::from(""));
                if d.rows.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("no rows on this page ({})", d.kind.label()),
                        Style::default().fg(t.dim),
                    )));
                }
                let columns = self.current_columns().cloned().unwrap_or_default();
                for (rowid, values) in &d.rows {
                    // A row off an index leaf has no rowid: its key is the handle.
                    lines.push(Line::from(Span::styled(
                        match rowid {
                            Some(id) => format!("rowid {id}"),
                            None => "keyed row".to_string(),
                        },
                        Style::default().fg(t.accent),
                    )));
                    for (i, v) in values.iter().enumerate() {
                        let name = columns.get(i).cloned().unwrap_or_else(|| format!("c{i}"));
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("  {:>16}  ", crate::app::truncate(&name, 16)),
                                Style::default().fg(t.dim),
                            ),
                            Span::styled(
                                crate::app::truncate(&show(v), 60),
                                Style::default().fg(t.primary),
                            ),
                        ]));
                    }
                }
            }
        }
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.alt))
                    .title(
                        " what this frame wrote — j/k step · [ ] commits · / filter · Esc back ",
                    ),
            ),
            right,
        );
    }
}

/// A value as the inspector shows it: a blob by size, everything else as itself.
fn show(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Int(i) => i.to_string(),
        Value::Real(f) => format!("{f}"),
        Value::Text(t) => t.clone(),
        Value::Blob(b) => format!("<blob {} bytes>", b.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "zdbview_frames_{}_{seq}_{name}",
            std::process::id()
        ));
        for suffix in ["", "-wal", "-shm"] {
            let mut n = p.as_os_str().to_os_string();
            n.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(n));
        }
        p
    }

    fn clean(p: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut n = p.as_os_str().to_os_string();
            n.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(n));
        }
    }

    /// A log in WAL mode with autocheckpoint off, so its frames survive.
    fn wal_db(name: &str) -> (PathBuf, rusqlite::Connection) {
        let path = scratch(name);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        (path, conn)
    }

    /// The point of the whole view: a frame's payload is a page, and decoding it
    /// shows the rows that write put there.
    #[test]
    fn a_frame_shows_the_rows_the_write_contained() {
        let (path, conn) = wal_db("what.db");
        conn.execute_batch("CREATE TABLE t (a TEXT, b INTEGER)")
            .unwrap();
        conn.execute("INSERT INTO t VALUES ('first', 1)", [])
            .unwrap();
        conn.execute("INSERT INTO t VALUES ('second', 2)", [])
            .unwrap();

        let columns = HashMap::from([("t".to_string(), vec!["a".to_string(), "b".to_string()])]);
        let mut view = FrameView::open(&path, columns).expect("a log with frames");
        assert!(view.frames.len() >= 2, "{:?}", view.frames.len());
        // Newest first, so the top frame is the last write.
        let top = view.current.as_ref().expect("decoded");
        assert!(top.frame.commit, "the newest frame ends a transaction");

        // Somewhere in the log is the page that holds both rows.
        let mut found = None;
        for i in 0..view.frames.len() {
            view.sel = i;
            view.decode();
            if let Some(d) = &view.current {
                if d.rows.len() == 2 {
                    found = Some(d.clone());
                    break;
                }
            }
        }
        let d = found.expect("a frame whose page holds both rows");
        assert_eq!(d.owner.as_deref(), Some("t"), "attributed to the table");
        assert_eq!(d.kind, PageKind::TableLeaf);
        assert_eq!(d.rows[0].1[0], Value::Text("first".into()));
        assert_eq!(d.rows[1].1[1], Value::Int(2));
        drop(conn);
        clean(&path);
    }

    /// Stepping: `j`/`k` move one frame, `[`/`]` move a whole transaction, and the
    /// ends clamp instead of wrapping.
    #[test]
    fn stepping_walks_frames_and_commits() {
        let (path, conn) = wal_db("step.db");
        conn.execute_batch("CREATE TABLE t (v TEXT)").unwrap();
        // Three transactions, each big enough to write several pages.
        for round in 0..3 {
            conn.execute_batch("BEGIN").unwrap();
            for i in 0..80 {
                conn.execute(
                    "INSERT INTO t VALUES (?1)",
                    [format!("round {round} row {i} with padding to fill pages")],
                )
                .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
        }

        let mut view = FrameView::open(&path, HashMap::new()).expect("frames");
        let commits: Vec<usize> = view
            .frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.commit)
            .map(|(i, _)| i)
            .collect();
        assert!(commits.len() >= 3, "one per transaction: {commits:?}");

        assert_eq!(view.sel, 0);
        view.on_key(KeyCode::Char('k'), 10);
        assert_eq!(view.sel, 0, "the newest end clamps");
        view.on_key(KeyCode::Char('j'), 10);
        assert_eq!(view.sel, 1, "j steps back in time");

        // `]` goes to the next commit further back, `[` returns toward the newest.
        view.sel = 0;
        view.on_key(KeyCode::Char(']'), 10);
        assert_eq!(view.sel, commits[1], "next commit older: {commits:?}");
        view.on_key(KeyCode::Char('['), 10);
        assert_eq!(view.sel, commits[0]);

        view.on_key(KeyCode::Char('G'), 10);
        assert_eq!(view.sel, view.frames.len() - 1, "G is the oldest frame");
        view.on_key(KeyCode::Char('j'), 10);
        assert_eq!(view.sel, view.frames.len() - 1, "the old end clamps too");

        assert_eq!(view.on_key(KeyCode::Esc, 10), Action::Back);
        assert_eq!(view.on_key(KeyCode::Char('q'), 10), Action::Quit);
        drop(conn);
        clean(&path);
    }

    /// `/` narrows the log to one table's frames or to commits, and stepping then
    /// walks only what is listed.
    #[test]
    fn the_filter_narrows_the_log() {
        let (path, conn) = wal_db("filter.db");
        conn.execute_batch(
            "CREATE TABLE alpha (v TEXT);
             CREATE TABLE beta (v TEXT);",
        )
        .unwrap();
        for i in 0..40 {
            conn.execute(
                "INSERT INTO alpha VALUES (?1)",
                [format!("a{i} padded out")],
            )
            .unwrap();
        }
        for i in 0..40 {
            conn.execute("INSERT INTO beta VALUES (?1)", [format!("b{i} padded out")])
                .unwrap();
        }

        let mut view = FrameView::open(&path, HashMap::new()).expect("frames");
        let all = view.frames.len();
        assert!(view.visible().len() == all, "no filter lists everything");

        // Only alpha's frames.
        view.filter = "alpha".into();
        let listed = view.visible();
        assert!(
            !listed.is_empty() && listed.len() < all,
            "{} of {all}",
            listed.len()
        );
        assert!(
            listed.iter().all(
                |&i| view.owners.get(&view.frames[i].page).map(String::as_str) == Some("alpha")
            ),
            "every listed frame belongs to alpha"
        );

        // Stepping stays inside the filter.
        view.sel = listed[0];
        view.on_key(KeyCode::Char('j'), 5);
        assert_eq!(view.sel, listed[1], "j moves to the next listed frame");
        view.on_key(KeyCode::Char('G'), 5);
        assert_eq!(
            view.sel,
            *listed.last().unwrap(),
            "G is the oldest listed one"
        );

        // `commit` is a filter too, and Esc clears an applied one without leaving.
        view.filter = "commit".into();
        assert!(
            view.visible().iter().all(|&i| view.frames[i].commit),
            "only commit frames"
        );
        assert_eq!(view.on_key(KeyCode::Esc, 5), Action::None);
        assert!(view.filter.is_empty(), "the first Esc clears the filter");
        assert_eq!(
            view.on_key(KeyCode::Esc, 5),
            Action::Back,
            "the second leaves"
        );

        // A filter matching nothing lists nothing rather than everything.
        view.filter = "no such table".into();
        assert!(view.visible().is_empty());
        drop(conn);
        clean(&path);
    }

    /// A database with no log has nothing to inspect, which the host has to be
    /// able to tell.
    #[test]
    fn no_log_means_no_view() {
        let path = scratch("nolog.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('x')")
            .unwrap();
        drop(conn);
        assert!(
            FrameView::open(&path, HashMap::new()).is_none(),
            "rollback-journal mode has no frames to walk"
        );
        clean(&path);
    }
}
