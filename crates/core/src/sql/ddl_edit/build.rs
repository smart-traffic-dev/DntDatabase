//! Spec §5.2: the statements that apply an accepted edit, per engine, kind and
//! strategy, with their warnings and (PostgreSQL) the post-checks run before COMMIT.

use super::{kind_label, EditTarget, Header, Strategy};
use crate::error::DbError;
use crate::meta::ObjectKind;
use crate::server::{DbKind, ServerInfo, ServerVariant};
use crate::sql::ident::{qualified_name, quote_ident};
use serde::{Deserialize, Serialize};

/// What the catalog knows about the edited object (read by the backend at open).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogFacts {
    /// PostgreSQL routines: `pg_get_function_identity_arguments`.
    pub identity_args: Option<String>,
    pub has_comment: bool,
    pub has_grants: bool,
    /// PostgreSQL materialized views: `pg_get_indexdef` of every index, in order.
    pub index_defs: Vec<String>,
    /// Triggers: the table.
    pub table: Option<String>,
    /// PostgreSQL triggers: the table's oid.
    pub table_oid: Option<u32>,
    /// MySQL triggers: e.g. `BEFORE INSERT`.
    pub timing_event: Option<String>,
    /// MySQL triggers: other triggers on the table with the same timing and event.
    pub same_timing_triggers: u32,
    /// PostgreSQL views and materialized views: `pg_class.reloptions` (`key=value`).
    #[serde(default)]
    pub reloptions: Vec<String>,
    /// PostgreSQL views: `pg_get_triggerdef` of every user trigger, in order.
    #[serde(default)]
    pub trigger_defs: Vec<String>,
    /// PostgreSQL views: `pg_get_ruledef` of every rule other than `_RETURN`, in order.
    #[serde(default)]
    pub rule_defs: Vec<String>,
}

/// A read run inside the PostgreSQL transaction before COMMIT: one text value that
/// must equal `expect`, else the transaction is rolled back with `failure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostCheck {
    pub sql: String,
    pub params: Vec<String>,
    pub expect: String,
    pub failure: String,
}

/// The statements of an edit, as previewed and sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    pub statements: Vec<String>,
    pub warnings: Vec<String>,
    /// PostgreSQL: the statements run in one transaction.
    pub transactional: bool,
    /// MySQL: the object does not exist between a DROP and the CREATE.
    pub gap: bool,
    pub checks: Vec<PostCheck>,
    /// One sentence per check, for the preview.
    pub check_notes: Vec<String>,
}

/// Everything `build` needs; `stmt` and `header` come from `isolate` + `parse_header`
/// and were accepted by `check_target`.
pub struct BuildInput<'a> {
    pub target: &'a EditTarget,
    pub stmt: &'a str,
    pub header: &'a Header,
    pub strategy: Option<Strategy>,
    pub server: &'a ServerInfo,
    pub facts: &'a CatalogFacts,
    pub rehearsal_name: &'a str,
}

/// Every plan's last warning (the executor bounds each lock wait).
const LOCK_WARNING: &str = "Each lock wait is limited to 5 seconds.";
const RESTRICT_WARNING: &str =
    "A RESTRICT drop fails if other objects depend on it; the transaction is then rolled back.";
const MYSQL_ROUTINE_GRANTS: &str =
    "Privileges granted on the routine may be revoked when it is dropped.";
const MV_REFILL: &str =
    "The materialized view is filled again WITH DATA; it holds an exclusive lock while it runs.";
const REPLACE_SIGNATURE_FAILURE: &str = "The argument list changed, so CREATE OR REPLACE would add a second routine instead of replacing it. Use Drop and re-create to change the signature.";

const ROUTINE_CHECK_SQL: &str = "SELECT count(*)::text || ':' || coalesce(min(p.prokind::text), '') FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = $1 AND p.proname = $2";
const RELATION_CHECK_SQL: &str = "SELECT coalesce((SELECT c.relkind::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2), '')";
const TRIGGER_EDIT_CHECK_SQL: &str = "SELECT count(*)::text FROM pg_trigger t WHERE t.tgrelid = $1::oid AND t.tgname = $2 AND NOT t.tgisinternal";
const TRIGGER_NEW_CHECK_SQL: &str = "SELECT count(*)::text FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2 AND t.tgname = $3 AND NOT t.tgisinternal";

/// The strategies offered for the target on `server` (none for a new object).
pub fn strategies(t: &EditTarget, server: &ServerInfo) -> Vec<Strategy> {
    use ObjectKind::*;
    if t.name.is_none() {
        return Vec::new();
    }
    match server.kind {
        DbKind::Postgres => match t.kind {
            View | Function | Procedure => vec![Strategy::Replace, Strategy::Recreate],
            MaterializedView | Trigger => vec![Strategy::Recreate],
            _ => Vec::new(),
        },
        DbKind::MySql => match t.kind {
            View => vec![Strategy::Replace],
            Function | Procedure | Trigger | Event => {
                if server.variant == ServerVariant::MariaDb {
                    vec![Strategy::Replace]
                } else {
                    vec![Strategy::Recreate]
                }
            }
            _ => Vec::new(),
        },
    }
}

