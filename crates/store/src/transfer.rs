//! Import flow shared by every source (`.dntconn`, Navicat `.ncx`): a parser produces an
//! [`ImportBundle`], [`preview`] reports conflicts with what is already stored, and [`apply`]
//! writes the bundle under a [`ConflictPolicy`].

use crate::repo::{
    connection_names_in_group, find_connection, group_exists, insert_connection, insert_group,
    set_or_keep, update_connection_row, ConnectionRepo, Group, NewSecrets,
};
use dnt_core::config::ConnectionConfig;
use dnt_core::error::DbError;
use dnt_core::secret::SecretKind;
use dnt_core::server::DbKind;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::{HashMap, HashSet};
use std::fmt;
use uuid::Uuid;

/// Parsed import file. Holds secrets, so it is never serialized.
pub struct ImportBundle {
    pub source: ImportSource,
    pub groups: Vec<Group>,
    pub connections: Vec<ConnectionConfig>,
    pub secrets: HashMap<Uuid, NewSecrets>,
    pub skipped: Vec<SkippedItem>,
    /// Per-connection warnings from the parser, shown in the preview.
    pub warnings: HashMap<Uuid, Vec<String>>,
}

/// Lists which connections carry secrets, never their values.
impl fmt::Debug for ImportBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut with_secrets: Vec<&Uuid> = self.secrets.keys().collect();
        with_secrets.sort();
        f.debug_struct("ImportBundle")
            .field("source", &self.source)
            .field("groups", &self.groups)
            .field("connections", &self.connections)
            .field("secrets", &with_secrets)
            .field("skipped", &self.skipped)
            .field("warnings", &self.warnings)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ImportSource {
    Dntconn,
    Navicat,
}

/// An entry of the import file that cannot be imported at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SkippedItem {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ImportPreviewRow {
    pub id: Uuid,
    pub name: String,
    pub kind: DbKind,
    pub host: String,
    /// Destination group names joined by " / "; empty for the root.
    pub group_path: String,
    pub conflict: Option<ConflictKind>,
    pub has_password: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// A stored connection has the same id; resolved by the [`ConflictPolicy`].
    SameId,
    /// A stored connection in the destination group has the same name; the import is renamed
    /// `<name> (2)`, `(3)`, ...
    SameName,
}

