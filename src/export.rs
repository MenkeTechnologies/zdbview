//! Dependency-free CSV/JSON export of tabular and key/value data.

/// Render columns + rows as RFC-4180 CSV (fields quoted when needed).
pub fn rows_to_csv(columns: &[String], rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str(&csv_line(columns));
    out.push('\n');
    for r in rows {
        out.push_str(&csv_line(r));
        out.push('\n');
    }
    out
}

/// Render columns + rows as a JSON array of objects.
pub fn rows_to_json(columns: &[String], rows: &[Vec<String>]) -> String {
    let mut out = String::from("[");
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        for (j, c) in columns.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&json_str(c));
            out.push(':');
            out.push_str(&json_str(r.get(j).map(|s| s.as_str()).unwrap_or("")));
        }
        out.push('}');
    }
    out.push(']');
    out
}

/// One decoded key/value record for export: key, named scalar fields, and the
/// raw value bytes (emitted as a lowercase hex string).
pub struct RecordExport<'a> {
    pub key: &'a str,
    pub fields: &'a [(String, String)],
    pub value: &'a [u8],
}

/// Render key/value records as a JSON array of objects.
pub fn records_to_json(records: &[RecordExport]) -> String {
    let mut out = String::from("[");
    for (i, rec) in records.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"key\":");
        out.push_str(&json_str(rec.key));
        for (name, val) in rec.fields {
            out.push(',');
            out.push_str(&json_str(name));
            out.push(':');
            out.push_str(&json_str(val));
        }
        out.push_str(",\"value_hex\":");
        out.push_str(&json_str(&to_hex(rec.value)));
        out.push('}');
    }
    out.push(']');
    out
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn csv_line(fields: &[String]) -> String {
    fields
        .iter()
        .map(|f| csv_field(f))
        .collect::<Vec<_>>()
        .join(",")
}

/// JSON-escape a string, including the surrounding quotes.
pub fn json_escape(s: &str) -> String {
    json_str(s)
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// One row as an `INSERT`, quoted the way SQLite quotes text. Used by the SQL dump
/// and by the `insert` output mode, which is the shell's `.mode insert`.
pub fn insert_statement(table: &str, columns: &[String], row: &[String]) -> String {
    let cols: Vec<String> = columns
        .iter()
        .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
        .collect();
    let vals: Vec<String> = row.iter().map(|v| sql_literal(v)).collect();
    format!(
        "INSERT INTO \"{}\" ({}) VALUES ({});",
        table.replace('"', "\"\""),
        cols.join(", "),
        vals.join(", ")
    )
}

/// A cell as a SQL literal: `NULL` unquoted, a number bare, anything else quoted
/// with doubled single quotes.
///
/// This works from what the grid displays, so a text cell whose contents read
/// `NULL` or `42` comes out unquoted. `--export sql`, which dumps from the
/// database rather than the grid, is type-exact instead.
fn sql_literal(v: &str) -> String {
    if v == "NULL" {
        return "NULL".into();
    }
    if !v.is_empty() && v.parse::<f64>().is_ok() {
        return v.to_string();
    }
    format!("'{}'", v.replace('\'', "''"))
}

/// Tab-separated, the shell's `.mode tabs`. Tabs and newlines inside a value are
/// escaped rather than written raw, or one row would become two.
pub fn rows_to_tsv(columns: &[String], rows: &[Vec<String>]) -> String {
    let esc = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('\t', "\\t")
            .replace('\n', "\\n")
    };
    let mut out = columns
        .iter()
        .map(|c| esc(c))
        .collect::<Vec<_>>()
        .join("\t");
    out.push('\n');
    for r in rows {
        out.push_str(&r.iter().map(|c| esc(c)).collect::<Vec<_>>().join("\t"));
        out.push('\n');
    }
    out
}

/// A GitHub-flavoured markdown table, the shell's `.mode markdown`. Column widths
/// follow the widest cell so the source lines up as text too.
pub fn rows_to_markdown(columns: &[String], rows: &[Vec<String>]) -> String {
    let esc = |s: &str| s.replace('|', "\\|").replace('\n', " ");
    let mut widths: Vec<usize> = columns.iter().map(|c| esc(c).chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(esc(c).chars().count());
            }
        }
    }
    let line = |cells: Vec<String>| -> String {
        let padded: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<width$}", c, width = widths.get(i).copied().unwrap_or(0)))
            .collect();
        format!("| {} |\n", padded.join(" | "))
    };
    let mut out = line(columns.iter().map(|c| esc(c)).collect());
    out.push_str(&format!(
        "|{}|\n",
        widths
            .iter()
            .map(|w| "-".repeat(w + 2))
            .collect::<Vec<_>>()
            .join("|")
    ));
    for r in rows {
        out.push_str(&line(r.iter().map(|c| esc(c)).collect()));
    }
    out
}

