//! File writers of the result export (CSV, JSON, Excel, SQL `INSERT`, Markdown) and the
//! [`RowSink`] that streams a result into one.
//!
//! Every format writes rows as they arrive (Excel through `rust_xlsxwriter`'s
//! constant-memory mode, which keeps one row in memory and the rest in a temp file). The
//! common value rules:
//!
//! - NULL: an empty unquoted CSV field (an empty string is always `""`), JSON `null`, an
//!   empty Excel cell, SQL `NULL`, Markdown `NULL`.
//! - Binary: hex, `0x…` in text formats; `X'…'` (MySQL) / `E'\\x…'::bytea` (PostgreSQL) in
//!   SQL. A binary value the UI only holds a preview of (the loaded rows: its first 64
//!   bytes) is never written as if it were whole: it becomes `NULL` (SQL, JSON) or an
//!   empty cell (CSV, Excel, Markdown) and is counted ([`Written::partial_binaries`]).
//! - Column names repeated in a result (`SELECT a.id, b.id`) get a suffix (`id`, `id_2`)
//!   in headers and JSON keys; the SQL format refuses them (an `INSERT` cannot name a
//!   column twice).
//! - JSON integers beyond ±(2^53 − 1) are strings (JavaScript reads larger ones wrongly).
//! - SQL files import the same whatever the server's `sql_mode` /
//!   `standard_conforming_strings`: MySQL files start with `SET NAMES utf8mb4;` and write
//!   a string holding `\` or NUL as `_utf8mb4 X'…'`; PostgreSQL writes such a string as
//!   `E'…'` (backslashes doubled).
//! - Dates and times: ISO 8601 (`2024-01-31`, `2024-01-31T10:00:00`, a timestamp with
//!   time zone keeps its offset); SQL keeps the server's `2024-01-31 10:00:00`, which
//!   both engines read back; Excel gets real date cells when the value fits Excel's
//!   calendar (1900 onwards), else text.
//! - CSV follows RFC 4180: CRLF line ends, a field is quoted when it holds the
//!   delimiter, a quote, CR or LF, or is empty (or always with "quote all"), quotes
//!   doubled. With "protect against formula injection", a text cell starting with
//!   `=` `+` `-` `@`, a tab or CR gets a leading `'` (spreadsheets then show it as text).

use crate::commands::transfer::path_error;
use crate::result_store::{block_outside_executor, value_bytes};
use dnt_core::driver::{ResultSummary, RowSink};
use dnt_core::error::DbError;
use dnt_core::server::SqlKind;
use dnt_core::sql::ident::quote_ident;
use dnt_core::value::{ColumnMeta, Value};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Csv,
    Json,
    Xlsx,
    Sql,
    Markdown,
}

impl ExportFormat {
    /// The file extension (no dot).
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::Xlsx => "xlsx",
            ExportFormat::Sql => "sql",
            ExportFormat::Markdown => "md",
        }
    }

    /// The save dialog's filter name.
    pub fn filter_name(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Json => "JSON",
            ExportFormat::Xlsx => "Excel workbook",
            ExportFormat::Sql => "SQL",
            ExportFormat::Markdown => "Markdown",
        }
    }
}

/// What the export dialog chose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// CSV: the field delimiter, one character (`,` `;` tab `|`…).
    pub delimiter: String,
    /// CSV: a first row with the column names.
    pub header: bool,
    /// CSV: quote every non-NULL field.
    pub quote_all: bool,
    /// CSV: start with a UTF-8 byte order mark (Excel reads the file as UTF-8 then).
    pub bom: bool,
    /// SQL: the table the `INSERT`s go into (one identifier, quoted per engine).
    pub table_name: String,
    /// CSV: prefix `'` to text cells a spreadsheet would read as a formula.
    #[serde(default)]
    pub formula_guard: bool,
}

/// The file written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ExportSummary {
    #[specta(type = f64)]
    pub rows: u64,
    pub path: String,
    /// Excel: text cells cut to Excel's 32,767 characters.
    pub cut_cells: u32,
    /// Binary values only partly loaded, written as NULL / empty (see the module docs).
    pub partial_binaries: u32,
    #[specta(type = f64)]
    pub elapsed_ms: u64,
}

/// What a writer wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Written {
    pub rows: u64,
    pub cut_cells: u32,
    pub partial_binaries: u32,
}

/// Progress of an export streaming to its file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ExportProgress {
    #[specta(type = f64)]
    pub rows: u64,
}

/// Excel's limits.
pub const XLSX_MAX_ROWS: u64 = 1_048_576;
pub const XLSX_MAX_COLS: usize = 16_384;
pub const XLSX_MAX_CHARS: usize = 32_767;

fn export_error(message: impl Into<String>) -> DbError {
    DbError::Config {
        field: Some("export".into()),
        message: message.into(),
    }
}

/// The checked delimiter of `opts` (CSV).
fn delimiter(opts: &ExportOptions) -> Result<char, DbError> {
    let mut chars = opts.delimiter.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !matches!(c, '"' | '\r' | '\n') => Ok(c),
        _ => Err(export_error(
            "The delimiter must be one character other than a quote or a line break.",
        )),
    }
}

/// Checks `opts` before anything is read or written.
pub fn check_options(opts: &ExportOptions) -> Result<(), DbError> {
    match opts.format {
        ExportFormat::Csv => delimiter(opts).map(|_| ()),
        ExportFormat::Sql if opts.table_name.trim().is_empty() => {
            Err(export_error("The SQL export needs a table name."))
        }
        _ => Ok(()),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// `YYYY-MM-DD HH:MM:SS…` with a `T` between date and time; anything else unchanged.
pub fn iso_datetime(s: &str) -> String {
    let b = s.as_bytes();
    let looks_like_date = b.len() >= 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b' '
        && b[..4].iter().all(u8::is_ascii_digit)
        && !s.ends_with(" BC");
    if looks_like_date {
        format!("{}T{}", &s[..10], &s[11..])
    } else {
        s.to_string()
    }
}

fn float_text(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        f.to_string()
    }
}

/// A binary value of which only a preview is held (the loaded rows).
pub fn is_partial(v: &Value) -> bool {
    matches!(v, Value::Bytes { len, preview } if *len > preview.len() as u64)
}

/// The partly loaded binary values of `row` (array items included).
pub fn count_partial(row: &[Value]) -> u32 {
    row.iter()
        .map(|v| match v {
            Value::Array(items) => count_partial(items),
            v => u32::from(is_partial(v)),
        })
        .fold(0u32, u32::saturating_add)
}

/// A PostgreSQL array literal: `{1,2,"a b",NULL}`; a `bytea` item is `"\\x…"` (its
/// text form, as PostgreSQL prints it).
fn array_literal(items: &[Value]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|v| match v {
            Value::Null => "NULL".into(),
            v if is_partial(v) => "NULL".into(),
            Value::Array(inner) => array_literal(inner),
            other => {
                let t = match other {
                    Value::Bytes { preview, .. } => format!("\\x{}", hex(preview)),
                    other => cell_text(other).unwrap_or_default(),
                };
                let plain = !t.is_empty()
                    && !t.eq_ignore_ascii_case("null")
                    && !t
                        .chars()
                        .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\') || c.is_whitespace());
                if plain {
                    t
                } else {
                    format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\""))
                }
            }
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

/// The text of a cell in text formats; `None` for NULL; empty for a partly loaded
/// binary value.
pub fn cell_text(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => return None,
        v if is_partial(v) => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::Float(f) => float_text(*f),
        Value::Decimal(s) | Value::Text(s) | Value::Json(s) | Value::Uuid(s) => s.clone(),
        Value::Bytes { preview, .. } => format!("0x{}", hex(preview)),
        Value::Date(s) | Value::Time(s) => s.clone(),
        Value::DateTime(s) | Value::TimestampTz(s) => iso_datetime(s),
        Value::Array(items) => array_literal(items),
        Value::Other { text, .. } => text.clone(),
    })
}

