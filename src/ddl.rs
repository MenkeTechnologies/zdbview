//! Schema editing: the model behind DB Browser for SQLite's "Edit Table
//! Definition" and "Edit Index" dialogs.
//!
//! Everything here is pure: DDL text in, a structure out, DDL text back. The
//! store runs the statements ([`crate::sqlite::SqliteStore::apply_ddl`]); this
//! module decides what they are, which is the part worth testing without a
//! database.
//!
//! The source of truth for an existing object is its own `CREATE` statement in
//! `sqlite_master`, not the pragmas. `PRAGMA table_info` knows the name, type,
//! `NOT NULL`, default and primary key of a column and nothing else — it cannot
//! see `COLLATE`, `CHECK`, `UNIQUE`, `REFERENCES` or a generated expression, and
//! a rebuild driven by the pragmas would silently drop all five. So the DDL is
//! parsed, and anything this module does not model is carried through verbatim
//! in [`ColumnDef::extra`] and [`TableDef::constraints`] rather than lost.
//!
//! Editing a column is not `ALTER TABLE` in SQLite: only appending a column,
//! renaming one, dropping one and renaming the table are native. Everything else
//! — a changed type, a changed constraint, a reordered column — is the rebuild
//! SQLite documents and DB4S performs: create a new table, copy the rows across,
//! drop the old one, rename the new one into its place, then put the indexes,
//! triggers and views back. [`plan`] picks whichever of those is enough.

/// One column of a table definition.
///
/// `orig` is what makes a rebuild lossless: it records the name the column had
/// when the definition was read, so a renamed column is still recognised as the
/// same column and its data is carried across, while a column the user added has
/// no original and is filled from its default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    /// The name this column had in the database, or `None` for a new column.
    pub orig: Option<String>,
    /// Declared type. Empty is legal SQLite — a column with no type at all.
    pub ty: String,
    pub not_null: bool,
    pub pk: bool,
    pub autoincrement: bool,
    pub unique: bool,
    /// The default as SQL: a literal (`0`, `'x'`), a keyword (`CURRENT_TIME`) or
    /// a parenthesised expression. Empty means no default.
    pub default: String,
    /// The body of a `CHECK (…)`, without the parentheses.
    pub check: String,
    pub collate: String,
    /// A foreign key, from `REFERENCES` to the end of the clause.
    pub fk: String,
    /// `GENERATED ALWAYS AS (…)` body, without the parentheses.
    pub generated: String,
    /// A generated column is `STORED` rather than the default `VIRTUAL`.
    pub stored: bool,
    /// Anything in the column definition this module does not model, kept so a
    /// rebuild reproduces it instead of dropping it.
    pub extra: String,
}

impl ColumnDef {
    /// A new column with just a name and type, as the designer's `a` key adds it.
    pub fn new(name: &str, ty: &str) -> Self {
        ColumnDef {
            name: name.to_string(),
            ty: ty.to_string(),
            ..Default::default()
        }
    }

    /// This column as it appears inside `CREATE TABLE`. `inline_pk` is false when
    /// the table has a composite primary key, which has to be a table constraint.
    pub fn to_sql(&self, inline_pk: bool) -> String {
        let mut s = quote(&self.name);
        if !self.ty.is_empty() {
            s.push(' ');
            s.push_str(&self.ty);
        }
        if !self.generated.is_empty() {
            s.push_str(" GENERATED ALWAYS AS (");
            s.push_str(&self.generated);
            s.push(')');
            s.push_str(if self.stored { " STORED" } else { " VIRTUAL" });
        }
        if self.pk && inline_pk {
            s.push_str(" PRIMARY KEY");
            if self.autoincrement {
                s.push_str(" AUTOINCREMENT");
            }
        }
        if self.not_null {
            s.push_str(" NOT NULL");
        }
        if self.unique {
            s.push_str(" UNIQUE");
        }
        if !self.default.is_empty() {
            s.push_str(" DEFAULT ");
            s.push_str(&self.default);
        }
        if !self.collate.is_empty() {
            s.push_str(" COLLATE ");
            s.push_str(&self.collate);
        }
        if !self.check.is_empty() {
            s.push_str(" CHECK (");
            s.push_str(&self.check);
            s.push(')');
        }
        if !self.fk.is_empty() {
            s.push(' ');
            s.push_str(&self.fk);
        }
        if !self.extra.is_empty() {
            s.push(' ');
            s.push_str(&self.extra);
        }
        s
    }

    /// Whether anything but the name differs — the test for "this column can be
    /// renamed in place instead of rebuilt".
    fn same_shape(&self, other: &ColumnDef) -> bool {
        self.ty == other.ty
            && self.not_null == other.not_null
            && self.pk == other.pk
            && self.autoincrement == other.autoincrement
            && self.unique == other.unique
            && self.default == other.default
            && self.check == other.check
            && self.collate == other.collate
            && self.fk == other.fk
            && self.generated == other.generated
            && self.stored == other.stored
            && self.extra == other.extra
    }
}

/// A whole table definition, as the designer edits it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableDef {
    pub name: String,
    /// The name the table had when it was read, or `None` for a new table.
    pub orig: Option<String>,
    pub columns: Vec<ColumnDef>,
    /// Table-level constraints (`PRIMARY KEY (a,b)`, `FOREIGN KEY …`, a named
    /// `CONSTRAINT …`) kept as written. The composite primary key is the one
    /// exception: it is re-derived from the columns on the way out, so editing
    /// which columns are keys works.
    pub constraints: Vec<String>,
    pub without_rowid: bool,
    pub strict: bool,
}

impl TableDef {
    /// Columns marked as primary key, in table order.
    pub fn pk_columns(&self) -> Vec<&ColumnDef> {
        self.columns.iter().filter(|c| c.pk).collect()
    }

    /// The `CREATE TABLE` statement for this definition, under `name` — which is
    /// not always `self.name`, because a rebuild first creates the table under a
    /// temporary name.
    pub fn create_sql_as(&self, name: &str) -> String {
        let pk = self.pk_columns();
        let inline_pk = pk.len() == 1;
        let mut parts: Vec<String> = self.columns.iter().map(|c| c.to_sql(inline_pk)).collect();
        if pk.len() > 1 {
            let cols: Vec<String> = pk.iter().map(|c| quote(&c.name)).collect();
            parts.push(format!("PRIMARY KEY ({})", cols.join(", ")));
        }
        parts.extend(self.constraints.iter().cloned());

        let mut s = format!("CREATE TABLE {} (\n\t", quote(name));
        s.push_str(&parts.join(",\n\t"));
        s.push_str("\n)");
        let mut tail: Vec<&str> = Vec::new();
        if self.without_rowid {
            tail.push("WITHOUT ROWID");
        }
        if self.strict {
            tail.push("STRICT");
        }
        if !tail.is_empty() {
            s.push(' ');
            s.push_str(&tail.join(", "));
        }
        s
    }