/// Builds the statements of spec §5.2.
pub fn build(input: &BuildInput) -> Result<Built, DbError> {
    let BuildInput {
        target: t,
        stmt,
        header: h,
        strategy,
        server,
        facts,
        rehearsal_name,
    } = *input;
    let pg = server.kind == DbKind::Postgres;
    let mut out = Built {
        statements: Vec::new(),
        warnings: Vec::new(),
        transactional: pg,
        gap: false,
        checks: Vec::new(),
        check_notes: Vec::new(),
    };
    let Some(name) = t.name.as_deref() else {
        // A new object: the text alone (check_target refused OR REPLACE).
        out.statements.push(stmt.to_string());
        if pg {
            let schema = pg_schema(t)?;
            new_check(&mut out, t, h, schema)?;
        }
        out.warnings.push(LOCK_WARNING.into());
        return Ok(out);
    };
    let strategy = strategy
        .filter(|s| strategies(t, server).contains(s))
        .ok_or_else(|| DbError::Config {
            field: Some("strategy".into()),
            message: format!(
                "This way of applying the change is not available for a {} on this server.",
                kind_label(t.kind)
            ),
        })?;
    let q = qualified_name(server.kind, &t.database, t.schema.as_deref(), name);
    let what = kind_label(t.kind);
    if pg {
        let schema = pg_schema(t)?;
        let mut dropped = true;
        match (t.kind, strategy) {
            (_, Strategy::Replace) => {
                dropped = false;
                out.statements.push(or_replace(stmt, h));
            }
            (ObjectKind::View, Strategy::Recreate) => {
                out.statements.push(format!("DROP VIEW {q} RESTRICT"));
                out.statements.push(stmt.to_string());
                // The drop takes the view's triggers and rules with it.
                out.statements.extend(
                    facts
                        .trigger_defs
                        .iter()
                        .chain(&facts.rule_defs)
                        .map(|d| d.trim_end().trim_end_matches(';').to_string()),
                );
            }
            (ObjectKind::MaterializedView, Strategy::Recreate) => {
                out.statements
                    .push(format!("DROP MATERIALIZED VIEW {q} RESTRICT"));
                out.statements.push(stmt.to_string());
                out.statements.extend(facts.index_defs.iter().cloned());
            }
            (ObjectKind::Function | ObjectKind::Procedure, Strategy::Recreate) => {
                let args = facts
                    .identity_args
                    .as_deref()
                    .ok_or_else(|| missing_fact("identity_args", "argument list"))?;
                out.statements
                    .push(format!("DROP {} {q}({args}) RESTRICT", kind_word(t.kind)));
                out.statements.push(stmt.to_string());
            }
            (ObjectKind::Trigger, Strategy::Recreate) => {
                let table = facts
                    .table
                    .as_deref()
                    .ok_or_else(|| missing_fact("table", "table"))?;
                out.statements.push(format!(
                    "DROP TRIGGER {} ON {}",
                    quote_ident(DbKind::Postgres, name),
                    qualified_name(DbKind::Postgres, &t.database, Some(schema), table)
                ));
                out.statements.push(stmt.to_string());
            }
            _ => unreachable!("strategies() offers no other PostgreSQL edit"),
        }
        // Post-check.
        match t.kind {
            ObjectKind::Function | ObjectKind::Procedure => {
                let failure = match strategy {
                    Strategy::Replace => REPLACE_SIGNATURE_FAILURE.to_string(),
                    Strategy::Recreate => not_one_routine(name),
                };
                routine_check(&mut out, t.kind, schema, name, failure);
            }
            ObjectKind::View | ObjectKind::MaterializedView => {
                relation_check(&mut out, t.kind, schema, name);
            }
            ObjectKind::Trigger => {
                let table = facts.table.as_deref().unwrap_or_default();
                let oid = facts
                    .table_oid
                    .ok_or_else(|| missing_fact("table_oid", "table"))?;
                out.checks.push(PostCheck {
                    sql: TRIGGER_EDIT_CHECK_SQL.into(),
                    params: vec![oid.to_string(), name.to_string()],
                    expect: "1".into(),
                    failure: format!("The trigger was not re-created on {table}."),
                });
                out.check_notes
                    .push(format!("Before COMMIT: trigger {name} exists on {table}."));
            }
            _ => {}
        }
        // Warnings.
        if matches!(t.kind, ObjectKind::View | ObjectKind::MaterializedView)
            && !facts.reloptions.is_empty()
        {
            // CREATE OR REPLACE VIEW replaces the options too; a re-create starts empty.
            out.warnings.push(format!(
                "The {what}'s current options are {}; the text sets them, and an option it leaves out is reset.",
                facts.reloptions.join(", ")
            ));
        }
        if dropped {
            if t.kind != ObjectKind::Trigger {
                out.warnings.push(RESTRICT_WARNING.into());
            }
            if facts.has_comment {
                out.warnings.push(format!(
                    "The {what} loses its comment: drop and re-create does not re-apply it."
                ));
            }
            if facts.has_grants && t.kind != ObjectKind::Trigger {
                out.warnings.push(format!(
                    "The {what} loses its grants: drop and re-create does not re-apply them; grant them again."
                ));
            }
            if t.kind == ObjectKind::MaterializedView {
                out.warnings.push(MV_REFILL.into());
            }
            if t.kind == ObjectKind::View {
                let count = |n: usize, one: &str| match n {
                    0 => None,
                    1 => Some(format!("1 {one}")),
                    n => Some(format!("{n} {one}s")),
                };
                let parts: Vec<String> = [
                    count(facts.trigger_defs.len(), "trigger"),
                    count(facts.rule_defs.len(), "rule"),
                ]
                .into_iter()
                .flatten()
                .collect();
                if !parts.is_empty() {
                    let (verb, def, fit) = if facts.trigger_defs.len() + facts.rule_defs.len() == 1
                    {
                        ("is", "its current definition", "it no longer fits")
                    } else {
                        ("are", "their current definitions", "they no longer fit")
                    };
                    out.warnings.push(format!(
                        "The view's {} {verb} re-created after it from {def}; the change is rolled back if {fit} the new view.",
                        parts.join(" and ")
                    ));
                }
            }
        }
    } else {
        match strategy {
            Strategy::Replace => out.statements.push(or_replace(stmt, h)),
            Strategy::Recreate => {
                out.gap = true;
                if matches!(t.kind, ObjectKind::Function | ObjectKind::Procedure) {
                    // Rehearsal (R5): the same text under a temporary name.
                    let rehearsal = quote_ident(DbKind::MySql, rehearsal_name);
                    out.statements.push(format!(
                        "{}{rehearsal}{}",
                        &stmt[..h.name_span.0],
                        &stmt[h.name_span.1..]
                    ));
                    out.statements
                        .push(format!("DROP {} {rehearsal}", kind_word(t.kind)));
                }
                out.statements
                    .push(format!("DROP {} {q}", kind_word(t.kind)));
                out.statements.push(stmt.to_string());
            }
        }
        let table = facts.table.as_deref().or(t.table.as_deref());
        if out.gap {
            out.warnings.push(match t.kind {
                ObjectKind::Trigger => match table {
                    Some(table) => format!(
                        "Between the DROP and the CREATE the trigger does not exist; writes to `{table}` do not fire it."
                    ),
                    None => "Between the DROP and the CREATE the trigger does not exist; writes to its table do not fire it.".into(),
                },
                ObjectKind::Event => "Between the DROP and the CREATE the event does not exist; a scheduled run may be missed.".into(),
                _ => format!(
                    "Between the DROP and the CREATE the {what} does not exist; calls to it fail."
                ),
            });
        }
        if t.kind == ObjectKind::Trigger && facts.same_timing_triggers > 0 {
            let others = match facts.timing_event.as_deref() {
                Some(te) => format!("Other {te} triggers"),
                None => "Other triggers with the same timing and event".into(),
            };
            let on = match table {
                Some(table) => format!("`{table}`"),
                None => "the table".into(),
            };
            out.warnings.push(format!(
                "{others} exist on {on}; the re-created trigger fires after them."
            ));
        }
        if out.gap && matches!(t.kind, ObjectKind::Function | ObjectKind::Procedure) {
            out.warnings.push(MYSQL_ROUTINE_GRANTS.into());
        }
    }
    out.warnings.push(LOCK_WARNING.into());
    Ok(out)
}

