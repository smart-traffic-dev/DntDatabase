//! Ephemeral, disk-backed query results.
//!
//! The database driver hands complete [`Value`] rows to a bounded writer. Committed
//! frames are indexed in memory and may be paged without retaining the complete result.

use crate::session_manager::lock;
use dnt_core::driver::{ResultSummary, RowSink};
use dnt_core::error::DbError;
use dnt_core::value::{ColumnMeta, Value};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use uuid::Uuid;

pub const RESULT_PAGE_ROWS: usize = 500;
pub const RESULT_PAGE_BYTES: usize = 4 * 1024 * 1024;
pub const RESULT_CELL_BYTES: usize = 64 * 1024;
const WRITER_QUEUE_FRAMES: usize = 8;
const FRAME_TARGET_BYTES: usize = 4 * 1024 * 1024;
const MAX_COPY_CELLS: usize = 100_000;

fn store_error(message: impl Into<String>) -> DbError {
    DbError::Internal {
        message: message.into(),
    }
}

fn io_error(action: &str, path: &Path, e: &std::io::Error) -> DbError {
    store_error(format!(
        "cannot {action} temporary query result {}: {e}",
        path.display()
    ))
}

#[derive(Debug, Clone)]
struct Frame {
    start: u64,
    rows: u32,
    offset: u64,
    len: u64,
}

#[derive(Debug, Default)]
struct EntryState {
    frames: Vec<Frame>,
    rows: u64,
    done: bool,
    /// Its statement ended before the result did: the committed rows stay readable,
    /// but an export of it fails rather than pass for complete.
    abandoned: bool,
    error: Option<DbError>,
    expired: bool,
}

#[derive(Debug)]
struct Entry {
    token: Uuid,
    connection: Uuid,
    result_id: u32,
    columns: Vec<ColumnMeta>,
    path: PathBuf,
    state: Mutex<EntryState>,
    changed: Condvar,
}

#[derive(Default)]
struct StoreState {
    root: PathBuf,
    entries: Mutex<HashMap<Uuid, Arc<Entry>>>,
    by_tab: Mutex<HashMap<String, HashSet<Uuid>>>,
    exports: Mutex<HashMap<Uuid, Arc<AtomicBool>>>,
    /// Locked while this process owns `root`, so another instance's startup cleanup skips it.
    _owner: Option<File>,
}

const OWNER_LOCK: &str = "owner.lock";

