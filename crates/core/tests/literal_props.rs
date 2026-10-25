//! Property test: whatever text goes into `literal`, the server's lexer (modelled by
//! `core::sql::tokenize`) sees exactly one string literal, nothing that ends the
//! statement, and the literal's body decodes back to the original text.

use dnt_core::sql::build::{dump_literal, literal, BuildError, ColumnTarget};
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
    ("\"char\"", true),
    ("public.\"int\"", true),
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

/// An array item as a tiny PG `array_in` reads it back.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    Null,
    Text(String),
    Array(Vec<Item>),
}

impl Item {
    fn to_value(&self) -> Value {
        match self {
            Item::Null => Value::Null,
            Item::Text(s) => Value::Text(s.clone()),
            Item::Array(items) => Value::Array(items.iter().map(Item::to_value).collect()),
        }
    }
}

fn items() -> impl Strategy<Value = Vec<Item>> {
    let leaf = prop_oneof![
        1 => Just(Item::Null),
        6 => text().prop_map(Item::Text),
    ];
    let item = leaf.prop_recursive(2, 24, 6, |inner| {
        prop::collection::vec(inner, 0..6).prop_map(Item::Array)
    });
    prop::collection::vec(item, 0..8)
}

/// PG `array_in` for text items: `{…}` of items separated by `,`; an item is `"…"` (with
/// `\` escaping the next character), a nested `{…}`, or bare text up to `,` / `}` where
/// `NULL` (any case) is a null.
fn parse_array(s: &str) -> Vec<Item> {
    fn array(chars: &mut std::iter::Peekable<std::str::Chars>) -> Vec<Item> {
        assert_eq!(chars.next(), Some('{'));
        let mut out = Vec::new();
        if chars.peek() == Some(&'}') {
            chars.next();
            return out;
        }
        loop {
            match chars.peek() {
                Some('{') => out.push(Item::Array(array(chars))),
                Some('"') => {
                    chars.next();
                    let mut t = String::new();
                    loop {
                        match chars.next().expect("unterminated quoted item") {
                            '\\' => t.push(chars.next().expect("dangling backslash")),
                            '"' => break,
                            c => t.push(c),
                        }
                    }
                    out.push(Item::Text(t));
                }
                _ => {
                    let mut t = String::new();
                    while let Some(&c) = chars.peek() {
                        if c == ',' || c == '}' {
                            break;
                        }
                        assert!(c != '"' && c != '\\' && c != '{', "unquoted special {c:?}");
                        assert!(!c.is_whitespace(), "unquoted whitespace");
                        t.push(c);
                        chars.next();
                    }
                    assert!(!t.is_empty(), "empty bare item");
                    out.push(if t.eq_ignore_ascii_case("null") {
                        Item::Null
                    } else {
                        Item::Text(t)
                    });
                }
            }
            match chars.next() {
                Some(',') => {}
                Some('}') => return out,
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    let mut chars = s.chars().peekable();
    let out = array(&mut chars);
    assert_eq!(chars.next(), None, "text after the array");
    out
}

fn has_nul(items: &[Item]) -> bool {
    items.iter().any(|i| match i {
        Item::Null => false,
        Item::Text(s) => s.contains('\0'),
        Item::Array(inner) => has_nul(inner),
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// A PG array dump literal is one string literal cast to the column's type, and its
    /// array text reads back as the items.
    #[test]
    fn dump_literal_pg_array_round_trips(items in items()) {
        let col = ColumnTarget {
            name: "c".into(),
            db_type: "text[]".into(),
            logical: LogicalType::Array,
            collation: None,
        };
        let v = FullValue::try_from_value(Value::Array(
            items.iter().map(Item::to_value).collect(),
        ))
        .unwrap();
        let d = Dialect::Postgres;
        let sql = match dump_literal(&v, &col, d) {
            Ok(sql) => sql,
            Err(BuildError::Invalid { .. }) => {
                prop_assert!(has_nul(&items), "{:?}", items);
                return Ok(());
            }
            Err(e) => return Err(TestCaseError::fail(format!("{items:?}: {e}"))),
        };
        prop_assert!(!has_nul(&items));
        let toks = tokenize(&sql, d);
        let kinds: Vec<_> = toks.iter().map(|t| t.kind).collect();
        prop_assert_eq!(
            kinds,
            vec![TokKind::Quoted, TokKind::Punct(b':'), TokKind::Punct(b':'), TokKind::Word("text"), TokKind::Punct(b'['), TokKind::Punct(b']')],
            "{}", sql
        );
        let body = &sql[toks[0].start..toks[0].end];
        prop_assert!(body.starts_with('\''));
        prop_assert_eq!(parse_array(&decode(body, d)), items, "{}", sql);
    }

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