    pub fn create_sql(&self) -> String {
        self.create_sql_as(&self.name)
    }

    /// What is wrong with this definition, if anything. Checked before any
    /// statement is generated, so a bad edit is reported rather than half-applied.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("table needs a name".into());
        }
        if self.columns.is_empty() {
            return Err("table needs at least one column".into());
        }
        for (i, c) in self.columns.iter().enumerate() {
            if c.name.trim().is_empty() {
                return Err(format!("column {} needs a name", i + 1));
            }
            if self
                .columns
                .iter()
                .skip(i + 1)
                .any(|o| o.name.eq_ignore_ascii_case(&c.name))
            {
                return Err(format!("duplicate column name \"{}\"", c.name));
            }
        }
        let pk = self.pk_columns();
        if self.without_rowid && pk.is_empty() {
            return Err("WITHOUT ROWID needs a primary key".into());
        }
        if let Some(c) = pk.iter().find(|c| c.autoincrement) {
            if pk.len() > 1 {
                return Err("AUTOINCREMENT needs a single-column primary key".into());
            }
            if !c.ty.trim().eq_ignore_ascii_case("INTEGER") {
                return Err("AUTOINCREMENT needs an INTEGER primary key".into());
            }
        }
        Ok(())
    }
}

/// One term of an index: a column name or an expression, with its direction and
/// collation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexColumn {
    /// A bare column name, or an expression when the index is on one.
    pub expr: String,
    pub collate: String,
    pub desc: bool,
}

impl IndexColumn {
    fn to_sql(&self, quote_name: bool) -> String {
        let mut s = if quote_name {
            quote(&self.expr)
        } else {
            self.expr.clone()
        };
        if !self.collate.is_empty() {
            s.push_str(" COLLATE ");
            s.push_str(&self.collate);
        }
        if self.desc {
            s.push_str(" DESC");
        }
        s
    }
}

/// An index definition, as the designer edits it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexDef {
    pub name: String,
    pub orig: Option<String>,
    pub table: String,
    pub unique: bool,
    pub columns: Vec<IndexColumn>,
    /// The body of a partial index's `WHERE`, without the keyword.
    pub where_clause: String,
}

impl IndexDef {
    pub fn create_sql(&self) -> String {
        let cols: Vec<String> = self
            .columns
            .iter()
            // An expression is emitted as written; a plain name is quoted, so a
            // column called `index` or `order` still works.
            .map(|c| c.to_sql(is_plain_ident(&c.expr)))
            .collect();
        let mut s = format!(
            "CREATE {}INDEX {} ON {} ({})",
            if self.unique { "UNIQUE " } else { "" },
            quote(&self.name),
            quote(&self.table),
            cols.join(", ")
        );
        if !self.where_clause.is_empty() {
            s.push_str(" WHERE ");
            s.push_str(&self.where_clause);
        }
        s
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("index needs a name".into());
        }
        if self.table.trim().is_empty() {
            return Err("index needs a table".into());
        }
        if self.columns.iter().all(|c| c.expr.trim().is_empty()) {
            return Err("index needs at least one column".into());
        }
        Ok(())
    }
}

// ----- emitting identifiers -------------------------------------------------

