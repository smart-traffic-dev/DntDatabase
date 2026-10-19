//! What a statement that succeeded did to its session: state a reconnect cannot
//! restore, the tracked database / `search_path`, and MySQL `sql_mode` changes.

use super::lexer::{leading_keywords, readings, tokenize, Dialect, TokKind, Token};
use super::split::strip_comments;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionEffect {
    /// State a reconnect cannot restore.
    pub custom: bool,
    /// MySQL: the session `sql_mode` may have changed.
    pub sql_mode: bool,
    /// MySQL plain `USE db` (not inside `/*! */`), backticks removed.
    pub use_db: Option<String>,
    /// PG: `Some(Some(stmt without comments))` for a session `SET search_path`,
    /// `Some(None)` for `RESET search_path | ALL`.
    pub search_path: Option<Option<String>>,
}

/// The session effect of `stmt`, a statement that succeeded (`None` when nothing
/// changes). Lexical, like `classify`; under MySQL it is the union over both
/// [`readings`] of executable comments, so `/*!40101 SET NAMES utf8 */` counts.
///
/// - MySQL: an assignment of a session-scope `SET` (anything but `GLOBAL`,
///   `PERSIST`, `PERSIST_ONLY`, `@@global.`, `@@persist.`, `@@persist_only.`;
///   user variables always), `CREATE [OR REPLACE] TEMPORARY`, `PREPARE`, `LOCK
///   TABLES` and a depth-0 user-variable assignment in any other statement (`@v
///   := …`, `INTO @v`) are `custom`; an assignment to the session `sql_mode` also
///   sets `sql_mode`; a plain `USE db` sets `use_db`. The `autocommit` assignment
///   is transaction control, tracked by the drivers, and is skipped (the other
///   assignments of the same `SET` still count).
/// - PG: session-level `SET` other than `search_path` (not `SET LOCAL`, `SET
///   TRANSACTION`, `SET CONSTRAINTS`; quoted and dotted names included),
///   temporary tables / views / sequences, `PREPARE` (not `PREPARE TRANSACTION`),
///   `LISTEN` and `DECLARE` are `custom`; `SET [SESSION] search_path` and `RESET
///   search_path | ALL` (plain or `"quoted"`) set `search_path`. Settings changed
///   through functions (`set_config`) are not detected.
pub fn session_effect(stmt: &str, dialect: Dialect) -> Option<SessionEffect> {
    let mut effect = SessionEffect::default();
    match dialect {
        Dialect::MySql(_) => {
            for reading in readings(stmt, dialect) {
                mysql_effect(&reading, dialect, &mut effect);
            }
            effect.use_db = parse_use(&strip_comments(stmt, dialect));
        }
        Dialect::Postgres => pg_effect(stmt, &mut effect),
    }
    (effect != SessionEffect::default()).then_some(effect)
}

fn mysql_effect(stmt: &str, dialect: Dialect, effect: &mut SessionEffect) {
    let words = leading_keywords(stmt, dialect, 4);
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let toks = tokenize(stmt, dialect);
    match words.as_slice() {
        // `SET` must be the first token, not a word after leading punctuation.
        ["SET", ..] if is_word(toks.first(), "SET") => set_assignments(&toks[1..], effect),
        ["CREATE", "TEMPORARY", ..]
        | ["CREATE", "OR", "REPLACE", "TEMPORARY"]
        | ["PREPARE", ..]
        | ["LOCK", "TABLES" | "TABLE", ..] => effect.custom = true,
        _ => effect.custom |= assigns_user_variable(&toks),
    }
}

fn is_word(t: Option<&Token>, kw: &str) -> bool {
    matches!(t.map(|t| t.kind), Some(TokKind::Word(w)) if w.eq_ignore_ascii_case(kw))
}

/// A user variable (`@v`, not `@@v`) as a word.
fn is_user_var(t: Option<&Token>) -> bool {
    matches!(t.map(|t| t.kind), Some(TokKind::Word(w)) if w.starts_with('@') && !w.starts_with("@@"))
}

/// Whether a statement other than `SET` assigns a user variable at depth 0:
/// `@v := …` or `… INTO @v`.
fn assigns_user_variable(toks: &[Token]) -> bool {
    let punct = |i: usize, c: u8| toks.get(i).map(|t| t.kind) == Some(TokKind::Punct(c));
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Open => depth += 1,
            TokKind::Close => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            TokKind::Word(w) if w.eq_ignore_ascii_case("INTO") && is_user_var(toks.get(i + 1)) => {
                return true
            }
            TokKind::Word(_)
                if is_user_var(Some(t)) && punct(i + 1, b':') && punct(i + 2, b'=') =>
            {
                return true
            }
            _ => {}
        }
    }
    false
}

