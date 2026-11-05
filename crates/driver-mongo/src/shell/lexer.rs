//! JS-style tokens for the console (spec §4.1, §4.3): identifiers, strings (`'…'`, `"…"`,
//! `` `…` `` without `${}`), numbers, `/regex/flags` and punctuation. Whitespace and
//! comments (`//`, `/* */`) are skipped. Offsets are bytes into the source text.

use super::ShellError;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// Identifier or keyword, `$` and `_` included (`db`, `find`, `true`, `$gt`).
    Ident(String),
    /// String literal of any quote style, escapes decoded.
    Str(String),
    /// Numeric literal exactly as written, without a sign (`12`, `1.5e3`, `0x1F`).
    Num(String),
    /// `/pattern/flags`; the pattern is kept as written (escapes included).
    Regex { pattern: String, flags: String },
    /// Any other single character: `( ) [ ] { } , : ; . = + - * ! …`.
    Punct(char),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset just past the last character.
    pub end: usize,
}

/// Splits `text` into tokens. Fails only on an unterminated string, template, regex or
/// comment, a `${}` substitution, a bad escape, or a malformed number.
pub fn lex(text: &str) -> Result<Vec<Token>, ShellError> {
    let mut lx = Lexer {
        src: text,
        pos: 0,
        out: Vec::new(),
    };
    lx.run()?;
    Ok(lx.out)
}

