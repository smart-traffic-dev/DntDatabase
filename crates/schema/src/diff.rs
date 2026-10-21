//! The changes that turn the stored table into the draft (spec §4.2).

use crate::model::*;
use serde::{Deserialize, Serialize};
use specta::Type;

/// One change, in the order renderers emit them (drops first, adds last).
// The variants carry whole definitions by value (the IPC shape); lists are short.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    CreateTable,
    RenameTable {
        from: String,
        to: String,
    },
    SetOptions {
        old: TableOptionsDef,
        new: TableOptionsDef,
    },
    SetComment {
        comment: Option<String>,
    },
    DropForeignKey {
        name: String,
    },
    DropCheck {
        name: String,
    },
    DropIndex {
        name: String,
    },
    DropPrimaryKey {
        name: Option<String>,
    },
    RenameColumn {
        from: String,
        to: String,
    },
    AlterColumn {
        name: String,
        old: ColumnDef,
        new: ColumnDef,
    },
    MoveColumn {
        name: String,
        position: Position,
    },
    DropColumn {
        name: String,
    },
    AddColumn {
        column: ColumnDef,
        position: Position,
    },
    AddPrimaryKey {
        key: KeyDef,
    },
    AddIndex {
        index: IndexDef,
    },
    RenameIndex {
        from: String,
        to: String,
    },
    AddCheck {
        check: CheckDef,
    },
    AddForeignKey {
        fk: FkDef,
    },
}

