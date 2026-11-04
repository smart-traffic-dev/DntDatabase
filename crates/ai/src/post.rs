//! Turns the model's raw text into the ghost-text suggestion (spec §5.3), and judges
//! the Settings → Test probe (spec §6.2).

use dnt_core::sql::{tokenize, Dialect, TokKind};

/// Lines of a suggestion when the settings give none.
pub const DEFAULT_MAX_LINES: usize = 8;
/// Shortest trimmed suggestion the repetition check applies to: a lone `)` or `,`
/// that also ends the prefix is a real continuation more often than an echo.
const REPEAT_MIN: usize = 3;

/// Control tokens of common FIM model families that some servers leave in the text.
const FIM_TOKENS: &[&str] = &[
    "<|fim_prefix|>",
    "<|fim_suffix|>",
    "<|fim_middle|>",
    "<|fim_pad|>",
    "<|endoftext|>",
    "<|file_separator|>",
    "<fim_prefix>",
    "<fim_suffix>",
    "<fim_middle>",
    "<PRE>",
    "<SUF>",
    "<MID>",
    "<EOT>",
    "<｜fim▁begin｜>",
    "<｜fim▁hole｜>",
    "<｜fim▁end｜>",
    "<｜end▁of▁sentence｜>",
];

#[derive(Debug, Clone, Copy)]
pub struct PostCtx<'a> {
    pub dialect: Dialect,
    /// [`crate::prompt::FimPrompt::statement_head`].
    pub statement_head: &'a str,
    /// [`crate::prompt::FimPrompt::prefix_tail`].
    pub prefix_tail: &'a str,
    pub suffix: &'a str,
    pub max_lines: usize,
}

/// The suggestion to show, or `None` when there is nothing worth showing.
pub fn clean(raw: &str, ctx: &PostCtx) -> Option<String> {
    let mut text = raw.replace("\r\n", "\n");
    if let Some(at) = FIM_TOKENS.iter().filter_map(|t| text.find(t)).min() {
        text.truncate(at);
    }
    if let Some(at) = blank_line(&text) {
        text.truncate(at);
    }
    if let Some((at, _)) = text.match_indices('\n').nth(ctx.max_lines.max(1) - 1) {
        text.truncate(at);
    }
    let overlap = suffix_overlap(&text, ctx.suffix.trim_start());
    text.truncate(text.len() - overlap);
    let kept = text.trim_end().len();
    text.truncate(kept);
    if let Some(end) = first_statement_end(&text, ctx) {
        text.truncate(end);
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() >= REPEAT_MIN && ctx.prefix_tail.trim_end().ends_with(trimmed) {
        return None;
    }
    Some(text)
}

/// Offset of the `\n` that starts the first blank line (only spaces / tabs before the
/// next `\n`).
fn blank_line(text: &str) -> Option<usize> {
    text.match_indices('\n').map(|(i, _)| i).find(|&i| {
        text[i + 1..]
            .trim_start_matches([' ', '\t'])
            .starts_with('\n')
    })
}

/// Length of the longest tail of `text` that equals a head of `suffix`: the model
/// repeating what already follows the cursor (often a `)` or `;`).
fn suffix_overlap(text: &str, suffix: &str) -> usize {
    suffix
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&k| k <= text.len())
        .filter(|&k| text.ends_with(&suffix[..k]))
        .last()
        .unwrap_or(0)
}

/// End (just past the `;`) of the statement the suggestion finishes, read with the
/// current statement's head so open strings and comments are honoured.
fn first_statement_end(text: &str, ctx: &PostCtx) -> Option<usize> {
    let joined = format!("{}{text}", ctx.statement_head);
    let head = ctx.statement_head.len();
    tokenize(&joined, ctx.dialect)
        .into_iter()
        .find(|t| t.kind == TokKind::Punct(b';') && t.start >= head)
        .map(|t| t.end - head)
}

/// Prompt of the Settings → Test request.
pub const PROBE_PROMPT: &str = "SELECT id, name\nFROM users\nWHERE ";
/// Suffix of the Settings → Test request.
pub const PROBE_SUFFIX: &str = "\nORDER BY id;";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// A usable fill for the hole.
    Ok,
    /// The reply continues past the suffix (writes `ORDER BY` or ends the statement):
    /// the endpoint or model most likely ignores `suffix`.
    IgnoresSuffix,
    /// Nothing usable came back.
    Empty,
}