/// Scope of one MySQL `SET` assignment.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Session,
    Global,
}

fn scope_keyword(w: &str) -> Option<Scope> {
    match w.to_ascii_uppercase().as_str() {
        "SESSION" | "LOCAL" | "@@SESSION" | "@@LOCAL" => Some(Scope::Session),
        "GLOBAL" | "PERSIST" | "PERSIST_ONLY" | "@@GLOBAL" | "@@PERSIST" | "@@PERSIST_ONLY" => {
            Some(Scope::Global)
        }
        _ => None,
    }
}

/// The comma-separated assignments after `SET`. A scope keyword applies to the
/// assignments after it that name none (as in MySQL); `@@scope.var` and user
/// variables (`@x`, always session) name their own.
fn set_assignments(toks: &[Token], effect: &mut SessionEffect) {
    let mut sticky = Scope::Session;
    let mut depth = 0usize;
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Open => depth += 1,
            TokKind::Close => depth = depth.saturating_sub(1),
            TokKind::Punct(b',') if depth == 0 => {
                assignment(&toks[start..i], &mut sticky, effect);
                start = i + 1;
            }
            _ => {}
        }
    }
    assignment(&toks[start..], &mut sticky, effect);
}

fn assignment(a: &[Token], sticky: &mut Scope, effect: &mut SessionEffect) {
    let word = |i: usize| match a.get(i).map(|t| t.kind) {
        Some(TokKind::Word(w)) => Some(w),
        _ => None,
    };
    let dot = |i: usize| a.get(i).map(|t| t.kind) == Some(TokKind::Punct(b'.'));
    let Some(first) = word(0) else {
        // Nothing recognisable (e.g. a quoted name): assume session state.
        effect.custom |= !a.is_empty();
        return;
    };
    let (scope, name) = if let (Some(s), false) = (scope_keyword(first), first.starts_with('@')) {
        *sticky = s;
        (s, word(1))
    } else if let Some(var) = first.strip_prefix("@@") {
        match scope_keyword(first) {
            Some(s) if dot(1) => (s, word(2)),
            // `@@var` is the session value.
            _ => (Scope::Session, Some(var)),
        }
    } else if first.starts_with('@') {
        // A user variable lives in the session.
        (Scope::Session, None)
    } else {
        (*sticky, Some(first))
    };
    // `autocommit` is transaction control, tracked and re-applied by the drivers.
    if scope == Scope::Session && !name.is_some_and(|n| n.eq_ignore_ascii_case("autocommit")) {
        effect.custom = true;
        if name.is_some_and(|n| n.eq_ignore_ascii_case("sql_mode")) {
            effect.sql_mode = true;
        }
    }
}

/// The database named by a `USE db` statement (backticks removed), if `sql` is one.
fn parse_use(sql: &str) -> Option<String> {
    let s = sql.trim().trim_end_matches(';').trim_end();
    let kw = s.get(..3)?;
    let rest = &s[3..];
    if !kw.eq_ignore_ascii_case("USE") || !rest.starts_with(|c: char| c.is_whitespace() || c == '`')
    {
        return None;
    }
    let name = rest.trim_start();
    if let Some(inner) = name.strip_prefix('`').and_then(|n| n.strip_suffix('`')) {
        return (!inner.is_empty()).then(|| inner.replace("``", "`"));
    }
    (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| name.to_string())
}

fn pg_effect(stmt: &str, effect: &mut SessionEffect) {
    let toks = tokenize(stmt, Dialect::Postgres);
    if is_word(toks.first(), "SET") {
        return pg_set(stmt, &toks, effect);
    }
    if is_word(toks.first(), "RESET") {
        if pg_setting_name(stmt, &toks, 1)
            .is_some_and(|n| n.eq_ignore_ascii_case("search_path") || n.eq_ignore_ascii_case("all"))
        {
            effect.search_path = Some(None);
        }
        return;
    }
    let words = leading_keywords(stmt, Dialect::Postgres, 4);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    effect.custom = match w.as_slice() {
        ["CREATE", "TEMP" | "TEMPORARY", ..]
        | ["CREATE", "GLOBAL" | "LOCAL", "TEMP" | "TEMPORARY", ..]
        | ["CREATE", "OR", "REPLACE", "TEMP" | "TEMPORARY"] => true,
        ["PREPARE", second, ..] => *second != "TRANSACTION",
        ["LISTEN" | "DECLARE", ..] => true,
        _ => false,
    };
}

