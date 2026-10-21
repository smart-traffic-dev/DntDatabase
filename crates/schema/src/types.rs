//! The type catalog and the parsing / spelling of column types.

use crate::model::{ParamUse, TypeInfo, TypeSpec};
use dnt_core::server::{DbKind, ServerInfo};

use ParamUse::{None as No, Optional as Opt, Required as Req};

/// `(base, length, precision, scale, unsigned, values, collation)`.
type Entry = (&'static str, ParamUse, ParamUse, ParamUse, bool, bool, bool);

const MYSQL: &[Entry] = &[
    // Integer `length` is the (deprecated) display width, e.g. `tinyint(1)`.
    ("tinyint", Opt, No, No, true, false, false),
    ("smallint", Opt, No, No, true, false, false),
    ("mediumint", Opt, No, No, true, false, false),
    ("int", Opt, No, No, true, false, false),
    ("bigint", Opt, No, No, true, false, false),
    ("decimal", No, Opt, Opt, true, false, false),
    ("float", No, Opt, Opt, true, false, false),
    ("double", No, Opt, Opt, true, false, false),
    ("bit", Opt, No, No, false, false, false),
    ("bool", No, No, No, false, false, false),
    ("char", Opt, No, No, false, false, true),
    ("varchar", Req, No, No, false, false, true),
    ("binary", Opt, No, No, false, false, false),
    ("varbinary", Req, No, No, false, false, false),
    ("tinytext", No, No, No, false, false, true),
    ("text", No, No, No, false, false, true),
    ("mediumtext", No, No, No, false, false, true),
    ("longtext", No, No, No, false, false, true),
    ("tinyblob", No, No, No, false, false, false),
    ("blob", No, No, No, false, false, false),
    ("mediumblob", No, No, No, false, false, false),
    ("longblob", No, No, No, false, false, false),
    ("enum", No, No, No, false, true, true),
    ("set", No, No, No, false, true, true),
    ("date", No, No, No, false, false, false),
    // `precision` of a temporal type is its fractional seconds.
    ("time", No, Opt, No, false, false, false),
    ("datetime", No, Opt, No, false, false, false),
    ("timestamp", No, Opt, No, false, false, false),
    ("year", No, No, No, false, false, false),
    ("json", No, No, No, false, false, false),
];

const POSTGRES: &[Entry] = &[
    ("smallint", No, No, No, false, false, false),
    ("integer", No, No, No, false, false, false),
    ("bigint", No, No, No, false, false, false),
    ("numeric", No, Opt, Opt, false, false, false),
    ("real", No, No, No, false, false, false),
    ("double precision", No, No, No, false, false, false),
    ("boolean", No, No, No, false, false, false),
    ("char", Opt, No, No, false, false, true),
    ("varchar", Opt, No, No, false, false, true),
    ("text", No, No, No, false, false, true),
    ("bytea", No, No, No, false, false, false),
    ("date", No, No, No, false, false, false),
    ("time", No, Opt, No, false, false, false),
    ("timetz", No, Opt, No, false, false, false),
    ("timestamp", No, Opt, No, false, false, false),
    ("timestamptz", No, Opt, No, false, false, false),
    ("interval", No, Opt, No, false, false, false),
    ("uuid", No, No, No, false, false, false),
    ("json", No, No, No, false, false, false),
    ("jsonb", No, No, No, false, false, false),
    ("inet", No, No, No, false, false, false),
    ("cidr", No, No, No, false, false, false),
    ("macaddr", No, No, No, false, false, false),
];

fn entries(server: &ServerInfo) -> &'static [Entry] {
    match server.kind {
        DbKind::MySql => MYSQL,
        DbKind::Postgres => POSTGRES,
    }
}

fn info(e: &Entry) -> TypeInfo {
    let &(base, length, precision, scale, unsigned, values, collation) = e;
    TypeInfo {
        base: base.into(),
        length,
        precision,
        scale,
        unsigned,
        values,
        collation,
    }
}

/// The types the designer offers for `server`, in display order.
pub fn type_catalog(server: &ServerInfo) -> Vec<TypeInfo> {
    entries(server).iter().map(info).collect()
}

/// The catalog entry for `base` (exact, lowercase name), if `server` has it.
pub(crate) fn type_info(base: &str, server: &ServerInfo) -> Option<TypeInfo> {
    entries(server).iter().find(|e| e.0 == base).map(info)
}

