//! The generator a column starts with (spec §4.3): from its kind (identity, generated, FK,
//! enum / CHECK list, bool, uuid), then its name, then its type.

use super::generator::{format_date, ColumnPlan, FakeKind, Generator};
use super::limits::{decimal_spec, decimal_whole_max, int_bounds, is_bit, max_bytes};
use crate::convert::TargetColumn;
use dnt_core::meta::{ForeignKeyDetail, TableDetail};
use dnt_core::server::DbKind;
use dnt_core::value::LogicalType;
use time::Date;

/// The words of a column name: snake_case, kebab-case and camelCase, lowercased.
fn tokens(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The foreign key `column` belongs to.
pub fn fk_of<'a>(detail: &'a TableDetail, column: &str) -> Option<&'a ForeignKeyDetail> {
    detail
        .foreign_keys
        .iter()
        .find(|f| f.columns.iter().any(|c| c == column))
}

/// `column` alone is the primary key or a (non-partial) unique index.
pub fn unique_single(detail: &TableDetail, column: &str) -> bool {
    detail
        .primary_key
        .iter()
        .chain(&detail.indexes)
        .any(|ix| (ix.unique || ix.primary) && !ix.partial && ix.columns == [column])
}

/// Replaces every quoted literal of `expr` with `$` (collecting the literals); `None` when a
/// literal is not closed.
fn literals(expr: &str) -> Option<(String, Vec<String>)> {
    let mut skeleton = String::new();
    let mut values = Vec::new();
    let mut chars = expr.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            skeleton.push(c);
            continue;
        }
        let mut lit = String::new();
        loop {
            match chars.next()? {
                '\'' if chars.peek() == Some(&'\'') => {
                    chars.next();
                    lit.push('\'');
                }
                '\'' => break,
                ch => lit.push(ch),
            }
        }
        values.push(lit);
        skeleton.push('$');
    }
    Some((skeleton, values))
}

/// The skeleton without casts (`::type[]`), charset introducers (`_utf8mb4$`), quoting,
/// parentheses, brackets, commas and spaces.
fn bare(skeleton: &str) -> String {
    let s = skeleton.to_lowercase();
    let mut out = String::new();
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == ':' && b.get(i + 1) == Some(&':') {
            i += 2;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == ' ' || b[i] == '_') {
                i += 1;
            }
            if b.get(i) == Some(&'[') && b.get(i + 1) == Some(&']') {
                i += 2;
            }
            continue;
        }
        if b[i] == '_' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_alphanumeric() {
                j += 1;
            }
            if j > i + 1 && b.get(j) == Some(&'$') {
                i = j;
                continue;
            }
        }
        if !matches!(
            b[i],
            ' ' | '(' | ')' | '`' | '"' | ',' | '[' | ']' | '\n' | '\t'
        ) {
            out.push(b[i]);
        }
        i += 1;
    }
    out.strip_prefix("check").map(str::to_string).unwrap_or(out)
}

/// The values of a CHECK constraint that limits `column` to a list: `col IN ('a','b')`
/// (MySQL adds charset introducers) or PostgreSQL's `(col)::text = ANY ((ARRAY['a'::…])…)`.
/// Best effort: anything else (other columns, `AND`/`OR`, other operators) gives none.
pub fn check_values(detail: &TableDetail, column: &str) -> Vec<String> {
    let col = column.to_lowercase();
    for check in &detail.checks {
        let Some((skeleton, values)) = literals(&check.expression) else {
            continue;
        };
        if values.is_empty() {
            continue;
        }
        let dollars = "$".repeat(values.len());
        let b = bare(&skeleton);
        if b == format!("{col}in{dollars}") || b == format!("{col}=anyarray{dollars}") {
            return values;
        }
    }
    Vec::new()
}

/// The same day `years` earlier (29 February becomes the 28th).
fn years_before(day: Date, years: i32) -> Date {
    let y = day.year() - years;
    Date::from_calendar_date(y, day.month(), day.day())
        .or_else(|_| Date::from_calendar_date(y, day.month(), 28))
        .unwrap_or(day)
}

