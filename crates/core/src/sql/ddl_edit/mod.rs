//! Editing the DDL of views, routines, triggers and events (P4c): the pure checks
//! that the user's edited text is exactly one `CREATE` statement of the object
//! being edited, before anything is built or sent.

mod build;
mod header;
mod isolate;
mod template;

pub use build::{build, strategies, BuildInput, Built, CatalogFacts, PostCheck};
pub use header::{check_target, parse_header, Header};
pub use isolate::isolate;
pub use template::{notes, template};

use crate::error::DbError;
use crate::meta::ObjectKind;
use crate::server::{ServerInfo, SqlKind};
use serde::{Deserialize, Serialize};
use specta::Type;

/// The largest edited text accepted (1 MiB).
pub const MAX_TEXT_BYTES: usize = 1 << 20;

/// What the edited text must define and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct EditTarget {
    pub kind: ObjectKind,
    pub database: String,
    /// PostgreSQL only.
    pub schema: Option<String>,
    /// `None`: a new object.
    pub name: Option<String>,
    /// A trigger's table (edit: from the catalog; new: optional hint).
    pub table: Option<String>,
}

/// How an existing object is changed: `CREATE OR REPLACE`, or drop and re-create.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Replace,
    Recreate,
}

/// Whether objects of `kind` can be edited through their DDL on `server`.
pub fn editable_kind(kind: ObjectKind, server: &ServerInfo) -> Result<(), DbError> {
    use ObjectKind::*;
    let editable: &[ObjectKind] = match server.kind.require_sql()? {
        SqlKind::MySql => &[View, Function, Procedure, Trigger, Event],
        SqlKind::Postgres => &[View, MaterializedView, Function, Procedure, Trigger],
    };
    if editable.contains(&kind) {
        Ok(())
    } else {
        Err(DbError::Config {
            field: Some("kind".into()),
            message: format!(
                "The DDL of a {} cannot be edited on this server.",
                kind_label(kind)
            ),
        })
    }
}

/// A `Config { field: "text" }` error with a sentence the user can act on.
pub(super) fn text_error(message: impl Into<String>) -> DbError {
    DbError::Config {
        field: Some("text".into()),
        message: message.into(),
    }
}

/// The lower-case name of a kind, as used in messages.
pub(super) fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "materialized view",
        ObjectKind::Function => "function",
        ObjectKind::Procedure => "procedure",
        ObjectKind::Trigger => "trigger",
        ObjectKind::Event => "event",
        ObjectKind::Sequence => "sequence",
        ObjectKind::Type => "type",
        ObjectKind::Collection => "collection",
        ObjectKind::Key => "key",
    }
}

#[cfg(test)]
pub(super) mod test_servers {
    use crate::server::{detect_mysql, detect_pg, ServerInfo};

    pub fn mysql8() -> ServerInfo {
        detect_mysql("8.0.36").unwrap()
    }
    pub fn mysql57() -> ServerInfo {
        detect_mysql("5.7.44-log").unwrap()
    }
    pub fn mariadb() -> ServerInfo {
        detect_mysql("10.6.16-MariaDB").unwrap()
    }
    pub fn pg17() -> ServerInfo {
        detect_pg(170002, "PostgreSQL 17.2")
    }
    pub fn pg12() -> ServerInfo {
        detect_pg(120020, "PostgreSQL 12.20")
    }
}

#[cfg(test)]
mod tests {
    use super::test_servers::*;
    use super::*;

    #[test]
    fn editable_kinds_per_engine() {
        use ObjectKind::*;
        for k in [View, Function, Procedure, Trigger, Event] {
            assert!(editable_kind(k, &mysql8()).is_ok(), "{k:?}");
            assert!(editable_kind(k, &mariadb()).is_ok(), "{k:?}");
        }
        for k in [MaterializedView, Table, Sequence, Type] {
            assert!(
                matches!(editable_kind(k, &mysql8()), Err(DbError::Config { field: Some(f), .. }) if f == "kind"),
                "{k:?}"
            );
        }
        for k in [View, MaterializedView, Function, Procedure, Trigger] {
            assert!(editable_kind(k, &pg17()).is_ok(), "{k:?}");
        }
        for k in [Event, Table, Sequence, Type] {
            assert!(editable_kind(k, &pg17()).is_err(), "{k:?}");
        }
    }

    #[test]
    fn strategy_json_is_snake_case() {
        assert_eq!(
            serde_json::to_value(Strategy::Recreate).unwrap(),
            serde_json::json!("recreate")
        );
    }
}
