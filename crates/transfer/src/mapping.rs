//! Source-to-target column mapping (spec §6.2): automatic mapping, the checks the wizard and the
//! backend share, upsert key choices (R14) and the builder options of an import mode.

use std::collections::HashSet;

use dnt_core::meta::{IdentityKind, IndexDetail, TableDetail};
use dnt_core::sql::build::{InsertOptions, OnConflict};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::convert::TargetColumn;

/// What the import does with the rows already in the table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImportMode {
    Append,
    /// Delete every row first (`TRUNCATE` when `truncate`), then insert (R17).
    Replace {
        truncate: bool,
    },
    /// Insert or update by `key`: the primary key or a NOT NULL unique index (R14).
    Upsert {
        key: Vec<String>,
    },
}

/// What a refused row does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ErrorPolicy {
    /// The first error stops the import.
    Stop,
    /// Skip refused rows and log them; more than `max_errors` stops the import (R16).
    Skip { max_errors: u32 },
}

impl ErrorPolicy {
    pub const DEFAULT_MAX_ERRORS: u32 = 1000;
}

/// One target column and the source column (index) it reads, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Mapping {
    pub target: String,
    pub source: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MappingIssue {
    /// The target column concerned, if one is.
    pub column: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
pub struct MappingReport {
    pub errors: Vec<MappingIssue>,
    pub warnings: Vec<MappingIssue>,
}

impl MappingReport {
    fn error(&mut self, column: Option<&str>, message: String) {
        self.errors.push(MappingIssue {
            column: column.map(str::to_string),
            message,
        });
    }

    fn warning(&mut self, column: Option<&str>, message: String) {
        self.warnings.push(MappingIssue {
            column: column.map(str::to_string),
            message,
        });
    }
}

/// A key an upsert can use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct KeyChoice {
    pub name: String,
    pub columns: Vec<String>,
    pub primary: bool,
}

