//! Streaming import readers (spec §6.1, R12): CSV/TSV, JSON (top-level array of objects), JSONL and
//! XLSX (one sheet). Every reader yields [`Record`]s one at a time; no reader loads a whole file.
//!
//! A [`ReadError`] names the record and a reason, never a cell value.

mod csv;
mod json;
mod xlsx;

use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
use specta::Type;

/// Rows [`inspect`] returns at most.
pub const INSPECT_ROWS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    Csv,
    Json,
    Jsonl,
    Xlsx,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SourceOptions {
    pub format: SourceFormat,
    /// CSV only: one of `,` `;` tab `|`; `None` guesses from the first line.
    pub delimiter: Option<char>,
    /// CSV and XLSX: the first row names the columns.
    pub header: bool,
    /// XLSX only: sheet name; `None` = the first sheet.
    pub sheet: Option<String>,
}

/// One source value. Conversion to the target type happens later (`convert`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "t", content = "v")]
pub enum Cell {
    Null,
    Text(String),
    /// Number text as written in JSON, or an XLSX number (integers without `.0`).
    Number(String),
    Bool(bool),
    /// A nested JSON object or array, verbatim.
    Json(String),
    /// An XLSX date/time, ISO 8601 with seconds (`2024-01-02`, `2024-01-02T03:04:05`, `03:04:05`).
    DateTime(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Record {
    /// 1-based: CSV line where the record starts, JSON element index, JSONL line, XLSX row.
    #[specta(type = f64)]
    pub number: u64,
    pub cells: Vec<Cell>,
}

/// A reader failure: the record (if any) and a reason. Never holds a cell value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type, thiserror::Error)]
pub struct ReadError {
    #[specta(type = Option<f64>)]
    pub record: Option<u64>,
    pub message: String,
}

impl ReadError {
    pub fn new(record: Option<u64>, message: impl Into<String>) -> Self {
        Self {
            record,
            message: message.into(),
        }
    }

    pub(crate) fn at(record: u64, message: impl Into<String>) -> Self {
        Self::new(Some(record), message)
    }

    pub(crate) fn file(message: impl Into<String>) -> Self {
        Self::new(None, message)
    }

    pub(crate) fn io(e: &io::Error) -> Self {
        Self::file(format!("Could not read the file: {e}"))
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.record {
            Some(n) => write!(f, "Record {n}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

pub trait RecordReader: Send {
    fn columns(&self) -> &[String];
    fn next_record(&mut self) -> Result<Option<Record>, ReadError>;
    /// Progress: bytes of the file consumed so far (XLSX: estimated from the rows read); equals
    /// [`RecordReader::total_bytes`] once the last record was returned.
    fn bytes_read(&self) -> u64;
    fn total_bytes(&self) -> u64;
    /// Non-fatal notes for the job log (JSON keys first seen after the column scan). Never values.
    fn warnings(&self) -> &[String] {
        &[]
    }
    /// XLSX: every sheet name of the workbook.
    fn sheets(&self) -> &[String] {
        &[]
    }
    /// CSV: the delimiter in use (given or guessed).
    fn delimiter(&self) -> Option<char> {
        None
    }
}

pub fn open(path: &Path, opts: &SourceOptions) -> Result<Box<dyn RecordReader>, ReadError> {
    Ok(match opts.format {
        SourceFormat::Csv => Box::new(csv::CsvReader::open(path, opts)?),
        SourceFormat::Json => Box::new(json::open_array(path)?),
        SourceFormat::Jsonl => Box::new(json::open_lines(path)?),
        SourceFormat::Xlsx => Box::new(xlsx::XlsxReader::open(path, opts)?),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Inspection {
    pub columns: Vec<String>,
    /// The first [`INSPECT_ROWS`] records at most.
    pub rows: Vec<Record>,
    /// XLSX: all sheet names; empty otherwise.
    pub sheets: Vec<String>,
    /// CSV: the delimiter used (guessed when the options gave none).
    pub delimiter: Option<char>,
    #[specta(type = f64)]
    pub total_bytes: u64,
}

/// Opens the source and reads its first [`INSPECT_ROWS`] records for the wizard preview.
pub fn inspect(path: &Path, opts: &SourceOptions) -> Result<Inspection, ReadError> {
    let mut reader = open(path, opts)?;
    let mut rows = Vec::new();
    while rows.len() < INSPECT_ROWS {
        match reader.next_record()? {
            Some(r) => rows.push(r),
            None => break,
        }
    }
    Ok(Inspection {
        columns: reader.columns().to_vec(),
        rows,
        sheets: reader.sheets().to_vec(),
        delimiter: reader.delimiter(),
        total_bytes: reader.total_bytes(),
    })
}

/// Makes header names usable as column names: an empty name becomes `column_<n>` (1-based
/// position), a repeated name gets the first free `_2`, `_3`, … suffix. A suffix never takes a
/// name the header spells itself, so explicit names keep their place.
pub(crate) fn unique_names(raw: Vec<String>) -> Vec<String> {
    let spelled: HashSet<String> = raw.iter().filter(|s| !s.is_empty()).cloned().collect();
    let mut used: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for (i, name) in raw.into_iter().enumerate() {
        let generated = name.is_empty();
        let base = if generated {
            format!("column_{}", i + 1)
        } else {
            name
        };
        let free = |c: &String, used: &HashSet<String>| {
            !used.contains(c) && (!generated || !spelled.contains(c))
        };
        let chosen = if free(&base, &used) {
            base
        } else {
            (2u64..)
                .map(|k| format!("{base}_{k}"))
                .find(|c| !used.contains(c) && !spelled.contains(c))
                .expect("an unused suffix exists")
        };
        used.insert(chosen.clone());
        out.push(chosen);
    }
    out
}

/// Columns of a headerless source: `column_1 … column_n`.
pub(crate) fn numbered_columns(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("column_{i}")).collect()
}

/// A reader that counts the bytes pulled from the file (progress).
pub(crate) struct Counting<R> {
    inner: R,
    count: u64,
}

impl<R> Counting<R> {
    pub(crate) fn new(inner: R, count: u64) -> Self {
        Self { inner, count }
    }

    pub(crate) fn count(&self) -> u64 {
        self.count
    }
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += n as u64;
        Ok(n)
    }
}

pub(crate) const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Opens the file, skips a UTF-8 BOM and returns it with its size and the bytes skipped.
pub(crate) fn open_skipping_bom(path: &Path) -> Result<(File, u64, u64), ReadError> {
    use std::io::{Seek, SeekFrom};
    let mut file =
        File::open(path).map_err(|e| ReadError::file(format!("Could not open the file: {e}")))?;
    let total = file.metadata().map_err(|e| ReadError::io(&e))?.len();
    let mut head = [0u8; 3];
    let mut got = 0;
    while got < 3 {
        match file.read(&mut head[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(ReadError::io(&e)),
        }
    }
    if got == 3 && head == BOM {
        Ok((file, total, 3))
    } else {
        file.seek(SeekFrom::Start(0))
            .map_err(|e| ReadError::io(&e))?;
        Ok((file, total, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn unique_names_rules() {
        assert_eq!(unique_names(s(&["a", "a", "a"])), s(&["a", "a_2", "a_3"]));
        assert_eq!(
            unique_names(s(&["", "column_1"])),
            s(&["column_1_2", "column_1"])
        );
        assert_eq!(
            unique_names(s(&["x", "", ""])),
            s(&["x", "column_2", "column_3"])
        );
    }

    #[test]
    fn display_names_the_record() {
        assert_eq!(ReadError::at(7, "bad").to_string(), "Record 7: bad");
        assert_eq!(ReadError::file("bad").to_string(), "bad");
    }
}