/// Judges the raw reply to [`PROBE_PROMPT`] / [`PROBE_SUFFIX`].
pub fn judge_probe(raw: &str) -> ProbeVerdict {
    let ctx = PostCtx {
        dialect: Dialect::Postgres,
        statement_head: PROBE_PROMPT,
        prefix_tail: PROBE_PROMPT,
        suffix: PROBE_SUFFIX,
        max_lines: DEFAULT_MAX_LINES,
    };
    let Some(cleaned) = clean(raw, &ctx) else {
        return ProbeVerdict::Empty;
    };
    // Judged on the raw text: `clean` would remove an echoed suffix.
    let raw = raw.trim();
    if raw.to_ascii_uppercase().contains("ORDER BY") || raw.ends_with(';') || cleaned.ends_with(';')
    {
        ProbeVerdict::IgnoresSuffix
    } else {
        ProbeVerdict::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(head: &'a str, suffix: &'a str) -> PostCtx<'a> {
        PostCtx {
            dialect: Dialect::Postgres,
            statement_head: head,
            prefix_tail: head,
            suffix,
            max_lines: DEFAULT_MAX_LINES,
        }
    }

    fn c(raw: &str) -> Option<String> {
        clean(raw, &ctx("SELECT * FROM t WHERE ", ""))
    }

    #[test]
    fn crlf_normalized() {
        assert_eq!(
            c("a = 1\r\n  AND b = 2").as_deref(),
            Some("a = 1\n  AND b = 2")
        );
    }

    #[test]
    fn strips_fim_tokens() {
        assert_eq!(c("a = 1<|endoftext|>junk").as_deref(), Some("a = 1"));
        assert_eq!(c("a = 1<EOT>").as_deref(), Some("a = 1"));
        assert_eq!(c("a = 1<｜end▁of▁sentence｜>").as_deref(), Some("a = 1"));
        assert_eq!(c("<|fim_middle|>"), None);
    }

    #[test]
    fn cuts_at_blank_line() {
        assert_eq!(c("a = 1\n  \nSELECT 2").as_deref(), Some("a = 1"));
        assert_eq!(c("a = 1\n\t\nx").as_deref(), Some("a = 1"));
        assert_eq!(c("\n\nSELECT 2"), None);
        // A leading single line break is a continuation on the next line.
        assert_eq!(c("\n  AND b = 2").as_deref(), Some("\n  AND b = 2"));
    }

    #[test]
    fn limits_lines() {
        let raw: String = (0..20).map(|i| format!("  AND c{i} = {i}\n")).collect();
        let out = clean(
            &format!("a = 1\n{raw}"),
            &PostCtx {
                max_lines: 3,
                ..ctx("SELECT * FROM t WHERE ", "")
            },
        )
        .unwrap();
        assert_eq!(out, "a = 1\n  AND c0 = 0\n  AND c1 = 1");
    }

    #[test]
    fn drops_suffix_overlap() {
        let out = clean("id = 1)", &ctx("SELECT * FROM t WHERE (", ")\nLIMIT 1")).unwrap();
        assert_eq!(out, "id = 1");
        let out = clean(
            "1\nORDER BY id;",
            &ctx("SELECT 1 WHERE a = ", "\nORDER BY id;"),
        )
        .unwrap();
        assert_eq!(out, "1");
        // No overlap: kept.
        let out = clean("id = 1", &ctx("SELECT * FROM t WHERE ", ";")).unwrap();
        assert_eq!(out, "id = 1");
    }

    #[test]
    fn keeps_first_statement_only() {
        assert_eq!(c("1;\nSELECT 2;").as_deref(), Some("1;"));
    }

    #[test]
    fn semicolon_inside_string_is_not_an_end() {
        assert_eq!(
            c("name = 'a;b' AND x = 1;").as_deref(),
            Some("name = 'a;b' AND x = 1;")
        );
        // A string the head opened is closed by the suggestion.
        let out = clean(
            "c;d' AND x = 1; SELECT 9",
            &ctx("SELECT * FROM t WHERE n = 'ab", ""),
        )
        .unwrap();
        assert_eq!(out, "c;d' AND x = 1;");
    }

    #[test]
    fn drops_repetition() {
        assert_eq!(
            clean("FROM t WHERE", &ctx("SELECT * FROM t WHERE ", "")),
            None
        );
        // Short echoes are kept.
        assert_eq!(clean(")", &ctx("SELECT f(a)", "")).as_deref(), Some(")"));
    }

    #[test]
    fn whitespace_is_none() {
        assert_eq!(c("   \t"), None);
        assert_eq!(c(""), None);
    }

    #[test]
    fn probe_ok() {
        assert_eq!(judge_probe("active = 1"), ProbeVerdict::Ok);
        assert_eq!(judge_probe(" name LIKE 'a%'"), ProbeVerdict::Ok);
    }

    #[test]
    fn probe_ignores_suffix() {
        assert_eq!(
            judge_probe("active = 1\nORDER BY id;"),
            ProbeVerdict::IgnoresSuffix
        );
        assert_eq!(judge_probe("active = 1;"), ProbeVerdict::IgnoresSuffix);
    }

    #[test]
    fn probe_empty() {
        assert_eq!(judge_probe(""), ProbeVerdict::Empty);
        assert_eq!(judge_probe("<|endoftext|>"), ProbeVerdict::Empty);
    }
}
