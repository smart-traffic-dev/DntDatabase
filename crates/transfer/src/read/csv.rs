//! CSV/TSV reader (R12): UTF-8 only, BOM stripped, RFC 4180 quoting, delimiter `,` `;` tab `|`,
//! strict field count. Fields are kept as text; empty and null text are decided at conversion.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use ::csv::{ByteRecord, ReaderBuilder};

use super::{
    numbered_columns, open_skipping_bom, unique_names, Cell, ReadError, Record, RecordReader,
    SourceOptions, BOM,
};

/// Delimiters R12 allows, in tie-break order for the guess.
pub(crate) const DELIMITERS: [char; 4] = [',', ';', '\t', '|'];

/// Bytes of the file start the delimiter guess looks at.
const GUESS_BYTES: u64 = 64 * 1024;

/// Counts bytes and remembers where line ends are, so a record's start line is exact.
///
/// `csv` reports a record's position before it skips the previous record's `\n` (CRLF files) and
/// blank lines, so its line number can be low; [`LineTrack::line_of`] skips those bytes itself.
/// Entries before the current record are dropped, so the queue only spans read-ahead bytes.
struct LineTrack<R> {
    inner: R,
    /// Bytes read (offsets are relative to the reader start, like `csv` positions).
    pos: u64,
    /// Offsets of `\r` / `\n` bytes not yet passed, with `true` for `\n`.
    eols: VecDeque<(u64, bool)>,
    /// `\n` bytes before the oldest queued offset.
    lf_before: u64,
}

impl<R> LineTrack<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            pos: 0,
            eols: VecDeque::new(),
            lf_before: 0,
        }
    }

    /// 1-based line of the first content byte at or after `start`.
    fn line_of(&mut self, start: u64) -> u64 {
        while let Some(&(o, lf)) = self.eols.front() {
            if o >= start {
                break;
            }
            self.lf_before += lf as u64;
            self.eols.pop_front();
        }
        let mut at = start;
        while let Some(&(o, lf)) = self.eols.front() {
            if o != at {
                break;
            }
            self.lf_before += lf as u64;
            self.eols.pop_front();
            at += 1;
        }
        self.lf_before + 1
    }
}

