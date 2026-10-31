//! The data seeder's row engine (data seeder spec §4.2): determinism, NULL %, unique values,
//! `max_chars`, foreign keys and the plan checks.

use std::collections::HashSet;
use std::sync::Arc;

use dnt_core::server::DbKind;
use dnt_core::value::{FullValue, LogicalType, Value};
use dnt_transfer::convert::TargetColumn;
use dnt_transfer::seed::engine::{check_plans, FkKeys, SeedColumnSpec, Seeder};
use dnt_transfer::seed::{ColumnIssue, ColumnPlan, FakeKind, Generator};
use proptest::prelude::*;

const MY: DbKind = DbKind::MySql;

fn column(name: &str, db_type: &str, logical: LogicalType, nullable: bool) -> TargetColumn {
    TargetColumn {
        name: name.into(),
        db_type: db_type.into(),
        logical,
        nullable,
        has_default: false,
        auto_increment: false,
        identity: None,
        generated: false,
        enum_values: Vec::new(),
        max_chars: db_type
            .strip_prefix("varchar(")
            .and_then(|r| r.strip_suffix(')'))
            .and_then(|n| n.parse().ok()),
        collation: None,
        unsigned: false,
    }
}

fn spec(col: TargetColumn, generator: Generator) -> SeedColumnSpec {
    SeedColumnSpec {
        plan: ColumnPlan {
            name: col.name.clone(),
            generator,
            null_percent: 0,
            unique: false,
        },
        column: col,
        fk: None,
    }
}

fn unique(mut s: SeedColumnSpec) -> SeedColumnSpec {
    s.plan.unique = true;
    s
}

fn nulls(mut s: SeedColumnSpec, p: u8) -> SeedColumnSpec {
    s.plan.null_percent = p;
    s
}

fn text(v: &FullValue) -> String {
    match v.to_display() {
        Value::Text(s) => s,
        other => panic!("not text: {other:?}"),
    }
}

fn rows(seeder: &mut Seeder, n: u64) -> Vec<Vec<FullValue>> {
    (0..n).map(|r| seeder.row(r).unwrap()).collect()
}

fn int(name: &str) -> TargetColumn {
    column(name, "int", LogicalType::Integer, false)
}

fn varchar(name: &str, n: u32) -> TargetColumn {
    column(name, &format!("varchar({n})"), LogicalType::Text, false)
}

fn fake(kind: FakeKind) -> Generator {
    Generator::Fake { fake: kind }
}

#[test]
fn same_seed_same_rows() {
    let specs = || {
        vec![
            spec(int("a"), Generator::IntRange { min: 1, max: 1000 }),
            spec(varchar("b", 100), fake(FakeKind::FullName)),
        ]
    };
    let mut x = Seeder::new(specs(), 42, 100, MY).unwrap();
    let mut y = Seeder::new(specs(), 42, 100, MY).unwrap();
    assert_eq!(rows(&mut x, 100), rows(&mut y, 100));
    let mut z = Seeder::new(specs(), 43, 100, MY).unwrap();
    assert_ne!(rows(&mut x, 10), rows(&mut z, 10));
}

#[test]
fn other_column_unaffected() {
    let a = || spec(int("a"), Generator::IntRange { min: 1, max: 1000 });
    let mut x = Seeder::new(
        vec![a(), spec(varchar("b", 100), fake(FakeKind::City))],
        7,
        50,
        MY,
    )
    .unwrap();
    let mut y = Seeder::new(
        vec![a(), spec(varchar("b", 100), fake(FakeKind::Email))],
        7,
        50,
        MY,
    )
    .unwrap();
    let col_a = |rs: Vec<Vec<FullValue>>| rs.into_iter().map(|r| r[0].clone()).collect::<Vec<_>>();
    assert_eq!(col_a(rows(&mut x, 50)), col_a(rows(&mut y, 50)));
}

#[test]
fn null_percent() {
    let n = column("n", "int", LogicalType::Integer, true);
    let mut s = Seeder::new(
        vec![
            nulls(spec(n, Generator::IntRange { min: 1, max: 9 }), 30),
            nulls(spec(int("m"), Generator::IntRange { min: 1, max: 9 }), 50),
        ],
        1,
        10_000,
        MY,
    )
    .unwrap();
    let all = rows(&mut s, 10_000);
    let share = all.iter().filter(|r| r[0].is_null()).count() as f64 / 10_000.0;
    assert!((0.27..=0.33).contains(&share), "{share}");
    assert!(all.iter().all(|r| !r[1].is_null()));
}

