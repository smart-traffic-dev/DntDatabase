//! Backup and restore (spec §7): the integrated dump ("DntDatabase dump v1", §7.1,
//! R22–R24, R29), dumps through external tools (§7.2) and restore (§7.3, R30–R34).
//!
//! - `dump`: the selectable objects of a database (with the skipped ones and why, R22),
//!   `dump_start` and the dump job ([`dump::DumpJob`]).
//! - `ext_tools`: N7, the external tool runner (discovery, secret files, process
//!   plumbing, Stop = kill; §7.2, R25–R29); `tool_dump`: dumps through `mysqldump`,
//!   `mariadb-dump` or `pg_dump`.
//! - `restore`: three guarded paths (app runner, psql, pg_restore).
//!
//! A dump only reads: it runs on its own session inside one read-only snapshot (R24), is
//! allowed on read-only connections and needs no confirmation. Its file is written as
//! `<file>.dntpart` and renamed when complete; a failed or stopped dump removes it (R29).
//! Values become SQL only through `dnt_core::sql::build::dump_row_tuple`.
pub mod dump;
pub mod ext_tools;
pub mod restore;
#[cfg(test)]
mod tests;
pub mod tool_dump;

pub use ext_tools::ToolsList;
pub use restore::{RestorePlanStore, RestorePreview, RestoreRequest};
pub use tool_dump::ToolDumpRequest;

use dnt_core::meta::ObjectKind;
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

/// One object a dump can hold.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub struct DumpObject {
    /// PostgreSQL: the schema; MySQL: `None`.
    pub schema: Option<String>,
    pub name: String,
    /// `Table`, `View`; PostgreSQL also `Sequence` (stand-alone) and `Type` (enum, composite).
    pub kind: ObjectKind,
    /// Tables: dump the rows too.
    pub data: bool,
}

/// An object the integrated dump does not cover (R22), and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SkippedObject {
    pub schema: Option<String>,
    pub name: String,
    pub kind: ObjectKind,
    pub reason: String,
}

/// What `dump_objects` lists: the selectable objects and the skipped ones.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
pub struct DumpObjects {
    pub objects: Vec<DumpObject>,
    pub skipped: Vec<SkippedObject>,
}

/// What the user asked to dump (also the stored parameters of "Run again").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DumpRequest {
    pub connection_id: Uuid,
    pub database: String,
    /// PostgreSQL: the schemas the dump covers (named in the header).
    pub schemas: Vec<String>,
    pub objects: Vec<DumpObject>,
    /// `DROP … IF EXISTS` before each object is created.
    pub include_drop: bool,
    /// Write `.sql.gz` (gzip).
    pub compress: bool,
    /// A path admitted by `dump_pick_path`.
    pub path: String,
}
