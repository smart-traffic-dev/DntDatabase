//! Batch builder (spec §6.3, R19): splits by rows and bytes, equals `insert_sql_with`, classifies
//! like its template.

use dnt_core::server::DbKind;
use dnt_core::sql::build::{insert_sql_with, ColumnTarget, InsertOptions, OnConflict, TableRef};
use dnt_core::sql::{classify, Dialect, StatementKind};
use dnt_core::value::{FullValue, LogicalType, Value};
use dnt_transfer::batch::{Batch, BatchBuilder};

fn table(kind: DbKind) -> TableRef {
    TableRef {
        kind: kind.as_sql(),
        database: "db".into(),
        schema: (kind == DbKind::Postgres).then(|| "public".into()),
        name: "t".into(),
    }
}

fn cols() -> Vec<ColumnTarget> {
    vec![
        ColumnTarget {
            name: "id".into(),
            db_type: "integer".into(),
            logical: LogicalType::Integer,
            collation: None,
        },
        ColumnTarget {
            name: "name".into(),
            db_type: "text".into(),
            logical: LogicalType::Text,
            collation: None,
        },
    ]
}

fn full(v: Value) -> FullValue {
    FullValue::try_from_value(v).unwrap()
}

fn row(i: i64, name: &str) -> Vec<FullValue> {
    vec![full(Value::Int(i)), full(Value::Text(name.into()))]
}

fn upsert() -> InsertOptions {
    InsertOptions {
        overriding_system_value: false,
        on_conflict: OnConflict::Update {
            key: vec!["id".into()],
            update: vec!["name".into()],
        },
    }
}

fn dialects() -> [(DbKind, Dialect); 2] {
    [
        (DbKind::MySql, Dialect::MYSQL),
        (DbKind::Postgres, Dialect::Postgres),
    ]
}

/// Pushes every row (record number = 10 + index) and flushes; returns the batches.
fn run(b: &mut BatchBuilder, rows: &[Vec<FullValue>]) -> Vec<Batch> {
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(batch) = b.push(r.clone(), 10 + i as u64).unwrap() {
            out.push(batch);
        }
    }
    out.extend(b.flush());
    assert!(b.flush().is_none());
    out
}

#[test]
fn splits_by_rows_and_equals_insert_sql_with() {
    for (kind, d) in dialects() {
        for opts in [InsertOptions::default(), upsert()] {
            let rows: Vec<_> = (0..7).map(|i| row(i, &format!("n{i}"))).collect();
            let mut b =
                BatchBuilder::new(table(kind), cols(), opts.clone(), d, 3, 1 << 20).unwrap();
            let batches = run(&mut b, &rows);
            assert_eq!(
                batches.iter().map(|b| b.rows).collect::<Vec<_>>(),
                [3, 3, 1]
            );
            let mut start = 0;
            for batch in &batches {
                let n = batch.rows as usize;
                let expect =
                    insert_sql_with(&table(kind), &cols(), &rows[start..start + n], &opts, d)
                        .unwrap();
                assert_eq!(batch.sql, expect);
                let records: Vec<u64> = (start..start + n).map(|i| 10 + i as u64).collect();
                assert_eq!(batch.records, records);
                assert_eq!(batch.first_record, records[0]);
                start += n;
            }
        }
    }
}

#[test]
fn splits_by_bytes() {
    let (kind, d) = dialects()[1];
    let rows: Vec<_> = (0..6).map(|i| row(i, &"x".repeat(20))).collect();
    let one = insert_sql_with(
        &table(kind),
        &cols(),
        &rows[..1],
        &InsertOptions::default(),
        d,
    )
    .unwrap()
    .len();
    let two = insert_sql_with(
        &table(kind),
        &cols(),
        &rows[..2],
        &InsertOptions::default(),
        d,
    )
    .unwrap()
    .len();
    // Room for two rows but not three.
    let mut b =
        BatchBuilder::new(table(kind), cols(), InsertOptions::default(), d, 1000, two).unwrap();
    let batches = run(&mut b, &rows);
    assert_eq!(
        batches.iter().map(|b| b.rows).collect::<Vec<_>>(),
        [2, 2, 2]
    );
    for batch in &batches {
        assert!(batch.sql.len() <= two);
    }
    assert!(one < two);
}

#[test]
fn a_row_larger_than_max_bytes_is_its_own_batch() {
    let (kind, d) = dialects()[0];
    let rows = vec![
        row(1, "a"),
        row(2, &"big".repeat(100)),
        row(3, "b"),
        row(4, "c"),
    ];
    let mut b =
        BatchBuilder::new(table(kind), cols(), InsertOptions::default(), d, 1000, 120).unwrap();
    let batches = run(&mut b, &rows);
    assert_eq!(
        batches
            .iter()
            .map(|b| b.records.clone())
            .collect::<Vec<_>>(),
        [vec![10], vec![11], vec![12, 13]]
    );
    assert!(batches[1].sql.len() > 120);
    assert_eq!(
        batches[1].sql,
        insert_sql_with(
            &table(kind),
            &cols(),
            &rows[1..2],
            &InsertOptions::default(),
            d
        )
        .unwrap()
    );
}

