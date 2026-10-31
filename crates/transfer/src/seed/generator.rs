//! Generators (spec §4.1) and the value of one cell (§4.2). A generator makes a source cell
//! (text, number, bool); `convert` turns it into a value of the column exactly as the
//! import does, so type rules (lengths, enums, unsigned, dates) are enforced in one place.

use super::fake::{fake_text, pattern};
use super::limits::{
    decimal_spec, decimal_whole_max, int_bounds, is_bit, is_timestamp, max_bytes, JS_SAFE,
};
use super::rng::SplitMix64;
use crate::convert::{convert, BinaryEncoding, ConvertOptions, EmptyAs, TargetColumn};
use crate::read::Cell;
use dnt_core::server::SqlKind;
use dnt_core::value::{FullValue, LogicalType};
use serde::{Deserialize, Serialize};
use specta::Type;
use time::{Date, Month};

/// How a column's values are made. `i64` fields cross IPC as JS numbers: requests keep them
/// within ±2^53 (checked by [`check_generator`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Generator {
    /// Left out of the `INSERT`: the column's default, `AUTO_INCREMENT` or identity applies.
    Skip,
    Sequence {
        #[specta(type = f64)]
        start: i64,
        #[specta(type = f64)]
        step: i64,
    },
    IntRange {
        #[specta(type = f64)]
        min: i64,
        #[specta(type = f64)]
        max: i64,
    },
    DecimalRange {
        min: f64,
        max: f64,
        scale: u8,
    },
    Bool {
        true_percent: u8,
    },
    /// `YYYY-MM-DD`.
    DateRange {
        from: String,
        to: String,
    },
    /// `YYYY-MM-DD HH:MM:SS`.
    DateTimeRange {
        from: String,
        to: String,
    },
    /// `HH:MM:SS`.
    TimeRange {
        from: String,
        to: String,
    },
    Pick {
        values: Vec<String>,
    },
    /// A key of the referenced table (the name of a foreign key of the table).
    ForeignKey {
        constraint: String,
    },
    Fake {
        fake: FakeKind,
    },
    Uuid,
    Constant {
        value: String,
    },
    /// `#` digit, `?` letter A–Z, `*` letter or digit, `\x` a literal `x`.
    Pattern {
        pattern: String,
    },
    JsonObject,
    RandomBytes {
        len: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FakeKind {
    FirstName,
    LastName,
    FullName,
    Email,
    Username,
    Phone,
    StreetAddress,
    City,
    State,
    ZipCode,
    Country,
    Company,
    JobTitle,
    Url,
    Domain,
    /// Renamed explicitly: specta's snake_case would export `ipv_4` while serde reads and
    /// writes `ipv4`.
    #[serde(rename = "ipv4")]
    Ipv4,
    Slug,
    Word,
    Sentence,
    Paragraph,
    HexColor,
    PasswordHash,
}

/// One column's generator and options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ColumnPlan {
    pub name: String,
    pub generator: Generator,
    /// 0–100; only used on nullable columns.
    pub null_percent: u8,
    pub unique: bool,
}

/// How generated text becomes a value: an empty text stays empty in text columns.
fn convert_options() -> ConvertOptions {
    ConvertOptions {
        empty: EmptyAs::EmptyString,
        null_text: None,
        trim: false,
        binary: BinaryEncoding::Hex,
    }
}

/// The integers `col` and a request (JS numbers) both hold.
pub(crate) fn int_limits(col: &TargetColumn, kind: SqlKind) -> (i128, i128) {
    let whole = (col.logical == LogicalType::Decimal)
        .then(|| decimal_whole_max(&col.db_type))
        .flatten();
    let (lo, hi) = if col.logical == LogicalType::Integer {
        int_bounds(col, kind)
    } else if let Some(max) = whole {
        (if col.unsigned { 0 } else { -max }, max)
    } else {
        (i64::MIN as i128, i64::MAX as i128)
    };
    (lo.max(-JS_SAFE), hi.min(JS_SAFE))
}

pub(crate) fn parse_date(s: &str) -> Option<Date> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u32> {
        let part = &s[r];
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let month = Month::try_from(num(5..7)? as u8).ok()?;
    Date::from_calendar_date(num(0..4)? as i32, month, num(8..10)? as u8).ok()
}

