//! Multi-row `INSERT` batches (spec §6.3, R19). A batch is byte for byte
//! `build::insert_sql_with` of its rows: head + row tuples joined with `", "` + tail, built only
//! with the core builder.

use std::collections::HashSet;

use dnt_core::sql::build::{
    insert_head, insert_tail, literal, row_tuple, BuildError, ColumnTarget, InsertOptions,
    OnConflict, TableRef,
};
use dnt_core::sql::Dialect;
use dnt_core::value::FullValue;

/// Rows per batch by default.
pub const DEFAULT_BATCH_ROWS: u32 = 1000;
/// Statement size per batch by default (1 MiB).
pub const DEFAULT_BATCH_BYTES: usize = 1024 * 1024;

/// One `INSERT` ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub sql: String,
    pub rows: u32,
    /// Source record number of the first row.
    pub first_record: u64,
    /// Source record number of every row, in order.
    pub records: Vec<u64>,
}

/// Collects rows into batches of at most `max_rows` rows and `max_bytes` bytes of SQL. A row
/// that alone exceeds `max_bytes` is sent as its own batch.
///
/// PostgreSQL upsert: one `INSERT … ON CONFLICT DO UPDATE` cannot affect the same row twice, so
/// a row whose key repeats a key already in the pending batch starts a new batch (a file with a
/// repeated key is imported in file order, the later row winning, as on MySQL).
///
/// A batch is returned by the [`push`](BatchBuilder::push) of the first row that no longer fits
/// it (so a full batch waits for the next row or [`flush`](BatchBuilder::flush)); `push` returns
/// at most one batch.
#[derive(Debug)]
pub struct BatchBuilder {
    cols: Vec<ColumnTarget>,
    dialect: Dialect,
    max_rows: u32,
    max_bytes: usize,
    head: String,
    tail: String,
    tuples: String,
    rows: u32,
    records: Vec<u64>,
    /// PostgreSQL upsert: positions of the key columns in `cols`; empty otherwise.
    key_at: Vec<usize>,
    /// Key literals of the pending rows (PostgreSQL upsert only).
    keys: HashSet<Vec<String>>,
}

impl BatchBuilder {
    /// Fails when the options do not build (e.g. `OVERRIDING SYSTEM VALUE` on MySQL, an upsert
    /// column that is not inserted). `max_rows` 0 counts as 1.
    pub fn new(
        table: TableRef,
        cols: Vec<ColumnTarget>,
        opts: InsertOptions,
        dialect: Dialect,
        max_rows: u32,
        max_bytes: usize,
    ) -> Result<BatchBuilder, BuildError> {
        let head = insert_head(&table, &cols, &opts, dialect)?;
        let tail = insert_tail(&cols, &opts, dialect)?;
        let key_at = match (&opts.on_conflict, dialect) {
            (OnConflict::Update { key, .. }, Dialect::Postgres) => key
                .iter()
                .filter_map(|k| cols.iter().position(|c| &c.name == k))
                .collect(),
            _ => Vec::new(),
        };
        Ok(BatchBuilder {
            cols,
            dialect,
            max_rows: max_rows.max(1),
            max_bytes,
            head,
            tail,
            tuples: String::new(),
            rows: 0,
            records: Vec::new(),
            key_at,
            keys: HashSet::new(),
        })
    }

    /// Adds `row` (source record `record`). Returns the previous batch when this row does not
    /// fit in it. A row whose literals do not build is refused and leaves the batch unchanged.
    pub fn push(&mut self, row: Vec<FullValue>, record: u64) -> Result<Option<Batch>, BuildError> {
        let tuple = row_tuple(&self.cols, &row, self.dialect)?;
        let key = self.key_of(&row)?;
        let repeated = key.as_ref().is_some_and(|k| self.keys.contains(k));
        let out = if self.rows > 0
            && (repeated || self.rows >= self.max_rows || self.size_with(&tuple) > self.max_bytes)
        {
            self.take()
        } else {
            None
        };
        if self.rows > 0 {
            self.tuples.push_str(", ");
        }
        self.tuples.push_str(&tuple);
        self.rows += 1;
        self.records.push(record);
        if let Some(k) = key {
            self.keys.insert(k);
        }
        Ok(out)
    }

    /// The last, partly filled batch, if any row is waiting.
    pub fn flush(&mut self) -> Option<Batch> {
        self.take()
    }

    /// The row's key literals (PostgreSQL upsert); `None` without a key or with a NULL in it
    /// (NULLs never conflict).
    fn key_of(&self, row: &[FullValue]) -> Result<Option<Vec<String>>, BuildError> {
        if self.key_at.is_empty() || self.key_at.iter().any(|&i| row[i].is_null()) {
            return Ok(None);
        }
        self.key_at
            .iter()
            .map(|&i| literal(&row[i], &self.cols[i], self.dialect))
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    /// Statement length with `tuple` appended to the current rows.
    fn size_with(&self, tuple: &str) -> usize {
        let sep = if self.rows > 0 { 2 } else { 0 };
        self.head.len() + self.tuples.len() + sep + tuple.len() + self.tail.len()
    }

    fn take(&mut self) -> Option<Batch> {
        if self.rows == 0 {
            return None;
        }
        let mut sql = String::with_capacity(self.head.len() + self.tuples.len() + self.tail.len());
        sql.push_str(&self.head);
        sql.push_str(&self.tuples);
        sql.push_str(&self.tail);
        self.tuples.clear();
        self.keys.clear();
        let records = std::mem::take(&mut self.records);
        let rows = std::mem::replace(&mut self.rows, 0);
        Some(Batch {
            sql,
            rows,
            first_record: records[0],
            records,
        })
    }
}