/// One `column = value` per line with a blank line between rows: the shell's
/// `.mode line`, which is what you want when a row is too wide to read across.
pub fn rows_to_lines(columns: &[String], rows: &[Vec<String>]) -> String {
    let width = columns.iter().map(|c| c.chars().count()).max().unwrap_or(0);
    let mut out = String::new();
    for r in rows {
        for (i, c) in columns.iter().enumerate() {
            out.push_str(&format!(
                "{:>width$} = {}\n",
                c,
                r.get(i).map(String::as_str).unwrap_or(""),
                width = width
            ));
        }
        out.push('\n');
    }
    out
}

/// Every row as an `INSERT`, the shell's `.mode insert`.
pub fn rows_to_inserts(table: &str, columns: &[String], rows: &[Vec<String>]) -> String {
    rows.iter()
        .map(|r| format!("{}\n", insert_statement(table, columns, r)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_when_needed() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let rows = vec![
            vec!["1".to_string(), "plain".to_string()],
            vec!["2".to_string(), "has,comma".to_string()],
            vec!["3".to_string(), "has\"quote".to_string()],
        ];
        let csv = rows_to_csv(&cols, &rows);
        assert!(csv.starts_with("a,b\n"));
        assert!(csv.contains("2,\"has,comma\""));
        assert!(csv.contains("3,\"has\"\"quote\""));
    }

    #[test]
    fn json_escapes_and_shapes() {
        let cols = vec!["k".to_string()];
        let rows = vec![vec!["a\"b\nc".to_string()]];
        let j = rows_to_json(&cols, &rows);
        assert_eq!(j, "[{\"k\":\"a\\\"b\\nc\"}]");
    }

    /// The four output modes ported from the shell (`.mode tabs`, `.mode markdown`,
    /// `.mode line`, `.mode insert`), each checked on the case that breaks a naive
    /// implementation: a separator inside a value.
    #[test]
    fn tsv_escapes_its_own_separators() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let rows = vec![vec!["1".to_string(), "two\tlines\nhere".to_string()]];
        let tsv = rows_to_tsv(&cols, &rows);
        assert_eq!(tsv, "a\tb\n1\ttwo\\tlines\\nhere\n");
        assert_eq!(
            tsv.lines().count(),
            2,
            "an embedded newline must not split the row"
        );
    }

    #[test]
    fn markdown_pads_columns_and_escapes_pipes() {
        let cols = vec!["id".to_string(), "note".to_string()];
        let rows = vec![
            vec!["1".to_string(), "a|b".to_string()],
            vec!["22".to_string(), "longer".to_string()],
        ];
        let md = rows_to_markdown(&cols, &rows);
        let lines: Vec<&str> = md.lines().collect();
        assert_eq!(lines[0], "| id | note   |");
        assert_eq!(lines[1], "|----|--------|");
        assert_eq!(
            lines[2], "| 1  | a\\|b   |",
            "a pipe inside a cell is escaped"
        );
        assert_eq!(lines[3], "| 22 | longer |");
    }

    #[test]
    fn line_mode_prints_one_column_per_line() {
        let cols = vec!["id".to_string(), "body".to_string()];
        let rows = vec![
            vec!["1".to_string(), "x".to_string()],
            vec!["2".to_string(), "y".to_string()],
        ];
        assert_eq!(
            rows_to_lines(&cols, &rows),
            "  id = 1\nbody = x\n\n  id = 2\nbody = y\n\n",
            "names right-aligned to the widest, a blank line between rows"
        );
    }

    #[test]
    fn insert_mode_quotes_like_sqlite() {
        let cols = vec!["id".to_string(), "note".to_string(), "gone".to_string()];
        let rows = vec![vec![
            "1".to_string(),
            "it's here".to_string(),
            "NULL".to_string(),
        ]];
        let out = rows_to_inserts("my table", &cols, &rows);
        assert_eq!(
            out.trim_end(),
            r#"INSERT INTO "my table" ("id", "note", "gone") VALUES (1, 'it''s here', NULL);"#
        );
        // A name carrying a double quote is still one identifier.
        let odd = vec!["we\"ird".to_string()];
        assert!(
            insert_statement("t", &odd, &["v".to_string()]).contains("\"we\"\"ird\""),
            "quotes double inside identifiers"
        );
    }

    #[test]
    fn records_json_includes_hex_value() {
        let fields = vec![("len".to_string(), "2".to_string())];
        let recs = vec![RecordExport {
            key: "/x",
            fields: &fields,
            value: &[0xde, 0xad],
        }];
        let j = records_to_json(&recs);
        assert_eq!(j, "[{\"key\":\"/x\",\"len\":\"2\",\"value_hex\":\"dead\"}]");
    }
}