/// Seconds since midnight of `HH:MM:SS`.
pub(crate) fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 8 || b[2] != b':' || b[5] != b':' {
        return None;
    }
    let part = |r: std::ops::Range<usize>| -> Option<i64> {
        let p = &s[r];
        p.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| p.parse().ok())
            .flatten()
    };
    let (h, m, sec) = (part(0..2)?, part(3..5)?, part(6..8)?);
    (h < 24 && m < 60 && sec < 60).then_some(h * 3600 + m * 60 + sec)
}

/// Seconds since the Julian epoch of `YYYY-MM-DD HH:MM:SS`.
pub(crate) fn parse_datetime(s: &str) -> Option<i64> {
    if s.len() != 19 || s.as_bytes()[10] != b' ' {
        return None;
    }
    let day = parse_date(&s[..10])?;
    Some(i64::from(day.to_julian_day()) * 86_400 + parse_time(&s[11..])?)
}

pub(crate) fn format_date(day: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        day.year(),
        u8::from(day.month()),
        day.day()
    )
}

fn format_time(secs: i64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

fn date_of_julian(j: i64) -> Date {
    Date::from_julian_day(j as i32).expect("a day between two valid days")
}

/// The problem with `gen`'s options for `col` (spec §4.2), if any. Checks that need the
/// row count or other columns live in the engine.
pub fn check_generator(gen: &Generator, col: &TargetColumn, kind: SqlKind) -> Option<String> {
    let range = |lo: i128, hi: i128| -> Option<String> {
        let (min, max) = int_limits(col, kind);
        (lo < min || hi > max).then(|| format!("Values must be between {min} and {max}."))
    };
    let order = |reversed: bool, what: &str| reversed.then(|| what.to_string());
    match gen {
        Generator::Sequence { start, step } => {
            if *step == 0 {
                return Some("The step cannot be 0.".into());
            }
            range(i128::from(*start), i128::from(*start))
        }
        Generator::IntRange { min, max } => order(min > max, "The minimum is above the maximum.")
            .or_else(|| range(i128::from(*min), i128::from(*max))),
        Generator::DecimalRange { min, max, scale } => {
            if !(min.is_finite() && max.is_finite()) {
                return Some("Enter numbers for the minimum and the maximum.".into());
            }
            if min > max {
                return Some("The minimum is above the maximum.".into());
            }
            if col.logical == LogicalType::Decimal {
                if let Some((p, s)) = decimal_spec(&col.db_type) {
                    if u32::from(*scale) > s {
                        return Some(format!("The scale must be at most {s}."));
                    }
                    let bound = 10f64.powi(p.saturating_sub(s) as i32);
                    if *max >= bound || *min <= -bound {
                        return Some(format!("Values must be below {bound}."));
                    }
                }
            }
            if *scale > 12 {
                return Some("The scale must be at most 12.".into());
            }
            (col.unsigned && *min < 0.0).then(|| "Values must be 0 or more.".to_string())
        }
        Generator::Bool { true_percent } => {
            (*true_percent > 100).then(|| "Enter a percentage from 0 to 100.".to_string())
        }
        Generator::DateRange { from, to } => match (parse_date(from), parse_date(to)) {
            (Some(a), Some(b)) => order(a > b, "The start is after the end."),
            _ => Some("Enter dates as YYYY-MM-DD.".into()),
        },
        Generator::DateTimeRange { from, to } => match (parse_datetime(from), parse_datetime(to)) {
            (Some(a), Some(b)) if a > b => Some("The start is after the end.".into()),
            (Some(a), Some(b)) => {
                let first = parse_datetime("1970-01-01 00:00:01").expect("a valid literal");
                let last = parse_datetime("2038-01-19 03:14:07").expect("a valid literal");
                let mysql_timestamp = match kind {
                    SqlKind::MySql => is_timestamp(&col.db_type),
                    SqlKind::Postgres => false,
                };
                (mysql_timestamp && (a < first || b > last)).then(|| {
                    "MySQL TIMESTAMP holds 1970-01-01 00:00:01 to 2038-01-19 03:14:07.".to_string()
                })
            }
            _ => Some("Enter dates and times as YYYY-MM-DD HH:MM:SS.".into()),
        },
        Generator::TimeRange { from, to } => match (parse_time(from), parse_time(to)) {
            (Some(a), Some(b)) => order(a > b, "The start is after the end."),
            _ => Some("Enter times as HH:MM:SS.".into()),
        },
        Generator::Pick { values } => values
            .is_empty()
            .then(|| "Enter at least one value.".to_string()),
        Generator::Pattern { pattern } => pattern.is_empty().then(|| "Enter a pattern.".into()),
        Generator::RandomBytes { len } => {
            if *len == 0 || *len > 65_535 {
                Some("Enter a length from 1 to 65535.".to_string())
            } else if is_bit(&col.db_type) {
                Some(
                    "Random bytes do not fit a BIT column; pick from hex values such as 00, 01."
                        .to_string(),
                )
            } else {
                max_bytes(&col.db_type)
                    .filter(|n| len > n)
                    .map(|n| format!("The column holds at most {n} bytes."))
            }
        }
        Generator::Skip
        | Generator::ForeignKey { .. }
        | Generator::Fake { .. }
        | Generator::Uuid
        | Generator::Constant { .. }
        | Generator::JsonObject => None,
    }
}

/// The smallest and largest value of a decimal range, in units of its scale.
fn decimal_units(min: f64, max: f64, scale: u8) -> (i128, i128) {
    let units = 10f64.powi(i32::from(scale));
    let lo = (min * units).ceil() as i128;
    (lo, ((max * units).floor() as i128).max(lo))
}

/// `min..=max` with `scale` decimals, as text.
fn decimal_text(rng: &mut SplitMix64, min: f64, max: f64, scale: u8) -> String {
    let (lo, hi) = decimal_units(min, max, scale);
    units_text(rng.range_i128(lo, hi), scale)
}

/// `v` units of `10^-scale` as text.
fn units_text(v: i128, scale: u8) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let abs = v.unsigned_abs();
    if scale == 0 {
        return format!("{sign}{abs}");
    }
    let units = 10u128.pow(u32::from(scale));
    format!(
        "{sign}{}.{:0width$}",
        abs / units,
        abs % units,
        width = usize::from(scale)
    )
}