/// One output format.
pub trait FormatWriter: Send {
    /// The columns, before any row (called once).
    fn begin(&mut self, cols: &[ColumnMeta]) -> Result<(), DbError>;
    fn row(&mut self, row: &[Value]) -> Result<(), DbError>;
    /// Completes the file (flush, closing bracket, Excel save).
    fn finish(&mut self) -> Result<(), DbError>;
    /// Cells cut to fit the format (Excel).
    fn cut_cells(&self) -> u32 {
        0
    }
}

/// A buffered file with the path for error messages.
struct Out {
    w: BufWriter<File>,
    path: String,
}

/// Creates the temp file `path` (`<file>.dntpart`) for writing. Only a NEW file: an
/// existing one (a stale temp file, another export writing the same file) or anything
/// planted there, a symbolic link included (`create_new` never follows one), is refused
/// and left as it is.
fn create_part(path: &Path, shown: &str) -> Result<File, DbError> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                let name = path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy();
                export_error(format!(
                    "The temporary file {name} already exists next to the file (another \
                     export may be writing it). Remove it, or choose another file."
                ))
            } else {
                path_error("write", shown, &e)
            }
        })
}

impl Out {
    fn create(path: &Path, shown: &str) -> Result<Out, DbError> {
        let f = create_part(path, shown)?;
        Ok(Out {
            w: BufWriter::with_capacity(256 * 1024, f),
            path: shown.to_string(),
        })
    }
    fn put(&mut self, s: &str) -> Result<(), DbError> {
        self.w
            .write_all(s.as_bytes())
            .map_err(|e| path_error("write", &self.path, &e))
    }
    fn flush(&mut self) -> Result<(), DbError> {
        self.w
            .flush()
            .map_err(|e| path_error("write", &self.path, &e))
    }
}

// ---- CSV ----

struct Csv {
    out: Out,
    /// A UTF-8 byte order mark first (written by `begin`).
    bom: bool,
    delim: char,
    header: bool,
    quote_all: bool,
    formula_guard: bool,
}

/// Whether a spreadsheet may read a cell starting like `s` as a formula.
fn formula_like(s: &str) -> bool {
    s.starts_with(['=', '+', '-', '@', '\t', '\r'])
}

impl Csv {
    /// A non-NULL field: quoted when needed; an empty string is always `""` (an empty
    /// unquoted field is NULL).
    fn field(&self, s: &str) -> String {
        let needs = self.quote_all
            || s.is_empty()
            || s.contains(self.delim)
            || s.contains('"')
            || s.contains('\r')
            || s.contains('\n');
        if needs {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    }

    /// `s` with a leading `'` when formula protection is on and `s` looks like one.
    fn guarded(&self, s: &str) -> String {
        if self.formula_guard && formula_like(s) {
            format!("'{s}")
        } else {
            s.to_string()
        }
    }

    fn line(&mut self, fields: Vec<String>) -> Result<(), DbError> {
        let mut line = fields.join(&self.delim.to_string());
        line.push_str("\r\n");
        self.out.put(&line)
    }
}

impl FormatWriter for Csv {
    fn begin(&mut self, cols: &[ColumnMeta]) -> Result<(), DbError> {
        if self.bom {
            self.out.put("\u{feff}")?;
        }
        if self.header {
            let names = unique_names(cols)
                .iter()
                .map(|n| self.field(&self.guarded(n)))
                .collect();
            self.line(names)?;
        }
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> Result<(), DbError> {
        let fields = row
            .iter()
            .map(|v| match (v, cell_text(v)) {
                // A partly loaded binary value is left empty, like NULL.
                (_, None) => String::new(),
                (v, Some(_)) if is_partial(v) => String::new(),
                // Numbers are never guarded (`-5` stays a number).
                (Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_), Some(t)) => {
                    self.field(&t)
                }
                (_, Some(t)) => self.field(&self.guarded(&t)),
            })
            .collect();
        self.line(fields)
    }
    fn finish(&mut self) -> Result<(), DbError> {
        self.out.flush()
    }
}

// ---- JSON ----

struct Json {
    out: Out,
    keys: Vec<String>,
    rows: u64,
}

/// Column names made unique (`a`, `a_2`, …) for JSON object keys.
pub fn unique_names(cols: &[ColumnMeta]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    cols.iter()
        .map(|c| {
            let mut name = c.name.clone();
            let mut n = 2;
            while !seen.insert(name.clone()) {
                name = format!("{}_{n}", c.name);
                n += 1;
            }
            name
        })
        .collect()
}

fn is_json_number(s: &str) -> bool {
    matches!(
        serde_json::from_str::<serde_json::Value>(s),
        Ok(serde_json::Value::Number(_))
    )
}

/// Largest integer JavaScript reads exactly (`Number.MAX_SAFE_INTEGER`, 2^53 − 1).
const JSON_MAX_SAFE: u64 = (1 << 53) - 1;

fn json_value(v: &Value) -> String {
    let string = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "null".into());
    match v {
        Value::Null => "null".into(),
        v if is_partial(v) => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) if n.unsigned_abs() <= JSON_MAX_SAFE => n.to_string(),
        Value::UInt(n) if *n <= JSON_MAX_SAFE => n.to_string(),
        Value::Int(n) => string(&n.to_string()),
        Value::UInt(n) => string(&n.to_string()),
        Value::Float(f) if f.is_finite() => {
            serde_json::to_string(f).unwrap_or_else(|_| string(&float_text(*f)))
        }
        Value::Float(f) => string(&float_text(*f)),
        Value::Decimal(s) if is_json_number(s) => s.clone(),
        Value::Json(s) if serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok() => {
            // Compact and valid; kept as the server returned it.
            s.trim().to_string()
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(json_value).collect();
            format!("[{}]", parts.join(","))
        }
        other => string(&cell_text(other).unwrap_or_default()),
    }
}

impl FormatWriter for Json {
    fn begin(&mut self, cols: &[ColumnMeta]) -> Result<(), DbError> {
        self.keys = unique_names(cols)
            .iter()
            .map(|k| serde_json::to_string(k).unwrap_or_default())
            .collect();
        self.out.put("[")
    }
    fn row(&mut self, row: &[Value]) -> Result<(), DbError> {
        let fields: Vec<String> = self
            .keys
            .iter()
            .zip(row)
            .map(|(k, v)| format!("{k}: {}", json_value(v)))
            .collect();
        let sep = if self.rows == 0 { "\n  " } else { ",\n  " };
        self.rows += 1;
        self.out.put(&format!("{sep}{{{}}}", fields.join(", ")))
    }
    fn finish(&mut self) -> Result<(), DbError> {
        if self.keys.is_empty() && self.rows == 0 {
            // No result at all: still a valid (empty) array.
            self.out.put("[")?;
        }
        self.out.put(if self.rows == 0 { "]\n" } else { "\n]\n" })?;
        self.out.flush()
    }
}

// ---- Markdown ----

struct Markdown {
    out: Out,
}

fn md_cell(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace("\r\n", "<br>")
        .replace(['\n', '\r'], "<br>")
}

impl FormatWriter for Markdown {
    fn begin(&mut self, cols: &[ColumnMeta]) -> Result<(), DbError> {
        let names: Vec<String> = unique_names(cols).iter().map(|n| md_cell(n)).collect();
        let rule: Vec<&str> = cols.iter().map(|_| "---").collect();
        self.out.put(&format!(
            "| {} |\n| {} |\n",
            names.join(" | "),
            rule.join(" | ")
        ))
    }
    fn row(&mut self, row: &[Value]) -> Result<(), DbError> {
        let cells: Vec<String> = row
            .iter()
            .map(|v| md_cell(&cell_text(v).unwrap_or_else(|| "NULL".into())))
            .collect();
        self.out.put(&format!("| {} |\n", cells.join(" | ")))
    }
    fn finish(&mut self) -> Result<(), DbError> {
        self.out.flush()
    }
}

// ---- SQL INSERT ----

struct Sql {
    out: Out,
    kind: SqlKind,
    table: String,
    columns: String,
}