/// Where a column goes (MySQL `FIRST` / `AFTER x`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", content = "column", rename_all = "snake_case")]
pub enum Position {
    First,
    After(String),
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum DiffError {
    /// The draft changes a locked element, or a column a locked element uses.
    Locked { element: ElementRef, reason: String },
    /// The draft does not fit the stored table (e.g. an `original` that names nothing).
    Invalid(Vec<DraftError>),
}

/// The changes from `old` (the stored table) to `new` (the draft).
///
/// Elements are matched by `original` (`None` = new); an old element no draft element
/// claims is dropped. The result follows the [`Change`] variant order; within a kind,
/// drops follow the old ordinal and everything else the new ordinal. "Changed" means
/// any field differs except `original` and `locked`; column names that a rename
/// changes are followed in key, index and FK column lists, so a rename alone does
/// not rebuild them.
///
/// Refused with [`DiffError::Locked`]: any change to an element locked in `old` or
/// `new`, renaming or dropping a column that a locked element uses (a column of a
/// locked key, index or FK, or a word-boundary, case-insensitive mention in a locked
/// index expression, a locked check or a generated column), and any change at all
/// to a table with a `read_only_reason`. A locked column never moves: the columns
/// that keep their place are chosen to include every locked one when possible.
///
/// Refused with [`DiffError::Invalid`]: `original`s that do not fit the stored table,
/// and name collisions inside one diff (ruling T6-2: swaps, chains, a rename onto
/// a dropped element's name, a new element reusing a name another element holds or
/// renames away), which the user applies in two steps.
pub fn diff(old: &TableDef, new: &TableDef) -> Result<Vec<Change>, DiffError> {
    let Some(old_name) = &old.original_name else {
        return Ok(vec![Change::CreateTable]);
    };

    let mut errs = Vec::new();
    if new.original_name.as_ref() != Some(old_name) {
        errs.push(DraftError {
            element: ElementRef::Table,
            field: Some("original_name".into()),
            message: "The draft belongs to another table.".into(),
        });
    }
    let cols = pair(&old.columns, &new.columns, &mut errs);
    let idxs = pair(&old.indexes, &new.indexes, &mut errs);
    let fks = pair(&old.foreign_keys, &new.foreign_keys, &mut errs);
    let checks = pair(&old.checks, &new.checks, &mut errs);
    if let Some(k) = &new.primary_key {
        let claims = k.original.is_some()
            && k.original.as_deref()
                != old.primary_key.as_ref().and_then(|o| o.original.as_deref());
        if claims {
            errs.push(bad_original(ElementRef::PrimaryKey));
        }
        if k.locked && k.original.is_none() {
            errs.push(new_locked(ElementRef::PrimaryKey));
        }
    }
    if errs.is_empty() {
        name_collisions(old, new, &cols, &idxs, &fks, &checks, &mut errs);
    }
    if !errs.is_empty() {
        return Err(DiffError::Invalid(errs));
    }

    // Old column name → new name, for the columns this draft renames.
    let renames: Vec<(&str, &str)> = cols
        .pairs
        .iter()
        .map(|&(o, n)| (old.columns[o].name.as_str(), new.columns[n].name.as_str()))
        .filter(|(o, n)| o != n)
        .collect();
    // Case-insensitive, like `locked_user`.
    let follow = |name: &String| -> String {
        let lower = name.to_lowercase();
        renames
            .iter()
            .find(|(o, _)| o.to_lowercase() == lower)
            .map_or_else(|| name.clone(), |(_, n)| n.to_string())
    };

    // Columns.
    let mut out = Out::default();
    for &(o, n) in &cols.pairs {
        let (oc, nc) = (&old.columns[o], &new.columns[n]);
        let renamed = oc.name != nc.name;
        let altered = column_content(oc) != column_content(nc);
        if (renamed || altered) && (oc.locked || nc.locked) {
            return Err(locked(ElementRef::Column(oc.name.clone()), "column"));
        }
        if renamed {
            out.rename_column.push(Change::RenameColumn {
                from: oc.name.clone(),
                to: nc.name.clone(),
            });
        }
        if altered {
            out.alter_column.push(Change::AlterColumn {
                name: nc.name.clone(),
                old: oc.clone(),
                new: nc.clone(),
            });
        }
    }
    for &o in &cols.dropped {
        let oc = &old.columns[o];
        if oc.locked {
            return Err(locked(ElementRef::Column(oc.name.clone()), "column"));
        }
        out.drop_column.push(Change::DropColumn {
            name: oc.name.clone(),
        });
    }
    out.move_column = moves(&cols.pairs, old, new)?;
    for &n in &cols.added {
        let position = match n {
            0 => Position::First,
            _ => Position::After(new.columns[n - 1].name.clone()),
        };
        out.add_column.push(Change::AddColumn {
            column: new.columns[n].clone(),
            position,
        });
    }

    // Primary key.
    match (&old.primary_key, &new.primary_key) {
        (None, None) => {}
        (Some(ok), None) => out.drop_pk(ok)?,
        (None, Some(nk)) => out
            .add_primary_key
            .push(Change::AddPrimaryKey { key: nk.clone() }),
        (Some(ok), Some(nk)) => {
            let kept = nk.original.is_some();
            let same = kept
                && KeyDef {
                    columns: ok.columns.iter().map(follow).collect(),
                    ..key_content(ok)
                } == key_content(nk);
            if !same {
                if kept && nk.locked {
                    return Err(locked(ElementRef::PrimaryKey, "primary key"));
                }
                out.drop_pk(ok)?;
                out.add_primary_key
                    .push(Change::AddPrimaryKey { key: nk.clone() });
            }
        }
    }

    // Indexes: a name-only change is a rename, any other change drop + add.
    for &(o, n) in &idxs.pairs {
        let (oi, ni) = (&old.indexes[o], &new.indexes[n]);
        let followed = IndexDef {
            parts: oi
                .parts
                .iter()
                .map(|p| IndexPartDef {
                    column: p.column.as_ref().map(follow),
                    ..p.clone()
                })
                .collect(),
            include: oi.include.iter().map(follow).collect(),
            ..index_content(oi)
        };
        let changed = followed != index_content(ni);
        if (changed || oi.name != ni.name) && (oi.locked || ni.locked) {
            return Err(locked(ElementRef::Index(oi.name.clone()), "index"));
        }
        if changed {
            out.drop_index.push((
                o,
                Change::DropIndex {
                    name: oi.name.clone(),
                },
            ));
            out.add_index
                .push((n, Change::AddIndex { index: ni.clone() }));
        } else if oi.name != ni.name {
            out.rename_index.push(Change::RenameIndex {
                from: oi.name.clone(),
                to: ni.name.clone(),
            });
        }
    }
    for &o in &idxs.dropped {
        let oi = &old.indexes[o];
        if oi.locked {
            return Err(locked(ElementRef::Index(oi.name.clone()), "index"));
        }
        out.drop_index.push((
            o,
            Change::DropIndex {
                name: oi.name.clone(),
            },
        ));
    }
    for &n in &idxs.added {
        let index = new.indexes[n].clone();
        out.add_index.push((n, Change::AddIndex { index }));
    }

    // Foreign keys: any change (name included) is drop + add.
    for &(o, n) in &fks.pairs {
        let (of, nf) = (&old.foreign_keys[o], &new.foreign_keys[n]);
        let followed = FkDef {
            columns: of.columns.iter().map(follow).collect(),
            ..fk_content(of)
        };
        if followed != fk_content(nf) {
            if of.locked || nf.locked {
                return Err(locked(
                    ElementRef::ForeignKey(of.name.clone()),
                    "foreign key",
                ));
            }
            out.drop_fk.push((
                o,
                Change::DropForeignKey {
                    name: of.name.clone(),
                },
            ));
            out.add_fk
                .push((n, Change::AddForeignKey { fk: nf.clone() }));
        }
    }
    for &o in &fks.dropped {
        let of = &old.foreign_keys[o];
        if of.locked {
            return Err(locked(
                ElementRef::ForeignKey(of.name.clone()),
                "foreign key",
            ));
        }
        out.drop_fk.push((
            o,
            Change::DropForeignKey {
                name: of.name.clone(),
            },
        ));
    }
    for &n in &fks.added {
        let fk = new.foreign_keys[n].clone();
        out.add_fk.push((n, Change::AddForeignKey { fk }));
    }

    // Checks: any change is drop + add.
    for &(o, n) in &checks.pairs {
        let (oc, nc) = (&old.checks[o], &new.checks[n]);
        if check_content(oc) != check_content(nc) {
            if oc.locked || nc.locked {
                return Err(locked(ElementRef::Check(oc.name.clone()), "check"));
            }
            out.drop_check.push((
                o,
                Change::DropCheck {
                    name: oc.name.clone(),
                },
            ));
            out.add_check
                .push((n, Change::AddCheck { check: nc.clone() }));
        }
    }
    for &o in &checks.dropped {
        let oc = &old.checks[o];
        if oc.locked {
            return Err(locked(ElementRef::Check(oc.name.clone()), "check"));
        }
        out.drop_check.push((
            o,
            Change::DropCheck {
                name: oc.name.clone(),
            },
        ));
    }
    for &n in &checks.added {
        let check = new.checks[n].clone();
        out.add_check.push((n, Change::AddCheck { check }));
    }

    // Columns a locked element uses cannot be renamed or dropped.
    let gone = renames
        .iter()
        .map(|(o, _)| *o)
        .chain(cols.dropped.iter().map(|&o| old.columns[o].name.as_str()));
    for name in gone {
        if let Some(element) = locked_user(old, name) {
            return Err(DiffError::Locked {
                element,
                reason: format!("Column \"{name}\" is used by a locked element."),
            });
        }
    }

    // Table level.
    if new.name != old.name {
        out.table.push(Change::RenameTable {
            from: old.name.clone(),
            to: new.name.clone(),
        });
    }
    let options = |o: &TableOptionsDef| TableOptionsDef {
        locked: false,
        ..o.clone()
    };
    if options(&old.options) != options(&new.options) {
        if old.options.locked || new.options.locked {
            return Err(locked(ElementRef::Options, "table options"));
        }
        out.table.push(Change::SetOptions {
            old: old.options.clone(),
            new: new.options.clone(),
        });
    }
    if old.comment != new.comment {
        out.table.push(Change::SetComment {
            comment: new.comment.clone(),
        });
    }

    let changes = out.finish();
    if let (Some(reason), false) = (&old.read_only_reason, changes.is_empty()) {
        return Err(DiffError::Locked {
            element: ElementRef::Table,
            reason: reason.clone(),
        });
    }
    Ok(changes)
}

/// The changes of each kind, collected out of order and assembled in enum order.
#[derive(Default)]
struct Out {
    /// `RenameTable`, `SetOptions`, `SetComment` (pushed in that order).
    table: Vec<Change>,
    drop_fk: Vec<(usize, Change)>,
    drop_check: Vec<(usize, Change)>,
    drop_index: Vec<(usize, Change)>,
    drop_pk: Option<Change>,
    rename_column: Vec<Change>,
    alter_column: Vec<Change>,
    move_column: Vec<Change>,
    drop_column: Vec<Change>,
    add_column: Vec<Change>,
    add_primary_key: Vec<Change>,
    add_index: Vec<(usize, Change)>,
    rename_index: Vec<Change>,
    add_check: Vec<(usize, Change)>,
    add_fk: Vec<(usize, Change)>,
}

impl Out {
    fn drop_pk(&mut self, key: &KeyDef) -> Result<(), DiffError> {
        if key.locked {
            return Err(locked(ElementRef::PrimaryKey, "primary key"));
        }
        self.drop_pk = Some(Change::DropPrimaryKey {
            name: key.name.clone(),
        });
        Ok(())
    }

