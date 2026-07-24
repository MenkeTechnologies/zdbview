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
