//! Property test: whatever text goes into `literal`, the server's lexer (modelled by
//! `core::sql::tokenize`) sees exactly one string literal, nothing that ends the
//! statement, and the literal's body decodes back to the original text.

use dnt_core::sql::build::{literal, BuildError, ColumnTarget};
use dnt_core::sql::{tokenize, Dialect, MySqlMode, TokKind};
use dnt_core::value::{FullValue, LogicalType, Value};
use proptest::prelude::*;

const PIECES: &[&str] = &[
    "'", "''", "\\", "\\'", "\"", "`", "$$", "$a$", ";", "--", "-- ", "/*", "*/", "#", "\n", "\r",
    " ", "\t", "\0", "E'", "e", "x", "_utf8mb4", "ễ", "—", "😀", "\u{a0}", "\u{2028}", "a", "1",
    "?", ":", "::", "(", ")",
];

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(PIECES), 0..40).prop_map(|p| p.concat())
}

fn dialects() -> Vec<Dialect> {
    let mut out = vec![Dialect::Postgres];
    for ansi_quotes in [false, true] {
        for no_backslash_escapes in [false, true] {
            out.push(Dialect::MySql(MySqlMode {
                ansi_quotes,
                no_backslash_escapes,
            }));
        }
    }
    out
}

/// Builds the value a column holds from its text.
type MakeValue = fn(String) -> Value;

/// Columns whose literal is a quoted string: plain text, a PG cast, MySQL `CAST(… AS JSON)`.
fn targets() -> Vec<(ColumnTarget, MakeValue)> {
    let c = |db_type: &str, logical| ColumnTarget {
        name: "c".into(),
        db_type: db_type.into(),
        logical,
        collation: None,
    };
    vec![
        (c("text", LogicalType::Text), Value::Text),
        (c("jsonb", LogicalType::Json), Value::Json),
        (c("uuid", LogicalType::Uuid), Value::Uuid),
        (c("date", LogicalType::Date), Value::Date),
        (c("order_status", LogicalType::Enum), Value::Text),
    ]
}

/// The text a quoted literal stands for, read the way the server reads it.
fn decode(quoted: &str, d: Dialect) -> String {
    let body = &quoted[1..quoted.len() - 1];
    let backslash = matches!(d, Dialect::MySql(m) if !m.no_backslash_escapes);
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if backslash => match chars.next().expect("dangling backslash") {
                '0' => out.push('\0'),
                other => out.push(other),
            },
            '\'' => {
                assert_eq!(chars.next(), Some('\''), "lone quote in {quoted:?}");
                out.push('\'');
            }
            c => out.push(c),
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn literal_never_escapes_its_quotes(s in text()) {
        for d in dialects() {
            for (col, make) in targets() {
                let v = FullValue::try_from_value(make(s.clone())).unwrap();
                let sql = match literal(&v, &col, d) {
                    Ok(sql) => sql,
                    Err(BuildError::Invalid { .. }) => {
                        // Only a NUL that the dialect cannot write is refused.
                        let backslash = matches!(d, Dialect::MySql(m) if !m.no_backslash_escapes);
                        prop_assert!(s.contains('\0') && !backslash, "{d:?} {s:?}");
                        continue;
                    }
                    Err(e) => return Err(TestCaseError::fail(format!("{d:?} {s:?}: {e}"))),
                };
                let toks = tokenize(&sql, d);
                let quoted: Vec<_> = toks.iter().filter(|t| t.kind == TokKind::Quoted).collect();
                prop_assert_eq!(quoted.len(), 1, "{:?} {:?} -> {}", d, s, sql);
                prop_assert!(
                    !toks.iter().any(|t| t.kind == TokKind::Punct(b';')),
                    "{:?} -> {}", d, sql
                );
                // Everything around the string is the fixed prefix / cast.
                for t in &toks {
                    if let TokKind::Word(w) = t.kind {
                        prop_assert!(
                            ["_utf8mb4", "CAST", "AS", "JSON", col.db_type.as_str()].contains(&w),
                            "{:?} -> {}: word {:?}", d, sql, w
                        );
                    }
                }
                let q = quoted[0];
                let body = &sql[q.start..q.end];
                prop_assert!(body.starts_with('\'') && body.ends_with('\'') && body.len() >= 2);
                prop_assert_eq!(decode(body, d), s.clone(), "{:?} -> {}", d, sql);
            }
        }
    }

    #[test]
    fn binary_literal_is_hex_only(b in prop::collection::vec(any::<u8>(), 0..64)) {
        let col = ColumnTarget {
            name: "c".into(),
            db_type: "bytea".into(),
            logical: LogicalType::Binary,
            collation: None,
        };
        for d in dialects() {
            let sql = literal(&FullValue::bytes(b.clone()), &col, d).unwrap();
            let toks = tokenize(&sql, d);
            prop_assert_eq!(toks.iter().filter(|t| t.kind == TokKind::Quoted).count(), 1);
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            prop_assert!(sql.contains(&hex), "{}", sql);
        }
    }
}
