//! Spec §5.1 steps 3–5: the `CREATE` header of the isolated statement, and the
//! check that it defines the object being edited.

use super::{editable_kind, kind_label, text_error, EditTarget};
use crate::error::DbError;
use crate::meta::ObjectKind;
use crate::server::{DbKind, ServerInfo, ServerVariant};
use crate::sql::ddl::validate_identifier;
use crate::sql::lexer::{is_kw, tokenize, word_at, Dialect, TokKind, Token};
use crate::sql::multi::skip_user;

/// The parsed `CREATE` header of an edited statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub kind: ObjectKind,
    pub or_replace: bool,
    pub qualifier: Option<String>,
    pub name: String,
    /// Byte offset just past the `CREATE` keyword.
    pub create_end: usize,
    /// Byte span of the (qualified) name.
    pub name_span: (usize, usize),
    /// Triggers: the `ON` table (qualifier, name).
    pub on_table: Option<(Option<String>, String)>,
}

/// Parses the `CREATE` header of `stmt` (an isolated statement): spec §5.1 step 3.
pub fn parse_header(stmt: &str, dialect: Dialect) -> Result<Header, DbError> {
    let toks = tokenize(stmt, dialect);
    if toks.iter().any(|t| t.kind == TokKind::ExecComment) {
        return Err(text_error(
            "Executable comments (/*! … */) cannot be applied here; remove them.",
        ));
    }
    let kw = |i: usize, k: &str| is_kw(word_at(&toks, i), k);
    let punct = |i: usize, c: u8| toks.get(i).is_some_and(|t| t.kind == TokKind::Punct(c));
    if !kw(0, "CREATE") {
        return Err(text_error("The text must be a CREATE statement."));
    }
    let create_end = toks[0].end;
    let mut i = 1;
    let mut or_replace = false;
    if kw(i, "OR") && kw(i + 1, "REPLACE") {
        or_replace = true;
        i += 2;
        if kw(i, "MATERIALIZED") {
            return Err(DbError::Unsupported {
                feature: "CREATE OR REPLACE MATERIALIZED VIEW".into(),
                version: "PostgreSQL (none)".into(),
            });
        }
    }
    let mysql = dialect.is_mysql();
    loop {
        if mysql && kw(i, "ALGORITHM") && punct(i + 1, b'=') && word_at(&toks, i + 2).is_some() {
            i += 3;
        } else if mysql && kw(i, "DEFINER") && punct(i + 1, b'=') {
            i = skip_user(&toks, i + 2)
                .ok_or_else(|| text_error("Write the DEFINER as user@host or CURRENT_USER."))?;
        } else if mysql && kw(i, "SQL") && kw(i + 1, "SECURITY") && word_at(&toks, i + 2).is_some()
        {
            i += 3;
        } else if !mysql && (kw(i, "TEMP") || kw(i, "TEMPORARY")) {
            return Err(text_error("Temporary objects cannot be edited here."));
        } else if kw(i, "AGGREGATE") || (!mysql && (kw(i, "RECURSIVE") || kw(i, "CONSTRAINT"))) {
            i += 1;
        } else {
            break;
        }
    }
    let kind = if kw(i, "VIEW") {
        ObjectKind::View
    } else if kw(i, "MATERIALIZED") && kw(i + 1, "VIEW") {
        i += 1;
        ObjectKind::MaterializedView
    } else if kw(i, "FUNCTION") {
        ObjectKind::Function
    } else if kw(i, "PROCEDURE") {
        ObjectKind::Procedure
    } else if kw(i, "TRIGGER") {
        ObjectKind::Trigger
    } else if kw(i, "EVENT") {
        if !mysql {
            return Err(text_error("Event triggers cannot be edited here."));
        }
        ObjectKind::Event
    } else {
        return Err(text_error(
            "The text must create a view, materialized view, function, procedure, trigger or event.",
        ));
    };
    i += 1;
    if kw(i, "IF") && kw(i + 1, "NOT") && kw(i + 2, "EXISTS") {
        return Err(text_error(
            "IF NOT EXISTS would hide an existing object; remove it.",
        ));
    }
    let what = kind_label(kind);
    let (qualifier, name, name_span, next) =
        qualified_ident(stmt, &toks, i, dialect).ok_or_else(|| {
            text_error(format!(
                "Write the {what}'s name after {}.",
                kind_word(kind)
            ))
        })?;
    i = next;
    // Depth-0 words after the name.
    let mut depth = 0i64;
    let mut on_table = None;
    let mut j = i;
    while j < toks.len() {
        match toks[j].kind {
            TokKind::Open => depth += 1,
            TokKind::Close => depth -= 1,
            TokKind::Word(w) if depth == 0 => {
                if mysql && kind == ObjectKind::Function && w.eq_ignore_ascii_case("SONAME") {
                    return Err(text_error("Loadable functions cannot be edited here."));
                }
                if kind == ObjectKind::Trigger && on_table.is_none() && w.eq_ignore_ascii_case("ON")
                {
                    let (q, n, _, _) = qualified_ident(stmt, &toks, j + 1, dialect)
                        .ok_or_else(|| text_error("Write the trigger's table after ON."))?;
                    on_table = Some((q, n));
                }
            }
            _ => {}
        }
        j += 1;
    }
    if kind == ObjectKind::Trigger && on_table.is_none() {
        return Err(text_error("Write the trigger's table after ON."));
    }
    Ok(Header {
        kind,
        or_replace,
        qualifier,
        name,
        create_end,
        name_span,
        on_table,
    })
}

