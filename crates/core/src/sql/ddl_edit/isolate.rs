//! Spec §5.1 steps 1–2: the one statement of the edited text.

use super::{editable_kind, parse_header, text_error, MAX_TEXT_BYTES};
use crate::error::DbError;
use crate::meta::ObjectKind;
use crate::server::ServerInfo;
use crate::sql::lexer::{is_kw, tokenize, word_at, Dialect, TokKind, Token};
use crate::sql::multi::resplits;
use crate::sql::split::{delimiter_directive, split};

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
            refuse_exec_comments(&toks)?;
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
                refuse_exec_comments(&tokenize(&candidate, dialect))?;
                let end = body_end(&candidate, dialect)?;
                Ok(candidate[..end].trim().to_string())
            } else {
                let stmt = only_statement(text, dialect)?;
                refuse_exec_comments(&tokenize(&stmt, dialect))?;
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

/// MySQL executable comments (`/*! … */`) run depending on the server version.
fn refuse_exec_comments(toks: &[Token]) -> Result<(), DbError> {
    if toks.iter().any(|t| t.kind == TokKind::ExecComment) {
        return Err(text_error(
            "Executable comments (/*! … */) cannot be applied here; remove them.",
        ));
    }
    Ok(())
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

/// Whether `text` holds a MySQL client `DELIMITER` line, as `split` reads one (a
/// `DELIMITER` word outside strings and comments that `split` takes as a directive).
fn has_delimiter_directive(text: &str, toks: &[Token]) -> bool {
    toks.iter().any(|t| {
        matches!(t.kind, TokKind::Word(w) if w.eq_ignore_ascii_case("DELIMITER"))
            && delimiter_directive(text, t.start).is_some()
    })
}

/// What opened a block the body scanner tracks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Block {
    Begin,
    If,
    /// A `CASE` statement (closed by `END CASE`).
    CaseStmt,
    /// A `CASE` expression (closed by `END`).
    CaseExpr,
    Loop,
    /// A `WHILE` before its `DO`.
    WhileCond,
    While,
    Repeat,
}

const QUOTE_BEGIN: &str =
    "BEGIN can only open a block at the start of a statement; quote it (`begin`) if it is a name.";
const MISMATCH: &str =
    "BEGIN … END does not match; check the blocks, and quote names such as `end`.";

/// Words that open or close blocks: as an unquoted label they are refused.
const BLOCK_WORDS: [&str; 7] = ["BEGIN", "END", "IF", "CASE", "LOOP", "WHILE", "REPEAT"];

/// Words never taken as an end label (processed as keywords instead).
const END_LABEL_KEYWORDS: [&str; 13] = [
    "BEGIN", "END", "IF", "CASE", "LOOP", "WHILE", "REPEAT", "ELSE", "ELSEIF", "THEN", "WHEN",
    "UNTIL", "DO",
];

/// Whether `t` is an identifier token: a word not starting with a digit, or a
/// backtick-quoted (or, under `ANSI_QUOTES`, double-quoted) name.
fn is_identifier(stmt: &str, t: &Token, dialect: Dialect) -> bool {
    match t.kind {
        TokKind::Word(w) => !w.starts_with(|c: char| c.is_ascii_digit()),
        TokKind::Quoted => match stmt.as_bytes()[t.start] {
            b'`' => true,
            b'"' => matches!(dialect, Dialect::MySql(m) if m.ansi_quotes),
            _ => false,
        },
        _ => false,
    }
}

/// The MySQL body scanner: the byte offset where the one compound statement of
/// `stmt` ends (its first `;` at paren depth 0 with no block open, or the end of the
/// text), refusing the text when anything but `;` follows, or when its blocks or
/// parentheses do not match.
///
/// Exact block matching, no name guessing (spec R6, controller ruling): a word right
/// after `.` is a name; otherwise `BEGIN`, `IF`, `CASE`, `LOOP`, `WHILE` and `REPEAT`
/// open blocks at the start of a statement, `CASE` elsewhere opens a CASE expression,
/// `BEGIN` elsewhere is refused, and every `END [IF|CASE|LOOP|WHILE|REPEAT]` must close
/// the innermost block of its kind. An unquoted `end` used as a name therefore always
/// leads to a refusal (quote it).
fn body_end(stmt: &str, dialect: Dialect) -> Result<usize, DbError> {
    let toks = tokenize(stmt, dialect);
    let header = parse_header(stmt, dialect)?;
    let after_name = toks
        .iter()
        .position(|t| t.start >= header.name_span.1)
        .unwrap_or(toks.len());
    let body = body_start(&toks, after_name, header.kind);
    let mut paren: i64 = 0;
    let mut blocks: Vec<Block> = Vec::new();
    // Token indexes known to start a statement (the body, handler statements).
    let mut starts: Vec<usize> = body.into_iter().collect();
    let mut at_start = false;
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        let start = at_start || starts.contains(&i);
        // A word after `ident.` is a name (`t.end`); after `1.` (a decimal) it is not.
        let prev_dot = i > 1
            && toks[i - 1].kind == TokKind::Punct(b'.')
            && is_identifier(stmt, &toks[i - 2], dialect);
        let mut next_start = false;
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
            TokKind::Punct(b';') if paren != 0 => {
                return Err(text_error(
                    "The text has an unclosed parenthesis before `;`; check the parentheses.",
                ));
            }
            TokKind::Punct(b';') if blocks.is_empty() => {
                if toks[i + 1..].iter().any(|t| t.kind != TokKind::Punct(b';')) {
                    return Err(text_error(AFTER_BODY));
                }
                return Ok(t.start);
            }
            TokKind::Punct(b';') => next_start = true,
            TokKind::Word(_) | TokKind::Quoted
                if paren == 0
                    && start
                    && !prev_dot
                    && toks
                        .get(i + 1)
                        .is_some_and(|n| n.kind == TokKind::Punct(b':')) =>
            {
                // `label: BEGIN | LOOP | WHILE | REPEAT`.
                if let TokKind::Word(w) = t.kind {
                    if BLOCK_WORDS.iter().any(|k| w.eq_ignore_ascii_case(k)) {
                        return Err(text_error(format!(
                            "The label {w} is a block keyword; quote it (`{w}`)."
                        )));
                    }
                }
                let opener = word_at(&toks, i + 2);
                if !["BEGIN", "LOOP", "WHILE", "REPEAT"]
                    .iter()
                    .any(|k| is_kw(opener, k))
                {
                    return Err(text_error(
                        "A label must be followed by BEGIN, LOOP, WHILE or REPEAT.",
                    ));
                }
                i += 1;
                next_start = true;
            }
            TokKind::Word(w) if paren == 0 && !prev_dot => {
                let kw = |k: &str| w.eq_ignore_ascii_case(k);
                let inner = blocks.last().copied();
                if kw("END") {
                    let next = word_at(&toks, i + 1);
                    let closes = [
                        ("IF", Block::If),
                        ("CASE", Block::CaseStmt),
                        ("LOOP", Block::Loop),
                        ("WHILE", Block::While),
                        ("REPEAT", Block::Repeat),
                    ]
                    .iter()
                    .find(|(k, _)| is_kw(next, k))
                    .map(|&(_, b)| b);
                    let Some(open) = blocks.pop() else {
                        return Err(text_error(
                            "The text has an unbalanced END; check the BEGIN … END blocks.",
                        ));
                    };
                    match closes {
                        Some(b) if b == open => i += 1,
                        None if matches!(open, Block::Begin | Block::CaseExpr) => {}
                        _ => return Err(text_error(MISMATCH)),
                    }
                    // An optional end label: a name that is not a keyword of the block
                    // syntax, followed by `;` or the end of the text.
                    let label = toks.get(i + 1).is_some_and(|n| match n.kind {
                        TokKind::Word(w) => {
                            !END_LABEL_KEYWORDS.iter().any(|k| w.eq_ignore_ascii_case(k))
                        }
                        TokKind::Quoted => is_identifier(stmt, n, dialect),
                        _ => false,
                    }) && toks
                        .get(i + 2)
                        .is_none_or(|n| n.kind == TokKind::Punct(b';'));
                    if label
                        && matches!(
                            open,
                            Block::Begin | Block::Loop | Block::While | Block::Repeat
                        )
                    {
                        i += 1;
                    }
                } else if start && kw("BEGIN") {
                    blocks.push(Block::Begin);
                    next_start = true;
                } else if kw("BEGIN") {
                    return Err(text_error(QUOTE_BEGIN));
                } else if kw("CASE") {
                    blocks.push(if start {
                        Block::CaseStmt
                    } else {
                        Block::CaseExpr
                    });
                } else if start && kw("IF") {
                    blocks.push(Block::If);
                } else if start && kw("LOOP") {
                    blocks.push(Block::Loop);
                    next_start = true;
                } else if start && kw("REPEAT") {
                    blocks.push(Block::Repeat);
                    next_start = true;
                } else if start && kw("WHILE") {
                    blocks.push(Block::WhileCond);
                } else if kw("DO") && inner == Some(Block::WhileCond) {
                    blocks.pop();
                    blocks.push(Block::While);
                    next_start = true;
                } else if (kw("THEN") || kw("ELSE"))
                    && matches!(inner, Some(Block::If | Block::CaseStmt))
                {
                    next_start = true;
                } else if start && kw("DECLARE") {
                    if let Some(k) = handler_start(&toks, i) {
                        starts.push(k);
                    }
                }
            }
            _ => {}
        }
        at_start = next_start;
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

fn kw_at(toks: &[Token], i: usize, k: &str) -> bool {
    is_kw(word_at(toks, i), k)
}

/// The index past the parenthesised group opening at `i`.
fn skip_group(toks: &[Token], i: usize) -> Option<usize> {
    if toks.get(i)?.kind != TokKind::Open {
        return None;
    }
    let mut depth = 0i64;
    for (j, t) in toks.iter().enumerate().skip(i) {
        match t.kind {
            TokKind::Open => depth += 1,
            TokKind::Close => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The index past routine characteristics starting at `i`.
fn skip_characteristics(toks: &[Token], mut i: usize) -> usize {
    loop {
        let k = |o: usize, w: &str| kw_at(toks, i + o, w);
        let comment = k(0, "COMMENT") && toks.get(i + 1).is_some_and(|t| t.kind == TokKind::Quoted);
        let two = comment
            || ((k(0, "LANGUAGE") || k(0, "CONTAINS") || k(0, "NO")) && k(1, "SQL"))
            || (k(0, "NOT") && k(1, "DETERMINISTIC"));
        let three = ((k(0, "READS") || k(0, "MODIFIES")) && k(1, "SQL") && k(2, "DATA"))
            || (k(0, "SQL") && k(1, "SECURITY") && (k(2, "DEFINER") || k(2, "INVOKER")));
        i += if two {
            2
        } else if three {
            3
        } else if k(0, "DETERMINISTIC") {
            1
        } else {
            return i;
        };
    }
}

/// The index past a `RETURNS` data type starting at `i` (a type name, an optional
/// length, and the usual attributes); a type it does not know leaves the index
/// inside the type, so the body's first token is not found (an under-count).
fn skip_type(toks: &[Token], mut i: usize) -> Option<usize> {
    word_at(toks, i)?;
    i += 1;
    loop {
        let k = |o: usize, w: &str| kw_at(toks, i + o, w);
        if toks.get(i).is_some_and(|t| t.kind == TokKind::Open) {
            i = skip_group(toks, i)?;
        } else if [
            "UNSIGNED",
            "SIGNED",
            "ZEROFILL",
            "BINARY",
            "ASCII",
            "UNICODE",
            "PRECISION",
            "VARYING",
        ]
        .iter()
        .any(|w| k(0, w))
        {
            i += 1;
        } else if (k(0, "CHARACTER") || k(0, "CHAR"))
            && k(1, "SET")
            && word_at(toks, i + 2).is_some()
        {
            i += 3;
        } else if (k(0, "CHARSET") || k(0, "COLLATE"))
            && toks
                .get(i + 1)
                .is_some_and(|t| matches!(t.kind, TokKind::Word(_) | TokKind::Quoted))
        {
            i += 2;
        } else {
            return Some(i);
        }
    }
}

/// The index of the body's first token, from the token just past the name.
fn body_start(toks: &[Token], after_name: usize, kind: ObjectKind) -> Option<usize> {
    let depth0 = |from: usize| {
        let mut depth = 0i64;
        toks.iter()
            .enumerate()
            .skip(from)
            .filter_map(move |(j, t)| {
                match t.kind {
                    TokKind::Open => depth += 1,
                    TokKind::Close => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(j)
            })
    };
    match kind {
        ObjectKind::Procedure => Some(skip_characteristics(toks, skip_group(toks, after_name)?)),
        ObjectKind::Function => {
            let i = skip_group(toks, after_name)?;
            if !kw_at(toks, i, "RETURNS") {
                return None;
            }
            Some(skip_characteristics(toks, skip_type(toks, i + 1)?))
        }
        ObjectKind::Trigger => {
            let row = depth0(after_name).find(|&j| {
                kw_at(toks, j, "FOR") && kw_at(toks, j + 1, "EACH") && kw_at(toks, j + 2, "ROW")
            })? + 3;
            if !(kw_at(toks, row, "FOLLOWS") || kw_at(toks, row, "PRECEDES")) {
                return Some(row);
            }
            let is_ident = |j: usize| {
                toks.get(j)
                    .is_some_and(|t| matches!(t.kind, TokKind::Word(_) | TokKind::Quoted))
            };
            if !is_ident(row + 1) {
                return None;
            }
            if toks
                .get(row + 2)
                .is_some_and(|t| t.kind == TokKind::Punct(b'.'))
            {
                return is_ident(row + 3).then_some(row + 4);
            }
            Some(row + 2)
        }
        ObjectKind::Event => depth0(after_name)
            .find(|&j| kw_at(toks, j, "DO"))
            .map(|j| j + 1),
        _ => None,
    }
}

/// For `DECLARE <action> HANDLER FOR <conditions>` at `i`, the index of the handler
/// statement's first token.
fn handler_start(toks: &[Token], i: usize) -> Option<usize> {
    if !(kw_at(toks, i + 2, "HANDLER") && kw_at(toks, i + 3, "FOR")) {
        return None;
    }
    let mut j = i + 4;
    loop {
        if kw_at(toks, j, "SQLSTATE") {
            j += 1;
            if kw_at(toks, j, "VALUE") {
                j += 1;
            }
            if toks.get(j)?.kind != TokKind::Quoted {
                return None;
            }
            j += 1;
        } else if kw_at(toks, j, "NOT") && kw_at(toks, j + 1, "FOUND") {
            j += 2;
        } else {
            word_at(toks, j)?;
            j += 1;
        }
        if toks.get(j).is_some_and(|t| t.kind == TokKind::Punct(b',')) {
            j += 1;
        } else {
            return Some(j);
        }
    }
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
            "CREATE PROCEDURE p() BEGIN SELECT `end` FROM t; SELECT a AS `begin` FROM t; SELECT t.case, t.select, t.as, t.if, t.loop FROM t; END",
            "CREATE PROCEDURE p(a INT) IF a > 1 THEN SELECT 1; ELSEIF a = 0 THEN BEGIN SELECT 0; END; ELSE SELECT 2; END IF",
            "CREATE PROCEDURE p(a INT) CASE a WHEN 1 THEN SELECT 1; ELSE CASE WHEN a > 2 THEN SELECT 2; ELSE BEGIN END; END CASE; END CASE",
            "CREATE PROCEDURE p() l: LOOP LEAVE l; END LOOP l",
            "CREATE PROCEDURE p() `l`: BEGIN SELECT `t`.end, t.begin, 1.5 FROM t; END `l`",
            "CREATE PROCEDURE p() BEGIN DECLARE done INT DEFAULT 0; DECLARE x INT; DECLARE c CURSOR FOR SELECT a FROM t WHERE IF(a, 1, 0) = 1; DECLARE CONTINUE HANDLER FOR NOT FOUND SET done = 1; OPEN c; rd: LOOP FETCH c INTO x; IF done THEN LEAVE rd; END IF; ITERATE rd; END LOOP rd; CLOSE c; SELECT REPEAT('a', 2), CASE WHEN x THEN 1 END AS y FROM t WHERE CASE x WHEN 1 THEN 1 ELSE 0 END = 1; END",
            "CREATE PROCEDURE p(a INT) BEGIN IF CASE WHEN a THEN 1 END = 1 THEN SET a = 1; END IF; WHILE CASE WHEN a > 0 THEN 1 ELSE 0 END DO SET a = a - 1; END WHILE; REPEAT SET a = a + 1; UNTIL CASE WHEN a > 2 THEN 1 END END REPEAT; END",
            "CREATE PROCEDURE p(a INT) BEGIN IF a THEN BEGIN SELECT 1; END; ELSEIF a > 2 THEN BEGIN END; ELSE BEGIN SELECT 2; END; END IF; END",
            "CREATE PROCEDURE p(a INT) BEGIN CASE WHEN a = 1 THEN BEGIN SELECT 1; END; ELSE BEGIN END; END CASE; END",
            "CREATE PROCEDURE p(a INT) BEGIN WHILE a > 0 DO BEGIN SET a = a - 1; END; END WHILE; l: REPEAT BEGIN SET a = a + 1; END; UNTIL a > 3 END REPEAT l; LOOP BEGIN LEAVE l; END; END LOOP; END",
            "CREATE PROCEDURE p() BEGIN DECLARE EXIT HANDLER FOR SQLSTATE VALUE '23000', NOT FOUND, 1062, SQLWARNING BEGIN ROLLBACK; END; SELECT 1; END",
            "CREATE PROCEDURE p() BEGIN DECLARE CONTINUE HANDLER FOR SQLSTATE '42S02' SET @x = 1; SELECT 1; END",
            "CREATE PROCEDURE p() CASE WHEN 1 THEN BEGIN SELECT 1; END; END CASE",
            "CREATE PROCEDURE p() LANGUAGE SQL NOT DETERMINISTIC READS SQL DATA SQL SECURITY INVOKER COMMENT 'x' BEGIN SELECT 1; END",
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
        for f in [
            "CREATE FUNCTION f() RETURNS INT BEGIN RETURN 1; END",
            "CREATE FUNCTION f() RETURNS varchar(10) CHARSET utf8mb4 COLLATE utf8mb4_0900_ai_ci DETERMINISTIC BEGIN RETURN 'a'; END",
            "CREATE FUNCTION f() RETURNS DECIMAL(10,2) UNSIGNED NO SQL BEGIN RETURN 1; END",
            "CREATE FUNCTION f() RETURNS INT l: BEGIN RETURN 1; END l",
        ] {
            assert_eq!(ok(f, ObjectKind::Function), f);
        }
        for trg in [
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW FOLLOWS other BEGIN SET NEW.a = 1; END",
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW PRECEDES `db`.`o` BEGIN SET NEW.a = 1; END",
        ] {
            assert_eq!(ok(trg, ObjectKind::Trigger), trg);
        }
        let ev = "CREATE EVENT e ON SCHEDULE EVERY 1 DAY STARTS NOW() ON COMPLETION PRESERVE DISABLE COMMENT 'c' DO BEGIN DELETE FROM t; END";
        assert_eq!(ok(ev, ObjectKind::Event), ev);
    }

    #[test]
    fn unquoted_begin_name_is_refused() {
        for text in [
            "CREATE PROCEDURE p() BEGIN SELECT a AS begin FROM t; END",
            "CREATE PROCEDURE p() BEGIN SELECT a FROM t ORDER BY begin; END",
            "CREATE FUNCTION f() RETURNS INT RETURN a AND begin",
            "CREATE PROCEDURE p() BEGIN DO begin; END",
            "CREATE PROCEDURE p() begin: BEGIN END",
            "CREATE FUNCTION f() RETURNS NATIONAL CHAR(10) BEGIN RETURN 'a'; END",
        ] {
            let m = refused(isolate(text, ObjectKind::Procedure, MY, &mysql8()));
            assert!(m.contains("quote it"), "{text}: {m}");
        }
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
            // Review: a name BEGIN / CASE must never hide a second statement.
            "CREATE PROCEDURE p() SELECT a FROM t ORDER BY begin; DELETE FROM t; END",
            "CREATE PROCEDURE p() SELECT t.case FROM t; DROP TABLE x; END",
            "CREATE PROCEDURE p() BEGIN SELECT t.case FROM t; END; DROP TABLE x; END",
            "CREATE PROCEDURE p() SELECT * FROM t ORDER BY begin; DELETE FROM t WHERE end < NOW()",
            "CREATE PROCEDURE p() BEGIN DO begin; END; DROP TABLE x; END",
            "CREATE FUNCTION f() RETURNS INT RETURN a AND begin; DROP TABLE x; END",
            "CREATE PROCEDURE p() SELECT CASE WHEN a THEN begin END; DROP TABLE x; END",
            "CREATE PROCEDURE p() BEGIN SELECT CASE WHEN a THEN begin END; END; DROP TABLE x; END",
            // Re-review: name `end` / keywords after `.` must not unbalance the count.
            "CREATE PROCEDURE p() SELECT CASE WHEN end THEN begin END FROM t; DROP TABLE x; END",
            "CREATE PROCEDURE p() BEGIN SELECT CASE WHEN end THEN begin END FROM t; END; DROP TABLE x; END",
            "CREATE EVENT e ON SCHEDULE EVERY 1 DAY DO SELECT CASE WHEN end THEN begin END FROM t; DELETE FROM t; END",
            "CREATE PROCEDURE p() SELECT CASE WHEN a THEN 1 ELSE t.select END FROM t; DROP TABLE x; END",
            "CREATE PROCEDURE p() BEGIN SET @v = CASE WHEN a THEN 1 ELSE t.as END; END; DROP TABLE x; END",
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW SET NEW.a = CASE WHEN NEW.b THEN 1 ELSE t.select END; DELETE FROM t; END",
            "CREATE PROCEDURE p() BEGIN SELECT end FROM t; END; DROP TABLE x",
            "CREATE PROCEDURE p() BEGIN IF a THEN SELECT 1; END; END IF; END; DROP TABLE x",
            "CREATE PROCEDURE p() BEGIN CASE a WHEN 1 THEN SELECT 1; END; END; DROP TABLE x",
            "CREATE PROCEDURE p() l: LOOP SELECT 1; END l; DROP TABLE x",
            "CREATE PROCEDURE p() x: SELECT 1; DROP TABLE x",
            // Round 3: a block keyword is never an end label; `1.` is a decimal.
            "CREATE PROCEDURE p() BEGIN BEGIN SELECT 1; END END; DROP TABLE x; END",
            "CREATE PROCEDURE p() BEGIN l: LOOP SELECT 1; END LOOP END; DROP TABLE x; END",
            "CREATE PROCEDURE p() BEGIN WHILE a DO SELECT 1; END WHILE END; DROP TABLE x; END",
            "CREATE PROCEDURE p() BEGIN REPEAT SELECT 1; UNTIL a END REPEAT END; DROP TABLE x; END",
            "CREATE PROCEDURE p() BEGIN BEGIN SELECT 1; END l END; DROP TABLE x; END",
            "CREATE PROCEDURE p() SELECT CASE WHEN a THEN 1. END FROM t; DROP TABLE x; END",
            "CREATE PROCEDURE p() BEGIN SET @v = CASE WHEN a THEN 1 ELSE 2. END; END; DROP TABLE x; END",
            "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW SET NEW.a = CASE WHEN NEW.b THEN 1. END; DELETE FROM t; END",
            "CREATE PROCEDURE p() BEGIN SELECT 'a'.end; END; DROP TABLE x; END",
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