/// Quote an identifier for SQL, doubling any embedded quote.
pub fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Whether `s` is a single bare identifier, so it can be quoted rather than
/// pasted through as an expression.
fn is_plain_ident(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

// ----- scanning -------------------------------------------------------------

/// Split `s` on top-level `sep`, ignoring separators inside parentheses, string
/// literals, quoted identifiers and comments. This is what makes a column list
/// splittable without a full parser: `CHECK (a, b)` and `'a,b'` stay whole.
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            '\'' | '"' | '`' | '[' => {
                let (text, len) = read_quoted(&s[i..]);
                cur.push_str(text);
                for _ in 1..len {
                    it.next();
                }
            }
            '-' if s[i..].starts_with("--") => {
                // Line comment: skip to the newline, keeping the newline itself.
                for (_, c2) in it.by_ref() {
                    if c2 == '\n' {
                        break;
                    }
                }
                cur.push(' ');
            }
            '/' if s[i..].starts_with("/*") => {
                let end = s[i + 2..]
                    .find("*/")
                    .map(|p| i + 2 + p + 2)
                    .unwrap_or(s.len());
                for _ in i + 1..end {
                    it.next();
                }
                cur.push(' ');
            }
            c if c == sep && depth == 0 => {
                out.push(cur.trim().to_string());
                cur = String::new();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// The quoted run at the start of `s` (`'…'`, `"…"`, `` `…` `` or `[…]`), as the
/// text including its delimiters and its length in `char`s.
fn read_quoted(s: &str) -> (&str, usize) {
    let mut it = s.char_indices();
    let (_, open) = match it.next() {
        Some(x) => x,
        None => return ("", 0),
    };
    let close = match open {
        '[' => ']',
        c => c,
    };
    let mut n = 1usize;
    while let Some((i, c)) = it.next() {
        n += 1;
        if c == close {
            // A doubled delimiter is an escaped one, so it does not end the run.
            if open != '[' && s[i + c.len_utf8()..].starts_with(close) {
                it.next();
                n += 1;
                continue;
            }
            return (&s[..i + c.len_utf8()], n);
        }
    }
    (s, n)
}

/// A token of SQL at nesting depth 0, as `(byte offset, text)`. Parenthesised
/// groups arrive as one token including their parentheses, which is what lets a
/// keyword scan ignore anything inside `CHECK (…)`.
fn tokens(s: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < s.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '(' {
            let start = i;
            let mut depth = 0usize;
            while i < s.len() {
                match b[i] as char {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    '\'' | '"' | '`' | '[' => {
                        let (t, _) = read_quoted(&s[i..]);
                        i += t.len();
                        continue;
                    }
                    _ => {}
                }
                i += 1;
            }
            out.push((start, s[start..i].to_string()));
            continue;
        }
        if matches!(c, '\'' | '"' | '`' | '[') {
            let (t, _) = read_quoted(&s[i..]);
            out.push((i, t.to_string()));
            i += t.len();
            continue;
        }
        if c == '-' && s[i..].starts_with("--") {
            i = s[i..].find('\n').map(|p| i + p).unwrap_or(s.len());
            continue;
        }
        if c == '/' && s[i..].starts_with("/*") {
            i = s[i + 2..]
                .find("*/")
                .map(|p| i + 2 + p + 2)
                .unwrap_or(s.len());
            continue;
        }
        // A bare run: identifier, number or operator up to the next boundary.
        let start = i;
        if c.is_alphanumeric() || c == '_' || c == '$' || c == '.' {
            while i < s.len() {
                let c2 = b[i] as char;
                if c2.is_alphanumeric() || c2 == '_' || c2 == '$' || c2 == '.' {
                    i += 1;
                } else {
                    break;
                }
            }
        } else {
            i += 1;
        }
        out.push((start, s[start..i].to_string()));
    }
    out
}

/// Strip the quoting from one identifier token.
pub fn unquote(tok: &str) -> String {
    let t = tok.trim();
    let mut cs = t.chars();
    match (cs.next(), t.chars().last()) {
        (Some('"'), Some('"')) if t.len() >= 2 => t[1..t.len() - 1].replace("\"\"", "\""),
        (Some('`'), Some('`')) if t.len() >= 2 => t[1..t.len() - 1].replace("``", "`"),
        (Some('['), Some(']')) if t.len() >= 2 => t[1..t.len() - 1].to_string(),
        (Some('\''), Some('\'')) if t.len() >= 2 => t[1..t.len() - 1].replace("''", "'"),
        _ => t.to_string(),
    }
}

/// The inside of a parenthesised token, or the token itself when it is not one.
fn inner(tok: &str) -> String {
    let t = tok.trim();
    if t.starts_with('(') && t.ends_with(')') && t.len() >= 2 {
        t[1..t.len() - 1].trim().to_string()
    } else {
        t.to_string()
    }
}

// ----- parsing --------------------------------------------------------------

/// Keywords that end the type name and start a column constraint.
fn is_constraint_start(tok: &str) -> bool {
    matches!(
        tok.to_ascii_uppercase().as_str(),
        "CONSTRAINT"
            | "PRIMARY"
            | "NOT"
            | "NULL"
            | "UNIQUE"
            | "CHECK"
            | "DEFAULT"
            | "COLLATE"
            | "REFERENCES"
            | "GENERATED"
            | "AS"
    )
}

/// Table-level constraint openers, which is how a definition in the column list
/// is told from a column.
fn is_table_constraint(tok: &str) -> bool {
    matches!(
        tok.to_ascii_uppercase().as_str(),
        "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN"
    )
}

/// Parse one column definition — everything between two top-level commas of a
/// `CREATE TABLE` column list.
pub fn parse_column(def: &str) -> ColumnDef {
    let toks = tokens(def);
    let mut col = ColumnDef::default();
    if toks.is_empty() {
        return col;
    }
    col.name = unquote(&toks[0].1);
    col.orig = Some(col.name.clone());

    // The type is everything up to the first constraint keyword. A named
    // constraint (`CONSTRAINT c CHECK …`) opens with CONSTRAINT, so the scan
    // stops there too.
    let mut i = 1usize;
    let ty_start = toks.get(1).map(|t| t.0).unwrap_or(def.len());
    let mut ty_end = ty_start;
    while i < toks.len() && !is_constraint_start(&toks[i].1) {
        ty_end = toks[i].0 + toks[i].1.len();
        i += 1;
    }
    col.ty = def[ty_start.min(def.len())..ty_end.min(def.len())]
        .trim()
        .to_string();

    let mut extra: Vec<String> = Vec::new();
    while i < toks.len() {
        let up = toks[i].1.to_ascii_uppercase();
        match up.as_str() {
            // A named constraint keeps its name only when the constraint itself
            // is one this module does not model; the modelled ones lose the name,
            // which is what DB4S's dialog does too.
            "CONSTRAINT" => i += 2,
            "PRIMARY" => {
                col.pk = true;
                i += 2; // PRIMARY KEY
                while i < toks.len() {
                    match toks[i].1.to_ascii_uppercase().as_str() {
                        "ASC" | "DESC" => i += 1,
                        "AUTOINCREMENT" => {
                            col.autoincrement = true;
                            i += 1;
                        }
                        "ON" => i += 3, // ON CONFLICT <action>
                        _ => break,
                    }
                }
            }
            "NOT" => {
                col.not_null = true;
                i += 2; // NOT NULL
                if toks.get(i).is_some_and(|t| t.1.eq_ignore_ascii_case("ON")) {
                    i += 3;
                }
            }
            "NULL" => i += 1,
            "UNIQUE" => {
                col.unique = true;
                i += 1;
                if toks.get(i).is_some_and(|t| t.1.eq_ignore_ascii_case("ON")) {
                    i += 3;
                }
            }
            "CHECK" => {
                col.check = toks.get(i + 1).map(|t| inner(&t.1)).unwrap_or_default();
                i += 2;
            }
            "DEFAULT" => {
                // A default is one token: a literal, a keyword, a parenthesised
                // expression, or a signed number, which arrives as two.
                let mut val = toks.get(i + 1).map(|t| t.1.clone()).unwrap_or_default();
                i += 2;
                if (val == "-" || val == "+") && i < toks.len() {
                    val.push_str(&toks[i].1);
                    i += 1;
                }
                col.default = val;
            }
            "COLLATE" => {
                col.collate = toks.get(i + 1).map(|t| t.1.clone()).unwrap_or_default();
                i += 2;
            }
            "GENERATED" | "AS" => {
                // GENERATED ALWAYS AS (expr) [STORED|VIRTUAL], or just AS (expr).
                let mut j = i + 1;
                if up == "GENERATED" {
                    j += 1; // ALWAYS
                    if toks.get(j).is_some_and(|t| t.1.eq_ignore_ascii_case("AS")) {
                        j += 1;
                    }
                }
                col.generated = toks.get(j).map(|t| inner(&t.1)).unwrap_or_default();
                j += 1;
                if toks
                    .get(j)
                    .is_some_and(|t| t.1.eq_ignore_ascii_case("STORED"))
                {
                    col.stored = true;
                    j += 1;
                } else if toks
                    .get(j)
                    .is_some_and(|t| t.1.eq_ignore_ascii_case("VIRTUAL"))
                {
                    j += 1;
                }
                i = j;
            }
            "REFERENCES" => {
                // The reference clause runs to the end of the definition, since
                // its actions (`ON DELETE CASCADE`, `DEFERRABLE …`) are open-ended.
                col.fk = def[toks[i].0..].trim().to_string();
                i = toks.len();
            }
            _ => {
                extra.push(toks[i].1.clone());
                i += 1;
            }
        }
    }
    col.extra = extra.join(" ");
    col
}

/// Parse a `CREATE TABLE` statement into the definition the designer edits.
/// Returns `None` when the statement is not one — a virtual table, say, whose
/// module arguments are not a column list.
pub fn parse_table(sql: &str) -> Option<TableDef> {
    let toks = tokens(sql);
    let mut i = 0usize;
    if !toks.first()?.1.eq_ignore_ascii_case("CREATE") {
        return None;
    }
    i += 1;
    // TEMP/TEMPORARY, and VIRTUAL which this module cannot model.
    while i < toks.len() {
        match toks[i].1.to_ascii_uppercase().as_str() {
            "TEMP" | "TEMPORARY" => i += 1,
            "VIRTUAL" => return None,
            _ => break,
        }
    }
    if !toks.get(i)?.1.eq_ignore_ascii_case("TABLE") {
        return None;
    }
    i += 1;
    if toks.get(i).is_some_and(|t| t.1.eq_ignore_ascii_case("IF")) {
        i += 3; // IF NOT EXISTS
    }
    let name = unquote(&toks.get(i)?.1);
    // A schema-qualified name (`main.t`) arrives as one token because `.` is part
    // of a bare run; the table is the part after the dot.
    let name = name.rsplit('.').next().unwrap_or(&name).to_string();
    i += 1;

    let body_tok = toks.get(i)?;
    if !body_tok.1.starts_with('(') {
        return None;
    }
    let body = inner(&body_tok.1);
    let tail = sql[body_tok.0 + body_tok.1.len()..].to_ascii_uppercase();

    let mut def = TableDef {
        name: name.clone(),
        orig: Some(name),
        without_rowid: tail.contains("WITHOUT ROWID"),
        strict: tail
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| w == "STRICT"),
        ..Default::default()
    };

    for part in split_top_level(&body, ',') {
        let first = match tokens(&part).into_iter().next() {
            Some((_, t)) => t,
            None => continue,
        };
        // `CONSTRAINT` opens a table constraint only when what follows it is one;
        // as a column name it is followed by a type.
        let table_level = if first.eq_ignore_ascii_case("CONSTRAINT") {
            tokens(&part)
                .get(2)
                .is_some_and(|t| is_table_constraint(&t.1))
        } else {
            is_table_constraint(&first) && !first.eq_ignore_ascii_case("CHECK")
                || first.eq_ignore_ascii_case("CHECK")
        };
        if table_level {
            def.constraints.push(part);
        } else {
            def.columns.push(parse_column(&part));
        }
    }

    // A composite primary key is a table constraint, but the designer edits it as
    // a per-column flag, so it is lifted onto the columns and dropped from the
    // constraint list — `create_sql` puts it back.
    if let Some(pos) = def.constraints.iter().position(|c| {
        let t = tokens(c);
        t.first()
            .is_some_and(|x| x.1.eq_ignore_ascii_case("PRIMARY"))
    }) {
        let cons = def.constraints.remove(pos);
        let cols = tokens(&cons)
            .into_iter()
            .find(|(_, t)| t.starts_with('('))
            .map(|(_, t)| inner(&t))
            .unwrap_or_default();
        for c in split_top_level(&cols, ',') {
            let key = tokens(&c)
                .into_iter()
                .next()
                .map(|(_, t)| unquote(&t))
                .unwrap_or_default();
            if let Some(col) = def
                .columns
                .iter_mut()
                .find(|col| col.name.eq_ignore_ascii_case(&key))
            {
                col.pk = true;
            }
        }
    }
    Some(def)
}