    /// Assembles the changes in `Change` order; the `(ordinal, change)` lists are
    /// sorted by their ordinal (old for drops, new for adds).
    fn finish(self) -> Vec<Change> {
        let sorted = |mut v: Vec<(usize, Change)>| {
            v.sort_by_key(|(i, _)| *i);
            v.into_iter().map(|(_, c)| c)
        };
        let mut out = self.table;
        out.extend(sorted(self.drop_fk));
        out.extend(sorted(self.drop_check));
        out.extend(sorted(self.drop_index));
        out.extend(self.drop_pk);
        out.extend(self.rename_column);
        out.extend(self.alter_column);
        out.extend(self.move_column);
        out.extend(self.drop_column);
        out.extend(self.add_column);
        out.extend(self.add_primary_key);
        out.extend(sorted(self.add_index));
        out.extend(self.rename_index);
        out.extend(sorted(self.add_check));
        out.extend(sorted(self.add_fk));
        out
    }
}

/// An element matched by `original`.
trait Element {
    fn original(&self) -> Option<&str>;
    fn name(&self) -> &str;
    fn locked(&self) -> bool;
    fn element(&self) -> ElementRef;
}

macro_rules! element {
    ($t:ty, $variant:ident) => {
        impl Element for $t {
            fn original(&self) -> Option<&str> {
                self.original.as_deref()
            }
            fn name(&self) -> &str {
                &self.name
            }
            fn locked(&self) -> bool {
                self.locked
            }
            fn element(&self) -> ElementRef {
                ElementRef::$variant(self.name.clone())
            }
        }
    };
}
element!(ColumnDef, Column);
element!(IndexDef, Index);
element!(FkDef, ForeignKey);
element!(CheckDef, Check);

/// How the elements of one kind match up.
struct Pairing {
    /// `(old index, new index)`, in new order.
    pairs: Vec<(usize, usize)>,
    /// Old indexes no draft element claims, in old order.
    dropped: Vec<usize>,
    /// New indexes with `original: None`, in new order.
    added: Vec<usize>,
}

fn pair<T: Element>(old: &[T], new: &[T], errs: &mut Vec<DraftError>) -> Pairing {
    let key = |e: &T| e.original().unwrap_or(e.name()).to_string();
    let mut claimed = vec![false; old.len()];
    let mut pairs = Vec::new();
    let mut added = Vec::new();
    for (n, e) in new.iter().enumerate() {
        let Some(original) = e.original() else {
            if e.locked() {
                errs.push(new_locked(e.element()));
            }
            added.push(n);
            continue;
        };
        match old.iter().position(|o| key(o) == original) {
            None => errs.push(bad_original(e.element())),
            Some(o) if claimed[o] => errs.push(DraftError {
                element: e.element(),
                field: Some("original".into()),
                message: "Another element already stands for this one.".into(),
            }),
            Some(o) => {
                claimed[o] = true;
                pairs.push((o, n));
            }
        }
    }
    let dropped = (0..old.len()).filter(|&o| !claimed[o]).collect();
    Pairing {
        pairs,
        dropped,
        added,
    }
}

/// Names in one namespace: the old elements' original names, and the draft
/// elements with the old element they stand for.
#[derive(Default)]
struct Namespace {
    /// Lowercased original name of each old element (its index is its id).
    olds: Vec<String>,
    /// `(old id, draft name, element, claims)`: `claims` when the name is new or
    /// changed, i.e. the draft takes a name the stored table does not give it.
    drafts: Vec<(Option<usize>, String, ElementRef, bool)>,
}

impl Namespace {
    fn add<T: Element>(&mut self, old: &[T], new: &[T], p: &Pairing) {
        let base = self.olds.len();
        let key = |e: &T| e.original().unwrap_or(e.name()).to_string();
        self.olds.extend(old.iter().map(|e| key(e).to_lowercase()));
        for &(o, n) in &p.pairs {
            let e = &new[n];
            let claims = e.name() != key(&old[o]);
            self.drafts
                .push((Some(base + o), e.name().to_string(), e.element(), claims));
        }
        for &n in &p.added {
            let e = &new[n];
            self.drafts
                .push((None, e.name().to_string(), e.element(), true));
        }
    }

