//! JSON (a top-level array of objects) and JSONL (one object per non-blank line) readers.
//!
//! Objects are parsed one at a time; values are kept as raw JSON text so numbers stay verbatim
//! (`1e400`, `12345678901234567890`). Columns are the keys in first-seen order over the first
//! [`COLUMN_SCAN`] objects; a key first seen later is ignored with a warning.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;

use super::{open_skipping_bom, too_large, Cell, Counting, ReadError, Record, RecordReader};

/// Objects scanned for column names before the first record is returned.
pub(crate) const COLUMN_SCAN: usize = 1_000;
/// Warnings kept per reader (one per late key).
const MAX_WARNINGS: usize = 100;
/// Longest key echoed in a warning.
const MAX_KEY_CHARS: usize = 64;

type Input = BufReader<Counting<File>>;

/// One JSON object: keys in document order with raw values.
pub(crate) struct RawObject(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for RawObject {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawObject;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawObject, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Box<RawValue>>()? {
                    out.push((k, v));
                }
                Ok(RawObject(out))
            }
        }
        d.deserialize_map(V)
    }
}

fn raw_to_cell(raw: &RawValue) -> Cell {
    let text = raw.get().trim();
    match text.as_bytes().first() {
        None | Some(b'n') => Cell::Null,
        Some(b't') => Cell::Bool(true),
        Some(b'f') => Cell::Bool(false),
        Some(b'"') => match serde_json::from_str::<String>(text) {
            Ok(s) => Cell::Text(s),
            // Unreachable for a value serde_json validated; keep it lossless anyway.
            Err(_) => Cell::Json(text.to_owned()),
        },
        Some(b'{') | Some(b'[') => Cell::Json(text.to_owned()),
        Some(_) => Cell::Number(text.to_owned()),
    }
}

/// A serde_json error described without any of the input (its messages can quote values).
fn describe(e: &serde_json::Error) -> String {
    use serde_json::error::Category;
    match e.classify() {
        Category::Io => format!("Could not read the file: {e}"),
        Category::Syntax => "Invalid JSON syntax".into(),
        Category::Data => "Unexpected JSON content".into(),
        Category::Eof => "The JSON ends unexpectedly".into(),
    }
}

/// Where objects come from.
pub(crate) trait ObjectSource: Send {
    /// The next object with its record number.
    fn next_object(&mut self) -> Result<Option<(u64, RawObject)>, ReadError>;
    fn bytes_read(&self) -> u64;
}

fn skip_ws(input: &mut Input) -> Result<Option<u8>, ReadError> {
    loop {
        let buf = input.fill_buf().map_err(|e| ReadError::io(&e))?;
        if buf.is_empty() {
            return Ok(None);
        }
        match buf
            .iter()
            .position(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            Some(i) => {
                let b = buf[i];
                input.consume(i);
                return Ok(Some(b));
            }
            None => {
                let n = buf.len();
                input.consume(n);
            }
        }
    }
}

fn input_bytes(input: &Input) -> u64 {
    input.get_ref().count() - input.buffer().len() as u64
}

struct ArraySource {
    input: Input,
    index: u64,
    finished: bool,
    limit: u64,
}

impl ArraySource {
    fn new(mut input: Input, limit: u64) -> Result<Self, ReadError> {
        match skip_ws(&mut input)? {
            Some(b'[') => input.consume(1),
            _ => {
                return Err(ReadError::file(
                    "The JSON file must hold an array of objects",
                ))
            }
        }
        Ok(Self {
            input,
            index: 0,
            finished: false,
            limit,
        })
    }
}

impl ObjectSource for ArraySource {
    fn next_object(&mut self) -> Result<Option<(u64, RawObject)>, ReadError> {
        if self.finished {
            return Ok(None);
        }
        let next = self.index + 1;
        let mut b = skip_ws(&mut self.input)?;
        if self.index == 0 && b == Some(b']') {
            self.input.consume(1);
            return self.finish();
        }
        if self.index > 0 {
            match b {
                Some(b',') => {
                    self.input.consume(1);
                    b = skip_ws(&mut self.input)?;
                }
                Some(b']') => {
                    self.input.consume(1);
                    return self.finish();
                }
                Some(_) => return Err(ReadError::at(next, "Expected , or ] after the element")),
                None => return Err(ReadError::at(next, "The JSON ends unexpectedly")),
            }
        }
        match b {
            Some(b'{') => {}
            None => return Err(ReadError::at(next, "The JSON ends unexpectedly")),
            Some(_) => return Err(ReadError::at(next, "The element is not an object")),
        }
        // `{` starts the value and `}` ends it, so the deserializer reads no byte past it. The
        // `take` stops an object larger than the limit before it fills memory.
        let mut limited = (&mut self.input).take(self.limit);
        let mut de = serde_json::Deserializer::from_reader(&mut limited);
        let parsed = RawObject::deserialize(&mut de);
        let exhausted = limited.limit() == 0;
        let obj = parsed.map_err(|e| {
            if exhausted {
                too_large(next, self.limit)
            } else {
                ReadError::at(next, describe(&e))
            }
        })?;
        self.index = next;
        Ok(Some((next, obj)))
    }

    fn bytes_read(&self) -> u64 {
        input_bytes(&self.input)
    }
}

impl ArraySource {
    fn finish(&mut self) -> Result<Option<(u64, RawObject)>, ReadError> {
        self.finished = true;
        match skip_ws(&mut self.input)? {
            None => Ok(None),
            Some(_) => Err(ReadError::file("Unexpected content after the JSON array")),
        }
    }
}

struct LineSource {
    input: Input,
    line: u64,
    buf: Vec<u8>,
    limit: u64,
}

