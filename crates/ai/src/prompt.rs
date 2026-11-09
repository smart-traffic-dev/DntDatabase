//! Builds the fill-in-the-middle prompt (spec §4): redact the document, refuse when
//! the cursor is in a secret, cut prefix / suffix, and prepend a schema header made of
//! SQL comments.
//!
//! All offsets are bytes; the editor's UTF-16 cursor is converted once on entry.

use dnt_core::meta::ColumnBrief;
use dnt_core::sql::{leading_blank_len, split, tokenize, Dialect, TokKind};
use dnt_query::{redact_spans, Utf16Map, REDACTED};

/// Bytes of editor text before the cursor.
pub const PREFIX_MAX: usize = 6 * 1024;
/// Bytes of editor text after the cursor.
pub const SUFFIX_MAX: usize = 2 * 1024;
/// Bytes of the schema header.
pub const HEADER_MAX: usize = 3 * 1024;
/// Names listed under `Other tables` (and `Other collections`).
pub(crate) const OTHER_TABLES_MAX: usize = 200;
/// Width of an `Other tables` line.
const LINE_WIDTH: usize = 100;

/// A table of the connection's schema cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableBrief {
    pub schema: Option<String>,
    pub name: String,
    pub columns: Vec<ColumnBrief>,
}

#[derive(Debug, Clone, Copy)]
pub struct PromptInput<'a> {
    pub dialect: Dialect,
    /// e.g. `PostgreSQL 16.4`; `None` names the dialect only.
    pub server_version: Option<&'a str>,
    pub database: Option<&'a str>,
    pub schema: Option<&'a str>,
    /// The whole editor document.
    pub text: &'a str,
    /// Monaco offset (UTF-16 code units).
    pub cursor_utf16: u32,
    /// The schema cache of `database`.
    pub tables: &'a [TableBrief],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FimPrompt {
    /// Header + redacted text before the cursor.
    pub prompt: String,
    /// Redacted text after the cursor.
    pub suffix: String,
    /// Redacted text of the current statement before the cursor (for [`crate::post::clean`]);
    /// empty when the cursor is between statements.
    pub statement_head: String,
}

impl FimPrompt {
    /// The last ≤ 256 bytes of the prompt, for the repetition check of [`crate::post::clean`].
    pub fn prefix_tail(&self) -> &str {
        let from = ceil_boundary(&self.prompt, self.prompt.len().saturating_sub(256));
        &self.prompt[from..]
    }
}

/// Why no prompt was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// The cursor is inside, or right after, a literal that redaction hides.
    InSecret,
    /// The text holds a secret the console language's redaction hides (a Command
    /// language reports only that something was hidden, not where).
    Secret,
}

pub fn build(input: &PromptInput) -> Result<FimPrompt, Skip> {
    let cursor = Utf16Map::new(input.text).to_byte(input.cursor_utf16);
    let (doc, cursor) = redact_document(input.text, input.dialect, cursor)?;

    let stmts = split(&doc, input.dialect);
    // The statement the cursor is in: the last one starting at or before it, when only
    // whitespace and comments lie between its end and the cursor (no delimiter yet).
    let current = stmts.iter().rev().find(|s| s.start <= cursor).filter(|s| {
        let gap = &doc[s.end.min(cursor)..cursor];
        leading_blank_len(gap, input.dialect) == gap.len()
    });
    let statement_head = current
        .map(|s| doc[s.start..cursor].to_string())
        .unwrap_or_default();
    let statement_text = current
        .map(|s| &doc[s.start..s.end.max(cursor)])
        .unwrap_or("");

    let prefix_from = prefix_start(&doc, cursor, stmts.iter().map(|s| s.start));
    let prefix = &doc[prefix_from..cursor];
    let suffix = cut_suffix(&doc[cursor..]);

    let header = header(input, statement_text);
    Ok(FimPrompt {
        prompt: header + prefix,
        suffix: suffix.to_string(),
        statement_head,
    })
}

