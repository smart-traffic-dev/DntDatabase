//! A small stand-in for the MongoDB console language in unit tests (the real one lives in
//! `dnt-driver-mongo`, which this crate does not depend on; `src-tauri/tests/ai.rs` runs
//! the prompt with it).
//!
//! Split: a command ends at `;`, or at a line break with brackets closed unless the next
//! line starts with `.`; quotes are honoured. Redact: anything holding `pwd`, `password`
//! or `mongodb://` is a secret.

use dnt_core::command::{CommandLanguage, CommandParseError, CommandSpan, ParsedCommand};
use dnt_core::redact::Redacted;

pub(crate) struct TestLang;

impl CommandLanguage for TestLang {
    fn split(&self, text: &str) -> Vec<CommandSpan> {
        let b = text.as_bytes();
        let mut out = Vec::new();
        let (mut start, mut depth, mut quote): (Option<usize>, i32, Option<u8>) = (None, 0, None);
        let push = |from: usize, to: usize, out: &mut Vec<CommandSpan>| {
            let body = &text[from..to];
            let end = from + body.trim_end().len();
            if end > from {
                out.push(CommandSpan {
                    range: (from, end),
                    text: text[from..end].to_string(),
                });
            }
        };
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if let Some(q) = quote {
                if c == b'\\' {
                    i += 1;
                } else if c == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            match c {
                b'\'' | b'"' | b'`' => {
                    start.get_or_insert(i);
                    quote = Some(c);
                }
                b'(' | b'[' | b'{' => {
                    start.get_or_insert(i);
                    depth += 1;
                }
                b')' | b']' | b'}' => {
                    start.get_or_insert(i);
                    depth -= 1;
                }
                b';' => {
                    if let Some(s) = start.take() {
                        push(s, i, &mut out);
                    }
                    depth = 0;
                }
                b'\n' if depth <= 0 => {
                    let next = text[i + 1..].trim_start();
                    if !next.starts_with('.') {
                        if let Some(s) = start.take() {
                            push(s, i, &mut out);
                        }
                    }
                }
                c if c.is_ascii_whitespace() => {}
                _ => {
                    start.get_or_insert(i);
                }
            }
            i += 1;
        }
        if let Some(s) = start {
            push(s, text.len(), &mut out);
        }
        out
    }

    fn parse(&self, _text: &str) -> Result<ParsedCommand, CommandParseError> {
        Err(CommandParseError {
            message: "not parsed in tests".into(),
            offset: None,
        })
    }

    fn redact(&self, text: &str) -> Redacted {
        let lower = text.to_ascii_lowercase();
        if ["pwd", "password", "mongodb://"]
            .iter()
            .any(|m| lower.contains(m))
        {
            Redacted {
                text: "***".into(),
                changed: true,
            }
        } else {
            Redacted {
                text: text.to_string(),
                changed: false,
            }
        }
    }
}

#[test]
fn test_lang_splits_like_the_console() {
    let spans: Vec<String> = TestLang
        .split("db.a.find({\n x: 1\n})\n  .limit(1); db.b.find()\n\nuse x")
        .into_iter()
        .map(|s| s.text)
        .collect();
    assert_eq!(
        spans,
        [
            "db.a.find({\n x: 1\n})\n  .limit(1)",
            "db.b.find()",
            "use x"
        ]
    );
}
