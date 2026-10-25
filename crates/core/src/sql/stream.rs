//! N6: incremental statement splitter for arbitrarily large scripts.
//!
//! [`StreamSplitter`] takes the input in chunks and returns the statements `split`
//! would return (same lexer, same trimming, absolute byte offsets), whatever the
//! chunking. A decision at a byte is only taken once every byte it may look at is
//! present: positions are scanned only up to the last `\n` received (all of
//! `split`'s lookaheads end within their line), and a construct that reaches the end
//! of the buffer waits for more input until [`StreamSplitter::finish`]. An unterminated
//! construct is rescanned only after its pending bytes have doubled, so the work stays
//! linear. With `detect_client`, PG `COPY … FROM stdin` data blocks and client
//! (`\…`) lines are reported as their own events.
//!
//! Memory: the buffer holds the current statement plus the unscanned tail. Since
//! nothing is decided after the last `\n`, a line is buffered whole until its `\n`
//! arrives (or `finish`); only a construct waiting for its end is bounded by
//! `max_statement_bytes` before that.

use std::collections::VecDeque;

use super::lexer::{is_kw, skip_at, tokenize, word_at, Dialect, Skipped, TokKind};
use super::split::{delimiter_directive_at, line_blank_before, statement_text};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitOptions {
    /// Report `COPY … FROM stdin` data blocks (PG) and client command lines.
    pub detect_client: bool,
    /// Largest statement accepted, in bytes.
    pub max_statement_bytes: usize,
}

impl Default for SplitOptions {
    fn default() -> Self {
        Self {
            detect_client: false,
            max_statement_bytes: 256 * 1024 * 1024,
        }
    }
}