#[test]
fn max_rows_one_sends_every_row_alone() {
    let (kind, d) = dialects()[1];
    let rows: Vec<_> = (0..3).map(|i| row(i, "r")).collect();
    let mut b =
        BatchBuilder::new(table(kind), cols(), InsertOptions::default(), d, 1, 1 << 20).unwrap();
    let batches = run(&mut b, &rows);
    assert_eq!(batches.len(), 3);
    assert!(batches.iter().all(|b| b.rows == 1));
}

#[test]
fn pg_upsert_starts_a_new_batch_when_a_key_repeats() {
    let (kind, d) = dialects()[1];
    let rows = vec![
        row(1, "a"),
        row(2, "b"),
        row(1, "c"),
        row(3, "d"),
        row(2, "e"),
        row(2, "f"),
    ];
    let mut b = BatchBuilder::new(table(kind), cols(), upsert(), d, 1000, 1 << 20).unwrap();
    let batches = run(&mut b, &rows);
    assert_eq!(
        batches
            .iter()
            .map(|b| b.records.clone())
            .collect::<Vec<_>>(),
        [vec![10, 11], vec![12, 13, 14], vec![15]]
    );
    // Each batch is still exactly insert_sql_with of its rows.
    assert_eq!(
        batches[1].sql,
        insert_sql_with(&table(kind), &cols(), &rows[2..5], &upsert(), d).unwrap()
    );
}

#[test]
fn repeated_keys_stay_together_without_pg_upsert() {
    let rows = vec![row(1, "a"), row(1, "b"), row(1, "c")];
    // MySQL upsert handles a repeated key in one statement; plain PG inserts have no key.
    for (kind, d, opts) in [
        (DbKind::MySql, Dialect::MYSQL, upsert()),
        (
            DbKind::Postgres,
            Dialect::Postgres,
            InsertOptions::default(),
        ),
    ] {
        let mut b = BatchBuilder::new(table(kind), cols(), opts, d, 1000, 1 << 20).unwrap();
        let batches = run(&mut b, &rows);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].rows, 3);
    }
}

#[test]
fn batch_is_head_tuples_and_tail() {
    let (kind, d) = dialects()[1];
    let mut b = BatchBuilder::new(table(kind), cols(), upsert(), d, 10, 1 << 20).unwrap();
    assert!(b.flush().is_none(), "nothing pending");
    assert!(b.push(row(1, "a"), 1).unwrap().is_none());
    assert_eq!(
        b.flush().unwrap().sql,
        "INSERT INTO \"public\".\"t\" (\"id\", \"name\") VALUES (1, 'a') \
         ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\""
    );
}

#[test]
fn bad_row_is_an_error_and_keeps_the_buffer() {
    let (kind, d) = dialects()[0];
    let mut b = BatchBuilder::new(
        table(kind),
        cols(),
        InsertOptions::default(),
        d,
        10,
        1 << 20,
    )
    .unwrap();
    assert!(b.push(row(1, "a"), 1).unwrap().is_none());
    assert!(b.push(vec![full(Value::Int(2))], 2).is_err());
    let nan = vec![full(Value::Float(f64::NAN)), full(Value::Text("x".into()))];
    let mut float_cols = cols();
    float_cols[0].logical = LogicalType::Float;
    let mut fb = BatchBuilder::new(
        table(kind),
        float_cols,
        InsertOptions::default(),
        d,
        10,
        1 << 20,
    )
    .unwrap();
    assert!(fb.push(nan, 3).is_err());
    assert!(fb.flush().is_none());
    let last = b.flush().unwrap();
    assert_eq!(last.records, [1]);
}

#[test]
fn invalid_options_are_refused_at_new() {
    let overriding = InsertOptions {
        overriding_system_value: true,
        on_conflict: OnConflict::Error,
    };
    assert!(BatchBuilder::new(
        table(DbKind::MySql),
        cols(),
        overriding,
        Dialect::MYSQL,
        10,
        1 << 20
    )
    .is_err());
}

#[test]
fn generated_batches_classify_like_the_template() {
    for (kind, d) in dialects() {
        for opts in [InsertOptions::default(), upsert()] {
            let mut b = BatchBuilder::new(table(kind), cols(), opts, d, 2, 1 << 20).unwrap();
            let rows: Vec<_> = (0..5)
                .map(|i| row(i, "it's; a -- tricky /* value */"))
                .collect();
            for batch in run(&mut b, &rows) {
                assert_eq!(
                    classify(&batch.sql, d),
                    StatementKind::Write,
                    "{}",
                    batch.sql
                );
                assert_eq!(dnt_core::sql::split(&batch.sql, d).len(), 1);
            }
        }
    }
}