/// Parse a `CREATE INDEX` statement.
pub fn parse_index(sql: &str) -> Option<IndexDef> {
    let toks = tokens(sql);
    let mut i = 0usize;
    if !toks.first()?.1.eq_ignore_ascii_case("CREATE") {
        return None;
    }
    i += 1;
    let mut def = IndexDef::default();
    if toks.get(i)?.1.eq_ignore_ascii_case("UNIQUE") {
        def.unique = true;
        i += 1;
    }
    if !toks.get(i)?.1.eq_ignore_ascii_case("INDEX") {
        return None;
    }
    i += 1;
    if toks.get(i).is_some_and(|t| t.1.eq_ignore_ascii_case("IF")) {
        i += 3;
    }
    let name = unquote(&toks.get(i)?.1);
    def.name = name.rsplit('.').next().unwrap_or(&name).to_string();
    def.orig = Some(def.name.clone());
    i += 1;
    if !toks.get(i)?.1.eq_ignore_ascii_case("ON") {
        return None;
    }
    i += 1;
    def.table = unquote(&toks.get(i)?.1);
    i += 1;

    let cols = toks.get(i)?;
    if !cols.1.starts_with('(') {
        return None;
    }
    for part in split_top_level(&inner(&cols.1), ',') {
        let pt = tokens(&part);
        let mut c = IndexColumn::default();
        let mut end = part.len();
        let mut j = pt.len();
        // Read the trailing ASC/DESC and COLLATE off the end; what is left is the
        // column or the expression.
        while j > 0 {
            let up = pt[j - 1].1.to_ascii_uppercase();
            if up == "DESC" {
                c.desc = true;
                end = pt[j - 1].0;
                j -= 1;
            } else if up == "ASC" {
                end = pt[j - 1].0;
                j -= 1;
            } else if j >= 2 && pt[j - 2].1.eq_ignore_ascii_case("COLLATE") {
                c.collate = pt[j - 1].1.clone();
                end = pt[j - 2].0;
                j -= 2;
            } else {
                break;
            }
        }
        let expr = part[..end].trim();
        c.expr = if tokens(expr).len() == 1 {
            unquote(expr)
        } else {
            expr.to_string()
        };
        def.columns.push(c);
    }
    i += 1;
    if toks
        .get(i)
        .is_some_and(|t| t.1.eq_ignore_ascii_case("WHERE"))
    {
        let start = toks[i].0 + toks[i].1.len();
        def.where_clause = sql[start..].trim().trim_end_matches(';').trim().to_string();
    }
    Some(def)
}

// ----- planning an edit -----------------------------------------------------

/// The statements that turn `old` into `new`, and whether they rebuild the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterPlan {
    pub statements: Vec<String>,
    /// A rebuild copies the rows through a new table; the cheap paths do not.
    /// The store runs a rebuild with foreign keys off and checks them after.
    pub rebuild: bool,
}

/// Name of the table a rebuild builds before renaming it into place. SQLite's own
/// documented procedure uses a temporary name for exactly this reason: the old
/// table still exists while the rows are copied. The name cannot start with
/// `sqlite_`, which SQLite reserves and refuses to create.
const REBUILD_TMP: &str = "zdbview_rebuild_tmp";

/// Statements that create `def` from nothing.
pub fn plan_create(def: &TableDef) -> AlterPlan {
    AlterPlan {
        statements: vec![def.create_sql()],
        rebuild: false,
    }
}

/// One object that depends on the table being edited: an index, a trigger or a
/// view. A rebuild has to take all three down and put them back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependent {
    /// `sqlite_master.type`: `index`, `trigger` or `view`.
    pub kind: String,
    pub name: String,
    pub sql: String,
}