/// A statement (or client command line); `text` is the input between `start` and
/// `end` (absolute byte offsets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStatement {
    pub text: String,
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitEvent {
    Statement(StreamStatement),
    /// `COPY … FROM stdin` data block (PG, `detect_client`): from the line after the
    /// COPY statement's delimiter up to and including the `\.` line (or end of input).
    CopyData {
        start: u64,
        end: u64,
    },
    /// A line starting with `\` (only spaces/tabs before it) outside quoted text and
    /// comments (`detect_client`); `text` is the line without its line break.
    ClientCommand(StreamStatement),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SplitError {
    #[error("The script is not valid UTF-8 at byte {offset}.")]
    InvalidUtf8 { offset: u64 },
    #[error("The statement at byte {start} is larger than {limit} bytes.")]
    StatementTooLarge { start: u64, limit: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Sql,
    /// Inside a COPY data block that started at this absolute offset.
    Copy {
        start: u64,
    },
}

/// Incremental splitter: [`push`](Self::push) chunks, call
/// [`next_event`](Self::next_event) until `None`, repeat, then
/// [`finish`](Self::finish) and drain the remaining events.
#[derive(Debug)]
pub struct StreamSplitter {
    dialect: Dialect,
    opts: SplitOptions,
    /// `buf[..head]` is consumed; `buf[head - 1]` is the byte before the current
    /// segment (a `\n` sentinel at the start of input), kept for `split`'s look-back.
    buf: Vec<u8>,
    /// Absolute offset of `buf[1]`.
    origin: u64,
    /// Start of the current segment (statement candidate).
    head: usize,
    /// Next byte to decide on; `head..scan` holds no boundary.
    scan: usize,
    /// First non-whitespace byte of the current segment.
    first: Option<usize>,
    /// Whether the part of `buf[head]`'s line before `head` holds only spaces/tabs.
    line_blank: bool,
    delim: Vec<u8>,
    /// Bytes that can only be stepped over (see [`plain_table`]).
    plain: [bool; 256],
    /// Index of the last `\n` in `buf`, searched up to `nl_checked`.
    last_nl: Option<usize>,
    nl_checked: usize,
    /// Total bytes pushed.
    received: u64,
    /// A construct at `scan` reached the end of the buffer: wait until this many
    /// bytes have been received.
    wait_for: u64,
    /// In COPY mode: where the search for the next line end resumes.
    copy_scan: usize,
    mode: Mode,
    finished: bool,
    done: bool,
    error: Option<SplitError>,
    queue: VecDeque<SplitEvent>,
}

impl StreamSplitter {
    pub fn new(dialect: Dialect, opts: SplitOptions) -> Self {
        Self {
            dialect,
            opts,
            buf: vec![b'\n'],
            origin: 0,
            head: 1,
            scan: 1,
            first: None,
            line_blank: true,
            delim: b";".to_vec(),
            plain: plain_table(b';'),
            last_nl: None,
            nl_checked: 1,
            received: 0,
            wait_for: 0,
            copy_scan: 1,
            mode: Mode::Sql,
            finished: false,
            done: false,
            error: None,
            queue: VecDeque::new(),
        }
    }

    /// Lexing rules for everything not returned yet (the current segment is rescanned).
    pub fn set_dialect(&mut self, dialect: Dialect) {
        if dialect != self.dialect {
            self.dialect = dialect;
            self.scan = self.head;
            self.first = None;
            self.wait_for = 0;
        }
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// Appends input. Ignored after [`finish`](Self::finish).
    pub fn push(&mut self, chunk: &[u8]) {
        if self.finished || chunk.is_empty() {
            return;
        }
        self.compact();
        self.buf.extend_from_slice(chunk);
        self.received += chunk.len() as u64;
    }

    /// No more input: the rest of the buffer is decided as `split` decides the end
    /// of its input.
    pub fn finish(&mut self) {
        self.finished = true;
    }

    /// The next event, or `None` when more input is needed (or everything was
    /// returned). Scans only as far as one event needs. Errors are sticky.
    pub fn next_event(&mut self) -> Result<Option<SplitEvent>, SplitError> {
        loop {
            if let Some(e) = self.queue.pop_front() {
                return Ok(Some(e));
            }
            if let Some(e) = &self.error {
                return Err(e.clone());
            }
            if self.done {
                return Ok(None);
            }
            let step = match self.mode {
                Mode::Sql => self.step_sql(),
                Mode::Copy { start } => self.step_copy(start),
            };
            match step {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                // Events queued before the error are returned first.
                Err(e) => self.error = Some(e),
            }
        }
    }

    /// All input was split and every event returned.
    pub fn is_done(&self) -> bool {
        self.done && self.queue.is_empty()
    }

    /// Absolute offset up to which the input has been turned into events.
    pub fn consumed(&self) -> u64 {
        self.abs(self.head)
    }

    fn abs(&self, i: usize) -> u64 {
        self.origin + i as u64 - 1
    }

    /// Drops consumed bytes (keeping the context byte) once they are at least half of
    /// the buffer, so moving the tail stays amortised linear.
    fn compact(&mut self) {
        let k = self.head - 1;
        if k == 0 || k * 2 < self.buf.len() {
            return;
        }
        self.buf.drain(..k);
        self.origin += k as u64;
        self.head -= k;
        // In COPY mode `scan` may lag behind `head`; it is reset on leaving.
        self.scan = self.scan.saturating_sub(k).max(self.head);
        self.first = self.first.map(|f| f - k);
        self.copy_scan = self.copy_scan.saturating_sub(k).max(self.head);
        self.nl_checked = self.nl_checked.saturating_sub(k);
        self.last_nl = self.last_nl.and_then(|n| n.checked_sub(k));
    }

    /// Positions before this index may be decided.
    fn limit(&mut self) -> usize {
        if self.finished {
            return self.buf.len();
        }
        let from = self.nl_checked;
        let fresh = &self.buf[from..];
        // `contains` is core's memchr; `rposition` from the end then stops quickly.
        if fresh.contains(&b'\n') {
            self.last_nl = fresh.iter().rposition(|&c| c == b'\n').map(|p| from + p);
        }
        self.nl_checked = self.buf.len();
        self.last_nl.map_or(0, |n| n + 1)
    }

    fn too_large(&self, from: usize) -> SplitError {
        SplitError::StatementTooLarge {
            start: self.abs(from),
            limit: self.opts.max_statement_bytes,
        }
    }

    /// The current segment, significant up to `pos`, must fit the limit.
    fn check_size(&self, pos: usize) -> Result<(), SplitError> {
        match self.first {
            Some(f) if pos - f > self.opts.max_statement_bytes => Err(self.too_large(f)),
            _ => Ok(()),
        }
    }

    fn utf8(&self, from: usize, to: usize) -> Result<&str, SplitError> {
        std::str::from_utf8(&self.buf[from..to]).map_err(|e| SplitError::InvalidUtf8 {
            offset: self.abs(from + e.valid_up_to()),
        })
    }

    /// Ends the current segment at `to` (like `split::push`); returns whether the
    /// queued statement opens a COPY data block.
    fn emit(&mut self, to: usize) -> Result<bool, SplitError> {
        let raw = self.utf8(self.head, to)?;
        let Some((offset, text)) = statement_text(raw, self.dialect) else {
            return Ok(false);
        };
        let start = self.head + offset;
        if text.len() > self.opts.max_statement_bytes {
            return Err(self.too_large(start));
        }
        let copy = self.opts.detect_client
            && self.dialect == Dialect::Postgres
            && is_copy_from_stdin(text);
        let text = text.to_string();
        let start = self.abs(start);
        self.queue.push_back(SplitEvent::Statement(StreamStatement {
            end: start + text.len() as u64,
            text,
            start,
        }));
        Ok(copy)
    }

    /// Starts the next segment at `at`.
    fn advance(&mut self, at: usize) {
        self.line_blank =
            line_blank_before(&self.buf[self.head..], at - self.head, self.line_blank);
        self.head = at;
        self.scan = at;
        self.first = None;
    }

    /// Index just past the `\n` ending the line that holds `i` (the buffer end when
    /// there is none, i.e. at the end of input), and the line end before it.
    fn line_after(&self, i: usize) -> (usize, usize) {
        match self.buf[i..].iter().position(|&c| c == b'\n') {
            Some(p) => (i + p, i + p + 1),
            None => (self.buf.len(), self.buf.len()),
        }
    }

    /// `split`'s loop over the decidable part of the buffer. `Ok(true)`: an event was
    /// queued or the state moved on; `Ok(false)`: more input is needed.
    fn step_sql(&mut self) -> Result<bool, SplitError> {
        if !self.finished && self.received < self.wait_for {
            return Ok(false);
        }
        let limit = self.limit();
        while self.scan < limit {
            let i = self.scan;
            let c = self.buf[i];
            if self.plain[c as usize] {
                let mut j = i + 1;
                while j < limit && self.plain[self.buf[j] as usize] {
                    j += 1;
                }
                self.first.get_or_insert(i);
                self.scan = j;
                self.check_size(j)?;
                continue;
            }
            if c.is_ascii_whitespace() {
                self.scan += 1;
                continue;
            }
            if c == b'\\'
                && self.opts.detect_client
                && line_blank_before(&self.buf[self.head..], i - self.head, self.line_blank)
            {
                self.emit(i)?;
                let (end, next) = self.line_after(i);
                let line = self.utf8(i, end)?.trim_end();
                let start = self.abs(i);
                let cmd = StreamStatement {
                    text: line.to_string(),
                    start,
                    end: start + line.len() as u64,
                };
                self.queue.push_back(SplitEvent::ClientCommand(cmd));
                self.advance(next);
                return Ok(true);
            }
            if self.dialect.is_mysql() && c.eq_ignore_ascii_case(&b'd') {
                let h = self.head;
                if let Some((tok, next)) =
                    delimiter_directive_at(&self.buf[h..], i - h, self.line_blank)
                {
                    self.emit(i)?;
                    self.utf8(i, h + next)?;
                    self.delim = self.buf[h + tok.start..h + tok.end].to_vec();
                    self.plain = plain_table(self.delim[0]);
                    self.advance(h + next);
                    return Ok(true);
                }
            }
            match skip_at(&self.buf, i, self.dialect) {
                Some((kind, end)) => {
                    if end >= self.buf.len() && !self.finished {
                        let from = self.first.unwrap_or(i);
                        if self.buf.len() - from > self.opts.max_statement_bytes {
                            return Err(self.too_large(from));
                        }
                        self.wait_for = self.received + (self.buf.len() - i) as u64;
                        return Ok(false);
                    }
                    self.first.get_or_insert(i);
                    if kind != Skipped::Comment {
                        self.check_size(end)?;
                    }
                    self.scan = end;
                }
                None if self.buf[i..].starts_with(&self.delim) => {
                    let copy = self.emit(i)?;
                    let after = i + self.delim.len();
                    if copy {
                        // The data starts on the line after the delimiter.
                        let (_, data) = self.line_after(after);
                        self.advance(data);
                        self.enter_copy();
                    } else {
                        self.advance(after);
                    }
                    return Ok(true);
                }
                None => {
                    self.first.get_or_insert(i);
                    self.scan = i + 1;
                    self.check_size(i + 1)?;
                }
            }
        }
        if !self.finished {
            return Ok(false);
        }
        let end = self.buf.len();
        let copy = self.emit(end)?;
        self.advance(end);
        if copy {
            self.enter_copy();
        } else {
            self.done = true;
        }
        Ok(true)
    }

    fn enter_copy(&mut self) {
        self.mode = Mode::Copy {
            start: self.abs(self.head),
        };
        self.copy_scan = self.head;
    }

    /// Steps over complete data lines until the `\.` line (or the end of input).
    fn step_copy(&mut self, start: u64) -> Result<bool, SplitError> {
        let mut from = self.copy_scan.max(self.head);
        loop {
            let Some(p) = self.buf[from..].iter().position(|&c| c == b'\n') else {
                self.copy_scan = self.buf.len();
                if !self.finished {
                    return Ok(false);
                }
                let end = self.buf.len();
                self.leave_copy(start, end);
                return Ok(true);
            };
            let nl = from + p;
            let line = &self.buf[self.head..nl];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line == b"\\." {
                self.leave_copy(start, nl + 1);
                return Ok(true);
            }
            self.head = nl + 1;
            from = self.head;
        }
    }

    fn leave_copy(&mut self, start: u64, end: usize) {
        self.queue.push_back(SplitEvent::CopyData {
            start,
            end: self.abs(end),
        });
        self.head = end;
        self.scan = end;
        self.first = None;
        self.line_blank = true;
        self.mode = Mode::Sql;
    }
}

/// Bytes that cannot start a lexer construct, a `DELIMITER` directive, the current
/// delimiter or a client command, and are not whitespace: `split` steps over them
/// one by one, so a run of them can be skipped at once. Conservative across dialects.
fn plain_table(delim0: u8) -> [bool; 256] {
    let mut t = [false; 256];
    for (c, slot) in (0..=255u8).zip(t.iter_mut()) {
        *slot = !c.is_ascii_whitespace() && !b"-/'\"#`Ee$dD\\".contains(&c) && c != delim0;
    }
    t
}

/// Whether `stmt` is PG `COPY … FROM STDIN`: first word `COPY`, and at paren depth 0
/// a `FROM` immediately followed by the word `STDIN` (ASCII case-insensitive).
pub fn is_copy_from_stdin(stmt: &str) -> bool {
    let toks = tokenize(stmt, Dialect::Postgres);
    if !is_kw(word_at(&toks, 0), "COPY") {
        return false;
    }
    let mut depth = 0i32;
    for (k, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Open => depth += 1,
            TokKind::Close => depth -= 1,
            TokKind::Word(w)
                if depth == 0
                    && w.eq_ignore_ascii_case("FROM")
                    && is_kw(word_at(&toks, k + 1), "STDIN") =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Splits `input` fed in chunks of `chunk` bytes (tests and callers holding the
/// whole input).
pub fn split_all(
    input: &[u8],
    dialect: Dialect,
    opts: SplitOptions,
    chunk: usize,
) -> Result<Vec<SplitEvent>, SplitError> {
    let mut s = StreamSplitter::new(dialect, opts);
    let mut out = Vec::new();
    for part in input.chunks(chunk.max(1)) {
        s.push(part);
        while let Some(e) = s.next_event()? {
            out.push(e);
        }
    }
    s.finish();
    while let Some(e) = s.next_event()? {
        out.push(e);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::split::fixtures::{cases, extra_cases};
    use crate::sql::{split, MySqlMode};
    use std::time::{Duration, Instant};

    const CHUNKS: [usize; 6] = [1, 2, 3, 7, 64, 4096];
    const MYSQL: Dialect = Dialect::MYSQL;
    use Dialect::Postgres;

    fn expected(input: &str, dialect: Dialect) -> Vec<SplitEvent> {
        split(input, dialect)
            .into_iter()
            .map(|s| {
                SplitEvent::Statement(StreamStatement {
                    text: s.text,
                    start: s.start as u64,
                    end: s.end as u64,
                })
            })
            .collect()
    }

    fn assert_matches_split(input: &str, dialect: Dialect) {
        let want = expected(input, dialect);
        for chunk in CHUNKS {
            let got = split_all(input.as_bytes(), dialect, SplitOptions::default(), chunk)
                .unwrap_or_else(|e| panic!("{e} for {input:?}"));
            assert_eq!(got, want, "chunk {chunk}, input {input:?} ({dialect:?})");
        }
    }

    fn stmt(text: &str, start: u64) -> SplitEvent {
        SplitEvent::Statement(StreamStatement {
            text: text.into(),
            start,
            end: start + text.len() as u64,
        })
    }

    fn command(text: &str, start: u64) -> SplitEvent {
        SplitEvent::ClientCommand(StreamStatement {
            text: text.into(),
            start,
            end: start + text.len() as u64,
        })
    }

    fn detect() -> SplitOptions {
        SplitOptions {
            detect_client: true,
            ..SplitOptions::default()
        }
    }

    #[test]
    fn matches_split_on_split_fixtures() {
        for (input, dialect, _) in cases().into_iter().chain(extra_cases()) {
            assert_matches_split(input, dialect);
        }
        // The look-backs and line-end rules of `split`, also across chunk cuts.
        for (input, dialect) in [
            ("SELECT 1 --\r; DROP TABLE t", Postgres),
            ("SELECT 1 --\r; DROP TABLE t", MYSQL),
            ("SELECT 1 -- c\r\nSELECT 2;", Postgres),
            ("SELECT 1; -- c\r\nSELECT 2;\r\n", MYSQL),
            ("select a$b$; select 1;e'x;'", Postgres),
            ("select 1;E'a\\';b'; x;$q$;$q$", Postgres),
            (
                "select 1; delimiter //\nselect 2//\nx delimiter ;\n//",
                MYSQL,
            ),
            ("select 1;  \t delimiter $$\nselect 2$$ select 3", MYSQL),
            ("select 1; \u{a0}select 2;\u{2028}", MYSQL),
            ("select 'đ';\nselect `ư`--", MYSQL),
            ("select 1 --", MYSQL),
            ("select 1 -- ", MYSQL),
        ] {
            assert_matches_split(input, dialect);
        }
    }

    #[test]
    fn matches_split_on_testdata_files() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/sql");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if !name.ends_with(".sql") {
                continue;
            }
            let dialect = if name.starts_with("pg_") {
                Postgres
            } else {
                MYSQL
            };
            let input = std::fs::read_to_string(&path).unwrap();
            assert!(!split(&input, dialect).is_empty(), "{name}");
            assert_matches_split(&input, dialect);
            seen += 1;
        }
        assert!(seen >= 5, "testdata files found: {seen}");
    }

    #[test]
    fn set_dialect_applies_to_rest() {
        let ansi = Dialect::MySql(MySqlMode {
            ansi_quotes: true,
            ..Default::default()
        });
        let input = "SET sql_mode='ANSI_QUOTES';\nselect \"a;b\";\n";
        let mut s = StreamSplitter::new(MYSQL, SplitOptions::default());
        s.push(input.as_bytes());
        s.finish();
        assert_eq!(
            s.next_event().unwrap(),
            Some(stmt("SET sql_mode='ANSI_QUOTES'", 0))
        );
        s.set_dialect(ansi);
        assert_eq!(s.dialect(), ansi);
        assert_eq!(s.next_event().unwrap(), Some(stmt("select \"a;b\"", 28)));
        assert_eq!(s.next_event().unwrap(), None);
        assert!(s.is_done());
        // Without the switch the default mode reads `"a;b"` as a string too, but a
        // backslash-escaped quote shows the difference.
        let input = "SET sql_mode='ANSI_QUOTES';\nselect \"a\\\";b\";\n";
        let mut s = StreamSplitter::new(MYSQL, SplitOptions::default());
        s.push(input.as_bytes());
        assert!(s.next_event().unwrap().is_some());
        s.set_dialect(ansi);
        s.finish();
        assert_eq!(s.next_event().unwrap(), Some(stmt("select \"a\\\"", 28)));
        assert_eq!(s.next_event().unwrap(), Some(stmt("b\";", 40)));
    }

    #[test]
    fn copy_block_is_reported() {
        let input = "COPY t (a) FROM stdin;\n1\n2\n\\.\nSELECT 1;";
        let data = input.find("1\n").unwrap() as u64;
        let after = input.find("SELECT 1").unwrap() as u64;
        let want = vec![
            stmt("COPY t (a) FROM stdin", 0),
            SplitEvent::CopyData {
                start: data,
                end: after,
            },
            stmt("SELECT 1", after),
        ];
        for chunk in CHUNKS {
            assert_eq!(
                split_all(input.as_bytes(), Postgres, detect(), chunk).unwrap(),
                want,
                "chunk {chunk}"
            );
        }
        // CRLF data, data with quotes and `;`, a block cut short by end of input.
        let input = "copy t from STDIN;\r\na';\t$$\r\n\\.\r\nselect 2;\ncopy u from stdin;\nx\n";
        let got = split_all(input.as_bytes(), Postgres, detect(), 3).unwrap();
        let second = input.find("x\n").unwrap() as u64;
        assert_eq!(
            got,
            vec![
                stmt("copy t from STDIN", 0),
                SplitEvent::CopyData {
                    start: 20,
                    end: input.find("select").unwrap() as u64
                },
                stmt("select 2", input.find("select").unwrap() as u64),
                stmt("copy u from stdin", input.find("copy u").unwrap() as u64),
                SplitEvent::CopyData {
                    start: second,
                    end: input.len() as u64
                },
            ]
        );
        // Not a data block: other dialect, detection off, COPY … TO STDOUT.
        let plain = split_all(
            b"COPY t FROM stdin;\n1\n",
            Postgres,
            SplitOptions::default(),
            4,
        )
        .unwrap();
        assert_eq!(plain, vec![stmt("COPY t FROM stdin", 0), stmt("1", 19)]);
        let mysql = split_all(b"COPY t FROM stdin;\n1\n", MYSQL, detect(), 4).unwrap();
        assert_eq!(mysql, vec![stmt("COPY t FROM stdin", 0), stmt("1", 19)]);
        let out = split_all(b"COPY t TO stdout;\n1\n", Postgres, detect(), 4).unwrap();
        assert_eq!(out, vec![stmt("COPY t TO stdout", 0), stmt("1", 18)]);
    }

    #[test]
    fn is_copy_from_stdin_reads_tokens() {
        assert!(is_copy_from_stdin("COPY t (a, b) FROM stdin"));
        assert!(is_copy_from_stdin(
            "copy public.\"t\" from STDIN with (format csv)"
        ));
        assert!(is_copy_from_stdin("/* c */ COPY t FROM\n  stdin"));
        assert!(!is_copy_from_stdin("COPY t FROM '/tmp/x'"));
        assert!(!is_copy_from_stdin("COPY (select 1 from stdin) TO stdout"));
        assert!(!is_copy_from_stdin("SELECT 1 FROM stdin"));
        assert!(!is_copy_from_stdin("COPY t TO stdout"));
        assert!(!is_copy_from_stdin("COPY t FROM 'stdin'"));
    }

    #[test]
    fn client_command_is_reported() {
        let input = "\\connect db\nSELECT 1;";
        for chunk in CHUNKS {
            assert_eq!(
                split_all(input.as_bytes(), Postgres, detect(), chunk).unwrap(),
                vec![command("\\connect db", 0), stmt("SELECT 1", 12)],
                "chunk {chunk}"
            );
        }
        let input = "\\restrict abc\r\nSET x = 1;\n";
        assert_eq!(
            split_all(input.as_bytes(), Postgres, detect(), 2).unwrap(),
            vec![command("\\restrict abc", 0), stmt("SET x = 1", 15)]
        );
        // A pending statement is flushed first; indentation is allowed.
        let input = "SELECT 1\n  \\g\nSELECT 2;";
        assert_eq!(
            split_all(input.as_bytes(), Postgres, detect(), 5).unwrap(),
            vec![
                stmt("SELECT 1", 0),
                command("\\g", 11),
                stmt("SELECT 2", 14)
            ]
        );
        // The MySQL client's `\G` on its own line (refused later by the caller).
        let input = "select 1\n\\G\n";
        assert_eq!(
            split_all(input.as_bytes(), MYSQL, detect(), 1).unwrap(),
            vec![stmt("select 1", 0), command("\\G", 9)]
        );
        // Off: the line is ordinary text, as in `split`.
        assert_eq!(
            split_all(
                b"\\connect db\nSELECT 1;",
                Postgres,
                SplitOptions::default(),
                3
            )
            .unwrap(),
            vec![stmt("\\connect db\nSELECT 1", 0)]
        );
    }

    #[test]
    fn backslash_inside_string_is_not_a_command() {
        let input = "select '\n\\x';\nselect 1 /*\n\\q */;\nselect 2 \\x;\n";
        for chunk in CHUNKS {
            assert_eq!(
                split_all(input.as_bytes(), MYSQL, detect(), chunk).unwrap(),
                expected(input, MYSQL),
                "chunk {chunk}"
            );
            assert_eq!(
                split_all(input.as_bytes(), Postgres, detect(), chunk).unwrap(),
                expected(input, Postgres),
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn invalid_utf8_is_an_error() {
        let mut input = b"select 1;\nselect '".to_vec();
        input.extend([0xff, b'\'', b';']);
        for chunk in CHUNKS {
            let mut s = StreamSplitter::new(MYSQL, SplitOptions::default());
            let mut events = Vec::new();
            let mut err = None;
            for part in input.chunks(chunk) {
                s.push(part);
                loop {
                    match s.next_event() {
                        Ok(Some(e)) => events.push(e),
                        Ok(None) => break,
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                }
            }
            s.finish();
            if err.is_none() {
                loop {
                    match s.next_event() {
                        Ok(Some(e)) => events.push(e),
                        Ok(None) => break,
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                }
            }
            assert_eq!(events, vec![stmt("select 1", 0)], "chunk {chunk}");
            assert_eq!(err, Some(SplitError::InvalidUtf8 { offset: 18 }));
            // The error is sticky.
            assert_eq!(s.next_event(), Err(SplitError::InvalidUtf8 { offset: 18 }));
        }
        // A sequence cut by end of input is invalid too.
        assert_eq!(
            split_all(b"select '\xc3", MYSQL, SplitOptions::default(), 1),
            Err(SplitError::InvalidUtf8 { offset: 8 })
        );
        assert!(SplitError::InvalidUtf8 { offset: 8 }
            .to_string()
            .contains("UTF-8"));
    }

    #[test]
    fn oversized_statement_is_an_error() {
        let opts = SplitOptions {
            detect_client: false,
            max_statement_bytes: 16,
        };
        let input = "select 1;\nselect 'aaaaaaaaaaaaaaaaaaaaaaaa';\nselect 2;";
        for chunk in CHUNKS {
            assert_eq!(
                split_all(input.as_bytes(), MYSQL, opts, chunk),
                Err(SplitError::StatementTooLarge {
                    start: 10,
                    limit: 16
                }),
                "chunk {chunk}"
            );
        }
        // An unterminated string is caught without waiting for the end of input.
        let mut s = StreamSplitter::new(MYSQL, opts);
        s.push(b"select 'a\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(
            s.next_event(),
            Err(SplitError::StatementTooLarge {
                start: 0,
                limit: 16
            })
        );
        // Exactly at the limit is fine.
        let ok = split_all(b"select 123456789;", MYSQL, opts, 3).unwrap();
        assert_eq!(ok, vec![stmt("select 123456789", 0)]);
    }

    #[test]
    fn consumed_tracks_returned_bytes() {
        let mut s = StreamSplitter::new(MYSQL, SplitOptions::default());
        s.push(b"select 1;\nselect 2");
        assert_eq!(s.next_event().unwrap(), Some(stmt("select 1", 0)));
        assert_eq!(s.consumed(), 9);
        assert_eq!(s.next_event().unwrap(), None);
        assert!(!s.is_done());
        s.finish();
        assert_eq!(s.next_event().unwrap(), Some(stmt("select 2", 10)));
        assert_eq!(s.next_event().unwrap(), None);
        assert!(s.is_done());
        assert_eq!(s.consumed(), 18);
    }

    #[test]
    fn long_line_is_linear() {
        const LEN: usize = 64 << 20;
        let line = vec![b'x'; 64 << 10];
        let mut lines = line.clone();
        *lines.last_mut().unwrap() = b'\n';
        // One line; one string over many lines (rescanned only after doubling).
        for (dialect, open, chunk) in [
            (MYSQL, &b""[..], &line),
            (Postgres, &b"select '"[..], &line),
            (Postgres, &b"select '"[..], &lines),
        ] {
            let started = Instant::now();
            let mut s = StreamSplitter::new(dialect, SplitOptions::default());
            s.push(open);
            let mut pushed = 0;
            while pushed < LEN {
                s.push(chunk);
                pushed += chunk.len();
                assert_eq!(s.next_event().unwrap(), None);
            }
            s.push(if open.is_empty() { b";" } else { b"';" });
            s.finish();
            let Some(SplitEvent::Statement(st)) = s.next_event().unwrap() else {
                panic!("no statement");
            };
            assert_eq!(
                st.end - st.start,
                (LEN + open.len() + open.len().min(1)) as u64
            );
            assert_eq!(s.next_event().unwrap(), None);
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{dialect:?}: {:?}",
                started.elapsed()
            );
        }
    }

    #[test]
    fn many_small_statements_in_one_chunk_are_linear() {
        let input = "select 1;\n".repeat(200_000);
        let started = Instant::now();
        let got = split_all(input.as_bytes(), MYSQL, SplitOptions::default(), 1 << 22).unwrap();
        assert_eq!(got.len(), 200_000);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }
}
