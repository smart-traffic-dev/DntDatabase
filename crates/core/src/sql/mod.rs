//! SQL script handling: dialect-aware statement splitting and statement classification.

mod classify;
mod lexer;
mod split;

pub use classify::{classify, StatementKind, TxControl};
pub use lexer::{
    is_ident_byte, leading_keywords, readings, tokenize, Dialect, MySqlMode, TokKind, Token,
};
pub use split::{split, strip_comments, Statement};
