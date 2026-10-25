//! Spec §5.1 steps 1–2: the one statement of the edited text.

use super::{editable_kind, text_error, MAX_TEXT_BYTES};
use crate::error::DbError;
use crate::meta::ObjectKind;
use crate::server::ServerInfo;
use crate::sql::lexer::{is_kw, tokenize, word_at, Dialect, TokKind, Token};
use crate::sql::multi::resplits;
use crate::sql::split::split;

const ONE_STATEMENT: &str = "Only one statement can be applied; remove the other statements.";
const AFTER_BODY: &str =
    "Only one statement can be applied; remove what follows the end of the body.";

/// The one statement of `text` (trimmed, without its delimiter), or a
/// `Config { field: "text" }` error saying why there is not exactly one.
pub fn isolate(
    text: &str,
    kind: ObjectKind,
    dialect: Dialect,
    server: &ServerInfo,
) -> Result<String, DbError> {
    editable_kind(kind, server)?;
    if text.len() > MAX_TEXT_BYTES {
        return Err(text_error(
            "The text is larger than 1 MiB; shorten it to apply it here.",
        ));
    }
    let toks = tokenize(text, dialect);
    if toks.is_empty() {
        return Err(text_error(
            "The text is empty; write the CREATE statement of the object.",
        ));
    }
    match dialect {
        Dialect::MySql(_) => {
            if toks.iter().any(|t| t.kind == TokKind::ExecComment) {
                return Err(text_error(
                    "Executable comments (/*! … */) cannot be applied here; remove them.",
                ));
            }
            if matches!(
                kind,
                ObjectKind::Function
                    | ObjectKind::Procedure
                    | ObjectKind::Trigger
                    | ObjectKind::Event
            ) {
                let candidate = if has_delimiter_directive(text, &toks) {
                    only_statement(text, dialect)?
                } else {
                    text.trim().to_string()
                };
                let end = body_end(&candidate, dialect)?;
                Ok(candidate[..end].trim().to_string())
            } else {
                let stmt = only_statement(text, dialect)?;
                if resplits(&stmt, dialect) {
                    return Err(text_error(ONE_STATEMENT));
                }
                Ok(stmt)
            }
        }
        Dialect::Postgres => {
            let atomic = toks
                .windows(2)
                .any(|w| is_kw(word_at(w, 0), "BEGIN") && is_kw(word_at(w, 1), "ATOMIC"));
            if !atomic {
                return only_statement(text, dialect);
            }
            if server.version.0 < 14 {
                return Err(DbError::Unsupported {
                    feature: "BEGIN ATOMIC function bodies".into(),
                    version: "PostgreSQL 14".into(),
                });
            }
            // Taken whole: the extended protocol refuses a second command anyway.
            let mut end = text.len();
            for t in toks.iter().rev() {
                if t.kind != TokKind::Punct(b';') {
                    break;
                }
                end = t.start;
            }
            Ok(text[..end].trim().to_string())
        }
    }
}

/// The only `split` statement of `text`.
fn only_statement(text: &str, dialect: Dialect) -> Result<String, DbError> {
    let mut stmts = split(text, dialect);
    match stmts.len() {
        0 => Err(text_error(
            "The text is empty; write the CREATE statement of the object.",
        )),
        1 => Ok(stmts.remove(0).text),
        _ => Err(text_error(ONE_STATEMENT)),
    }
}

/// Whether `text` holds a MySQL client `DELIMITER` line, as `split` reads one: the
/// word `DELIMITER` (outside strings and comments) with only blanks before it on its
/// line and a space or tab after it.
fn has_delimiter_directive(text: &str, toks: &[Token]) -> bool {
    let b = text.as_bytes();
    toks.iter().any(|t| {
        matches!(t.kind, TokKind::Word(w) if w.eq_ignore_ascii_case("DELIMITER"))
            && matches!(b.get(t.end), Some(b' ' | b'\t'))
            && b[..t.start]
                .iter()
                .rev()
                .take_while(|&&c| c != b'\n')
                .all(|&c| c == b' ' || c == b'\t')
    })
}

/// What opened a block the body scanner tracks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Begin,
    Case,
}