/// PG `SET …` (`toks[0]` is `SET`). `LOCAL`, `TRANSACTION` and `CONSTRAINTS` end with
/// the transaction; `[SESSION] search_path` (plain or quoted) is tracked; any other
/// setting, quoted or dotted names included, is custom.
fn pg_set(stmt: &str, toks: &[Token], effect: &mut SessionEffect) {
    let mut i = 1;
    let word = |i: usize| match toks.get(i).map(|t| t.kind) {
        Some(TokKind::Word(w)) => Some(w.to_ascii_uppercase()),
        _ => None,
    };
    if word(i).as_deref() == Some("SESSION") {
        i += 1;
    } else if matches!(
        word(i).as_deref(),
        Some("LOCAL" | "TRANSACTION" | "CONSTRAINTS")
    ) {
        return;
    }
    if i >= toks.len() {
        // A bare `SET` / `SET SESSION` is a syntax error; nothing ran.
        return;
    }
    let dotted = toks.get(i + 1).map(|t| t.kind) == Some(TokKind::Punct(b'.'));
    if !dotted
        && pg_setting_name(stmt, toks, i).is_some_and(|n| n.eq_ignore_ascii_case("search_path"))
    {
        effect.search_path = Some(Some(
            strip_comments(stmt, Dialect::Postgres).trim().to_string(),
        ));
    } else {
        effect.custom = true;
    }
}