/// Lower-case alphanumerics only: `Customer Name` and `customer_name` are both `customername`.
fn normalised(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// One mapping per target, in target order. Each target takes the first unused source column
/// that matches exactly; the rest then try ASCII case-insensitive matches, then normalised names
/// ([`normalised`]). A source column is used once at most; generated columns stay unmapped.
pub fn auto_map(source: &[String], targets: &[TargetColumn]) -> Vec<Mapping> {
    let mut chosen: Vec<Option<u32>> = vec![None; targets.len()];
    let mut used = vec![false; source.len()];
    let norm_src: Vec<String> = source.iter().map(|s| normalised(s)).collect();
    for stage in 0..3 {
        for (ti, t) in targets.iter().enumerate() {
            if chosen[ti].is_some() || t.generated {
                continue;
            }
            let norm_t = normalised(&t.name);
            let hit = source.iter().enumerate().position(|(si, s)| {
                !used[si]
                    && match stage {
                        0 => *s == t.name,
                        1 => s.eq_ignore_ascii_case(&t.name),
                        _ => !norm_t.is_empty() && norm_src[si] == norm_t,
                    }
            });
            if let Some(si) = hit {
                used[si] = true;
                chosen[ti] = u32::try_from(si).ok();
            }
        }
    }
    targets
        .iter()
        .zip(chosen)
        .map(|(t, source)| Mapping {
            target: t.name.clone(),
            source,
        })
        .collect()
}

/// The checks of a mapping for `mode` (spec §6.2): unknown or repeated targets, generated
/// columns, identity `ALWAYS` without `override_identity`, required columns left unmapped, and
/// for upsert a key that is mapped and is one of `keys` ([`key_choices`]).
pub fn check_mapping(
    m: &[Mapping],
    targets: &[TargetColumn],
    keys: &[KeyChoice],
    mode: &ImportMode,
    override_identity: bool,
) -> MappingReport {
    let mut r = MappingReport::default();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut mapped: Vec<&str> = Vec::new();
    for e in m {
        let Some(t) = targets.iter().find(|t| t.name == e.target) else {
            r.error(
                Some(&e.target),
                format!("Column {} does not exist in the target table.", e.target),
            );
            continue;
        };
        if !seen.insert(t.name.as_str()) {
            r.error(
                Some(&t.name),
                format!("Column {} is mapped more than once.", t.name),
            );
            continue;
        }
        if e.source.is_none() {
            continue;
        }
        if t.generated {
            r.error(
                Some(&t.name),
                format!(
                    "Column {} is generated and cannot be written; leave it unmapped.",
                    t.name
                ),
            );
            continue;
        }
        if t.identity == Some(IdentityKind::Always) && !override_identity {
            r.error(
                Some(&t.name),
                format!(
                    "Column {} is an identity column GENERATED ALWAYS; turn on Override identity \
                     or leave it unmapped.",
                    t.name
                ),
            );
            continue;
        }
        if t.identity.is_some() {
            r.warning(
                Some(&t.name),
                format!(
                    "Values for identity column {} are written as given; its sequence is not \
                     advanced.",
                    t.name
                ),
            );
        }
        mapped.push(t.name.as_str());
    }
    let is_mapped = |name: &str| m.iter().any(|e| e.target == name && e.source.is_some());
    if !m.iter().any(|e| e.source.is_some()) {
        r.error(None, "Map at least one column.".into());
    }
    for t in targets {
        if !t.nullable && !t.has_default && !t.generated && !is_mapped(&t.name) {
            r.error(
                Some(&t.name),
                format!(
                    "Column {} is NOT NULL and has no default; map a source column to it.",
                    t.name
                ),
            );
        }
    }
    if let ImportMode::Upsert { key } = mode {
        check_upsert_key(&mut r, key, targets, keys, &mapped, &is_mapped);
    }
    r
}

fn check_upsert_key(
    r: &mut MappingReport,
    key: &[String],
    targets: &[TargetColumn],
    keys: &[KeyChoice],
    mapped: &[&str],
    is_mapped: &dyn Fn(&str) -> bool,
) {
    if key.is_empty() {
        r.error(None, "Choose the key columns of the upsert.".into());
        return;
    }
    let before = r.errors.len();
    let mut listed: HashSet<&str> = HashSet::new();
    for k in key {
        if !listed.insert(k.as_str()) {
            r.error(Some(k), format!("Key column {k} is listed more than once."));
        } else if !targets.iter().any(|t| &t.name == k) {
            r.error(
                Some(k),
                format!("Key column {k} does not exist in the target table."),
            );
        } else if !is_mapped(k) {
            r.error(
                Some(k),
                format!("Key column {k} must be mapped to a source column."),
            );
        }
    }
    let wanted: HashSet<&str> = key.iter().map(String::as_str).collect();
    let valid = keys.iter().any(|kc| {
        kc.columns.len() == wanted.len() && kc.columns.iter().all(|c| wanted.contains(c.as_str()))
    });
    if !valid {
        r.error(
            None,
            "The upsert key must be the primary key or a unique index whose columns are all \
             NOT NULL."
                .into(),
        );
    }
    let updated = mapped
        .iter()
        .any(|c| !wanted.contains(c) && targets.iter().any(|t| t.name == *c && updatable(t)));
    if r.errors.len() == before && !updated {
        r.warning(
            None,
            "Only key columns are mapped: rows whose key already exists are left unchanged.".into(),
        );
    }
}

/// A column an upsert may `SET`: PostgreSQL refuses `SET` on an identity `ALWAYS` column (even
/// with `OVERRIDING SYSTEM VALUE`) and on a generated column.
fn updatable(t: &TargetColumn) -> bool {
    !t.generated && t.identity != Some(IdentityKind::Always)
}

/// The builder options of `mode` over the mapped columns (in insert order). Upsert updates
/// every mapped column that is not part of the key and can be updated (not identity `ALWAYS`,
/// not generated); with none left PG writes `DO NOTHING`. `override_identity` becomes PostgreSQL
/// `OVERRIDING SYSTEM VALUE`: pass `false` on MySQL (the builder refuses it there).
pub fn insert_options(
    mode: &ImportMode,
    mapped: &[TargetColumn],
    override_identity: bool,
) -> InsertOptions {
    let on_conflict = match mode {
        ImportMode::Append | ImportMode::Replace { .. } => OnConflict::Error,
        ImportMode::Upsert { key } => OnConflict::Update {
            key: key.clone(),
            update: mapped
                .iter()
                .filter(|c| !key.contains(&c.name) && updatable(c))
                .map(|c| c.name.clone())
                .collect(),
        },
    };
    InsertOptions {
        overriding_system_value: override_identity,
        on_conflict,
    }
}

/// Keys an upsert can use (R14): the primary key, then every unique, non-partial index on plain
/// columns (no expression, no prefix) whose columns are all NOT NULL.
pub fn key_choices(detail: &TableDetail) -> Vec<KeyChoice> {
    let not_null = |c: &str| detail.columns.iter().any(|cd| cd.name == c && !cd.nullable);
    let plain = |ix: &IndexDetail| {
        !ix.columns.is_empty()
            && !ix.partial
            && ix.expression.is_none()
            && ix
                .parts
                .iter()
                .all(|p| p.column.is_some() && p.expression.is_none() && p.prefix.is_none())
    };
    let mut out = Vec::new();
    let pk = detail
        .primary_key
        .as_ref()
        .or_else(|| detail.indexes.iter().find(|ix| ix.primary));
    if let Some(pk) = pk.filter(|pk| plain(pk)) {
        out.push(KeyChoice {
            name: pk.name.clone(),
            columns: pk.columns.clone(),
            primary: true,
        });
    }
    let pk_name = pk.map(|p| p.name.as_str());
    for ix in &detail.indexes {
        if ix.primary || Some(ix.name.as_str()) == pk_name || !ix.unique || !plain(ix) {
            continue;
        }
        if ix.columns.iter().all(|c| not_null(c)) {
            out.push(KeyChoice {
                name: ix.name.clone(),
                columns: ix.columns.clone(),
                primary: false,
            });
        }
    }
    out
}