fn uuid_text(rng: &mut SplitMix64) -> String {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&rng.next_u64().to_be_bytes());
    b[8..].copy_from_slice(&rng.next_u64().to_be_bytes());
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

/// The cell `gen` makes for `row` before conversion; `None` for the generators the engine
/// handles (`Skip`, `ForeignKey`).
fn raw(gen: &Generator, rng: &mut SplitMix64, row: u64) -> Option<Cell> {
    let text = Cell::Text;
    Some(match gen {
        Generator::Skip | Generator::ForeignKey { .. } => return None,
        Generator::Sequence { start, step } => {
            Cell::Number((i128::from(*start) + i128::from(*step) * i128::from(row)).to_string())
        }
        Generator::IntRange { min, max } => Cell::Number(
            rng.range_i128(i128::from(*min), i128::from(*max))
                .to_string(),
        ),
        Generator::DecimalRange { min, max, scale } => {
            Cell::Number(decimal_text(rng, *min, *max, *scale))
        }
        Generator::Bool { true_percent } => Cell::Bool(rng.percent(*true_percent)),
        Generator::DateRange { from, to } => {
            let (a, b) = (parse_date(from)?, parse_date(to)?);
            let j = rng.range_i128(a.to_julian_day().into(), b.to_julian_day().into());
            text(format_date(date_of_julian(j as i64)))
        }
        Generator::DateTimeRange { from, to } => {
            let (a, b) = (parse_datetime(from)?, parse_datetime(to)?);
            let t = rng.range_i128(a.into(), b.into()) as i64;
            text(format!(
                "{} {}",
                format_date(date_of_julian(t.div_euclid(86_400))),
                format_time(t.rem_euclid(86_400))
            ))
        }
        Generator::TimeRange { from, to } => {
            let (a, b) = (parse_time(from)?, parse_time(to)?);
            text(format_time(rng.range_i128(a.into(), b.into()) as i64))
        }
        Generator::Pick { values } => {
            text(values.get(rng.below(values.len() as u64) as usize)?.clone())
        }
        Generator::Fake { fake } => text(fake_text(*fake, rng)),
        Generator::Pattern { pattern: p } => text(pattern(p, rng)),
        Generator::Uuid => text(uuid_text(rng)),
        Generator::Constant { value } => text(value.clone()),
        Generator::JsonObject => Cell::Json(format!(
            "{{\"id\": {}, \"score\": {}, \"active\": {}}}",
            rng.range_i128(1, 100_000),
            rng.below(101),
            rng.percent(50)
        )),
        Generator::RandomBytes { len } => text(
            (0..*len)
                .map(|_| format!("{:02x}", rng.below(256)))
                .collect(),
        ),
    })
}

