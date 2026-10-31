//! The row engine (spec §4.2): one row at a time, in order, with NULL %, unique values,
//! foreign keys and the plan checks that need the row count.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::fake::with_suffix;
use super::generator::{
    capacity, check_generator, cut, generate, int_limits, nth_cell, to_value, FakeKind,
};
use super::limits::{decimal_whole_max, int_bounds};
use super::rng::{cell_rng, SplitMix64};
use super::{ColumnIssue, ColumnPlan, Generator};
use crate::convert::TargetColumn;
use crate::read::Cell;
use dnt_core::server::DbKind;
use dnt_core::value::{FullValue, LogicalType, Value};

/// Attempts at a fresh value for a unique column before it is given up (or probed).
const UNIQUE_TRIES: u32 = 50;

/// The keys of a foreign key's parent table, read before generating (spec §5.3).
#[derive(Debug, Clone, PartialEq)]
pub struct FkKeys {
    pub constraint: String,
    /// The parent table's name, for messages.
    pub parent: String,
    /// The child columns of the foreign key, in key order.
    pub columns: Vec<String>,
    /// Distinct key tuples (in `columns` order).
    pub keys: Arc<Vec<Vec<FullValue>>>,
}

/// One column of the table with its plan; `fk` for a `ForeignKey` generator.
#[derive(Debug, Clone)]
pub struct SeedColumnSpec {
    pub column: TargetColumn,
    pub plan: ColumnPlan,
    pub fk: Option<Arc<FkKeys>>,
}

fn issue(column: &str, message: impl Into<String>) -> ColumnIssue {
    ColumnIssue {
        column: column.to_string(),
        message: message.into(),
    }
}

fn unique_message(rows: u32, column: &str) -> String {
    format!("Cannot generate {rows} unique values for `{column}`; widen the range.")
}

fn null() -> FullValue {
    FullValue::try_from_value(Value::Null).expect("NULL is complete")
}

/// Fake kinds made unique with a row-number suffix (spec §4.2).
fn suffixed(kind: FakeKind) -> bool {
    !matches!(kind, FakeKind::Ipv4 | FakeKind::HexColor)
}

/// The problem with one value of a `Pick` list or a `Constant` for `col`: the type's numeric
/// range (which `convert` does not check), then the conversion itself.
fn listed_value_problem(v: &str, col: &TargetColumn, kind: DbKind) -> Option<String> {
    match col.logical {
        LogicalType::Integer => {
            if let Ok(n) = v.trim().parse::<i128>() {
                let (lo, hi) = int_bounds(col, kind);
                if n < lo || n > hi {
                    return Some(format!("Values must be between {lo} and {hi}."));
                }
            }
        }
        LogicalType::Decimal => {
            if let (Ok(n), Some(max)) = (v.trim().parse::<f64>(), decimal_whole_max(&col.db_type)) {
                let bound = max + 1;
                if n.abs() >= bound as f64 {
                    return Some(format!("Values must be below {bound}."));
                }
            }
        }
        _ => {}
    }
    to_value(&Cell::Text(v.to_string()), col, kind).err()
}

/// The problem with one column's plan for `rows` rows, if any.
fn check_one(s: &SeedColumnSpec, at: usize, rows: u32, kind: DbKind) -> Option<String> {
    let col = &s.column;
    let gen = &s.plan.generator;
    if col.generated {
        return (*gen != Generator::Skip)
            .then(|| "Generated column: the server computes it.".into());
    }
    if *gen == Generator::Skip {
        return (!col.nullable && !col.has_default)
            .then(|| format!("{} has no default; choose a generator.", col.name));
    }
    if let Some(m) = check_generator(gen, col, kind) {
        return Some(m);
    }
    if let Generator::Sequence { start, step } = gen {
        let last = i128::from(*start) + i128::from(*step) * i128::from(rows.saturating_sub(1));
        let (lo, hi) = int_limits(col, kind);
        if last < lo || last > hi {
            return Some(format!("Values must be between {lo} and {hi}."));
        }
    }
    if let Generator::ForeignKey { .. } = gen {
        let k = match &s.fk {
            None => return Some("The foreign key has no keys to pick from.".into()),
            Some(k) => k,
        };
        if k.keys.is_empty() && !col.nullable {
            return Some(format!("{} has no rows to reference.", k.parent));
        }
        if s.plan.unique && rows as usize > k.keys.len() && !col.nullable {
            return Some(unique_message(rows, &col.name));
        }
        return None;
    }
    if let (Generator::Pick { values }, LogicalType::Enum) = (gen, col.logical) {
        if let Some(v) = values.iter().find(|v| !col.enum_values.contains(v)) {
            return Some(format!("\"{v}\" is not one of the enum values."));
        }
    }
    // Every value a list or a constant can produce, not one drawn at random.
    let listed: &[String] = match gen {
        Generator::Pick { values } => values,
        Generator::Constant { value } => std::slice::from_ref(value),
        _ => &[],
    };
    for v in listed {
        if let Some(m) = listed_value_problem(v, col, kind) {
            return Some(m);
        }
    }
    if s.plan.unique && rows > 1 {
        if let Some(cap) = capacity(gen) {
            if cap < u128::from(rows) {
                return Some(unique_message(rows, &col.name));
            }
        }
    }
    // One trial value catches a generator that cannot make values of the column at all.
    generate(gen, col, &mut cell_rng(0, 0, at as u32), 0, kind).err()
}

