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

/// Column kinds whose literal is a quoted string: plain text, a PG cast, MySQL
/// `CAST(… AS JSON)`.
const KINDS: &[(LogicalType, MakeValue)] = &[
    (LogicalType::Text, Value::Text),
    (LogicalType::Json, Value::Json),
    (LogicalType::Uuid, Value::Uuid),
    (LogicalType::Date, Value::Date),
    (LogicalType::Enum, Value::Text),
];

/// `db_type`s as a catalog may report them (raw typnames can hold anything), and whether
/// PG accepts them as a cast target.
const DB_TYPES: &[(&str, bool)] = &[
    ("jsonb", true),
    ("uuid", true),
    ("date", true),
    ("order_status", true),
    ("public.order_status", true),
    ("OrderStatus", true),
    ("\"My Schema\".\"Weird\"\"T\"", true),
    ("\"a'b;--\"", true),
    ("đơn_hàng", true),
    ("int4[]", true),
    ("numeric(40,10)", true),
    ("character varying(10)", true),
    ("timestamp(3) with time zone", true),
    ("bool or true", false),
    ("text is not null", false),
    ("numeric(1) or true", false),
    ("int); DROP TABLE x; --", false),
    ("uuid; DROP TABLE t", false),
    ("x'y", false),
    ("a\"b", false),
    ("a--b", false),
    ("int4 /* c */", false),
    ("$$x$$", false),
    ("e'x'", false),
    ("\"unterminated", false),
    ("timestamp with time zone or true", false),
];

/// Words a literal may contain outside its string: the prefix / cast keywords and the
/// lower-case parts of the accepted type names above. Never the `db_type` itself.
const WORDS: &[&str] = &[
    "_utf8mb4",
    "CAST",
    "AS",
    "JSON",
    "jsonb",
    "uuid",
    "date",
    "order_status",
    "public",
    "int4",
    "numeric",
    "character",
    "varying",
    "timestamp",
    "with",
    "time",
    "zone",
];

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
    fn literal_never_escapes_its_quotes(
        s in text(),
        (db_type, cast_ok) in prop::sample::select(DB_TYPES),
    ) {
        for d in dialects() {
            for &(logical, make) in KINDS {
                let col = ColumnTarget {
                    name: "c".into(),
                    db_type: db_type.into(),
                    logical,
                    collation: None,
                };
                // Only PG non-text columns use the db_type (as a cast target).
                let casts = d == Dialect::Postgres && logical != LogicalType::Text;
                let v = FullValue::try_from_value(make(s.clone())).unwrap();
                let backslash = matches!(d, Dialect::MySql(m) if !m.no_backslash_escapes);
                let sql = match literal(&v, &col, d) {
                    Ok(sql) => sql,
                    Err(BuildError::Invalid { .. }) => {
                        // Only a NUL that the dialect cannot write is refused.
                        prop_assert!(s.contains('\0') && !backslash, "{d:?} {s:?}");
                        continue;
                    }
                    Err(BuildError::Unsupported { .. }) => {
                        prop_assert!(casts && !cast_ok, "{d:?} {db_type:?} refused");
                        continue;
                    }
                    Err(e) => return Err(TestCaseError::fail(format!("{d:?} {s:?}: {e}"))),
                };
                prop_assert!(!casts || cast_ok, "{:?} {:?} accepted: {}", d, db_type, sql);
                let toks = tokenize(&sql, d);
                // Exactly one string literal; any other quoted token is a PG
                // double-quoted identifier of the cast target.
                let strings: Vec<_> = toks
                    .iter()
                    .filter(|t| t.kind == TokKind::Quoted && sql[t.start..].starts_with('\''))
                    .collect();
                prop_assert_eq!(strings.len(), 1, "{:?} {:?} -> {}", d, s, sql);
                for t in &toks {
                    match t.kind {
                        TokKind::Quoted => prop_assert!(
                            sql[t.start..].starts_with('\'')
                                || (casts && sql[t.start..].starts_with('"')),
                            "{:?} -> {}", d, sql
                        ),
                        TokKind::Word(w) => prop_assert!(
                            WORDS.contains(&w) || w.bytes().all(|b| b.is_ascii_digit()),
                            "{:?} -> {}: word {:?}", d, sql, w
                        ),
                        TokKind::Punct(p) => prop_assert!(
                            b":.,[]".contains(&p),
                            "{:?} -> {}: punct {:?}", d, sql, p as char
                        ),
                        TokKind::Open | TokKind::Close => {}
                        TokKind::ExecComment => {
                            return Err(TestCaseError::fail(format!("{d:?} -> {sql}")))
                        }
                    }
                }
                let q = strings[0];
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