/// `text` with every secret replaced by [`REDACTED`], and the cursor moved with it.
/// Every statement and every stretch between statements is redacted on its own, so a
/// secret in a comment outside any statement is hidden too.
fn redact_document(text: &str, dialect: Dialect, cursor: usize) -> Result<(String, usize), Skip> {
    let mut bounds = vec![0, text.len()];
    for s in split(text, dialect) {
        bounds.push(s.start);
        bounds.push(s.end);
    }
    bounds.sort_unstable();
    bounds.dedup();
    let mut spans = Vec::new();
    for w in bounds.windows(2) {
        let (from, to) = (w[0], w[1]);
        spans.extend(
            redact_spans(&text[from..to], dialect)
                .into_iter()
                .map(|(a, b)| (from + a, from + b)),
        );
    }
    if spans.iter().any(|&(a, b)| a < cursor && cursor <= b) {
        return Err(Skip::InSecret);
    }
    let mut out = String::with_capacity(text.len());
    let (mut at, mut moved) = (0, cursor);
    for (a, b) in spans {
        out.push_str(&text[at..a]);
        out.push_str(REDACTED);
        at = b;
        if b <= cursor {
            moved = moved + REDACTED.len() - (b - a);
        }
    }
    out.push_str(&text[at..]);
    Ok((out, moved))
}

/// Where the prefix starts: the whole text before the cursor when it fits, else the
/// earliest statement start within the last [`PREFIX_MAX`] bytes, else a line start
/// inside the current (long) statement.
pub(crate) fn prefix_start(doc: &str, cursor: usize, starts: impl Iterator<Item = usize>) -> usize {
    if cursor <= PREFIX_MAX {
        return 0;
    }
    let lower = cursor - PREFIX_MAX;
    if let Some(start) = starts.filter(|&s| s >= lower && s <= cursor).min() {
        return start;
    }
    let from = ceil_boundary(doc, lower);
    match doc[from..cursor].find('\n') {
        Some(nl) => from + nl + 1,
        None => from,
    }
}

/// At most [`SUFFIX_MAX`] bytes; when cut, cut back to the last line break.
pub(crate) fn cut_suffix(rest: &str) -> &str {
    if rest.len() <= SUFFIX_MAX {
        return rest;
    }
    let cut = &rest[..floor_boundary(rest, SUFFIX_MAX)];
    match cut.rfind('\n') {
        Some(nl) if nl > 0 => &cut[..nl],
        _ => cut,
    }
}

/// The SQL-comment header: dialect, database, the tables of `statement` with their
/// columns, then other table names; at most [`HEADER_MAX`] bytes.
fn header(input: &PromptInput, statement: &str) -> String {
    let mut out = String::new();
    let dialect = match input.dialect {
        Dialect::MySql(_) => "MySQL",
        Dialect::Postgres => "PostgreSQL",
    };
    out.push_str(&format!(
        "-- Dialect: {}\n",
        clean(input.server_version.unwrap_or(dialect))
    ));
    if let Some(db) = input.database {
        out.push_str(&format!("-- Database: {}", clean(db)));
        if let Some(schema) = input.schema {
            out.push_str(&format!(", schema: {}", clean(schema)));
        }
        out.push('\n');
    }

    let referenced = referenced_tables(input, statement);
    let mut listed = vec![false; input.tables.len()];
    if !referenced.is_empty() && out.len() + "-- Tables:\n".len() <= HEADER_MAX {
        out.push_str("-- Tables:\n");
        for i in referenced {
            match table_line(input, &input.tables[i], HEADER_MAX - out.len()) {
                Some(line) => {
                    out.push_str(&line);
                    listed[i] = true;
                }
                None => break,
            }
        }
    }

    let others: Vec<String> = input
        .tables
        .iter()
        .zip(&listed)
        .filter(|(_, &l)| !l)
        .map(|(t, _)| display_name(input, t))
        .collect();
    push_other_names(&mut out, "-- Other tables: ", "--   ", others);
    out
}