/// What to do with an imported connection whose id is already stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    Skip,
    Overwrite,
    Copy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ImportPreview {
    pub rows: Vec<ImportPreviewRow>,
    pub skipped: Vec<SkippedItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ImportResult {
    pub imported: u32,
    pub overwritten: u32,
    /// Conflicts skipped by policy plus entries the parser could not import.
    pub skipped: u32,
    /// Stored connections replaced by `Overwrite`: an open one must be closed (it still
    /// uses the old settings).
    pub overwritten_ids: Vec<Uuid>,
}

pub fn preview(repo: &ConnectionRepo, bundle: &ImportBundle) -> Result<ImportPreview, DbError> {
    let (groups, configs) = repo.snapshot()?;
    let stored_groups: HashSet<Uuid> = groups.iter().map(|g| g.id).collect();
    let bundle_groups: HashSet<Uuid> = bundle.groups.iter().map(|g| g.id).collect();
    // Stored groups win: an imported group with a known id is reused as is.
    let mut tree: HashMap<Uuid, (&str, Option<Uuid>)> = HashMap::new();
    for g in bundle.groups.iter().chain(&groups) {
        tree.insert(g.id, (g.name.as_str(), g.parent_id));
    }
    let rows = bundle
        .connections
        .iter()
        .map(|c| {
            let dest = c
                .group_id
                .filter(|g| stored_groups.contains(g) || bundle_groups.contains(g));
            let conflict = if configs.iter().any(|e| e.id == c.id) {
                Some(ConflictKind::SameId)
            } else if configs
                .iter()
                .any(|e| e.group_id == dest && e.name == c.name)
            {
                Some(ConflictKind::SameName)
            } else {
                None
            };
            let mut warnings = bundle.warnings.get(&c.id).cloned().unwrap_or_default();
            if let Some(path) = c.ssh.as_ref().and_then(|s| s.key_path.as_ref()) {
                if !path.exists() {
                    warnings.push(format!("SSH key not found: {}", path.display()));
                }
            }
            ImportPreviewRow {
                id: c.id,
                name: c.name.clone(),
                kind: c.kind,
                host: c.host.clone(),
                group_path: group_path(&tree, dest),
                conflict,
                has_password: bundle
                    .secrets
                    .get(&c.id)
                    .is_some_and(|s| s.db_password.is_some()),
                warnings,
            }
        })
        .collect();
    Ok(ImportPreview {
        rows,
        skipped: bundle.skipped.clone(),
    })
}

/// Writes the bundle in one SQLite transaction. Groups whose id is already stored are reused;
/// connections whose name is taken in their destination group get a ` (n)` suffix.
pub fn apply(
    repo: &ConnectionRepo,
    bundle: ImportBundle,
    policy: ConflictPolicy,
) -> Result<ImportResult, DbError> {
    let ImportBundle {
        groups,
        connections,
        mut secrets,
        skipped,
        ..
    } = bundle;
    for c in &connections {
        c.validate()?;
    }
    let mut result = ImportResult {
        imported: 0,
        overwritten: 0,
        skipped: skipped.len() as u32,
        overwritten_ids: Vec::new(),
    };
    repo.transaction(|tx| {
        import_groups(tx, groups)?;
        let mut created = Vec::new();
        let mut to_store = Vec::new();
        for mut cfg in connections {
            if let Some(g) = cfg.group_id {
                if !group_exists(tx, g)? {
                    cfg.group_id = None;
                }
            }
            let cfg_secrets = secrets.remove(&cfg.id);
            let mut overwritten = false;
            if find_connection(tx, cfg.id)?.is_some() {
                match policy {
                    ConflictPolicy::Skip => {
                        result.skipped += 1;
                        continue;
                    }
                    ConflictPolicy::Overwrite => {
                        cfg.name = unique_name(tx, &cfg)?;
                        update_connection_row(tx, &mut cfg)?;
                        result.overwritten += 1;
                        result.overwritten_ids.push(cfg.id);
                        overwritten = true;
                    }
                    ConflictPolicy::Copy => {
                        cfg.id = Uuid::new_v4();
                        cfg.name = unique_name(tx, &cfg)?;
                        insert_connection(tx, &mut cfg)?;
                        created.push(cfg.id);
                        result.imported += 1;
                    }
                }
            } else {
                cfg.name = unique_name(tx, &cfg)?;
                insert_connection(tx, &mut cfg)?;
                created.push(cfg.id);
                result.imported += 1;
            }
            // An overwrite always goes through `apply_secrets`, even with nothing to set, so a
            // switch to Ask mode clears the secrets stored for the old config.
            match cfg_secrets {
                Some(s) => to_store.push((cfg.id, cfg.secret_mode, s)),
                None if overwritten => to_store.push((
                    cfg.id,
                    cfg.secret_mode,
                    NewSecrets {
                        db_password: None,
                        ssh_password: None,
                        ssh_key_passphrase: None,
                    },
                )),
                None => {}
            }
        }
        // Keychain writes last: if one fails, the transaction rolls back and the secrets of
        // connections created here are removed (best effort).
        for (id, mode, s) in to_store {
            let stored = repo.apply_secrets(
                id,
                mode,
                [
                    (SecretKind::DbPassword, set_or_keep(s.db_password)),
                    (SecretKind::SshPassword, set_or_keep(s.ssh_password)),
                    (
                        SecretKind::SshKeyPassphrase,
                        set_or_keep(s.ssh_key_passphrase),
                    ),
                ],
            );
            if let Err(e) = stored {
                for id in &created {
                    let _ = repo.secret_store().delete_all(*id);
                }
                return Err(e);
            }
        }
        Ok(())
    })?;
    Ok(result)
}

/// Creates the bundle's groups parents-first, keeping their ids. Groups already stored are
/// reused untouched; a parent that is neither stored nor imported (or a parent cycle) puts the
/// group at the root.
fn import_groups(conn: &Connection, groups: Vec<Group>) -> Result<(), DbError> {
    let in_bundle: HashSet<Uuid> = groups.iter().map(|g| g.id).collect();
    let mut pending = groups;
    pending.sort_by_key(|g| g.sort_order);
    let mut placed = HashSet::new();
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .position(|g| {
                g.parent_id
                    .is_none_or(|p| placed.contains(&p) || !in_bundle.contains(&p))
            })
            .unwrap_or(0);
        let mut group = pending.remove(ready);
        placed.insert(group.id);
        if group_exists(conn, group.id)? {
            continue;
        }
        if let Some(p) = group.parent_id {
            if !group_exists(conn, p)? {
                group.parent_id = None;
            }
        }
        insert_group(conn, group)?;
    }
    Ok(())
}

/// `cfg.name`, or the first free `<name> (n)` (n ≥ 2) among the other connections of its group.
fn unique_name(conn: &Connection, cfg: &ConnectionConfig) -> Result<String, DbError> {
    let taken = connection_names_in_group(conn, cfg.group_id, cfg.id)?;
    if !taken.contains(&cfg.name) {
        return Ok(cfg.name.clone());
    }
    Ok((2..)
        .map(|n| format!("{} ({n})", cfg.name))
        .find(|candidate| !taken.contains(candidate))
        .expect("finitely many names are taken"))
}

fn group_path(tree: &HashMap<Uuid, (&str, Option<Uuid>)>, group: Option<Uuid>) -> String {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = group;
    while let Some(id) = cur {
        let Some((name, parent)) = tree.get(&id) else {
            break;
        };
        if !seen.insert(id) {
            break;
        }
        names.push(*name);
        cur = *parent;
    }
    names.reverse();
    names.join(" / ")
}
