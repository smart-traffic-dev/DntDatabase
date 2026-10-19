//! Query-editor decisions: which statements a run executes (`plan`) and the
//! byte <-> UTF-16 offset mapping shared with the editor (`offsets`).
//!
//! Every offset exchanged with the UI is a UTF-16 code-unit offset into the
//! editor text; byte offsets stay inside this crate and `dnt-core`.

pub mod offsets;
pub mod plan;

pub use offsets::Utf16Map;
pub use plan::{plan, statement_ranges, PlannedStmt, ScriptPlan, StmtKindLabel, Target, Verdict};
