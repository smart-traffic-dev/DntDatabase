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
    /// `charset`/`collation`: MySQL only (`None`: the server default). `owner`: PostgreSQL only.
    CreateNamespace {
        database: String,
        name: String,
        #[serde(default)]
        charset: Option<String>,
        #[serde(default)]
        collation: Option<String>,
        #[serde(default)]
        owner: Option<String>,
    },
    DropNamespace {
        database: String,
        name: String,
    },
    RenameTable {
        object: ObjectRef,
        name: String,
    },
    DropObject {
        object: ObjectRef,
    },
    TruncateTable {
        object: ObjectRef,
    },
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

/// A MySQL character set or collation name, written unquoted: ASCII word characters only.
fn charset_word<'a>(field: &str, s: &'a str) -> Result<&'a str, DbError> {
    if s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        Ok(s)
    } else {
        Err(DbError::Config {
            field: Some(field.into()),
            message: format!("This is not a valid MySQL {field} name."),
        })
    }
}

pub fn action_sql(kind: DbKind, action: &ObjectAction) -> Result<String, DbError> {
    validate_identifier(kind, action.name())?;
    if kind == DbKind::Postgres || action.object().is_some() {
        validate_identifier(kind, action.database())?;
    }
    let q = |s: &str| quote_ident(kind, s);
    match action {
        ObjectAction::CreateNamespace {
            name,
            charset,
            collation,
            owner,
            ..
        } => {
            fn set(o: &Option<String>) -> Option<&str> {
                o.as_deref().filter(|s| !s.is_empty())
            }
            let (charset, collation, owner) = (set(charset), set(collation), set(owner));
            if kind == DbKind::MySql {
                if name.contains(['/', '\\']) {
                    return Err(invalid("Database names cannot contain path separators."));
                }
                if owner.is_some() {
                    return Err(invalid("MySQL databases have no owner."));
                }
                let mut sql = format!("CREATE DATABASE {}", q(name));
                if let Some(cs) = charset {
                    sql.push_str(&format!(" CHARACTER SET {}", charset_word("charset", cs)?));
                }
                if let Some(co) = collation {
                    sql.push_str(&format!(" COLLATE {}", charset_word("collation", co)?));
                }
                Ok(sql)
            } else {
                if charset.is_some() || collation.is_some() {
                    return Err(invalid(
                        "PostgreSQL schemas have no character set or collation.",
                    ));
                }
                let mut sql = format!("CREATE SCHEMA {}", q(name));
                if let Some(role) = owner {
                    // A role name is never qualified, so a dot is part of it (`john.doe`).
                    if role.trim().is_empty() || role.contains('\0') || role.len() > 63 {
                        return Err(DbError::Config {
                            field: Some("owner".into()),
                            message: "This owner is not a valid role name.".into(),
                        });
                    }
                    sql.push_str(&format!(" AUTHORIZATION {}", q(role)));
                }
                Ok(sql)
            }
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
    fn create(charset: Option<&str>, collation: Option<&str>, owner: Option<&str>) -> ObjectAction {
        ObjectAction::CreateNamespace {
            database: "db".into(),
            name: "n`\"s".into(),
            charset: charset.map(Into::into),
            collation: collation.map(Into::into),
            owner: owner.map(Into::into),
        }
    }
    #[test]
    fn create_namespace_options() {
        assert_eq!(
            action_sql(DbKind::MySql, &create(None, None, None)).unwrap(),
            "CREATE DATABASE `n``\"s`"
        );
        assert_eq!(
            action_sql(
                DbKind::MySql,
                &create(Some("utf8mb4"), Some("utf8mb4_bin"), None)
            )
            .unwrap(),
            "CREATE DATABASE `n``\"s` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin"
        );
        // Empty strings mean the server default.
        assert_eq!(
            action_sql(
                DbKind::MySql,
                &create(Some(""), Some("latin1_bin"), Some(""))
            )
            .unwrap(),
            "CREATE DATABASE `n``\"s` COLLATE latin1_bin"
        );
        assert!(action_sql(DbKind::MySql, &create(Some("utf8; DROP"), None, None)).is_err());
        assert!(action_sql(DbKind::MySql, &create(None, None, Some("root"))).is_err());
        assert_eq!(
            action_sql(DbKind::Postgres, &create(None, None, Some("app\"owner"))).unwrap(),
            "CREATE SCHEMA \"n`\"\"s\" AUTHORIZATION \"app\"\"owner\""
        );
        assert_eq!(
            action_sql(DbKind::Postgres, &create(None, None, Some("john.doe"))).unwrap(),
            "CREATE SCHEMA \"n`\"\"s\" AUTHORIZATION \"john.doe\""
        );
        assert!(action_sql(DbKind::Postgres, &create(None, None, Some(" "))).is_err());
        assert!(action_sql(DbKind::Postgres, &create(None, None, Some(&"r".repeat(64)))).is_err());
        assert!(action_sql(DbKind::Postgres, &create(Some("UTF8"), None, None)).is_err());
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