impl ObjectSource for LineSource {
    fn next_object(&mut self) -> Result<Option<(u64, RawObject)>, ReadError> {
        loop {
            self.buf.clear();
            // At most `limit` bytes plus the newline are read into memory.
            let n = (&mut self.input)
                .take(self.limit + 1)
                .read_until(b'\n', &mut self.buf)
                .map_err(|e| ReadError::io(&e))?;
            if n == 0 {
                return Ok(None);
            }
            self.line += 1;
            if n as u64 > self.limit && self.buf.last() != Some(&b'\n') {
                return Err(too_large(self.line, self.limit));
            }
            let line = self.buf.trim_ascii();
            if line.is_empty() {
                continue;
            }
            if line[0] != b'{' {
                return Err(ReadError::at(self.line, "The line is not a JSON object"));
            }
            let obj = serde_json::from_slice::<RawObject>(line)
                .map_err(|e| ReadError::at(self.line, describe(&e)))?;
            return Ok(Some((self.line, obj)));
        }
    }

    fn bytes_read(&self) -> u64 {
        input_bytes(&self.input)
    }
}

pub(crate) struct ObjectReader<S> {
    source: S,
    columns: Vec<String>,
    index: HashMap<String, usize>,
    buffered: VecDeque<(u64, RawObject)>,
    /// An error met while scanning for columns, returned after the buffered records.
    pending_error: Option<ReadError>,
    warned: HashSet<String>,
    warnings: Vec<String>,
    total: u64,
    /// No further record (end or error).
    done: bool,
    /// The source reached its end cleanly.
    eof: bool,
}

fn open_input(path: &Path) -> Result<(Input, u64), ReadError> {
    let (file, total, skipped) = open_skipping_bom(path)?;
    Ok((BufReader::new(Counting::new(file, skipped)), total))
}

pub(crate) fn open_array(path: &Path, limit: u64) -> Result<impl RecordReader, ReadError> {
    let (input, total) = open_input(path)?;
    ObjectReader::new(ArraySource::new(input, limit)?, total, limit)
}

pub(crate) fn open_lines(path: &Path, limit: u64) -> Result<impl RecordReader, ReadError> {
    let (input, total) = open_input(path)?;
    ObjectReader::new(
        LineSource {
            input,
            line: 0,
            buf: Vec::new(),
            limit,
        },
        total,
        limit,
    )
}

impl<S: ObjectSource> ObjectReader<S> {
    /// Reads ahead up to [`COLUMN_SCAN`] objects, stopping early once `limit` bytes were read so
    /// the read-ahead stays within one record's budget.
    fn new(mut source: S, total: u64, limit: u64) -> Result<Self, ReadError> {
        let mut columns: Vec<String> = Vec::new();
        let mut index = HashMap::new();
        let mut buffered = VecDeque::new();
        let mut pending_error = None;
        let mut eof = false;
        while buffered.len() < COLUMN_SCAN {
            match source.next_object() {
                Ok(Some((n, obj))) => {
                    for (k, _) in &obj.0 {
                        if !index.contains_key(k) {
                            index.insert(k.clone(), columns.len());
                            columns.push(k.clone());
                        }
                    }
                    buffered.push_back((n, obj));
                    if source.bytes_read() > limit {
                        break;
                    }
                }
                Ok(None) => {
                    eof = true;
                    break;
                }
                Err(e) => {
                    // Nothing read yet: the file itself is unusable.
                    if buffered.is_empty() && e.record.is_none() {
                        return Err(e);
                    }
                    pending_error = Some(e);
                    break;
                }
            }
        }
        Ok(Self {
            source,
            columns,
            index,
            buffered,
            pending_error,
            warned: HashSet::new(),
            warnings: Vec::new(),
            total,
            done: eof,
            eof,
        })
    }

    fn make_record(&mut self, number: u64, obj: RawObject) -> Record {
        let mut cells = vec![Cell::Null; self.columns.len()];
        for (k, v) in obj.0 {
            match self.index.get(&k) {
                Some(&i) => cells[i] = raw_to_cell(&v),
                None => {
                    if self.warnings.len() < MAX_WARNINGS && self.warned.insert(k.clone()) {
                        let shown: String = k.chars().take(MAX_KEY_CHARS).collect();
                        let more = if shown.len() < k.len() { "…" } else { "" };
                        self.warnings.push(format!(
                            "Record {number}: key \"{shown}{more}\" was not in the first {COLUMN_SCAN} records and is ignored"
                        ));
                    }
                }
            }
        }
        Record { number, cells }
    }
}

impl<S: ObjectSource> RecordReader for ObjectReader<S> {
    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn next_record(&mut self) -> Result<Option<Record>, ReadError> {
        if let Some((n, obj)) = self.buffered.pop_front() {
            return Ok(Some(self.make_record(n, obj)));
        }
        if let Some(e) = self.pending_error.take() {
            self.done = true;
            return Err(e);
        }
        if self.done {
            return Ok(None);
        }
        match self.source.next_object() {
            Ok(Some((n, obj))) => Ok(Some(self.make_record(n, obj))),
            Ok(None) => {
                self.done = true;
                self.eof = true;
                Ok(None)
            }
            Err(e) => {
                self.done = true;
                Err(e)
            }
        }
    }

    fn bytes_read(&self) -> u64 {
        if self.eof && self.buffered.is_empty() {
            self.total
        } else if self.buffered.is_empty() {
            self.source.bytes_read().min(self.total)
        } else {
            // Objects read ahead for the column scan are not progress until handed out.
            0
        }
    }

    fn total_bytes(&self) -> u64 {
        self.total
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}