/// Runs `f`, which may block on local I/O or a full queue, without pinning a Tokio
/// worker of a multi-threaded runtime.
pub(crate) fn block_outside_executor<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// A directory younger than this may belong to a store that has not locked it yet.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// Removes result directories of earlier processes that no longer hold their owner lock.
fn remove_stale(base: &Path, min_age: std::time::Duration) {
    let Ok(dirs) = std::fs::read_dir(base) else {
        return;
    };
    for dir in dirs.flatten() {
        let path = dir.path();
        let young = dir
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_none_or(|age| age < min_age);
        if young {
            continue;
        }
        if !path.is_dir() {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let owned = File::open(path.join(OWNER_LOCK))
            .map(|file| file.try_lock().is_err())
            .unwrap_or(false);
        if owned {
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(&path) {
            tracing::warn!(path = %path.display(), error = %e, "cannot remove stale query results");
        }
    }
}

type Commit = Arc<dyn Fn(u32, u64, u32) + Send + Sync>;
type Fail = Arc<dyn Fn(DbError) + Send + Sync>;

enum WriteMessage {
    Start {
        entry: Arc<Entry>,
    },
    Frame {
        entry: Arc<Entry>,
        bytes: Vec<u8>,
        rows: u32,
        commit: Commit,
        fail: Fail,
    },
    End {
        entry: Arc<Entry>,
        reply: mpsc::Sender<Result<(), DbError>>,
    },
    /// After the frames queued before it: the result ended without being complete.
    Abandon {
        entry: Arc<Entry>,
    },
    Remove {
        paths: Vec<PathBuf>,
    },
}

/// Process-local result storage. Clone is cheap and shares one bounded writer thread.
#[derive(Clone)]
pub struct ResultStore {
    state: Arc<StoreState>,
    tx: SyncSender<WriteMessage>,
    _worker: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl ResultStore {
    /// Stores results in a fresh directory under `base`, first removing the directories
    /// that crashed earlier processes left there.
    pub fn new(base: PathBuf) -> Result<Self, DbError> {
        Self::with_stale_age(base, STALE_AFTER)
    }

    fn with_stale_age(base: PathBuf, min_age: std::time::Duration) -> Result<Self, DbError> {
        std::fs::create_dir_all(&base).map_err(|e| io_error("create", &base, &e))?;
        remove_stale(&base, min_age);
        let root = base.join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&root).map_err(|e| io_error("create", &root, &e))?;
        let lock_path = root.join(OWNER_LOCK);
        let owner = File::create(&lock_path).map_err(|e| io_error("create", &lock_path, &e))?;
        owner
            .try_lock()
            .map_err(|e| store_error(format!("cannot lock {}: {e}", lock_path.display())))?;
        let state = Arc::new(StoreState {
            root,
            _owner: Some(owner),
            ..StoreState::default()
        });
        let (tx, rx) = mpsc::sync_channel(WRITER_QUEUE_FRAMES);
        let worker_state = Arc::clone(&state);
        let worker = std::thread::Builder::new()
            .name("dnt-result-writer".into())
            .spawn(move || writer_loop(worker_state, rx))
            .map_err(|e| store_error(format!("cannot start query result writer: {e}")))?;
        Ok(ResultStore {
            state,
            tx,
            _worker: Arc::new(Mutex::new(Some(worker))),
        })
    }

    pub fn temporary() -> Result<Self, DbError> {
        Self::new(std::env::temp_dir().join("dntdatabase-results"))
    }

    /// Detaches the previous run of `tab` before a replacement run starts.
    pub fn begin_run(&self, tab: &str) {
        self.remove_tokens(lock(&self.state.by_tab).remove(tab).unwrap_or_default());
    }

    pub fn remove_tab(&self, tab: &str) {
        self.begin_run(tab);
    }

    pub fn remove_connection(&self, connection: Uuid) {
        let tokens: HashSet<Uuid> = lock(&self.state.entries)
            .iter()
            .filter(|(_, e)| e.connection == connection)
            .map(|(token, _)| *token)
            .collect();
        if tokens.is_empty() {
            return;
        }
        for values in lock(&self.state.by_tab).values_mut() {
            values.retain(|token| !tokens.contains(token));
        }
        self.remove_tokens(tokens);
    }

    fn remove_tokens(&self, tokens: HashSet<Uuid>) {
        if tokens.is_empty() {
            return;
        }
        let mut entries = lock(&self.state.entries);
        let paths: Vec<PathBuf> = tokens
            .into_iter()
            .filter_map(|token| {
                entries.remove(&token).map(|entry| {
                    lock(&entry.state).expired = true;
                    entry.changed.notify_all();
                    entry.path.clone()
                })
            })
            .collect();
        drop(entries);
        if !paths.is_empty() {
            let _ = self.tx.send(WriteMessage::Remove { paths });
        }
    }

    pub(crate) fn create_result(
        &self,
        connection: Uuid,
        tab: &str,
        _run: Uuid,
        result_id: u32,
        columns: Vec<ColumnMeta>,
    ) -> ResultToken {
        let token = Uuid::new_v4();
        let entry = Arc::new(Entry {
            token,
            connection,
            result_id,
            columns,
            path: self.state.root.join(format!("{token}.rows")),
            state: Mutex::new(EntryState::default()),
            changed: Condvar::new(),
        });
        lock(&self.state.entries).insert(token, Arc::clone(&entry));
        lock(&self.state.by_tab)
            .entry(tab.to_string())
            .or_default()
            .insert(token);
        if self.tx.send(WriteMessage::Start { entry }).is_err() {
            if let Some(entry) = lock(&self.state.entries).get(&token) {
                lock(&entry.state).error = Some(store_error("query result writer stopped"));
            }
        }
        ResultToken(token)
    }

    pub(crate) fn append(
        &self,
        token: ResultToken,
        rows: Vec<Vec<Value>>,
        commit: Commit,
        fail: Fail,
    ) {
        let Some(entry) = lock(&self.state.entries).get(&token.0).cloned() else {
            return;
        };
        for rows in split_rows(rows) {
            let count = u32::try_from(rows.len()).unwrap_or(u32::MAX);
            let bytes = codec::encode_rows(&rows);
            let message = WriteMessage::Frame {
                entry: Arc::clone(&entry),
                bytes,
                rows: count,
                commit: Arc::clone(&commit),
                fail: Arc::clone(&fail),
            };
            if block_outside_executor(|| self.tx.send(message)).is_err() {
                let error = store_error("query result writer stopped");
                lock(&entry.state).error = Some(error.clone());
                fail(error);
                return;
            }
        }
    }

    /// Marks a result whose statement ended before `finish` as incomplete, once the
    /// frames already queued for it are written. Never blocks.
    pub(crate) fn abandon(&self, token: ResultToken) {
        if let Ok(entry) = self.entry(token.0) {
            // An unbounded wait is not allowed here (drop paths): a full queue marks it now.
            match self.tx.try_send(WriteMessage::Abandon {
                entry: Arc::clone(&entry),
            }) {
                Ok(()) => {}
                Err(_) => {
                    lock(&entry.state).abandoned = true;
                    entry.changed.notify_all();
                }
            }
        }
    }

    pub(crate) fn finish(&self, token: ResultToken) -> Result<(), DbError> {
        // Detached while streaming: nothing reads it any more, which is not a run failure.
        let Ok(entry) = self.entry(token.0) else {
            return Ok(());
        };
        let (tx, rx) = mpsc::channel();
        block_outside_executor(|| {
            self.tx
                .send(WriteMessage::End { entry, reply: tx })
                .map_err(|_| store_error("query result writer stopped"))?;
            rx.recv()
                .map_err(|_| store_error("query result writer stopped"))?
        })
    }

    fn entry(&self, token: Uuid) -> Result<Arc<Entry>, DbError> {
        lock(&self.state.entries)
            .get(&token)
            .cloned()
            .ok_or_else(|| DbError::Config {
                field: Some("result".into()),
                message: "This query result is no longer available.".into(),
            })
    }

    pub fn page(&self, token: Uuid, offset: u64, limit: u32) -> Result<ResultPage, DbError> {
        let entry = self.entry(token)?;
        let limit = usize::try_from(limit.min(RESULT_PAGE_ROWS as u32)).unwrap_or(RESULT_PAGE_ROWS);
        let exact = read_range(&entry, offset, limit)?;
        let (rows, truncated_cells, bytes) = display_rows(exact);
        let state = lock(&entry.state);
        Ok(ResultPage {
            rows,
            truncated_cells,
            offset,
            committed_rows: state.rows,
            done: state.done,
            bytes,
        })
    }

    pub fn values(
        &self,
        token: Uuid,
        rows: &[u64],
        cols: &[u32],
    ) -> Result<Vec<Vec<Value>>, DbError> {
        let cells = rows.len().saturating_mul(cols.len());
        if cells > MAX_COPY_CELLS {
            return Err(DbError::Config {
                field: Some("copy".into()),
                message: format!("Copy at most {MAX_COPY_CELLS} cells at a time."),
            });
        }
        let entry = self.entry(token)?;
        let state = lock(&entry.state);
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        let row_count = state.rows;
        drop(state);
        if rows.iter().any(|&row| row >= row_count) {
            return Err(DbError::Config {
                field: Some("copy".into()),
                message: "The selected row is not available yet.".into(),
            });
        }
        if let Some(&col) = cols.iter().find(|&&c| c as usize >= entry.columns.len()) {
            return Err(DbError::Config {
                field: Some("copy".into()),
                message: format!("Column {col} is no longer available."),
            });
        }
        // Each frame is decoded once however many selected rows it holds.
        let mut wanted: Vec<u64> = rows.to_vec();
        wanted.sort_unstable();
        wanted.dedup();
        let mut found: HashMap<u64, Vec<Value>> = HashMap::with_capacity(wanted.len());
        for_each_frame(&entry, &wanted, |row, source| {
            let selected = cols
                .iter()
                .map(|&col| source.get(col as usize).cloned().unwrap_or(Value::Null))
                .collect();
            found.insert(row, selected);
        })?;
        rows.iter()
            .map(|row| {
                found
                    .get(row)
                    .cloned()
                    .ok_or_else(|| store_error("query result row is missing"))
            })
            .collect()
    }

    /// Streams every exact committed row to `sink` without materializing the result.
    pub fn stream(&self, token: Uuid, sink: &mut dyn RowSink) -> Result<u64, DbError> {
        self.stream_cancellable(token, sink, &AtomicBool::new(false))
    }

    /// Streams committed frames and follows a running result until it completes.
    pub fn stream_cancellable(
        &self,
        token: Uuid,
        sink: &mut dyn RowSink,
        cancelled: &AtomicBool,
    ) -> Result<u64, DbError> {
        let entry = self.entry(token)?;
        sink.result_start(&entry.columns);
        let mut file = None;
        let mut next = 0usize;
        let row_count = loop {
            if cancelled.load(Ordering::SeqCst) {
                return Err(DbError::cancelled());
            }
            let state = lock(&entry.state);
            if let Some(error) = &state.error {
                return Err(error.clone());
            }
            if state.expired {
                return Err(DbError::Config {
                    field: Some("result".into()),
                    message: "This query result is no longer available.".into(),
                });
            }
            let frame = if let Some(frame) = state.frames.get(next).cloned() {
                next += 1;
                drop(state);
                frame
            } else if state.done {
                break state.rows;
            } else if state.abandoned {
                return Err(store_error(
                    "The statement ended before this result was complete.",
                ));
            } else {
                let _ = entry
                    .changed
                    .wait_timeout(state, std::time::Duration::from_millis(100))
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                continue;
            };
            sink.rows(read_frame(&entry, &mut file, &frame)?);
        };
        sink.result_end(&ResultSummary {
            affected: None,
            row_count,
            truncated: false,
            elapsed_ms: 0,
        });
        Ok(row_count)
    }

    pub fn begin_export(&self, operation: Uuid) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(old) = lock(&self.state.exports).insert(operation, Arc::clone(&cancel)) {
            old.store(true, Ordering::SeqCst);
        }
        cancel
    }

    pub fn cancel_export(&self, operation: Uuid) -> bool {
        let Some(cancel) = lock(&self.state.exports).get(&operation).cloned() else {
            return false;
        };
        cancel.store(true, Ordering::SeqCst);
        true
    }

    pub fn finish_export(&self, operation: Uuid) {
        lock(&self.state.exports).remove(&operation);
    }

    pub fn find(&self, token: Uuid, query: &str, max: u32) -> Result<ResultSearch, DbError> {
        let needle = query.to_lowercase();
        if needle.is_empty() {
            return Ok(ResultSearch {
                matches: Vec::new(),
                capped: false,
            });
        }
        let entry = self.entry(token)?;
        let state = lock(&entry.state);
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        let frames = state.frames.clone();
        drop(state);
        let max = max.clamp(1, 10_000) as usize;
        let mut file = None;
        let mut matches = Vec::new();
        for frame in frames {
            let rows = read_frame(&entry, &mut file, &frame)?;
            for (r, row) in rows.iter().enumerate() {
                for (c, value) in row.iter().enumerate() {
                    if search_text(value).to_lowercase().contains(&needle) {
                        if matches.len() >= max {
                            return Ok(ResultSearch {
                                matches,
                                capped: true,
                            });
                        }
                        matches.push(ResultMatch {
                            row: frame.start + r as u64,
                            col: c as u32,
                        });
                    }
                }
            }
        }
        Ok(ResultSearch {
            matches,
            capped: false,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResultToken(pub Uuid);

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ResultPage {
    pub rows: Vec<Vec<Value>>,
    /// `(row within page, column)` of display values that are previews.
    pub truncated_cells: Vec<(u32, u32)>,
    #[specta(type = f64)]
    pub offset: u64,
    #[specta(type = f64)]
    pub committed_rows: u64,
    pub done: bool,
    #[specta(type = f64)]
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ResultMatch {
    #[specta(type = f64)]
    pub row: u64,
    pub col: u32,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ResultSearch {
    pub matches: Vec<ResultMatch>,
    pub capped: bool,
}

fn writer_loop(state: Arc<StoreState>, rx: Receiver<WriteMessage>) {
    let mut files: HashMap<Uuid, File> = HashMap::new();
    while let Ok(message) = rx.recv() {
        match message {
            WriteMessage::Start { entry } => match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&entry.path)
            {
                Ok(file) => {
                    files.insert(entry.token, file);
                }
                Err(e) => lock(&entry.state).error = Some(io_error("create", &entry.path, &e)),
            },
            WriteMessage::Frame {
                entry,
                bytes,
                rows,
                commit,
                fail,
            } => {
                let result = write_frame(&entry, &mut files, &bytes, rows);
                match result {
                    Ok(Some(start)) => commit(entry.result_id, start, rows),
                    // Detached (rerun, tab or connection closed): the rows are not wanted.
                    Ok(None) => {}
                    Err(error) => {
                        lock(&entry.state).error = Some(error.clone());
                        entry.changed.notify_all();
                        fail(error);
                    }
                }
            }
            WriteMessage::Abandon { entry } => {
                files.remove(&entry.token);
                lock(&entry.state).abandoned = true;
                entry.changed.notify_all();
            }
            WriteMessage::End { entry, reply } => {
                let (error, expired) = {
                    let state = lock(&entry.state);
                    (state.error.clone(), state.expired)
                };
                // The result is complete: its write handle is no longer needed.
                let file = files.remove(&entry.token);
                let result = match (error, expired) {
                    (Some(error), _) => Err(error),
                    (None, true) => Ok(()),
                    (None, false) => match file {
                        Some(mut file) => {
                            file.flush().map_err(|e| io_error("flush", &entry.path, &e))
                        }
                        None => Err(store_error("query result file was not opened")),
                    },
                };
                if result.is_ok() {
                    lock(&entry.state).done = true;
                }
                entry.changed.notify_all();
                let _ = reply.send(result);
            }
            WriteMessage::Remove { paths } => {
                for path in paths {
                    if let Some(token) = token_from_path(&path) {
                        files.remove(&token);
                    }
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
    drop(files);
    let _ = std::fs::remove_dir_all(&state.root);
}

fn token_from_path(path: &Path) -> Option<Uuid> {
    path.file_stem()?.to_str()?.parse().ok()
}

fn write_frame(
    entry: &Entry,
    files: &mut HashMap<Uuid, File>,
    bytes: &[u8],
    rows: u32,
) -> Result<Option<u64>, DbError> {
    {
        let state = lock(&entry.state);
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        if state.expired {
            return Ok(None);
        }
    }
    let file = files
        .get_mut(&entry.token)
        .ok_or_else(|| store_error("query result file was not opened"))?;
    let offset = file
        .stream_position()
        .map_err(|e| io_error("seek", &entry.path, &e))?;
    let len =
        u64::try_from(bytes.len()).map_err(|_| store_error("query result frame is too large"))?;
    file.write_all(&len.to_le_bytes())
        .and_then(|_| file.write_all(bytes))
        .and_then(|_| file.flush())
        .map_err(|e| io_error("write", &entry.path, &e))?;
    let mut state = lock(&entry.state);
    let start = state.rows;
    state.frames.push(Frame {
        start,
        rows,
        offset,
        len,
    });
    state.rows = state.rows.saturating_add(u64::from(rows));
    drop(state);
    entry.changed.notify_all();
    Ok(Some(start))
}

fn split_rows(rows: Vec<Vec<Value>>) -> Vec<Vec<Vec<Value>>> {
    let mut out = Vec::new();
    let mut batch = Vec::new();
    let mut bytes = 0usize;
    for row in rows {
        let size = row
            .iter()
            .map(value_bytes)
            .sum::<usize>()
            .saturating_add(64);
        if !batch.is_empty() && bytes.saturating_add(size) > FRAME_TARGET_BYTES {
            out.push(std::mem::take(&mut batch));
            bytes = 0;
        }
        bytes = bytes.saturating_add(size);
        batch.push(row);
    }
    if !batch.is_empty() {
        out.push(batch);
    }
    out
}

/// Decodes one committed frame, opening the result file on first use.
fn read_frame(
    entry: &Entry,
    file: &mut Option<File>,
    frame: &Frame,
) -> Result<Vec<Vec<Value>>, DbError> {
    let file = match file {
        Some(file) => file,
        None => {
            file.insert(File::open(&entry.path).map_err(|e| io_error("open", &entry.path, &e))?)
        }
    };
    file.seek(SeekFrom::Start(frame.offset + 8))
        .map_err(|e| io_error("seek", &entry.path, &e))?;
    let size =
        usize::try_from(frame.len).map_err(|_| store_error("query result frame is too large"))?;
    let mut bytes = vec![0; size];
    file.read_exact(&mut bytes)
        .map_err(|e| io_error("read", &entry.path, &e))?;
    let rows = codec::decode_rows(&bytes)
        .ok_or_else(|| store_error("temporary query result frame is corrupt"))?;
    if rows.len() != frame.rows as usize {
        return Err(store_error("temporary query result frame is corrupt"));
    }
    Ok(rows)
}

/// Calls `f` with every row of `wanted` (sorted, unique), decoding each frame once.
fn for_each_frame(
    entry: &Entry,
    wanted: &[u64],
    mut f: impl FnMut(u64, &[Value]),
) -> Result<(), DbError> {
    let frames = lock(&entry.state).frames.clone();
    let mut file = None;
    let mut next = 0usize;
    for frame in frames {
        if next >= wanted.len() {
            break;
        }
        let frame_end = frame.start.saturating_add(u64::from(frame.rows));
        if wanted[next] >= frame_end {
            continue;
        }
        let rows = read_frame(entry, &mut file, &frame)?;
        while next < wanted.len() && wanted[next] < frame_end {
            let row = wanted[next];
            if let Some(source) = usize::try_from(row - frame.start)
                .ok()
                .and_then(|i| rows.get(i))
            {
                f(row, source);
            }
            next += 1;
        }
    }
    Ok(())
}

fn read_range(entry: &Entry, offset: u64, limit: usize) -> Result<Vec<Vec<Value>>, DbError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let frames = {
        let state = lock(&entry.state);
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        state.frames.clone()
    };
    let end = offset.saturating_add(limit as u64);
    let mut file = None;
    let mut out = Vec::with_capacity(limit);
    for frame in frames {
        let frame_end = frame.start.saturating_add(u64::from(frame.rows));
        if frame_end <= offset || frame.start >= end {
            continue;
        }
        let rows = read_frame(entry, &mut file, &frame)?;
        let from = usize::try_from(offset.saturating_sub(frame.start)).unwrap_or(usize::MAX);
        let to = usize::try_from(end.saturating_sub(frame.start))
            .unwrap_or(usize::MAX)
            .min(rows.len());
        if from < to {
            out.extend(rows.into_iter().skip(from).take(to - from));
        }
    }
    Ok(out)
}

/// Approximate payload of `value` in bytes (queue and page budgets).
pub(crate) fn value_bytes(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::UInt(_) | Value::Float(_) => 8,
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Json(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::TimestampTz(s) => s.len(),
        Value::Bytes { preview, .. } => preview.len(),
        Value::Array(items) => items.iter().map(value_bytes).sum(),
        Value::Other { type_name, text } => type_name.len() + text.len(),
    }
}

fn search_text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Bool(value) => value.to_string(),
        Value::Int(value) => value.to_string(),
        Value::UInt(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Decimal(value)
        | Value::Text(value)
        | Value::Json(value)
        | Value::Uuid(value)
        | Value::Date(value)
        | Value::Time(value)
        | Value::DateTime(value)
        | Value::TimestampTz(value) => value.clone(),
        Value::Bytes { len, preview } => {
            let hex: String = preview.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("0x{hex} ({len} bytes)")
        }
        Value::Array(items) => format!(
            "{{{}}}",
            items.iter().map(search_text).collect::<Vec<_>>().join(",")
        ),
        Value::Other { text, .. } => text.clone(),
    }
}

fn cut_string(mut value: String, cap: usize) -> (String, bool) {
    if value.len() <= cap {
        return (value, false);
    }
    let suffix = "…";
    let target = cap.saturating_sub(suffix.len());
    let mut end = target.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    if cap >= suffix.len() {
        value.push_str(suffix);
    }
    (value, true)
}

fn display_value(value: Value, cap: usize) -> (Value, bool) {
    match value {
        Value::Text(s) => {
            let (s, cut) = cut_string(s, cap);
            (Value::Text(s), cut)
        }
        Value::Json(s) => {
            let (s, cut) = cut_string(s, cap);
            (Value::Json(s), cut)
        }
        Value::Other { type_name, text } => {
            let (text, cut) = cut_string(text, cap);
            (Value::Other { type_name, text }, cut)
        }
        // The grid shows binary data as a short preview and its length, as before.
        Value::Bytes { len, preview } => {
            let shown = Value::bytes(&preview[..preview.len().min(cap)]);
            let cut = matches!(&shown, Value::Bytes { preview: p, .. } if p.len() < preview.len());
            match shown {
                Value::Bytes { preview, .. } => (Value::Bytes { len, preview }, cut),
                _ => unreachable!(),
            }
        }
        Value::Array(items) => {
            let each = cap.checked_div(items.len().max(1)).unwrap_or(0);
            let mut cut = false;
            let items = items
                .into_iter()
                .map(|item| {
                    let (item, item_cut) = display_value(item, each);
                    cut |= item_cut;
                    item
                })
                .collect();
            (Value::Array(items), cut)
        }
        value => (value, false),
    }
}

fn display_rows(rows: Vec<Vec<Value>>) -> (Vec<Vec<Value>>, Vec<(u32, u32)>, usize) {
    let total_cells = rows.iter().map(Vec::len).sum::<usize>().max(1);
    let dynamic_cap = (RESULT_PAGE_BYTES / total_cells).min(RESULT_CELL_BYTES);
    let mut truncated = Vec::new();
    let mut bytes = 0usize;
    let rows = rows
        .into_iter()
        .enumerate()
        .map(|(r, row)| {
            row.into_iter()
                .enumerate()
                .map(|(c, value)| {
                    let (value, cut) = display_value(value, dynamic_cap);
                    if cut {
                        truncated.push((r as u32, c as u32));
                    }
                    bytes = bytes.saturating_add(value_bytes(&value));
                    value
                })
                .collect()
        })
        .collect();
    (rows, truncated, bytes)
}

/// The exact on-disk row encoding. JSON is not used: it cannot carry a NaN or an
/// infinite float.
mod codec {
    use dnt_core::value::Value;

    fn put_len(out: &mut Vec<u8>, n: usize) {
        out.extend_from_slice(&(n as u64).to_le_bytes());
    }

    fn put_str(out: &mut Vec<u8>, s: &str) {
        put_len(out, s.len());
        out.extend_from_slice(s.as_bytes());
    }

    fn put_value(out: &mut Vec<u8>, value: &Value) {
        let text = |out: &mut Vec<u8>, tag: u8, s: &str| {
            out.push(tag);
            put_str(out, s);
        };
        match value {
            Value::Null => out.push(0),
            Value::Bool(b) => out.extend_from_slice(&[1, u8::from(*b)]),
            Value::Int(n) => {
                out.push(2);
                out.extend_from_slice(&n.to_le_bytes());
            }
            Value::UInt(n) => {
                out.push(3);
                out.extend_from_slice(&n.to_le_bytes());
            }
            Value::Float(f) => {
                out.push(4);
                out.extend_from_slice(&f.to_bits().to_le_bytes());
            }
            Value::Decimal(s) => text(out, 5, s),
            Value::Text(s) => text(out, 6, s),
            Value::Json(s) => text(out, 7, s),
            Value::Uuid(s) => text(out, 8, s),
            Value::Bytes { len, preview } => {
                out.push(9);
                out.extend_from_slice(&len.to_le_bytes());
                put_len(out, preview.len());
                out.extend_from_slice(preview);
            }
            Value::Date(s) => text(out, 10, s),
            Value::Time(s) => text(out, 11, s),
            Value::DateTime(s) => text(out, 12, s),
            Value::TimestampTz(s) => text(out, 13, s),
            Value::Array(items) => {
                out.push(14);
                put_len(out, items.len());
                for item in items {
                    put_value(out, item);
                }
            }
            Value::Other { type_name, text } => {
                out.push(15);
                put_str(out, type_name);
                put_str(out, text);
            }
        }
    }

    pub(super) fn encode_rows(rows: &[Vec<Value>]) -> Vec<u8> {
        let mut out = Vec::new();
        put_len(&mut out, rows.len());
        for row in rows {
            put_len(&mut out, row.len());
            for value in row {
                put_value(&mut out, value);
            }
        }
        out
    }

    struct Reader<'a>(&'a [u8]);

    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            if self.0.len() < n {
                return None;
            }
            let (head, rest) = self.0.split_at(n);
            self.0 = rest;
            Some(head)
        }
        fn u8(&mut self) -> Option<u8> {
            self.take(1).map(|b| b[0])
        }
        fn u64(&mut self) -> Option<u64> {
            Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
        }
        fn len(&mut self) -> Option<usize> {
            let n = usize::try_from(self.u64()?).ok()?;
            // A count never exceeds the bytes left: a corrupt length cannot over-allocate.
            (n <= self.0.len()).then_some(n)
        }
        fn string(&mut self) -> Option<String> {
            let n = self.len()?;
            String::from_utf8(self.take(n)?.to_vec()).ok()
        }
        fn value(&mut self) -> Option<Value> {
            Some(match self.u8()? {
                0 => Value::Null,
                1 => Value::Bool(self.u8()? != 0),
                2 => Value::Int(self.u64()? as i64),
                3 => Value::UInt(self.u64()?),
                4 => Value::Float(f64::from_bits(self.u64()?)),
                5 => Value::Decimal(self.string()?),
                6 => Value::Text(self.string()?),
                7 => Value::Json(self.string()?),
                8 => Value::Uuid(self.string()?),
                9 => {
                    let len = self.u64()?;
                    let n = self.len()?;
                    Value::Bytes {
                        len,
                        preview: self.take(n)?.to_vec(),
                    }
                }
                10 => Value::Date(self.string()?),
                11 => Value::Time(self.string()?),
                12 => Value::DateTime(self.string()?),
                13 => Value::TimestampTz(self.string()?),
                14 => {
                    let n = self.len()?;
                    let mut items = Vec::with_capacity(n);
                    for _ in 0..n {
                        items.push(self.value()?);
                    }
                    Value::Array(items)
                }
                15 => Value::Other {
                    type_name: self.string()?,
                    text: self.string()?,
                },
                _ => return None,
            })
        }
    }

    pub(super) fn decode_rows(bytes: &[u8]) -> Option<Vec<Vec<Value>>> {
        let mut r = Reader(bytes);
        let count = r.len()?;
        let mut rows = Vec::with_capacity(count);
        for _ in 0..count {
            let cols = r.len()?;
            let mut row = Vec::with_capacity(cols);
            for _ in 0..cols {
                row.push(r.value()?);
            }
            rows.push(row);
        }
        r.0.is_empty().then_some(rows)
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
            nullable: None,
            origin_table: None,
            origin_column: None,
        }
    }

    fn ignore() -> Commit {
        Arc::new(|_, _, _| {})
    }

    fn panic_on_fail() -> Fail {
        Arc::new(|e| panic!("{e}"))
    }

    #[test]
    fn codec_round_trips_every_value() {
        let row = vec![
            Value::Null,
            Value::Bool(true),
            Value::Int(i64::MIN),
            Value::UInt(u64::MAX),
            Value::Float(f64::NAN),
            Value::Float(f64::NEG_INFINITY),
            Value::Float(-0.0),
            Value::Decimal("1.50".into()),
            Value::Text("ä\u{0}x".into()),
            Value::Json("{}".into()),
            Value::Uuid("u".into()),
            Value::Bytes {
                len: 3,
                preview: vec![0, 255, 7],
            },
            Value::Date("d".into()),
            Value::Time("t".into()),
            Value::DateTime("dt".into()),
            Value::TimestampTz("tz".into()),
            Value::Array(vec![Value::Int(1), Value::Array(vec![Value::Null])]),
            Value::Other {
                type_name: "point".into(),
                text: "(1,2)".into(),
            },
        ];
        let decoded = codec::decode_rows(&codec::encode_rows(std::slice::from_ref(&row))).unwrap();
        // NaN != NaN: compare the bits.
        let bits = |v: &Value| match v {
            Value::Float(f) => Some(f.to_bits()),
            _ => None,
        };
        for (a, b) in row.iter().zip(&decoded[0]) {
            match bits(a) {
                Some(x) => assert_eq!(Some(x), bits(b)),
                None => assert_eq!(a, b),
            }
        }
        let mut bytes = codec::encode_rows(&[row]);
        bytes.pop();
        assert!(
            codec::decode_rows(&bytes).is_none(),
            "a partial frame is rejected"
        );
    }

    #[test]
    fn rows_span_frames_and_copy_reads_each_exactly() {
        let (_dir, store) = store();
        let token = store.create_result(Uuid::nil(), "tab", Uuid::new_v4(), 0, vec![col("a")]);
        // ~9 MiB: at least three frames.
        let rows: Vec<Vec<Value>> = (0..300)
            .map(|i| vec![Value::Text(format!("{i:05}{}", "y".repeat(32 * 1024)))])
            .collect();
        store.append(token, rows.clone(), ignore(), panic_on_fail());
        store.finish(token).unwrap();
        let frames = lock(&store.entry(token.0).unwrap().state).frames.len();
        assert!(frames >= 3, "{frames} frames");
        let page = store.page(token.0, 120, 500).unwrap();
        assert_eq!(page.rows.len(), 180);
        assert_eq!(page.committed_rows, 300);
        assert!(page.done);
        let exact = store.values(token.0, &[299, 0, 150, 0], &[0]).unwrap();
        assert_eq!(exact[0][0], rows[299][0]);
        assert_eq!(exact[1][0], rows[0][0]);
        assert_eq!(exact[2][0], rows[150][0]);
        assert_eq!(exact[3][0], rows[0][0]);
        let found = store.find(token.0, "00299", 10).unwrap();
        assert_eq!(found.matches.len(), 1);
        assert_eq!(found.matches[0].row, 299);
        assert!(store.values(token.0, &[300], &[0]).is_err());
        assert!(store.values(token.0, &[0], &[1]).is_err());
    }

    #[test]
    fn stream_follows_a_running_result_and_stops_on_cancel() {
        struct Count(u64);
        impl RowSink for Count {
            fn result_start(&mut self, _: &[ColumnMeta]) {}
            fn rows(&mut self, batch: Vec<Vec<Value>>) {
                self.0 += batch.len() as u64;
            }
            fn result_end(&mut self, _: &ResultSummary) {}
        }
        let (_dir, store) = store();
        let token = store.create_result(Uuid::nil(), "tab", Uuid::new_v4(), 0, vec![col("a")]);
        store.append(token, vec![vec![Value::Int(1)]], ignore(), panic_on_fail());
        let reader = store.clone();
        let handle = std::thread::spawn(move || {
            let mut sink = Count(0);
            reader.stream(token.0, &mut sink).map(|n| (n, sink.0))
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        store.append(
            token,
            vec![vec![Value::Int(2)]; 4],
            ignore(),
            panic_on_fail(),
        );
        store.finish(token).unwrap();
        assert_eq!(handle.join().unwrap().unwrap(), (5, 5));

        let token = store.create_result(Uuid::nil(), "tab2", Uuid::new_v4(), 0, vec![col("a")]);
        let cancelled = AtomicBool::new(true);
        let err = store
            .stream_cancellable(token.0, &mut Count(0), &cancelled)
            .unwrap_err();
        assert!(matches!(err, DbError::Cancelled { .. }), "{err:?}");
    }

    #[test]
    fn closing_a_connection_removes_its_files_and_startup_removes_stale_ones() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("results");
        let stale = base.join("old-process");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("x.rows"), b"x").unwrap();
        // A young directory may be a store that has not locked it yet: kept.
        let first = ResultStore::new(base.clone()).unwrap();
        assert!(stale.exists());
        drop(first);
        let store = ResultStore::with_stale_age(base.clone(), std::time::Duration::ZERO).unwrap();
        assert!(!stale.exists(), "an unowned directory is removed");
        // A live store's directory survives another store's startup.
        let other = ResultStore::with_stale_age(base.clone(), std::time::Duration::ZERO).unwrap();
        assert!(store.state.root.exists());
        drop(other);

        let conn = Uuid::new_v4();
        let token = store.create_result(conn, "tab", Uuid::new_v4(), 0, vec![col("a")]);
        store.append(token, vec![vec![Value::Int(1)]], ignore(), panic_on_fail());
        store.finish(token).unwrap();
        let path = store.entry(token.0).unwrap().path.clone();
        assert!(path.exists());
        store.remove_connection(conn);
        assert!(store.page(token.0, 0, 1).is_err());
        // The writer removes the file asynchronously, in order.
        let (tx, rx) = mpsc::channel();
        let probe = store.create_result(conn, "probe", Uuid::new_v4(), 0, vec![]);
        store
            .tx
            .send(WriteMessage::End {
                entry: store.entry(probe.0).unwrap(),
                reply: tx,
            })
            .unwrap();
        rx.recv().unwrap().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn an_abandoned_result_stays_readable_but_never_exports_as_complete() {
        struct Count(u64);
        impl RowSink for Count {
            fn result_start(&mut self, _: &[ColumnMeta]) {}
            fn rows(&mut self, batch: Vec<Vec<Value>>) {
                self.0 += batch.len() as u64;
            }
            fn result_end(&mut self, _: &ResultSummary) {}
        }
        let (_dir, store) = store();
        let token = store.create_result(Uuid::nil(), "tab", Uuid::new_v4(), 0, vec![col("a")]);
        store.append(
            token,
            vec![vec![Value::Int(1)]; 3],
            ignore(),
            panic_on_fail(),
        );
        store.abandon(token);
        let mut sink = Count(0);
        assert!(store.stream(token.0, &mut sink).is_err());
        assert_eq!(sink.0, 3, "the committed rows were read first");
        assert_eq!(store.page(token.0, 0, 10).unwrap().rows.len(), 3);
    }

    #[test]
    fn rows_written_after_detach_are_dropped_without_failing_the_run() {
        let (_dir, store) = store();
        let token = store.create_result(Uuid::nil(), "tab", Uuid::new_v4(), 0, vec![col("a")]);
        store.remove_tab("tab");
        store.append(token, vec![vec![Value::Int(1)]], ignore(), panic_on_fail());
        store.finish(token).unwrap();
    }

    fn store() -> (tempfile::TempDir, ResultStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ResultStore::new(dir.path().join("results")).unwrap();
        (dir, store)
    }

    #[test]
    fn exact_values_survive_clipped_pages_and_frame_boundaries() {
        let (_dir, store) = store();
        let token = store.create_result(Uuid::nil(), "tab", Uuid::new_v4(), 0, vec![col("a")]);
        let committed = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&committed);
        store.append(
            token,
            vec![
                vec![Value::Text("x".repeat(RESULT_CELL_BYTES + 10))],
                vec![Value::Bytes {
                    len: 4,
                    preview: vec![1, 2, 3, 4],
                }],
            ],
            Arc::new(move |_, start, rows| lock(&seen).push((start, rows))),
            Arc::new(|e| panic!("{e}")),
        );
        store.finish(token).unwrap();
        assert_eq!(&*lock(&committed), &[(0, 2)]);
        let page = store.page(token.0, 0, 500).unwrap();
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.truncated_cells, [(0, 0)]);
        assert!(page.bytes <= RESULT_PAGE_BYTES);
        let exact = store.values(token.0, &[0, 1], &[0]).unwrap();
        assert_eq!(exact[0][0], Value::Text("x".repeat(RESULT_CELL_BYTES + 10)));
        assert_eq!(
            exact[1][0],
            Value::Bytes {
                len: 4,
                preview: vec![1, 2, 3, 4]
            }
        );
    }

    #[test]
    fn replacement_run_expires_old_tokens() {
        let (_dir, store) = store();
        let token = store.create_result(Uuid::nil(), "tab", Uuid::new_v4(), 0, vec![]);
        store.begin_run("tab");
        assert!(store.page(token.0, 0, 1).is_err());
    }
}
