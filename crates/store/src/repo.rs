//! `ConnectionRepo`: connections and groups in SQLite, secrets in a `SecretStore`.
//!
//! Groups and connections are ordered independently: `sort_order` ranks the groups of one
//! parent, and separately the connections of one group.

use crate::db::internal;
use dnt_core::config::{ConnOptions, ConnectionConfig, EnvTag, SecretMode, SshConfig, SslConfig};
use dnt_core::error::DbError;
use dnt_core::secret::{SecretKind, SecretStore};
use dnt_core::server::DbKind;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Group {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: String,
    /// Exported as a JS number (positions are small).
    #[specta(type = f64)]
    pub sort_order: i64,
}

/// A connection as shown in the tree: config plus secret *presence* flags, never values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ConnectionSummary {
    pub config: ConnectionConfig,
    pub has_db_password: bool,
    pub has_ssh_secret: bool,
    pub last_connected_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Tree {
    pub groups: Vec<Group>,
    pub connections: Vec<ConnectionSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "type", content = "id", rename_all = "lowercase")]
pub enum NodeId {
    Group(Uuid),
    Connection(Uuid),
}

/// Secrets supplied when creating a connection.
pub struct NewSecrets {
    pub db_password: Option<SecretString>,
    pub ssh_password: Option<SecretString>,
    pub ssh_key_passphrase: Option<SecretString>,
}

pub enum SecretChange {
    Keep,
    Set(SecretString),
    Clear,
}

/// Per-secret changes applied when updating a connection.
pub struct SecretUpdate {
    pub db_password: SecretChange,
    pub ssh_password: SecretChange,
    pub ssh_key_passphrase: SecretChange,
}

pub struct ConnectionRepo {
    conn: Mutex<Connection>,
    secrets: Arc<dyn SecretStore>,
}

const CONN_COLUMNS: &str = "id, group_id, name, sort_order, kind, host, port, username, \
    default_db, env_tag, read_only, secret_mode, ssh_json, ssl_json, options_json, \
    last_connected_at";

