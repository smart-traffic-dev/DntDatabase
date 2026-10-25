//! XLSX reader (R12): one sheet through calamine's streaming cell reader.
//!
//! `XlsxCellReader` borrows the workbook, so a worker thread owns both and sends finished rows
//! through a bounded channel; memory holds the shared-string table (calamine loads it when the
//! workbook opens) plus at most [`CHANNEL_ROWS`] rows. Dropping the reader stops the worker at its
//! next send.
//!
//! Empty → `Null`, string → `Text`, int/float → `Number` (integers without `.0`), bool → `Bool`,
//! date/datetime → `DateTime` (ISO, seconds), duration → `Text` (`h:mm:ss`), error cell →
//! `ReadError`. Rows without any value are skipped. The record number is the sheet row (1-based).

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

use calamine::{open_workbook, DataRef, ExcelDateTime, Reader, Xlsx, XlsxError};

use super::{numbered_columns, unique_names, Cell, ReadError, Record, RecordReader, SourceOptions};

/// Rows the worker may run ahead of the consumer.
const CHANNEL_ROWS: usize = 64;
/// Largest integer an f64 holds exactly (2^53).
const EXACT_INT: f64 = 9_007_199_254_740_992.0;

/// A sheet row (0-based index) with its non-empty cells `(0-based column, value)`.
type RawRow = (u32, Vec<(u32, Cell)>);

enum Msg {
    Start {
        sheets: Vec<String>,
        /// Declared used range `(start row, start col, end row, end col)`, if the sheet has one.
        dims: Option<(u32, u32, u32, u32)>,
    },
    /// A row (0-based index) with its non-empty cells `(0-based column, value)`.
    Row(u32, Vec<(u32, Cell)>),
    End,
    Err(ReadError),
}

pub(crate) struct XlsxReader {
    rx: Receiver<Msg>,
    columns: Vec<String>,
    sheets: Vec<String>,
    first_col: u32,
    /// Row read ahead (the first data row of a headerless sheet).
    pending: Option<RawRow>,
    /// Declared row range for the progress estimate.
    rows: Option<(u32, u32)>,
    last_row: Option<u32>,
    total: u64,
    done: bool,
}

/// Maps calamine errors to fixed messages (some of its variants carry cell text).
fn xlsx_error(e: &XlsxError, record: Option<u64>) -> ReadError {
    let msg = match e {
        XlsxError::Io(io) => format!("Could not read the file: {io}"),
        XlsxError::Zip(_) => "The file is not an XLSX workbook".into(),
        XlsxError::Password => "The workbook is password protected".into(),
        XlsxError::WorksheetNotFound(_) | XlsxError::NotAWorksheet(_) => {
            "The selected sheet is not a worksheet of this workbook".into()
        }
        _ => "The XLSX file is malformed or unsupported".into(),
    };
    ReadError::new(record, msg)
}

fn column_letters(mut col: u32) -> String {
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (col % 26) as u8);
        if col < 26 {
            break;
        }
        col = col / 26 - 1;
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII")
}

