//! Typed, single-statement DDL for connection-tree actions.
use crate::error::DbError;
use crate::meta::{ObjectKind, ObjectRef};
use crate::server::DbKind;
use crate::sql::ident::{qualified_name, quote_ident};
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectAction {
    CreateNamespace { database: String, name: String },
    DropNamespace { database: String, name: String },
    RenameTable { object: ObjectRef, name: String },
    DropObject { object: ObjectRef },
    TruncateTable { object: ObjectRef },
}

impl ObjectAction {
    pub fn database(&self) -> &str {
        match self {
            Self::CreateNamespace { database, .. } | Self::DropNamespace { database, .. } => {
                database
            }
            Self::RenameTable { object, .. }
            | Self::DropObject { object }
            | Self::TruncateTable { object } => &object.database,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Self::CreateNamespace { name, .. } | Self::DropNamespace { name, .. } => name,
            Self::RenameTable { object, .. }
            | Self::DropObject { object }
            | Self::TruncateTable { object } => &object.name,
        }
    }
    pub fn object(&self) -> Option<&ObjectRef> {
        match self {
            Self::RenameTable { object, .. }
            | Self::DropObject { object }
            | Self::TruncateTable { object } => Some(object),
            _ => None,
        }
    }
    pub fn destructive(&self) -> bool {
        matches!(
            self,
            Self::DropNamespace { .. } | Self::DropObject { .. } | Self::TruncateTable { .. }
        )
    }
}

fn invalid(message: &str) -> DbError {
    DbError::Config {
        field: Some("name".into()),
        message: message.into(),
    }
}

pub fn validate_identifier(kind: DbKind, name: &str) -> Result<(), DbError> {
    let too_long = match kind {
        DbKind::MySql => name.chars().count() > 64,
        DbKind::Postgres => name.len() > 63,
    };
    if name.trim().is_empty() || name.contains(['\0', '.']) || too_long {
        return Err(invalid(
            "Use a nonempty, unqualified identifier within the server's name limit.",
        ));
    }
    // MySQL identifiers cannot contain supplementary Unicode characters or end in a space.
    if kind == DbKind::MySql && (name.ends_with(' ') || name.chars().any(|c| u32::from(c) > 0xffff))
    {
        return Err(invalid("This name is not a valid MySQL identifier."));
    }
    Ok(())
}

pub fn action_sql(kind: DbKind, action: &ObjectAction) -> Result<String, DbError> {
    validate_identifier(kind, action.name())?;
    if kind == DbKind::Postgres || action.object().is_some() {
        validate_identifier(kind, action.database())?;
    }
    let q = |s: &str| quote_ident(kind, s);
    match action {
        ObjectAction::CreateNamespace { name, .. } => {
            if kind == DbKind::MySql && name.contains(['/', '\\']) {
                return Err(invalid("Database names cannot contain path separators."));
            }
            Ok(format!(
                "CREATE {} {}",
                if kind == DbKind::MySql {
                    "DATABASE"
                } else {
                    "SCHEMA"
                },
                q(name)
            ))
        }
        ObjectAction::DropNamespace { name, .. } => Ok(format!(
            "DROP {} {}{}",
            if kind == DbKind::MySql {
                "DATABASE"
            } else {
                "SCHEMA"
            },
            q(name),
            if kind == DbKind::Postgres {
                " RESTRICT"
            } else {
                ""
            }
        )),
        _ => {
            let obj = action.object().expect("relation action");
            if kind == DbKind::Postgres {
                validate_identifier(
                    kind,
                    obj.schema
                        .as_deref()
                        .ok_or_else(|| invalid("An explicit PostgreSQL schema is required."))?,
                )?;
            } else if obj.schema.is_some() {
                return Err(invalid("MySQL actions do not accept a schema."));
            }
            let relation = qualified_name(kind, &obj.database, obj.schema.as_deref(), &obj.name);
            match action {
                ObjectAction::RenameTable { name, .. } if obj.kind == ObjectKind::Table => {
                    validate_identifier(kind, name)?;
                    if name == &obj.name {
                        return Err(invalid("Choose a different table name."));
                    }
                    Ok(if kind == DbKind::MySql {
                        format!(
                            "RENAME TABLE {relation} TO {}",
                            qualified_name(kind, &obj.database, None, name)
                        )
                    } else {
                        format!("ALTER TABLE {relation} RENAME TO {}", q(name))
                    })
                }
                ObjectAction::TruncateTable { .. } if obj.kind == ObjectKind::Table => Ok(format!(
                    "TRUNCATE TABLE {relation}{}",
                    if kind == DbKind::Postgres {
                        " CONTINUE IDENTITY RESTRICT"
                    } else {
                        ""
                    }
                )),
                ObjectAction::DropObject { .. } => {
                    let ty = match obj.kind {
                        ObjectKind::Table => "TABLE",
                        ObjectKind::View => "VIEW",
                        ObjectKind::MaterializedView if kind == DbKind::Postgres => {
                            "MATERIALIZED VIEW"
                        }
                        _ => return Err(invalid("This object kind is outside P4a.")),
                    };
                    Ok(format!(
                        "DROP {ty} {relation}{}",
                        if kind == DbKind::Postgres {
                            " RESTRICT"
                        } else {
                            ""
                        }
                    ))
                }
                _ => Err(invalid("This action requires an ordinary table.")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn object(kind: ObjectKind, pg: bool) -> ObjectRef {
        ObjectRef {
            database: "db".into(),
            schema: pg.then(|| "public".into()),
            name: "a`\"b".into(),
            kind,
        }
    }
    #[test]
    fn quotes_actions_and_never_adds_cascade() {
        assert_eq!(
            action_sql(
                DbKind::MySql,
                &ObjectAction::DropObject {
                    object: object(ObjectKind::Table, false)
                }
            )
            .unwrap(),
            "DROP TABLE `db`.`a``\"b`"
        );
        assert_eq!(
            action_sql(
                DbKind::Postgres,
                &ObjectAction::TruncateTable {
                    object: object(ObjectKind::Table, true)
                }
            )
            .unwrap(),
            "TRUNCATE TABLE \"public\".\"a`\"\"b\" CONTINUE IDENTITY RESTRICT"
        );
        assert!(action_sql(
            DbKind::MySql,
            &ObjectAction::DropObject {
                object: object(ObjectKind::Function, false)
            }
        )
        .is_err());
        assert!(action_sql(
            DbKind::MySql,
            &ObjectAction::TruncateTable {
                object: object(ObjectKind::View, false)
            }
        )
        .is_err());
    }
    #[test]
    fn limits_use_server_units() {
        assert!(validate_identifier(DbKind::Postgres, &"ệ".repeat(21)).is_ok());
        assert!(validate_identifier(DbKind::Postgres, &"ệ".repeat(22)).is_err());
        assert!(validate_identifier(DbKind::MySql, &"ệ".repeat(64)).is_ok());
        for bad in ["", " ", "a.b", "a\0b"] {
            assert!(validate_identifier(DbKind::Postgres, bad).is_err());
        }
        assert!(validate_identifier(DbKind::MySql, "x😀").is_err());
    }
    #[test]
    fn rename_is_explicit_and_qualified() {
        let a = ObjectAction::RenameTable {
            object: object(ObjectKind::Table, true),
            name: "new".into(),
        };
        assert_eq!(
            action_sql(DbKind::Postgres, &a).unwrap(),
            "ALTER TABLE \"public\".\"a`\"\"b\" RENAME TO \"new\""
        );
    }
}