/// Dates or date-times of the two years up to `today`, by the column's type.
fn recent(logical: LogicalType, today: Date) -> Generator {
    let from = format_date(years_before(today, 2));
    let to = format_date(today);
    match logical {
        LogicalType::DateTime => Generator::DateTimeRange {
            from: format!("{from} 00:00:00"),
            to: format!("{to} 23:59:59"),
        },
        _ => Generator::DateRange { from, to },
    }
}

fn fake(kind: FakeKind) -> Option<Generator> {
    Some(Generator::Fake { fake: kind })
}

/// The decimal places a decimal/float column gets: its scale, at most `max`.
fn scale_of(col: &TargetColumn, max: u8) -> u8 {
    match decimal_spec(&col.db_type) {
        Some((_, s)) => (s as u8).min(max),
        None => max,
    }
}

/// Rule 5 (spec §4.3): the generator a column's name suggests, when its type fits.
fn by_name(col: &TargetColumn, t: &[String]) -> Option<Generator> {
    use LogicalType as L;
    let has = |w: &[&str]| t.iter().any(|x| w.contains(&x.as_str()));
    let joined: String = t.concat();
    let text = col.logical == L::Text;
    let numeric = matches!(col.logical, L::Decimal | L::Float);
    let int = col.logical == L::Integer;
    // A small `decimal(p,s)` caps the range below `10^(p−s)`.
    let whole = decimal_whole_max(&col.db_type).map_or(f64::INFINITY, |m| m as f64);
    let decimal = |min: f64, max: f64, scale: u8| {
        Some(Generator::DecimalRange {
            min: min.max(-whole),
            max: max.min(whole),
            scale: scale_of(col, scale),
        })
    };
    if text {
        if has(&["email", "mail"]) {
            return fake(FakeKind::Email);
        }
        if joined.contains("firstname") || joined.contains("givenname") || has(&["fname"]) {
            return fake(FakeKind::FirstName);
        }
        if joined.contains("lastname")
            || joined.contains("familyname")
            || has(&["surname", "lname"])
        {
            return fake(FakeKind::LastName);
        }
        if joined.contains("username") || has(&["login", "handle"]) {
            return fake(FakeKind::Username);
        }
        if has(&["company", "organization", "organisation", "employer"]) {
            return fake(FakeKind::Company);
        }
        if joined.contains("jobtitle") || has(&["position"]) {
            return fake(FakeKind::JobTitle);
        }
        if has(&["phone", "mobile", "tel", "telephone", "fax"]) {
            return fake(FakeKind::Phone);
        }
        if has(&["ip"]) {
            return fake(FakeKind::Ipv4);
        }
        if has(&["address", "street", "address1", "line1"]) {
            return fake(FakeKind::StreetAddress);
        }
        if has(&["city", "town"]) {
            return fake(FakeKind::City);
        }
        if has(&["state", "province", "region"]) {
            return fake(FakeKind::State);
        }
        if has(&["zip", "zipcode", "postal", "postcode"]) {
            return fake(FakeKind::ZipCode);
        }
        if has(&["country"]) {
            return fake(FakeKind::Country);
        }
        if has(&["url", "website", "homepage", "link"]) {
            return fake(FakeKind::Url);
        }
        if has(&["domain", "host", "hostname"]) {
            return fake(FakeKind::Domain);
        }
        if has(&["slug"]) {
            return fake(FakeKind::Slug);
        }
        if has(&["color", "colour"]) {
            return fake(FakeKind::HexColor);
        }
        if has(&["password", "passwd", "hash"]) {
            return fake(FakeKind::PasswordHash);
        }
        if has(&["name", "fullname", "displayname"]) {
            return fake(FakeKind::FullName);
        }
        if has(&["title", "subject", "headline"]) {
            return fake(FakeKind::Sentence);
        }
        if has(&[
            "description",
            "bio",
            "notes",
            "note",
            "body",
            "content",
            "summary",
            "about",
            "comment",
        ]) {
            return match col.max_chars {
                Some(n) if n < 200 => fake(FakeKind::Sentence),
                _ => fake(FakeKind::Paragraph),
            };
        }
        if has(&["code", "sku"]) && col.max_chars.is_none_or(|n| n >= 9) {
            return Some(Generator::Pattern {
                pattern: "???-#####".into(),
            });
        }
    }
    if numeric && has(&["price", "amount", "total", "cost", "salary", "balance"]) {
        return decimal(1.0, 1000.0, 2);
    }
    if numeric && has(&["lat", "latitude"]) {
        return decimal(-90.0, 90.0, 6);
    }
    if numeric && has(&["lng", "lon", "longitude"]) {
        return decimal(-180.0, 180.0, 6);
    }
    if int && has(&["qty", "quantity", "stock", "count"]) {
        return Some(Generator::IntRange { min: 0, max: 100 });
    }
    if int && has(&["age"]) {
        return Some(Generator::IntRange { min: 18, max: 80 });
    }
    if int && has(&["rating", "score", "stars"]) {
        return Some(Generator::IntRange { min: 1, max: 5 });
    }
    if has(&["birth", "birthday", "birthdate", "dob"]) || joined.contains("dateofbirth") {
        return match col.logical {
            L::Date => Some(Generator::DateRange {
                from: "1950-01-01".into(),
                to: "2005-12-31".into(),
            }),
            L::DateTime => Some(Generator::DateTimeRange {
                from: "1950-01-01 00:00:00".into(),
                to: "2005-12-31 23:59:59".into(),
            }),
            _ => None,
        };
    }
    None
}