fn is_ident_start(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphanumeric()
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    out: Vec<Token>,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    /// The character after the next one.
    fn peek2(&self) -> Option<char> {
        let mut it = self.src[self.pos..].chars();
        it.next();
        it.next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat_while(&mut self, f: impl Fn(char) -> bool) {
        while self.peek().is_some_and(&f) {
            self.bump();
        }
    }

    fn run(&mut self) -> Result<(), ShellError> {
        while let Some(c) = self.peek() {
            let start = self.pos;
            if c.is_whitespace() {
                self.bump();
                continue;
            }
            let kind = match (c, self.peek2()) {
                ('/', Some('/')) => {
                    self.eat_while(|c| c != '\n');
                    continue;
                }
                ('/', Some('*')) => {
                    self.block_comment(start)?;
                    continue;
                }
                ('/', _) if self.regex_allowed() => self.regex(start)?,
                ('\'' | '"', _) => self.string(c, start)?,
                ('`', _) => self.template(start)?,
                ('0'..='9', _) | ('.', Some('0'..='9')) => self.number(start)?,
                (c, _) if is_ident_start(c) => {
                    self.eat_while(is_ident_continue);
                    TokenKind::Ident(self.src[start..self.pos].to_string())
                }
                (c, _) => {
                    self.bump();
                    TokenKind::Punct(c)
                }
            };
            self.out.push(Token {
                kind,
                start,
                end: self.pos,
            });
        }
        Ok(())
    }

    /// A `/` starts a regex at the start, or after punctuation that cannot end a value
    /// (`(`, `,`, `:`, `[`, `{`, `=`, `;`, …); after a value (`)`, `]`, `}`, a name, a
    /// number, a string) it is division.
    fn regex_allowed(&self) -> bool {
        match self.out.last() {
            None => true,
            Some(Token {
                kind: TokenKind::Punct(c),
                ..
            }) => !matches!(c, ')' | ']' | '}'),
            Some(_) => false,
        }
    }

    fn block_comment(&mut self, start: usize) -> Result<(), ShellError> {
        match self.src[start + 2..].find("*/") {
            Some(i) => {
                self.pos = start + 2 + i + 2;
                Ok(())
            }
            None => Err(ShellError::new("Unterminated comment", start)),
        }
    }

    fn regex(&mut self, start: usize) -> Result<TokenKind, ShellError> {
        let unterminated = || ShellError::new("Unterminated regular expression", start);
        self.bump();
        let mut in_class = false;
        loop {
            match self.bump().ok_or_else(unterminated)? {
                '\n' | '\r' => return Err(unterminated()),
                '\\' => match self.bump() {
                    None | Some('\n' | '\r') => return Err(unterminated()),
                    Some(_) => {}
                },
                '[' => in_class = true,
                ']' => in_class = false,
                '/' if !in_class => break,
                _ => {}
            }
        }
        let pattern = self.src[start + 1..self.pos - 1].to_string();
        let flags_start = self.pos;
        self.eat_while(is_ident_continue);
        Ok(TokenKind::Regex {
            pattern,
            flags: self.src[flags_start..self.pos].to_string(),
        })
    }

    fn string(&mut self, quote: char, start: usize) -> Result<TokenKind, ShellError> {
        self.bump();
        let mut s = String::new();
        loop {
            let at = self.pos;
            match self.bump() {
                None | Some('\n' | '\r') => {
                    return Err(ShellError::new("Unterminated string", start))
                }
                Some(c) if c == quote => return Ok(TokenKind::Str(s)),
                Some('\\') => self.escape(&mut s, at, "Unterminated string", start)?,
                Some(c) => s.push(c),
            }
        }
    }

    fn template(&mut self, start: usize) -> Result<TokenKind, ShellError> {
        const UNTERMINATED: &str = "Unterminated template literal";
        self.bump();
        let mut s = String::new();
        loop {
            let at = self.pos;
            match self.bump() {
                None => return Err(ShellError::new(UNTERMINATED, start)),
                Some('`') => return Ok(TokenKind::Str(s)),
                Some('$') if self.peek() == Some('{') => {
                    return Err(ShellError::new(
                        "Template literal substitutions (${…}) are not supported",
                        at,
                    ))
                }
                Some('\\') => self.escape(&mut s, at, UNTERMINATED, start)?,
                // A template's line breaks are LF, whatever the source uses.
                Some('\r') => {
                    if self.peek() == Some('\n') {
                        self.bump();
                    }
                    s.push('\n');
                }
                Some(c) => s.push(c),
            }
        }
    }

    /// One escape after `\` (at byte `at`), appended to `s`.
    fn escape(
        &mut self,
        s: &mut String,
        at: usize,
        unterminated: &str,
        start: usize,
    ) -> Result<(), ShellError> {
        let bad = || ShellError::new("Invalid escape sequence", at);
        let Some(c) = self.bump() else {
            return Err(ShellError::new(unterminated, start));
        };
        match c {
            'n' => s.push('\n'),
            't' => s.push('\t'),
            'r' => s.push('\r'),
            'b' => s.push('\u{8}'),
            'f' => s.push('\u{c}'),
            'v' => s.push('\u{b}'),
            '0' if !self.peek().is_some_and(|c| c.is_ascii_digit()) => s.push('\0'),
            '0'..='9' => return Err(bad()),
            'x' => {
                let n = self.hex(2).ok_or_else(bad)?;
                s.push(char::from_u32(n).ok_or_else(bad)?);
            }
            'u' => s.push(self.unicode_escape().ok_or_else(bad)?),
            // Line continuation.
            '\r' => {
                if self.peek() == Some('\n') {
                    self.bump();
                }
            }
            '\n' | '\u{2028}' | '\u{2029}' => {}
            // `\'`, `\"`, `\\`, `\/`, `` \` ``, `\$` and any other character stand for themselves.
            c => s.push(c),
        }
        Ok(())
    }

    /// After `\u`: `XXXX` (a surrogate pair needs both halves) or `{X…}`.
    fn unicode_escape(&mut self) -> Option<char> {
        if self.peek() == Some('{') {
            self.bump();
            let digits_start = self.pos;
            self.eat_while(|c| c.is_ascii_hexdigit());
            let digits = &self.src[digits_start..self.pos];
            if digits.is_empty() || digits.len() > 6 || self.bump() != Some('}') {
                return None;
            }
            return char::from_u32(u32::from_str_radix(digits, 16).ok()?);
        }
        let hi = self.hex(4)?;
        if !(0xD800..0xDC00).contains(&hi) {
            return char::from_u32(hi);
        }
        if self.bump() != Some('\\') || self.bump() != Some('u') {
            return None;
        }
        let lo = self.hex(4)?;
        if !(0xDC00..0xE000).contains(&lo) {
            return None;
        }
        char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
    }

    /// Exactly `n` hex digits.
    fn hex(&mut self, n: usize) -> Option<u32> {
        let digits = self.src.get(self.pos..self.pos + n)?;
        if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        self.pos += n;
        u32::from_str_radix(digits, 16).ok()
    }

    /// Decimal (`12`, `1.5`, `.5`, `1e3`) or hex (`0x1F`). A number running straight into
    /// a name, a digit or another `.` is malformed.
    fn number(&mut self, start: usize) -> Result<TokenKind, ShellError> {
        let bad = || ShellError::new("Invalid number", start);
        let digits = |lx: &mut Self| {
            let from = lx.pos;
            lx.eat_while(|c| c.is_ascii_digit());
            lx.pos > from
        };
        if self.peek() == Some('0') && matches!(self.peek2(), Some('x' | 'X')) {
            self.pos += 2;
            let from = self.pos;
            self.eat_while(|c| c.is_ascii_hexdigit());
            if self.pos == from {
                return Err(bad());
            }
        } else {
            let int = digits(self);
            if int && self.src[start..self.pos].len() > 1 && self.src[start..].starts_with('0') {
                return Err(bad());
            }
            if self.peek() == Some('.') {
                self.bump();
                let frac = digits(self);
                if !int && !frac {
                    return Err(bad());
                }
            }
            if matches!(self.peek(), Some('e' | 'E')) {
                self.bump();
                if matches!(self.peek(), Some('+' | '-')) {
                    self.bump();
                }
                if !digits(self) {
                    return Err(bad());
                }
            }
        }
        if self
            .peek()
            .is_some_and(|c| is_ident_continue(c) || c == '.')
        {
            return Err(bad());
        }
        Ok(TokenKind::Num(self.src[start..self.pos].to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<TokenKind> {
        lex(text)
            .unwrap_or_else(|e| panic!("{text}: {e:?}"))
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    fn ident(s: &str) -> TokenKind {
        TokenKind::Ident(s.into())
    }

    fn p(c: char) -> TokenKind {
        TokenKind::Punct(c)
    }

    #[test]
    fn lexes_tokens_with_byte_offsets() {
        let text = "db.c.find({a: \"x;y\", é: 'z'}) // done; really\n/* a; b */ .sort({b: -1.5e3})";
        let toks = lex(text).unwrap();
        let spans: Vec<(&str, usize)> = toks
            .iter()
            .map(|t| (&text[t.start..t.end], t.start))
            .collect();
        assert_eq!(
            spans,
            vec![
                ("db", 0),
                (".", 2),
                ("c", 3),
                (".", 4),
                ("find", 5),
                ("(", 9),
                ("{", 10),
                ("a", 11),
                (":", 12),
                ("\"x;y\"", 14),
                (",", 19),
                ("é", 21),
                (":", 23),
                ("'z'", 25),
                ("}", 28),
                (")", 29),
                (".", 58),
                ("sort", 59),
                ("(", 63),
                ("{", 64),
                ("b", 65),
                (":", 66),
                ("-", 68),
                ("1.5e3", 69),
                ("}", 74),
                (")", 75),
            ]
        );
        assert_eq!(toks[9].kind, TokenKind::Str("x;y".into()));
        assert_eq!(toks[11].kind, ident("é"));
        assert_eq!(toks[23].kind, TokenKind::Num("1.5e3".into()));
    }

    #[test]
    fn strings_in_every_quote_style() {
        assert_eq!(
            kinds(
                r#"'it\'s' "a\"b\n\t\\" `multi
line;` 'é\u{1F600}\x41\/\d' "😀""#
            ),
            vec![
                TokenKind::Str("it's".into()),
                TokenKind::Str("a\"b\n\t\\".into()),
                TokenKind::Str("multi\nline;".into()),
                TokenKind::Str("é😀A/d".into()),
                TokenKind::Str("😀".into()),
            ]
        );
        // `$` without `{` is plain text in a template literal.
        assert_eq!(kinds("`$a`"), vec![TokenKind::Str("$a".into())]);
        let e = lex("x = `a${b}`").unwrap_err();
        assert_eq!(e.offset, 6);
        assert_eq!(
            e.message,
            "Template literal substitutions (${…}) are not supported"
        );
        let e = lex("{a: 'abc\n}").unwrap_err();
        assert_eq!((e.message.as_str(), e.offset), ("Unterminated string", 4));
        let e = lex("`never closed").unwrap_err();
        assert_eq!(
            (e.message.as_str(), e.offset),
            ("Unterminated template literal", 0)
        );
        assert_eq!(lex(r"'\uD83D'").unwrap_err().offset, 1);
    }

    #[test]
    fn regex_versus_division() {
        let re = |p: &str, f: &str| TokenKind::Regex {
            pattern: p.into(),
            flags: f.into(),
        };
        assert_eq!(kinds("/a;b/i"), vec![re("a;b", "i")]);
        assert_eq!(
            kinds("find({n: /x\\/y[/]z/m})"),
            vec![
                ident("find"),
                p('('),
                p('{'),
                ident("n"),
                p(':'),
                re("x\\/y[/]z", "m"),
                p('}'),
                p(')'),
            ]
        );
        assert_eq!(kinds("[/a/, /b/]")[1], re("a", ""));
        assert_eq!(kinds("[/a/, /b/]")[3], re("b", ""));
        assert_eq!(kinds("x = /a/")[2], re("a", ""));
        // After a value, `/` is division.
        assert_eq!(
            kinds("a / b / c"),
            vec![ident("a"), p('/'), ident("b"), p('/'), ident("c")]
        );
        assert_eq!(kinds("f() / 2")[3], p('/'));
        let e = lex("x(/abc\n/)").unwrap_err();
        assert_eq!(
            (e.message.as_str(), e.offset),
            ("Unterminated regular expression", 2)
        );
    }

    #[test]
    fn comments_and_numbers() {
        assert_eq!(
            kinds("a // b /c/\n/* d\n e */ f"),
            vec![ident("a"), ident("f")]
        );
        let e = lex("a /* open").unwrap_err();
        assert_eq!((e.message.as_str(), e.offset), ("Unterminated comment", 2));
        let num = |s: &str| TokenKind::Num(s.into());
        assert_eq!(
            kinds("0 12 1.5 .5 1e3 2E-2 0x1F 3.0"),
            vec![
                num("0"),
                num("12"),
                num("1.5"),
                num(".5"),
                num("1e3"),
                num("2E-2"),
                num("0x1F"),
                num("3.0"),
            ]
        );
        for bad in ["1e", "5abc", "01", "0x", "1.5.2"] {
            let e = lex(bad).unwrap_err();
            assert_eq!(
                (e.message.as_str(), e.offset),
                ("Invalid number", 0),
                "{bad}"
            );
        }
        // Characters outside the language are punctuation; the parser reports them.
        assert_eq!(kinds("=>"), vec![p('='), p('>')]);
        assert!(lex("").unwrap().is_empty());
        assert!(lex("  // only a comment").unwrap().is_empty());
    }
}