type QualifiedIdent = (Option<String>, String, (usize, usize), usize);

/// `identifier [. identifier]` at token `i`: (qualifier, name, byte span, next token).
fn qualified_ident(stmt: &str, toks: &[Token], i: usize, d: Dialect) -> Option<QualifiedIdent> {
    let first = ident(stmt, toks.get(i)?, d)?;
    let start = toks[i].start;
    let is_dot = |k: usize| toks.get(k).is_some_and(|t| t.kind == TokKind::Punct(b'.'));
    if !is_dot(i + 1) {
        return Some((None, first, (start, toks[i].end), i + 1));
    }
    let second = ident(stmt, toks.get(i + 2)?, d)?;
    if is_dot(i + 3) {
        // `a.b.c`: refused rather than guessing which part is the schema.
        return None;
    }
    Some((Some(first), second, (start, toks[i + 2].end), i + 3))
}

/// The identifier a token spells, unquoted (PostgreSQL unquoted words fold to ASCII
/// lower case); `None` for anything that is not a name (strings, punctuation).
fn ident(stmt: &str, t: &Token, d: Dialect) -> Option<String> {
    match t.kind {
        TokKind::Word(w) => Some(match d {
            Dialect::Postgres => w.to_ascii_lowercase(),
            Dialect::MySql(_) => w.to_string(),
        }),
        TokKind::Quoted => {
            let s = &stmt[t.start..t.end];
            let q = s.as_bytes()[0];
            let quote_ok = match d {
                Dialect::MySql(m) => q == b'`' || (q == b'"' && m.ansi_quotes),
                Dialect::Postgres => q == b'"',
            };
            if !quote_ok {
                return None;
            }
            unquote(s, q as char)
        }
        _ => None,
    }
}

/// `s` without its surrounding quotes `q`, with doubled quotes collapsed; `None` when
/// unterminated or empty.
fn unquote(s: &str, q: char) -> Option<String> {
    let inner = s.strip_prefix(q)?.strip_suffix(q)?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == q && chars.next() != Some(q) {
            return None;
        }
        out.push(c);
    }
    (!out.is_empty()).then_some(out)
}

fn kind_word(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Trigger => "TRIGGER",
        ObjectKind::Event => "EVENT",
        _ => "VIEW",
    }
}

