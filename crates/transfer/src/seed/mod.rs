//! The data seeder's engine (data seeder spec §4): a seeded PRNG, typed value generators,
//! fake English data, the inference of a generator from a column, and the row engine.
//!
//! Pure: nothing here talks to a database or builds SQL. Values become SQL only through
//! `dnt_core::sql::build` in the caller.

pub mod engine;
pub mod fake;
pub mod generator;
pub mod infer;
pub mod limits;
pub mod rng;
pub mod words;

pub use generator::{ColumnPlan, FakeKind, Generator};

use serde::{Deserialize, Serialize};
use specta::Type;

/// A problem with one column's generator, shown under that column in the dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ColumnIssue {
    pub column: String,
    pub message: String,
}