/// A string literal for `kind` that reads back the same under any server setting:
///
/// - MySQL: quotes doubled; a string holding `\` or NUL (which `NO_BACKSLASH_ESCAPES`
///   reads differently from the default mode) is written as `_utf8mb4 X'…'`, its UTF-8
///   bytes in hex.
/// - PostgreSQL: quotes doubled; a string holding `\` is an escape string `E'…'` with
///   backslashes doubled (the same with `standard_conforming_strings` on or off).
pub fn sql_string(kind: SqlKind, s: &str) -> String {
    let quoted = || s.replace('\'', "''");
    match kind {
        SqlKind::MySql if s.contains(['\\', '\0']) => format!("_utf8mb4 X'{}'", hex(s.as_bytes())),
        SqlKind::Postgres if s.contains('\\') => {
            format!("E'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
        }
        SqlKind::MySql | SqlKind::Postgres => format!("'{}'", quoted()),
    }
}

/// Whether `s` is a plain SQL number: `-?(D+(.D*)?|.D+)([eE][+-]?D+)?`.
pub fn is_sql_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let start = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i - start
    };
    let int = digits(&mut i);
    let mut frac = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        frac = digits(&mut i);
    }
    if int == 0 && frac == 0 {
        return false;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == b.len()
}

/// Whether `items` (an array value) hold a binary value anywhere.
fn has_bytes(items: &[Value]) -> bool {
    items.iter().any(|v| match v {
        Value::Bytes { .. } => true,
        Value::Array(inner) => has_bytes(inner),
        _ => false,
    })
}

/// A PostgreSQL `ARRAY[…]` of `bytea` items (a `bytea[]` array literal would need
/// doubly escaped backslashes).
fn pg_bytea_array(items: &[Value]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|v| match v {
            Value::Array(inner) => pg_bytea_array(inner),
            v => sql_literal(SqlKind::Postgres, v),
        })
        .collect();
    format!("ARRAY[{}]", parts.join(", "))
}

/// A MySQL geometry value (its internal form in hex) as a binary literal.
fn mysql_geometry(text: &str) -> Option<String> {
    (!text.is_empty()
        && text.len().is_multiple_of(2)
        && text.bytes().all(|c| c.is_ascii_hexdigit()))
    .then(|| format!("X'{text}'"))
}

/// The SQL literal of a value for `kind`.
pub fn sql_literal(kind: SqlKind, v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        v if is_partial(v) => "NULL".into(),
        Value::Bool(true) => "TRUE".into(),
        Value::Bool(false) => "FALSE".into(),
        Value::Int(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Float(f) => sql_string(kind, &float_text(*f)),
        Value::Decimal(s) if is_sql_number(s) => s.clone(),
        Value::Bytes { preview, .. } => match kind {
            SqlKind::MySql => format!("X'{}'", hex(preview)),
            // An escape string, so `standard_conforming_strings = off` reads it the same.
            SqlKind::Postgres => format!("E'\\\\x{}'::bytea", hex(preview)),
        },
        Value::Array(items) if has_bytes(items) => match kind {
            SqlKind::MySql => text_literal(kind, v),
            SqlKind::Postgres => format!("{}::bytea[]", pg_bytea_array(items)),
        },
        Value::Other { type_name, text } if type_name == "geometry" => match kind {
            SqlKind::MySql => mysql_geometry(text).unwrap_or_else(|| sql_string(kind, text)),
            SqlKind::Postgres => text_literal(kind, v),
        },
        Value::DateTime(s) | Value::TimestampTz(s) => sql_string(kind, s),
        other => text_literal(kind, other),
    }
}

/// A value as the string literal of its cell text.
fn text_literal(kind: SqlKind, v: &Value) -> String {
    sql_string(kind, &cell_text(v).unwrap_or_default())
}

impl FormatWriter for Sql {
    fn begin(&mut self, cols: &[ColumnMeta]) -> Result<(), DbError> {
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = cols.iter().find(|c| !seen.insert(c.name.as_str())) {
            return Err(export_error(format!(
                "The result has two columns named \"{}\"; an INSERT cannot name a column \
                 twice. Give them different names (AS …) or export to another format.",
                dup.name
            )));
        }
        let names: Vec<String> = cols
            .iter()
            .map(|c| quote_ident(self.kind, &c.name))
            .collect();
        self.columns = names.join(", ");
        match self.kind {
            // The file's strings are UTF-8 whatever the importing client's charset.
            SqlKind::MySql => self.out.put("SET NAMES utf8mb4;\n")?,
            SqlKind::Postgres => {}
        }
        Ok(())
    }
    fn row(&mut self, row: &[Value]) -> Result<(), DbError> {
        let values: Vec<String> = row.iter().map(|v| sql_literal(self.kind, v)).collect();
        self.out.put(&format!(
            "INSERT INTO {} ({}) VALUES ({});\n",
            self.table,
            self.columns,
            values.join(", ")
        ))
    }
    fn finish(&mut self) -> Result<(), DbError> {
        self.out.flush()
    }
}

// ---- Excel ----

struct Xlsx {
    book: rust_xlsxwriter::Workbook,
    /// The temp file, created up front (see [`create_part`]); the workbook is saved
    /// into it by `finish`.
    file: Option<File>,
    shown: String,
    row: u32,
    cut: u32,
    header: rust_xlsxwriter::Format,
    date: rust_xlsxwriter::Format,
    datetime: rust_xlsxwriter::Format,
    time: rust_xlsxwriter::Format,
}

fn xlsx_error(e: rust_xlsxwriter::XlsxError) -> DbError {
    export_error(format!("Could not write the Excel file: {e}"))
}

impl Xlsx {
    fn create(path: &Path, shown: &str) -> Result<Xlsx, DbError> {
        use rust_xlsxwriter::{Format, Workbook};
        let mut book = Workbook::new();
        // Constant memory: rows go to a temp file as they are written.
        book.add_worksheet_with_constant_memory()
            .map_err(xlsx_error)?;
        // Last: nothing fails after the temp file exists.
        let file = create_part(path, shown)?;
        Ok(Xlsx {
            book,
            file: Some(file),
            shown: shown.to_string(),
            row: 0,
            cut: 0,
            header: Format::new().set_bold(),
            date: Format::new().set_num_format("yyyy-mm-dd"),
            datetime: Format::new().set_num_format("yyyy-mm-dd hh:mm:ss"),
            time: Format::new().set_num_format("hh:mm:ss"),
        })
    }

    fn sheet(&mut self) -> Result<&mut rust_xlsxwriter::Worksheet, DbError> {
        self.book.worksheet_from_index(0).map_err(xlsx_error)
    }

    /// Text cut to Excel's cell limit (counted).
    fn fit(&mut self, s: String) -> String {
        if s.chars().count() <= XLSX_MAX_CHARS {
            return s;
        }
        self.cut = self.cut.saturating_add(1);
        s.chars().take(XLSX_MAX_CHARS).collect()
    }

    fn next_row(&mut self) -> Result<u32, DbError> {
        if u64::from(self.row) >= XLSX_MAX_ROWS {
            return Err(export_error(format!(
                "Excel sheets hold at most {XLSX_MAX_ROWS} rows; export to CSV instead."
            )));
        }
        let r = self.row;
        self.row += 1;
        Ok(r)
    }
}

fn excel_date(s: &str) -> Option<rust_xlsxwriter::ExcelDateTime> {
    let d = rust_xlsxwriter::ExcelDateTime::parse_from_str(s).ok()?;
    // Excel's calendar starts in 1900.
    (s.get(..4)?.parse::<u16>().ok()? >= 1900).then_some(d)
}

impl FormatWriter for Xlsx {
    fn begin(&mut self, cols: &[ColumnMeta]) -> Result<(), DbError> {
        if cols.len() > XLSX_MAX_COLS {
            return Err(export_error(format!(
                "Excel sheets hold at most {XLSX_MAX_COLS} columns."
            )));
        }
        let r = self.next_row()?;
        let header = self.header.clone();
        let names = unique_names(cols);
        let sheet = self.sheet()?;
        for (i, name) in names.into_iter().enumerate() {
            sheet
                .write_string_with_format(r, i as u16, name, &header)
                .map_err(xlsx_error)?;
        }
        Ok(())
    }