impl<R: Read> Read for LineTrack<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        for (i, &b) in buf[..n].iter().enumerate() {
            if b == b'\n' || b == b'\r' {
                self.eols.push_back((self.pos + i as u64, b == b'\n'));
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

pub(crate) struct CsvReader {
    rdr: ::csv::Reader<LineTrack<File>>,
    /// Bytes skipped before the reader (BOM).
    skipped: u64,
    columns: Vec<String>,
    record: ByteRecord,
    /// First data record of a headerless file (read to learn the width).
    pending: Option<Record>,
    delimiter: char,
    total: u64,
    done: bool,
}

impl CsvReader {
    pub(crate) fn open(path: &Path, opts: &SourceOptions) -> Result<Self, ReadError> {
        let delimiter = match opts.delimiter {
            Some(d) if DELIMITERS.contains(&d) => d,
            Some(_) => {
                return Err(ReadError::file(
                    "Unsupported delimiter: use comma, semicolon, tab or |",
                ))
            }
            None => guess_delimiter(path)?,
        };
        let (file, total, skipped) = open_skipping_bom(path)?;
        let rdr = ReaderBuilder::new()
            .delimiter(delimiter as u8)
            .has_headers(false)
            .flexible(true)
            .from_reader(LineTrack::new(file));
        let mut me = Self {
            rdr,
            skipped,
            columns: Vec::new(),
            record: ByteRecord::new(),
            pending: None,
            delimiter,
            total,
            done: false,
        };
        if let Some(first) = me.read_raw()? {
            if opts.header {
                me.columns = unique_names(
                    first
                        .cells
                        .into_iter()
                        .map(|c| match c {
                            Cell::Text(s) => s,
                            _ => String::new(),
                        })
                        .collect(),
                );
            } else {
                me.columns = numbered_columns(first.cells.len());
                me.pending = Some(first);
            }
        }
        Ok(me)
    }

    /// Next record as text cells, without the field-count check.
    fn read_raw(&mut self) -> Result<Option<Record>, ReadError> {
        if self.done {
            return Ok(None);
        }
        let more = self.rdr.read_byte_record(&mut self.record).map_err(|e| {
            let line = e.position().map(|p| p.line());
            match e.into_kind() {
                ::csv::ErrorKind::Io(io) => {
                    ReadError::new(line, format!("Could not read the file: {io}"))
                }
                _ => ReadError::new(line, "The CSV file could not be parsed"),
            }
        })?;
        if !more {
            self.done = true;
            return Ok(None);
        }
        let start = self.record.position().map(|p| p.byte()).unwrap_or(0);
        let number = self.rdr.get_mut().line_of(start);
        let mut cells = Vec::with_capacity(self.record.len());
        for (i, field) in self.record.iter().enumerate() {
            let text = std::str::from_utf8(field).map_err(|_| {
                ReadError::at(
                    number,
                    format!(
                        "Field {} is not valid UTF-8 (only UTF-8 files are supported)",
                        i + 1
                    ),
                )
            })?;
            cells.push(Cell::Text(text.to_owned()));
        }
        Ok(Some(Record { number, cells }))
    }
}

impl RecordReader for CsvReader {
    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn next_record(&mut self) -> Result<Option<Record>, ReadError> {
        let rec = match self.pending.take() {
            Some(r) => r,
            None => match self.read_raw()? {
                Some(r) => r,
                None => return Ok(None),
            },
        };
        if rec.cells.len() != self.columns.len() {
            return Err(ReadError::at(
                rec.number,
                format!(
                    "Has {} fields, expected {}",
                    rec.cells.len(),
                    self.columns.len()
                ),
            ));
        }
        Ok(Some(rec))
    }

    fn bytes_read(&self) -> u64 {
        if self.done {
            self.total
        } else {
            (self.rdr.get_ref().pos + self.skipped).min(self.total)
        }
    }

    fn total_bytes(&self) -> u64 {
        self.total
    }

    fn delimiter(&self) -> Option<char> {
        Some(self.delimiter)
    }
}

/// Counts each allowed delimiter outside quotes on the first line (a quoted newline does not end
/// it) and takes the most frequent; none found → comma.
pub(crate) fn guess_delimiter(path: &Path) -> Result<char, ReadError> {
    let file =
        File::open(path).map_err(|e| ReadError::file(format!("Could not open the file: {e}")))?;
    let mut head = Vec::new();
    file.take(GUESS_BYTES)
        .read_to_end(&mut head)
        .map_err(|e| ReadError::io(&e))?;
    Ok(guess_from(head.strip_prefix(BOM).unwrap_or(&head)))
}

fn guess_from(bytes: &[u8]) -> char {
    let mut counts = [0usize; DELIMITERS.len()];
    let mut quoted = false;
    for &b in bytes {
        match b {
            b'"' => quoted = !quoted,
            b'\n' | b'\r' if !quoted => break,
            _ if !quoted => {
                if let Some(i) = DELIMITERS.iter().position(|&d| d as u8 == b) {
                    counts[i] += 1;
                }
            }
            _ => {}
        }
    }
    let mut best = 0;
    for i in 1..counts.len() {
        if counts[i] > counts[best] {
            best = i;
        }
    }
    DELIMITERS[best]
}

#[cfg(test)]
mod tests {
    use super::guess_from;

    #[test]
    fn guess_counts_outside_quotes_on_the_first_line() {
        assert_eq!(guess_from(b"a;b|c;d"), ';');
        assert_eq!(guess_from(b"\"a\nb;c;d\",e\n;;;;"), ',');
        assert_eq!(guess_from(b""), ',');
        assert_eq!(guess_from(b"a\tb|c\r\n;;;"), '\t');
    }
}