#[test]
fn skip_columns_are_left_out() {
    let mut id = int("id");
    id.auto_increment = true;
    id.has_default = true;
    let mut s = Seeder::new(
        vec![
            spec(id, Generator::Skip),
            spec(varchar("name", 50), fake(FakeKind::FirstName)),
        ],
        1,
        5,
        MY,
    )
    .unwrap();
    let cols: Vec<String> = s.columns().into_iter().map(|c| c.name).collect();
    assert_eq!(cols, vec!["name"]);
    assert_eq!(s.row(0).unwrap().len(), 1);
}

#[test]
fn sequence_uses_row_index() {
    let mut s = Seeder::new(
        vec![spec(int("n"), Generator::Sequence { start: 1, step: 1 })],
        9,
        3,
        MY,
    )
    .unwrap();
    let got: Vec<Value> = rows(&mut s, 3).iter().map(|r| r[0].to_display()).collect();
    assert_eq!(got, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
}

#[test]
fn unique_fake_gets_suffix() {
    let mut s = Seeder::new(
        vec![unique(spec(
            varchar("first", 50),
            fake(FakeKind::FirstName),
        ))],
        3,
        5000,
        MY,
    )
    .unwrap();
    let all: HashSet<String> = rows(&mut s, 5000).iter().map(|r| text(&r[0])).collect();
    assert_eq!(all.len(), 5000);
}

#[test]
fn unique_suffix_survives_max_chars() {
    let mut s = Seeder::new(
        vec![unique(spec(varchar("email", 12), fake(FakeKind::Email)))],
        4,
        1000,
        MY,
    )
    .unwrap();
    let all: Vec<String> = rows(&mut s, 1000).iter().map(|r| text(&r[0])).collect();
    let distinct: HashSet<&String> = all.iter().collect();
    assert_eq!(distinct.len(), 1000);
    assert!(all.iter().all(|e| e.chars().count() <= 12), "{all:?}");
}

#[test]
fn fake_text_is_cut_to_max_chars() {
    let mut s = Seeder::new(
        vec![spec(varchar("note", 10), fake(FakeKind::Paragraph))],
        5,
        200,
        MY,
    )
    .unwrap();
    assert!(rows(&mut s, 200)
        .iter()
        .all(|r| text(&r[0]).chars().count() <= 10));
}

#[test]
fn unique_suffix_too_long_is_an_issue() {
    let specs = vec![unique(spec(varchar("u", 3), fake(FakeKind::Username)))];
    let want = "Cannot generate 5000 unique values for `u`; widen the range.";
    match Seeder::new(specs, 6, 5000, MY) {
        Err(issues) => assert_eq!(issues[0].message, want),
        Ok(mut s) => {
            let mut seen = HashSet::new();
            let mut failed = None;
            for r in 0..5000 {
                match s.row(r) {
                    Ok(v) => assert!(seen.insert(text(&v[0])), "duplicate"),
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            assert_eq!(
                failed,
                Some(ColumnIssue {
                    column: "u".into(),
                    message: want.into()
                })
            );
        }
    }
}

#[test]
fn check_plans_unique_feasibility() {
    let msg = |col: &str, n: u32| {
        format!("Cannot generate {n} unique values for `{col}`; widen the range.")
    };
    let issues = check_plans(
        &[unique(spec(
            int("n"),
            Generator::IntRange { min: 1, max: 100 },
        ))],
        1000,
        MY,
    );
    assert_eq!(issues[0].message, msg("n", 1000));
    let pick = Generator::Pick {
        values: vec!["a".into(), "b".into(), "c".into()],
    };
    let issues = check_plans(&[unique(spec(varchar("p", 5), pick))], 4, MY);
    assert_eq!(issues[0].message, msg("p", 4));
    let b = column("b", "boolean", LogicalType::Bool, false);
    let issues = check_plans(
        &[unique(spec(b, Generator::Bool { true_percent: 50 }))],
        3,
        MY,
    );
    assert_eq!(issues[0].message, msg("b", 3));
    assert!(check_plans(
        &[unique(spec(
            int("n"),
            Generator::IntRange { min: 1, max: 100 }
        ))],
        100,
        MY
    )
    .is_empty());
}

#[test]
fn check_plans_rules() {
    let mut g = int("g");
    g.generated = true;
    let issues = check_plans(&[spec(g, Generator::IntRange { min: 1, max: 2 })], 1, MY);
    assert_eq!(
        issues[0].message,
        "Generated column: the server computes it."
    );

    let issues = check_plans(&[spec(int("req"), Generator::Skip)], 1, MY);
    assert_eq!(issues[0].message, "req has no default; choose a generator.");

    let mut e = column("status", "enum('new','paid')", LogicalType::Enum, false);
    e.enum_values = vec!["new".into(), "paid".into()];
    let issues = check_plans(
        &[spec(
            e,
            Generator::Pick {
                values: vec!["new".into(), "x".into()],
            },
        )],
        1,
        MY,
    );
    assert_eq!(issues[0].message, "\"x\" is not one of the enum values.");

    let issues = check_plans(
        &[spec(
            int("n"),
            Generator::Constant {
                value: "abc".into(),
            },
        )],
        1,
        MY,
    );
    assert_eq!(
        issues[0].message,
        "A generated value was refused: not an integer."
    );

    let fk = Generator::ForeignKey {
        constraint: "fk".into(),
    };
    let issues = check_plans(&[spec(int("c"), fk)], 1, MY);
    assert_eq!(
        issues[0].message,
        "The foreign key has no keys to pick from."
    );

    let issues = check_plans(
        &[spec(
            int("s"),
            Generator::Sequence {
                start: 2147483640,
                step: 1,
            },
        )],
        100,
        MY,
    );
    assert_eq!(
        issues[0].message,
        "Values must be between -2147483648 and 2147483647."
    );
}

fn keys(constraint: &str, cols: &[&str], tuples: Vec<Vec<i64>>) -> Arc<FkKeys> {
    Arc::new(FkKeys {
        constraint: constraint.into(),
        parent: "customers".into(),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        keys: Arc::new(
            tuples
                .into_iter()
                .map(|t| {
                    t.into_iter()
                        .map(|n| FullValue::try_from_value(Value::Int(n)).unwrap())
                        .collect()
                })
                .collect(),
        ),
    })
}

fn fk_spec(col: TargetColumn, k: &Arc<FkKeys>) -> SeedColumnSpec {
    let mut s = spec(
        col,
        Generator::ForeignKey {
            constraint: k.constraint.clone(),
        },
    );
    s.fk = Some(Arc::clone(k));
    s
}

#[test]
fn fk_tuples_stay_together() {
    let k = keys("fk_ab", &["a", "b"], vec![vec![1, 10], vec![2, 20]]);
    let mut s = Seeder::new(
        vec![fk_spec(int("a"), &k), fk_spec(int("b"), &k)],
        8,
        200,
        MY,
    )
    .unwrap();
    for r in rows(&mut s, 200) {
        let (a, b) = (r[0].to_display(), r[1].to_display());
        assert!(
            (a == Value::Int(1) && b == Value::Int(10))
                || (a == Value::Int(2) && b == Value::Int(20)),
            "{a:?} {b:?}"
        );
    }
    let na = column("a", "int", LogicalType::Integer, true);
    let nb = column("b", "int", LogicalType::Integer, true);
    let mut s = Seeder::new(
        vec![nulls(fk_spec(na, &k), 100), nulls(fk_spec(nb, &k), 100)],
        8,
        20,
        MY,
    )
    .unwrap();
    assert!(rows(&mut s, 20)
        .iter()
        .all(|r| r[0].is_null() && r[1].is_null()));
}

#[test]
fn fk_unique_picks_each_key_once() {
    let k = keys("fk", &["c"], (1..=50).map(|n| vec![n]).collect());
    let mut s = Seeder::new(vec![unique(fk_spec(int("c"), &k))], 2, 50, MY).unwrap();
    let all: HashSet<String> = rows(&mut s, 50)
        .iter()
        .map(|r| format!("{:?}", r[0].to_display()))
        .collect();
    assert_eq!(all.len(), 50);
    let issues = check_plans(&[unique(fk_spec(int("c"), &k))], 51, MY);
    assert_eq!(
        issues[0].message,
        "Cannot generate 51 unique values for `c`; widen the range."
    );
}

#[test]
fn fk_empty_parent() {
    let k = keys("fk", &["c"], vec![]);
    let issues = check_plans(&[fk_spec(int("c"), &k)], 1, MY);
    assert_eq!(issues[0].message, "customers has no rows to reference.");
    let nullable = column("c", "int", LogicalType::Integer, true);
    assert!(check_plans(&[fk_spec(nullable.clone(), &k)], 1, MY).is_empty());
    let mut s = Seeder::new(vec![fk_spec(nullable, &k)], 1, 5, MY).unwrap();
    assert!(rows(&mut s, 5).iter().all(|r| r[0].is_null()));
}

const ALL_FAKES: [FakeKind; 22] = [
    FakeKind::FirstName,
    FakeKind::LastName,
    FakeKind::FullName,
    FakeKind::Email,
    FakeKind::Username,
    FakeKind::Phone,
    FakeKind::StreetAddress,
    FakeKind::City,
    FakeKind::State,
    FakeKind::ZipCode,
    FakeKind::Country,
    FakeKind::Company,
    FakeKind::JobTitle,
    FakeKind::Url,
    FakeKind::Domain,
    FakeKind::Ipv4,
    FakeKind::Slug,
    FakeKind::Word,
    FakeKind::Sentence,
    FakeKind::Paragraph,
    FakeKind::HexColor,
    FakeKind::PasswordHash,
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn values_within_type_limits(
        lo in -128i64..=127,
        span in 0i64..=255,
        max_chars in 1u32..=50,
        fake_at in 0usize..22,
        seed in any::<u32>(),
    ) {
        let hi = (lo + span).min(127);
        let tiny = column("t", "tinyint", LogicalType::Integer, false);
        let dec = column("d", "decimal(5,2)", LogicalType::Decimal, false);
        let txt = varchar("s", max_chars);
        let mut s = Seeder::new(
            vec![
                spec(tiny, Generator::IntRange { min: lo, max: hi }),
                spec(dec, Generator::DecimalRange { min: -999.99, max: 999.99, scale: 2 }),
                spec(txt, fake(ALL_FAKES[fake_at])),
            ],
            seed,
            20,
            MY,
        )
        .unwrap();
        for r in 0..20 {
            let row = s.row(r).unwrap();
            let Value::Int(n) = row[0].to_display() else { panic!("int") };
            prop_assert!((lo..=hi).contains(&n));
            let Value::Decimal(d) = row[1].to_display() else { panic!("decimal") };
            let v: f64 = d.parse().unwrap();
            prop_assert!(v.abs() < 1000.0);
            prop_assert!(text(&row[2]).chars().count() <= max_chars as usize);
        }
    }
}

// Final review fixes ------------------------------------------------------------------------

/// Every value of a list is checked, not one drawn at random (review I2).
#[test]
fn pick_values_are_all_checked() {
    let mut values: Vec<String> = (0..49).map(|i| format!("v{i}")).collect();
    values.push("this value is too long".into());
    let issues = check_plans(
        &[spec(varchar("tag", 10), Generator::Pick { values })],
        1,
        MY,
    );
    assert_eq!(
        issues[0].message,
        "A generated value was refused: longer than 10 characters."
    );
}

/// Numbers in lists and constants respect the column type (review I3).
#[test]
fn numeric_lists_respect_the_type() {
    let tiny = column("t", "tinyint", LogicalType::Integer, false);
    let pick = Generator::Pick {
        values: vec!["1".into(), "300".into()],
    };
    assert_eq!(
        check_plans(&[spec(tiny, pick)], 1, MY)[0].message,
        "Values must be between -128 and 127."
    );
    let dec = column("d", "decimal(5,2)", LogicalType::Decimal, false);
    let constant = Generator::Constant {
        value: "99999.99".into(),
    };
    assert_eq!(
        check_plans(&[spec(dec, constant)], 1, MY)[0].message,
        "Values must be below 1000."
    );
}

/// A sequence on a decimal column stays within its precision (review I3).
#[test]
fn sequence_on_decimal_respects_precision() {
    let dec = column("d", "decimal(5,2)", LogicalType::Decimal, false);
    let issues = check_plans(
        &[spec(dec, Generator::Sequence { start: 1, step: 1 })],
        1000,
        MY,
    );
    assert_eq!(issues[0].message, "Values must be between -999 and 999.");
}