/// Every column's problem for `rows` rows (spec §4.2, §5.2).
pub fn check_plans(specs: &[SeedColumnSpec], rows: u32, kind: DbKind) -> Vec<ColumnIssue> {
    specs
        .iter()
        .enumerate()
        .filter_map(|(i, s)| check_one(s, i, rows, kind).map(|m| issue(&s.column.name, m)))
        .collect()
}

/// `text` (a fake value of `kind`) with the row-number suffix, cut to `max` characters by
/// shortening its body; `None` when even one body character does not fit.
fn suffixed_within(kind: FakeKind, text: &str, n: u64, max: Option<u32>) -> Option<String> {
    let full = with_suffix(Some(kind), text, n);
    let Some(max) = max else {
        return Some(full);
    };
    let len = full.chars().count();
    if len <= max as usize {
        return Some(full);
    }
    let over = len - max as usize;
    if kind == FakeKind::Email {
        if let Some((local, domain)) = text.split_once('@') {
            let keep = local
                .chars()
                .count()
                .checked_sub(over)
                .filter(|k| *k >= 1)?;
            return Some(with_suffix(
                Some(kind),
                &format!("{}@{domain}", cut(local, keep)),
                n,
            ));
        }
    }
    let keep = text.chars().count().checked_sub(over).filter(|k| *k >= 1)?;
    Some(with_suffix(Some(kind), &cut(text, keep), n))
}

/// Generates the rows of one job, in order (rows 0, 1, 2, …).
pub struct Seeder {
    specs: Vec<SeedColumnSpec>,
    seed: u32,
    rows: u32,
    kind: DbKind,
    /// Values already used, per column (unique columns only).
    seen: Vec<HashSet<String>>,
    /// The first column of each column's foreign key (its RNG picks the key of the group).
    fk_first: Vec<Option<usize>>,
}

impl Seeder {
    /// Fails with every column's problem for `rows` rows.
    pub fn new(
        specs: Vec<SeedColumnSpec>,
        seed: u32,
        rows: u32,
        kind: DbKind,
    ) -> Result<Seeder, Vec<ColumnIssue>> {
        let issues = check_plans(&specs, rows, kind);
        if !issues.is_empty() {
            return Err(issues);
        }
        let mut first: HashMap<&str, usize> = HashMap::new();
        let fk_first = specs
            .iter()
            .enumerate()
            .map(|(i, s)| match &s.plan.generator {
                Generator::ForeignKey { constraint } => {
                    Some(*first.entry(constraint.as_str()).or_insert(i))
                }
                _ => None,
            })
            .collect();
        Ok(Seeder {
            seen: vec![HashSet::new(); specs.len()],
            specs,
            seed,
            rows,
            kind,
            fk_first,
        })
    }

    /// The columns of the `INSERT` (every column whose generator is not `Skip`), in table order.
    pub fn columns(&self) -> Vec<TargetColumn> {
        self.specs
            .iter()
            .filter(|s| s.plan.generator != Generator::Skip)
            .map(|s| s.column.clone())
            .collect()
    }

