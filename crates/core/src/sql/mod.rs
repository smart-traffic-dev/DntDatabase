//! SQL script handling: dialect-aware statement splitting and statement classification.

mod classify;
mod effect;
pub mod ident;
mod lexer;
mod multi;
mod split;

pub use classify::{classify, StatementKind, TxControl};
pub use effect::{session_effect, SessionEffect};
pub use lexer::{
    is_ident_byte, leading_keywords, readings, tokenize, Dialect, MySqlMode, TokKind, Token,
};
pub use multi::{resplit_reading, resplits};
pub use split::{leading_blank_len, split, strip_comments, Statement};