impl ConnectionRepo {
    pub fn new(conn: Connection, secrets: Arc<dyn SecretStore>) -> Self {
        ConnectionRepo {
            conn: Mutex::new(conn),
            secrets,
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, DbError> {
        self.conn.lock().map_err(|_| DbError::Internal {
            message: "connection repository lock is poisoned".into(),
        })
    }

    pub fn tree(&self) -> Result<Tree, DbError> {
        let (groups, rows) = {
            let conn = self.lock()?;
            (load_groups(&conn)?, load_raw_connections(&conn)?)
        };
        // Keychain lookups happen without holding the database lock. They only check
        // presence; an entry that cannot be read is shown as absent (the user is asked
        // for it when connecting) rather than failing the whole tree.
        let mut connections = Vec::with_capacity(rows.len());
        for raw in rows {
            let last_connected_at = raw.last_connected_at.clone();
            let config = raw.into_config()?;
            let has = |kind: SecretKind| match self.secrets.has(config.id, kind) {
                Ok(present) => present,
                Err(_) => {
                    tracing::warn!(
                        connection = %config.id,
                        kind = kind.as_str(),
                        "cannot check a stored secret; treating it as absent"
                    );
                    false
                }
            };
            let has_db_password = has(SecretKind::DbPassword);
            let has_ssh_secret = has(SecretKind::SshPassword) || has(SecretKind::SshKeyPassphrase);
            connections.push(ConnectionSummary {
                config,
                has_db_password,
                has_ssh_secret,
                last_connected_at,
            });
        }
        Ok(Tree {
            groups,
            connections,
        })
    }

    pub fn get(&self, id: Uuid) -> Result<ConnectionConfig, DbError> {
        let conn = self.lock()?;
        load_connection(&conn, id)
    }

    /// Validates and inserts `cfg`. A nil id gets a fresh v4 id; an explicit id is kept.
    pub fn create(
        &self,
        mut cfg: ConnectionConfig,
        secrets: NewSecrets,
    ) -> Result<ConnectionConfig, DbError> {
        cfg.validate()?;
        if cfg.id.is_nil() {
            cfg.id = Uuid::new_v4();
        }
        let mut conn = self.lock()?;
        let tx = begin(&mut conn)?;
        insert_connection(&tx, &mut cfg)?;
        let applied = self.apply_secrets(
            cfg.id,
            cfg.secret_mode,
            [
                (SecretKind::DbPassword, set_or_keep(secrets.db_password)),
                (SecretKind::SshPassword, set_or_keep(secrets.ssh_password)),
                (
                    SecretKind::SshKeyPassphrase,
                    set_or_keep(secrets.ssh_key_passphrase),
                ),
            ],
        );
        if let Err(e) = applied {
            // Best effort: do not leave secrets behind for a connection that was not saved.
            let _ = self.secrets.delete_all(cfg.id);
            return Err(e);
        }
        commit(tx)?;
        Ok(cfg)
    }

    /// Updates the stored config. Position (`sort_order`) is kept unless `group_id` changes,
    /// in which case the connection is appended to its new group.
    pub fn update(
        &self,
        mut cfg: ConnectionConfig,
        secrets: SecretUpdate,
    ) -> Result<ConnectionConfig, DbError> {
        cfg.validate()?;
        let mut conn = self.lock()?;
        let tx = begin(&mut conn)?;
        update_connection_row(&tx, &mut cfg)?;
        self.apply_secrets(
            cfg.id,
            cfg.secret_mode,
            [
                (SecretKind::DbPassword, secrets.db_password),
                (SecretKind::SshPassword, secrets.ssh_password),
                (SecretKind::SshKeyPassphrase, secrets.ssh_key_passphrase),
            ],
        )?;
        commit(tx)?;
        Ok(cfg)
    }

    /// Copies a connection (same group, appended) as "<name> copy", including its secrets.
    pub fn duplicate(&self, id: Uuid) -> Result<ConnectionConfig, DbError> {
        let mut cfg = self.get(id)?;
        let secrets = NewSecrets {
            db_password: self.secrets.get(id, SecretKind::DbPassword)?,
            ssh_password: self.secrets.get(id, SecretKind::SshPassword)?,
            ssh_key_passphrase: self.secrets.get(id, SecretKind::SshKeyPassphrase)?,
        };
        cfg.id = Uuid::nil();
        cfg.name = format!("{} copy", cfg.name);
        self.create(cfg, secrets)
    }

    pub fn delete(&self, id: Uuid) -> Result<(), DbError> {
        let mut conn = self.lock()?;
        let tx = begin(&mut conn)?;
        let n = tx
            .execute(
                "DELETE FROM connections WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(|e| internal("delete connection", e))?;
        if n == 0 {
            return Err(not_found("connection", id));
        }
        self.secrets.delete_all(id)?;
        commit(tx)
    }

    pub fn create_group(&self, name: &str, parent: Option<Uuid>) -> Result<Group, DbError> {
        let mut conn = self.lock()?;
        let tx = begin(&mut conn)?;
        let group = Group {
            id: Uuid::new_v4(),
            parent_id: parent,
            name: name.to_string(),
            sort_order: 0,
        };
        let group = insert_group(&tx, group)?;
        commit(tx)?;
        Ok(group)
    }

    pub fn rename_group(&self, id: Uuid, name: &str) -> Result<(), DbError> {
        validate_group_name(name)?;
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE groups SET name = ?2 WHERE id = ?1",
                params![id.to_string(), name],
            )
            .map_err(|e| internal("rename group", e))?;
        if n == 0 {
            return Err(not_found("group", id));
        }
        Ok(())
    }

    /// Deletes a group and its subgroups; their connections move to the root (appended).
    pub fn delete_group(&self, id: Uuid) -> Result<(), DbError> {
        let mut conn = self.lock()?;
        let tx = begin(&mut conn)?;
        let exists = tx
            .query_row(
                "SELECT 1 FROM groups WHERE id = ?1",
                params![id.to_string()],
                |_| Ok(()),
            )
            .optional()
            .map_err(|e| internal("load group", e))?;
        if exists.is_none() {
            return Err(not_found("group", id));
        }
        let moved: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "WITH RECURSIVE sub(id) AS ( \
                       SELECT ?1 UNION ALL \
                       SELECT g.id FROM groups g JOIN sub ON g.parent_id = sub.id) \
                     SELECT c.id FROM connections c JOIN groups g ON c.group_id = g.id \
                     WHERE c.group_id IN (SELECT id FROM sub) \
                     ORDER BY g.sort_order, c.sort_order, c.name",
                )
                .map_err(|e| internal("list group connections", e))?;
            let ids = stmt
                .query_map(params![id.to_string()], |r| r.get(0))
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .map_err(|e| internal("list group connections", e))?;
            ids
        };
        let first = next_connection_order(&tx, None)?;
        for (order, conn_id) in (first..).zip(&moved) {
            tx.execute(
                "UPDATE connections SET group_id = NULL, sort_order = ?2 WHERE id = ?1",
                params![conn_id, order],
            )
            .map_err(|e| internal("move connection to root", e))?;
        }
        // Subgroups go via ON DELETE CASCADE.
        tx.execute("DELETE FROM groups WHERE id = ?1", params![id.to_string()])
            .map_err(|e| internal("delete group", e))?;
        commit(tx)
    }