/// A server type spelling (MySQL `COLUMN_TYPE`, PostgreSQL `format_type`) as a
/// [`TypeSpec`]. Anything outside the catalog (arrays, domains, user-defined types,
/// `zerofill`, interval fields, …) is [`TypeSpec::Custom`] with the text as given.
pub fn parse_type(db_type: &str, server: &ServerInfo) -> TypeSpec {
    let text = db_type.trim();
    parse_known(text, server).unwrap_or_else(|| TypeSpec::Custom { text: text.into() })
}

fn parse_known(text: &str, server: &ServerInfo) -> Option<TypeSpec> {
    let lower = text.to_ascii_lowercase();
    if server.kind == DbKind::MySql {
        for base in ["enum", "set"] {
            if let Some(list) = lower
                .strip_prefix(base)
                .and_then(|r| r.strip_prefix('('))
                .filter(|r| r.ends_with(')'))
            {
                // Values keep their case: take them from `text`, not `lower`.
                let inner = &text[text.len() - list.len()..text.len() - 1];
                return Some(TypeSpec::Known {
                    base: base.into(),
                    length: None,
                    precision: None,
                    scale: None,
                    unsigned: false,
                    values: parse_quoted_list(inner)?,
                });
            }
        }
    }
    // `<name>[(<args>)][ <suffix>]`
    let (name, args, suffix) = match lower.find('(') {
        Some(open) => {
            let close = open + lower[open..].find(')')?;
            (
                &lower[..open],
                Some(&lower[open + 1..close]),
                &lower[close + 1..],
            )
        }
        // Without parameters the suffix is what follows the first word, except for
        // PostgreSQL names of several words (`double precision`, `character varying`).
        None => {
            let first = lower.split_whitespace().next().unwrap_or("");
            let split = server.kind == DbKind::MySql || matches!(first, "time" | "timestamp");
            match lower.trim().split_once(char::is_whitespace) {
                Some((name, rest)) if split => (name, None, rest),
                _ => (lower.as_str(), None, ""),
            }
        }
    };
    if suffix.contains(['(', ')']) {
        return None;
    }
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let suffix = suffix.split_whitespace().collect::<Vec<_>>().join(" ");
    let (base, unsigned) = match server.kind {
        DbKind::MySql => match suffix.as_str() {
            "" => (name, false),
            "unsigned" => (name, true),
            _ => return None,
        },
        DbKind::Postgres => (pg_base(&name, &suffix)?.to_string(), false),
    };
    let ti = type_info(&base, server)?;
    if unsigned && !ti.unsigned {
        return None;
    }
    let mut nums = Vec::new();
    if let Some(args) = args {
        // MySQL 5.7 spells the only YEAR as `year(4)`.
        if !(server.kind == DbKind::MySql && base == "year" && args.trim() == "4") {
            for a in args.split(',') {
                nums.push(a.trim().parse::<u32>().ok()?);
            }
        }
    }
    let (mut length, mut precision, mut scale) = (None, None, None);
    match nums.as_slice() {
        [] => {}
        [n] if ti.length != No => length = Some(*n),
        [p] if ti.precision != No => precision = Some(*p),
        [p, s] if ti.precision != No && ti.scale != No => {
            precision = Some(*p);
            scale = Some(*s);
        }
        _ => return None,
    }
    if length.is_some() && length == default_display_width(&base, unsigned) {
        length = None;
    }
    Some(TypeSpec::Known {
        base,
        length,
        precision,
        scale,
        unsigned,
        values: Vec::new(),
    })
}

/// The canonical catalog name of a PostgreSQL `format_type` spelling.
fn pg_base(name: &str, suffix: &str) -> Option<&'static str> {
    let zone = |without: &'static str, with: &'static str| match suffix {
        "" | "without time zone" => Some(without),
        "with time zone" => Some(with),
        _ => None,
    };
    match name {
        "timestamp" => return zone("timestamp", "timestamptz"),
        "time" => return zone("time", "timetz"),
        _ if !suffix.is_empty() => return None,
        _ => {}
    }
    Some(match name {
        "smallint" | "int2" => "smallint",
        "integer" | "int" | "int4" => "integer",
        "bigint" | "int8" => "bigint",
        "numeric" | "decimal" => "numeric",
        "real" | "float4" => "real",
        "double precision" | "float8" => "double precision",
        "boolean" | "bool" => "boolean",
        // Not `bpchar`: unbounded `bpchar` is not `char(1)`, so it stays Custom.
        "character" | "char" => "char",
        "character varying" | "varchar" => "varchar",
        "timestamptz" => "timestamptz",
        "timetz" => "timetz",
        "text" => "text",
        "bytea" => "bytea",
        "date" => "date",
        "interval" => "interval",
        "uuid" => "uuid",
        "json" => "json",
        "jsonb" => "jsonb",
        "inet" => "inet",
        "cidr" => "cidr",
        "macaddr" => "macaddr",
        _ => return None,
    })
}