fn number_text(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < EXACT_INT {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

fn datetime_text(dt: &ExcelDateTime) -> String {
    let v = dt.as_f64();
    if dt.is_duration() {
        let secs = (v * 86_400.0).round() as i64;
        let sign = if secs < 0 { "-" } else { "" };
        let s = secs.unsigned_abs();
        return format!("{sign}{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60);
    }
    // The date comes from calamine (floor of the serial); the time of day is rounded to the
    // second here and clamped to 23:59:59, so a value just under midnight never shows hour 24
    // (calamine's own rounding can carry into it) nor moves to the next day.
    let (y, mo, d, ..) = dt.to_ymd_hms_milli();
    let frac = if v.is_finite() { v - v.floor() } else { 0.0 };
    let secs = ((frac * 86_400.0).round() as u32).min(86_399);
    let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if (0.0..1.0).contains(&v) {
        format!("{h:02}:{mi:02}:{s:02}")
    } else if secs == 0 {
        format!("{y:04}-{mo:02}-{d:02}")
    } else {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}")
    }
}

/// `Ok(None)` = empty cell; `Err(())` = an error value.
fn cell_of(v: DataRef<'_>) -> Result<Option<Cell>, ()> {
    Ok(Some(match v {
        DataRef::Empty => return Ok(None),
        DataRef::Int(i) => Cell::Number(i.to_string()),
        DataRef::Float(f) => Cell::Number(number_text(f)),
        DataRef::String(s) => Cell::Text(s),
        DataRef::SharedString(s) => Cell::Text(s.to_owned()),
        DataRef::Bool(b) => Cell::Bool(b),
        DataRef::DateTime(dt) => {
            if dt.is_duration() {
                Cell::Text(datetime_text(&dt))
            } else {
                Cell::DateTime(datetime_text(&dt))
            }
        }
        DataRef::DateTimeIso(s) => Cell::DateTime(s),
        DataRef::DurationIso(s) => Cell::Text(s),
        DataRef::Error(_) => return Err(()),
    }))
}

/// Worker body: returns early (quietly) when the consumer is gone.
fn work(path: &Path, sheet: Option<&str>, tx: &SyncSender<Msg>) -> Result<(), ReadError> {
    let mut wb: Xlsx<BufReader<File>> = open_workbook(path).map_err(|e| xlsx_error(&e, None))?;
    let sheets = wb.sheet_names();
    let name = match sheet {
        Some(s) => {
            if !sheets.iter().any(|n| n == s) {
                return Err(ReadError::file("The workbook has no sheet of that name"));
            }
            s.to_owned()
        }
        None => sheets
            .first()
            .cloned()
            .ok_or_else(|| ReadError::file("The workbook has no sheet"))?,
    };
    let mut cells = wb
        .worksheet_cells_reader(&name)
        .map_err(|e| xlsx_error(&e, None))?;
    let d = cells.dimensions();
    let dims = (d.end >= d.start && d != calamine::Dimensions::default())
        .then_some((d.start.0, d.start.1, d.end.0, d.end.1));
    if tx.send(Msg::Start { sheets, dims }).is_err() {
        return Ok(());
    }
    let mut row: Option<u32> = None;
    let mut values: Vec<(u32, Cell)> = Vec::new();
    loop {
        let next = cells.next_cell().map_err(|e| {
            let at = row.map(|r| r as u64 + 1);
            xlsx_error(&e, at)
        })?;
        let Some(cell) = next else { break };
        let (r, c) = cell.get_position();
        if row != Some(r) {
            if let Some(prev) = row {
                if !values.is_empty()
                    && tx
                        .send(Msg::Row(prev, std::mem::take(&mut values)))
                        .is_err()
                {
                    return Ok(());
                }
            }
            row = Some(r);
        }
        match cell_of(cell.get_value().clone()) {
            Ok(Some(v)) => values.push((c, v)),
            Ok(None) => {}
            Err(()) => {
                return Err(ReadError::at(
                    r as u64 + 1,
                    format!("Cell {}{} holds an error value", column_letters(c), r + 1),
                ))
            }
        }
    }
    if let Some(prev) = row {
        if !values.is_empty() && tx.send(Msg::Row(prev, values)).is_err() {
            return Ok(());
        }
    }
    let _ = tx.send(Msg::End);
    Ok(())
}

fn spawn(path: PathBuf, sheet: Option<String>) -> Result<Receiver<Msg>, ReadError> {
    let (tx, rx) = sync_channel(CHANNEL_ROWS);
    std::thread::Builder::new()
        .name("dnt-xlsx-reader".into())
        .spawn(move || {
            if let Err(e) = work(&path, sheet.as_deref(), &tx) {
                let _ = tx.send(Msg::Err(e));
            }
        })
        .map_err(|e| ReadError::file(format!("Could not start the XLSX reader: {e}")))?;
    Ok(rx)
}

fn header_text(c: Cell) -> String {
    match c {
        Cell::Null => String::new(),
        Cell::Bool(b) => if b { "TRUE" } else { "FALSE" }.into(),
        Cell::Text(s) | Cell::Number(s) | Cell::Json(s) | Cell::DateTime(s) => s,
    }
}

impl XlsxReader {
    pub(crate) fn open(path: &Path, opts: &SourceOptions) -> Result<Self, ReadError> {
        let total = std::fs::metadata(path)
            .map_err(|e| ReadError::file(format!("Could not open the file: {e}")))?
            .len();
        let rx = spawn(path.to_path_buf(), opts.sheet.clone())?;
        let mut me = Self {
            rx,
            columns: Vec::new(),
            sheets: Vec::new(),
            first_col: 0,
            pending: None,
            rows: None,
            last_row: None,
            total,
            done: false,
        };
        let dims = match me.recv()? {
            Some(Msg::Start { sheets, dims }) => {
                me.sheets = sheets;
                dims
            }
            _ => return Err(ReadError::file("The XLSX reader stopped unexpectedly")),
        };
        me.rows = dims.map(|(r0, _, r1, _)| (r0, r1));
        let first = me.next_row()?;
        let Some((row, cells)) = first else {
            return Ok(me);
        };
        if opts.header {
            let lo = cells.iter().map(|(c, _)| *c).min().unwrap_or(0);
            let hi = cells.iter().map(|(c, _)| *c).max().unwrap_or(0);
            let mut names = vec![String::new(); (hi - lo + 1) as usize];
            for (c, v) in cells {
                names[(c - lo) as usize] = header_text(v);
            }
            me.first_col = lo;
            me.columns = unique_names(names);
        } else {
            let lo = cells.iter().map(|(c, _)| *c).min().unwrap_or(0);
            let hi = cells.iter().map(|(c, _)| *c).max().unwrap_or(0);
            let (lo, hi) = match dims {
                Some((_, c0, _, c1)) => (lo.min(c0), hi.max(c1)),
                None => (lo, hi),
            };
            me.first_col = lo;
            me.columns = numbered_columns((hi - lo + 1) as usize);
            me.pending = Some((row, cells));
        }
        Ok(me)
    }

    /// One message; `None` after `End`.
    fn recv(&mut self) -> Result<Option<Msg>, ReadError> {
        if self.done {
            return Ok(None);
        }
        match self.rx.recv() {
            Ok(Msg::End) => {
                self.done = true;
                Ok(None)
            }
            Ok(Msg::Err(e)) => {
                self.done = true;
                Err(e)
            }
            Ok(m) => Ok(Some(m)),
            Err(_) => {
                self.done = true;
                Err(ReadError::file("The XLSX reader stopped unexpectedly"))
            }
        }
    }

    fn next_row(&mut self) -> Result<Option<RawRow>, ReadError> {
        match self.recv()? {
            Some(Msg::Row(r, cells)) => {
                self.last_row = Some(r);
                Ok(Some((r, cells)))
            }
            Some(_) => Err(ReadError::file("The XLSX reader stopped unexpectedly")),
            None => Ok(None),
        }
    }
}

impl RecordReader for XlsxReader {
    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn next_record(&mut self) -> Result<Option<Record>, ReadError> {
        let (row, values) = match self.pending.take() {
            Some(p) => p,
            None => match self.next_row()? {
                Some(p) => p,
                None => return Ok(None),
            },
        };
        let number = row as u64 + 1;
        let width = self.columns.len() as u32;
        let mut cells = vec![Cell::Null; self.columns.len()];
        for (c, v) in values {
            if c < self.first_col || c - self.first_col >= width {
                return Err(ReadError::at(
                    number,
                    format!(
                        "Cell {}{} is outside the sheet's columns",
                        column_letters(c),
                        number
                    ),
                ));
            }
            cells[(c - self.first_col) as usize] = v;
        }
        Ok(Some(Record { number, cells }))
    }

    fn bytes_read(&self) -> u64 {
        if self.done && self.pending.is_none() {
            return self.total;
        }
        match (self.rows, self.last_row) {
            (Some((r0, r1)), Some(r)) if r1 > r0 => {
                let part = (r.saturating_sub(r0)) as f64 / (r1 - r0 + 1) as f64;
                ((self.total as f64 * part) as u64).min(self.total.saturating_sub(1))
            }
            _ => 0,
        }
    }

    fn total_bytes(&self) -> u64 {
        self.total
    }

    fn sheets(&self) -> &[String] {
        &self.sheets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters() {
        assert_eq!(column_letters(0), "A");
        assert_eq!(column_letters(25), "Z");
        assert_eq!(column_letters(26), "AA");
        assert_eq!(column_letters(701), "ZZ");
        assert_eq!(column_letters(702), "AAA");
    }

    #[test]
    fn numbers_drop_dot_zero() {
        assert_eq!(number_text(42.0), "42");
        assert_eq!(number_text(-3.0), "-3");
        assert_eq!(number_text(1.5), "1.5");
        assert_eq!(number_text(1e20), "100000000000000000000");
    }
}
