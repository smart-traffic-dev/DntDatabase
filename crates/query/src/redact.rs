//! Password redaction for query history and logs (spec §8.1).
//!
//! Works over the shared `core` lexer, so strings, comments, E-strings and
//! dollar quotes follow the statement's dialect. The rules err on the side of
//! redacting: a literal is replaced whenever it may be a secret, e.g. the
//! right-hand side of `password = '…'` in a plain `SELECT` (accepted
//! over-redaction). Statements without a trigger stay byte-identical.
//!
//! Triggers (the literal after them, past an optional `=`, `:=`, `TO`, or a
//! parenthesised argument list whose literals are all redacted):
//! - `IDENTIFIED BY`, `IDENTIFIED WITH <plugin> BY|AS`, `IDENTIFIED VIA <plugin>
//!   USING` (any `BY`/`AS`/`USING` once `IDENTIFIED` was seen);
//! - any word or quoted identifier containing `PASSWORD` or `PASSWD`
//!   (`PASSWORD`, `MASTER_PASSWORD`, `SOURCE_PASSWORD`, `ENCRYPTED PASSWORD`,
//!   `OPTIONS (password '…')`, `PASSWORD(…)`, `@password := …`), and
//!   `ENCRYPTION_KEY`;
//! - `SET PASSWORD [FOR <user>] =`: every literal after the `=`;
//! - `REPLACE '<current password>'` (MySQL 8 `ALTER USER … REPLACE`);
//! - `CONNECTION '…'` (FEDERATED URLs, PG subscriptions), and any string whose
//!   content holds `password=` / `pwd=` (libpq / ODBC connection strings).
//!
//! Under MySQL a statement is scanned twice: as the splitter lexes it, and with
//! every executable-comment opener (`/*!NNNNN`, `/*M!NNNNNN`) blanked, as a
//! server that runs the comment body lexes it. The union of both is redacted.

use dnt_core::sql::{tokenize, Dialect, TokKind, Token};

/// What every redacted literal becomes.
pub const REDACTED: &str = "'[REDACTED]'";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub changed: bool,
}

/// Replaces every possible secret literal of `stmt` with [`REDACTED`]. Never
/// panics; linear in the length of `stmt`.
pub fn redact(stmt: &str, dialect: Dialect) -> Redacted {
    let mut spans = Vec::new();
    collect(stmt, &tokenize(stmt, dialect), dialect, &mut spans);
    if dialect.is_mysql() {
        if let Some(inlined) = blank_exec_openers(stmt) {
            collect(&inlined, &tokenize(&inlined, dialect), dialect, &mut spans);
        }
    }
    if spans.is_empty() {
        return Redacted {
            text: stmt.to_string(),
            changed: false,
        };
    }
    spans.sort_unstable();
    let mut text = String::with_capacity(stmt.len());
    let (mut at, mut i) = (0, 0);
    while i < spans.len() {
        let (start, mut end) = spans[i];
        i += 1;
        while i < spans.len() && spans[i].0 <= end {
            end = end.max(spans[i].1);
            i += 1;
        }
        // Spans are char boundaries of `stmt` (blanking only rewrites ASCII).
        text.push_str(stmt.get(at..start).unwrap_or_default());
        text.push_str(REDACTED);
        at = end;
    }
    text.push_str(stmt.get(at..).unwrap_or_default());
    Redacted {
        text,
        changed: true,
    }
}

/// `stmt` with every `/*!digits` / `/*M!digits` opener replaced by spaces (same
/// byte offsets), or `None` when it has none.
fn blank_exec_openers(stmt: &str) -> Option<String> {
    let mut b = stmt.as_bytes().to_vec();
    let mut found = false;
    let mut i = 0;
    while i + 2 < b.len() {
        let len = if b[i..].starts_with(b"/*!") {
            3
        } else if b[i..].starts_with(b"/*M!") {
            4
        } else {
            i += 1;
            continue;
        };
        let mut j = i + len;
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        b[i..j].fill(b' ');
        found = true;
        i = j;
    }
    found.then(|| String::from_utf8(b).ok()).flatten()
}