    /// Moves a node under `new_parent` (None = root) at `index` among its siblings of the
    /// same kind; an index past the end appends. Destination siblings are renumbered 0..n.
    pub fn move_node(
        &self,
        node: NodeId,
        new_parent: Option<Uuid>,
        index: usize,
    ) -> Result<(), DbError> {
        let mut conn = self.lock()?;
        let tx = begin(&mut conn)?;
        if let Some(parent) = new_parent {
            let exists = tx
                .query_row(
                    "SELECT 1 FROM groups WHERE id = ?1",
                    params![parent.to_string()],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|e| internal("load group", e))?;
            if exists.is_none() {
                return Err(DbError::Config {
                    field: Some("parent_id".into()),
                    message: format!("group {parent} does not exist"),
                });
            }
        }
        let parent_str = new_parent.map(|p| p.to_string());
        let (table, parent_col, id) = match node {
            NodeId::Group(id) => ("groups", "parent_id", id),
            NodeId::Connection(id) => ("connections", "group_id", id),
        };
        let id_str = id.to_string();
        if let NodeId::Group(_) = node {
            if let Some(parent) = new_parent {
                let cycle: bool = tx
                    .query_row(
                        "WITH RECURSIVE sub(id) AS ( \
                           SELECT ?1 UNION ALL \
                           SELECT g.id FROM groups g JOIN sub ON g.parent_id = sub.id) \
                         SELECT EXISTS (SELECT 1 FROM sub WHERE id = ?2)",
                        params![id_str, parent.to_string()],
                        |r| r.get(0),
                    )
                    .map_err(|e| internal("check group cycle", e))?;
                if cycle {
                    return Err(DbError::Config {
                        field: Some("parent_id".into()),
                        message: "a group cannot be moved into itself or its subgroups".into(),
                    });
                }
            }
        }
        let exists = tx
            .query_row(
                &format!("SELECT 1 FROM {table} WHERE id = ?1"),
                params![id_str],
                |_| Ok(()),
            )
            .optional()
            .map_err(|e| internal("load node", e))?;
        if exists.is_none() {
            return Err(not_found(table.trim_end_matches('s'), id));
        }
        let mut siblings: Vec<String> = {
            let mut stmt = tx
                .prepare(&format!(
                    "SELECT id FROM {table} WHERE {parent_col} IS ?1 AND id <> ?2 \
                     ORDER BY sort_order, name"
                ))
                .map_err(|e| internal("list siblings", e))?;
            let ids = stmt
                .query_map(params![parent_str, id_str], |r| r.get(0))
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .map_err(|e| internal("list siblings", e))?;
            ids
        };
        siblings.insert(index.min(siblings.len()), id_str.clone());
        tx.execute(
            &format!("UPDATE {table} SET {parent_col} = ?2 WHERE id = ?1"),
            params![id_str, parent_str],
        )
        .map_err(|e| internal("move node", e))?;
        for (order, sibling) in siblings.iter().enumerate() {
            tx.execute(
                &format!("UPDATE {table} SET sort_order = ?2 WHERE id = ?1"),
                params![sibling, order as i64],
            )
            .map_err(|e| internal("renumber siblings", e))?;
        }
        commit(tx)
    }

