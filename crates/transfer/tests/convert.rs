//! Cell conversion (spec §6.2): one accept/reject table per logical type, null and empty rules,
//! and `TargetColumn::from_detail`.

use dnt_core::meta::{ColumnDetail, IdentityKind};
use dnt_core::server::DbKind;
use dnt_core::sql::build::{literal, BuildError};
use dnt_core::sql::Dialect;
use dnt_core::value::{FullValue, LogicalType, Value};
use dnt_transfer::convert::{
    convert, BinaryEncoding, CellError, CellReason, ConvertOptions, EmptyAs, TargetColumn,
};
use dnt_transfer::read::Cell;

const MY: DbKind = DbKind::MySql;
const PG: DbKind = DbKind::Postgres;

fn col(logical: LogicalType, db_type: &str) -> TargetColumn {
    TargetColumn {
        name: "c".into(),
        db_type: db_type.into(),
        logical,
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

fn opts() -> ConvertOptions {
    ConvertOptions::default()
}

fn text(s: &str) -> Cell {
    Cell::Text(s.into())
}

fn num(s: &str) -> Cell {
    Cell::Number(s.into())
}

fn ok(cell: Cell, c: &TargetColumn, kind: DbKind) -> Value {
    match convert(&cell, c, &opts(), kind) {
        Ok(v) => v.as_value().cloned().expect("not binary"),
        Err(e) => panic!("{cell:?} into {}: {e}", c.db_type),
    }
}

fn err(cell: Cell, c: &TargetColumn, kind: DbKind) -> CellReason {
    match convert(&cell, c, &opts(), kind) {
        Ok(v) => panic!("{cell:?} into {} accepted as {v:?}", c.db_type),
        Err(e) => {
            assert_eq!(e.column, "c");
            e.reason
        }
    }
}

#[test]
fn integer_accepts_and_rejects() {
    let c = col(LogicalType::Integer, "bigint");
    assert_eq!(ok(text("42"), &c, MY), Value::Int(42));
    assert_eq!(ok(text("-7"), &c, MY), Value::Int(-7));
    assert_eq!(ok(num("9"), &c, PG), Value::Int(9));
    assert_eq!(
        ok(text("18446744073709551615"), &c, MY),
        Value::UInt(u64::MAX)
    );
    assert_eq!(
        err(text("18446744073709551616"), &c, MY),
        CellReason::OutOfRange
    );
    assert_eq!(
        err(text("-9223372036854775809"), &c, MY),
        CellReason::OutOfRange
    );
    for bad in ["1.5", "abc", "1e3", "", " 1", "+-1", "0x10"] {
        if bad.is_empty() {
            // Empty cell into a non-text column is NULL.
            assert!(convert(&text(bad), &c, &opts(), MY).unwrap().is_null());
            continue;
        }
        assert!(
            matches!(err(text(bad), &c, MY), CellReason::Invalid { .. }),
            "{bad}"
        );
    }
    assert_eq!(ok(Cell::Bool(true), &c, MY), Value::Int(1));
    let mut u = col(LogicalType::Integer, "int(10) unsigned");
    u.unsigned = true;
    assert_eq!(err(text("-1"), &u, MY), CellReason::Negative);
    assert_eq!(ok(text("0"), &u, MY), Value::Int(0));
}

#[test]
fn decimal_accepts_and_rejects() {
    let c = col(LogicalType::Decimal, "decimal(12,2)");
    assert_eq!(
        ok(text("12345678901234567890.25"), &c, MY),
        Value::Decimal("12345678901234567890.25".into())
    );
    assert_eq!(ok(num("1e400"), &c, PG), Value::Decimal("1e400".into()));
    assert_eq!(ok(text("-0.5"), &c, MY), Value::Decimal("-0.5".into()));
    assert_eq!(ok(text("NaN"), &c, PG), Value::Decimal("NaN".into()));
    for bad in ["1,5", "abc", "1.", ".5", "NaN"] {
        assert!(
            matches!(err(text(bad), &c, MY), CellReason::Invalid { .. }),
            "{bad}"
        );
    }
}

#[test]
fn float_keeps_nan_and_mysql_refuses_it_at_literal_time() {
    let c = col(LogicalType::Float, "double");
    assert_eq!(ok(text("1.5"), &c, MY), Value::Float(1.5));
    assert_eq!(ok(num("-2e-3"), &c, PG), Value::Float(-0.002));
    let nan = convert(&text("NaN"), &c, &opts(), MY).unwrap();
    assert!(matches!(nan.as_value(), Some(Value::Float(f)) if f.is_nan()));
    // Conversion keeps NaN; MySQL refuses it when the literal is built.
    let target = c.column_target();
    assert!(matches!(
        literal(&nan, &target, Dialect::MYSQL),
        Err(BuildError::Invalid { .. })
    ));
    assert_eq!(
        literal(&nan, &target, Dialect::Postgres).unwrap(),
        "'NaN'::float8"
    );
    assert_eq!(
        ok(text("-Infinity"), &c, PG),
        Value::Float(f64::NEG_INFINITY)
    );
    assert_eq!(err(num("1e400"), &c, PG), CellReason::OutOfRange);
    assert!(matches!(
        err(text("one"), &c, PG),
        CellReason::Invalid { .. }
    ));
}

#[test]
fn bool_spellings() {
    let c = col(LogicalType::Bool, "boolean");
    for t in ["true", "TRUE", "1", "yes", "Y", "t"] {
        assert_eq!(ok(text(t), &c, PG), Value::Bool(true), "{t}");
    }
    for f in ["false", "0", "No", "n", "F"] {
        assert_eq!(ok(text(f), &c, PG), Value::Bool(false), "{f}");
    }
    assert_eq!(ok(num("1"), &c, PG), Value::Bool(true));
    assert_eq!(ok(Cell::Bool(false), &c, PG), Value::Bool(false));
    for bad in ["2", "on", "maybe"] {
        assert!(
            matches!(err(text(bad), &c, PG), CellReason::Invalid { .. }),
            "{bad}"
        );
    }
}

#[test]
fn text_length_is_counted_in_chars() {
    let mut c = col(LogicalType::Text, "varchar(6)");
    c.max_chars = Some(6);
    assert_eq!(ok(text("Nguyễn"), &c, MY), Value::Text("Nguyễn".into()));
    assert_eq!(err(text("Nguyễnx"), &c, MY), CellReason::TooLong { max: 6 });
    // Numbers keep their verbatim text in a text column.
    let t = col(LogicalType::Text, "text");
    assert_eq!(ok(num("1.50"), &t, MY), Value::Text("1.50".into()));
    assert_eq!(
        ok(num("12345678901234567890"), &t, MY),
        Value::Text("12345678901234567890".into())
    );
    assert_eq!(ok(Cell::Bool(true), &t, MY), Value::Text("true".into()));
    assert_eq!(
        ok(Cell::Json("{\"a\":1}".into()), &t, PG),
        Value::Text("{\"a\":1}".into())
    );
    assert_eq!(
        ok(Cell::DateTime("2024-01-02T03:04:05".into()), &t, PG),
        Value::Text("2024-01-02T03:04:05".into())
    );
}

#[test]
fn dates_and_times_are_normalised() {
    let d = col(LogicalType::Date, "date");
    assert_eq!(
        ok(text("2024-01-02"), &d, MY),
        Value::Date("2024-01-02".into())
    );
    assert_eq!(
        ok(Cell::DateTime("2024-01-02".into()), &d, MY),
        Value::Date("2024-01-02".into())
    );
    for bad in ["2024-1-2", "02/01/2024", "2024-01-02X03:04:05", "x"] {
        assert!(
            matches!(err(text(bad), &d, MY), CellReason::Invalid { .. }),
            "{bad}"
        );
    }
    let t = col(LogicalType::Time, "time");
    assert_eq!(ok(text("03:04"), &t, MY), Value::Time("03:04".into()));
    assert_eq!(
        ok(text("03:04:05.123456"), &t, MY),
        Value::Time("03:04:05.123456".into())
    );
    assert!(matches!(
        err(text("03:04:05.1234567"), &t, MY),
        CellReason::Invalid { .. }
    ));
    assert!(matches!(
        err(text("3:4"), &t, MY),
        CellReason::Invalid { .. }
    ));

    let dt = col(LogicalType::DateTime, "datetime");
    assert_eq!(
        ok(text("2024-01-02T03:04:05"), &dt, MY),
        Value::DateTime("2024-01-02 03:04:05".into())
    );
    assert_eq!(
        ok(Cell::DateTime("2024-01-02T03:04:05".into()), &dt, MY),
        Value::DateTime("2024-01-02 03:04:05".into())
    );
    assert_eq!(
        ok(text("2024-01-02 03:04"), &dt, MY),
        Value::DateTime("2024-01-02 03:04".into())
    );
    assert_eq!(
        ok(text("2024-01-02 03:04:05.5"), &dt, MY),
        Value::DateTime("2024-01-02 03:04:05.5".into())
    );
    // A plain timestamp has no offset.
    assert!(matches!(
        err(text("2024-01-02T03:04:05+07:00"), &dt, PG),
        CellReason::Invalid { .. }
    ));
    let tz = col(LogicalType::DateTime, "timestamp with time zone");
    assert_eq!(
        ok(text("2024-01-02T03:04:05+07:00"), &tz, PG),
        Value::TimestampTz("2024-01-02 03:04:05+07:00".into())
    );
    assert_eq!(
        ok(text("2024-01-02T03:04:05Z"), &tz, PG),
        Value::TimestampTz("2024-01-02 03:04:05Z".into())
    );
    assert_eq!(
        ok(text("2024-01-02 03:04:05"), &tz, PG),
        Value::TimestampTz("2024-01-02 03:04:05".into())
    );
}

#[test]
fn date_column_keeps_the_date_of_a_datetime() {
    let d = col(LogicalType::Date, "date");
    for (kind, cell) in [
        (MY, text("2024-01-02 03:04:05")),
        (MY, text("2024-01-02T00:00:00")),
        (MY, Cell::DateTime("2024-01-02T03:04:05".into())),
        (MY, text("2024-01-02 03:04:05.5+07:00")),
        (PG, text("2024-01-02T03:04:05Z")),
    ] {
        assert_eq!(
            ok(cell, &d, kind),
            Value::Date("2024-01-02".into()),
            "{kind:?}"
        );
    }
    for bad in ["2024-01-02 25:00:00", "2024-01-02 03:04:05 junk"] {
        assert!(
            matches!(err(text(bad), &d, MY), CellReason::Invalid { .. }),
            "{bad}"
        );
    }
}

#[test]
fn mysql_zero_dates_are_left_to_the_server() {
    let d = col(LogicalType::Date, "date");
    let dt = col(LogicalType::DateTime, "datetime");
    assert_eq!(
        ok(text("0000-00-00"), &d, MY),
        Value::Date("0000-00-00".into())
    );
    assert_eq!(
        ok(text("2024-02-00"), &d, MY),
        Value::Date("2024-02-00".into())
    );
    assert_eq!(
        ok(text("0000-00-00 00:00:00"), &dt, MY),
        Value::DateTime("0000-00-00 00:00:00".into())
    );
    // An export writes the `T` form.
    assert_eq!(
        ok(text("0000-00-00T00:00:00"), &dt, MY),
        Value::DateTime("0000-00-00 00:00:00".into())
    );
    // PostgreSQL has no zero dates; month 13 is wrong everywhere.
    for (kind, bad) in [(PG, "0000-00-00"), (MY, "2024-13-01"), (MY, "2024-01-32")] {
        assert!(
            matches!(err(text(bad), &d, kind), CellReason::Invalid { .. }),
            "{bad}"
        );
    }
}

#[test]
fn mysql_datetime_takes_an_offset_in_its_own_form() {
    for ty in ["datetime", "timestamp"] {
        let dt = col(LogicalType::DateTime, ty);
        for (input, sent) in [
            ("2024-01-02T03:04:05+07:00", "2024-01-02 03:04:05+07:00"),
            ("2024-01-02T03:04:05.123+0700", "2024-01-02 03:04:05.123+07:00"),
            ("2024-01-02 03:04:05-05", "2024-01-02 03:04:05-05:00"),
            ("2024-01-02T03:04:05Z", "2024-01-02 03:04:05+00:00"),
            ("2024-01-02T03:04:05-00:00", "2024-01-02 03:04:05+00:00"),
            ("2024-01-02 03:04:05+14:00", "2024-01-02 03:04:05+14:00"),
            ("2024-01-02 03:04:05-13:59", "2024-01-02 03:04:05-13:59"),
        ] {
            assert_eq!(
                ok(text(input), &dt, MY),
                Value::DateTime(sent.into()),
                "{input} into {ty}"
            );
        }
        // MySQL's range is -13:59 to +14:00.
        for bad in [
            "2024-01-02 03:04:05+14:01",
            "2024-01-02 03:04:05-14:00",
            "2024-01-02 03:04:05+0760",
        ] {
            assert!(
                matches!(err(text(bad), &dt, MY), CellReason::Invalid { .. }),
                "{bad} into {ty}"
            );
        }
    }
}

#[test]
fn json_uuid_enum() {
    let j = col(LogicalType::Json, "jsonb");
    assert_eq!(
        ok(Cell::Json("{\"a\":[1,2]}".into()), &j, PG),
        Value::Json("{\"a\":[1,2]}".into())
    );
    assert_eq!(ok(text("[1]"), &j, PG), Value::Json("[1]".into()));
    assert_eq!(ok(num("1.5"), &j, PG), Value::Json("1.5".into()));
    assert_eq!(ok(Cell::Bool(true), &j, PG), Value::Json("true".into()));
    assert!(matches!(
        err(text("{a:1}"), &j, PG),
        CellReason::Invalid { .. }
    ));

    let u = col(LogicalType::Uuid, "uuid");
    let id = "123e4567-e89b-12d3-a456-426614174000";
    assert_eq!(ok(text(id), &u, PG), Value::Uuid(id.into()));
    assert!(matches!(
        err(text("123e4567e89b12d3a456426614174000"), &u, PG),
        CellReason::Invalid { .. }
    ));

    let mut e = col(LogicalType::Enum, "enum('new','paid')");
    e.enum_values = vec!["new".into(), "paid".into()];
    assert_eq!(ok(text("paid"), &e, MY), Value::Text("paid".into()));
    assert_eq!(err(text("Paid"), &e, MY), CellReason::NotInEnum);
}

#[test]
fn binary_encodings() {
    let c = col(LogicalType::Binary, "bytea");
    let bin = |cell: Cell, enc: BinaryEncoding| {
        let o = ConvertOptions {
            binary: enc,
            ..ConvertOptions::default()
        };
        convert(&cell, &c, &o, PG)
    };
    let bytes = |r: Result<FullValue, CellError>| r.unwrap().as_bytes().unwrap().to_vec();
    assert_eq!(
        bytes(bin(text("0xDEADbeef"), BinaryEncoding::Hex)),
        [0xde, 0xad, 0xbe, 0xef]
    );
    assert_eq!(bytes(bin(text("00ff"), BinaryEncoding::Hex)), [0, 255]);
    assert!(bin(text("0x0"), BinaryEncoding::Hex).is_err());
    assert!(bin(text("zz"), BinaryEncoding::Hex).is_err());
    assert_eq!(
        bytes(bin(text("AAEC/w=="), BinaryEncoding::Base64)),
        [0, 1, 2, 255]
    );
    assert!(bin(text("***"), BinaryEncoding::Base64).is_err());
    assert_eq!(
        bytes(bin(text("hé"), BinaryEncoding::Utf8Text)),
        "hé".as_bytes()
    );
    // An empty cell is NULL, not empty bytes.
    assert!(bin(text(""), BinaryEncoding::Hex).unwrap().is_null());
}

#[test]
fn array_and_other_are_unsupported() {
    for (l, t) in [
        (LogicalType::Array, "integer[]"),
        (LogicalType::Other, "point"),
    ] {
        let c = col(l, t);
        assert_eq!(err(text("x"), &c, PG), CellReason::Unsupported);
        // NULL is still fine.
        assert!(convert(&Cell::Null, &c, &opts(), PG).unwrap().is_null());
    }
}

#[test]
fn null_text_empty_cells_and_trim() {
    let t = col(LogicalType::Text, "text");
    let i = col(LogicalType::Integer, "int");
    let o = ConvertOptions {
        empty: EmptyAs::EmptyString,
        null_text: Some("\\N".into()),
        trim: true,
        binary: BinaryEncoding::Hex,
    };
    assert!(convert(&text("\\N"), &t, &o, MY).unwrap().is_null());
    assert!(convert(&text("  \\N "), &t, &o, MY).unwrap().is_null());
    assert_eq!(
        convert(&text(""), &t, &o, MY).unwrap().as_value(),
        Some(&Value::Text(String::new()))
    );
    // Empty into a non-text column is NULL whatever `empty` says.
    assert!(convert(&text("   "), &i, &o, MY).unwrap().is_null());
    assert_eq!(
        convert(&text(" 5 "), &i, &o, MY).unwrap().as_value(),
        Some(&Value::Int(5))
    );
    // Default: empty text is NULL, no trimming, no null text.
    assert!(convert(&text(""), &t, &opts(), MY).unwrap().is_null());
    assert_eq!(
        convert(&text(" a "), &t, &opts(), MY).unwrap().as_value(),
        Some(&Value::Text(" a ".into()))
    );
    assert_eq!(
        convert(&text("\\N"), &t, &opts(), MY).unwrap().as_value(),
        Some(&Value::Text("\\N".into()))
    );
}

#[test]
fn null_into_not_null_is_an_error_even_with_a_default() {
    let mut c = col(LogicalType::Integer, "int");
    c.nullable = false;
    assert_eq!(err(Cell::Null, &c, MY), CellReason::NotNull);
    assert_eq!(err(text(""), &c, MY), CellReason::NotNull);
    // A mapped column gets an explicit NULL, never its default: refused.
    c.has_default = true;
    assert_eq!(err(Cell::Null, &c, MY), CellReason::NotNull);
    assert_eq!(err(Cell::Null, &c, PG), CellReason::NotNull);
    // Except MySQL AUTO_INCREMENT: NULL makes MySQL generate the value.
    c.auto_increment = true;
    assert!(convert(&Cell::Null, &c, &opts(), MY).unwrap().is_null());
    assert!(convert(&text(""), &c, &opts(), MY).unwrap().is_null());
    // PostgreSQL identity is not AUTO_INCREMENT.
    let mut id = col(LogicalType::Integer, "integer");
    id.nullable = false;
    id.has_default = true;
    id.identity = Some(IdentityKind::ByDefault);
    assert_eq!(err(Cell::Null, &id, PG), CellReason::NotNull);
}

#[test]
fn mysql_time_is_a_duration_pg_time_is_a_clock() {
    let t = col(LogicalType::Time, "time");
    for ok_my in ["838:59:59", "-1:00:00", "5:04", "-838:59:59.5", "03:04:05"] {
        assert_eq!(
            ok(text(ok_my), &t, MY),
            Value::Time(ok_my.into()),
            "{ok_my}"
        );
    }
    for bad_my in ["1000:00:00", "--1:00", "1:2:3", "+1:00:00"] {
        assert!(
            matches!(err(text(bad_my), &t, MY), CellReason::Invalid { .. }),
            "{bad_my}"
        );
    }
    for bad_pg in ["838:59:59", "-1:00:00", "5:04", "24:00:01"] {
        assert!(
            matches!(err(text(bad_pg), &t, PG), CellReason::Invalid { .. }),
            "{bad_pg}"
        );
    }
    assert_eq!(ok(text("23:59:59"), &t, PG), Value::Time("23:59:59".into()));
}

#[test]
fn pg_fixed_char_ignores_trailing_spaces() {
    let mut c = col(LogicalType::Text, "character(3)");
    c.max_chars = Some(3);
    assert_eq!(ok(text("ab    "), &c, PG), Value::Text("ab    ".into()));
    assert_eq!(err(text("abcd  "), &c, PG), CellReason::TooLong { max: 3 });
    let mut b = col(LogicalType::Text, "char(3)");
    b.max_chars = Some(3);
    assert_eq!(ok(text("abc "), &b, PG), Value::Text("abc ".into()));
    // varchar counts every character; MySQL keeps the plain rule.
    let mut v = col(LogicalType::Text, "character varying(3)");
    v.max_chars = Some(3);
    assert_eq!(err(text("ab  "), &v, PG), CellReason::TooLong { max: 3 });
    assert_eq!(err(text("abc "), &b, MY), CellReason::TooLong { max: 3 });
}

#[test]
fn cell_error_never_holds_the_value() {
    let mut c = col(LogicalType::Text, "varchar(3)");
    c.max_chars = Some(3);
    let secret = "s3cr3t-value";
    let e = convert(&text(secret), &c, &opts(), MY).unwrap_err();
    assert!(!e.to_string().contains(secret), "{e}");
    assert!(!format!("{e:?}").contains(secret));
    let i = col(LogicalType::Integer, "int");
    let e = convert(&text(secret), &i, &opts(), MY).unwrap_err();
    assert!(!e.to_string().contains(secret), "{e}");
    assert!(!format!("{e:?}").contains(secret));
    assert!(e.to_string().starts_with("column c: "), "{e}");
}

fn detail(name: &str, db_type: &str) -> ColumnDetail {
    ColumnDetail {
        name: name.into(),
        db_type: db_type.into(),
        nullable: true,
        ..ColumnDetail::default()
    }
}

#[test]
fn target_column_from_detail() {
    let cases = [
        ("varchar(255)", Some(255)),
        ("char(2)", Some(2)),
        ("character varying(40)", Some(40)),
        ("character(1)", Some(1)),
        ("VARCHAR(10)", Some(10)),
        ("text", None),
        ("varchar", None),
        ("int(11)", None),
        ("binary(16)", None),
    ];
    for (ty, max) in cases {
        let t = TargetColumn::from_detail(&detail("a", ty), LogicalType::Text, MY);
        assert_eq!(t.max_chars, max, "{ty}");
    }
    let plain = TargetColumn::from_detail(&detail("a", "int"), LogicalType::Integer, MY);
    assert!(!plain.has_default && !plain.generated && plain.nullable);

    let mut d = detail("id", "int(10) unsigned");
    d.nullable = false;
    d.auto_increment = true;
    d.unsigned = true;
    let t = TargetColumn::from_detail(&d, LogicalType::Integer, MY);
    assert!(t.has_default && t.auto_increment && t.unsigned && !t.nullable);

    let mut d = detail("id", "integer");
    d.identity = Some("ALWAYS".into());
    d.identity_kind = Some(IdentityKind::Always);
    let t = TargetColumn::from_detail(&d, LogicalType::Integer, PG);
    assert!(t.has_default);
    assert_eq!(t.identity, Some(IdentityKind::Always));

    let mut d = detail("g", "int");
    d.generated = Some("a + 1".into());
    assert!(TargetColumn::from_detail(&d, LogicalType::Integer, MY).generated);

    let mut d = detail("n", "varchar(5)");
    d.default = Some("'x'".into());
    d.collation = Some("utf8mb4_bin".into());
    let t = TargetColumn::from_detail(&d, LogicalType::Text, MY);
    assert!(t.has_default);
    assert_eq!(t.collation.as_deref(), Some("utf8mb4_bin"));
    assert_eq!(t.column_target().collation.as_deref(), Some("utf8mb4_bin"));
}

#[test]
fn options_serialize_as_the_ui_sees_them() {
    let o = ConvertOptions::default();
    assert_eq!(
        serde_json::to_value(&o).unwrap(),
        serde_json::json!({"empty": "null", "null_text": null, "trim": false, "binary": "hex"})
    );
    assert_eq!(
        serde_json::to_value(EmptyAs::EmptyString).unwrap(),
        serde_json::json!("empty_string")
    );
    assert_eq!(
        serde_json::to_value(BinaryEncoding::Utf8Text).unwrap(),
        serde_json::json!("utf8_text")
    );
}