/// Statements that turn `old` into `new`.
///
/// `aux` is every index, trigger and view that mentions the table. They are
/// dropped before the swap and recreated after it, against the new shape.
///
/// Dropping the views first is not tidiness — it is required. SQLite validates
/// the whole schema during `ALTER TABLE … RENAME TO`, so a view left pointing at
/// the table that the rebuild just dropped makes the rename fail with
/// `error in view …: no such table`, and the rebuild dies half way.
pub fn plan(old: &TableDef, new: &TableDef, aux: &[Dependent]) -> AlterPlan {
    let renamed = !old.name.eq_ignore_ascii_case(&new.name);

    // Which of the old columns survived, by original name.
    let kept: Vec<(&ColumnDef, &ColumnDef)> = new
        .columns
        .iter()
        .filter_map(|n| {
            let o = n.orig.as_ref()?;
            old.columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(o))
                .map(|c| (c, n))
        })
        .collect();
    let dropped = old.columns.len() - kept.len();
    let added = new.columns.iter().filter(|c| c.orig.is_none()).count();
    let reordered = kept.len() == old.columns.len()
        && added == 0
        && !kept
            .iter()
            .zip(old.columns.iter())
            .all(|((o, _), c)| std::ptr::eq(*o, c));
    let shape_changed = kept.iter().any(|(o, n)| !o.same_shape(n))
        || old.constraints != new.constraints
        || old.without_rowid != new.without_rowid
        || old.strict != new.strict;
    let renamed_cols: Vec<(&ColumnDef, &ColumnDef)> = kept
        .iter()
        .filter(|(o, n)| !o.name.eq_ignore_ascii_case(&n.name))
        .cloned()
        .collect();

    // A column can only be appended in place when SQLite allows it: no primary
    // key, no UNIQUE, no NOT NULL without a default, and no STORED generated
    // column. Anything else has to go through the rebuild.
    let appends_ok = new.columns.iter().filter(|c| c.orig.is_none()).all(|c| {
        !c.pk
            && !c.unique
            && (!c.not_null || !c.default.is_empty())
            && !(c.stored && !c.generated.is_empty())
    }) && new
        .columns
        .iter()
        .skip(new.columns.len() - added)
        .all(|c| c.orig.is_none());

    if !shape_changed && !reordered && dropped == 0 && (added == 0 || appends_ok) {
        // The native path: rename the table, rename columns, append columns.
        let mut stmts = Vec::new();
        let mut table = old.name.clone();
        if renamed {
            stmts.push(format!(
                "ALTER TABLE {} RENAME TO {}",
                quote(&old.name),
                quote(&new.name)
            ));
            table = new.name.clone();
        }
        for (o, n) in &renamed_cols {
            stmts.push(format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {}",
                quote(&table),
                quote(&o.name),
                quote(&n.name)
            ));
        }
        for c in new.columns.iter().filter(|c| c.orig.is_none()) {
            stmts.push(format!(
                "ALTER TABLE {} ADD COLUMN {}",
                quote(&table),
                c.to_sql(true)
            ));
        }
        return AlterPlan {
            statements: stmts,
            rebuild: false,
        };
    }

    // The rebuild. A generated column has no stored data, so it is created by the
    // new definition and left out of the copy.
    let copy: Vec<(&ColumnDef, &ColumnDef)> = kept
        .iter()
        .filter(|(o, n)| o.generated.is_empty() && n.generated.is_empty())
        .cloned()
        .collect();
    let mut stmts = vec![new.create_sql_as(REBUILD_TMP)];
    if !copy.is_empty() {
        let dst: Vec<String> = copy.iter().map(|(_, n)| quote(&n.name)).collect();
        let src: Vec<String> = copy.iter().map(|(o, _)| quote(&o.name)).collect();
        stmts.push(format!(
            "INSERT INTO {} ({}) SELECT {} FROM {}",
            quote(REBUILD_TMP),
            dst.join(", "),
            src.join(", "),
            quote(&old.name)
        ));
    }
    // Views and triggers come down before the swap: the rename validates every
    // object in the schema, and one pointing at the dropped table fails it. The
    // table's own indexes go with the table, so they are not dropped by name.
    for dep in aux.iter().filter(|d| d.kind != "index") {
        if let Some(sql) = drop_sql(&dep.kind, &dep.name) {
            stmts.push(sql);
        }
    }
    stmts.push(format!("DROP TABLE {}", quote(&old.name)));
    stmts.push(format!(
        "ALTER TABLE {} RENAME TO {}",
        quote(REBUILD_TMP),
        quote(&new.name)
    ));

    // Put the dependent objects back, following the table's new name and its
    // renamed columns. An index is re-derived through the parser so its table and
    // column names are correct; a trigger or view is rewritten identifier by
    // identifier, which is all that can be done without a full SQL parser.
    for dep in aux {
        stmts.push(rewrite_aux(&dep.sql, old, new, &renamed_cols));
    }
    AlterPlan {
        statements: stmts,
        rebuild: true,
    }
}

/// One dependent object's `CREATE` statement, adjusted for a renamed table and
/// renamed columns.
fn rewrite_aux(
    sql: &str,
    old: &TableDef,
    new: &TableDef,
    renamed_cols: &[(&ColumnDef, &ColumnDef)],
) -> String {
    if let Some(mut idx) = parse_index(sql) {
        idx.table = new.name.clone();
        for c in idx.columns.iter_mut() {
            if let Some((_, n)) = renamed_cols
                .iter()
                .find(|(o, _)| o.name.eq_ignore_ascii_case(&c.expr))
            {
                c.expr = n.name.clone();
            } else if !is_plain_ident(&c.expr) {
                for (o, n) in renamed_cols {
                    c.expr = rename_ident(&c.expr, &o.name, &n.name);
                }
            }
        }
        // An index on a dropped column cannot be recreated; the caller sees the
        // empty statement and skips it rather than failing the whole rebuild.
        let gone = idx.columns.iter().any(|c| {
            is_plain_ident(&c.expr)
                && !new
                    .columns
                    .iter()
                    .any(|col| col.name.eq_ignore_ascii_case(&c.expr))
        });
        return if gone {
            String::new()
        } else {
            idx.create_sql()
        };
    }
    let mut out = rename_ident(sql, &old.name, &new.name);
    for (o, n) in renamed_cols {
        out = rename_ident(&out, &o.name, &n.name);
    }
    out
}