    pub fn touch_connected(&self, id: Uuid) -> Result<(), DbError> {
        let now = now_rfc3339()?;
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE connections SET last_connected_at = ?2 WHERE id = ?1",
                params![id.to_string(), now],
            )
            .map_err(|e| internal("record last connection", e))?;
        if n == 0 {
            return Err(not_found("connection", id));
        }
        Ok(())
    }

    /// A value of the `app_settings` table (app.db is the one database the repository owns).
    pub fn app_setting(&self, key: &str) -> Result<Option<String>, DbError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT value FROM app_settings WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| internal("read app setting", e))
    }

    pub fn set_app_setting(&self, key: &str, value: &str) -> Result<(), DbError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO app_settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(|e| internal("write app setting", e))?;
        Ok(())
    }

    pub(crate) fn secret_store(&self) -> &dyn SecretStore {
        &*self.secrets
    }

    /// All groups and connection configs, without touching the secret store.
    pub(crate) fn snapshot(&self) -> Result<(Vec<Group>, Vec<ConnectionConfig>), DbError> {
        let conn = self.lock()?;
        let groups = load_groups(&conn)?;
        let configs = load_raw_connections(&conn)?
            .into_iter()
            .map(RawConn::into_config)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((groups, configs))
    }

    /// Runs `f` in one SQLite transaction, committed only when `f` succeeds.
    pub(crate) fn transaction<R>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<R, DbError>,
    ) -> Result<R, DbError> {
        let mut conn = self.lock()?;
        let tx = begin(&mut conn)?;
        let out = f(&tx)?;
        commit(tx)?;
        Ok(out)
    }

    /// Ask mode never keeps secrets: anything stored is removed. Otherwise applies each change.
    pub(crate) fn apply_secrets(
        &self,
        id: Uuid,
        mode: SecretMode,
        changes: [(SecretKind, SecretChange); 3],
    ) -> Result<(), DbError> {
        if mode == SecretMode::Ask {
            return self.secrets.delete_all(id);
        }
        for (kind, change) in changes {
            match change {
                SecretChange::Keep => {}
                SecretChange::Set(value) => self.secrets.set(id, kind, &value)?,
                SecretChange::Clear => self.secrets.delete(id, kind)?,
            }
        }
        Ok(())
    }
}

pub(crate) fn set_or_keep(value: Option<SecretString>) -> SecretChange {
    value.map_or(SecretChange::Keep, SecretChange::Set)
}

fn begin(conn: &mut Connection) -> Result<Transaction<'_>, DbError> {
    conn.transaction()
        .map_err(|e| internal("begin transaction", e))
}

fn commit(tx: Transaction<'_>) -> Result<(), DbError> {
    tx.commit().map_err(|e| internal("commit transaction", e))
}

fn not_found(what: &str, id: Uuid) -> DbError {
    DbError::Internal {
        message: format!("{what} {id} not found"),
    }
}

