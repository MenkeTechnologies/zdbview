//! CSV reading for `--import`, the counterpart to the writers in [`crate::export`]
//! and the shell's `.import`.
//!
//! RFC 4180 with the concessions every real file needs: CRLF or LF line endings, a
//! quoted field may contain commas, newlines and doubled quotes, and a trailing
//! newline is not a final empty record. Written by hand for the same reason the
//! writers are: one dependency-free pass, and the escaping rules are the whole
//! problem.

use anyhow::{anyhow, Result};

/// A parsed file: the header row and the records under it.
#[derive(Debug, PartialEq)]
pub struct Csv {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Parse `text` as CSV with `sep` as the field separator (`,` for CSV, `\t` for
/// the shell's `.mode tabs` files). The first record is the header.
pub fn parse(text: &str, sep: char) -> Result<Csv> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut field = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    // `started` distinguishes a file's trailing newline from an empty last record.
    let mut started = false;

    while let Some(c) = chars.next() {
        started = true;
        if quoted {
            if c == '"' {
                // A doubled quote is one literal quote; anything else closes.
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            _ if c == sep => row.push(std::mem::take(&mut field)),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut row));
                started = false;
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut row));
                started = false;
            }
            _ => field.push(c),
        }
    }
    if quoted {
        return Err(anyhow!("unterminated quoted field"));
    }
    if started {
        row.push(field);
        records.push(row);
    }
    let mut it = records.into_iter();
    let header = it.next().ok_or_else(|| anyhow!("empty file"))?;
    Ok(Csv {
        header,
        rows: it.collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_records_split_on_the_separator() {
        let csv = parse("a,b\n1,2\n3,4\n", ',').unwrap();
        assert_eq!(csv.header, ["a", "b"]);
        assert_eq!(csv.rows, [["1", "2"], ["3", "4"]]);
    }

    #[test]
    fn a_quoted_field_may_hold_the_separator_a_newline_and_a_quote() {
        let csv = parse("a,b\n\"x,y\",\"line1\nline2\"\n\"say \"\"hi\"\"\",z\n", ',').unwrap();
        assert_eq!(csv.rows[0], ["x,y", "line1\nline2"]);
        assert_eq!(csv.rows[1], ["say \"hi\"", "z"]);
    }

    #[test]
    fn crlf_and_a_missing_final_newline_are_both_accepted() {
        let csv = parse("a,b\r\n1,2\r\n3,4", ',').unwrap();
        assert_eq!(csv.rows, [["1", "2"], ["3", "4"]]);
        // A trailing newline must not produce a record of one empty field.
        assert_eq!(parse("a\n1\n", ',').unwrap().rows, [["1"]]);
    }

    #[test]
    fn empty_fields_survive_and_tabs_work_as_a_separator() {
        assert_eq!(parse("a,b,c\n1,,3\n", ',').unwrap().rows[0], ["1", "", "3"]);
        let tsv = parse("a\tb\n1\t2\n", '\t').unwrap();
        assert_eq!(tsv.header, ["a", "b"]);
        assert_eq!(tsv.rows, [["1", "2"]]);
    }

    #[test]
    fn a_file_that_cannot_be_parsed_is_an_error() {
        assert!(parse("a,b\n\"unterminated\n", ',').is_err());
        assert!(parse("", ',').is_err(), "an empty file has no header");
    }
}