    fn row(&mut self, row: &[Value]) -> Result<(), DbError> {
        let r = self.next_row()?;
        for (i, v) in row.iter().enumerate() {
            let c = i as u16;
            enum Cell {
                Skip,
                Num(f64),
                Bool(bool),
                Date(rust_xlsxwriter::ExcelDateTime, u8),
                Text(String),
            }
            let cell = match v {
                Value::Null => Cell::Skip,
                v if is_partial(v) => Cell::Skip,
                Value::Bool(b) => Cell::Bool(*b),
                Value::Int(n) if n.unsigned_abs() <= 1 << 53 => Cell::Num(*n as f64),
                Value::UInt(n) if *n <= 1 << 53 => Cell::Num(*n as f64),
                Value::Float(f) if f.is_finite() => Cell::Num(*f),
                // Up to 15 significant digits round-trip through a double.
                Value::Decimal(s)
                    if s.chars().filter(char::is_ascii_digit).count() <= 15
                        && s.parse::<f64>().is_ok_and(f64::is_finite) =>
                {
                    Cell::Num(s.parse().unwrap_or_default())
                }
                Value::Date(s) => match excel_date(s) {
                    Some(d) => Cell::Date(d, 0),
                    None => Cell::Text(s.clone()),
                },
                Value::DateTime(s) => match excel_date(&iso_datetime(s)) {
                    Some(d) => Cell::Date(d, 1),
                    None => Cell::Text(iso_datetime(s)),
                },
                Value::Time(s) => match rust_xlsxwriter::ExcelDateTime::parse_from_str(s) {
                    Ok(d) if !s.starts_with('-') => Cell::Date(d, 2),
                    _ => Cell::Text(s.clone()),
                },
                other => Cell::Text(cell_text(other).unwrap_or_default()),
            };
            match cell {
                Cell::Skip => {}
                Cell::Num(n) => {
                    self.sheet()?.write_number(r, c, n).map_err(xlsx_error)?;
                }
                Cell::Bool(b) => {
                    self.sheet()?.write_boolean(r, c, b).map_err(xlsx_error)?;
                }
                Cell::Date(d, k) => {
                    let f = match k {
                        0 => self.date.clone(),
                        1 => self.datetime.clone(),
                        _ => self.time.clone(),
                    };
                    self.sheet()?
                        .write_datetime_with_format(r, c, &d, &f)
                        .map_err(xlsx_error)?;
                }
                Cell::Text(t) => {
                    let t = self.fit(t);
                    self.sheet()?.write_string(r, c, t).map_err(xlsx_error)?;
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), DbError> {
        let file = self.file.take().ok_or_else(|| DbError::Internal {
            message: "the Excel file was already saved".into(),
        })?;
        let io = |e: std::io::Error| path_error("write", &self.shown, &e);
        let mut out = BufWriter::new(file);
        self.book.save_to_writer(&mut out).map_err(|e| match e {
            rust_xlsxwriter::XlsxError::IoError(e) => io(e),
            other => xlsx_error(other),
        })?;
        out.into_inner().map_err(|e| io(e.into_error()))?;
        Ok(())
    }

    fn cut_cells(&self) -> u32 {
        self.cut
    }
}

/// A writer of `opts.format` to `path` (`shown`: the name used in error messages).
/// `kind`: the engine whose identifiers and literals the SQL format uses.
pub fn open_writer(
    opts: &ExportOptions,
    kind: SqlKind,
    path: &Path,
    shown: &str,
) -> Result<Box<dyn FormatWriter>, DbError> {
    check_options(opts)?;
    // Nothing may fail once the temp file exists: a caller removes only a temp file it
    // created (an existing one is refused and left alone).
    Ok(match opts.format {
        ExportFormat::Csv => {
            let delim = delimiter(opts)?;
            Box::new(Csv {
                out: Out::create(path, shown)?,
                bom: opts.bom,
                delim,
                header: opts.header,
                quote_all: opts.quote_all,
                formula_guard: opts.formula_guard,
            })
        }
        ExportFormat::Json => Box::new(Json {
            out: Out::create(path, shown)?,
            keys: Vec::new(),
            rows: 0,
        }),
        ExportFormat::Markdown => Box::new(Markdown {
            out: Out::create(path, shown)?,
        }),
        ExportFormat::Sql => Box::new(Sql {
            out: Out::create(path, shown)?,
            kind,
            table: quote_ident(kind, opts.table_name.trim()),
            columns: String::new(),
        }),
        ExportFormat::Xlsx => Box::new(Xlsx::create(path, shown)?),
    })
}

/// The file being written: a temp file next to the target, moved onto it once complete
/// (a failed or cancelled export leaves nothing half-written behind).
pub struct Target {
    pub final_path: PathBuf,
    pub part: PathBuf,
}

impl Target {
    pub fn new(path: &str) -> Result<Target, DbError> {
        if path.trim().is_empty() {
            return Err(export_error("Choose a file to export to."));
        }
        let final_path = PathBuf::from(path);
        let mut part = final_path.clone().into_os_string();
        part.push(".dntpart");
        Ok(Target {
            final_path,
            part: PathBuf::from(part),
        })
    }

    /// Moves the complete temp file onto the target.
    pub fn commit(&self) -> Result<(), DbError> {
        std::fs::rename(&self.part, &self.final_path).map_err(|e| {
            let _ = std::fs::remove_file(&self.part);
            path_error("write", &self.final_path.to_string_lossy(), &e)
        })
    }

    /// Removes the temp file (best effort).
    pub fn discard(&self) {
        let _ = std::fs::remove_file(&self.part);
    }

    /// A guard that removes the temp file when dropped unless [`PartGuard::commit`]
    /// ran (an early return or a panic leaves no `.dntpart` behind). Take it only once
    /// this job created the temp file (never over another writer's).
    pub fn guard(&self) -> PartGuard<'_> {
        PartGuard {
            target: self,
            armed: true,
        }
    }
}

/// See [`Target::guard`].
pub struct PartGuard<'a> {
    target: &'a Target,
    armed: bool,
}

impl PartGuard<'_> {
    /// Moves the complete temp file onto the target (removed when that fails).
    pub fn commit(mut self) -> Result<(), DbError> {
        self.armed = false;
        self.target.commit()
    }
}

impl Drop for PartGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.target.discard();
        }
    }
}

/// How often progress is reported while streaming.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

/// Streams the first result of a statement into a [`FormatWriter`] owned by a dedicated
/// writer thread, so formatting and file writes never run on a Tokio worker. The queue
/// between them holds at most [`EXPORT_QUEUE_BYTES`] of row payload: a full queue slows
/// the database stream down. A write error stops taking rows, is kept (see
/// [`ExportSink::take_error`]) and calls `abort` (which cancels the statement).
pub struct ExportSink {
    tx: std::sync::mpsc::Sender<ExportWrite>,
    gate: Arc<QueueGate>,
    error: Arc<Mutex<Option<DbError>>>,
    results: usize,
    /// Rows handed to the writer.
    rows: u64,
}

enum ExportWrite {
    Begin(Vec<ColumnMeta>),
    Rows(Vec<Vec<Value>>, usize),
    Finish(std::sync::mpsc::Sender<Result<Written, DbError>>),
    Abort(std::sync::mpsc::Sender<()>),
}

/// Most row payload (approximate bytes) queued for the export writer.
pub const EXPORT_QUEUE_BYTES: usize = 32 * 1024 * 1024;
const EXPORT_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// The queued payload; `closed` once the writer stopped taking rows.
#[derive(Default)]
struct QueueGate {
    state: Mutex<(usize, bool)>,
    changed: std::sync::Condvar,
}

