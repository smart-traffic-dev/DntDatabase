//! What a column's type allows: integer bounds and `decimal(p,s)`.

use crate::convert::TargetColumn;
use dnt_core::server::DbKind;

/// The largest integer a request may carry (JS numbers are exact up to 2^53 − 1).
pub const JS_SAFE: i128 = (1 << 53) - 1;

/// The type name without its display width, `unsigned` / `zerofill` and case.
fn base_type(db_type: &str) -> String {
    let t = db_type.trim().to_ascii_lowercase();
    let t = t.split('(').next().unwrap_or("").trim();
    t.split_whitespace().next().unwrap_or("").to_string()
}

/// The values an integer column holds; any other column gets the `i64` range.
pub fn int_bounds(col: &TargetColumn, kind: DbKind) -> (i128, i128) {
    let unsigned = kind == DbKind::MySql && col.unsigned;
    let base = base_type(&col.db_type);
    if kind == DbKind::MySql && base == "year" {
        return (1901, 2155);
    }
    let bits: u32 = match base.as_str() {
        "tinyint" if kind == DbKind::MySql => 8,
        "smallint" | "int2" | "smallserial" | "serial2" => 16,
        "mediumint" if kind == DbKind::MySql => 24,
        "int" | "integer" | "int4" | "serial" | "serial4" => 32,
        _ => 64,
    };
    if unsigned {
        (0, (1i128 << bits) - 1)
    } else {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    }
}

/// A MySQL `BIT(n)` column (bits, not bytes: random bytes rarely fit).
pub fn is_bit(db_type: &str) -> bool {
    base_type(db_type) == "bit"
}

/// MySQL `TIMESTAMP` (1970-01-01 00:00:01 to 2038-01-19 03:14:07 UTC).
pub fn is_timestamp(db_type: &str) -> bool {
    base_type(db_type) == "timestamp"
}

/// The most bytes of `binary(n)` / `varbinary(n)`; `None` for other types.
pub fn max_bytes(db_type: &str) -> Option<u32> {
    let t = db_type.trim().to_ascii_lowercase();
    let rest = ["varbinary(", "binary("]
        .iter()
        .find_map(|p| t.strip_prefix(p))?;
    rest.split_once(')')?.0.trim().parse().ok()
}

/// The largest absolute value below `10^(p−s)` of a `decimal(p,s)` column, in whole units
/// (`999` for `decimal(5,2)`); `None` without a precision.
pub fn decimal_whole_max(db_type: &str) -> Option<i128> {
    let (p, s) = decimal_spec(db_type)?;
    Some(10i128.saturating_pow(p.saturating_sub(s)) - 1)
}

/// `(precision, scale)` of `decimal(p[,s])` / `numeric(p[,s])`; `None` without a precision.
pub fn decimal_spec(db_type: &str) -> Option<(u32, u32)> {
    let t = db_type.trim().to_ascii_lowercase();
    let rest = ["decimal(", "numeric(", "dec("]
        .iter()
        .find_map(|p| t.strip_prefix(p))?;
    let (inner, _) = rest.split_once(')')?;
    let mut parts = inner.split(',').map(|s| s.trim().parse::<u32>());
    let p = parts.next()?.ok()?;
    let s = match parts.next() {
        Some(s) => s.ok()?,
        None => 0,
    };
    Some((p, s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::TargetColumn;
    use dnt_core::server::DbKind;
    use dnt_core::value::LogicalType;

    fn col(db_type: &str, unsigned: bool) -> TargetColumn {
        TargetColumn {
            name: "n".into(),
            db_type: db_type.into(),
            logical: LogicalType::Integer,
            nullable: false,
            has_default: false,
            auto_increment: false,
            identity: None,
            generated: false,
            enum_values: vec![],
            max_chars: None,
            collation: None,
            unsigned,
        }
    }

    #[test]
    fn int_bounds_by_type() {
        let m = DbKind::MySql;
        let p = DbKind::Postgres;
        assert_eq!(int_bounds(&col("tinyint", false), m), (-128, 127));
        assert_eq!(int_bounds(&col("tinyint(4) unsigned", true), m), (0, 255));
        assert_eq!(int_bounds(&col("smallint", false), m), (-32768, 32767));
        assert_eq!(int_bounds(&col("mediumint", false), m), (-8388608, 8388607));
        assert_eq!(
            int_bounds(&col("int(11)", false), m),
            (-2147483648, 2147483647)
        );
        assert_eq!(int_bounds(&col("int unsigned", true), m), (0, 4294967295));
        assert_eq!(
            int_bounds(&col("bigint", false), m),
            (i64::MIN as i128, i64::MAX as i128)
        );
        assert_eq!(
            int_bounds(&col("bigint unsigned", true), m),
            (0, u64::MAX as i128)
        );
        assert_eq!(
            int_bounds(&col("integer", false), p),
            (-2147483648, 2147483647)
        );
        assert_eq!(int_bounds(&col("smallint", false), p), (-32768, 32767));
        assert_eq!(int_bounds(&col("year", false), m), (1901, 2155));
        assert_eq!(
            int_bounds(&col("bigint", false), p),
            (i64::MIN as i128, i64::MAX as i128)
        );
    }

    #[test]
    fn decimal_spec_reads_precision_and_scale() {
        assert_eq!(decimal_spec("decimal(12,2)"), Some((12, 2)));
        assert_eq!(decimal_spec("NUMERIC(5, 2)"), Some((5, 2)));
        assert_eq!(decimal_spec("decimal(10)"), Some((10, 0)));
        assert_eq!(decimal_spec("numeric"), None);
        assert_eq!(decimal_spec("double precision"), None);
    }
}