fn validate_group_name(name: &str) -> Result<(), DbError> {
    if name.trim().is_empty() {
        return Err(DbError::Config {
            field: Some("name".into()),
            message: "group name must not be empty".into(),
        });
    }
    Ok(())
}

fn next_connection_order(conn: &Connection, group: Option<Uuid>) -> Result<i64, DbError> {
    conn.query_row(
        "SELECT COALESCE(MAX(sort_order) + 1, 0) FROM connections WHERE group_id IS ?1",
        params![group.map(|g| g.to_string())],
        |r| r.get(0),
    )
    .map_err(|e| internal("compute connection order", e))
}

fn load_connection(conn: &Connection, id: Uuid) -> Result<ConnectionConfig, DbError> {
    find_connection(conn, id)?.ok_or_else(|| not_found("connection", id))
}

pub(crate) fn find_connection(
    conn: &Connection,
    id: Uuid,
) -> Result<Option<ConnectionConfig>, DbError> {
    conn.query_row(
        &format!("SELECT {CONN_COLUMNS} FROM connections WHERE id = ?1"),
        params![id.to_string()],
        RawConn::from_row,
    )
    .optional()
    .map_err(|e| internal("load connection", e))?
    .map(RawConn::into_config)
    .transpose()
}

/// Inserts `cfg` (already validated) appended to its group; sets `cfg.sort_order`.
pub(crate) fn insert_connection(
    conn: &Connection,
    cfg: &mut ConnectionConfig,
) -> Result<(), DbError> {
    cfg.sort_order = next_connection_order(conn, cfg.group_id)?;
    let now = now_rfc3339()?;
    let cols = ConnColumns::from_config(cfg)?;
    conn.execute(
        "INSERT INTO connections (id, group_id, name, sort_order, kind, host, port, \
         username, default_db, env_tag, read_only, secret_mode, ssh_json, ssl_json, \
         options_json, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?16)",
        params![
            cfg.id.to_string(),
            cfg.group_id.map(|g| g.to_string()),
            cfg.name,
            cfg.sort_order,
            cols.kind,
            cfg.host,
            cfg.port,
            cfg.username,
            cfg.default_db,
            cols.env_tag,
            cfg.read_only,
            cols.secret_mode,
            cols.ssh_json,
            cols.ssl_json,
            cols.options_json,
            now,
        ],
    )
    .map_err(|e| internal("insert connection", e))?;
    Ok(())
}

/// Rewrites the stored row of `cfg` (already validated). Keeps its position unless the group
/// changes, in which case it is appended to the new group; sets `cfg.sort_order`.
pub(crate) fn update_connection_row(
    conn: &Connection,
    cfg: &mut ConnectionConfig,
) -> Result<(), DbError> {
    let current = load_connection(conn, cfg.id)?;
    cfg.sort_order = if current.group_id == cfg.group_id {
        current.sort_order
    } else {
        next_connection_order(conn, cfg.group_id)?
    };
    let now = now_rfc3339()?;
    let cols = ConnColumns::from_config(cfg)?;
    conn.execute(
        "UPDATE connections SET group_id = ?2, name = ?3, sort_order = ?4, kind = ?5, \
         host = ?6, port = ?7, username = ?8, default_db = ?9, env_tag = ?10, \
         read_only = ?11, secret_mode = ?12, ssh_json = ?13, ssl_json = ?14, \
         options_json = ?15, updated_at = ?16 WHERE id = ?1",
        params![
            cfg.id.to_string(),
            cfg.group_id.map(|g| g.to_string()),
            cfg.name,
            cfg.sort_order,
            cols.kind,
            cfg.host,
            cfg.port,
            cfg.username,
            cfg.default_db,
            cols.env_tag,
            cfg.read_only,
            cols.secret_mode,
            cols.ssh_json,
            cols.ssl_json,
            cols.options_json,
            now,
        ],
    )
    .map_err(|e| internal("update connection", e))?;
    Ok(())
}