/// Rule 6 (spec §4.3): the generator of a column's type.
fn by_type(col: &TargetColumn, today: Date) -> Option<Generator> {
    use LogicalType as L;
    Some(match col.logical {
        L::Integer => {
            // `YEAR` starts at 1901: the range starts at the type's floor when it is above 1.
            let (lo, hi) = int_bounds(col, DbKind::MySql);
            let min = lo.max(1);
            Generator::IntRange {
                min: min as i64,
                max: hi.min(min + 999) as i64,
            }
        }
        L::Decimal | L::Float => {
            let max = match decimal_spec(&col.db_type) {
                Some((p, s)) => (10f64.powi(p.saturating_sub(s) as i32) - 1.0).min(1000.0),
                None => 1000.0,
            };
            Generator::DecimalRange {
                min: 0.0,
                max,
                scale: scale_of(col, 2),
            }
        }
        L::Text => match col.max_chars {
            Some(n) if n <= 10 => Generator::Pattern {
                pattern: "?".repeat(n as usize),
            },
            Some(n) if n < 30 => Generator::Fake {
                fake: FakeKind::Word,
            },
            _ => Generator::Fake {
                fake: FakeKind::Sentence,
            },
        },
        L::Date | L::DateTime => recent(col.logical, today),
        L::Time => Generator::TimeRange {
            from: "00:00:00".into(),
            to: "23:59:59".into(),
        },
        L::Json => Generator::JsonObject,
        L::Binary if is_bit(&col.db_type) => Generator::Pick {
            values: vec!["00".into(), "01".into()],
        },
        L::Binary => Generator::RandomBytes {
            len: max_bytes(&col.db_type).map_or(16, |n| n.clamp(1, 16)),
        },
        L::Uuid => Generator::Uuid,
        L::Bool => Generator::Bool { true_percent: 50 },
        L::Enum | L::Array | L::Other => return None,
    })
}