/// Checks that the header defines the target object, on this engine and version:
/// spec §5.1 steps 4–5.
pub fn check_target(h: &Header, t: &EditTarget, server: &ServerInfo) -> Result<(), DbError> {
    editable_kind(t.kind, server)?;
    if h.kind != t.kind {
        return Err(text_error(format!(
            "The text defines a {}, not a {}.",
            kind_label(h.kind),
            kind_label(t.kind)
        )));
    }
    let pg = server.kind == DbKind::Postgres;
    let new = t.name.is_none();
    if new && h.or_replace {
        return Err(text_error("Remove OR REPLACE: the object is new."));
    }
    // Engine and version.
    if h.or_replace {
        let mysql_proper = matches!(
            server.variant,
            ServerVariant::MySql57 | ServerVariant::MySql8
        );
        if mysql_proper
            && matches!(
                h.kind,
                ObjectKind::Function
                    | ObjectKind::Procedure
                    | ObjectKind::Trigger
                    | ObjectKind::Event
            )
        {
            // MySQL has no CREATE OR REPLACE for these: write CREATE, and the app
            // drops and re-creates the object.
            return Err(DbError::Unsupported {
                feature: format!("CREATE OR REPLACE {}", kind_word(h.kind)),
                version: "MariaDB".into(),
            });
        }
        if pg && h.kind == ObjectKind::Trigger && server.version.0 < 14 {
            return Err(DbError::Unsupported {
                feature: "CREATE OR REPLACE TRIGGER".into(),
                version: "PostgreSQL 14".into(),
            });
        }
    }
    // The name.
    match &t.name {
        Some(n) if &h.name != n => {
            let shown = match &h.qualifier {
                Some(q) => format!("`{q}`.`{}`", h.name),
                None => format!("`{}`", h.name),
            };
            return Err(text_error(format!(
                "The text defines {shown}; renaming through the DDL editor is not supported."
            )));
        }
        Some(_) => {}
        None => validate_identifier(server.kind, &h.name).map_err(|e| match e {
            DbError::Config { message, .. } => text_error(message),
            other => other,
        })?,
    }
    // The qualifier.
    let namespace = if pg {
        let schema = t.schema.as_deref().ok_or_else(|| DbError::Config {
            field: Some("schema".into()),
            message: "A PostgreSQL schema is required.".into(),
        })?;
        match h.qualifier.as_deref() {
            // A PostgreSQL trigger takes its table's schema; its name is never
            // qualified (the `ON` table is checked below).
            None if h.kind == ObjectKind::Trigger => schema,
            Some(_) if h.kind == ObjectKind::Trigger => {
                return Err(text_error(
                    "A PostgreSQL trigger name cannot be qualified; qualify the table after ON instead.",
                ))
            }
            None => {
                return Err(text_error(format!(
                    "Qualify the name with its schema: {schema}.{}",
                    h.name
                )))
            }
            Some(q) if q != schema => {
                return Err(text_error(format!(
                    "The text puts the {} in schema {q}; keep it in {schema}.",
                    kind_label(h.kind)
                )))
            }
            Some(_) => schema,
        }
    } else {
        let db = t.database.as_str();
        if let Some(q) = h.qualifier.as_deref() {
            if q != db {
                return Err(text_error(format!(
                    "The text puts the {} in database `{q}`; keep it in `{db}`.",
                    kind_label(h.kind)
                )));
            }
        }
        db
    };
    // A trigger's table.
    if h.kind == ObjectKind::Trigger {
        let (tq, tn) = h
            .on_table
            .as_ref()
            .ok_or_else(|| text_error("Write the trigger's table after ON."))?;
        if let Some(table) = &t.table {
            if tn != table {
                return Err(text_error(format!(
                    "The text puts the trigger on {tn}; it must stay on {table}."
                )));
            }
        }
        match tq.as_deref() {
            Some(q) if q != namespace => {
                return Err(text_error(format!(
                    "The trigger's table must be in {namespace}, not {q}."
                )))
            }
            None if pg && new => {
                return Err(text_error(format!(
                    "Qualify the trigger's table with its schema: {namespace}.{tn}"
                )))
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_servers::*;
    use super::*;
    use crate::sql::lexer::MySqlMode;

    const MY: Dialect = Dialect::MYSQL;
    const PG: Dialect = Dialect::Postgres;

    fn parse(stmt: &str, d: Dialect) -> Header {
        parse_header(stmt, d).unwrap_or_else(|e| panic!("{stmt:?}: {e:?}"))
    }

    fn span<'a>(stmt: &'a str, h: &Header) -> &'a str {
        &stmt[h.name_span.0..h.name_span.1]
    }

    fn text_refusal(r: Result<impl std::fmt::Debug, DbError>) -> String {
        match r {
            Err(DbError::Config {
                field: Some(f),
                message,
            }) if f == "text" => message,
            other => panic!("expected a text refusal, got {other:?}"),
        }
    }

    #[test]
    fn parse_header_kinds() {
        let s = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS SELECT 1";
        let h = parse(s, MY);
        assert_eq!(h.kind, ObjectKind::View);
        assert!(!h.or_replace);
        assert_eq!((h.qualifier.as_deref(), h.name.as_str()), (None, "v"));
        assert_eq!(span(s, &h), "`v`");
        assert_eq!(h.create_end, 6);
        assert_eq!(h.on_table, None);

        let s = "/* c */ CREATE DEFINER=CURRENT_USER PROCEDURE `db`.`p``x`(IN a INT) BEGIN END";
        let h = parse(s, MY);
        assert_eq!(h.kind, ObjectKind::Procedure);
        assert_eq!(h.qualifier.as_deref(), Some("db"));
        assert_eq!(h.name, "p`x");
        assert_eq!(span(s, &h), "`db`.`p``x`");
        assert_eq!(&s[..h.create_end], "/* c */ CREATE");

        let s =
            "CREATE DEFINER=CURRENT_USER() FUNCTION f(a INT) RETURNS INT DETERMINISTIC RETURN a";
        let h = parse(s, MY);
        assert_eq!((h.kind, h.name.as_str()), (ObjectKind::Function, "f"));

        let s = "CREATE OR REPLACE DEFINER=`root`@`localhost` TRIGGER `db`.`Trg` BEFORE INSERT ON `db`.`Orders` FOR EACH ROW SET NEW.a = 1";
        let h = parse(s, MY);
        assert_eq!(h.kind, ObjectKind::Trigger);
        assert!(h.or_replace);
        assert_eq!(
            (h.qualifier.as_deref(), h.name.as_str()),
            (Some("db"), "Trg")
        );
        assert_eq!(h.on_table, Some((Some("db".into()), "Orders".into())));

        let s = "CREATE DEFINER = root@localhost EVENT e ON SCHEDULE EVERY 1 DAY DO DELETE FROM t";
        let h = parse(s, MY);
        assert_eq!((h.kind, h.name.as_str()), (ObjectKind::Event, "e"));

        let s = "CREATE AGGREGATE FUNCTION agg(x INT) RETURNS INT BEGIN RETURN 1; END";
        assert_eq!(parse(s, MY).kind, ObjectKind::Function);

        // MySQL keeps the case of unquoted names.
        let s = "CREATE VIEW Db.MyView AS SELECT 1";
        let h = parse(s, MY);
        assert_eq!(
            (h.qualifier.as_deref(), h.name.as_str()),
            (Some("Db"), "MyView")
        );
        // Without ANSI_QUOTES a double-quoted text is a string, not a name.
        text_refusal(parse_header("CREATE VIEW \"v\" AS SELECT 1", MY));
        let ansi = Dialect::MySql(MySqlMode {
            ansi_quotes: true,
            no_backslash_escapes: false,
        });
        let s = "CREATE VIEW \"d\".\"v\"\"w\" AS SELECT 1";
        let h = parse(s, ansi);
        assert_eq!(
            (h.qualifier.as_deref(), h.name.as_str()),
            (Some("d"), "v\"w")
        );
        assert_eq!(span(s, &h), "\"d\".\"v\"\"w\"");

        // PostgreSQL.
        let s = "CREATE OR REPLACE FUNCTION public.f(a integer) RETURNS integer LANGUAGE sql AS $$ SELECT a $$";
        let h = parse(s, PG);
        assert_eq!(h.kind, ObjectKind::Function);
        assert!(h.or_replace);
        assert_eq!(
            (h.qualifier.as_deref(), h.name.as_str()),
            (Some("public"), "f")
        );
        assert_eq!(span(s, &h), "public.f");

        let h = parse(
            "CREATE FUNCTION Public.F() RETURNS int AS 'x' LANGUAGE sql",
            PG,
        );
        assert_eq!(
            (h.qualifier.as_deref(), h.name.as_str()),
            (Some("public"), "f")
        );

        let s = "CREATE MATERIALIZED VIEW \"S\".\"Mv\" AS SELECT 1";
        let h = parse(s, PG);
        assert_eq!(h.kind, ObjectKind::MaterializedView);
        assert_eq!((h.qualifier.as_deref(), h.name.as_str()), (Some("S"), "Mv"));
        assert_eq!(span(s, &h), "\"S\".\"Mv\"");

        let s = "CREATE OR REPLACE RECURSIVE VIEW public.r (n) AS VALUES (1)";
        assert_eq!(parse(s, PG).kind, ObjectKind::View);

        let s = "CREATE PROCEDURE s.p(IN a int) LANGUAGE plpgsql AS $$ BEGIN END $$";
        assert_eq!(parse(s, PG).kind, ObjectKind::Procedure);

        let s = "CREATE TRIGGER trg BEFORE UPDATE OF a, b ON orders FOR EACH ROW EXECUTE FUNCTION public.f()";
        let h = parse(s, PG);
        assert_eq!(h.kind, ObjectKind::Trigger);
        assert_eq!((h.qualifier.as_deref(), h.name.as_str()), (None, "trg"));
        assert_eq!(h.on_table, Some((None, "orders".into())));

        let s = "CREATE CONSTRAINT TRIGGER Trg AFTER INSERT ON App.\"Orders\" FOR EACH ROW EXECUTE FUNCTION f()";
        let h = parse(s, PG);
        assert_eq!(h.name, "trg");
        assert_eq!(h.on_table, Some((Some("app".into()), "Orders".into())));
    }

    #[test]
    fn parse_header_refusals() {
        let m = text_refusal(parse_header("CREATE VIEW IF NOT EXISTS v AS SELECT 1", MY));
        assert_eq!(m, "IF NOT EXISTS would hide an existing object; remove it.");
        text_refusal(parse_header(
            "CREATE DEFINER=CURRENT_USER EVENT IF NOT EXISTS e ON SCHEDULE EVERY 1 DAY DO SELECT 1",
            MY,
        ));
        text_refusal(parse_header(
            "CREATE OR REPLACE FUNCTION IF NOT EXISTS f() RETURNS INT RETURN 1",
            MY,
        ));
        text_refusal(parse_header("CREATE TEMP VIEW v AS SELECT 1", PG));
        text_refusal(parse_header(
            "CREATE OR REPLACE TEMPORARY VIEW v AS SELECT 1",
            PG,
        ));
        let m = text_refusal(parse_header(
            "CREATE FUNCTION metaphon RETURNS STRING SONAME 'udf_example.so'",
            MY,
        ));
        assert_eq!(m, "Loadable functions cannot be edited here.");
        text_refusal(parse_header(
            "CREATE AGGREGATE FUNCTION a RETURNS INTEGER SONAME 'x.so'",
            MY,
        ));
        match parse_header(
            "CREATE OR REPLACE MATERIALIZED VIEW public.mv AS SELECT 1",
            PG,
        ) {
            Err(DbError::Unsupported { feature, version }) => {
                assert_eq!(feature, "CREATE OR REPLACE MATERIALIZED VIEW");
                assert_eq!(version, "PostgreSQL (none)");
            }
            other => panic!("{other:?}"),
        }
        for (s, d) in [
            ("SELECT 1", MY),
            ("SELECT 1", PG),
            ("CREATE TABLE t (a int)", MY),
            ("CREATE TABLE public.t (a int)", PG),
            ("CREATE VIEW (a) AS SELECT 1", PG),
            ("CREATE VIEW 'v' AS SELECT 1", MY),
            ("CREATE VIEW $$v$$ AS SELECT 1", PG),
            ("CREATE VIEW", MY),
            ("CREATE VIEW a.b.c AS SELECT 1", PG),
            ("CREATE TRIGGER t BEFORE INSERT", MY),
            (
                "CREATE TRIGGER t BEFORE INSERT ON 'x' FOR EACH ROW SET @a = 1",
                MY,
            ),
            (
                "CREATE EVENT TRIGGER et ON ddl_command_start EXECUTE FUNCTION f()",
                PG,
            ),
        ] {
            text_refusal(parse_header(s, d));
        }
    }

    fn target(kind: ObjectKind, schema: Option<&str>, name: Option<&str>) -> EditTarget {
        EditTarget {
            kind,
            database: "db".into(),
            schema: schema.map(Into::into),
            name: name.map(Into::into),
            table: None,
        }
    }

    fn check(stmt: &str, d: Dialect, t: &EditTarget, s: &ServerInfo) -> Result<(), DbError> {
        check_target(&parse(stmt, d), t, s)
    }

    #[test]
    fn check_target_accepts_the_edited_object() {
        let t = target(ObjectKind::Procedure, None, Some("p"));
        check("CREATE PROCEDURE p() BEGIN END", MY, &t, &mysql8()).unwrap();
        check("CREATE PROCEDURE `db`.`p`() BEGIN END", MY, &t, &mysql8()).unwrap();
        check(
            "CREATE OR REPLACE PROCEDURE p() BEGIN END",
            MY,
            &t,
            &mariadb(),
        )
        .unwrap();
        let v = target(ObjectKind::View, None, Some("v"));
        check("CREATE OR REPLACE VIEW v AS SELECT 1", MY, &v, &mysql57()).unwrap();
        let f = target(ObjectKind::Function, Some("public"), Some("f"));
        check(
            "CREATE OR REPLACE FUNCTION public.f() RETURNS int AS 'SELECT 1' LANGUAGE sql",
            PG,
            &f,
            &pg12(),
        )
        .unwrap();
        let mut trg = target(ObjectKind::Trigger, Some("public"), Some("trg"));
        trg.table = Some("orders".into());
        check(
            "CREATE OR REPLACE TRIGGER trg BEFORE INSERT ON orders FOR EACH ROW EXECUTE FUNCTION public.f()",
            PG,
            &trg,
            &pg17(),
        )
        .unwrap();
        // New objects.
        let new = target(ObjectKind::Procedure, None, None);
        check("CREATE PROCEDURE np() BEGIN END", MY, &new, &mysql8()).unwrap();
        let mut new_trg = target(ObjectKind::Trigger, Some("public"), None);
        check(
            "CREATE TRIGGER nt BEFORE INSERT ON public.orders FOR EACH ROW EXECUTE FUNCTION public.f()",
            PG,
            &new_trg,
            &pg17(),
        )
        .unwrap();
        new_trg.table = Some("orders".into());
        check(
            "CREATE TRIGGER nt BEFORE INSERT ON public.orders FOR EACH ROW EXECUTE FUNCTION public.f()",
            PG,
            &new_trg,
            &pg17(),
        )
        .unwrap();
    }

    #[test]
    fn check_target_refuses_other_objects() {
        let p = target(ObjectKind::Procedure, None, Some("p"));
        let m = text_refusal(check("CREATE PROCEDURE q() BEGIN END", MY, &p, &mysql8()));
        assert!(
            m.contains("renaming through the DDL editor is not supported"),
            "{m}"
        );
        assert!(m.contains("`q`"), "{m}");
        // Other database / schema.
        text_refusal(check(
            "CREATE PROCEDURE other.p() BEGIN END",
            MY,
            &p,
            &mysql8(),
        ));
        let f = target(ObjectKind::Function, Some("public"), Some("f"));
        text_refusal(check(
            "CREATE OR REPLACE FUNCTION other.f() RETURNS int AS 'SELECT 1' LANGUAGE sql",
            PG,
            &f,
            &pg17(),
        ));
        // Unqualified PostgreSQL name.
        let m = text_refusal(check(
            "CREATE OR REPLACE FUNCTION f() RETURNS int AS 'SELECT 1' LANGUAGE sql",
            PG,
            &f,
            &pg17(),
        ));
        assert_eq!(m, "Qualify the name with its schema: public.f");
        // Other kind.
        let mv = target(ObjectKind::MaterializedView, Some("public"), Some("v"));
        let m = text_refusal(check("CREATE VIEW public.v AS SELECT 1", PG, &mv, &pg17()));
        assert_eq!(m, "The text defines a view, not a materialized view.");
        let m = text_refusal(check(
            "CREATE FUNCTION p() RETURNS INT RETURN 1",
            MY,
            &p,
            &mysql8(),
        ));
        assert_eq!(m, "The text defines a function, not a procedure.");
        // Trigger on another table.
        let mut trg = target(ObjectKind::Trigger, None, Some("trg"));
        trg.table = Some("orders".into());
        text_refusal(check(
            "CREATE TRIGGER trg BEFORE INSERT ON items FOR EACH ROW SET NEW.a = 1",
            MY,
            &trg,
            &mysql8(),
        ));
        text_refusal(check(
            "CREATE TRIGGER trg BEFORE INSERT ON other.orders FOR EACH ROW SET NEW.a = 1",
            MY,
            &trg,
            &mysql8(),
        ));
        let mut pg_trg = target(ObjectKind::Trigger, Some("public"), Some("trg"));
        pg_trg.table = Some("orders".into());
        text_refusal(check(
            "CREATE TRIGGER trg BEFORE INSERT ON other.orders FOR EACH ROW EXECUTE FUNCTION public.f()",
            PG,
            &pg_trg,
            &pg17(),
        ));
        text_refusal(check(
            "CREATE TRIGGER public.trg BEFORE INSERT ON orders FOR EACH ROW EXECUTE FUNCTION public.f()",
            PG,
            &pg_trg,
            &pg17(),
        ));
        // OR REPLACE on MySQL (not MariaDB) routines, triggers and events.
        for (s, kind, feature) in [
            (
                "CREATE OR REPLACE PROCEDURE p() BEGIN END",
                ObjectKind::Procedure,
                "CREATE OR REPLACE PROCEDURE",
            ),
            (
                "CREATE OR REPLACE FUNCTION p() RETURNS INT RETURN 1",
                ObjectKind::Function,
                "CREATE OR REPLACE FUNCTION",
            ),
            (
                "CREATE OR REPLACE EVENT p ON SCHEDULE EVERY 1 DAY DO SELECT 1",
                ObjectKind::Event,
                "CREATE OR REPLACE EVENT",
            ),
        ] {
            for server in [mysql8(), mysql57()] {
                match check(s, MY, &target(kind, None, Some("p")), &server) {
                    Err(DbError::Unsupported {
                        feature: f,
                        version,
                    }) => {
                        assert_eq!(f, feature);
                        assert_eq!(version, "MariaDB");
                    }
                    other => panic!("{s}: {other:?}"),
                }
            }
        }
        match check(
            "CREATE OR REPLACE TRIGGER trg BEFORE INSERT ON orders FOR EACH ROW SET NEW.a = 1",
            MY,
            &trg,
            &mysql8(),
        ) {
            Err(DbError::Unsupported { feature, .. }) => {
                assert_eq!(feature, "CREATE OR REPLACE TRIGGER")
            }
            other => panic!("{other:?}"),
        }
        // PostgreSQL 12 has no CREATE OR REPLACE TRIGGER.
        match check(
            "CREATE OR REPLACE TRIGGER trg BEFORE INSERT ON orders FOR EACH ROW EXECUTE FUNCTION public.f()",
            PG,
            &pg_trg,
            &pg12(),
        ) {
            Err(DbError::Unsupported { feature, version }) => {
                assert_eq!(feature, "CREATE OR REPLACE TRIGGER");
                assert_eq!(version, "PostgreSQL 14");
            }
            other => panic!("{other:?}"),
        }
        // New object with OR REPLACE.
        let new = target(ObjectKind::View, None, None);
        let m = text_refusal(check(
            "CREATE OR REPLACE VIEW nv AS SELECT 1",
            MY,
            &new,
            &mariadb(),
        ));
        assert_eq!(m, "Remove OR REPLACE: the object is new.");
        // New object with an invalid name.
        text_refusal(check("CREATE VIEW `x😀` AS SELECT 1", MY, &new, &mysql8()));
        text_refusal(check("CREATE VIEW `a.b` AS SELECT 1", MY, &new, &mysql8()));
        let new_pg = target(ObjectKind::View, Some("public"), None);
        text_refusal(check(
            &format!("CREATE VIEW public.{} AS SELECT 1", "a".repeat(64)),
            PG,
            &new_pg,
            &pg17(),
        ));
        // New PostgreSQL objects need a schema, and new triggers a qualified table.
        text_refusal(check("CREATE VIEW nv AS SELECT 1", PG, &new_pg, &pg17()));
        let new_trg = target(ObjectKind::Trigger, Some("public"), None);
        text_refusal(check(
            "CREATE TRIGGER nt BEFORE INSERT ON orders FOR EACH ROW EXECUTE FUNCTION public.f()",
            PG,
            &new_trg,
            &pg17(),
        ));
        // Engine kinds.
        let ev = target(ObjectKind::MaterializedView, None, Some("v"));
        assert!(check("CREATE MATERIALIZED VIEW v AS SELECT 1", MY, &ev, &mysql8()).is_err());
    }
}