/// Pushes the byte spans of `src` to redact, per the triggers in the module doc.
fn collect(src: &str, toks: &[Token], dialect: Dialect, out: &mut Vec<(usize, usize)>) {
    let mut scan = Scan {
        toks,
        out,
        covered: None,
        tail_searched: false,
    };
    let mut identified = false;
    for i in 0..toks.len() {
        match toks[i].kind {
            TokKind::Word(w) => {
                let up = w.to_ascii_uppercase();
                match up.as_str() {
                    "IDENTIFIED" => identified = true,
                    "PASSWORD" if is_word(toks, i.wrapping_sub(1), "SET") => scan.set_password(i),
                    "BY" | "AS" | "USING" if identified => scan.target(i + 1, true, true),
                    "REPLACE" => scan.target(i + 1, false, false),
                    "CONNECTION" => scan.target(i + 1, false, true),
                    _ if is_secret_name(&up) => scan.target(i + 1, true, true),
                    _ => {}
                }
            }
            TokKind::Quoted => {
                let t = toks[i];
                let text = src.get(t.start..t.end).unwrap_or_default();
                if is_ident_quote(text, dialect) {
                    if is_secret_name(&text.to_ascii_uppercase()) {
                        scan.target(i + 1, true, true);
                    }
                } else if has_kv_secret(text.as_bytes()) {
                    scan.out.push((t.start, t.end));
                }
            }
            _ => {}
        }
    }
}

struct Scan<'t, 'a, 'o> {
    toks: &'t [Token<'a>],
    out: &'o mut Vec<(usize, usize)>,
    /// Every literal up to this token index was already redacted by a region.
    covered: Option<usize>,
    /// A `SET PASSWORD FOR …` search for `=` already ran to the end.
    tail_searched: bool,
}

impl Scan<'_, '_, '_> {
    /// `SET PASSWORD` at token `i`: with `=` or `FOR`, every literal after the
    /// `=`; otherwise (`OPTIONS (SET password '…')`) a plain trigger.
    fn set_password(&mut self, i: usize) {
        let toks = self.toks;
        if is_punct(toks, i + 1, b'=') {
            self.region(i + 2, toks.len());
        } else if is_word(toks, i + 1, "FOR") {
            if !self.tail_searched {
                self.tail_searched = true;
                if let Some(eq) = (i + 2..toks.len()).find(|&j| is_punct(toks, j, b'=')) {
                    self.region(eq + 1, toks.len());
                }
            }
        } else {
            self.target(i + 1, true, true);
        }
    }

    /// Redacts the literal at token `k`. With `skip`, first steps over up to
    /// three `=`, `:` or `TO` tokens, and a `(` there redacts every literal up
    /// to its matching `)`. `number` also redacts a bare numeric literal.
    fn target(&mut self, k: usize, number: bool, skip: bool) {
        let toks = self.toks;
        let mut k = k;
        if skip {
            let mut stepped = 0;
            while stepped < 3
                && (is_punct(toks, k, b'=') || is_punct(toks, k, b':') || is_word(toks, k, "TO"))
            {
                k += 1;
                stepped += 1;
            }
            if toks.get(k).is_some_and(|t| t.kind == TokKind::Open) {
                // Inside an earlier region, every literal is already redacted.
                if self.covered.is_some_and(|c| k <= c) {
                    return;
                }
                let end = matching_close(toks, k);
                self.region(k + 1, end);
                return;
            }
        }
        // Charset introducers and prefixes glued to the string: `_utf8mb4'…'`,
        // `N'…'`, `X'…'`, PG `U&'…'`.
        if glued(toks, k) {
            k += 1;
        } else if is_word(toks, k, "U") && is_punct(toks, k + 1, b'&') && glued(toks, k + 1) {
            k += 2;
        }
        let Some(t) = toks.get(k) else { return };
        match t.kind {
            TokKind::Quoted => {
                // Adjacent literals are concatenated by the server.
                let mut end = t.end;
                let mut j = k + 1;
                while let Some(n) = toks.get(j).filter(|n| n.kind == TokKind::Quoted) {
                    end = n.end;
                    j += 1;
                }
                self.out.push((t.start, end));
            }
            TokKind::Word(w) if number && w.starts_with(|c: char| c.is_ascii_digit()) => {
                self.out.push((t.start, t.end));
            }
            _ => {}
        }
    }

    /// Redacts every literal among tokens `from..to`, unless an enclosing
    /// region already did (keeps nested `PASSWORD(PASSWORD(…))` linear).
    fn region(&mut self, from: usize, to: usize) {
        if self.covered.is_some_and(|c| from <= c) {
            return;
        }
        for t in self.toks.get(from..to).unwrap_or_default() {
            match t.kind {
                TokKind::Quoted => self.out.push((t.start, t.end)),
                TokKind::Word(w) if w.starts_with(|c: char| c.is_ascii_digit()) => {
                    self.out.push((t.start, t.end))
                }
                _ => {}
            }
        }
        self.covered = Some(to);
    }
}