/// The plan a column starts with (spec §4.3); `today` anchors "the last two years".
pub fn infer_plan(col: &TargetColumn, detail: &TableDetail, today: Date) -> ColumnPlan {
    let t = tokens(&col.name);
    let mut null_percent = 0;
    let serial = detail
        .columns
        .iter()
        .find(|c| c.name == col.name)
        .and_then(|c| c.default.as_deref())
        .is_some_and(|d| d.trim_start().to_ascii_lowercase().starts_with("nextval("));
    let generator = if col.auto_increment || col.identity.is_some() || col.generated || serial {
        Generator::Skip
    } else if let Some(fk) = fk_of(detail, &col.name) {
        Generator::ForeignKey {
            constraint: fk.name.clone(),
        }
    } else if !col.enum_values.is_empty() {
        Generator::Pick {
            values: col.enum_values.clone(),
        }
    } else if let values @ [_, ..] = check_values(detail, &col.name).as_slice() {
        Generator::Pick {
            values: values.to_vec(),
        }
    } else if col.logical == LogicalType::Bool
        || (col.logical == LogicalType::Integer
            && col.db_type.to_ascii_lowercase().starts_with("tinyint(1)")
            && t.first().is_some_and(|w| w == "is" || w == "has"))
    {
        Generator::Bool { true_percent: 50 }
    } else if col.logical == LogicalType::Uuid {
        Generator::Uuid
    } else if let Some(g) = by_name(col, &t).or_else(|| by_type(col, today)) {
        g
    } else if col.nullable {
        null_percent = 100;
        Generator::Constant {
            value: String::new(),
        }
    } else if col.has_default {
        Generator::Skip
    } else {
        Generator::Constant {
            value: String::new(),
        }
    };
    ColumnPlan {
        name: col.name.clone(),
        generator,
        null_percent,
        unique: unique_single(detail, &col.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed::{FakeKind, Generator};
    use dnt_core::meta::{
        CheckDetail, ColumnDetail, ForeignKeyDetail, IndexDetail, ObjectKind, ObjectRef,
        TableDetail, TableOptions,
    };
    use dnt_core::value::LogicalType;

    fn today() -> time::Date {
        time::Date::from_calendar_date(2026, time::Month::October, 30).unwrap()
    }

    fn col(name: &str, db_type: &str, logical: LogicalType, nullable: bool) -> TargetColumn {
        TargetColumn {
            name: name.into(),
            db_type: db_type.into(),
            logical,
            nullable,
            has_default: false,
            auto_increment: false,
            identity: None,
            generated: false,
            enum_values: vec![],
            max_chars: ["varchar(", "character varying("]
                .iter()
                .find_map(|p| db_type.strip_prefix(p))
                .and_then(|r| r.strip_suffix(')'))
                .and_then(|n| n.parse().ok()),
            collation: None,
            unsigned: db_type.contains("unsigned"),
        }
    }

    fn detail(columns: &[&str]) -> TableDetail {
        TableDetail {
            object: ObjectRef {
                database: "shop".into(),
                schema: None,
                name: "t".into(),
                kind: ObjectKind::Table,
            },
            columns: columns
                .iter()
                .enumerate()
                .map(|(i, c)| ColumnDetail {
                    name: c.to_string(),
                    ordinal: i as u32 + 1,
                    ..ColumnDetail::default()
                })
                .collect(),
            primary_key: None,
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
            triggers: vec![],
            options: TableOptions::default(),
            row_estimate: None,
            size_bytes: None,
            comment: None,
        }
    }

    fn infer(name: &str, db_type: &str, logical: LogicalType) -> Generator {
        infer_plan(
            &col(name, db_type, logical, false),
            &detail(&[name]),
            today(),
        )
        .generator
    }

    fn fake(kind: FakeKind) -> Generator {
        Generator::Fake { fake: kind }
    }

    #[test]
    fn infers_by_name_and_type() {
        use LogicalType as L;
        let cases: Vec<(&str, &str, LogicalType, Generator)> = vec![
            ("email", "varchar(120)", L::Text, fake(FakeKind::Email)),
            (
                "contactEmail",
                "varchar(120)",
                L::Text,
                fake(FakeKind::Email),
            ),
            (
                "first_name",
                "varchar(50)",
                L::Text,
                fake(FakeKind::FirstName),
            ),
            ("lastName", "varchar(50)", L::Text, fake(FakeKind::LastName)),
            ("surname", "varchar(50)", L::Text, fake(FakeKind::LastName)),
            ("name", "varchar(80)", L::Text, fake(FakeKind::FullName)),
            (
                "customerName",
                "varchar(80)",
                L::Text,
                fake(FakeKind::FullName),
            ),
            ("username", "varchar(40)", L::Text, fake(FakeKind::Username)),
            ("phone", "varchar(20)", L::Text, fake(FakeKind::Phone)),
            (
                "address_line1",
                "varchar(120)",
                L::Text,
                fake(FakeKind::StreetAddress),
            ),
            ("city", "varchar(60)", L::Text, fake(FakeKind::City)),
            ("province", "varchar(60)", L::Text, fake(FakeKind::State)),
            (
                "postal_code",
                "varchar(10)",
                L::Text,
                fake(FakeKind::ZipCode),
            ),
            ("country", "varchar(60)", L::Text, fake(FakeKind::Country)),
            ("company", "varchar(80)", L::Text, fake(FakeKind::Company)),
            (
                "job_title",
                "varchar(80)",
                L::Text,
                fake(FakeKind::JobTitle),
            ),
            ("website", "varchar(200)", L::Text, fake(FakeKind::Url)),
            ("domain", "varchar(100)", L::Text, fake(FakeKind::Domain)),
            ("ip_address", "varchar(45)", L::Text, fake(FakeKind::Ipv4)),
            ("slug", "varchar(100)", L::Text, fake(FakeKind::Slug)),
            ("color", "varchar(7)", L::Text, fake(FakeKind::HexColor)),
            (
                "password_hash",
                "varchar(60)",
                L::Text,
                fake(FakeKind::PasswordHash),
            ),
            ("title", "varchar(255)", L::Text, fake(FakeKind::Sentence)),
            ("notes", "text", L::Text, fake(FakeKind::Paragraph)),
            ("notes", "varchar(100)", L::Text, fake(FakeKind::Sentence)),
            (
                "price",
                "decimal(10,2)",
                L::Decimal,
                Generator::DecimalRange {
                    min: 1.0,
                    max: 1000.0,
                    scale: 2,
                },
            ),
            (
                "qty",
                "int",
                L::Integer,
                Generator::IntRange { min: 0, max: 100 },
            ),
            (
                "age",
                "int",
                L::Integer,
                Generator::IntRange { min: 18, max: 80 },
            ),
            (
                "rating",
                "smallint",
                L::Integer,
                Generator::IntRange { min: 1, max: 5 },
            ),
            (
                "lat",
                "decimal(9,6)",
                L::Decimal,
                Generator::DecimalRange {
                    min: -90.0,
                    max: 90.0,
                    scale: 6,
                },
            ),
            (
                "longitude",
                "double",
                L::Float,
                Generator::DecimalRange {
                    min: -180.0,
                    max: 180.0,
                    scale: 6,
                },
            ),
            (
                "code",
                "varchar(20)",
                L::Text,
                Generator::Pattern {
                    pattern: "???-#####".into(),
                },
            ),
            (
                "dob",
                "date",
                L::Date,
                Generator::DateRange {
                    from: "1950-01-01".into(),
                    to: "2005-12-31".into(),
                },
            ),
            (
                "created_at",
                "timestamp",
                L::DateTime,
                Generator::DateTimeRange {
                    from: "2024-10-30 00:00:00".into(),
                    to: "2026-10-30 23:59:59".into(),
                },
            ),
            (
                "ship_date",
                "date",
                L::Date,
                Generator::DateRange {
                    from: "2024-10-30".into(),
                    to: "2026-10-30".into(),
                },
            ),
            (
                "is_active",
                "tinyint(1)",
                L::Integer,
                Generator::Bool { true_percent: 50 },
            ),
            (
                "enabled",
                "boolean",
                L::Bool,
                Generator::Bool { true_percent: 50 },
            ),
            ("x", "varchar(255)", L::Text, fake(FakeKind::Sentence)),
            ("x", "varchar(20)", L::Text, fake(FakeKind::Word)),
            ("x", "varchar(40)", L::Text, fake(FakeKind::Sentence)),
            (
                "x",
                "varchar(8)",
                L::Text,
                Generator::Pattern {
                    pattern: "????????".into(),
                },
            ),
            (
                "x",
                "int",
                L::Integer,
                Generator::IntRange { min: 1, max: 1000 },
            ),
            (
                "x",
                "tinyint",
                L::Integer,
                Generator::IntRange { min: 1, max: 127 },
            ),
            (
                "x",
                "decimal(5,2)",
                L::Decimal,
                Generator::DecimalRange {
                    min: 0.0,
                    max: 999.0,
                    scale: 2,
                },
            ),
            (
                "x",
                "time",
                L::Time,
                Generator::TimeRange {
                    from: "00:00:00".into(),
                    to: "23:59:59".into(),
                },
            ),
            ("x", "jsonb", L::Json, Generator::JsonObject),
            ("x", "bytea", L::Binary, Generator::RandomBytes { len: 16 }),
            ("x", "uuid", L::Uuid, Generator::Uuid),
            (
                "y",
                "year",
                L::Integer,
                Generator::IntRange {
                    min: 1901,
                    max: 2155,
                },
            ),
            (
                "flag",
                "bit(1)",
                L::Binary,
                Generator::Pick {
                    values: vec!["00".into(), "01".into()],
                },
            ),
            (
                "b",
                "binary(8)",
                L::Binary,
                Generator::RandomBytes { len: 8 },
            ),
            (
                "price",
                "decimal(5,2)",
                L::Decimal,
                Generator::DecimalRange {
                    min: 1.0,
                    max: 999.0,
                    scale: 2,
                },
            ),
        ];
        for (name, db_type, logical, want) in cases {
            assert_eq!(infer(name, db_type, logical), want, "{name} {db_type}");
        }
    }

    #[test]
    fn text_fallback_short_and_long() {
        // `varchar(30)` and longer: a sentence; shorter (> 10): a word.
        assert_eq!(
            infer("x", "varchar(30)", LogicalType::Text),
            fake(FakeKind::Sentence)
        );
        assert_eq!(
            infer("x", "varchar(20)", LogicalType::Text),
            fake(FakeKind::Word)
        );
        assert_eq!(
            infer("x", "text", LogicalType::Text),
            fake(FakeKind::Sentence)
        );
    }

    #[test]
    fn unknown_types() {
        let d = detail(&["x"]);
        let nullable = infer_plan(&col("x", "int[]", LogicalType::Array, true), &d, today());
        assert_eq!(
            nullable.generator,
            Generator::Constant {
                value: String::new()
            }
        );
        assert_eq!(nullable.null_percent, 100);
        let mut with_default = col("x", "geometry", LogicalType::Other, false);
        with_default.has_default = true;
        assert_eq!(
            infer_plan(&with_default, &d, today()).generator,
            Generator::Skip
        );
        let plain = infer_plan(
            &col("x", "geometry", LogicalType::Other, false),
            &d,
            today(),
        );
        assert_eq!(
            plain.generator,
            Generator::Constant {
                value: String::new()
            }
        );
        assert_eq!(plain.null_percent, 0);
    }

    #[test]
    fn precedence() {
        let mut ai = col("id", "int", LogicalType::Integer, false);
        ai.auto_increment = true;
        ai.has_default = true;
        let d = detail(&["id"]);
        assert_eq!(infer_plan(&ai, &d, today()).generator, Generator::Skip);
        let mut ident = col("id", "integer", LogicalType::Integer, false);
        ident.identity = Some(dnt_core::meta::IdentityKind::Always);
        assert_eq!(infer_plan(&ident, &d, today()).generator, Generator::Skip);
        let mut gen = col("total", "int", LogicalType::Integer, false);
        gen.generated = true;
        assert_eq!(infer_plan(&gen, &d, today()).generator, Generator::Skip);

        let mut fk = detail(&["email"]);
        fk.foreign_keys.push(ForeignKeyDetail {
            name: "fk_email".into(),
            columns: vec!["email".into()],
            ref_table: "people".into(),
            ref_columns: vec!["email".into()],
            ..ForeignKeyDetail::default()
        });
        let email = col("email", "varchar(100)", LogicalType::Text, false);
        assert_eq!(
            infer_plan(&email, &fk, today()).generator,
            Generator::ForeignKey {
                constraint: "fk_email".into()
            }
        );

        let mut e = col("name", "enum('a','b')", LogicalType::Enum, false);
        e.enum_values = vec!["a".into(), "b".into()];
        assert_eq!(
            infer_plan(&e, &detail(&["name"]), today()).generator,
            Generator::Pick {
                values: vec!["a".into(), "b".into()]
            }
        );

        let mut checked = detail(&["status"]);
        checked.checks.push(CheckDetail {
            name: "ck".into(),
            expression: "(`status` in (_utf8mb4'new',_utf8mb4'paid'))".into(),
            definition: None,
        });
        let status = col("status", "varchar(10)", LogicalType::Text, false);
        assert_eq!(
            infer_plan(&status, &checked, today()).generator,
            Generator::Pick {
                values: vec!["new".into(), "paid".into()]
            }
        );
    }

    fn index(name: &str, cols: &[&str], primary: bool, partial: bool) -> IndexDetail {
        IndexDetail {
            name: name.into(),
            unique: true,
            primary,
            partial,
            columns: cols.iter().map(|c| c.to_string()).collect(),
            ..IndexDetail::default()
        }
    }

    #[test]
    fn unique_default() {
        let mut d = detail(&["id", "email", "a", "b", "p"]);
        d.primary_key = Some(index("PRIMARY", &["id"], true, false));
        d.indexes = vec![
            index("uq_email", &["email"], false, false),
            index("uq_ab", &["a", "b"], false, false),
            index("uq_p", &["p"], false, true),
        ];
        assert!(unique_single(&d, "id"));
        assert!(unique_single(&d, "email"));
        assert!(!unique_single(&d, "a"));
        assert!(!unique_single(&d, "p"));
        let email = col("email", "varchar(100)", LogicalType::Text, false);
        assert!(infer_plan(&email, &d, today()).unique);
        let a = col("a", "int", LogicalType::Integer, false);
        assert!(!infer_plan(&a, &d, today()).unique);
    }

    fn check(expr: &str) -> TableDetail {
        let mut d = detail(&["status", "total"]);
        d.checks.push(CheckDetail {
            name: "ck".into(),
            expression: expr.into(),
            definition: None,
        });
        d
    }

    #[test]
    fn parses_check_in_lists() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            check_values(
                &check("(`status` in (_utf8mb4'new',_utf8mb4'paid'))"),
                "status"
            ),
            s(&["new", "paid"])
        );
        assert_eq!(
            check_values(
                &check(
                    "((status)::text = ANY ((ARRAY['new'::character varying, 'paid'::character varying])::text[]))"
                ),
                "status"
            ),
            s(&["new", "paid"])
        );
        assert_eq!(
            check_values(&check("CHECK (status IN ('a','b'))"), "status"),
            s(&["a", "b"])
        );
        assert_eq!(
            check_values(&check("status IN ('it''s', 'x')"), "status"),
            s(&["it's", "x"])
        );
        assert!(check_values(&check("total >= 0"), "total").is_empty());
        assert!(check_values(&check("status IN ('a') AND total > 0"), "status").is_empty());
        assert!(check_values(&check("status IN ('a','b')"), "total").is_empty());
    }

    #[test]
    fn fk_of_finds_the_constraint() {
        let mut d = detail(&["customer_id"]);
        d.foreign_keys.push(ForeignKeyDetail {
            name: "fk_c".into(),
            columns: vec!["customer_id".into()],
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            ..ForeignKeyDetail::default()
        });
        assert_eq!(
            fk_of(&d, "customer_id").map(|f| f.name.as_str()),
            Some("fk_c")
        );
        assert!(fk_of(&d, "other").is_none());
    }
}