/// Appends `names` (sorted case-insensitively, at most [`OTHER_TABLES_MAX`]) as lines of
/// at most [`LINE_WIDTH`] bytes, the first starting with `first`, the next ones with
/// `more`, while `out` stays within [`HEADER_MAX`] bytes.
pub(crate) fn push_other_names(out: &mut String, first: &str, more: &str, mut names: Vec<String>) {
    names.sort_by_key(|n| n.to_lowercase());
    names.truncate(OTHER_TABLES_MAX);
    let mut line = String::new();
    for name in names {
        let piece = if line.is_empty() {
            format!("{first}{name}")
        } else if line.len() + 2 + name.len() > LINE_WIDTH {
            out.push_str(&line);
            out.push('\n');
            // Written: a `break` below must not write it again.
            line.clear();
            format!("{more}{name}")
        } else {
            format!("{line}, {name}")
        };
        if out.len() + piece.len() + 1 > HEADER_MAX {
            break;
        }
        line = piece;
    }
    if !line.is_empty() {
        out.push_str(&line);
        out.push('\n');
    }
}

/// `--   name(col type, …)\n` within `room` bytes, dropping trailing columns (written
/// as `…`) when needed; `None` when not even `--   name(…)\n` fits.
fn table_line(input: &PromptInput, t: &TableBrief, room: usize) -> Option<String> {
    let cols: Vec<String> = t
        .columns
        .iter()
        .map(|c| format!("{} {}", clean(&c.name), clean(&c.data_type)))
        .collect();
    member_line("--   ", &display_name(input, t), &cols, room)
}

/// `{lead}name(part, …)\n` within `room` bytes, dropping trailing parts (written as `…`)
/// when needed; `None` when not even `{lead}name(…)\n` fits.
pub(crate) fn member_line(lead: &str, name: &str, parts: &[String], room: usize) -> Option<String> {
    let full = format!("{lead}{name}({})\n", parts.join(", "));
    if full.len() <= room {
        return Some(full);
    }
    for keep in (0..parts.len()).rev() {
        let mut shown = parts[..keep].to_vec();
        shown.push("…".to_string());
        let line = format!("{lead}{name}({})\n", shown.join(", "));
        if line.len() <= room {
            return Some(line);
        }
    }
    None
}

/// `schema.name` when the table is in another schema than the editor's, else `name`.
fn display_name(input: &PromptInput, t: &TableBrief) -> String {
    match (&t.schema, input.schema) {
        (Some(s), cur) if Some(s.as_str()) != cur => format!("{}.{}", clean(s), clean(&t.name)),
        _ => clean(&t.name),
    }
}

/// Indexes into `input.tables` of the tables `statement` names, in order of first
/// reference. `name` matches case-insensitively; `schema.name` must match both parts.
/// When an unqualified name exists in several schemas, the editor's schema wins.
fn referenced_tables(input: &PromptInput, statement: &str) -> Vec<usize> {
    let toks = tokenize(statement, input.dialect);
    let ident = |i: usize| -> Option<String> {
        let t = toks.get(i)?;
        match t.kind {
            TokKind::Word(w) => Some(w.to_string()),
            TokKind::Quoted => unquote(&statement[t.start..t.end]),
            _ => None,
        }
    };
    let mut out: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let Some(first) = ident(i) else {
            i += 1;
            continue;
        };
        let dotted = toks.get(i + 1).map(|t| t.kind) == Some(TokKind::Punct(b'.'));
        let (schema, name, step) = match (dotted, ident(i + 2)) {
            (true, Some(second)) => (Some(first), second, 3),
            _ => (None, first, 1),
        };
        let mut hits: Vec<usize> = input
            .tables
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.name.eq_ignore_ascii_case(&name)
                    && schema.as_ref().is_none_or(|s| {
                        t.schema
                            .as_deref()
                            .is_some_and(|ts| ts.eq_ignore_ascii_case(s))
                    })
            })
            .map(|(k, _)| k)
            .collect();
        if schema.is_none() && hits.len() > 1 {
            let in_current: Vec<usize> = hits
                .iter()
                .copied()
                .filter(|&k| input.tables[k].schema.as_deref() == input.schema)
                .collect();
            if !in_current.is_empty() {
                hits = in_current;
            }
        }
        for k in hits {
            if !out.contains(&k) {
                out.push(k);
            }
        }
        i += step;
    }
    out
}