/// Names of the connections in `group` (None = root), other than `except`.
pub(crate) fn connection_names_in_group(
    conn: &Connection,
    group: Option<Uuid>,
    except: Uuid,
) -> Result<HashSet<String>, DbError> {
    let mut stmt = conn
        .prepare("SELECT name FROM connections WHERE group_id IS ?1 AND id <> ?2")
        .map_err(|e| internal("list connection names", e))?;
    let names = stmt
        .query_map(
            params![group.map(|g| g.to_string()), except.to_string()],
            |r| r.get(0),
        )
        .and_then(|rows| rows.collect::<Result<HashSet<String>, _>>())
        .map_err(|e| internal("list connection names", e))?;
    Ok(names)
}

pub(crate) fn group_exists(conn: &Connection, id: Uuid) -> Result<bool, DbError> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM groups WHERE id = ?1)",
        params![id.to_string()],
        |r| r.get(0),
    )
    .map_err(|e| internal("load group", e))
}

/// Inserts `group` with its id, appended among the children of its parent; returns it with
/// the assigned `sort_order`.
pub(crate) fn insert_group(conn: &Connection, mut group: Group) -> Result<Group, DbError> {
    validate_group_name(&group.name)?;
    let parent_str = group.parent_id.map(|p| p.to_string());
    group.sort_order = conn
        .query_row(
            "SELECT COALESCE(MAX(sort_order) + 1, 0) FROM groups WHERE parent_id IS ?1",
            params![parent_str],
            |r| r.get(0),
        )
        .map_err(|e| internal("compute group order", e))?;
    conn.execute(
        "INSERT INTO groups (id, parent_id, name, sort_order) VALUES (?1, ?2, ?3, ?4)",
        params![
            group.id.to_string(),
            parent_str,
            group.name,
            group.sort_order
        ],
    )
    .map_err(|e| internal("insert group", e))?;
    Ok(group)
}

fn load_groups(conn: &Connection) -> Result<Vec<Group>, DbError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, parent_id, name, sort_order FROM groups \
             ORDER BY parent_id, sort_order, name",
        )
        .map_err(|e| internal("list groups", e))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .map_err(|e| internal("list groups", e))?;
    rows.into_iter()
        .map(|(id, parent, name, sort_order)| {
            Ok(Group {
                id: parse_uuid(&id)?,
                parent_id: parent.as_deref().map(parse_uuid).transpose()?,
                name,
                sort_order,
            })
        })
        .collect()
}

fn load_raw_connections(conn: &Connection) -> Result<Vec<RawConn>, DbError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {CONN_COLUMNS} FROM connections ORDER BY group_id, sort_order, name"
        ))
        .map_err(|e| internal("list connections", e))?;
    let rows = stmt
        .query_map([], RawConn::from_row)
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .map_err(|e| internal("list connections", e))?;
    Ok(rows)
}

pub(crate) fn now_rfc3339() -> Result<String, DbError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| internal("format timestamp", e))
}

fn parse_uuid(s: &str) -> Result<Uuid, DbError> {
    Uuid::parse_str(s).map_err(|e| internal(format!("invalid id {s:?} in database"), e))
}

/// Text/JSON column values derived from a config.
struct ConnColumns {
    kind: &'static str,
    env_tag: Option<&'static str>,
    secret_mode: &'static str,
    ssh_json: Option<String>,
    ssl_json: Option<String>,
    options_json: String,
}