/// The MySQL body scanner: the byte offset where the one compound statement of
/// `stmt` ends (its first `;` at paren and block depth 0, or the end of the text),
/// refusing the text when anything but `;` follows, or when its blocks or
/// parentheses do not balance. Refuses rather than guesses (spec R6).
fn body_end(stmt: &str, dialect: Dialect) -> Result<usize, DbError> {
    let toks = tokenize(stmt, dialect);
    let mut paren: i64 = 0;
    let mut blocks: Vec<Block> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        match t.kind {
            TokKind::Open => paren += 1,
            TokKind::Close => {
                paren -= 1;
                if paren < 0 {
                    return Err(text_error(
                        "The text has an unbalanced closing parenthesis; check the parentheses.",
                    ));
                }
            }
            TokKind::Punct(b';') if blocks.is_empty() => {
                if paren != 0 {
                    return Err(text_error(
                        "The text has an unclosed parenthesis before `;`; check the parentheses.",
                    ));
                }
                if toks[i + 1..].iter().any(|t| t.kind != TokKind::Punct(b';')) {
                    return Err(text_error(AFTER_BODY));
                }
                return Ok(t.start);
            }
            TokKind::Word(w) if paren == 0 => {
                if w.eq_ignore_ascii_case("BEGIN") {
                    if !is_name(&toks, i) {
                        blocks.push(Block::Begin);
                    }
                } else if w.eq_ignore_ascii_case("CASE") {
                    blocks.push(Block::Case);
                } else if w.eq_ignore_ascii_case("END") {
                    // The END of a CASE expression is often followed by `AS`, `FROM`,
                    // `,` or `)`: when a CASE is the innermost open block, only the
                    // previous token can make this END a name.
                    let name = if blocks.last() == Some(&Block::Case) {
                        is_name_by_prev(&toks, i)
                    } else {
                        is_name(&toks, i)
                    };
                    if !name {
                        let next = word_at(&toks, i + 1);
                        if ["IF", "LOOP", "WHILE", "REPEAT"]
                            .iter()
                            .any(|k| is_kw(next, k))
                        {
                            i += 1;
                        } else {
                            if is_kw(next, "CASE") {
                                i += 1;
                            }
                            if blocks.pop().is_none() {
                                return Err(text_error(
                                    "The text has an unbalanced END; check the BEGIN … END blocks.",
                                ));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    if !blocks.is_empty() {
        return Err(text_error(
            "BEGIN … END is not closed; add the missing END.",
        ));
    }
    if paren != 0 {
        return Err(text_error(
            "The text has an unclosed parenthesis; check the parentheses.",
        ));
    }
    Ok(stmt.len())
}

/// Whether the `BEGIN`/`END` at token `i` stands where a name would (spec §5.1).
fn is_name(toks: &[Token], i: usize) -> bool {
    is_name_by_prev(toks, i)
        || toks.get(i + 1).is_some_and(|n| {
            matches!(
                n.kind,
                TokKind::Punct(b',') | TokKind::Close | TokKind::Punct(b'.') | TokKind::Punct(b'=')
            )
        })
        || is_kw(word_at(toks, i + 1), "FROM")
        || is_kw(word_at(toks, i + 1), "AS")
}

fn is_name_by_prev(toks: &[Token], i: usize) -> bool {
    let Some(p) = i.checked_sub(1).and_then(|j| toks.get(j)) else {
        return false;
    };
    matches!(
        p.kind,
        TokKind::Punct(b'.') | TokKind::Punct(b',') | TokKind::Open | TokKind::Punct(b'=')
    ) || is_kw(word_at(toks, i - 1), "SELECT")
        || is_kw(word_at(toks, i - 1), "AS")
}

#[cfg(test)]
mod tests {
    use super::super::test_servers::*;
    use super::super::MAX_TEXT_BYTES;
    use super::*;
    use crate::sql::lexer::MySqlMode;

    const MY: Dialect = Dialect::MYSQL;
    const PG: Dialect = Dialect::Postgres;

    fn ok(text: &str, kind: ObjectKind) -> String {
        isolate(text, kind, MY, &mysql8()).unwrap_or_else(|e| panic!("{text:?}: {e:?}"))
    }

    fn refused(r: Result<String, DbError>) -> String {
        match r {
            Err(DbError::Config {
                field: Some(f),
                message,
            }) if f == "text" => message,
            other => panic!("expected a text refusal, got {other:?}"),
        }
    }

    #[test]
    fn isolate_view_single_and_refuses_two() {
        assert_eq!(
            ok("CREATE VIEW v AS SELECT 1;", ObjectKind::View),
            "CREATE VIEW v AS SELECT 1"
        );
        assert_eq!(
            ok(
                "  -- note\nCREATE VIEW v AS SELECT ';' ;  \n",
                ObjectKind::View
            ),
            "-- note\nCREATE VIEW v AS SELECT ';'"
        );
        refused(isolate(
            "CREATE VIEW v AS SELECT 1; SELECT 2",
            ObjectKind::View,
            MY,
            &mysql8(),
        ));
        // A delimiter left on would make the server run both pieces.
        refused(isolate(
            "DELIMITER $$\nCREATE VIEW v AS SELECT 1; DROP TABLE t$$",
            ObjectKind::View,
            MY,
            &mariadb(),
        ));
    }

    #[test]
    fn isolate_compound_bodies() {
        let procs = [
            "CREATE PROCEDURE p() BEGIN SELECT 1; BEGIN SELECT 2; END; END",
            "CREATE PROCEDURE p() l1: BEGIN l2: BEGIN LEAVE l2; END l2; SELECT 1; END l1",
            "CREATE PROCEDURE p(IN a INT) BEGIN IF a > 1 THEN SELECT 1; ELSEIF a = 0 THEN SELECT 0; ELSE SELECT 2; END IF; END",
            "CREATE PROCEDURE p(a INT) BEGIN CASE a WHEN 1 THEN SELECT 1; ELSE BEGIN END; END CASE; END",
            "CREATE PROCEDURE p(a INT) BEGIN DECLARE x INT; SET x = CASE WHEN a > 1 THEN 1 ELSE 2 END; SELECT CASE a WHEN 1 THEN 'a' END AS c; END",
            "CREATE PROCEDURE p() BEGIN DECLARE i INT DEFAULT 0; l: LOOP SET i = i + 1; IF i > 3 THEN LEAVE l; END IF; END LOOP l; WHILE i > 0 DO SET i = i - 1; END WHILE; REPEAT SET i = i + 1; UNTIL i > 2 END REPEAT; END",
            "CREATE PROCEDURE p() BEGIN DECLARE CONTINUE HANDLER FOR SQLEXCEPTION BEGIN SELECT 'err'; END; SELECT 1; END",
            "CREATE PROCEDURE p() BEGIN SELECT `begin`, `end` FROM t; END",
            "CREATE PROCEDURE p() BEGIN SELECT t.end FROM t; SELECT t.begin, a FROM t; END",
            "CREATE PROCEDURE p() BEGIN SELECT end FROM t; SELECT a AS begin FROM t; END",
            "CREATE PROCEDURE p() BEGIN SELECT CASE WHEN a THEN 1 END, CASE WHEN b THEN 2 END FROM t; SELECT (CASE WHEN a THEN 1 END) = 1; END",
            "CREATE PROCEDURE p() SELECT 1",
            "CREATE PROCEDURE p() SELECT CASE WHEN a THEN 1 END FROM t",
            "CREATE DEFINER=`root`@`%` PROCEDURE `db`.`p`(IN a INT) COMMENT 'BEGIN; END' BEGIN SELECT ';'; END",
        ];
        for p in procs {
            assert_eq!(ok(p, ObjectKind::Procedure), p);
            let semi = format!("{p};\n-- trailing\n");
            assert_eq!(ok(&semi, ObjectKind::Procedure), p, "{semi}");
        }
        let trg = "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; END";
        assert_eq!(ok(trg, ObjectKind::Trigger), trg);
        let trg1 = "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW SET NEW.a = 1";
        assert_eq!(ok(&format!("{trg1};"), ObjectKind::Trigger), trg1);
        let ev = "CREATE EVENT e ON SCHEDULE EVERY 1 DAY DISABLE DO BEGIN DELETE FROM t; END";
        assert_eq!(ok(ev, ObjectKind::Event), ev);
        let f = "CREATE FUNCTION f(a INT) RETURNS INT DETERMINISTIC BEGIN RETURN a + 1; END";
        assert_eq!(ok(f, ObjectKind::Function), f);
        let f1 = "CREATE FUNCTION f(a INT) RETURNS INT DETERMINISTIC RETURN a + 1";
        assert_eq!(ok(f1, ObjectKind::Function), f1);
    }

    #[test]
    fn second_statement_after_body_is_refused() {
        for text in [
            "CREATE PROCEDURE p() BEGIN SELECT 1; END; DROP TABLE t",
            "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END; DROP TABLE t$$\nDELIMITER ;",
            "CREATE PROCEDURE p() SELECT 1; DROP TABLE t",
            "CREATE PROCEDURE p() BEGIN SELECT 1; END END; DROP TABLE t",
            "CREATE PROCEDURE p() BEGIN SELECT 1; BEGIN SELECT 2; END",
            "CREATE PROCEDURE p() BEGIN SELECT 1; END; END",
            "CREATE PROCEDURE p() SELECT 1); DROP TABLE t; SELECT (2",
            "CREATE PROCEDURE p() SELECT (1; DROP TABLE t)",
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW SET NEW.a = 1; DELETE FROM t",
        ] {
            let msg = refused(isolate(text, ObjectKind::Procedure, MY, &mysql8()));
            assert!(!msg.is_empty(), "{text}");
        }
        let msg = refused(isolate(
            "CREATE PROCEDURE p() BEGIN SELECT 1; END; DROP TABLE t",
            ObjectKind::Procedure,
            MY,
            &mysql8(),
        ));
        assert_eq!(
            msg,
            "Only one statement can be applied; remove what follows the end of the body."
        );
        assert!(refused(isolate(
            "CREATE PROCEDURE p() BEGIN SELECT 1;",
            ObjectKind::Procedure,
            MY,
            &mysql8()
        ))
        .contains("not closed"));
        assert!(refused(isolate(
            "CREATE PROCEDURE p() SELECT 1 END",
            ObjectKind::Procedure,
            MY,
            &mysql8()
        ))
        .contains("unbalanced END"));
        // Two DELIMITER statements.
        refused(isolate(
            "DELIMITER $$\nCREATE PROCEDURE p() BEGIN END$$\nCREATE PROCEDURE q() BEGIN END$$\nDELIMITER ;",
            ObjectKind::Procedure,
            MY,
            &mysql8(),
        ));
    }

    #[test]
    fn isolate_delimiter_block() {
        assert_eq!(
            ok(
                "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;",
                ObjectKind::Procedure
            ),
            "CREATE PROCEDURE p() BEGIN SELECT 1; END"
        );
        assert_eq!(
            ok(
                "delimiter //\nCREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; END //\ndelimiter ;\n",
                ObjectKind::Trigger
            ),
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; END"
        );
    }

    #[test]
    fn isolate_refuses_exec_comments_and_empty() {
        for text in ["", "   \n\t", "-- only a comment\n", "/* x */"] {
            refused(isolate(text, ObjectKind::View, MY, &mysql8()));
            refused(isolate(text, ObjectKind::View, PG, &pg17()));
        }
        refused(isolate(
            "CREATE /*!50001 OR REPLACE */ VIEW v AS SELECT 1",
            ObjectKind::View,
            MY,
            &mysql57(),
        ));
        refused(isolate(
            "CREATE PROCEDURE p() BEGIN /*M!100000 DROP TABLE t; */ END",
            ObjectKind::Procedure,
            MY,
            &mariadb(),
        ));
        let big = format!("CREATE VIEW v AS SELECT '{}'", "x".repeat(MAX_TEXT_BYTES));
        refused(isolate(&big, ObjectKind::View, MY, &mysql8()));
        refused(isolate(&big, ObjectKind::View, PG, &pg17()));
        // Kinds the engine cannot edit.
        assert!(isolate("CREATE TABLE t (a int)", ObjectKind::Table, MY, &mysql8()).is_err());
        // Under ANSI_QUOTES a double-quoted `;` is part of a name.
        let ansi = Dialect::MySql(MySqlMode {
            ansi_quotes: true,
            no_backslash_escapes: false,
        });
        assert_eq!(
            isolate(
                "CREATE PROCEDURE \"p;\"() BEGIN END",
                ObjectKind::Procedure,
                ansi,
                &mysql8()
            )
            .unwrap(),
            "CREATE PROCEDURE \"p;\"() BEGIN END"
        );
    }

    #[test]
    fn isolate_pg() {
        let f = "CREATE OR REPLACE FUNCTION public.f() RETURNS int LANGUAGE plpgsql AS $$ BEGIN PERFORM 1; RETURN 1; END $$";
        assert_eq!(
            isolate(&format!("{f};\n"), ObjectKind::Function, PG, &pg17()).unwrap(),
            f
        );
        let atomic = "CREATE OR REPLACE FUNCTION public.f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; SELECT 2; END";
        assert_eq!(
            isolate(&format!("{atomic};"), ObjectKind::Function, PG, &pg17()).unwrap(),
            atomic
        );
        assert_eq!(
            isolate(&format!("{atomic} ;\n"), ObjectKind::Function, PG, &pg17()).unwrap(),
            atomic
        );
        match isolate(atomic, ObjectKind::Function, PG, &pg12()) {
            Err(DbError::Unsupported { feature, version }) => {
                assert_eq!(feature, "BEGIN ATOMIC function bodies");
                assert_eq!(version, "PostgreSQL 14");
            }
            other => panic!("{other:?}"),
        }
        refused(isolate(
            "CREATE VIEW public.v AS SELECT 1; DROP TABLE t",
            ObjectKind::View,
            PG,
            &pg17(),
        ));
        refused(isolate(
            "CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS 'SELECT 1'; SELECT 2",
            ObjectKind::Function,
            PG,
            &pg17(),
        ));
        // `begin atomic` inside a string is not a body.
        refused(isolate(
            "CREATE VIEW public.v AS SELECT 'begin atomic'; SELECT 2",
            ObjectKind::View,
            PG,
            &pg17(),
        ));
    }
}