/// `stmt` with `OR REPLACE` after `CREATE`, unless it already says so.
fn or_replace(stmt: &str, h: &Header) -> String {
    if h.or_replace {
        stmt.to_string()
    } else {
        format!(
            "{} OR REPLACE{}",
            &stmt[..h.create_end],
            &stmt[h.create_end..]
        )
    }
}

fn pg_schema(t: &EditTarget) -> Result<&str, DbError> {
    t.schema.as_deref().ok_or_else(|| DbError::Config {
        field: Some("schema".into()),
        message: "A PostgreSQL schema is required.".into(),
    })
}

fn missing_fact(field: &str, what: &str) -> DbError {
    DbError::Config {
        field: Some(field.into()),
        message: format!("The object's {what} is not known; reopen the editor."),
    }
}

fn not_one_routine(name: &str) -> String {
    format!("After the change there is not exactly one routine named {name}.")
}

fn routine_check(out: &mut Built, kind: ObjectKind, schema: &str, name: &str, failure: String) {
    let prokind = if kind == ObjectKind::Procedure {
        "p"
    } else {
        "f"
    };
    out.checks.push(PostCheck {
        sql: ROUTINE_CHECK_SQL.into(),
        params: vec![schema.to_string(), name.to_string()],
        expect: format!("1:{prokind}"),
        failure,
    });
    out.check_notes.push(format!(
        "Before COMMIT: exactly one routine named {name} exists."
    ));
}

fn relation_check(out: &mut Built, kind: ObjectKind, schema: &str, name: &str) {
    let (relkind, what) = if kind == ObjectKind::MaterializedView {
        ("m", "a materialized view")
    } else {
        ("v", "a view")
    };
    out.checks.push(PostCheck {
        sql: RELATION_CHECK_SQL.into(),
        params: vec![schema.to_string(), name.to_string()],
        expect: relkind.into(),
        failure: format!("After the change {name} is not {what}."),
    });
    out.check_notes
        .push(format!("Before COMMIT: {name} is {what}."));
}

/// The post-check of a new PostgreSQL object: it exists in the target schema.
fn new_check(out: &mut Built, t: &EditTarget, h: &Header, schema: &str) -> Result<(), DbError> {
    let name = h.name.as_str();
    match t.kind {
        ObjectKind::Function | ObjectKind::Procedure => {
            routine_check(out, t.kind, schema, name, not_one_routine(name))
        }
        ObjectKind::View | ObjectKind::MaterializedView => {
            relation_check(out, t.kind, schema, name)
        }
        ObjectKind::Trigger => {
            let (_, table) = h
                .on_table
                .as_ref()
                .ok_or_else(|| super::text_error("Write the trigger's table after ON."))?;
            out.checks.push(PostCheck {
                sql: TRIGGER_NEW_CHECK_SQL.into(),
                params: vec![schema.to_string(), table.clone(), name.to_string()],
                expect: "1".into(),
                failure: format!("The trigger was not created on {table}."),
            });
            out.check_notes
                .push(format!("Before COMMIT: trigger {name} exists on {table}."));
        }
        _ => {}
    }
    Ok(())
}