/// The name inside a `"…"`, `` `…` `` or `[…]` identifier; `None` for string literals.
fn unquote(tok: &str) -> Option<String> {
    let (open, close) = match tok.chars().next()? {
        '"' => ('"', '"'),
        '`' => ('`', '`'),
        '[' => ('[', ']'),
        _ => return None,
    };
    let inner = tok.strip_prefix(open)?.strip_suffix(close)?;
    let doubled = format!("{close}{close}");
    Some(inner.replace(&doubled, &close.to_string()))
}

/// Control characters (line breaks included) become spaces, so a name never leaves
/// its comment line.
pub(crate) fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: Dialect = Dialect::Postgres;
    const MY: Dialect = Dialect::MYSQL;

    fn col(name: &str, ty: &str) -> ColumnBrief {
        ColumnBrief {
            name: name.into(),
            data_type: ty.into(),
        }
    }

    fn table(schema: Option<&str>, name: &str, cols: &[(&str, &str)]) -> TableBrief {
        TableBrief {
            schema: schema.map(str::to_string),
            name: name.into(),
            columns: cols.iter().map(|(n, t)| col(n, t)).collect(),
        }
    }

    fn u16_len(s: &str) -> u32 {
        s.encode_utf16().count() as u32
    }

    /// `text` with the cursor at `|` (removed).
    fn input<'a>(
        dialect: Dialect,
        text: &'a str,
        cursor: u32,
        tables: &'a [TableBrief],
    ) -> PromptInput<'a> {
        PromptInput {
            dialect,
            server_version: None,
            database: Some("shop"),
            schema: if dialect == PG { Some("public") } else { None },
            text,
            cursor_utf16: cursor,
            tables,
        }
    }

    fn build_at(dialect: Dialect, marked: &str, tables: &[TableBrief]) -> Result<FimPrompt, Skip> {
        let at = marked.find('|').expect("cursor marker");
        let text = marked.replacen('|', "", 1);
        let cursor = u16_len(&text[..at]);
        build(&input(dialect, &text, cursor, tables))
    }

    /// The prompt without its header.
    fn body(p: &FimPrompt) -> &str {
        let header_end = p
            .prompt
            .lines()
            .take_while(|l| l.starts_with("-- "))
            .map(|l| l.len() + 1)
            .sum::<usize>();
        &p.prompt[header_end..]
    }

    #[test]
    fn cursor_in_secret_skips() {
        assert_eq!(
            build_at(MY, "CREATE USER u IDENTIFIED BY 'hunt|", &[]),
            Err(Skip::InSecret)
        );
        assert_eq!(
            build_at(PG, "ALTER ROLE r PASSWORD 'hu|nter2';", &[]),
            Err(Skip::InSecret)
        );
    }

    #[test]
    fn cursor_right_after_secret_skips() {
        assert_eq!(
            build_at(MY, "CREATE USER u IDENTIFIED BY 'hunter2'|", &[]),
            Err(Skip::InSecret)
        );
        // Right before the literal is fine.
        assert!(build_at(MY, "CREATE USER u IDENTIFIED BY |'hunter2'", &[]).is_ok());
    }

    #[test]
    fn prompt_never_contains_secrets() {
        let marked = "ALTER USER a IDENTIFIED BY 'hunter2';\n\
                      -- password='comment-secret'\n\
                      SELECT * FROM t WHERE |;\n\
                      CREATE USER b IDENTIFIED BY 's3cret-after';";
        let p = build_at(MY, marked, &[]).unwrap();
        for secret in ["hunter2", "comment-secret", "s3cret-after"] {
            assert!(
                !p.prompt.contains(secret),
                "{secret} in prompt: {}",
                p.prompt
            );
            assert!(
                !p.suffix.contains(secret),
                "{secret} in suffix: {}",
                p.suffix
            );
        }
        assert!(p.prompt.contains(REDACTED));
        assert!(p.suffix.contains(REDACTED));
        assert!(body(&p).ends_with("SELECT * FROM t WHERE "));
        assert!(p.suffix.starts_with(";\nCREATE USER b"));
    }

    #[test]
    fn utf16_cursor_maps_to_bytes() {
        let p = build_at(PG, "SELECT 'Tiếng 😀' AS x, |y FROM t", &[]).unwrap();
        assert!(body(&p).ends_with("AS x, "), "{}", p.prompt);
        assert_eq!(p.suffix, "y FROM t");
    }

    #[test]
    fn prefix_cut_at_statement_start() {
        let filler: String = (0..1000).map(|i| format!("SELECT {i};\n")).collect();
        let marked = format!("{filler}SELECT * FROM t WHERE |");
        let p = build_at(PG, &marked, &[]).unwrap();
        let b = body(&p);
        assert!(b.len() <= PREFIX_MAX, "{}", b.len());
        assert!(b.starts_with("SELECT "), "{:?}", &b[..20]);
        assert!(b.ends_with("SELECT * FROM t WHERE "));
    }

    #[test]
    fn prefix_cut_long_statement_at_line() {
        let cols: String = (0..1000).map(|i| format!("  col_{i},\n")).collect();
        let marked = format!("SELECT\n{cols}  |");
        let p = build_at(PG, &marked, &[]).unwrap();
        let b = body(&p);
        assert!(b.len() <= PREFIX_MAX, "{}", b.len());
        assert!(b.starts_with("  col_"), "{:?}", &b[..20]);
    }

    #[test]
    fn suffix_cut_at_line() {
        let tail: String = (0..500).map(|i| format!("SELECT {i};\n")).collect();
        let marked = format!("SELECT |;\n{tail}");
        let p = build_at(PG, &marked, &[]).unwrap();
        assert!(p.suffix.len() <= SUFFIX_MAX);
        assert!(
            p.suffix.ends_with(';'),
            "{:?}",
            &p.suffix[p.suffix.len() - 10..]
        );
    }

    #[test]
    fn header_lists_referenced_tables_first() {
        let tables = [
            table(Some("public"), "audit", &[("id", "bigint")]),
            table(
                Some("public"),
                "orders",
                &[("id", "bigint"), ("customer_id", "bigint")],
            ),
            table(
                Some("public"),
                "Customers",
                &[("id", "bigint"), ("email", "text")],
            ),
        ];
        let p = build_at(
            PG,
            "SELECT * FROM orders o JOIN \"Customers\" c ON c.id = o.customer_id WHERE |",
            &tables,
        )
        .unwrap();
        let want = "-- Dialect: PostgreSQL\n\
                    -- Database: shop, schema: public\n\
                    -- Tables:\n\
                    --   orders(id bigint, customer_id bigint)\n\
                    --   Customers(id bigint, email text)\n\
                    -- Other tables: audit\n";
        assert!(p.prompt.starts_with(want), "{}", p.prompt);
    }

    #[test]
    fn header_uses_the_whole_statement() {
        // The FROM clause after the cursor still names the table.
        let tables = [table(Some("public"), "orders", &[("id", "bigint")])];
        let p = build_at(PG, "SELECT |\nFROM orders", &tables).unwrap();
        assert!(
            p.prompt.contains("--   orders(id bigint)\n"),
            "{}",
            p.prompt
        );
    }

    #[test]
    fn header_schema_qualified_match() {
        let tables = [
            table(Some("public"), "orders", &[("id", "bigint")]),
            table(Some("sales"), "orders", &[("sale_id", "int")]),
        ];
        let p = build_at(PG, "SELECT * FROM sales.orders WHERE |", &tables).unwrap();
        assert!(
            p.prompt.contains("--   sales.orders(sale_id int)\n"),
            "{}",
            p.prompt
        );
        assert!(
            p.prompt.contains("-- Other tables: orders\n"),
            "{}",
            p.prompt
        );

        // Unqualified: the editor's schema wins.
        let p = build_at(PG, "SELECT * FROM orders WHERE |", &tables).unwrap();
        assert!(
            p.prompt.contains("--   orders(id bigint)\n"),
            "{}",
            p.prompt
        );
        assert!(!p.prompt.contains("sale_id"), "{}", p.prompt);
    }

    #[test]
    fn header_respects_budget() {
        let cols: Vec<(String, String)> = (0..50)
            .map(|i| (format!("column_{i}"), "varchar(255)".to_string()))
            .collect();
        let cols_ref: Vec<(&str, &str)> =
            cols.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let tables: Vec<TableBrief> = (0..500)
            .map(|i| table(Some("public"), &format!("table_{i}"), &cols_ref))
            .collect();
        let p = build_at(
            PG,
            "SELECT * FROM table_1 JOIN table_2 USING (column_0) JOIN table_3 USING (column_0) WHERE |",
            &tables,
        )
        .unwrap();
        let header_len = p.prompt.len() - body(&p).len();
        assert!(header_len <= HEADER_MAX, "{header_len}");
        assert!(p.prompt[..header_len].ends_with('\n'));
        assert!(
            p.prompt.contains("--   table_1("),
            "{}",
            &p.prompt[..header_len]
        );
        assert!(
            p.prompt.contains("…)"),
            "a long table is cut: {}",
            &p.prompt[..header_len]
        );
        for line in p.prompt[..header_len].lines() {
            assert!(line.starts_with("-- "), "{line:?}");
        }
    }

    #[test]
    fn header_sanitizes_names() {
        let tables = [table(None, "t", &[("a\n--x", "int\r\nDROP")])];
        let p = build_at(MY, "SELECT * FROM t WHERE |", &tables).unwrap();
        let header_len = p.prompt.len() - body(&p).len();
        for line in p.prompt[..header_len].lines() {
            assert!(line.starts_with("-- "), "{line:?}");
        }
        assert!(
            p.prompt.contains("--   t(a --x int  DROP)\n"),
            "{}",
            p.prompt
        );
    }

    #[test]
    fn no_database_header_has_dialect_only() {
        let text = "SELECT ";
        let p = build(&PromptInput {
            dialect: MY,
            server_version: Some("MySQL 8.0.36"),
            database: None,
            schema: None,
            text,
            cursor_utf16: u16_len(text),
            tables: &[],
        })
        .unwrap();
        assert_eq!(p.prompt, "-- Dialect: MySQL 8.0.36\nSELECT ");
    }

    #[test]
    fn statement_head_is_current_statement_prefix() {
        let p = build_at(PG, "SELECT 1;\nSELECT a, |b FROM t;", &[]).unwrap();
        assert_eq!(p.statement_head, "SELECT a, ");
        // Trailing spaces after the statement text are still that statement.
        let p = build_at(PG, "SELECT 1;\nSELECT * FROM |", &[]).unwrap();
        assert_eq!(p.statement_head, "SELECT * FROM ");
        // After a delimiter: between statements.
        let p = build_at(PG, "SELECT 1;\n|", &[]).unwrap();
        assert_eq!(p.statement_head, "");
    }

    #[test]
    fn prefix_tail_is_bounded() {
        let p = build_at(PG, &format!("SELECT '{}', |", "é".repeat(400)), &[]).unwrap();
        assert!(p.prefix_tail().len() <= 256);
        assert!(p.prompt.ends_with(p.prefix_tail()));
    }
}