/// The first `n` characters of `s`.
pub(crate) fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `cell` as a value of `col` (the message names why not, never the value).
pub(crate) fn to_value(
    cell: &Cell,
    col: &TargetColumn,
    kind: SqlKind,
) -> Result<FullValue, String> {
    convert(cell, col, &convert_options(), kind)
        .map_err(|e| format!("A generated value was refused: {}.", e.reason))
}

/// The value of one cell of `col` (spec §4.2): `Ok(None)` for `Skip` and `ForeignKey` (the
/// engine handles them); `Err` when the generated text is not a value of the column. Fake
/// text is cut to the column's `max_chars`.
pub fn generate(
    gen: &Generator,
    col: &TargetColumn,
    rng: &mut SplitMix64,
    row: u64,
    kind: SqlKind,
) -> Result<Option<FullValue>, String> {
    let Some(mut cell) = raw(gen, rng, row) else {
        return Ok(None);
    };
    if let (Generator::Fake { .. }, Cell::Text(s), Some(max)) = (gen, &cell, col.max_chars) {
        if col.logical == LogicalType::Text {
            cell = Cell::Text(cut(s, max as usize));
        }
    }
    to_value(&cell, col, kind).map(Some)
}

/// The distinct values of `values`, in first-seen order.
fn distinct(values: &[String]) -> Vec<&String> {
    let mut seen = std::collections::HashSet::new();
    values.iter().filter(|v| seen.insert(v.as_str())).collect()
}

/// How many distinct values `gen` can make when that is small enough to matter for unique
/// columns; `None` when it is unbounded or not countable.
pub(crate) fn capacity(gen: &Generator) -> Option<u128> {
    let span = |a: i128, b: i128| (b - a + 1).max(0) as u128;
    match gen {
        Generator::IntRange { min, max } => Some(span((*min).into(), (*max).into())),
        Generator::DecimalRange { min, max, scale } => {
            let (lo, hi) = decimal_units(*min, *max, *scale);
            Some(span(lo, hi))
        }
        Generator::Bool { .. } => Some(2),
        Generator::Constant { .. } => Some(1),
        Generator::Pick { values } => Some(distinct(values).len() as u128),
        Generator::DateRange { from, to } => Some(span(
            parse_date(from)?.to_julian_day().into(),
            parse_date(to)?.to_julian_day().into(),
        )),
        Generator::DateTimeRange { from, to } => Some(span(
            parse_datetime(from)?.into(),
            parse_datetime(to)?.into(),
        )),
        Generator::TimeRange { from, to } => {
            Some(span(parse_time(from)?.into(), parse_time(to)?.into()))
        }
        _ => None,
    }
}

