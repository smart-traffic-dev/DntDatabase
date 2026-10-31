//! How a Command driver builds the commands of the data view and its edits (spec §7.4).

use crate::command::{CommandClass, ParsedCommand};
use crate::edit::EditSet;
use crate::error::DbError;
use crate::keyspace::{KeyBytes, KeyName};
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
        value: KeyBytes,
    },
    HashSet {
        key: KeyName,
        field: KeyBytes,
        value: KeyBytes,
    },
    HashDelete {
        key: KeyName,
        field: KeyBytes,
    },
    /// `head`: push at the head (`LPUSH`), else at the tail.
    ListPush {
        key: KeyName,
        value: KeyBytes,
        head: bool,
    },
    ListSet {
        key: KeyName,
        index: i64,
        value: KeyBytes,
    },
    ListRemove {
        key: KeyName,
        value: KeyBytes,
        count: i64,
    },
    SetAdd {
        key: KeyName,
        member: KeyBytes,
    },
    SetRemove {
        key: KeyName,
        member: KeyBytes,
    },
    ZSetAdd {
        key: KeyName,
        member: KeyBytes,
        score: f64,
    },
    ZSetRemove {
        key: KeyName,
        member: KeyBytes,
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

/// Refuses a command the driver built for a read (`page`, `count`) that is not a plain
/// read: its class is not `Read`, or it drops something, is `unbounded` or changes the
/// session. A driver bug, never let through.
pub fn check_read(cmd: &ParsedCommand) -> Result<(), DbError> {
    if cmd.class == CommandClass::Read
        && cmd.drops.is_empty()
        && !cmd.unbounded
        && cmd.session_effect.is_none()
    {
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
    use crate::command::{CommandDrop, CommandDropKind, CommandSessionEffect, CommandTarget};
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
    fn key_edit_serde_round_trip() {
        let key = || KeyName::from_bytes(b"k".to_vec());
        let bin = || KeyBytes(vec![0xff, 0x00, b'a']);
        let edits = [
            KeyEdit::SetString {
                key: key(),
                value: bin(),
            },
            KeyEdit::HashSet {
                key: key(),
                field: bin(),
                value: "v".into(),
            },
            KeyEdit::HashDelete {
                key: key(),
                field: bin(),
            },
            KeyEdit::ListPush {
                key: key(),
                value: bin(),
                head: true,
            },
            KeyEdit::ListSet {
                key: key(),
                index: -1,
                value: bin(),
            },
            KeyEdit::ListRemove {
                key: key(),
                value: bin(),
                count: 2,
            },
            KeyEdit::SetAdd {
                key: key(),
                member: bin(),
            },
            KeyEdit::SetRemove {
                key: key(),
                member: bin(),
            },
            KeyEdit::ZSetAdd {
                key: key(),
                member: bin(),
                score: 1.5,
            },
            KeyEdit::ZSetRemove {
                key: key(),
                member: bin(),
            },
            KeyEdit::Expire {
                key: key(),
                ttl_ms: Some(1000),
            },
            KeyEdit::Rename {
                key: key(),
                to: KeyName::from_bytes(vec![0xfe]),
            },
            KeyEdit::Delete { key: key() },
        ];
        for edit in &edits {
            let json = serde_json::to_value(edit).unwrap();
            let back: KeyEdit = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(&back, edit, "{json}");
        }
        let zadd = serde_json::to_value(&edits[8]).unwrap();
        assert_eq!(zadd["op"], "z_set_add");
        // Binary members travel as standard base64.
        assert_eq!(zadd["member"], "/wBh");
        assert_eq!(KeyBytes::from("hi"), KeyBytes(b"hi".to_vec()));
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

        // A "read" that drops, acts on everything or changes the session is not one.
        let dropping = ParsedCommand {
            drops: vec![CommandDrop {
                kind: CommandDropKind::Database,
                target: CommandTarget {
                    database: None,
                    name: String::new(),
                },
            }],
            ..cmd("flush", CommandClass::Read)
        };
        let unbounded = ParsedCommand {
            unbounded: true,
            ..cmd("wipe", CommandClass::Read)
        };
        let session = ParsedCommand {
            session_effect: Some(CommandSessionEffect::UseDatabase("x".into())),
            ..cmd("use", CommandClass::Read)
        };
        for c in [dropping, unbounded, session] {
            assert!(
                matches!(check_read(&c), Err(DbError::Internal { .. })),
                "{c:?}"
            );
        }
    }
}
