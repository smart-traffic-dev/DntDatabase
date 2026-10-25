//! SQL script handling: dialect-aware statement splitting and statement classification.

pub mod build;
mod classify;
pub mod ddl;
mod effect;
pub mod filter;
pub mod ident;
mod lexer;
mod multi;
mod split;
pub mod stream;

pub use classify::{classify, StatementKind, TxControl};
pub use effect::{session_effect, SessionEffect};
pub use lexer::{
    is_ident_byte, leading_keywords, readings, tokenize, Dialect, MySqlMode, TokKind, Token,
};
pub use multi::{resplit_reading, resplits};
pub use split::{leading_blank_len, split, strip_comments, Statement};
pub use stream::{
    is_copy_from_stdin, split_all, SplitError, SplitEvent, SplitOptions, StreamSplitter,
    StreamStatement,
};
