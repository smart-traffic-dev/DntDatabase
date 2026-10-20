//! Query-editor decisions: which statements a run executes (`plan`), whether
//! each may run on a Production or read-only connection (`guard`), how a planned
//! script runs and when it stops (`run`), how a data-view edit set becomes
//! guarded key-based DML (`edit`), and the byte <-> UTF-16 offset mapping
//! shared with the editor (`offsets`).
//!
//! Every offset exchanged with the UI is a UTF-16 code-unit offset into the
//! editor text; byte offsets stay inside this crate and `dnt-core`.

pub mod edit;
pub mod explain;
pub mod guard;
pub mod offsets;
pub mod plan;
pub mod redact;
pub mod risk;
pub mod run;
pub mod unbounded;

pub use edit::{
    atomicity, build_edit_plan, editable, fingerprint, Atomicity, CellInput, EditError, EditPlan,
    EditSet, Editability, NotEditable, PlannedEdit, Protection, ProtectionKind, RowEdit, RowRef,
};
pub use explain::{
    explain_sql, parse_plan, self_cost, ExplainFormat, ExplainSql, PlanNode, PlanTree,
};
pub use guard::{guard, guard_with, lifts_read_only, other_whitelisted, verdict, GuardCtx};
pub use offsets::Utf16Map;
pub use plan::{
    mark_implicit_commits, plan, statement_ranges, PlannedStmt, ScriptPlan, StmtKindLabel, Target,
    Verdict,
};
pub use redact::{redact, Redacted, REDACTED};
pub use risk::{
    confirm_name, confirm_names_match, drop_targets, estimate_sql, estimate_table, parse_estimate,
    DropKind, DropTarget, RowEstimate,
};
pub use run::{
    error_offset, run, RunCancel, RunReport, RunSink, StmtReport, StopReason, ROLLED_BACK_NOTICE,
};
pub use unbounded::{unbounded_writes, UnboundedReason, UnboundedWrite, WriteOp};