/// The setting name at token `i`: a plain word, or a `"quoted"` identifier unquoted.
fn pg_setting_name(stmt: &str, toks: &[Token], i: usize) -> Option<String> {
    let t = toks.get(i)?;
    match t.kind {
        TokKind::Word(w) => Some(w.to_string()),
        TokKind::Quoted => {
            let q = &stmt[t.start..t.end];
            let inner = q.strip_prefix('"')?.strip_suffix('"')?;
            Some(inner.replace("\"\"", "\""))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MYSQL: Dialect = Dialect::MYSQL;
    const PG: Dialect = Dialect::Postgres;

    fn custom(sql: &str, d: Dialect) -> bool {
        session_effect(sql, d).is_some_and(|e| e.custom)
    }

    fn use_target(sql: &str) -> Option<String> {
        session_effect(sql, MYSQL).and_then(|e| e.use_db)
    }

    fn search_path_change(sql: &str) -> Option<Option<String>> {
        session_effect(sql, PG).and_then(|e| e.search_path)
    }

    #[test]
    fn exec_comment_set_names_is_session_state() {
        assert!(
            session_effect("/*!40101 SET NAMES utf8 */", MYSQL)
                .unwrap()
                .custom
        );
    }

    #[test]
    fn exec_comment_sql_mode_detected() {
        let e = session_effect(
            "/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */",
            MYSQL,
        )
        .unwrap();
        assert!(e.sql_mode && e.custom, "{e:?}");
    }

    #[test]
    fn sql_mode_assignment_targets() {
        for sql in [
            "SET sql_mode = ''",
            "set @@sql_mode = ''",
            "SET @@session.sql_mode = ''",
            "SET SESSION sql_mode = ''",
            "SET @x = 1, LOCAL sql_mode := ''",
            "SET GLOBAL max_connections = 10, SESSION sql_mode = ''",
        ] {
            assert!(session_effect(sql, MYSQL).unwrap().sql_mode, "{sql}");
        }
        for sql in [
            "SET @OLD_SQL_MODE = @@SQL_MODE",
            "SET @@global.sql_mode = ''",
            "SET GLOBAL sql_mode = ''",
            "SELECT @@sql_mode",
            "SET @x = (SELECT 1, sql_mode = 1)",
        ] {
            assert!(
                !session_effect(sql, MYSQL).is_some_and(|e| e.sql_mode),
                "{sql}"
            );
        }
    }

    #[test]
    fn set_global_is_not_session_state() {
        for sql in [
            "SET GLOBAL sql_mode=''",
            "SET PERSIST x=1",
            "SET @@global.x=1",
        ] {
            assert_eq!(session_effect(sql, MYSQL), None, "{sql}");
        }
        // A user variable or a session assignment after a global one still counts.
        assert!(custom("SET GLOBAL x = 1, @y = 2", MYSQL));
        assert!(custom("SET PERSIST x = 1, SESSION y = 2", MYSQL));
    }

    #[test]
    fn use_and_search_path() {
        // parses_use_statements
        assert_eq!(parse_use("USE shop"), Some("shop".into()));
        assert_eq!(parse_use("  use `my db`;"), Some("my db".into()));
        assert_eq!(parse_use("Use`a``b`"), Some("a`b".into()));
        assert_eq!(parse_use("USER()"), None);
        assert_eq!(parse_use("use"), None);
        assert_eq!(parse_use("use a b"), None);

        // use_behind_comments_is_tracked
        assert_eq!(use_target("/* pick */ USE shop"), Some("shop".into()));
        assert_eq!(use_target("-- pick\nUSE shop -- why"), Some("shop".into()));
        assert_eq!(
            use_target("# pick\nuse `my db` /* end */;"),
            Some("my db".into())
        );
        assert_eq!(use_target("USE /* odd */ shop"), Some("shop".into()));
        assert_eq!(use_target("/*!40101 USE shop */"), None);
        assert_eq!(use_target("/* USE shop */ SELECT 1"), None);
        assert_eq!(
            session_effect("/*!40101 USE x */", MYSQL).and_then(|e| e.use_db),
            None
        );
        // `USE` is MySQL only.
        assert_eq!(session_effect("USE shop", PG), None);

        // search_path_statements
        let set = |s: &str| Some(Some(s.to_string()));
        assert_eq!(
            search_path_change("SET search_path TO sales, public"),
            set("SET search_path TO sales, public")
        );
        assert_eq!(
            search_path_change("  set Search_Path=sales;"),
            set("set Search_Path=sales;")
        );
        assert_eq!(
            search_path_change("SET SESSION search_path = x"),
            set("SET SESSION search_path = x")
        );
        assert_eq!(search_path_change("RESET search_path"), Some(None));
        assert_eq!(search_path_change("reset all"), Some(None));
        assert_eq!(search_path_change("SET LOCAL search_path = x"), None);
        assert_eq!(search_path_change("SET search_path_x = 1"), None);
        assert_eq!(search_path_change("SET statement_timeout = 0"), None);
        assert_eq!(search_path_change("SELECT 1"), None);

        // search_path_behind_comments_is_tracked
        assert_eq!(
            search_path_change("/* pick */ SET search_path TO sales, public"),
            set("SET search_path TO sales, public")
        );
        assert_eq!(
            search_path_change("-- pick\nSET search_path = sales -- why"),
            set("SET search_path = sales")
        );
        assert_eq!(
            search_path_change("/* a */ /* b */ RESET search_path"),
            Some(None)
        );
        assert_eq!(
            search_path_change("/* SET search_path = x */ SELECT 1"),
            None
        );
        // A comment inside the value is dropped from the replayed statement.
        assert_eq!(
            search_path_change("SET search_path = /* x */ sales"),
            set("SET search_path =   sales")
        );
        // `search_path` is PG only.
        assert_eq!(
            session_effect("SET search_path = x", MYSQL).and_then(|e| e.search_path),
            None
        );
    }

    #[test]
    fn non_ascii_prefix_is_not_set() {
        for d in [MYSQL, PG] {
            assert_eq!(session_effect("ưSET a=1", d), None, "{d:?}");
            assert_eq!(session_effect("ưư SET @x = 1", d), None, "{d:?}");
            assert_eq!(session_effect("ế", d), None, "{d:?}");
            assert!(custom("/* ư */ SET a = 1", d), "{d:?}");
        }
    }

    #[test]
    fn mysql_session_state_cases() {
        // Moved from driver-mysql `session_state_changes_are_lexical`.
        for sql in [
            "SET @x = 1",
            "set sql_mode = ''",
            "/* c */ SET SESSION wait_timeout = 10",
            "SET @@session.sql_mode = ''",
            "SET NAMES latin1",
            "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
            "CREATE TEMPORARY TABLE tmp (a int)",
            "create  temporary table if not exists tmp (a int)",
            "PREPARE s FROM 'SELECT 1'",
        ] {
            assert!(custom(sql, MYSQL), "{sql}");
        }
        for sql in [
            "SELECT 1",
            "SET GLOBAL max_connections = 10",
            "SET PERSIST max_connections = 10",
            "SET @@global.sql_mode = ''",
            "CREATE TABLE t (a int)",
            "INSERT INTO t SELECT 'SET @x = 1'",
            "-- SET @x = 1\nSELECT 1",
        ] {
            assert!(!custom(sql, MYSQL), "{sql}");
        }
    }

    #[test]
    fn autocommit_assignment_alone_is_not_custom() {
        for sql in [
            "SET autocommit = 0",
            "set @@autocommit := 1",
            "SET SESSION autocommit = OFF",
            "SET @@session.autocommit = 0",
            "/*!40101 SET autocommit=0 */",
        ] {
            assert_eq!(session_effect(sql, MYSQL), None, "{sql}");
        }
        // The other assignments of the same `SET` still count.
        for sql in [
            "SET NAMES latin1, autocommit = 0",
            "SET autocommit = 0, @x = 1",
            "SET autocommit = 0, sql_mode = ''",
        ] {
            assert!(custom(sql, MYSQL), "{sql}");
        }
        // A user variable named like the setting is still a user variable.
        assert!(custom("SET @autocommit = 0", MYSQL));
    }

    #[test]
    fn mariadb_create_or_replace_temporary_is_custom() {
        assert!(custom("CREATE OR REPLACE TEMPORARY TABLE t (a int)", MYSQL));
        assert!(!custom("CREATE OR REPLACE TABLE t (a int)", MYSQL));
    }

    #[test]
    fn lock_tables_is_custom() {
        assert!(custom("LOCK TABLES t READ", MYSQL));
        assert!(custom("lock table t write", MYSQL));
        assert!(!custom("UNLOCK TABLES", MYSQL));
    }

    #[test]
    fn user_variable_assignment_in_a_statement_is_custom() {
        for sql in [
            "SELECT a INTO @v FROM t",
            "SELECT a FROM t INTO @v, @w",
            "SELECT @v := a FROM t",
            "SELECT @v:=1",
            "UPDATE t SET a = @n := @n + 1",
            "/*!40101 SELECT 1 INTO @v */",
        ] {
            assert!(custom(sql, MYSQL), "{sql}");
        }
        for sql in [
            "SELECT @v FROM t",
            "SELECT @@sql_mode INTO OUTFILE '/tmp/x'",
            "SELECT a INTO OUTFILE '/tmp/x' FROM t",
            "SELECT @v = 1",
            "SELECT 'INTO @v'",
            "SELECT (@v := 1)",
            "SELECT @@x := 1",
        ] {
            assert!(!custom(sql, MYSQL), "{sql}");
        }
    }

    #[test]
    fn pg_session_state_cases() {
        // Moved from driver-pg `session_state_changes_are_lexical`.
        for sql in [
            "SET statement_timeout = 0",
            "set work_mem to '64MB'",
            "/* c */ SET SESSION statement_timeout = 0",
            "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET ROLE dnt_limited",
            "SET TIME ZONE 'UTC'",
            "CREATE TEMP TABLE t (a int)",
            "create temporary table t (a int)",
            "CREATE GLOBAL TEMPORARY TABLE t (a int)",
            "CREATE LOCAL TEMP TABLE t (a int)",
            "CREATE OR REPLACE TEMP VIEW v AS SELECT 1",
            "PREPARE q AS SELECT 1",
            "LISTEN chan",
            "DECLARE c CURSOR WITH HOLD FOR SELECT 1",
            // Quoted setting names.
            r#"SET "TimeZone" = 'UTC'"#,
            r#"SET "work_mem" = '64MB'"#,
            r#"SET "myapp"."user" = 'bob'"#,
            r#"SET SESSION "work_mem" = '64MB'"#,
        ] {
            assert!(custom(sql, PG), "{sql}");
        }
        // A quoted `search_path` is tracked, not custom.
        let sp = session_effect(r#"SET "search_path" = 'sales'"#, PG).unwrap();
        assert!(!sp.custom, "{sp:?}");
        assert_eq!(
            sp.search_path,
            Some(Some(r#"SET "search_path" = 'sales'"#.into()))
        );
        assert_eq!(
            search_path_change(r#"SET "Search_Path" TO sales"#),
            Some(Some(r#"SET "Search_Path" TO sales"#.into()))
        );
        assert_eq!(search_path_change(r#"RESET "search_path""#), Some(None));
        for sql in [
            "SELECT 1",
            // Tracked and re-applied.
            "SET search_path TO sales",
            "SET SESSION search_path = sales",
            // Transaction-scoped.
            "SET LOCAL statement_timeout = 0",
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET CONSTRAINTS ALL DEFERRED",
            "PREPARE TRANSACTION 'x'",
            "CREATE TABLE t (a int)",
            "INSERT INTO t SELECT 'SET x = 1'",
            "-- SET x = 1\nSELECT 1",
            "ưư SET x = 1",
        ] {
            assert!(!custom(sql, PG), "{sql}");
        }
        // `/*!` is a plain comment in PG.
        assert_eq!(session_effect("/*!40101 SET NAMES utf8 */", PG), None);
    }
}
