//! Auto mapping, mapping checks and key choices (spec §6.2, R14).

use dnt_core::meta::{
    ColumnDetail, IdentityKind, IndexDetail, ObjectKind, ObjectRef, TableDetail, TableOptions,
};
use dnt_core::sql::build::{InsertOptions, OnConflict};
use dnt_core::value::LogicalType;
use dnt_transfer::convert::TargetColumn;
use dnt_transfer::mapping::{
    auto_map, check_mapping, insert_options, key_choices, ErrorPolicy, ImportMode, KeyChoice,
    Mapping, MappingReport,
};

fn target(name: &str) -> TargetColumn {
    TargetColumn {
        name: name.into(),
        db_type: "text".into(),
        logical: LogicalType::Text,
        nullable: true,
        has_default: false,
        auto_increment: false,
        identity: None,
        generated: false,
        enum_values: Vec::new(),
        max_chars: None,
        collation: None,
        unsigned: false,
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn m(target: &str, source: Option<u32>) -> Mapping {
    Mapping {
        target: target.into(),
        source,
    }
}

#[test]
fn auto_map_exact_then_case_then_normalised() {
    let targets = [
        target("id"),
        target("customer_name"),
        target("Email"),
        target("note"),
    ];
    let source = s(&["Customer Name", "EMAIL", "id", "email"]);
    assert_eq!(
        auto_map(&source, &targets),
        [
            m("id", Some(2)),
            m("customer_name", Some(0)),
            // Exact beats case-insensitive: "email" (3) is not exact either, so the first
            // case-insensitive match wins.
            m("Email", Some(1)),
            m("note", None),
        ]
    );
}

#[test]
fn auto_map_prefers_exact_over_earlier_case_match() {
    let targets = [target("Name"), target("name")];
    let source = s(&["NAME", "name"]);
    // "name" takes source 1 exactly; "Name" falls back to the case-insensitive source 0.
    assert_eq!(
        auto_map(&source, &targets),
        [m("Name", Some(0)), m("name", Some(1))]
    );
}

#[test]
fn auto_map_never_uses_a_source_twice() {
    let targets = [target("a_b"), target("ab"), target("AB")];
    let source = s(&["A B"]);
    let got = auto_map(&source, &targets);
    let used: Vec<u32> = got.iter().filter_map(|x| x.source).collect();
    assert_eq!(used, [0]);
    assert_eq!(got.len(), 3);
}

#[test]
fn auto_map_skips_generated_columns() {
    let mut g = target("total");
    g.generated = true;
    assert_eq!(auto_map(&s(&["total"]), &[g]), [m("total", None)]);
}

fn errors(r: &MappingReport) -> Vec<String> {
    r.errors.iter().map(|i| i.message.clone()).collect()
}

fn columns_of_errors(r: &MappingReport) -> Vec<Option<String>> {
    r.errors.iter().map(|i| i.column.clone()).collect()
}

#[test]
fn valid_mapping_has_no_errors() {
    let t = [target("a"), target("b")];
    let r = check_mapping(&[m("a", Some(0))], &t, &[], &ImportMode::Append, false);
    assert!(r.errors.is_empty(), "{r:?}");
}

#[test]
fn unknown_and_duplicate_targets_are_errors() {
    let t = [target("a")];
    let r = check_mapping(
        &[m("a", Some(0)), m("a", Some(1)), m("zz", Some(2))],
        &t,
        &[],
        &ImportMode::Append,
        false,
    );
    assert_eq!(
        columns_of_errors(&r),
        [Some("a".to_string()), Some("zz".to_string())]
    );
}

#[test]
fn nothing_mapped_is_an_error() {
    let t = [target("a")];
    let r = check_mapping(&[m("a", None)], &t, &[], &ImportMode::Append, false);
    assert_eq!(r.errors.len(), 1);
    assert_eq!(r.errors[0].column, None);
}

#[test]
fn generated_column_mapped_is_an_error() {
    let mut g = target("g");
    g.generated = true;
    let t = [target("a"), g];
    let r = check_mapping(
        &[m("a", Some(0)), m("g", Some(1))],
        &t,
        &[],
        &ImportMode::Append,
        false,
    );
    assert_eq!(columns_of_errors(&r), [Some("g".to_string())]);
}

#[test]
fn identity_always_needs_override() {
    let mut id = target("id");
    id.logical = LogicalType::Integer;
    id.nullable = false;
    id.has_default = true;
    id.identity = Some(IdentityKind::Always);
    let t = [id, target("a")];
    let map = [m("id", Some(0)), m("a", Some(1))];
    let r = check_mapping(&map, &t, &[], &ImportMode::Append, false);
    assert_eq!(columns_of_errors(&r), [Some("id".to_string())]);
    assert!(errors(&r)[0].contains("Override identity"), "{r:?}");
    let r = check_mapping(&map, &t, &[], &ImportMode::Append, true);
    assert!(r.errors.is_empty(), "{r:?}");
    // Writing identity values leaves the sequence behind: a warning.
    assert_eq!(r.warnings.len(), 1);
    assert_eq!(r.warnings[0].column.as_deref(), Some("id"));
}

#[test]
fn required_column_unmapped_is_an_error() {
    let mut req = target("req");
    req.nullable = false;
    let mut with_default = target("d");
    with_default.nullable = false;
    with_default.has_default = true;
    let t = [target("a"), req, with_default];
    let r = check_mapping(&[m("a", Some(0))], &t, &[], &ImportMode::Append, false);
    assert_eq!(columns_of_errors(&r), [Some("req".to_string())]);
    // Missing from the list entirely counts as unmapped too.
    let r = check_mapping(
        &[m("a", Some(0)), m("req", None)],
        &t,
        &[],
        &ImportMode::Append,
        false,
    );
    assert_eq!(columns_of_errors(&r), [Some("req".to_string())]);
}

fn keys() -> Vec<KeyChoice> {
    vec![
        KeyChoice {
            name: "PRIMARY".into(),
            columns: s(&["id"]),
            primary: true,
        },
        KeyChoice {
            name: "uq_ab".into(),
            columns: s(&["a", "b"]),
            primary: false,
        },
    ]
}

#[test]
fn upsert_key_must_be_mapped_and_a_key() {
    let t = [target("id"), target("a"), target("b"), target("c")];
    let all = [
        m("id", Some(0)),
        m("a", Some(1)),
        m("b", Some(2)),
        m("c", Some(3)),
    ];
    let up = |k: &[&str]| ImportMode::Upsert { key: s(k) };
    assert!(check_mapping(&all, &t, &keys(), &up(&["id"]), false)
        .errors
        .is_empty());
    // Key columns in another order are the same key.
    assert!(check_mapping(&all, &t, &keys(), &up(&["b", "a"]), false)
        .errors
        .is_empty());
    // Not a key.
    let r = check_mapping(&all, &t, &keys(), &up(&["c"]), false);
    assert_eq!(r.errors.len(), 1, "{r:?}");
    // Part of a key only.
    assert_eq!(
        check_mapping(&all, &t, &keys(), &up(&["a"]), false)
            .errors
            .len(),
        1
    );
    // Empty key.
    assert_eq!(
        check_mapping(&all, &t, &keys(), &up(&[]), false)
            .errors
            .len(),
        1
    );
    // Key column not mapped.
    let partial = [m("a", Some(1)), m("c", Some(3))];
    let r = check_mapping(&partial, &t, &keys(), &up(&["a", "b"]), false);
    assert_eq!(columns_of_errors(&r), [Some("b".to_string())]);
}

#[test]
fn upsert_without_non_key_columns_warns() {
    let t = [target("id"), target("a")];
    let r = check_mapping(
        &[m("id", Some(0))],
        &t,
        &keys(),
        &ImportMode::Upsert { key: s(&["id"]) },
        false,
    );
    assert!(r.errors.is_empty(), "{r:?}");
    assert_eq!(r.warnings.len(), 1, "{r:?}");
}

#[test]
fn insert_options_per_mode() {
    let mapped = s(&["id", "a", "b"]);
    assert_eq!(
        insert_options(&ImportMode::Append, &mapped, false),
        InsertOptions::default()
    );
    assert_eq!(
        insert_options(&ImportMode::Replace { truncate: true }, &mapped, true),
        InsertOptions {
            overriding_system_value: true,
            on_conflict: OnConflict::Error
        }
    );
    assert_eq!(
        insert_options(&ImportMode::Upsert { key: s(&["id"]) }, &mapped, false),
        InsertOptions {
            overriding_system_value: false,
            on_conflict: OnConflict::Update {
                key: s(&["id"]),
                update: s(&["a", "b"])
            }
        }
    );
}

fn col(name: &str, nullable: bool) -> ColumnDetail {
    ColumnDetail {
        name: name.into(),
        db_type: "int".into(),
        nullable,
        ..ColumnDetail::default()
    }
}

fn index(name: &str, cols: &[&str], unique: bool) -> IndexDetail {
    IndexDetail {
        name: name.into(),
        unique,
        columns: s(cols),
        ..IndexDetail::default()
    }
}

#[test]
fn key_choices_are_pk_and_not_null_full_unique_indexes() {
    let mut pk = index("PRIMARY", &["id"], true);
    pk.primary = true;
    let mut partial = index("uq_partial", &["a"], true);
    partial.partial = true;
    let mut expr = index("uq_expr", &["a"], true);
    expr.expression = Some("lower(a)".into());
    let detail = TableDetail {
        object: ObjectRef {
            database: "db".into(),
            schema: None,
            name: "t".into(),
            kind: ObjectKind::Table,
        },
        columns: vec![col("id", false), col("a", false), col("b", true)],
        primary_key: Some(pk.clone()),
        indexes: vec![
            pk,
            index("uq_a", &["a"], true),
            index("uq_ab", &["a", "b"], true),
            index("ix_a", &["a"], false),
            partial,
            expr,
        ],
        foreign_keys: Vec::new(),
        checks: Vec::new(),
        triggers: Vec::new(),
        options: TableOptions::default(),
        row_estimate: None,
        size_bytes: None,
        comment: None,
    };
    assert_eq!(
        key_choices(&detail),
        [
            KeyChoice {
                name: "PRIMARY".into(),
                columns: s(&["id"]),
                primary: true
            },
            KeyChoice {
                name: "uq_a".into(),
                columns: s(&["a"]),
                primary: false
            },
        ]
    );
}

#[test]
fn modes_and_policies_serialize_tagged() {
    assert_eq!(
        serde_json::to_value(ImportMode::Replace { truncate: true }).unwrap(),
        serde_json::json!({"kind": "replace", "truncate": true})
    );
    assert_eq!(
        serde_json::to_value(ImportMode::Append).unwrap(),
        serde_json::json!({"kind": "append"})
    );
    let up: ImportMode =
        serde_json::from_value(serde_json::json!({"kind": "upsert", "key": ["id"]})).unwrap();
    assert_eq!(up, ImportMode::Upsert { key: s(&["id"]) });
    assert_eq!(
        serde_json::to_value(ErrorPolicy::Skip { max_errors: 5 }).unwrap(),
        serde_json::json!({"kind": "skip", "max_errors": 5})
    );
    assert_eq!(
        serde_json::to_value(ErrorPolicy::Stop).unwrap(),
        serde_json::json!({"kind": "stop"})
    );
    assert_eq!(ErrorPolicy::DEFAULT_MAX_ERRORS, 1000);
}