impl QueueGate {
    /// Waits until `bytes` more fit (a lone batch always fits); false once closed.
    fn reserve(&self, bytes: usize) -> bool {
        let mut state = lock_export(&self.state);
        loop {
            let (queued, closed) = *state;
            if closed {
                return false;
            }
            if queued == 0 || queued.saturating_add(bytes) <= EXPORT_QUEUE_BYTES {
                state.0 = queued.saturating_add(bytes);
                return true;
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn release(&self, bytes: usize) {
        let mut state = lock_export(&self.state);
        state.0 = state.0.saturating_sub(bytes);
        drop(state);
        self.changed.notify_all();
    }

    fn close(&self) {
        lock_export(&self.state).1 = true;
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn queued(&self) -> usize {
        lock_export(&self.state).0
    }
}

impl ExportSink {
    pub fn new(
        writer: Box<dyn FormatWriter>,
        progress: Box<dyn FnMut(u64) + Send>,
        abort: Box<dyn Fn() + Send>,
    ) -> ExportSink {
        let (tx, rx) = std::sync::mpsc::channel();
        let error = Arc::new(Mutex::new(None));
        let gate = Arc::new(QueueGate::default());
        let worker = ExportWorker {
            writer: Some(writer),
            progress,
            abort,
            error: Arc::clone(&error),
            gate: Arc::clone(&gate),
        };
        std::thread::Builder::new()
            .name("dnt-export-writer".into())
            .spawn(move || worker.run(rx))
            .expect("start export writer");
        ExportSink {
            tx,
            gate,
            error,
            results: 0,
            rows: 0,
        }
    }

    /// Rows handed to the writer so far.
    pub fn row_count(&self) -> u64 {
        self.rows
    }

    pub fn take_error(&mut self) -> Option<DbError> {
        lock_export(&self.error).take()
    }

    fn send(&self, message: ExportWrite) -> Result<(), DbError> {
        self.tx
            .send(message)
            .map_err(|_| export_error("The export writer stopped unexpectedly."))
    }

    fn enqueue_rows(&mut self, batch: Vec<Vec<Value>>, bytes: usize) -> bool {
        let count = batch.len() as u64;
        // A full queue waits here, off the async executor. One indivisible oversized
        // row may pass only while the queue is empty.
        if !block_outside_executor(|| self.gate.reserve(bytes)) {
            return false;
        }
        match self.send(ExportWrite::Rows(batch, bytes)) {
            Ok(()) => {
                self.rows = self.rows.saturating_add(count);
                true
            }
            Err(error) => {
                self.gate.release(bytes);
                let mut slot = lock_export(&self.error);
                if slot.is_none() {
                    *slot = Some(error);
                }
                false
            }
        }
    }

    /// Completes the file. A statement without a result still gets a valid empty file.
    pub fn finish(self) -> Result<Written, DbError> {
        if let Some(e) = lock_export(&self.error).take() {
            return Err(e);
        }
        if self.results == 0 {
            self.send(ExportWrite::Begin(Vec::new()))?;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(ExportWrite::Finish(tx))?;
        block_outside_executor(|| rx.recv())
            .map_err(|_| export_error("The export writer stopped before finishing."))?
    }

    /// Drops the queued rows and closes the file (so it can be removed).
    pub fn abort(self) {
        self.gate.close();
        let (tx, rx) = std::sync::mpsc::channel();
        if self.send(ExportWrite::Abort(tx)).is_ok() {
            let _ = block_outside_executor(|| rx.recv());
        }
    }
}

impl RowSink for ExportSink {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.results += 1;
        if self.results == 1 {
            let _ = self.send(ExportWrite::Begin(cols.to_vec()));
        }
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        if self.results != 1 || lock_export(&self.error).is_some() {
            return;
        }
        let mut frame = Vec::new();
        let mut frame_bytes = 0usize;
        for row in batch {
            let row_bytes = row.iter().map(value_bytes).sum::<usize>().saturating_add(1);
            if !frame.is_empty() && frame_bytes.saturating_add(row_bytes) > EXPORT_FRAME_BYTES {
                if !self.enqueue_rows(std::mem::take(&mut frame), frame_bytes) {
                    return;
                }
                frame_bytes = 0;
            }
            frame_bytes = frame_bytes.saturating_add(row_bytes);
            frame.push(row);
        }
        if !frame.is_empty() {
            self.enqueue_rows(frame, frame_bytes);
        }
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

fn lock_export<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The export writer thread: owns the format writer and its file.
struct ExportWorker {
    writer: Option<Box<dyn FormatWriter>>,
    progress: Box<dyn FnMut(u64) + Send>,
    abort: Box<dyn Fn() + Send>,
    error: Arc<Mutex<Option<DbError>>>,
    gate: Arc<QueueGate>,
}

impl ExportWorker {
    /// Keeps the first error and stops the statement; queued rows are dropped from now on.
    fn fail(&self, e: DbError) {
        let mut slot = lock_export(&self.error);
        if slot.is_none() {
            *slot = Some(e);
            drop(slot);
            self.gate.close();
            (self.abort)();
        }
    }

    fn failed(&self) -> bool {
        lock_export(&self.error).is_some()
    }

    fn run(mut self, rx: std::sync::mpsc::Receiver<ExportWrite>) {
        let mut rows = 0u64;
        let mut partial = 0u32;
        let mut last = Instant::now();
        while let Ok(message) = rx.recv() {
            match message {
                ExportWrite::Begin(columns) => {
                    if let Some(Err(e)) = self.writer.as_mut().map(|w| w.begin(&columns)) {
                        self.fail(e);
                    }
                }
                ExportWrite::Rows(batch, bytes) => {
                    if !self.failed() {
                        for row in batch {
                            let Some(writer) = self.writer.as_mut() else {
                                break;
                            };
                            if let Err(e) = writer.row(&row) {
                                self.fail(e);
                                break;
                            }
                            rows = rows.saturating_add(1);
                            partial = partial.saturating_add(count_partial(&row));
                        }
                        if last.elapsed() >= PROGRESS_EVERY {
                            last = Instant::now();
                            (self.progress)(rows);
                        }
                    }
                    self.gate.release(bytes);
                }
                ExportWrite::Finish(reply) => {
                    let result = match (lock_export(&self.error).clone(), self.writer.as_mut()) {
                        (Some(e), _) => Err(e),
                        (None, None) => Err(export_error("The export writer was closed.")),
                        (None, Some(writer)) => writer.finish().map(|_| Written {
                            rows,
                            cut_cells: writer.cut_cells(),
                            partial_binaries: partial,
                        }),
                    };
                    if result.is_ok() {
                        (self.progress)(rows);
                    }
                    drop(self.writer.take());
                    self.gate.close();
                    let _ = reply.send(result);
                    return;
                }
                ExportWrite::Abort(reply) => {
                    drop(self.writer.take());
                    self.gate.close();
                    let _ = reply.send(());
                    return;
                }
            }
        }
        self.gate.close();
    }
}

/// Writes rows already in memory (the loaded rows) to `target`.
pub fn write_rows(
    opts: &ExportOptions,
    kind: SqlKind,
    target: &Target,
    columns: &[ColumnMeta],
    rows: &[Vec<Value>],
) -> Result<Written, DbError> {
    let shown = target.final_path.to_string_lossy().to_string();
    // Not ours to remove when it cannot be created (it may exist already).
    let mut w = open_writer(opts, kind, &target.part, &shown)?;
    let result = (|| {
        w.begin(columns)?;
        let mut partial = 0u32;
        for row in rows {
            w.row(row)?;
            partial = partial.saturating_add(count_partial(row));
        }
        w.finish()?;
        Ok(Written {
            rows: rows.len() as u64,
            cut_cells: w.cut_cells(),
            partial_binaries: partial,
        })
    })();
    // Closed before it is moved or removed.
    drop(w);
    match result {
        Ok(done) => {
            target.commit()?;
            Ok(done)
        }
        Err(e) => {
            target.discard();
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::value::LogicalType;

    fn col(name: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            db_type: "text".into(),
            logical: LogicalType::Text,
            nullable: Some(true),
            origin_table: None,
            origin_column: None,
        }
    }

    fn opts(format: ExportFormat) -> ExportOptions {
        ExportOptions {
            format,
            delimiter: ",".into(),
            header: true,
            quote_all: false,
            bom: false,
            table_name: "t".into(),
            formula_guard: false,
        }
    }

    fn sample() -> (Vec<ColumnMeta>, Vec<Vec<Value>>) {
        (
            vec![col("id"), col("name"), col("blob"), col("at")],
            vec![
                vec![
                    Value::Int(1),
                    Value::Text("a,\"b\"\nc".into()),
                    Value::bytes(&[0xde, 0xad]),
                    Value::DateTime("2024-01-31 10:00:00".into()),
                ],
                vec![
                    Value::Int(2),
                    Value::Null,
                    Value::Null,
                    Value::Date("2024-02-01".into()),
                ],
                vec![
                    Value::Int(3),
                    Value::Text(String::new()),
                    Value::Bytes {
                        len: 2,
                        preview: vec![0, 1],
                    },
                    Value::TimestampTz("2024-01-31T10:00:00+00:00".into()),
                ],
            ],
        )
    }

    fn export(o: &ExportOptions, kind: SqlKind) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.file");
        let target = Target::new(path.to_str().unwrap()).unwrap();
        let (cols, rows) = sample();
        assert_eq!(write_rows(o, kind, &target, &cols, &rows).unwrap().rows, 3);
        assert!(!target.part.exists());
        (dir, path)
    }

    fn read(path: &Path) -> String {
        String::from_utf8(std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn csv_is_rfc4180_with_null_unquoted() {
        let (_d, p) = export(&opts(ExportFormat::Csv), SqlKind::MySql);
        assert_eq!(
            read(&p),
            "id,name,blob,at\r\n\
             1,\"a,\"\"b\"\"\nc\",0xdead,2024-01-31T10:00:00\r\n\
             2,,,2024-02-01\r\n\
             3,\"\",0x0001,2024-01-31T10:00:00+00:00\r\n"
        );
    }

    fn one_row(
        o: &ExportOptions,
        kind: SqlKind,
        cols: &[ColumnMeta],
        row: Vec<Value>,
    ) -> (String, Written) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.out");
        let target = Target::new(path.to_str().unwrap()).unwrap();
        let written = write_rows(o, kind, &target, cols, &[row]).unwrap();
        // Excel files are binary: only the text formats are read back.
        let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
        (text, written)
    }

    #[test]
    fn csv_formula_protection_is_opt_in_and_spares_numbers() {
        let cols = [col("a"), col("b"), col("c"), col("d"), col("e")];
        let row = || {
            vec![
                Value::Text("=1+1".into()),
                Value::Text("@SUM(A1)".into()),
                Value::Int(-5),
                Value::Text("\tx".into()),
                Value::Text("plain".into()),
            ]
        };
        let plain = ExportOptions {
            header: false,
            ..opts(ExportFormat::Csv)
        };
        let (text, _) = one_row(&plain, SqlKind::MySql, &cols, row());
        assert_eq!(text, "=1+1,@SUM(A1),-5,\tx,plain\r\n");
        let guarded = ExportOptions {
            formula_guard: true,
            ..plain
        };
        let (text, _) = one_row(&guarded, SqlKind::MySql, &cols, row());
        assert_eq!(text, "'=1+1,'@SUM(A1),-5,'\tx,plain\r\n");
        assert!(formula_like("+1") && formula_like("-a") && formula_like("\rx"));
        assert!(!formula_like("a=1") && !formula_like(""));
    }

    #[test]
    fn partial_binaries_are_never_written_as_whole() {
        let partial = || Value::Bytes {
            len: 1000,
            preview: vec![0xab; 64],
        };
        let cols = [col("id"), col("blob"), col("arr")];
        let row = || {
            vec![
                Value::Int(1),
                partial(),
                Value::Array(vec![partial(), Value::bytes(&[1])]),
            ]
        };
        let (text, w) = one_row(&opts(ExportFormat::Csv), SqlKind::Postgres, &cols, row());
        assert_eq!(w.partial_binaries, 2);
        assert_eq!(
            text.lines().nth(1).unwrap(),
            "1,,\"{NULL,\"\"\\\\x01\"\"}\""
        );
        let (text, w) = one_row(&opts(ExportFormat::Json), SqlKind::Postgres, &cols, row());
        assert_eq!(w.partial_binaries, 2);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v[0]["blob"], serde_json::Value::Null);
        assert_eq!(v[0]["arr"], serde_json::json!([null, "0x01"]));
        let (text, _) = one_row(
            &opts(ExportFormat::Sql),
            SqlKind::MySql,
            &cols[..2],
            row()[..2].to_vec(),
        );
        assert!(text.contains("VALUES (1, NULL);"), "{text}");
        let (text, _) = one_row(
            &opts(ExportFormat::Markdown),
            SqlKind::MySql,
            &cols[..2],
            row()[..2].to_vec(),
        );
        assert_eq!(text.lines().nth(2).unwrap(), "| 1 |  |");
        let (_, w) = one_row(
            &opts(ExportFormat::Xlsx),
            SqlKind::MySql,
            &cols[..2],
            row()[..2].to_vec(),
        );
        assert_eq!(w.partial_binaries, 1);
        // A whole value (the streamed rows) is written and not counted.
        assert_eq!(count_partial(&[Value::bytes(&[1, 2])]), 0);
    }

    #[test]
    fn duplicate_column_names() {
        let cols = [col("id"), col("id")];
        let row = vec![Value::Int(1), Value::Int(2)];
        let (text, _) = one_row(&opts(ExportFormat::Csv), SqlKind::MySql, &cols, row.clone());
        assert!(text.starts_with("id,id_2\r\n"), "{text}");
        let (text, _) = one_row(
            &opts(ExportFormat::Markdown),
            SqlKind::MySql,
            &cols,
            row.clone(),
        );
        assert!(text.starts_with("| id | id_2 |"), "{text}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dup.sql");
        let target = Target::new(path.to_str().unwrap()).unwrap();
        let err = write_rows(
            &opts(ExportFormat::Sql),
            SqlKind::MySql,
            &target,
            &cols,
            &[row],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("two columns named \"id\""),
            "{err}"
        );
        assert!(!path.exists() && !target.part.exists());
    }

    #[test]
    fn strict_sql_numbers() {
        for ok in [
            "0",
            "-12.5",
            "1.",
            ".5",
            "1e10",
            "1E+3",
            "-2.5e-3",
            "123456789012345678901234567890",
        ] {
            assert!(is_sql_number(ok), "{ok}");
        }
        for bad in [
            "", "-", ".", "1-2", "1e+", "1e", "--1", "+1", "1.2.3", "e5", "1e5.5", "NaN", "1 ",
            "0x10", "1e+-2",
        ] {
            assert!(!is_sql_number(bad), "{bad}");
        }
        assert_eq!(
            sql_literal(SqlKind::MySql, &Value::Decimal("1-2".into())),
            "'1-2'"
        );
    }

    #[test]
    fn json_big_integers_are_strings() {
        let max = (1i64 << 53) - 1;
        assert_eq!(json_value(&Value::Int(max)), max.to_string());
        assert_eq!(json_value(&Value::Int(-max)), (-max).to_string());
        assert_eq!(json_value(&Value::Int(max + 1)), format!("\"{}\"", max + 1));
        assert_eq!(
            json_value(&Value::Int(-max - 1)),
            format!("\"{}\"", -max - 1)
        );
        assert_eq!(
            json_value(&Value::UInt(max as u64 + 1)),
            format!("\"{}\"", max + 1)
        );
    }

    #[test]
    fn csv_quote_all_bom_and_delimiter() {
        let o = ExportOptions {
            delimiter: ";".into(),
            quote_all: true,
            bom: true,
            header: false,
            ..opts(ExportFormat::Csv)
        };
        let (_d, p) = export(&o, SqlKind::MySql);
        let text = read(&p);
        assert!(text.starts_with('\u{feff}'));
        let lines: Vec<&str> = text.trim_start_matches('\u{feff}').split("\r\n").collect();
        // NULL stays an empty unquoted field; an empty string is quoted.
        // No header: the rows only.
        assert!(lines[0].starts_with("\"1\";"), "{}", lines[0]);
        assert_eq!(lines[1], "\"2\";;;\"2024-02-01\"");
        assert!(lines[2].starts_with("\"3\";\"\";"), "{}", lines[2]);
        let tab = ExportOptions {
            delimiter: "\t".into(),
            ..opts(ExportFormat::Csv)
        };
        let (_d, p) = export(&tab, SqlKind::MySql);
        assert!(read(&p).starts_with("id\tname\tblob\tat\r\n"));
        for bad in ["", ";;", "\"", "\n"] {
            let o = ExportOptions {
                delimiter: bad.into(),
                ..opts(ExportFormat::Csv)
            };
            assert!(check_options(&o).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn json_is_an_array_of_objects() {
        let (_d, p) = export(&opts(ExportFormat::Json), SqlKind::Postgres);
        let v: serde_json::Value = serde_json::from_str(&read(&p)).unwrap();
        assert_eq!(
            v,
            serde_json::json!([
                {"id": 1, "name": "a,\"b\"\nc", "blob": "0xdead", "at": "2024-01-31T10:00:00"},
                {"id": 2, "name": null, "blob": null, "at": "2024-02-01"},
                {"id": 3, "name": "", "blob": "0x0001", "at": "2024-01-31T10:00:00+00:00"},
            ])
        );
    }

    #[test]
    fn json_values_keep_types_and_duplicate_names_are_suffixed() {
        assert_eq!(json_value(&Value::Decimal("12.50".into())), "12.50");
        assert_eq!(json_value(&Value::Decimal("NaN".into())), "\"NaN\"");
        assert_eq!(
            json_value(&Value::Json("{\"a\": [1]}".into())),
            "{\"a\": [1]}"
        );
        assert_eq!(json_value(&Value::Json("not json".into())), "\"not json\"");
        assert_eq!(json_value(&Value::Float(f64::NAN)), "\"NaN\"");
        assert_eq!(
            json_value(&Value::UInt(u64::MAX)),
            "\"18446744073709551615\""
        );
        assert_eq!(
            json_value(&Value::Array(vec![Value::Int(1), Value::Null])),
            "[1,null]"
        );
        assert_eq!(
            unique_names(&[col("a"), col("a"), col("a_2")]),
            ["a", "a_2", "a_2_2"]
        );
        // No rows: an empty array.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.json");
        let target = Target::new(path.to_str().unwrap()).unwrap();
        write_rows(
            &opts(ExportFormat::Json),
            SqlKind::MySql,
            &target,
            &[col("a")],
            &[],
        )
        .unwrap();
        assert_eq!(read(&path), "[]\n");
    }

    #[test]
    fn markdown_escapes_pipes_and_newlines() {
        let (_d, p) = export(&opts(ExportFormat::Markdown), SqlKind::MySql);
        let text = read(&p);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "| id | name | blob | at |");
        assert_eq!(lines[1], "| --- | --- | --- | --- |");
        assert_eq!(
            lines[2],
            "| 1 | a,\"b\"<br>c | 0xdead | 2024-01-31T10:00:00 |"
        );
        assert_eq!(lines[3], "| 2 | NULL | NULL | 2024-02-01 |");
        assert_eq!(md_cell("a|b\\"), "a\\|b\\\\");
    }

    #[test]
    fn sql_inserts_quote_per_engine() {
        let o = ExportOptions {
            table_name: "we`ird\"t".into(),
            ..opts(ExportFormat::Sql)
        };
        let (_d, p) = export(&o, SqlKind::MySql);
        let text = read(&p);
        assert_eq!(text.lines().next(), Some("SET NAMES utf8mb4;"));
        let first = text.lines().nth(1).unwrap();
        assert_eq!(
            first,
            "INSERT INTO `we``ird\"t` (`id`, `name`, `blob`, `at`) VALUES (1, 'a,\"b\"",
        );
        assert!(text.contains("X'dead', '2024-01-31 10:00:00');"), "{text}");
        assert!(
            text.contains("VALUES (2, NULL, NULL, '2024-02-01');"),
            "{text}"
        );
        let (_d, p) = export(&o, SqlKind::Postgres);
        let text = read(&p);
        assert!(
            text.starts_with("INSERT INTO \"we`ird\"\"t\" (\"id\", \"name\", \"blob\", \"at\")"),
            "{text}"
        );
        assert!(text.contains("E'\\\\xdead'::bytea"), "{text}");
        let empty = ExportOptions {
            table_name: "  ".into(),
            ..opts(ExportFormat::Sql)
        };
        assert!(check_options(&empty).is_err());
    }

    #[test]
    fn sql_literals_escape_strings() {
        let my = |v: Value| sql_literal(SqlKind::MySql, &v);
        let pg = |v: Value| sql_literal(SqlKind::Postgres, &v);
        // A backslash or NUL: hex on MySQL (the same with NO_BACKSLASH_ESCAPES), an
        // escape string on PostgreSQL.
        assert_eq!(
            my(Value::Text("it's \\ ok".into())),
            "_utf8mb4 X'69742773205c206f6b'"
        );
        assert_eq!(my(Value::Text("it's ok".into())), "'it''s ok'");
        assert_eq!(pg(Value::Text("it's \\ ok".into())), "E'it''s \\\\ ok'");
        assert_eq!(pg(Value::Text("it's ok".into())), "'it''s ok'");
        assert_eq!(my(Value::Text("a\0b".into())), "_utf8mb4 X'610062'");
        assert_eq!(my(Value::Text("é".into())), "'é'");
        // bytea and bytea[].
        assert_eq!(pg(Value::bytes(&[0xde, 0xad])), "E'\\\\xdead'::bytea");
        assert_eq!(
            pg(Value::Array(vec![
                Value::bytes(&[1]),
                Value::Null,
                Value::Array(vec![Value::bytes(&[2])])
            ])),
            "ARRAY[E'\\\\x01'::bytea, NULL, ARRAY[E'\\\\x02'::bytea]]::bytea[]"
        );
        // MySQL has no bytea[]: an array holding bytes is written as its text.
        assert_eq!(
            my(Value::Array(vec![Value::bytes(&[1]), Value::Null])),
            "_utf8mb4 X'7b225c5c783031222c4e554c4c7d'"
        );
        // MySQL geometry (its internal form, hex) as a binary literal.
        let geo = |text: &str| Value::Other {
            type_name: "geometry".into(),
            text: text.into(),
        };
        assert_eq!(my(geo("00000000010100")), "X'00000000010100'");
        assert_eq!(my(geo("not hex")), "'not hex'");
        assert_eq!(pg(geo("0101")), "'0101'");
        assert_eq!(my(Value::Bool(true)), "TRUE");
        assert_eq!(my(Value::Decimal("-12.5".into())), "-12.5");
        assert_eq!(pg(Value::Decimal("NaN".into())), "'NaN'");
        assert_eq!(pg(Value::Float(f64::INFINITY)), "'Infinity'");
        assert_eq!(
            pg(Value::Array(vec![
                Value::Int(1),
                Value::Text("a b".into()),
                Value::Null
            ])),
            "'{1,\"a b\",NULL}'"
        );
        // An injection attempt in a value stays inside its literal.
        assert_eq!(
            my(Value::Text("'); DROP TABLE t; --".into())),
            "'''); DROP TABLE t; --'"
        );
    }

    #[test]
    fn xlsx_is_a_zip_with_one_sheet() {
        let (_d, p) = export(&opts(ExportFormat::Xlsx), SqlKind::MySql);
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[..2], b"PK");
        assert!(bytes.len() > 1000);
    }

    #[test]
    fn xlsx_cuts_long_text_and_counts_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.xlsx");
        let target = Target::new(path.to_str().unwrap()).unwrap();
        let rows = vec![vec![Value::Text("x".repeat(XLSX_MAX_CHARS + 5))]];
        let w = write_rows(
            &opts(ExportFormat::Xlsx),
            SqlKind::MySql,
            &target,
            &[col("t")],
            &rows,
        )
        .unwrap();
        assert_eq!((w.rows, w.cut_cells), (1, 1));
    }

    #[test]
    fn a_failed_export_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sql");
        let target = Target::new(path.to_str().unwrap()).unwrap();
        let bad = ExportOptions {
            table_name: String::new(),
            ..opts(ExportFormat::Sql)
        };
        assert!(write_rows(&bad, SqlKind::MySql, &target, &[col("a")], &[]).is_err());
        assert!(!path.exists() && !target.part.exists());
        assert!(Target::new(" ").is_err());
    }

    #[test]
    fn part_guard_discards_unless_committed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sql");
        let target = Target::new(path.to_str().unwrap()).unwrap();
        std::fs::write(&target.part, b"half").unwrap();
        // Dropped without a commit (an early return, or unwinding from a panic): the
        // temp file is removed. (No real panic here: it would race the panic-location
        // capture of `commands::guard`'s tests.)
        let early_return = |t: &Target| -> Result<(), ()> {
            let _g = t.guard();
            Err(())
        };
        assert!(early_return(&target).is_err());
        assert!(!target.part.exists() && !path.exists());
        std::fs::write(&target.part, b"whole").unwrap();
        target.guard().commit().unwrap();
        assert!(!target.part.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"whole");
    }

    #[test]
    fn an_existing_temp_file_is_refused_and_left_alone() {
        for format in [
            ExportFormat::Csv,
            ExportFormat::Json,
            ExportFormat::Markdown,
            ExportFormat::Sql,
            ExportFormat::Xlsx,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("out.file");
            let target = Target::new(path.to_str().unwrap()).unwrap();
            std::fs::write(&target.part, b"someone else's").unwrap();
            let rows = vec![vec![Value::Int(1)]];
            let err =
                write_rows(&opts(format), SqlKind::MySql, &target, &[col("a")], &rows).unwrap_err();
            assert!(
                matches!(&err, DbError::Config { message, .. } if message.contains(".dntpart")),
                "{format:?}: {err:?}"
            );
            // Neither removed nor overwritten, and the target was not written.
            assert_eq!(std::fs::read(&target.part).unwrap(), b"someone else's");
            assert!(!path.exists(), "{format:?}");
            // Same through the streaming writer.
            assert!(open_writer(&opts(format), SqlKind::MySql, &target.part, "out").is_err());
            assert_eq!(std::fs::read(&target.part).unwrap(), b"someone else's");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_link_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"keep").unwrap();
        let path = dir.path().join("out.csv");
        let target = Target::new(path.to_str().unwrap()).unwrap();
        std::os::unix::fs::symlink(&victim, &target.part).unwrap();
        let rows = vec![vec![Value::Int(1)]];
        assert!(write_rows(
            &opts(ExportFormat::Csv),
            SqlKind::MySql,
            &target,
            &[col("a")],
            &rows
        )
        .is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(
            target.part.symlink_metadata().is_ok(),
            "the link is left alone"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_planted_link_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"keep").unwrap();
        let path = dir.path().join("out.csv");
        let target = Target::new(path.to_str().unwrap()).unwrap();
        // Creating a symbolic link needs a privilege (or developer mode) on Windows.
        if std::os::windows::fs::symlink_file(&victim, &target.part).is_err() {
            eprintln!("skipped: symbolic links cannot be created here");
            return;
        }
        let rows = vec![vec![Value::Int(1)]];
        assert!(write_rows(
            &opts(ExportFormat::Csv),
            SqlKind::MySql,
            &target,
            &[col("a")],
            &rows
        )
        .is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(
            target.part.symlink_metadata().is_ok(),
            "the link is left alone"
        );
    }

    #[test]
    fn text_forms() {
        assert_eq!(
            iso_datetime("2024-01-31 10:00:00.5"),
            "2024-01-31T10:00:00.5"
        );
        assert_eq!(
            iso_datetime("0001-01-01 00:00:00 BC"),
            "0001-01-01 00:00:00 BC"
        );
        assert_eq!(iso_datetime("infinity"), "infinity");
        assert_eq!(cell_text(&Value::Float(1.5)).unwrap(), "1.5");
        assert_eq!(cell_text(&Value::Null), None);
        assert_eq!(
            cell_text(&Value::Array(vec![Value::Text("x,y".into())])).unwrap(),
            "{\"x,y\"}"
        );
    }

    #[test]
    fn the_sink_streams_the_first_result_and_reports_progress() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.csv");
        let w = open_writer(&opts(ExportFormat::Csv), SqlKind::MySql, &path, "s.csv").unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let mut sink = ExportSink::new(
            w,
            Box::new(move |n| s2.lock().unwrap().push(n)),
            Box::new(|| {}),
        );
        sink.result_start(&[col("a")]);
        sink.rows(vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
        sink.result_start(&[col("b")]);
        sink.rows(vec![vec![Value::Int(9)]]);
        assert_eq!(sink.row_count(), 2);
        let w = sink.finish().unwrap();
        assert_eq!((w.rows, w.cut_cells), (2, 0));
        assert_eq!(read(&path), "a\r\n1\r\n2\r\n");
        assert_eq!(seen.lock().unwrap().last(), Some(&2));
    }

    /// A writer that takes a row only when the test lets it.
    struct Gated(std::sync::mpsc::Receiver<()>, Arc<Mutex<u64>>);

    impl FormatWriter for Gated {
        fn begin(&mut self, _: &[ColumnMeta]) -> Result<(), DbError> {
            Ok(())
        }
        fn row(&mut self, _: &[Value]) -> Result<(), DbError> {
            self.0.recv().map_err(|_| export_error("closed"))?;
            *self.1.lock().unwrap() += 1;
            Ok(())
        }
        fn finish(&mut self) -> Result<(), DbError> {
            Ok(())
        }
    }

    #[test]
    fn a_slow_writer_bounds_the_queued_payload() {
        let (go, gate) = std::sync::mpsc::channel();
        let written = Arc::new(Mutex::new(0));
        let mut sink = ExportSink::new(
            Box::new(Gated(gate, Arc::clone(&written))),
            Box::new(|_| {}),
            Box::new(|| {}),
        );
        sink.result_start(&[col("a")]);
        let batch = || vec![vec![Value::Text("x".repeat(EXPORT_QUEUE_BYTES / 4))]];
        let producer = std::thread::spawn(move || {
            for _ in 0..12 {
                sink.rows(batch());
                assert!(sink.gate.queued() <= EXPORT_QUEUE_BYTES);
            }
            sink
        });
        std::thread::sleep(Duration::from_millis(100));
        // The producer waits on the full queue instead of buffering every batch.
        assert!(!producer.is_finished());
        for _ in 0..12 {
            go.send(()).unwrap();
        }
        let sink = producer.join().unwrap();
        assert_eq!(sink.finish().unwrap().rows, 12);
        assert_eq!(*written.lock().unwrap(), 12);
    }

    #[test]
    fn a_write_error_stops_taking_rows_and_unblocks_the_producer() {
        let (go, gate) = std::sync::mpsc::channel::<()>();
        drop(go);
        let aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&aborted);
        let mut sink = ExportSink::new(
            Box::new(Gated(gate, Arc::new(Mutex::new(0)))),
            Box::new(|_| {}),
            Box::new(move || flag.store(true, std::sync::atomic::Ordering::SeqCst)),
        );
        sink.result_start(&[col("a")]);
        for _ in 0..12 {
            sink.rows(vec![vec![Value::Text("x".repeat(EXPORT_QUEUE_BYTES / 4))]]);
        }
        assert!(aborted.load(std::sync::atomic::Ordering::SeqCst));
        assert!(sink.finish().is_err());
    }
}