/// Replace every identifier token equal to `from` with `to`, leaving string
/// literals and the insides of comments alone. Quoted and bare occurrences are
/// both replaced, and the result is always quoted.
pub fn rename_ident(sql: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let b = sql.as_bytes();
    let mut i = 0usize;
    while i < sql.len() {
        let c = b[i] as char;
        match c {
            // A single-quoted run is data, never an identifier.
            '\'' => {
                let (t, _) = read_quoted(&sql[i..]);
                out.push_str(t);
                i += t.len();
            }
            '"' | '`' | '[' => {
                let (t, _) = read_quoted(&sql[i..]);
                if unquote(t).eq_ignore_ascii_case(from) {
                    out.push_str(&quote(to));
                } else {
                    out.push_str(t);
                }
                i += t.len();
            }
            _ if c.is_alphanumeric() || c == '_' || c == '$' => {
                let start = i;
                while i < sql.len() {
                    let c2 = b[i] as char;
                    if c2.is_alphanumeric() || c2 == '_' || c2 == '$' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let word = &sql[start..i];
                if word.eq_ignore_ascii_case(from) {
                    out.push_str(&quote(to));
                } else {
                    out.push_str(word);
                }
            }
            _ => {
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

/// Whether `sql` uses `ident` as an identifier — quoted or bare, but never as
/// part of a string literal or a longer word. This is how a view is found to
/// depend on a table, since SQLite records no dependency for one.
pub fn mentions_ident(sql: &str, ident: &str) -> bool {
    // The rewrite is the same scan; a name that is not there rewrites to itself.
    let marker = "\u{0}zdbview\u{0}";
    rename_ident(sql, ident, marker).contains(marker)
}

/// `DROP …` for one schema object, by its `sqlite_master` type.
pub fn drop_sql(kind: &str, name: &str) -> Option<String> {
    let what = match kind.to_ascii_lowercase().as_str() {
        "table" => "TABLE",
        "index" => "INDEX",
        "view" => "VIEW",
        "trigger" => "TRIGGER",
        _ => return None,
    };
    Some(format!("DROP {} {}", what, quote(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A definition that uses every column constraint this module models, so the
    /// parse is checked against something more than `name TYPE`.
    const RICH: &str = r#"CREATE TABLE "order" (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        sku TEXT NOT NULL UNIQUE COLLATE NOCASE,
        qty INT DEFAULT 0 CHECK (qty >= 0),
        note TEXT DEFAULT 'a, b',
        cust INTEGER REFERENCES customer(id) ON DELETE CASCADE,
        total REAL GENERATED ALWAYS AS (qty * 1.5) STORED
    )"#;

    #[test]
    fn parses_every_column_constraint() {
        let d = parse_table(RICH).expect("parses");
        assert_eq!(d.name, "order");
        assert_eq!(d.columns.len(), 6);

        let id = &d.columns[0];
        assert!(id.pk && id.autoincrement && id.ty == "INTEGER");

        let sku = &d.columns[1];
        assert!(sku.not_null && sku.unique);
        assert_eq!(sku.collate, "NOCASE");

        let qty = &d.columns[2];
        assert_eq!(qty.default, "0");
        assert_eq!(qty.check, "qty >= 0");

        // The comma inside the literal must not have split the column list.
        assert_eq!(d.columns[3].default, "'a, b'");

        assert!(d.columns[4].fk.starts_with("REFERENCES customer(id)"));
        assert!(d.columns[4].fk.contains("ON DELETE CASCADE"));

        let total = &d.columns[5];
        assert_eq!(total.generated, "qty * 1.5");
        assert!(total.stored);
    }

    #[test]
    fn round_trips_through_sql() {
        let d = parse_table(RICH).unwrap();
        let again = parse_table(&d.create_sql()).unwrap();
        assert_eq!(d, again, "emitting and re-parsing must be a fixed point");
    }

    #[test]
    fn lifts_a_composite_primary_key_onto_its_columns() {
        let d = parse_table(
            "CREATE TABLE t (a TEXT, b TEXT, c TEXT, PRIMARY KEY (a, b)) WITHOUT ROWID",
        )
        .unwrap();
        assert!(d.without_rowid);
        assert!(
            d.constraints.is_empty(),
            "the key is not left as a constraint"
        );
        assert_eq!(
            d.columns.iter().filter(|c| c.pk).count(),
            2,
            "both key columns are flagged"
        );
        // And it goes back out as a table constraint, not two inline keys.
        let sql = d.create_sql();
        assert!(sql.contains("PRIMARY KEY (\"a\", \"b\")"), "{sql}");
        assert!(!sql.contains("\"a\" TEXT PRIMARY KEY"), "{sql}");
    }

    #[test]
    fn keeps_a_table_constraint_it_does_not_model() {
        let d = parse_table(
            "CREATE TABLE t (a TEXT, b TEXT, CONSTRAINT fk FOREIGN KEY (a) REFERENCES o(id))",
        )
        .unwrap();
        assert_eq!(d.constraints.len(), 1);
        assert!(d.create_sql().contains("FOREIGN KEY"));
    }

    #[test]
    fn refuses_a_virtual_table() {
        assert!(parse_table("CREATE VIRTUAL TABLE t USING fts5(body)").is_none());
    }

    #[test]
    fn renaming_only_uses_alter_table() {
        let old = parse_table("CREATE TABLE t (a TEXT)").unwrap();
        let mut new = old.clone();
        new.name = "t2".into();
        let p = plan(&old, &new, &[]);
        assert!(!p.rebuild);
        assert_eq!(p.statements, vec!["ALTER TABLE \"t\" RENAME TO \"t2\""]);
    }

    #[test]
    fn appending_and_renaming_columns_stays_native() {
        let old = parse_table("CREATE TABLE t (a TEXT)").unwrap();
        let mut new = old.clone();
        new.columns[0].name = "a2".into();
        new.columns.push(ColumnDef::new("b", "INTEGER"));
        let p = plan(&old, &new, &[]);
        assert!(!p.rebuild);
        assert_eq!(
            p.statements,
            vec![
                "ALTER TABLE \"t\" RENAME COLUMN \"a\" TO \"a2\"",
                "ALTER TABLE \"t\" ADD COLUMN \"b\" INTEGER",
            ]
        );
    }

    #[test]
    fn a_not_null_column_without_a_default_forces_the_rebuild() {
        // SQLite rejects this as ADD COLUMN, so the plan must not try.
        let old = parse_table("CREATE TABLE t (a TEXT)").unwrap();
        let mut new = old.clone();
        let mut c = ColumnDef::new("b", "INTEGER");
        c.not_null = true;
        new.columns.push(c);
        let p = plan(&old, &new, &[]);
        assert!(p.rebuild);
    }

    #[test]
    fn dropping_a_column_copies_only_what_is_left() {
        let old = parse_table("CREATE TABLE t (a TEXT, b TEXT, c TEXT)").unwrap();
        let mut new = old.clone();
        new.columns.remove(1);
        let p = plan(&old, &new, &[]);
        assert!(p.rebuild);
        let insert = p
            .statements
            .iter()
            .find(|s| s.starts_with("INSERT INTO"))
            .expect("the rows are copied");
        assert!(
            insert.contains("(\"a\", \"c\") SELECT \"a\", \"c\""),
            "{insert}"
        );
        assert!(p.statements.iter().any(|s| s == "DROP TABLE \"t\""));
        assert!(p
            .statements
            .iter()
            .any(|s| s == "ALTER TABLE \"zdbview_rebuild_tmp\" RENAME TO \"t\""));
    }

    fn dep(kind: &str, name: &str, sql: &str) -> Dependent {
        Dependent {
            kind: kind.into(),
            name: name.into(),
            sql: sql.into(),
        }
    }

    #[test]
    fn a_rebuild_recreates_an_index_against_the_new_names() {
        let old = parse_table("CREATE TABLE t (a TEXT, b TEXT)").unwrap();
        let mut new = old.clone();
        new.name = "t2".into();
        new.columns[0].name = "a2".into();
        new.columns[0].ty = "INTEGER".into(); // forces the rebuild
        let aux = vec![dep("index", "ix", "CREATE INDEX ix ON t (a)")];
        let p = plan(&old, &new, &aux);
        assert!(p.rebuild);
        let last = p.statements.last().unwrap();
        assert_eq!(last, "CREATE INDEX \"ix\" ON \"t2\" (\"a2\")");
        // An index belongs to the table and goes with it; dropping it by name
        // afterwards would fail.
        assert!(!p.statements.iter().any(|s| s.starts_with("DROP INDEX")));
    }

    #[test]
    fn an_index_on_a_dropped_column_is_not_recreated() {
        let old = parse_table("CREATE TABLE t (a TEXT, b TEXT)").unwrap();
        let mut new = old.clone();
        new.columns.remove(0);
        let p = plan(
            &old,
            &new,
            &[dep("index", "ix", "CREATE INDEX ix ON t (a)")],
        );
        assert!(p.statements.last().unwrap().is_empty());
    }

    #[test]
    fn a_trigger_follows_the_renamed_table() {
        let old = parse_table("CREATE TABLE t (a TEXT)").unwrap();
        let mut new = old.clone();
        new.name = "t2".into();
        new.columns[0].ty = "INTEGER".into();
        let aux = vec![dep(
            "trigger",
            "tr",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES ('t'); END",
        )];
        let p = plan(&old, &new, &aux);
        let tr = p.statements.last().unwrap();
        assert!(tr.contains("ON \"t2\""), "{tr}");
        // The literal 't' is data and must be left exactly as it was.
        assert!(tr.contains("VALUES ('t')"), "{tr}");
    }

    /// A view over the table has to come down before the swap: SQLite validates
    /// every object in the schema during the rename, and one that points at the
    /// dropped table fails it.
    #[test]
    fn a_view_is_dropped_before_the_swap_and_recreated_after() {
        let old = parse_table("CREATE TABLE t (a TEXT)").unwrap();
        let mut new = old.clone();
        new.columns[0].ty = "INTEGER".into();
        let p = plan(
            &old,
            &new,
            &[dep("view", "v", "CREATE VIEW v AS SELECT a FROM t")],
        );
        let at = |needle: &str| {
            p.statements
                .iter()
                .position(|s| s.contains(needle))
                .unwrap_or_else(|| panic!("no statement contains {needle}: {:?}", p.statements))
        };
        assert!(at("DROP VIEW") < at("DROP TABLE"), "{:?}", p.statements);
        assert!(at("DROP TABLE") < at("RENAME TO"), "{:?}", p.statements);
        assert!(at("RENAME TO") < at("CREATE VIEW"), "{:?}", p.statements);
    }

    #[test]
    fn parses_and_emits_an_index() {
        let i = parse_index(
            "CREATE UNIQUE INDEX ix ON t (a COLLATE NOCASE, lower(b) DESC) WHERE a IS NOT NULL",
        )
        .unwrap();
        assert!(i.unique);
        assert_eq!(i.table, "t");
        assert_eq!(i.columns[0].expr, "a");
        assert_eq!(i.columns[0].collate, "NOCASE");
        assert_eq!(i.columns[1].expr, "lower(b)");
        assert!(i.columns[1].desc);
        assert_eq!(i.where_clause, "a IS NOT NULL");
        assert_eq!(
            i.create_sql(),
            "CREATE UNIQUE INDEX \"ix\" ON \"t\" (\"a\" COLLATE NOCASE, lower(b) DESC) \
             WHERE a IS NOT NULL"
        );
    }

    #[test]
    fn validation_catches_what_sqlite_would_reject() {
        let mut d = parse_table("CREATE TABLE t (a TEXT)").unwrap();
        assert!(d.validate().is_ok());

        d.columns.push(ColumnDef::new("a", "TEXT"));
        assert!(d.validate().unwrap_err().contains("duplicate"));
        d.columns.pop();

        d.without_rowid = true;
        assert!(d.validate().unwrap_err().contains("primary key"));
        d.without_rowid = false;

        d.columns[0].pk = true;
        d.columns[0].autoincrement = true;
        assert!(d.validate().unwrap_err().contains("INTEGER"));
    }

    #[test]
    fn quotes_survive_a_round_trip() {
        let d = parse_table("CREATE TABLE \"we\"\"ird\" (\"a\"\"b\" TEXT)").unwrap();
        assert_eq!(d.name, "we\"ird");
        assert_eq!(d.columns[0].name, "a\"b");
        assert_eq!(parse_table(&d.create_sql()).unwrap(), d);
    }

    #[test]
    fn drop_statements_cover_every_object_kind() {
        assert_eq!(drop_sql("table", "t").unwrap(), "DROP TABLE \"t\"");
        assert_eq!(drop_sql("index", "i").unwrap(), "DROP INDEX \"i\"");
        assert_eq!(drop_sql("view", "v").unwrap(), "DROP VIEW \"v\"");
        assert_eq!(drop_sql("trigger", "g").unwrap(), "DROP TRIGGER \"g\"");
        assert!(drop_sql("nonsense", "x").is_none());
    }

    /// Renaming a table means rewriting every trigger and view that names it, so
    /// the rewrite has to know an identifier from a word that merely looks like
    /// one. Getting this wrong rewrites someone's data or leaves a dangling
    /// reference behind.
    #[test]
    fn a_rename_touches_identifiers_and_nothing_else() {
        let go = |sql: &str| rename_ident(sql, "log", "journal");

        // Bare, quoted every way SQLite allows, and case-insensitively.
        assert_eq!(go("SELECT * FROM log"), r#"SELECT * FROM "journal""#);
        assert_eq!(go(r#"SELECT * FROM "log""#), r#"SELECT * FROM "journal""#);
        assert_eq!(go("SELECT * FROM `log`"), r#"SELECT * FROM "journal""#);
        assert_eq!(go("SELECT * FROM [log]"), r#"SELECT * FROM "journal""#);
        assert_eq!(go("SELECT * FROM LOG"), r#"SELECT * FROM "journal""#);

        // A longer word that merely contains it is a different name.
        assert_eq!(go("SELECT * FROM logs"), "SELECT * FROM logs");
        assert_eq!(go("SELECT * FROM backlog"), "SELECT * FROM backlog");
        assert_eq!(go("SELECT log_id FROM t"), "SELECT log_id FROM t");

        // A string literal is data: renaming a table must not edit rows.
        assert_eq!(
            go("SELECT 'log' FROM log WHERE note = 'see log'"),
            r#"SELECT 'log' FROM "journal" WHERE note = 'see log'"#
        );
        // Including one holding a doubled quote, which does not end it.
        assert_eq!(
            go("SELECT 'it''s log' FROM log"),
            r#"SELECT 'it''s log' FROM "journal""#
        );

        // Every occurrence, not just the first, and qualified names too.
        assert_eq!(
            go("SELECT * FROM log JOIN log AS l ON l.id = log.id"),
            r#"SELECT * FROM "journal" JOIN "journal" AS l ON l.id = "journal".id"#
        );
        // A name that is not there comes back untouched, byte for byte.
        let untouched = "CREATE VIEW v AS SELECT * FROM other";
        assert_eq!(rename_ident(untouched, "log", "journal"), untouched);
    }

    /// A view depends on a table when it names it — SQLite records no dependency,
    /// so this scan is how a rebuild knows which views to drop and recreate.
    #[test]
    fn a_dependency_is_a_name_used_as_a_name() {
        assert!(mentions_ident("CREATE VIEW v AS SELECT * FROM log", "log"));
        assert!(mentions_ident(r#"SELECT * FROM "log""#, "log"));
        assert!(
            mentions_ident("SELECT * FROM LOG", "log"),
            "case-insensitively"
        );

        assert!(!mentions_ident("SELECT * FROM logs", "log"), "not a prefix");
        assert!(
            !mentions_ident("SELECT * FROM backlog", "log"),
            "not a suffix"
        );
        assert!(
            !mentions_ident("SELECT 'log' FROM t", "log"),
            "a string literal is not a dependency"
        );
        assert!(!mentions_ident("", "log"));
    }

    /// Quoting is how a name holding a space, a keyword or a quote survives, and
    /// unquoting has to undo exactly what quoting did.
    #[test]
    fn quoting_and_unquoting_are_inverses() {
        for name in [
            "plain",
            "odd name",
            "select",
            "with\"quote",
            "with'apostrophe",
            "with`tick",
            "with]bracket",
        ] {
            assert_eq!(unquote(&quote(name)), name, "{name:?} did not survive");
        }
        // The forms SQLite accepts all unquote to the same name.
        assert_eq!(unquote("\"log\""), "log");
        assert_eq!(unquote("`log`"), "log");
        assert_eq!(unquote("[log]"), "log");
        assert_eq!(unquote("log"), "log", "a bare name is already unquoted");
    }

    /// The designer edits one column at a time as text, so a definition has to
    /// survive being parsed apart and put back together — type, constraints and
    /// all — or an edit to one column quietly rewrites another's rules.
    #[test]
    fn a_column_definition_survives_being_parsed_and_written_again() {
        for def in [
            "id INTEGER PRIMARY KEY AUTOINCREMENT",
            "name TEXT NOT NULL",
            "score REAL DEFAULT 0.0",
            "note VARCHAR(255) COLLATE NOCASE",
            "made_at TEXT DEFAULT CURRENT_TIMESTAMP",
            "flag INTEGER NOT NULL DEFAULT 1 CHECK (flag IN (0, 1))",
            "total INTEGER GENERATED ALWAYS AS (a + b) VIRTUAL",
            "\"odd name\" TEXT UNIQUE",
        ] {
            let col = parse_column(def);
            let round = parse_column(&col.to_sql(false));
            assert_eq!(round.name, col.name, "{def}");
            assert_eq!(round.ty, col.ty, "the type survives: {def}");
            assert_eq!(round.not_null, col.not_null, "{def}");
            assert_eq!(round.unique, col.unique, "{def}");
            assert_eq!(round.default, col.default, "{def}");
            assert_eq!(round.check, col.check, "{def}");
            assert_eq!(round.collate, col.collate, "{def}");
            assert_eq!(round.generated, col.generated, "{def}");
        }

        // The pieces are read, not just carried: each is where it belongs.
        let col = parse_column("flag INTEGER NOT NULL DEFAULT 1 CHECK (flag IN (0, 1))");
        assert_eq!(col.name, "flag");
        assert_eq!(col.ty, "INTEGER");
        assert!(col.not_null);
        assert_eq!(col.default, "1");
        assert_eq!(col.check, "flag IN (0, 1)");
        // A quoted name is unquoted once, not carried with its quotes.
        assert_eq!(parse_column("\"odd name\" TEXT").name, "odd name");
        // Nothing at all is an empty column rather than a panic.
        assert_eq!(parse_column("").name, "");
    }

    /// A rebuild creates the new table under a temporary name and copies into
    /// it, so the statement has to be the definition under *that* name while
    /// everything else about it stays put — including how the primary key is
    /// written, which differs for one column and for several.
    #[test]
    fn a_definition_can_be_created_under_another_name() {
        let def = parse_table(
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, city TEXT)",
        )
        .expect("parses");
        assert_eq!(def.pk_columns().len(), 1);

        let temp = def.create_sql_as("people_zdbview_new");
        assert!(
            temp.starts_with("CREATE TABLE \"people_zdbview_new\""),
            "{temp}"
        );
        assert!(
            temp.contains("\"id\" INTEGER PRIMARY KEY"),
            "one key inlines: {temp}"
        );
        assert!(temp.contains("\"name\" TEXT NOT NULL"), "{temp}");
        assert_eq!(
            def.create_sql(),
            def.create_sql_as(&def.name),
            "the plain form is the same statement under its own name"
        );

        // Several key columns become a table constraint instead.
        let composite =
            parse_table("CREATE TABLE pair (a TEXT, b TEXT, c TEXT, PRIMARY KEY (a, b))")
                .expect("parses");
        assert_eq!(composite.pk_columns().len(), 2, "both columns are the key");
        let sql = composite.create_sql_as("pair_new");
        assert!(sql.contains("PRIMARY KEY (\"a\", \"b\")"), "{sql}");
        assert!(
            !sql.contains("\"a\" TEXT PRIMARY KEY"),
            "and neither column claims it alone: {sql}"
        );

        // The table's own tail options travel with it.
        let opts =
            parse_table("CREATE TABLE k (id TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID, STRICT")
                .expect("parses");
        let sql = opts.create_sql_as("k_new");
        assert!(sql.contains("WITHOUT ROWID"), "{sql}");
        assert!(sql.contains("STRICT"), "{sql}");
    }
}
