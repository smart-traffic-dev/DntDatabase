//! SQL script handling: dialect-aware statement splitting and statement classification.

mod classify;
mod split;

pub use classify::{classify, StatementKind, TxControl};
pub use split::{split, strip_comments, Dialect, Statement};