/// The `k`-th value (0-based, `k` < [`capacity`]) of a countable generator, before conversion.
pub(crate) fn nth_cell(gen: &Generator, k: u128) -> Option<Cell> {
    let k = k as i128;
    Some(match gen {
        Generator::IntRange { min, .. } => Cell::Number((i128::from(*min) + k).to_string()),
        Generator::DecimalRange { min, max, scale } => {
            Cell::Number(units_text(decimal_units(*min, *max, *scale).0 + k, *scale))
        }
        Generator::Bool { .. } => Cell::Bool(k == 1),
        Generator::Constant { value } => Cell::Text(value.clone()),
        Generator::Pick { values } => Cell::Text(distinct(values).get(k as usize)?.to_string()),
        Generator::DateRange { from, .. } => Cell::Text(format_date(date_of_julian(
            i64::from(parse_date(from)?.to_julian_day()) + k as i64,
        ))),
        Generator::DateTimeRange { from, .. } => {
            let t = parse_datetime(from)? + k as i64;
            Cell::Text(format!(
                "{} {}",
                format_date(date_of_julian(t.div_euclid(86_400))),
                format_time(t.rem_euclid(86_400))
            ))
        }
        Generator::TimeRange { from, .. } => Cell::Text(format_time(parse_time(from)? + k as i64)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::TargetColumn;
    use crate::seed::rng::SplitMix64;
    use dnt_core::server::SqlKind;
    use dnt_core::value::{LogicalType, Value};

    fn col(db_type: &str, logical: LogicalType) -> TargetColumn {
        TargetColumn {
            name: "c".into(),
            db_type: db_type.into(),
            logical,
            nullable: false,
            has_default: false,
            auto_increment: false,
            identity: None,
            generated: false,
            enum_values: vec![],
            max_chars: None,
            collation: None,
            unsigned: db_type.contains("unsigned"),
        }
    }

    fn value(gen: &Generator, c: &TargetColumn, rng: &mut SplitMix64, row: u64) -> Value {
        generate(gen, c, rng, row, SqlKind::MySql)
            .unwrap()
            .unwrap()
            .to_display()
    }

    #[test]
    fn int_range_respects_bounds() {
        let c = col("int", LogicalType::Integer);
        let g = Generator::IntRange { min: -3, max: 3 };
        let mut rng = SplitMix64::new(3);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let Value::Int(n) = value(&g, &c, &mut rng, 0) else {
                panic!("not an int")
            };
            assert!((-3..=3).contains(&n));
            seen.insert(n);
        }
        assert_eq!(seen.len(), 7);
    }

    #[test]
    fn decimal_range_has_scale() {
        let c = col("decimal(12,2)", LogicalType::Decimal);
        let g = Generator::DecimalRange {
            min: 1.0,
            max: 2.0,
            scale: 2,
        };
        let mut rng = SplitMix64::new(4);
        for _ in 0..1000 {
            let Value::Decimal(s) = value(&g, &c, &mut rng, 0) else {
                panic!("not a decimal")
            };
            let (whole, frac) = s.split_once('.').expect("a scale");
            assert_eq!(frac.len(), 2, "{s}");
            assert!(whole.chars().all(|ch| ch.is_ascii_digit()), "{s}");
            let v: f64 = s.parse().unwrap();
            assert!((1.0..=2.0).contains(&v), "{s}");
        }
    }

    #[test]
    fn check_rejects_out_of_type_ranges() {
        let m = SqlKind::MySql;
        let tiny = col("tinyint", LogicalType::Integer);
        assert_eq!(
            check_generator(&Generator::IntRange { min: 0, max: 1000 }, &tiny, m).as_deref(),
            Some("Values must be between -128 and 127.")
        );
        let uint = col("int unsigned", LogicalType::Integer);
        assert_eq!(
            check_generator(&Generator::IntRange { min: -5, max: 10 }, &uint, m).as_deref(),
            Some("Values must be between 0 and 4294967295.")
        );
        let num = col("numeric(5,2)", LogicalType::Decimal);
        let big = Generator::DecimalRange {
            min: 0.0,
            max: 10000.0,
            scale: 2,
        };
        assert_eq!(
            check_generator(&big, &num, SqlKind::Postgres).as_deref(),
            Some("Values must be below 1000.")
        );
        let int = col("int", LogicalType::Integer);
        assert_eq!(
            check_generator(&Generator::IntRange { min: 5, max: 1 }, &int, m).as_deref(),
            Some("The minimum is above the maximum.")
        );
        assert_eq!(
            check_generator(&Generator::IntRange { min: 1, max: 5 }, &int, m),
            None
        );
        let bigint = col("bigint", LogicalType::Integer);
        assert_eq!(
            check_generator(
                &Generator::IntRange {
                    min: 0,
                    max: 1 << 60
                },
                &bigint,
                m
            )
            .as_deref(),
            Some("Values must be between -9007199254740991 and 9007199254740991.")
        );
    }

    #[test]
    fn check_rejects_bad_dates() {
        let m = SqlKind::MySql;
        let d = col("date", LogicalType::Date);
        let bad = Generator::DateRange {
            from: "2026-02-30".into(),
            to: "2026-03-01".into(),
        };
        assert_eq!(
            check_generator(&bad, &d, m).as_deref(),
            Some("Enter dates as YYYY-MM-DD.")
        );
        let reversed = Generator::DateRange {
            from: "2026-03-02".into(),
            to: "2026-03-01".into(),
        };
        assert_eq!(
            check_generator(&reversed, &d, m).as_deref(),
            Some("The start is after the end.")
        );
        let dt = col("datetime", LogicalType::DateTime);
        let bad_dt = Generator::DateTimeRange {
            from: "2026-03-01".into(),
            to: "2026-03-01 10:00:00".into(),
        };
        assert_eq!(
            check_generator(&bad_dt, &dt, m).as_deref(),
            Some("Enter dates and times as YYYY-MM-DD HH:MM:SS.")
        );
        let t = col("time", LogicalType::Time);
        let bad_t = Generator::TimeRange {
            from: "25:00:00".into(),
            to: "26:00:00".into(),
        };
        assert_eq!(
            check_generator(&bad_t, &t, m).as_deref(),
            Some("Enter times as HH:MM:SS.")
        );
    }

    #[test]
    fn check_rejects_empty_and_out_of_range_options() {
        let m = SqlKind::MySql;
        let t = col("varchar(20)", LogicalType::Text);
        assert_eq!(
            check_generator(&Generator::Pick { values: vec![] }, &t, m).as_deref(),
            Some("Enter at least one value.")
        );
        assert_eq!(
            check_generator(&Generator::Bool { true_percent: 101 }, &t, m).as_deref(),
            Some("Enter a percentage from 0 to 100.")
        );
        assert_eq!(
            check_generator(&Generator::RandomBytes { len: 0 }, &t, m).as_deref(),
            Some("Enter a length from 1 to 65535.")
        );
        assert_eq!(
            check_generator(&Generator::Sequence { start: 1, step: 0 }, &t, m).as_deref(),
            Some("The step cannot be 0.")
        );
    }

    #[test]
    fn dates_in_range_and_formatted() {
        let mut rng = SplitMix64::new(5);
        let d = col("date", LogicalType::Date);
        let g = Generator::DateRange {
            from: "2024-02-27".into(),
            to: "2024-03-02".into(),
        };
        for _ in 0..200 {
            let Value::Date(s) = value(&g, &d, &mut rng, 0) else {
                panic!("not a date")
            };
            assert!(
                ("2024-02-27".to_string()..="2024-03-02".to_string()).contains(&s),
                "{s}"
            );
            assert_eq!(s.len(), 10);
        }
        let dt = col("datetime", LogicalType::DateTime);
        let g = Generator::DateTimeRange {
            from: "2024-01-01 10:00:00".into(),
            to: "2024-01-01 10:00:59".into(),
        };
        for _ in 0..200 {
            let Value::DateTime(s) = value(&g, &dt, &mut rng, 0) else {
                panic!("not a datetime")
            };
            assert!(s.starts_with("2024-01-01 10:00:"), "{s}");
            assert_eq!(s.len(), 19);
        }
        let t = col("time", LogicalType::Time);
        let g = Generator::TimeRange {
            from: "08:00:00".into(),
            to: "08:59:59".into(),
        };
        let Value::Time(s) = value(&g, &t, &mut rng, 0) else {
            panic!("not a time")
        };
        assert!(s.starts_with("08:") && s.len() == 8, "{s}");
    }

    #[test]
    fn typed_values() {
        let mut rng = SplitMix64::new(6);
        let b = col("boolean", LogicalType::Bool);
        assert!(matches!(
            value(&Generator::Bool { true_percent: 100 }, &b, &mut rng, 0),
            Value::Bool(true)
        ));
        let u = col("uuid", LogicalType::Uuid);
        let Value::Uuid(s) = value(&Generator::Uuid, &u, &mut rng, 0) else {
            panic!("not a uuid")
        };
        assert_eq!(s.len(), 36);
        assert_eq!(&s[14..15], "4");
        assert!("89ab".contains(&s[19..20]), "{s}");
        let j = col("json", LogicalType::Json);
        let Value::Json(s) = value(&Generator::JsonObject, &j, &mut rng, 0) else {
            panic!("not json")
        };
        assert!(serde_json::from_str::<serde_json::Value>(&s)
            .unwrap()
            .is_object());
        let bin = col("varbinary(32)", LogicalType::Binary);
        let v = generate(
            &Generator::RandomBytes { len: 16 },
            &bin,
            &mut rng,
            0,
            SqlKind::MySql,
        )
        .unwrap()
        .unwrap();
        assert_eq!(v.as_bytes().unwrap().len(), 16);
        let i = col("int", LogicalType::Integer);
        let seq = Generator::Sequence { start: 10, step: 5 };
        let got: Vec<Value> = (0..3).map(|r| value(&seq, &i, &mut rng, r)).collect();
        assert_eq!(got, vec![Value::Int(10), Value::Int(15), Value::Int(20)]);
        assert!(generate(&Generator::Skip, &i, &mut rng, 0, SqlKind::MySql)
            .unwrap()
            .is_none());
        let fk = Generator::ForeignKey {
            constraint: "fk".into(),
        };
        assert!(generate(&fk, &i, &mut rng, 0, SqlKind::MySql)
            .unwrap()
            .is_none());
    }

    #[test]
    fn integer_generators_respect_decimal_precision() {
        let m = SqlKind::MySql;
        let dec = col("decimal(5,2)", LogicalType::Decimal);
        assert_eq!(
            check_generator(&Generator::IntRange { min: 1, max: 1000 }, &dec, m).as_deref(),
            Some("Values must be between -999 and 999.")
        );
        assert_eq!(
            check_generator(&Generator::IntRange { min: 1, max: 999 }, &dec, m),
            None
        );
    }

    #[test]
    fn mysql_timestamp_range() {
        let ts = col("timestamp", LogicalType::DateTime);
        let old = Generator::DateTimeRange {
            from: "1960-01-01 00:00:00".into(),
            to: "2000-01-01 00:00:00".into(),
        };
        assert_eq!(
            check_generator(&old, &ts, SqlKind::MySql).as_deref(),
            Some("MySQL TIMESTAMP holds 1970-01-01 00:00:01 to 2038-01-19 03:14:07.")
        );
        assert_eq!(check_generator(&old, &ts, SqlKind::Postgres), None);
        let dt = col("datetime", LogicalType::DateTime);
        assert_eq!(check_generator(&old, &dt, SqlKind::MySql), None);
    }

    #[test]
    fn random_bytes_fit_the_column() {
        let m = SqlKind::MySql;
        let vb = col("varbinary(8)", LogicalType::Binary);
        assert_eq!(
            check_generator(&Generator::RandomBytes { len: 16 }, &vb, m).as_deref(),
            Some("The column holds at most 8 bytes.")
        );
        let bit = col("bit(1)", LogicalType::Binary);
        assert_eq!(
            check_generator(&Generator::RandomBytes { len: 1 }, &bit, m).as_deref(),
            Some("Random bytes do not fit a BIT column; pick from hex values such as 00, 01.")
        );
    }
    #[test]
    fn fake_and_pattern_generate_text() {
        let mut rng = SplitMix64::new(8);
        let t = col("varchar(100)", LogicalType::Text);
        let Value::Text(e) = value(
            &Generator::Fake {
                fake: FakeKind::Email,
            },
            &t,
            &mut rng,
            0,
        ) else {
            panic!("not text")
        };
        assert!(e.contains('@'), "{e}");
        let Value::Text(p) = value(
            &Generator::Pattern {
                pattern: "##".into(),
            },
            &t,
            &mut rng,
            0,
        ) else {
            panic!("not text")
        };
        assert!(p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()), "{p}");
    }

    #[test]
    fn conversion_failures_are_messages() {
        let mut rng = SplitMix64::new(7);
        let i = col("int", LogicalType::Integer);
        let err = generate(&Generator::Uuid, &i, &mut rng, 0, SqlKind::MySql).unwrap_err();
        assert_eq!(err, "A generated value was refused: not an integer.");
    }

    #[test]
    fn serde_shape() {
        assert_eq!(
            serde_json::to_value(Generator::IntRange { min: 1, max: 2 }).unwrap(),
            serde_json::json!({"kind": "int_range", "min": 1, "max": 2})
        );
        assert_eq!(
            serde_json::to_value(Generator::Fake {
                fake: FakeKind::FirstName
            })
            .unwrap(),
            serde_json::json!({"kind": "fake", "fake": "first_name"})
        );
        assert_eq!(
            serde_json::to_value(FakeKind::Ipv4).unwrap(),
            serde_json::json!("ipv4")
        );
        assert_eq!(
            serde_json::to_value(Generator::Skip).unwrap(),
            serde_json::json!({"kind": "skip"})
        );
    }
}