fn kind_word(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Trigger => "TRIGGER",
        ObjectKind::Event => "EVENT",
        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
        _ => "VIEW",
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_servers::*;
    use super::super::{check_target, isolate, parse_header};
    use super::*;
    use crate::meta::ObjectKind;
    use crate::sql::lexer::Dialect;

    const REH: &str = "_dnt_p4c_check_0a1b2c3d";

    fn dialect(s: &ServerInfo) -> Dialect {
        match s.kind {
            crate::server::DbKind::MySql => Dialect::MYSQL,
            crate::server::DbKind::Postgres => Dialect::Postgres,
        }
    }

    fn my_target(kind: ObjectKind, name: Option<&str>) -> EditTarget {
        EditTarget {
            kind,
            database: "db".into(),
            schema: None,
            name: name.map(Into::into),
            table: None,
        }
    }

    fn pg_target(kind: ObjectKind, name: Option<&str>) -> EditTarget {
        EditTarget {
            kind,
            database: "app".into(),
            schema: Some("public".into()),
            name: name.map(Into::into),
            table: None,
        }
    }

    /// Runs the full validation chain, then `build`.
    fn run(
        t: &EditTarget,
        text: &str,
        strategy: Option<Strategy>,
        server: &ServerInfo,
        facts: &CatalogFacts,
    ) -> Result<Built, DbError> {
        let d = dialect(server);
        let stmt = isolate(text, t.kind, d, server).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        let header = parse_header(&stmt, d).unwrap();
        check_target(&header, t, server).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        build(&BuildInput {
            target: t,
            stmt: &stmt,
            header: &header,
            strategy,
            server,
            facts,
            rehearsal_name: REH,
        })
    }

    fn ok(
        t: &EditTarget,
        text: &str,
        strategy: Option<Strategy>,
        server: &ServerInfo,
        facts: &CatalogFacts,
    ) -> Built {
        run(t, text, strategy, server, facts).unwrap_or_else(|e| panic!("{text}: {e:?}"))
    }

    const LOCK: &str = "Each lock wait is limited to 5 seconds.";
    const RESTRICT: &str =
        "A RESTRICT drop fails if other objects depend on it; the transaction is then rolled back.";
    const ROUTINE_SQL: &str = "SELECT count(*)::text || ':' || coalesce(min(p.prokind::text), '') FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = $1 AND p.proname = $2";
    const REL_SQL: &str = "SELECT coalesce((SELECT c.relkind::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2), '')";
    const TRG_SQL: &str = "SELECT count(*)::text FROM pg_trigger t WHERE t.tgrelid = $1::oid AND t.tgname = $2 AND NOT t.tgisinternal";

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // --- §5.2 rows ---------------------------------------------------------------

    #[test]
    fn build_rows_mysql_view_edit() {
        let t = my_target(ObjectKind::View, Some("v"));
        for s in [mysql57(), mysql8(), mariadb()] {
            let b = ok(
                &t,
                "CREATE ALGORITHM=UNDEFINED VIEW `db`.`v` AS SELECT 1;",
                Some(Strategy::Replace),
                &s,
                &CatalogFacts::default(),
            );
            assert_eq!(
                b.statements,
                strs(&["CREATE OR REPLACE ALGORITHM=UNDEFINED VIEW `db`.`v` AS SELECT 1"])
            );
            assert!(!b.gap && !b.transactional);
            assert!(b.checks.is_empty() && b.check_notes.is_empty());
            assert_eq!(b.warnings, strs(&[LOCK]));
        }
    }

    #[test]
    fn build_rows_mysql_routine_edit() {
        let t = my_target(ObjectKind::Procedure, Some("p"));
        let text =
            "CREATE DEFINER=`root`@`%` PROCEDURE `db`.`p`(IN a INT)\nBEGIN\n  SELECT a;\nEND";
        for s in [mysql57(), mysql8()] {
            let b = ok(
                &t,
                text,
                Some(Strategy::Recreate),
                &s,
                &CatalogFacts::default(),
            );
            assert_eq!(
                b.statements,
                strs(&[
                    "CREATE DEFINER=`root`@`%` PROCEDURE `_dnt_p4c_check_0a1b2c3d`(IN a INT)\nBEGIN\n  SELECT a;\nEND",
                    "DROP PROCEDURE `_dnt_p4c_check_0a1b2c3d`",
                    "DROP PROCEDURE `db`.`p`",
                    text,
                ])
            );
            assert!(b.gap && !b.transactional);
            assert!(b.checks.is_empty());
            assert_eq!(
                b.warnings,
                strs(&[
                    "Between the DROP and the CREATE the procedure does not exist; calls to it fail.",
                    "Privileges granted on the routine may be revoked when it is dropped.",
                    LOCK,
                ])
            );
        }
        let f = my_target(ObjectKind::Function, Some("f"));
        let text = "CREATE FUNCTION f(a INT) RETURNS INT DETERMINISTIC RETURN a";
        let b = ok(
            &f,
            text,
            Some(Strategy::Recreate),
            &mysql8(),
            &CatalogFacts::default(),
        );
        assert_eq!(
            b.statements,
            strs(&[
                "CREATE FUNCTION `_dnt_p4c_check_0a1b2c3d`(a INT) RETURNS INT DETERMINISTIC RETURN a",
                "DROP FUNCTION `_dnt_p4c_check_0a1b2c3d`",
                "DROP FUNCTION `db`.`f`",
                text,
            ])
        );
        assert!(b.gap);
        assert!(b.warnings[0].contains("the function does not exist"));
    }

    #[test]
    fn build_rows_mysql_trigger_and_event_edit() {
        let mut t = my_target(ObjectKind::Trigger, Some("trg"));
        t.table = Some("orders".into());
        let text = "CREATE TRIGGER `trg` BEFORE INSERT ON `orders` FOR EACH ROW SET NEW.a = 1";
        let facts = CatalogFacts {
            table: Some("orders".into()),
            ..Default::default()
        };
        let b = ok(&t, text, Some(Strategy::Recreate), &mysql8(), &facts);
        assert_eq!(b.statements, strs(&["DROP TRIGGER `db`.`trg`", text]));
        assert!(b.gap && !b.transactional && b.checks.is_empty());
        assert_eq!(
            b.warnings,
            strs(&[
                "Between the DROP and the CREATE the trigger does not exist; writes to `orders` do not fire it.",
                LOCK,
            ])
        );

        let e = my_target(ObjectKind::Event, Some("e"));
        let text = "CREATE EVENT e ON SCHEDULE EVERY 1 DAY DO DELETE FROM t";
        let b = ok(
            &e,
            text,
            Some(Strategy::Recreate),
            &mysql57(),
            &CatalogFacts::default(),
        );
        assert_eq!(b.statements, strs(&["DROP EVENT `db`.`e`", text]));
        assert!(b.gap);
        assert_eq!(
            b.warnings,
            strs(&[
                "Between the DROP and the CREATE the event does not exist; a scheduled run may be missed.",
                LOCK,
            ])
        );
    }

    #[test]
    fn build_rows_mariadb_routine_trigger_event_edit() {
        let mut trg = my_target(ObjectKind::Trigger, Some("trg"));
        trg.table = Some("orders".into());
        for (t, text, want) in [
            (
                my_target(ObjectKind::Procedure, Some("p")),
                "CREATE PROCEDURE p() BEGIN SELECT 1; END",
                "CREATE OR REPLACE PROCEDURE p() BEGIN SELECT 1; END",
            ),
            (
                my_target(ObjectKind::Function, Some("f")),
                "CREATE OR REPLACE FUNCTION f() RETURNS INT RETURN 1",
                "CREATE OR REPLACE FUNCTION f() RETURNS INT RETURN 1",
            ),
            (
                trg,
                "CREATE TRIGGER trg BEFORE INSERT ON orders FOR EACH ROW SET NEW.a = 1",
                "CREATE OR REPLACE TRIGGER trg BEFORE INSERT ON orders FOR EACH ROW SET NEW.a = 1",
            ),
            (
                my_target(ObjectKind::Event, Some("e")),
                "CREATE EVENT e ON SCHEDULE EVERY 1 DAY DO SELECT 1",
                "CREATE OR REPLACE EVENT e ON SCHEDULE EVERY 1 DAY DO SELECT 1",
            ),
        ] {
            let b = ok(
                &t,
                text,
                Some(Strategy::Replace),
                &mariadb(),
                &CatalogFacts::default(),
            );
            assert_eq!(b.statements, strs(&[want]), "{text}");
            assert!(!b.gap && !b.transactional && b.checks.is_empty());
            assert_eq!(b.warnings, strs(&[LOCK]));
        }
    }

    #[test]
    fn build_rows_pg_replace() {
        // View.
        let v = pg_target(ObjectKind::View, Some("v"));
        let b = ok(
            &v,
            "CREATE VIEW public.v AS SELECT 1",
            Some(Strategy::Replace),
            &pg17(),
            &CatalogFacts::default(),
        );
        assert_eq!(
            b.statements,
            strs(&["CREATE OR REPLACE VIEW public.v AS SELECT 1"])
        );
        assert!(b.transactional && !b.gap);
        assert_eq!(b.checks.len(), 1);
        assert_eq!(b.checks[0].sql, REL_SQL);
        assert_eq!(b.checks[0].params, strs(&["public", "v"]));
        assert_eq!(b.checks[0].expect, "v");
        assert_eq!(b.check_notes, strs(&["Before COMMIT: v is a view."]));
        assert_eq!(b.warnings, strs(&[LOCK]));

        // Function and procedure.
        let replace_failure = "The argument list changed, so CREATE OR REPLACE would add a second routine instead of replacing it. Use Drop and re-create to change the signature.";
        for (kind, text, want, expect) in [
            (
                ObjectKind::Function,
                "CREATE OR REPLACE FUNCTION public.f(a integer) RETURNS integer LANGUAGE sql AS $$ SELECT a $$",
                "CREATE OR REPLACE FUNCTION public.f(a integer) RETURNS integer LANGUAGE sql AS $$ SELECT a $$",
                "1:f",
            ),
            (
                ObjectKind::Procedure,
                "CREATE PROCEDURE public.f(a integer) LANGUAGE sql AS $$ SELECT a $$",
                "CREATE OR REPLACE PROCEDURE public.f(a integer) LANGUAGE sql AS $$ SELECT a $$",
                "1:p",
            ),
        ] {
            let t = pg_target(kind, Some("f"));
            for s in [pg12(), pg17()] {
                let b = ok(&t, text, Some(Strategy::Replace), &s, &CatalogFacts::default());
                assert_eq!(b.statements, strs(&[want]));
                assert!(b.transactional && !b.gap);
                assert_eq!(
                    b.checks,
                    vec![PostCheck {
                        sql: ROUTINE_SQL.into(),
                        params: strs(&["public", "f"]),
                        expect: expect.into(),
                        failure: replace_failure.into(),
                    }]
                );
                assert_eq!(
                    b.check_notes,
                    strs(&["Before COMMIT: exactly one routine named f exists."])
                );
                assert_eq!(b.warnings, strs(&[LOCK]));
            }
        }
    }

    #[test]
    fn build_rows_pg_recreate() {
        let v = pg_target(ObjectKind::View, Some("v"));
        let b = ok(
            &v,
            "CREATE VIEW public.v AS SELECT 1 AS a, 2 AS b",
            Some(Strategy::Recreate),
            &pg17(),
            &CatalogFacts::default(),
        );
        assert_eq!(
            b.statements,
            strs(&[
                "DROP VIEW \"public\".\"v\" RESTRICT",
                "CREATE VIEW public.v AS SELECT 1 AS a, 2 AS b",
            ])
        );
        assert!(b.transactional && !b.gap);
        assert_eq!(b.checks[0].sql, REL_SQL);
        assert_eq!(b.checks[0].expect, "v");
        assert_eq!(b.warnings, strs(&[RESTRICT, LOCK]));

        let facts = CatalogFacts {
            identity_args: Some("a integer, b text".into()),
            ..Default::default()
        };
        for (kind, word, expect) in [
            (ObjectKind::Function, "FUNCTION", "1:f"),
            (ObjectKind::Procedure, "PROCEDURE", "1:p"),
        ] {
            let t = pg_target(kind, Some("f"));
            let text = match kind {
                ObjectKind::Function => {
                    "CREATE FUNCTION public.f(a integer) RETURNS integer LANGUAGE sql AS $$ SELECT a $$"
                }
                _ => "CREATE PROCEDURE public.f(a integer) LANGUAGE sql AS $$ SELECT a $$",
            };
            let b = ok(&t, text, Some(Strategy::Recreate), &pg12(), &facts);
            assert_eq!(
                b.statements,
                vec![
                    format!("DROP {word} \"public\".\"f\"(a integer, b text) RESTRICT"),
                    text.to_string(),
                ]
            );
            assert_eq!(
                b.checks,
                vec![PostCheck {
                    sql: ROUTINE_SQL.into(),
                    params: strs(&["public", "f"]),
                    expect: expect.into(),
                    failure: "After the change there is not exactly one routine named f.".into(),
                }]
            );
        }
        // No arguments: an empty list is still written.
        let t = pg_target(ObjectKind::Function, Some("f"));
        let none = CatalogFacts {
            identity_args: Some(String::new()),
            ..Default::default()
        };
        let b = ok(
            &t,
            "CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
            Some(Strategy::Recreate),
            &pg17(),
            &none,
        );
        assert_eq!(b.statements[0], "DROP FUNCTION \"public\".\"f\"() RESTRICT");
    }

    #[test]
    fn build_rows_pg_materialized_view() {
        let t = pg_target(ObjectKind::MaterializedView, Some("mv"));
        let facts = CatalogFacts {
            index_defs: strs(&[
                "CREATE UNIQUE INDEX mv_id ON public.mv USING btree (id)",
                "CREATE INDEX mv_name ON public.mv USING btree (name)",
            ]),
            ..Default::default()
        };
        let text = "CREATE MATERIALIZED VIEW public.mv AS SELECT 1 AS id, 'a' AS name WITH DATA";
        let b = ok(&t, text, Some(Strategy::Recreate), &pg17(), &facts);
        assert_eq!(
            b.statements,
            strs(&[
                "DROP MATERIALIZED VIEW \"public\".\"mv\" RESTRICT",
                text,
                "CREATE UNIQUE INDEX mv_id ON public.mv USING btree (id)",
                "CREATE INDEX mv_name ON public.mv USING btree (name)",
            ])
        );
        assert!(b.transactional && !b.gap);
        assert_eq!(b.checks[0].sql, REL_SQL);
        assert_eq!(b.checks[0].params, strs(&["public", "mv"]));
        assert_eq!(b.checks[0].expect, "m");
        assert_eq!(
            b.check_notes,
            strs(&["Before COMMIT: mv is a materialized view."])
        );
        assert!(b.warnings.contains(&RESTRICT.to_string()));
        assert!(b.warnings.iter().any(|w| w.contains("WITH DATA")));
    }

    #[test]
    fn build_rows_pg_trigger_edit() {
        let mut t = pg_target(ObjectKind::Trigger, Some("trg"));
        t.table = Some("orders".into());
        let facts = CatalogFacts {
            table: Some("orders".into()),
            table_oid: Some(16_384),
            ..Default::default()
        };
        let text =
            "CREATE TRIGGER trg BEFORE UPDATE ON orders FOR EACH ROW EXECUTE FUNCTION public.f()";
        for s in [pg12(), pg17()] {
            let b = ok(&t, text, Some(Strategy::Recreate), &s, &facts);
            assert_eq!(
                b.statements,
                strs(&["DROP TRIGGER \"trg\" ON \"public\".\"orders\"", text])
            );
            assert!(b.transactional && !b.gap);
            assert_eq!(
                b.checks,
                vec![PostCheck {
                    sql: TRG_SQL.into(),
                    params: strs(&["16384", "trg"]),
                    expect: "1".into(),
                    failure: "The trigger was not re-created on orders.".into(),
                }]
            );
            assert_eq!(
                b.check_notes,
                strs(&["Before COMMIT: trigger trg exists on orders."])
            );
            assert_eq!(b.warnings, strs(&[LOCK]));
        }
        // Missing facts are a configuration error, not a guess.
        let no_oid = CatalogFacts {
            table: Some("orders".into()),
            ..Default::default()
        };
        assert!(matches!(
            run(&t, text, Some(Strategy::Recreate), &pg17(), &no_oid),
            Err(DbError::Config { .. })
        ));
    }

    #[test]
    fn build_rows_new_objects() {
        // MySQL family: the text alone; a strategy is ignored.
        let t = my_target(ObjectKind::Procedure, None);
        for s in [mysql57(), mysql8(), mariadb()] {
            for strategy in [None, Some(Strategy::Recreate), Some(Strategy::Replace)] {
                let b = ok(
                    &t,
                    "CREATE PROCEDURE np() BEGIN END",
                    strategy,
                    &s,
                    &CatalogFacts::default(),
                );
                assert_eq!(b.statements, strs(&["CREATE PROCEDURE np() BEGIN END"]));
                assert!(!b.gap && !b.transactional && b.checks.is_empty());
                assert_eq!(b.warnings, strs(&[LOCK]));
            }
        }
        // PostgreSQL: the object exists afterwards.
        let f = pg_target(ObjectKind::Function, None);
        let b = ok(
            &f,
            "CREATE FUNCTION public.nf() RETURNS int LANGUAGE sql AS 'SELECT 1'",
            None,
            &pg17(),
            &CatalogFacts::default(),
        );
        assert!(b.transactional && !b.gap);
        assert_eq!(
            b.checks,
            vec![PostCheck {
                sql: ROUTINE_SQL.into(),
                params: strs(&["public", "nf"]),
                expect: "1:f".into(),
                failure: "After the change there is not exactly one routine named nf.".into(),
            }]
        );
        let mv = pg_target(ObjectKind::MaterializedView, None);
        let b = ok(
            &mv,
            "CREATE MATERIALIZED VIEW public.nmv AS SELECT 1",
            None,
            &pg17(),
            &CatalogFacts::default(),
        );
        assert_eq!(
            b.statements,
            strs(&["CREATE MATERIALIZED VIEW public.nmv AS SELECT 1"])
        );
        assert_eq!(b.checks[0].sql, REL_SQL);
        assert_eq!(b.checks[0].params, strs(&["public", "nmv"]));
        assert_eq!(b.checks[0].expect, "m");
        assert_eq!(b.warnings, strs(&[LOCK]));

        let trg = pg_target(ObjectKind::Trigger, None);
        let b = ok(
            &trg,
            "CREATE TRIGGER nt BEFORE INSERT ON public.orders FOR EACH ROW EXECUTE FUNCTION public.f()",
            None,
            &pg12(),
            &CatalogFacts::default(),
        );
        assert_eq!(b.checks.len(), 1);
        let c = &b.checks[0];
        assert!(
            c.sql.contains("pg_trigger")
                && c.sql.contains("pg_class")
                && c.sql.contains("pg_namespace")
        );
        assert!(c.sql.contains("$1") && c.sql.contains("$2") && c.sql.contains("$3"));
        assert_eq!(c.params, strs(&["public", "orders", "nt"]));
        assert_eq!(c.expect, "1");
    }

    // --- details -------------------------------------------------------------------

    #[test]
    fn or_replace_inserted_once() {
        let t = my_target(ObjectKind::View, Some("v"));
        let b = ok(
            &t,
            "/* keep */ create or replace view v as select 1",
            Some(Strategy::Replace),
            &mysql8(),
            &CatalogFacts::default(),
        );
        assert_eq!(
            b.statements,
            strs(&["/* keep */ create or replace view v as select 1"])
        );
        let b = ok(
            &t,
            "/* keep */ create view v as select 1",
            Some(Strategy::Replace),
            &mysql8(),
            &CatalogFacts::default(),
        );
        assert_eq!(
            b.statements,
            strs(&["/* keep */ create OR REPLACE view v as select 1"])
        );
    }

    #[test]
    fn mysql_rehearsal_renames_only_the_name() {
        let t = my_target(ObjectKind::Procedure, Some("p"));
        let text =
            "CREATE PROCEDURE `db`.`p`(IN p INT)\nBEGIN\n  -- p calls p\n  SELECT p FROM p;\nEND";
        let b = ok(
            &t,
            text,
            Some(Strategy::Recreate),
            &mysql8(),
            &CatalogFacts::default(),
        );
        assert_eq!(
            b.statements[0],
            "CREATE PROCEDURE `_dnt_p4c_check_0a1b2c3d`(IN p INT)\nBEGIN\n  -- p calls p\n  SELECT p FROM p;\nEND"
        );
        assert_eq!(b.statements[3], text);
    }

    #[test]
    fn pg_recreate_needs_identity_args() {
        let t = pg_target(ObjectKind::Function, Some("f"));
        let r = run(
            &t,
            "CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
            Some(Strategy::Recreate),
            &pg17(),
            &CatalogFacts::default(),
        );
        assert!(
            matches!(r, Err(DbError::Config { field: Some(ref f), .. }) if f == "identity_args"),
            "{r:?}"
        );
        // Replace does not need them.
        ok(
            &t,
            "CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
            Some(Strategy::Replace),
            &pg17(),
            &CatalogFacts::default(),
        );
    }

    #[test]
    fn strategy_outside_list_is_refused() {
        let strategy_err = |r: Result<Built, DbError>| {
            assert!(
                matches!(r, Err(DbError::Config { field: Some(ref f), .. }) if f == "strategy"),
                "{r:?}"
            )
        };
        let f = CatalogFacts::default();
        let p = my_target(ObjectKind::Procedure, Some("p"));
        let text = "CREATE PROCEDURE p() BEGIN END";
        strategy_err(run(&p, text, Some(Strategy::Replace), &mysql8(), &f));
        strategy_err(run(&p, text, None, &mysql8(), &f));
        strategy_err(run(&p, text, Some(Strategy::Recreate), &mariadb(), &f));
        let v = my_target(ObjectKind::View, Some("v"));
        strategy_err(run(
            &v,
            "CREATE VIEW v AS SELECT 1",
            Some(Strategy::Recreate),
            &mysql57(),
            &f,
        ));
        let mv = pg_target(ObjectKind::MaterializedView, Some("mv"));
        strategy_err(run(
            &mv,
            "CREATE MATERIALIZED VIEW public.mv AS SELECT 1",
            Some(Strategy::Replace),
            &pg17(),
            &f,
        ));
        let pv = pg_target(ObjectKind::View, Some("v"));
        strategy_err(run(
            &pv,
            "CREATE VIEW public.v AS SELECT 1",
            None,
            &pg17(),
            &f,
        ));
    }

    #[test]
    fn strategies_per_engine_and_kind() {
        use ObjectKind::*;
        use Strategy::*;
        for k in [View, Function, Procedure] {
            assert_eq!(
                strategies(&pg_target(k, Some("x")), &pg17()),
                vec![Replace, Recreate]
            );
        }
        for k in [MaterializedView, Trigger] {
            assert_eq!(
                strategies(&pg_target(k, Some("x")), &pg12()),
                vec![Recreate]
            );
        }
        for s in [mysql57(), mysql8(), mariadb()] {
            assert_eq!(strategies(&my_target(View, Some("x")), &s), vec![Replace]);
        }
        for k in [Function, Procedure, Trigger, Event] {
            assert_eq!(
                strategies(&my_target(k, Some("x")), &mariadb()),
                vec![Replace]
            );
            assert_eq!(
                strategies(&my_target(k, Some("x")), &mysql8()),
                vec![Recreate]
            );
            assert_eq!(
                strategies(&my_target(k, Some("x")), &mysql57()),
                vec![Recreate]
            );
            assert!(strategies(&my_target(k, None), &mysql8()).is_empty());
        }
        assert!(strategies(&pg_target(View, None), &pg17()).is_empty());
    }

    #[test]
    fn pg_view_recreate_restores_triggers_and_rules() {
        let v = pg_target(ObjectKind::View, Some("v"));
        let facts = CatalogFacts {
            trigger_defs: strs(&[
                "CREATE TRIGGER vi INSTEAD OF INSERT ON public.v FOR EACH ROW EXECUTE FUNCTION public.f()",
            ]),
            rule_defs: strs(&["CREATE RULE vd AS\n    ON DELETE TO public.v DO INSTEAD NOTHING;"]),
            ..Default::default()
        };
        let text = "CREATE VIEW public.v AS SELECT 1 AS a";
        let b = ok(&v, text, Some(Strategy::Recreate), &pg17(), &facts);
        assert_eq!(
            b.statements,
            strs(&[
                "DROP VIEW \"public\".\"v\" RESTRICT",
                text,
                "CREATE TRIGGER vi INSTEAD OF INSERT ON public.v FOR EACH ROW EXECUTE FUNCTION public.f()",
                "CREATE RULE vd AS\n    ON DELETE TO public.v DO INSTEAD NOTHING",
            ])
        );
        assert!(b.warnings.contains(
            &"The view's 1 trigger and 1 rule are re-created after it from their current definitions; the change is rolled back if they no longer fit the new view.".to_string()
        ), "{:?}", b.warnings);
        // Replace keeps them: nothing is re-created.
        let b = ok(&v, text, Some(Strategy::Replace), &pg17(), &facts);
        assert_eq!(b.statements.len(), 1);
        assert!(!b.warnings.iter().any(|w| w.contains("re-created after it")));
    }

    #[test]
    fn pg_view_options_are_noted() {
        let v = pg_target(ObjectKind::View, Some("v"));
        let facts = CatalogFacts {
            reloptions: strs(&["security_barrier=true", "check_option=local"]),
            ..Default::default()
        };
        let note = "The view's current options are security_barrier=true, check_option=local; the text sets them, and an option it leaves out is reset.";
        for strategy in [Strategy::Replace, Strategy::Recreate] {
            let b = ok(
                &v,
                "CREATE VIEW public.v WITH (security_barrier=true) AS SELECT 1 WITH LOCAL CHECK OPTION",
                Some(strategy),
                &pg17(),
                &facts,
            );
            assert!(b.warnings.contains(&note.to_string()), "{:?}", b.warnings);
        }
        let mv = pg_target(ObjectKind::MaterializedView, Some("mv"));
        let facts = CatalogFacts {
            reloptions: strs(&["fillfactor=70"]),
            ..Default::default()
        };
        let b = ok(
            &mv,
            "CREATE MATERIALIZED VIEW public.mv WITH (fillfactor=70) AS SELECT 1",
            Some(Strategy::Recreate),
            &pg17(),
            &facts,
        );
        assert!(b.warnings.contains(
            &"The materialized view's current options are fillfactor=70; the text sets them, and an option it leaves out is reset.".to_string()
        ));
        // No options: no note.
        let b = ok(
            &v,
            "CREATE VIEW public.v AS SELECT 1",
            Some(Strategy::Replace),
            &pg17(),
            &CatalogFacts::default(),
        );
        assert_eq!(b.warnings, strs(&[LOCK]));
    }

    #[test]
    fn warnings_follow_facts() {
        // PostgreSQL drop + create: comment and grants.
        let v = pg_target(ObjectKind::View, Some("v"));
        let facts = CatalogFacts {
            has_comment: true,
            has_grants: true,
            ..Default::default()
        };
        let b = ok(
            &v,
            "CREATE VIEW public.v AS SELECT 1",
            Some(Strategy::Recreate),
            &pg17(),
            &facts,
        );
        assert_eq!(
            b.warnings,
            strs(&[
                RESTRICT,
                "The view loses its comment: drop and re-create does not re-apply it.",
                "The view loses its grants: drop and re-create does not re-apply them; grant them again.",
                LOCK,
            ])
        );
        // Replace keeps both: no warning.
        let b = ok(
            &v,
            "CREATE VIEW public.v AS SELECT 1",
            Some(Strategy::Replace),
            &pg17(),
            &facts,
        );
        assert_eq!(b.warnings, strs(&[LOCK]));

        // Materialized view: refill, comment, grants.
        let mv = pg_target(ObjectKind::MaterializedView, Some("mv"));
        let b = ok(
            &mv,
            "CREATE MATERIALIZED VIEW public.mv AS SELECT 1",
            Some(Strategy::Recreate),
            &pg17(),
            &facts,
        );
        assert_eq!(
            b.warnings,
            strs(&[
                RESTRICT,
                "The materialized view loses its comment: drop and re-create does not re-apply it.",
                "The materialized view loses its grants: drop and re-create does not re-apply them; grant them again.",
                "The materialized view is filled again WITH DATA; it holds an exclusive lock while it runs.",
                LOCK,
            ])
        );

        // PostgreSQL trigger: comment only (triggers have no grants, no RESTRICT).
        let mut trg = pg_target(ObjectKind::Trigger, Some("trg"));
        trg.table = Some("orders".into());
        let tf = CatalogFacts {
            has_comment: true,
            has_grants: true,
            table: Some("orders".into()),
            table_oid: Some(1),
            ..Default::default()
        };
        let b = ok(
            &trg,
            "CREATE TRIGGER trg BEFORE UPDATE ON orders FOR EACH ROW EXECUTE FUNCTION public.f()",
            Some(Strategy::Recreate),
            &pg17(),
            &tf,
        );
        assert_eq!(
            b.warnings,
            strs(&[
                "The trigger loses its comment: drop and re-create does not re-apply it.",
                LOCK,
            ])
        );

        // MySQL trigger order.
        let mut mt = my_target(ObjectKind::Trigger, Some("trg"));
        mt.table = Some("orders".into());
        let order = CatalogFacts {
            table: Some("orders".into()),
            timing_event: Some("BEFORE INSERT".into()),
            same_timing_triggers: 2,
            ..Default::default()
        };
        let text = "CREATE TRIGGER trg BEFORE INSERT ON orders FOR EACH ROW SET NEW.a = 1";
        let b = ok(&mt, text, Some(Strategy::Recreate), &mysql8(), &order);
        assert_eq!(
            b.warnings,
            strs(&[
                "Between the DROP and the CREATE the trigger does not exist; writes to `orders` do not fire it.",
                "Other BEFORE INSERT triggers exist on `orders`; the re-created trigger fires after them.",
                LOCK,
            ])
        );
        let b = ok(&mt, text, Some(Strategy::Replace), &mariadb(), &order);
        assert_eq!(
            b.warnings,
            strs(&[
                "Other BEFORE INSERT triggers exist on `orders`; the re-created trigger fires after them.",
                LOCK,
            ])
        );
        // No other trigger: no order warning.
        let b = ok(
            &mt,
            text,
            Some(Strategy::Recreate),
            &mysql8(),
            &CatalogFacts {
                same_timing_triggers: 0,
                ..order
            },
        );
        assert!(!b.warnings.iter().any(|w| w.contains("fires after")));
    }
}