impl ConnColumns {
    fn from_config(cfg: &ConnectionConfig) -> Result<Self, DbError> {
        let json = |what: &str, r: serde_json::Result<String>| {
            r.map_err(|e| internal(format!("serialize {what}"), e))
        };
        Ok(ConnColumns {
            kind: match cfg.kind {
                DbKind::MySql => "mysql",
                DbKind::Postgres => "postgres",
            },
            env_tag: cfg.env_tag.map(|t| match t {
                EnvTag::Production => "production",
                EnvTag::Staging => "staging",
                EnvTag::Local => "local",
            }),
            secret_mode: match cfg.secret_mode {
                SecretMode::Saved => "saved",
                SecretMode::Ask => "ask",
            },
            ssh_json: cfg
                .ssh
                .as_ref()
                .map(|s| json("ssh_json", serde_json::to_string(s)))
                .transpose()?,
            ssl_json: cfg
                .ssl
                .as_ref()
                .map(|s| json("ssl_json", serde_json::to_string(s)))
                .transpose()?,
            options_json: json("options_json", serde_json::to_string(&cfg.options))?,
        })
    }
}

/// A `connections` row as read from SQLite, before parsing.
struct RawConn {
    id: String,
    group_id: Option<String>,
    name: String,
    sort_order: i64,
    kind: String,
    host: String,
    port: i64,
    username: String,
    default_db: Option<String>,
    env_tag: Option<String>,
    read_only: bool,
    secret_mode: String,
    ssh_json: Option<String>,
    ssl_json: Option<String>,
    options_json: String,
    last_connected_at: Option<String>,
}

impl RawConn {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(RawConn {
            id: r.get(0)?,
            group_id: r.get(1)?,
            name: r.get(2)?,
            sort_order: r.get(3)?,
            kind: r.get(4)?,
            host: r.get(5)?,
            port: r.get(6)?,
            username: r.get(7)?,
            default_db: r.get(8)?,
            env_tag: r.get(9)?,
            read_only: r.get(10)?,
            secret_mode: r.get(11)?,
            ssh_json: r.get(12)?,
            ssl_json: r.get(13)?,
            options_json: r.get(14)?,
            last_connected_at: r.get(15)?,
        })
    }

    fn into_config(self) -> Result<ConnectionConfig, DbError> {
        let bad = |col: &str, v: &str| DbError::Internal {
            message: format!("invalid {col} value {v:?} in database"),
        };
        let kind = match self.kind.as_str() {
            "mysql" => DbKind::MySql,
            "postgres" => DbKind::Postgres,
            other => return Err(bad("kind", other)),
        };
        let env_tag = match self.env_tag.as_deref() {
            None => None,
            Some("production") => Some(EnvTag::Production),
            Some("staging") => Some(EnvTag::Staging),
            Some("local") => Some(EnvTag::Local),
            Some(other) => return Err(bad("env_tag", other)),
        };
        let secret_mode = match self.secret_mode.as_str() {
            "saved" => SecretMode::Saved,
            "ask" => SecretMode::Ask,
            other => return Err(bad("secret_mode", other)),
        };
        let port = u16::try_from(self.port).map_err(|_| bad("port", &self.port.to_string()))?;
        let ssh = self
            .ssh_json
            .as_deref()
            .map(serde_json::from_str::<SshConfig>)
            .transpose()
            .map_err(|e| internal("parse ssh_json", e))?;
        let ssl = self
            .ssl_json
            .as_deref()
            .map(serde_json::from_str::<SslConfig>)
            .transpose()
            .map_err(|e| internal("parse ssl_json", e))?;
        let options: ConnOptions = serde_json::from_str(&self.options_json)
            .map_err(|e| internal("parse options_json", e))?;
        Ok(ConnectionConfig {
            id: parse_uuid(&self.id)?,
            group_id: self.group_id.as_deref().map(parse_uuid).transpose()?,
            name: self.name,
            sort_order: self.sort_order,
            kind,
            host: self.host,
            port,
            username: self.username,
            default_db: self.default_db,
            env_tag,
            read_only: self.read_only,
            secret_mode,
            ssh,
            ssl,
            options,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn repo_is_send_and_sync() {
        assert_send_sync::<ConnectionRepo>();
    }
}
