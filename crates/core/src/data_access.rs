//! How a Command driver builds the commands of the data view and its edits (spec §7.4).

use crate::command::{CommandClass, ParsedCommand};
use crate::edit::EditSet;
use crate::error::DbError;
use crate::keyspace::KeyName;
use crate::meta::{ObjectRef, TableDetail};
use crate::sql::filter::{DataFilter, SortSpec};
use serde::{Deserialize, Serialize};

/// The data-view face of a Command session: the driver builds the commands, the backend
/// checks them (`page` / `count` must be reads, see [`check_read`]) and runs every one
/// through the guard. A driver's command is never exempt from the guard.
pub trait DataAccess: Send + Sync {
    /// One page of a collection's documents. Its class must be `Read`.
    fn page(&self, obj: &ObjectRef, spec: &PageSpec) -> Result<ParsedCommand, DbError>;
    /// The number of documents (matching `filter`). Its class must be `Read`.
    fn count(&self, obj: &ObjectRef, filter: Option<&DataFilter>)
        -> Result<ParsedCommand, DbError>;
    /// The commands that apply a set of staged edits, in order.
    fn build_edits(
        &self,
        obj: &ObjectRef,
        detail: &TableDetail,
        edits: &EditSet,
    ) -> Result<Vec<ParsedCommand>, DbError>;
    /// The commands that apply one edit of a key in database `db`.
    fn build_key_edit(&self, db: &str, edit: &KeyEdit) -> Result<Vec<ParsedCommand>, DbError>;
}

/// What page of a collection to read.
#[derive(Debug, Clone, PartialEq)]
pub struct PageSpec {
    pub sort: Vec<SortSpec>,
    pub filter: Option<DataFilter>,
    pub limit: u32,
    pub skip: u64,
}

/// One edit of a key-value key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum KeyEdit {
    SetString {
        key: KeyName,
        value: String,
    },
    HashSet {
        key: KeyName,
        field: String,
        value: String,
    },
    HashDelete {
        key: KeyName,
        field: String,
    },
    /// `head`: push at the head (`LPUSH`), else at the tail.
    ListPush {
        key: KeyName,
        value: String,
        head: bool,
    },
    ListSet {
        key: KeyName,
        index: i64,
        value: String,
    },
    ListRemove {
        key: KeyName,
        value: String,
        count: i64,
    },
    SetAdd {
        key: KeyName,
        member: String,
    },
    SetRemove {
        key: KeyName,
        member: String,
    },
    ZSetAdd {
        key: KeyName,
        member: String,
        score: f64,
    },
    ZSetRemove {
        key: KeyName,
        member: String,
    },
    /// `None` removes the expiry (persist).
    Expire {
        key: KeyName,
        ttl_ms: Option<i64>,
    },
    Rename {
        key: KeyName,
        to: KeyName,
    },
    Delete {
        key: KeyName,
    },
}

/// Refuses a command the driver built for a read (`page`, `count`) whose class is not
/// `Read`: a driver bug, never let through.
pub fn check_read(cmd: &ParsedCommand) -> Result<(), DbError> {
    if cmd.class == CommandClass::Read {
        Ok(())
    } else {
        Err(DbError::Internal {
            message: format!(
                "the driver built `{}` for a read, but it is not a read",
                cmd.name
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn cmd(name: &str, class: CommandClass) -> ParsedCommand {
        ParsedCommand {
            name: name.into(),
            class,
            targets: vec![],
            unbounded: false,
            drops: vec![],
            session_effect: None,
            payload: Arc::new(()),
        }
    }

    #[test]
    fn check_read_refuses_writes() {
        match check_read(&cmd("updateOne", CommandClass::Write)) {
            Err(DbError::Internal { message }) => assert_eq!(
                message,
                "the driver built `updateOne` for a read, but it is not a read"
            ),
            other => panic!("expected Internal, got {other:?}"),
        }
        assert!(check_read(&cmd("find", CommandClass::Read)).is_ok());
    }
}