    /// A name the draft takes must not be an original name of another old element
    /// (swaps, chains, renames onto dropped elements, reuse of a name renamed away)
    /// nor the draft name of another element.
    fn check(&self, errs: &mut Vec<DraftError>) {
        for (i, (me, name, element, claims)) in self.drafts.iter().enumerate() {
            if !claims {
                continue;
            }
            let lower = name.to_lowercase();
            let held = self
                .olds
                .iter()
                .enumerate()
                .any(|(k, o)| Some(k) != *me && *o == lower);
            let taken = self
                .drafts
                .iter()
                .enumerate()
                .any(|(j, d)| j != i && d.1.to_lowercase() == lower);
            if held || taken {
                errs.push(DraftError {
                    element: element.clone(),
                    field: Some("name".into()),
                    message: format!(
                        "\"{name}\" is still used by another element during this change. \
                         Apply it in two steps."
                    ),
                });
            }
        }
    }
}

/// Name collisions inside one diff (ruling T6-2). Columns are one namespace; keys,
/// indexes, foreign keys and checks share another (PostgreSQL keeps index and
/// constraint names in one relation namespace).
fn name_collisions(
    old: &TableDef,
    new: &TableDef,
    cols: &Pairing,
    idxs: &Pairing,
    fks: &Pairing,
    checks: &Pairing,
    errs: &mut Vec<DraftError>,
) {
    let mut columns = Namespace::default();
    columns.add(&old.columns, &new.columns, cols);
    columns.check(errs);

    let mut constraints = Namespace::default();
    constraints.add(&old.indexes, &new.indexes, idxs);
    constraints.add(&old.foreign_keys, &new.foreign_keys, fks);
    constraints.add(&old.checks, &new.checks, checks);
    let old_pk = old
        .primary_key
        .as_ref()
        .and_then(|k| k.original.as_ref().or(k.name.as_ref()));
    let pk_id = old_pk.map(|key| {
        constraints.olds.push(key.to_lowercase());
        constraints.olds.len() - 1
    });
    if let Some(name) = new.primary_key.as_ref().and_then(|k| k.name.as_ref()) {
        // A replacement key stands for the old one: it is dropped before the add.
        let claims = new.primary_key.as_ref().unwrap().original.is_none() || Some(name) != old_pk;
        constraints
            .drafts
            .push((pk_id, name.clone(), ElementRef::PrimaryKey, claims));
    }
    constraints.check(errs);
}

fn bad_original(element: ElementRef) -> DraftError {
    DraftError {
        element,
        field: Some("original".into()),
        message: "This element does not exist on the server.".into(),
    }
}

fn new_locked(element: ElementRef) -> DraftError {
    DraftError {
        element,
        field: Some("locked".into()),
        message: "A new element cannot be locked.".into(),
    }
}

fn locked(element: ElementRef, what: &str) -> DiffError {
    DiffError::Locked {
        element,
        reason: format!("This {what} is locked and cannot be changed."),
    }
}

// Content comparisons: every field but `original` and `locked` (and, where a
// rename is its own change, the name).
fn column_content(c: &ColumnDef) -> ColumnDef {
    ColumnDef {
        original: None,
        name: String::new(),
        locked: false,
        ..c.clone()
    }
}

fn key_content(k: &KeyDef) -> KeyDef {
    KeyDef {
        original: None,
        locked: false,
        ..k.clone()
    }
}

fn index_content(i: &IndexDef) -> IndexDef {
    IndexDef {
        original: None,
        name: String::new(),
        locked: false,
        ..i.clone()
    }
}

fn fk_content(f: &FkDef) -> FkDef {
    FkDef {
        original: None,
        locked: false,
        ..f.clone()
    }
}

fn check_content(c: &CheckDef) -> CheckDef {
    CheckDef {
        original: None,
        locked: false,
        ..c.clone()
    }
}

/// `MoveColumn`s for the kept columns whose relative order changed: the columns
/// outside the run that stays in order move, each after the previous kept column
/// of the draft (new columns are added after the moves run). The run keeps as many
/// locked columns as possible, then as many columns as possible; a locked column
/// that still has to move refuses the diff.
fn moves(
    pairs: &[(usize, usize)],
    old: &TableDef,
    new: &TableDef,
) -> Result<Vec<Change>, DiffError> {
    let seq: Vec<usize> = pairs.iter().map(|&(o, _)| o).collect();
    let is_locked = |&(o, n): &(usize, usize)| old.columns[o].locked || new.columns[n].locked;
    let weight: Vec<usize> = pairs
        .iter()
        .map(|p| if is_locked(p) { pairs.len() + 1 } else { 1 })
        .collect();
    let stay = stable_run(&seq, &weight);
    let mut out = Vec::new();
    for (i, &(o, n)) in pairs.iter().enumerate() {
        if stay[i] {
            continue;
        }
        if is_locked(&pairs[i]) {
            return Err(DiffError::Locked {
                element: ElementRef::Column(old.columns[o].name.clone()),
                reason: "This column is locked and cannot be moved.".into(),
            });
        }
        let position = match i {
            0 => Position::First,
            _ => Position::After(new.columns[pairs[i - 1].1].name.clone()),
        };
        out.push(Change::MoveColumn {
            name: new.columns[n].name.clone(),
            position,
        });
    }
    Ok(out)
}

/// Marks the strictly increasing subsequence of `seq` with the largest total
/// `weight` (the earliest one on ties).
fn stable_run(seq: &[usize], weight: &[usize]) -> Vec<bool> {
    let mut score = weight.to_vec();
    let mut prev = vec![usize::MAX; seq.len()];
    for i in 0..seq.len() {
        for j in 0..i {
            if seq[j] < seq[i] && score[j] + weight[i] > score[i] {
                score[i] = score[j] + weight[i];
                prev[i] = j;
            }
        }
    }
    let mut stay = vec![false; seq.len()];
    let best = (0..seq.len()).max_by_key(|&i| (score[i], std::cmp::Reverse(i)));
    let mut at = best.unwrap_or(usize::MAX);
    while at != usize::MAX {
        stay[at] = true;
        at = prev[at];
    }
    stay
}

/// The first locked element of `old` that uses column `name`.
fn locked_user(old: &TableDef, name: &str) -> Option<ElementRef> {
    let lower = name.to_lowercase();
    let same = |c: &String| c.to_lowercase() == lower;
    if let Some(k) = old.primary_key.as_ref().filter(|k| k.locked) {
        if k.columns.iter().any(same) {
            return Some(ElementRef::PrimaryKey);
        }
    }
    for i in old.indexes.iter().filter(|i| i.locked) {
        let uses = i.include.iter().any(same)
            || i.parts.iter().any(|p| {
                p.column.as_ref().is_some_and(same)
                    || p.expression.as_deref().is_some_and(|e| mentions(e, name))
            });
        if uses {
            return Some(ElementRef::Index(i.name.clone()));
        }
    }
    for f in old.foreign_keys.iter().filter(|f| f.locked) {
        if f.columns.iter().any(same) {
            return Some(ElementRef::ForeignKey(f.name.clone()));
        }
    }
    for c in old.checks.iter().filter(|c| c.locked) {
        if mentions(&c.expression, name) {
            return Some(ElementRef::Check(c.name.clone()));
        }
    }
    for c in &old.columns {
        let uses = c.name != name && c.generated.as_deref().is_some_and(|g| mentions(g, name));
        if uses {
            return Some(ElementRef::Column(c.name.clone()));
        }
    }
    None
}

/// `text` mentions `name` as a whole word, ignoring case.
fn mentions(text: &str, name: &str) -> bool {
    let text = text.to_lowercase();
    let name = name.to_lowercase();
    if name.is_empty() {
        return false;
    }
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut from = 0;
    while let Some(i) = text[from..].find(&name) {
        let at = from + i;
        let end = at + name.len();
        let before = text[..at].chars().next_back().is_some_and(word);
        let after = text[end..].chars().next().is_some_and(word);
        if !before && !after {
            return true;
        }
        from = at + text[at..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_table;
    use crate::test_util::*;
    use serde_json::json;

    fn ty(base: &str) -> TypeSpec {
        TypeSpec::Known {
            base: base.into(),
            length: None,
            precision: None,
            scale: None,
            unsigned: false,
            values: vec![],
        }
    }

    /// A column that exists on the server.
    fn col(name: &str) -> ColumnDef {
        ColumnDef {
            original: Some(name.into()),
            name: name.into(),
            data_type: ty("int"),
            nullable: true,
            default: None,
            auto_increment: false,
            identity: None,
            on_update: None,
            charset: None,
            collation: None,
            comment: None,
            using: None,
            generated: None,
            locked: false,
        }
    }

    fn part(column: &str) -> IndexPartDef {
        IndexPartDef {
            column: Some(column.into()),
            expression: None,
            prefix: None,
            descending: false,
        }
    }

    fn index(name: &str, parts: Vec<IndexPartDef>) -> IndexDef {
        IndexDef {
            original: Some(name.into()),
            name: name.into(),
            unique: false,
            parts,
            method: None,
            include: vec![],
            visible: true,
            comment: None,
            constraint: false,
            locked: false,
        }
    }

    fn fk(name: &str, column: &str) -> FkDef {
        FkDef {
            original: Some(name.into()),
            name: name.into(),
            columns: vec![column.into()],
            ref_schema: None,
            ref_table: "u".into(),
            ref_columns: vec!["id".into()],
            on_update: "RESTRICT".into(),
            on_delete: "RESTRICT".into(),
            match_type: None,
            deferrable: false,
            initially_deferred: false,
            locked: false,
        }
    }

    /// `t(id, a, b, c)`, PK on `id`, index `idx_a (a)`, FK `fk_b (b) → u(id)`.
    fn stored() -> TableDef {
        TableDef {
            original_name: Some("t".into()),
            name: "t".into(),
            columns: vec![col("id"), col("a"), col("b"), col("c")],
            primary_key: Some(KeyDef {
                original: Some("PRIMARY".into()),
                name: Some("PRIMARY".into()),
                columns: vec!["id".into()],
                locked: false,
            }),
            indexes: vec![index("idx_a", vec![part("a")])],
            foreign_keys: vec![fk("fk_b", "b")],
            checks: vec![],
            options: TableOptionsDef::default(),
            comment: None,
            read_only_reason: None,
        }
    }

    fn locked_err(r: Result<Vec<Change>, DiffError>) -> ElementRef {
        match r {
            Err(DiffError::Locked { element, reason }) => {
                assert!(!reason.is_empty());
                element
            }
            other => panic!("expected Locked, got {other:?}"),
        }
    }

    fn invalid_err(r: Result<Vec<Change>, DiffError>) -> Vec<DraftError> {
        match r {
            Err(DiffError::Invalid(errs)) => errs,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn no_change_is_empty() {
        let old = stored();
        assert_eq!(diff(&old, &old.clone()), Ok(vec![]));
        // `locked` flags are not content.
        let mut new = old.clone();
        new.columns[1].locked = true;
        new.indexes[0].locked = true;
        assert_eq!(diff(&old, &new), Ok(vec![]));
    }

    #[test]
    fn rename_detected_via_original() {
        let old = stored();
        let mut new = old.clone();
        new.columns[1].name = "a2".into();
        // The designer follows the rename in the index; that is no index change.
        new.indexes[0].parts[0].column = Some("a2".into());
        assert_eq!(
            diff(&old, &new),
            Ok(vec![Change::RenameColumn {
                from: "a".into(),
                to: "a2".into()
            }])
        );
        // Same for the PK and FK columns.
        let mut new = old.clone();
        new.columns[0].name = "pk".into();
        new.primary_key.as_mut().unwrap().columns = vec!["pk".into()];
        new.columns[2].name = "b2".into();
        new.foreign_keys[0].columns = vec!["b2".into()];
        assert_eq!(
            diff(&old, &new),
            Ok(vec![
                Change::RenameColumn {
                    from: "id".into(),
                    to: "pk".into()
                },
                Change::RenameColumn {
                    from: "b".into(),
                    to: "b2".into()
                },
            ])
        );
    }

    #[test]
    fn rename_and_retype_column() {
        let old = stored();
        let mut new = old.clone();
        new.columns[3].name = "c2".into();
        new.columns[3].data_type = ty("bigint");
        assert_eq!(
            diff(&old, &new),
            Ok(vec![
                Change::RenameColumn {
                    from: "c".into(),
                    to: "c2".into()
                },
                Change::AlterColumn {
                    name: "c2".into(),
                    old: old.columns[3].clone(),
                    new: new.columns[3].clone(),
                },
            ])
        );
    }

    #[test]
    fn dropped_and_added_column_order() {
        let old = stored();
        let mut new = old.clone();
        let y = ColumnDef {
            original: None,
            ..col("y")
        };
        let x = ColumnDef {
            original: None,
            ..col("x")
        };
        // Drop a and c (and the index on a); add y first and x after b.
        new.columns = vec![y.clone(), col("id"), col("b"), x.clone()];
        new.indexes.clear();
        assert_eq!(
            diff(&old, &new),
            Ok(vec![
                Change::DropIndex {
                    name: "idx_a".into()
                },
                Change::DropColumn { name: "a".into() },
                Change::DropColumn { name: "c".into() },
                Change::AddColumn {
                    column: y,
                    position: Position::First
                },
                Change::AddColumn {
                    column: x,
                    position: Position::After("b".into())
                },
            ])
        );
    }

    #[test]
    fn index_content_change_is_drop_add() {
        let old = stored();
        let mut new = old.clone();
        new.indexes[0].unique = true;
        assert_eq!(
            diff(&old, &new),
            Ok(vec![
                Change::DropIndex {
                    name: "idx_a".into()
                },
                Change::AddIndex {
                    index: new.indexes[0].clone()
                },
            ])
        );
        // A new index is an add only.
        let mut new = old.clone();
        new.indexes.push(IndexDef {
            original: None,
            ..index("idx_c", vec![part("c")])
        });
        assert_eq!(
            diff(&old, &new),
            Ok(vec![Change::AddIndex {
                index: new.indexes[1].clone()
            }])
        );
    }

    #[test]
    fn index_rename_only() {
        let old = stored();
        let mut new = old.clone();
        new.indexes[0].name = "idx_a2".into();
        assert_eq!(
            diff(&old, &new),
            Ok(vec![Change::RenameIndex {
                from: "idx_a".into(),
                to: "idx_a2".into()
            }])
        );
        // Renamed and changed: drop + add, no rename.
        new.indexes[0].parts.push(part("b"));
        assert_eq!(
            diff(&old, &new),
            Ok(vec![
                Change::DropIndex {
                    name: "idx_a".into()
                },
                Change::AddIndex {
                    index: new.indexes[0].clone()
                },
            ])
        );
    }

    #[test]
    fn fk_change_is_drop_add() {
        let old = stored();
        let mut new = old.clone();
        new.foreign_keys[0].on_delete = "CASCADE".into();
        assert_eq!(
            diff(&old, &new),
            Ok(vec![
                Change::DropForeignKey {
                    name: "fk_b".into()
                },
                Change::AddForeignKey {
                    fk: new.foreign_keys[0].clone()
                },
            ])
        );
        // Checks too, and a dropped check is a drop.
        let mut old = stored();
        old.checks.push(CheckDef {
            original: Some("ck".into()),
            name: "ck".into(),
            expression: "a > 0".into(),
            locked: false,
        });
        let mut new = old.clone();
        new.checks[0].expression = "a > 1".into();
        assert_eq!(
            diff(&old, &new),
            Ok(vec![
                Change::DropCheck { name: "ck".into() },
                Change::AddCheck {
                    check: new.checks[0].clone()
                },
            ])
        );
        new.checks.clear();
        assert_eq!(
            diff(&old, &new),
            Ok(vec![Change::DropCheck { name: "ck".into() }])
        );
    }

    #[test]
    fn move_column_after() {
        let old = stored();
        let mut new = old.clone();
        new.columns = vec![col("id"), col("b"), col("a"), col("c")];
        assert_eq!(
            diff(&old, &new),
            Ok(vec![Change::MoveColumn {
                name: "a".into(),
                position: Position::After("b".into())
            }])
        );
        let mut new = old.clone();
        new.columns = vec![col("c"), col("id"), col("a"), col("b")];
        assert_eq!(
            diff(&old, &new),
            Ok(vec![Change::MoveColumn {
                name: "c".into(),
                position: Position::First
            }])
        );
        // Drops and adds alone never move the kept columns.
        let mut new = old.clone();
        let z = ColumnDef {
            original: None,
            ..col("z")
        };
        new.columns = vec![col("id"), z.clone(), col("b"), col("c")];
        new.indexes.clear();
        assert_eq!(
            diff(&old, &new),
            Ok(vec![
                Change::DropIndex {
                    name: "idx_a".into()
                },
                Change::DropColumn { name: "a".into() },
                Change::AddColumn {
                    column: z,
                    position: Position::After("id".into())
                },
            ])
        );
        // A moved column lands after the previous kept column (a new column is not
        // there yet when the move runs).
        let mut new = old.clone();
        let n = ColumnDef {
            original: None,
            ..col("n")
        };
        new.columns = vec![col("id"), col("b"), n.clone(), col("a"), col("c")];
        assert_eq!(
            diff(&old, &new),
            Ok(vec![
                Change::MoveColumn {
                    name: "a".into(),
                    position: Position::After("b".into())
                },
                Change::AddColumn {
                    column: n,
                    position: Position::After("b".into())
                },
            ])
        );
    }

    #[test]
    fn locked_column_change_refused() {
        let mut old = stored();
        old.columns[1].locked = true;
        let a = ElementRef::Column("a".into());
        // Retype, rename, drop.
        let mut new = old.clone();
        new.columns[1].data_type = ty("bigint");
        assert_eq!(locked_err(diff(&old, &new)), a);
        let mut new = old.clone();
        new.columns[1].name = "a2".into();
        new.indexes[0].parts[0].column = Some("a2".into());
        assert_eq!(locked_err(diff(&old, &new)), a);
        let mut new = old.clone();
        new.columns.remove(1);
        new.indexes.clear();
        assert_eq!(locked_err(diff(&old, &new)), a);
        // The draft's flag counts too, and other locked elements.
        let old = stored();
        let mut new = old.clone();
        new.columns[1].locked = true;
        new.columns[1].nullable = false;
        assert_eq!(locked_err(diff(&old, &new)), a);
        let mut old = stored();
        old.indexes[0].locked = true;
        let mut new = old.clone();
        new.indexes[0].name = "idx_x".into();
        assert_eq!(
            locked_err(diff(&old, &new)),
            ElementRef::Index("idx_a".into())
        );
        let mut old = stored();
        old.primary_key.as_mut().unwrap().locked = true;
        let mut new = old.clone();
        new.primary_key = None;
        assert_eq!(locked_err(diff(&old, &new)), ElementRef::PrimaryKey);
        let mut old = stored();
        old.options.locked = true;
        let mut new = old.clone();
        new.options.engine = Some("MyISAM".into());
        assert_eq!(locked_err(diff(&old, &new)), ElementRef::Options);
        // A read-only table refuses any change.
        let mut old = stored();
        old.read_only_reason = Some("Partitioned tables are read-only.".into());
        let mut new = old.clone();
        new.comment = Some("x".into());
        assert_eq!(locked_err(diff(&old, &new)), ElementRef::Table);
        assert_eq!(diff(&old, &old.clone()), Ok(vec![]));
    }

    #[test]
    fn rename_column_used_by_locked_index_is_refused() {
        let mut old = stored();
        old.indexes.push(IndexDef {
            locked: true,
            ..index(
                "idx_expr",
                vec![IndexPartDef {
                    column: None,
                    expression: Some("lower(`A`), `ab`".into()),
                    prefix: None,
                    descending: false,
                }],
            )
        });
        let idx = ElementRef::Index("idx_expr".into());
        let mut new = old.clone();
        new.columns[1].name = "a2".into();
        new.indexes[0].parts[0].column = Some("a2".into());
        assert_eq!(locked_err(diff(&old, &new)), idx);
        // Dropping it too.
        let mut new = old.clone();
        new.columns.remove(1);
        new.indexes.remove(0);
        assert_eq!(locked_err(diff(&old, &new)), idx);
        // Word boundary: `ab` does not mention `b`; retyping `a` is not a rename.
        let mut new = old.clone();
        new.columns[2].name = "b2".into();
        new.foreign_keys[0].columns = vec!["b2".into()];
        new.columns[1].data_type = ty("bigint");
        assert!(diff(&old, &new).is_ok());

        // A locked check and a generated column.
        let mut old = stored();
        old.checks.push(CheckDef {
            original: Some("ck".into()),
            name: "ck".into(),
            expression: "c > 0".into(),
            locked: true,
        });
        let mut new = old.clone();
        new.columns[3].name = "c2".into();
        assert_eq!(locked_err(diff(&old, &new)), ElementRef::Check("ck".into()));
        let mut old = stored();
        old.columns.push(ColumnDef {
            generated: Some("c * 2".into()),
            locked: true,
            ..col("g")
        });
        let mut new = old.clone();
        new.columns.remove(3);
        assert_eq!(locked_err(diff(&old, &new)), ElementRef::Column("g".into()));
        // A plain column part of a locked index counts as a use.
        let mut old = stored();
        old.indexes[0].locked = true;
        let mut new = old.clone();
        new.columns[1].name = "a2".into();
        assert_eq!(
            locked_err(diff(&old, &new)),
            ElementRef::Index("idx_a".into())
        );
    }

    #[test]
    fn new_table_yields_create_table_only() {
        let old = new_table("t", &pg());
        let mut new = old.clone();
        new.name = "orders".into();
        new.columns.push(ColumnDef {
            original: None,
            ..col("note")
        });
        new.comment = Some("x".into());
        assert_eq!(diff(&old, &new), Ok(vec![Change::CreateTable]));
    }

    #[test]
    fn table_level_changes_in_order() {
        let old = stored();
        let mut new = old.clone();
        new.name = "t2".into();
        new.options.engine = Some("InnoDB".into());
        new.comment = Some("c".into());
        new.primary_key.as_mut().unwrap().columns = vec!["id".into(), "a".into()];
        assert_eq!(
            diff(&old, &new),
            Ok(vec![
                Change::RenameTable {
                    from: "t".into(),
                    to: "t2".into()
                },
                Change::SetOptions {
                    old: old.options.clone(),
                    new: new.options.clone()
                },
                Change::SetComment {
                    comment: Some("c".into())
                },
                Change::DropPrimaryKey {
                    name: Some("PRIMARY".into())
                },
                Change::AddPrimaryKey {
                    key: new.primary_key.clone().unwrap()
                },
            ])
        );
    }

    #[test]
    fn bad_originals_are_invalid() {
        let old = stored();
        // An original that names nothing in the stored table.
        let mut new = old.clone();
        new.columns[1].original = Some("nope".into());
        let errs = invalid_err(diff(&old, &new));
        assert_eq!(errs[0].element, ElementRef::Column("a".into()));
        assert_eq!(errs[0].field.as_deref(), Some("original"));
        // Two draft elements claiming the same original.
        let mut new = old.clone();
        new.indexes.push(index("idx_a", vec![part("b")]));
        new.indexes[1].name = "idx_b".into();
        let errs = invalid_err(diff(&old, &new));
        assert_eq!(errs[0].element, ElementRef::Index("idx_b".into()));
        // A new element marked locked.
        let mut new = old.clone();
        new.checks.push(CheckDef {
            original: None,
            name: "ck".into(),
            expression: "a > 0".into(),
            locked: true,
        });
        assert_eq!(
            invalid_err(diff(&old, &new))[0].element,
            ElementRef::Check("ck".into())
        );
        // A draft of another table.
        let mut new = old.clone();
        new.original_name = Some("other".into());
        assert_eq!(invalid_err(diff(&old, &new))[0].element, ElementRef::Table);
    }

    #[test]
    fn locked_column_stays_in_place() {
        // Unweighted, the stable run would keep id, b, c and move `a`; a locked `a`
        // stays and `b` moves around it.
        let mut old = stored();
        old.columns[1].locked = true;
        let mut new = old.clone();
        new.columns = vec![col("id"), col("b"), old.columns[1].clone(), col("c")];
        assert_eq!(
            diff(&old, &new),
            Ok(vec![Change::MoveColumn {
                name: "b".into(),
                position: Position::After("id".into())
            }])
        );
        // Dragging a locked column to the front moves the others instead.
        let mut new = old.clone();
        new.columns = vec![old.columns[1].clone(), col("id"), col("b"), col("c")];
        assert_eq!(
            diff(&old, &new),
            Ok(vec![Change::MoveColumn {
                name: "id".into(),
                position: Position::After("a".into())
            }])
        );
    }

    #[test]
    fn locked_column_that_must_move_is_refused() {
        let mut old = stored();
        old.columns[1].locked = true;
        old.columns[2].locked = true;
        let mut new = old.clone();
        new.columns.swap(1, 2);
        let el = locked_err(diff(&old, &new));
        assert!(
            el == ElementRef::Column("a".into()) || el == ElementRef::Column("b".into()),
            "{el:?}"
        );
    }

    fn collision(r: Result<Vec<Change>, DiffError>) -> ElementRef {
        let errs = invalid_err(r);
        assert_eq!(errs[0].field.as_deref(), Some("name"), "{errs:?}");
        assert!(errs[0].message.contains("two steps"), "{errs:?}");
        errs[0].element.clone()
    }

    #[test]
    fn column_name_collisions_are_invalid() {
        let old = stored();
        // Swap a <-> b.
        let mut new = old.clone();
        new.columns[1].name = "b".into();
        new.columns[2].name = "a".into();
        new.indexes.clear();
        new.foreign_keys.clear();
        assert_eq!(collision(diff(&old, &new)), ElementRef::Column("b".into()));
        // Chain a -> b, b -> x.
        let mut new = old.clone();
        new.columns[1].name = "b".into();
        new.columns[2].name = "x".into();
        new.indexes.clear();
        new.foreign_keys.clear();
        assert_eq!(collision(diff(&old, &new)), ElementRef::Column("b".into()));
        // Rename onto a dropped column.
        let mut new = old.clone();
        new.columns.remove(3);
        new.columns[1].name = "C".into();
        new.indexes.clear();
        assert_eq!(collision(diff(&old, &new)), ElementRef::Column("C".into()));
        // A new column reusing a name renamed away.
        let mut new = old.clone();
        new.columns[1].name = "a2".into();
        new.indexes[0].parts[0].column = Some("a2".into());
        new.columns.push(ColumnDef {
            original: None,
            ..col("a")
        });
        assert_eq!(collision(diff(&old, &new)), ElementRef::Column("a".into()));
        // A new column named like a dropped one.
        let mut new = old.clone();
        new.columns[3] = ColumnDef {
            original: None,
            ..col("c")
        };
        assert_eq!(collision(diff(&old, &new)), ElementRef::Column("c".into()));
        // Case-only rename of a column is fine.
        let mut new = old.clone();
        new.columns[3].name = "C".into();
        assert!(diff(&old, &new).is_ok());
    }

    #[test]
    fn constraint_name_collisions_are_invalid() {
        let mut old = stored();
        old.indexes.push(index("idx_b", vec![part("b")]));
        old.checks.push(CheckDef {
            original: Some("ck".into()),
            name: "ck".into(),
            expression: "a > 0".into(),
            locked: false,
        });
        // Index swap.
        let mut new = old.clone();
        new.indexes[0].name = "idx_b".into();
        new.indexes[1].name = "idx_a".into();
        assert_eq!(
            collision(diff(&old, &new)),
            ElementRef::Index("idx_b".into())
        );
        // Index renamed onto an FK's name (one namespace).
        let mut new = old.clone();
        new.indexes[0].name = "fk_b".into();
        assert_eq!(
            collision(diff(&old, &new)),
            ElementRef::Index("fk_b".into())
        );
        // A new check named like a dropped index.
        let mut new = old.clone();
        new.indexes.remove(1);
        new.checks.push(CheckDef {
            original: None,
            name: "idx_b".into(),
            expression: "b > 0".into(),
            locked: false,
        });
        assert_eq!(
            collision(diff(&old, &new)),
            ElementRef::Check("idx_b".into())
        );
        // Renamed and changed FK (drop + add) onto a check's name.
        let mut new = old.clone();
        new.foreign_keys[0].name = "ck".into();
        new.foreign_keys[0].on_delete = "CASCADE".into();
        assert_eq!(
            collision(diff(&old, &new)),
            ElementRef::ForeignKey("ck".into())
        );
        // A new FK reusing the name of an FK renamed away.
        let mut new = old.clone();
        new.foreign_keys[0].name = "fk_b2".into();
        new.foreign_keys.push(FkDef {
            original: None,
            ..fk("fk_b", "c")
        });
        assert_eq!(
            collision(diff(&old, &new)),
            ElementRef::ForeignKey("fk_b".into())
        );
        // A new PK named like an index; a replacement PK keeping the old name is fine.
        let mut new = old.clone();
        new.primary_key = Some(KeyDef {
            original: None,
            name: Some("idx_a".into()),
            columns: vec!["id".into()],
            locked: false,
        });
        assert_eq!(collision(diff(&old, &new)), ElementRef::PrimaryKey);
        let mut new = old.clone();
        new.primary_key = Some(KeyDef {
            original: None,
            name: Some("PRIMARY".into()),
            columns: vec!["id".into(), "a".into()],
            locked: false,
        });
        assert!(diff(&old, &new).is_ok());
    }

    #[test]
    fn fk_and_its_backing_index_may_share_a_name() {
        // MySQL names an FK's index after the FK; changing the FK is drop + add.
        let mut old = stored();
        old.indexes.push(index("fk_b", vec![part("b")]));
        let mut new = old.clone();
        new.foreign_keys[0].on_delete = "CASCADE".into();
        assert_eq!(
            diff(&old, &new),
            Ok(vec![
                Change::DropForeignKey {
                    name: "fk_b".into()
                },
                Change::AddForeignKey {
                    fk: new.foreign_keys[0].clone()
                },
            ])
        );
    }

    #[test]
    fn diff_ipc_shapes() {
        assert_eq!(
            serde_json::to_value(Change::CreateTable).unwrap(),
            json!({"kind": "create_table"})
        );
        assert_eq!(
            serde_json::to_value(Change::MoveColumn {
                name: "a".into(),
                position: Position::After("b".into())
            })
            .unwrap(),
            json!({"kind": "move_column", "name": "a", "position": {"kind": "after", "column": "b"}})
        );
        assert_eq!(
            serde_json::to_value(Position::First).unwrap(),
            json!({"kind": "first"})
        );
        assert_eq!(
            serde_json::to_value(DiffError::Locked {
                element: ElementRef::PrimaryKey,
                reason: "r".into()
            })
            .unwrap(),
            json!({"kind": "locked", "detail": {"element": {"kind": "primary_key"}, "reason": "r"}})
        );
        assert_eq!(
            serde_json::to_value(DiffError::Invalid(vec![])).unwrap(),
            json!({"kind": "invalid", "detail": []})
        );
    }
}