/// Index of the `)` matching the `(` at `open`, or the token count.
fn matching_close(toks: &[Token], open: usize) -> usize {
    let mut depth = 0usize;
    for (j, t) in toks.iter().enumerate().skip(open) {
        match t.kind {
            TokKind::Open => depth += 1,
            TokKind::Close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
    }
    toks.len()
}

/// Token `k` is a word or `&` immediately followed (no space) by a quoted token.
fn glued(toks: &[Token], k: usize) -> bool {
    match (toks.get(k), toks.get(k + 1)) {
        (Some(a), Some(b)) => {
            matches!(a.kind, TokKind::Word(_) | TokKind::Punct(b'&'))
                && b.kind == TokKind::Quoted
                && a.end == b.start
        }
        _ => false,
    }
}

fn is_word(toks: &[Token], i: usize, kw: &str) -> bool {
    matches!(toks.get(i), Some(Token { kind: TokKind::Word(w), .. }) if w.eq_ignore_ascii_case(kw))
}

fn is_punct(toks: &[Token], i: usize, c: u8) -> bool {
    toks.get(i).is_some_and(|t| t.kind == TokKind::Punct(c))
}

/// An uppercased name that marks the next literal as a secret.
fn is_secret_name(up: &str) -> bool {
    up.contains("PASSWORD") || up.contains("PASSWD") || up == "ENCRYPTION_KEY"
}

/// Whether the quoted token `text` is an identifier rather than a string.
fn is_ident_quote(text: &str, dialect: Dialect) -> bool {
    match (text.as_bytes().first(), dialect) {
        (Some(b'`'), _) | (Some(b'"'), Dialect::Postgres) => true,
        (Some(b'"'), Dialect::MySql(m)) => m.ansi_quotes,
        _ => false,
    }
}

/// Whether a string literal holds a `password=` / `pwd=` key (case-insensitive,
/// optional spaces before `=`), as connection strings do.
fn has_kv_secret(b: &[u8]) -> bool {
    (0..b.len()).any(|i| {
        [&b"password"[..], b"pwd"].iter().any(|key| {
            b.get(i..i + key.len())
                .is_some_and(|w| w.eq_ignore_ascii_case(key))
                && b[i + key.len()..]
                    .iter()
                    .find(|c| !c.is_ascii_whitespace())
                    .is_some_and(|&c| c == b'=')
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::sql::{split, MySqlMode};

    const MY: Dialect = Dialect::MYSQL;
    const PG: Dialect = Dialect::Postgres;

    fn red(s: &str, d: Dialect) -> String {
        let r = redact(s, d);
        assert_eq!(r.changed, r.text != s, "changed flag for {s:?}");
        r.text
    }

    fn assert_red(input: &str, d: Dialect, want: &str) {
        let r = redact(input, d);
        assert_eq!(r.text, want, "input {input:?}");
        assert!(r.changed, "changed for {input:?}");
    }

    #[test]
    fn redact_identified_by() {
        assert_red(
            "CREATE USER 'u'@'%' IDENTIFIED BY 's3cr''et'",
            MY,
            "CREATE USER 'u'@'%' IDENTIFIED BY '[REDACTED]'",
        );
    }

    #[test]
    fn redact_identified_with_by() {
        assert_red(
            "ALTER USER u IDENTIFIED WITH caching_sha2_password BY 'x'",
            MY,
            "ALTER USER u IDENTIFIED WITH caching_sha2_password BY '[REDACTED]'",
        );
        assert_red(
            "CREATE USER u IDENTIFIED WITH mysql_native_password AS '*ABC'",
            MY,
            "CREATE USER u IDENTIFIED WITH mysql_native_password AS '[REDACTED]'",
        );
        assert_red(
            "GRANT ALL ON *.* TO u IDENTIFIED BY PASSWORD '*ABC'",
            MY,
            "GRANT ALL ON *.* TO u IDENTIFIED BY PASSWORD '[REDACTED]'",
        );
    }

    #[test]
    fn redact_identified_via_using() {
        assert_red(
            "CREATE USER u IDENTIFIED VIA ed25519 USING PASSWORD('x')",
            MY,
            "CREATE USER u IDENTIFIED VIA ed25519 USING PASSWORD('[REDACTED]')",
        );
        assert_red(
            "CREATE USER u IDENTIFIED VIA ed25519 USING 'hash' OR unix_socket",
            MY,
            "CREATE USER u IDENTIFIED VIA ed25519 USING '[REDACTED]' OR unix_socket",
        );
    }

    #[test]
    fn redact_set_password_forms() {
        assert_red(
            "SET PASSWORD FOR 'u'@'h' = 'x'",
            MY,
            "SET PASSWORD FOR 'u'@'h' = '[REDACTED]'",
        );
        assert_red(
            "SET PASSWORD = PASSWORD('x')",
            MY,
            "SET PASSWORD = PASSWORD('[REDACTED]')",
        );
        assert_red(
            "SET PASSWORD FOR u = OLD_PASSWORD(CONCAT('a', 'b'))",
            MY,
            "SET PASSWORD FOR u = OLD_PASSWORD(CONCAT('[REDACTED]', '[REDACTED]'))",
        );
        assert_red(
            "ALTER USER u IDENTIFIED BY 'new' REPLACE 'old' RETAIN CURRENT PASSWORD",
            MY,
            "ALTER USER u IDENTIFIED BY '[REDACTED]' REPLACE '[REDACTED]' RETAIN CURRENT PASSWORD",
        );
    }

    #[test]
    fn redact_replication_passwords() {
        assert_red(
            "CHANGE MASTER TO MASTER_PASSWORD='x', MASTER_USER='u'",
            MY,
            "CHANGE MASTER TO MASTER_PASSWORD='[REDACTED]', MASTER_USER='u'",
        );
        assert_red(
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD = \"x\"",
            MY,
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD = '[REDACTED]'",
        );
    }

    #[test]
    fn redact_pg_role_passwords() {
        assert_red(
            "CREATE ROLE r LOGIN PASSWORD 'x'",
            PG,
            "CREATE ROLE r LOGIN PASSWORD '[REDACTED]'",
        );
        assert_red(
            "ALTER USER r WITH ENCRYPTED PASSWORD E'x\\'y'",
            PG,
            "ALTER USER r WITH ENCRYPTED PASSWORD '[REDACTED]'",
        );
        assert_red(
            "ALTER ROLE r PASSWORD $p$x$p$",
            PG,
            "ALTER ROLE r PASSWORD '[REDACTED]'",
        );
        assert_red(
            "ALTER ROLE r PASSWORD 'x' VALID UNTIL '2030-01-01'",
            PG,
            "ALTER ROLE r PASSWORD '[REDACTED]' VALID UNTIL '2030-01-01'",
        );
    }

    #[test]
    fn redact_pg_server_options() {
        assert_red(
            "CREATE USER MAPPING FOR u SERVER s OPTIONS (user 'u', password 'x')",
            PG,
            "CREATE USER MAPPING FOR u SERVER s OPTIONS (user 'u', password '[REDACTED]')",
        );
        assert_red(
            "ALTER SERVER s OPTIONS (SET password 'x')",
            PG,
            "ALTER SERVER s OPTIONS (SET password '[REDACTED]')",
        );
        assert_red(
            "ALTER USER MAPPING FOR u SERVER s OPTIONS (ADD \"password\" 'x')",
            PG,
            "ALTER USER MAPPING FOR u SERVER s OPTIONS (ADD \"password\" '[REDACTED]')",
        );
        assert_red(
            "CREATE SERVER s FOREIGN DATA WRAPPER mysql OPTIONS (USER 'u', PASSWORD 'x')",
            MY,
            "CREATE SERVER s FOREIGN DATA WRAPPER mysql OPTIONS (USER 'u', PASSWORD '[REDACTED]')",
        );
    }

    #[test]
    fn redact_encryption_key() {
        assert_red(
            "ALTER TABLE t ENCRYPTION_KEY = 'x'",
            MY,
            "ALTER TABLE t ENCRYPTION_KEY = '[REDACTED]'",
        );
    }

    #[test]
    fn redact_quotes_in_password() {
        let cases = [
            (MY, "CREATE USER u IDENTIFIED BY 'a\\'b$c'", "a\\'b$c"),
            (PG, "ALTER ROLE r PASSWORD 'a''b\\c$'", "a''b\\c$"),
            (PG, "ALTER ROLE r PASSWORD $$a'b$$", "a'b"),
        ];
        for (d, input, secret) in cases {
            let input = format!("{input}; SELECT 1");
            let first = &split(&input, d)[0].text;
            let out = red(first, d);
            assert!(!out.contains(secret), "{out}");
            assert!(out.contains(REDACTED), "{out}");
            let joined = format!("{out}; SELECT 2");
            assert_eq!(split(&joined, d).len(), 2, "{joined}");
        }
    }

    #[test]
    fn redact_untouched_statements_are_byte_identical() {
        for d in [MY, PG] {
            for s in [
                "SELECT 'password' AS label",
                "SELECT 1",
                "UPDATE t SET note = 'by'",
                "INSERT INTO t VALUES ('IDENTIFIED BY')",
                "KILL CONNECTION 5",
                "SELECT REPLACE('a', 'b', 'c')",
                "ALTER USER u PASSWORD EXPIRE INTERVAL 90 DAY",
                "",
            ] {
                let r = redact(s, d);
                assert_eq!(r.text, s);
                assert!(!r.changed, "{s}");
            }
        }
    }

    #[test]
    fn redact_password_column_comparison_is_redacted_conservatively() {
        assert_red(
            "SELECT * FROM u WHERE password = 'x'",
            MY,
            "SELECT * FROM u WHERE password = '[REDACTED]'",
        );
        assert_red(
            "UPDATE u SET pass_hash = 1, user_password = 'x' WHERE id = 1",
            PG,
            "UPDATE u SET pass_hash = 1, user_password = '[REDACTED]' WHERE id = 1",
        );
    }

    #[test]
    fn redact_literal_variants() {
        // Charset introducers, national and Unicode strings, adjacent literals
        // (concatenated by the server) and bare numbers.
        assert_red(
            "CREATE USER u IDENTIFIED BY _utf8mb4'x' 'y'",
            MY,
            "CREATE USER u IDENTIFIED BY _utf8mb4'[REDACTED]'",
        );
        assert_red(
            "SELECT * FROM t WHERE password = N'x'",
            MY,
            "SELECT * FROM t WHERE password = N'[REDACTED]'",
        );
        assert_red(
            "ALTER ROLE r PASSWORD U&'x'",
            PG,
            "ALTER ROLE r PASSWORD U&'[REDACTED]'",
        );
        assert_red(
            "SELECT * FROM t WHERE password = 123456",
            MY,
            "SELECT * FROM t WHERE password = '[REDACTED]'",
        );
        assert_red("SET @password := 'x'", MY, "SET @password := '[REDACTED]'");
    }

    #[test]
    fn redact_inside_executable_comments() {
        assert_red(
            "/*!50706 CREATE USER u IDENTIFIED BY 'x' */",
            MY,
            "/*!50706 CREATE USER u IDENTIFIED BY '[REDACTED]' */",
        );
        // The executing server reads `'a*/b'` as one string.
        let out = red("/*!50000 CREATE USER u IDENTIFIED BY 'a*/b' */", MY);
        assert!(!out.contains("a*/b") && !out.contains(" b"), "{out}");
        // The splitter (and a server ignoring the comment) reads `'pw'` as code.
        let out = red(
            "/*!40101 SET @x='a */ CREATE USER u IDENTIFIED BY 'pw' -- '",
            MY,
        );
        assert!(!out.contains("pw"), "{out}");
        // PG has no executable comments.
        let s = "/*!50706 CREATE USER u IDENTIFIED BY 'x' */ SELECT 1";
        assert_eq!(red(s, PG), s);
    }

    #[test]
    fn redact_connection_strings() {
        assert_red(
            "CREATE SUBSCRIPTION s CONNECTION 'host=h password=x' PUBLICATION p",
            PG,
            "CREATE SUBSCRIPTION s CONNECTION '[REDACTED]' PUBLICATION p",
        );
        assert_red(
            "SELECT dblink_connect('c', 'dbname=d Password = x')",
            PG,
            "SELECT dblink_connect('c', '[REDACTED]')",
        );
        assert_red(
            "CREATE TABLE t (a INT) ENGINE=FEDERATED CONNECTION='mysql://u:p@h/db/t'",
            MY,
            "CREATE TABLE t (a INT) ENGINE=FEDERATED CONNECTION='[REDACTED]'",
        );
    }

    #[test]
    fn redact_respects_sql_mode() {
        let nbe = Dialect::MySql(MySqlMode {
            ansi_quotes: false,
            no_backslash_escapes: true,
        });
        // Under NO_BACKSLASH_ESCAPES `'a\'` ends the password.
        assert_eq!(
            red("CREATE USER u IDENTIFIED BY 'a\\' COMMENT 'c'", nbe),
            "CREATE USER u IDENTIFIED BY '[REDACTED]' COMMENT 'c'"
        );
    }

    #[test]
    fn redact_never_panics_and_is_linear() {
        for s in [
            "PASSWORD",
            "PASSWORD =",
            "PASSWORD (",
            "SET PASSWORD FOR",
            "IDENTIFIED BY",
            "IDENTIFIED BY 'x",
            "PASSWORD $$x",
            "PASSWORD E'",
            "/*!",
            "/*!5",
            "PASSWORD 'ư😀",
            "ưPASSWORD = 'é'",
            "))((PASSWORD))",
        ] {
            for d in [MY, PG] {
                let out = red(s, d);
                assert!(!out.contains('x') || !s.contains("'x"), "{s} -> {out}");
            }
        }
        let mix = "/*!50000 SET PASSWORD FOR 'ư'@'h' = PASSWORD(E'a\\'😀$q$') */ \
                   IDENTIFIED VIA p USING U&'x' REPLACE N'y' /*M!100 x */ $t$z$t$ -- c\r\n";
        for (i, _) in mix.char_indices() {
            for d in [MY, PG] {
                let _ = redact(&mix[..i], d);
                let _ = redact(&mix[i..], d);
            }
        }
        let big = "PASSWORD(".repeat(50_000) + "'x'" + &")".repeat(50_000);
        let t = std::time::Instant::now();
        for d in [MY, PG] {
            assert!(!red(&big, d).contains("'x'"));
            let many = "SELECT password = 'x', ".repeat(20_000);
            assert!(!red(&many, d).contains("'x'"));
        }
        assert!(t.elapsed().as_secs() < 5, "{:?}", t.elapsed());
    }
}