/// The display width MySQL 5.7 / MariaDB report when none was given.
fn default_display_width(base: &str, unsigned: bool) -> Option<u32> {
    Some(match (base, unsigned) {
        ("tinyint", false) => 4,
        ("tinyint", true) => 3,
        ("smallint", false) => 6,
        ("smallint", true) => 5,
        ("mediumint", false) => 9,
        ("mediumint", true) => 8,
        ("int", false) => 11,
        ("int", true) => 10,
        ("bigint", _) => 20,
        _ => return None,
    })
}

/// `'a','b''c'` → `["a", "b'c"]`; `None` unless it is exactly a list of quoted strings.
fn parse_quoted_list(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.next()? != '\'' {
            return None;
        }
        let mut v = String::new();
        loop {
            match chars.next()? {
                '\'' if chars.next_if_eq(&'\'').is_some() => v.push('\''),
                '\'' => break,
                c => v.push(c),
            }
        }
        out.push(v);
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        match chars.next() {
            None => return Some(out),
            Some(',') => {}
            Some(_) => return None,
        }
    }
}

/// The canonical spelling of `t` for DDL: `decimal(14,2)`, `int unsigned`,
/// `enum('a','b''c')`, PG `numeric(14,2)`, `varchar(20)`, `timestamptz(3)`.
/// A [`TypeSpec::Custom`] is its text unchanged.
pub fn type_text(t: &TypeSpec, _server: &ServerInfo) -> String {
    let (base, length, precision, scale, unsigned, values) = match t {
        TypeSpec::Custom { text } => return text.clone(),
        TypeSpec::Known {
            base,
            length,
            precision,
            scale,
            unsigned,
            values,
        } => (base, length, precision, scale, unsigned, values),
    };
    let mut out = base.clone();
    if !values.is_empty() {
        let list: Vec<String> = values
            .iter()
            .map(|v| format!("'{}'", v.replace('\'', "''")))
            .collect();
        out.push_str(&format!("({})", list.join(",")));
    } else if let Some(n) = length {
        out.push_str(&format!("({n})"));
    } else if let Some(p) = precision {
        match scale {
            Some(s) => out.push_str(&format!("({p},{s})")),
            None => out.push_str(&format!("({p})")),
        }
    }
    if *unsigned {
        out.push_str(" unsigned");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    fn known(base: &str) -> TypeSpec {
        TypeSpec::Known {
            base: base.into(),
            length: None,
            precision: None,
            scale: None,
            unsigned: false,
            values: vec![],
        }
    }

    fn with(t: TypeSpec, f: impl FnOnce(&mut TypeSpec)) -> TypeSpec {
        let mut t = t;
        f(&mut t);
        t
    }

    fn set_len(n: u32) -> impl FnOnce(&mut TypeSpec) {
        move |t| {
            if let TypeSpec::Known { length, .. } = t {
                *length = Some(n);
            }
        }
    }

    fn set_ps(p: u32, s: Option<u32>) -> impl FnOnce(&mut TypeSpec) {
        move |t| {
            if let TypeSpec::Known {
                precision, scale, ..
            } = t
            {
                *precision = Some(p);
                *scale = s;
            }
        }
    }

    #[test]
    fn parse_type_known() {
        let my = mysql8();
        assert_eq!(
            parse_type("decimal(12,2)", &my),
            with(known("decimal"), set_ps(12, Some(2)))
        );
        assert_eq!(
            parse_type("int unsigned", &my),
            with(known("int"), |t| {
                if let TypeSpec::Known { unsigned, .. } = t {
                    *unsigned = true;
                }
            })
        );
        assert_eq!(
            parse_type("enum('a','b''c')", &my),
            with(known("enum"), |t| {
                if let TypeSpec::Known { values, .. } = t {
                    *values = vec!["a".into(), "b'c".into()];
                }
            })
        );
        assert_eq!(
            parse_type("varchar(16)", &my),
            with(known("varchar"), set_len(16))
        );
        let pg = pg();
        assert_eq!(
            parse_type("numeric(12,2)", &pg),
            with(known("numeric"), set_ps(12, Some(2)))
        );
        assert_eq!(
            parse_type("character varying(20)", &pg),
            with(known("varchar"), set_len(20))
        );
        assert_eq!(
            parse_type("integer[]", &pg),
            TypeSpec::Custom {
                text: "integer[]".into()
            }
        );
        assert_eq!(
            parse_type("order_status", &pg),
            TypeSpec::Custom {
                text: "order_status".into()
            }
        );
    }

    #[test]
    fn parse_type_server_spellings() {
        let my57 = mysql57();
        // MySQL 5.7 default display widths are dropped; a meaningful one is kept.
        assert_eq!(parse_type("int(11)", &my57), known("int"));
        assert_eq!(
            parse_type("int(10) unsigned", &my57),
            parse_type("int unsigned", &my57)
        );
        assert_eq!(
            parse_type("tinyint(1)", &my57),
            with(known("tinyint"), set_len(1))
        );
        assert_eq!(parse_type("year(4)", &my57), known("year"));
        assert_eq!(
            parse_type("datetime(6)", &my57),
            with(known("datetime"), set_ps(6, None))
        );
        assert_eq!(
            parse_type("int(10) unsigned zerofill", &my57),
            TypeSpec::Custom {
                text: "int(10) unsigned zerofill".into()
            }
        );
        assert_eq!(
            parse_type("geometry", &my57),
            TypeSpec::Custom {
                text: "geometry".into()
            }
        );
        let pg = pg();
        assert_eq!(
            parse_type("timestamp without time zone", &pg),
            known("timestamp")
        );
        assert_eq!(
            parse_type("timestamp(3) with time zone", &pg),
            with(known("timestamptz"), set_ps(3, None))
        );
        assert_eq!(parse_type("time with time zone", &pg), known("timetz"));
        assert_eq!(
            parse_type("character(10)", &pg),
            with(known("char"), set_len(10))
        );
        assert_eq!(
            parse_type("double precision", &pg),
            known("double precision")
        );
        assert_eq!(parse_type("numeric", &pg), known("numeric"));
        assert_eq!(
            parse_type("bpchar", &pg),
            TypeSpec::Custom {
                text: "bpchar".into()
            }
        );
        assert_eq!(
            parse_type("interval year to month", &pg),
            TypeSpec::Custom {
                text: "interval year to month".into()
            }
        );
        // A MySQL-only name is not a PG catalog type.
        assert_eq!(
            parse_type("tinyint", &pg),
            TypeSpec::Custom {
                text: "tinyint".into()
            }
        );
    }

    #[test]
    fn type_text_round_trips() {
        let my = mysql8();
        for (x, canonical) in [
            ("decimal(12,2)", "decimal(12,2)"),
            ("decimal(14,2)", "decimal(14,2)"),
            ("int unsigned", "int unsigned"),
            ("enum('a','b''c')", "enum('a','b''c')"),
            ("varchar(16)", "varchar(16)"),
            ("int(10) unsigned", "int unsigned"),
            ("tinyint(1)", "tinyint(1)"),
            ("datetime(6)", "datetime(6)"),
            ("set('a','b')", "set('a','b')"),
            ("double", "double"),
        ] {
            assert_eq!(type_text(&parse_type(x, &my), &my), canonical, "{x}");
        }
        let pg = pg();
        for (x, canonical) in [
            ("numeric(12,2)", "numeric(12,2)"),
            ("numeric(14,2)", "numeric(14,2)"),
            ("character varying(20)", "varchar(20)"),
            ("integer[]", "integer[]"),
            ("order_status", "order_status"),
            ("timestamp(3) with time zone", "timestamptz(3)"),
            ("double precision", "double precision"),
            ("character(10)", "char(10)"),
            ("bpchar", "bpchar"),
        ] {
            assert_eq!(type_text(&parse_type(x, &pg), &pg), canonical, "{x}");
        }
    }

    #[test]
    fn catalog_per_server() {
        let names = |s| {
            type_catalog(&s)
                .into_iter()
                .map(|t| t.base)
                .collect::<Vec<_>>()
        };
        let my = names(mysql8());
        assert!(my.contains(&"mediumint".to_string()));
        assert!(!my.contains(&"jsonb".to_string()));
        assert_eq!(my, names(mariadb()));
        let pg = names(pg());
        assert!(pg.contains(&"double precision".to_string()));
        assert!(pg.contains(&"timestamptz".to_string()));
        assert!(!pg.contains(&"enum".to_string()));
        let varchar = type_catalog(&mysql8())
            .into_iter()
            .find(|t| t.base == "varchar")
            .unwrap();
        assert_eq!(varchar.length, crate::ParamUse::Required);
        assert!(varchar.collation);
    }
}