    /// The values of row `row` for [`columns`](Seeder::columns).
    pub fn row(&mut self, row: u64) -> Result<Vec<FullValue>, ColumnIssue> {
        let mut tuples: HashMap<usize, Option<Vec<FullValue>>> = HashMap::new();
        let mut out = Vec::with_capacity(self.specs.len());
        for i in 0..self.specs.len() {
            match &self.specs[i].plan.generator {
                Generator::Skip => continue,
                Generator::ForeignKey { .. } => {
                    let first = self.fk_first[i].unwrap_or(i);
                    if let std::collections::hash_map::Entry::Vacant(e) = tuples.entry(first) {
                        e.insert(self.fk_tuple(first, row)?);
                    }
                    let s = &self.specs[i];
                    let pos =
                        s.fk.as_ref()
                            .and_then(|k| k.columns.iter().position(|c| *c == s.column.name));
                    let v = match (&tuples[&first], pos) {
                        (Some(t), Some(p)) => t.get(p).cloned().unwrap_or_else(null),
                        _ => null(),
                    };
                    out.push(v);
                }
                _ => out.push(self.cell(i, row)?),
            }
        }
        Ok(out)
    }

    /// The key tuple of the foreign key whose first column is `first`; `None` = NULL.
    fn fk_tuple(&mut self, first: usize, row: u64) -> Result<Option<Vec<FullValue>>, ColumnIssue> {
        let s = &self.specs[first];
        let Some(k) = s.fk.clone() else {
            return Ok(None);
        };
        let mut rng = cell_rng(self.seed, row, first as u32);
        let nulled = s.column.nullable && rng.percent(s.plan.null_percent);
        let len = k.keys.len();
        if nulled || len == 0 {
            return Ok(None);
        }
        let start = rng.below(len as u64) as usize;
        if !s.plan.unique {
            return Ok(Some(k.keys[start].clone()));
        }
        // Unique: the drawn key, else the next unused one (always found while keys remain).
        for step in 0..len {
            let at = (start + step) % len;
            if self.seen[first].insert(at.to_string()) {
                return Ok(Some(k.keys[at].clone()));
            }
        }
        let name = s.column.name.clone();
        if s.column.nullable {
            return Ok(None);
        }
        Err(issue(&name, unique_message(self.rows, &name)))
    }

    /// One cell of a column with a value generator.
    fn cell(&mut self, i: usize, row: u64) -> Result<FullValue, ColumnIssue> {
        let s = &self.specs[i];
        let (col, gen) = (&s.column, &s.plan.generator);
        let fail = |m: String| issue(&col.name, m);
        let mut rng = cell_rng(self.seed, row, i as u32);
        if col.nullable && rng.percent(s.plan.null_percent) {
            return Ok(null());
        }
        let make = |rng: &mut SplitMix64| -> Result<FullValue, ColumnIssue> {
            generate(gen, col, rng, row, self.kind)
                .map_err(fail)?
                .ok_or_else(|| fail("The column has no value generator.".into()))
        };
        let v = make(&mut rng)?;
        if !s.plan.unique || matches!(gen, Generator::Sequence { .. } | Generator::Uuid) {
            return Ok(v);
        }
        let key = |v: &FullValue| format!("{:?}", v.to_display());
        if self.seen[i].insert(key(&v)) {
            return Ok(v);
        }
        let unique_fail = || issue(&col.name, unique_message(self.rows, &col.name));
        if let Generator::Fake { fake } = gen {
            if suffixed(*fake) {
                let Value::Text(text) = v.to_display() else {
                    return Err(unique_fail());
                };
                let max = (col.logical == LogicalType::Text)
                    .then_some(col.max_chars)
                    .flatten();
                let t = suffixed_within(*fake, &text, row + 1, max).ok_or_else(unique_fail)?;
                let v = to_value(&Cell::Text(t), col, self.kind).map_err(fail)?;
                return if self.seen[i].insert(key(&v)) {
                    Ok(v)
                } else {
                    Err(unique_fail())
                };
            }
        }
        for _ in 0..UNIQUE_TRIES {
            let v = make(&mut rng)?;
            if self.seen[i].insert(key(&v)) {
                return Ok(v);
            }
        }
        // Countable generators: walk from a random start to the next unused value.
        if let Some(cap) = capacity(gen).filter(|c| *c > 0) {
            let start = u128::from(rng.next_u64()) % cap;
            for step in 0..cap {
                let Some(cell) = nth_cell(gen, (start + step) % cap) else {
                    break;
                };
                let v = to_value(&cell, col, self.kind).map_err(fail)?;
                if self.seen[i].insert(key(&v)) {
                    return Ok(v);
                }
            }
        }
        Err(unique_fail())
    }
}
