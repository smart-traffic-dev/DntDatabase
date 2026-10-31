//! Which commands a Command-family run executes, and whether each may run (spec §4.4):
//! the Command counterpart of [`crate::plan::plan`] and [`crate::guard::guard`].

use dnt_core::command::{CommandClass, CommandLanguage, CommandParseError, ParsedCommand};

use crate::offsets::Utf16Map;
use crate::plan::{StmtKindLabel, Target, Verdict};
use crate::policy::{policy, PolicyCtx, RiskFacts};

/// One planned command.
#[derive(Debug, Clone)]
pub struct PlannedCommand {
    pub index: u32,
    /// UTF-16 range in the editor text.
    pub range: (u32, u32),
    /// Byte range in the editor text.
    pub source_range: (usize, usize),
    pub text: String,
    pub parsed: Result<ParsedCommand, CommandParseError>,
    pub label: StmtKindLabel,
    pub verdict: Verdict,
    /// Set by [`guard_commands`] on Production for a Confirm command that needs its
    /// target's (or the connection's) name typed.
    pub confirm_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CommandPlan {
    /// The text that was split: the whole editor text, or the selection.
    pub source: String,
    /// UTF-16 offset of `source` in the editor text.
    pub base: u32,
    pub stmts: Vec<PlannedCommand>,
    /// Set by [`guard_commands`]: what the verdicts were judged with (`None`: not
    /// guarded; only parse errors are `Block`).
    pub guard: Option<PolicyCtx>,
}

impl CommandPlan {
    /// Every command parsed and none is `Block`: the script may run.
    pub fn runnable(&self) -> bool {
        self.stmts
            .iter()
            .all(|s| s.parsed.is_ok() && s.verdict != Verdict::Block)
    }
}

/// The UI label of a command class: `SessionChange` shows as `Other`, and `Admin` and
/// `Unknown` as `Admin`.
pub fn label_of(class: &CommandClass) -> StmtKindLabel {
    match class {
        CommandClass::Read => StmtKindLabel::Read,
        CommandClass::Write => StmtKindLabel::Write,
        CommandClass::Ddl => StmtKindLabel::Ddl,
        CommandClass::Transaction(_) => StmtKindLabel::Transaction,
        CommandClass::SessionChange => StmtKindLabel::Other,
        CommandClass::Admin | CommandClass::Unknown => StmtKindLabel::Admin,
    }
}

/// Plans the commands `target` selects in the editor text `text`, with the same
/// target rules as [`crate::plan::plan`]:
///
/// - `All` splits the whole text.
/// - `Cursor(c)` splits the whole text and keeps one command: the one whose range
///   contains `c` (both ends inclusive), else the nearest one ending before `c`, else
///   the first.
/// - `Selection(a, b)` is normalized and clamped, and only that slice is split.
///
/// Ranges are relative to the editor text. Empty or whitespace-only input gives an
/// empty plan. A command that does not parse is `Block` (label `Other`); every other
/// verdict is `Allow` until [`guard_commands`] sets them.
pub fn plan_commands(text: &str, target: Target, lang: &dyn CommandLanguage) -> CommandPlan {
    let map = Utf16Map::new(text);
    let (from, to) = match target {
        Target::All | Target::Cursor(_) => (0, text.len()),
        Target::Selection(a, b) => (map.to_byte(a.min(b)), map.to_byte(a.max(b))),
    };
    let source = &text[from..to];
    let mut found = lang.split(source);
    let range = |r: (usize, usize)| (map.to_u16(from + r.0), map.to_u16(from + r.1));

    if let Target::Cursor(c) = target {
        // Snap a cursor inside a surrogate pair, or past the end, onto the text.
        let c = map.to_u16(map.to_byte(c));
        let pick = found
            .iter()
            .position(|s| {
                let (start, end) = range(s.range);
                start <= c && c <= end
            })
            .or_else(|| found.iter().rposition(|s| range(s.range).1 <= c))
            .or(if found.is_empty() { None } else { Some(0) });
        found = pick.map(|i| found.swap_remove(i)).into_iter().collect();
    }

    let stmts = found
        .into_iter()
        .enumerate()
        .map(|(index, s)| {
            let parsed = lang.parse(&s.text);
            let (label, verdict) = match &parsed {
                Ok(cmd) => (label_of(&cmd.class), Verdict::Allow),
                Err(_) => (StmtKindLabel::Other, Verdict::Block),
            };
            PlannedCommand {
                index: index as u32,
                range: range(s.range),
                source_range: (from + s.range.0, from + s.range.1),
                text: s.text,
                parsed,
                label,
                verdict,
                confirm_name: None,
            }
        })
        .collect();

    CommandPlan {
        source: source.to_string(),
        base: map.to_u16(from),
        stmts,
        guard: None,
    }
}

/// Sets every command's verdict and `confirm_name` from [`policy`] for the connection
/// `ctx`. A command that does not parse is `Block` with no name (fail-closed).
pub fn guard_commands(plan: &mut CommandPlan, ctx: &PolicyCtx) {
    for s in &mut plan.stmts {
        match &s.parsed {
            Ok(cmd) => {
                let outcome = policy(&RiskFacts::from(cmd), ctx);
                s.verdict = outcome.verdict;
                s.confirm_name = outcome.confirm_name;
            }
            Err(_) => {
                s.verdict = Verdict::Block;
                s.confirm_name = None;
            }
        }
    }
    plan.guard = Some(ctx.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::command::{CommandSpan, CommandTarget};
    use dnt_core::config::EnvTag;
    use dnt_core::redact::Redacted;
    use std::sync::Arc;

    /// One command per line: `GET` is Read, `SET` Write, anything else Unknown; a
    /// line with `!` does not parse.
    struct Lines;

    impl CommandLanguage for Lines {
        fn split(&self, text: &str) -> Vec<CommandSpan> {
            let mut out = Vec::new();
            let mut at = 0;
            for line in text.split_inclusive('\n') {
                let lead = line.len() - line.trim_start().len();
                let body = line.trim();
                if !body.is_empty() {
                    let start = at + lead;
                    out.push(CommandSpan {
                        range: (start, start + body.len()),
                        text: body.to_string(),
                    });
                }
                at += line.len();
            }
            out
        }

        fn parse(&self, text: &str) -> Result<ParsedCommand, CommandParseError> {
            if let Some(offset) = text.find('!') {
                return Err(CommandParseError {
                    message: "unexpected '!'".to_string(),
                    offset: Some(offset),
                });
            }
            let mut words = text.split_whitespace();
            let name = words.next().unwrap_or_default().to_string();
            let class = match name.as_str() {
                "GET" => CommandClass::Read,
                "SET" => CommandClass::Write,
                _ => CommandClass::Unknown,
            };
            Ok(ParsedCommand {
                name,
                class,
                targets: words
                    .next()
                    .map(|k| CommandTarget {
                        database: None,
                        name: k.to_string(),
                    })
                    .into_iter()
                    .collect(),
                unbounded: false,
                drops: Vec::new(),
                session_effect: None,
                payload: Arc::new(()),
            })
        }

        fn redact(&self, text: &str) -> Redacted {
            crate::redact::redact(text, dnt_core::sql::Dialect::MYSQL)
        }
    }

    const T: &str = "GET a\nSET b\n\nGET c";

    fn texts(p: &CommandPlan) -> Vec<&str> {
        p.stmts.iter().map(|s| s.text.as_str()).collect()
    }

    fn verdicts(p: &CommandPlan) -> Vec<Verdict> {
        p.stmts.iter().map(|s| s.verdict).collect()
    }

    fn ctx(env: Option<EnvTag>, read_only: bool) -> PolicyCtx {
        PolicyCtx {
            env,
            read_only,
            confirm_unbounded: false,
            connection_name: "conn".to_string(),
        }
    }

    fn guarded(text: &str, ctx: &PolicyCtx) -> CommandPlan {
        let mut p = plan_commands(text, Target::All, &Lines);
        guard_commands(&mut p, ctx);
        p
    }

    #[test]
    fn cursor_picks_the_line() {
        for (cursor, want) in [
            (0, "GET a"),
            (5, "GET a"),
            (6, "SET b"),
            (11, "SET b"),
            // The blank line between: the nearest command before it.
            (12, "SET b"),
            (13, "GET c"),
            (9999, "GET c"),
        ] {
            let p = plan_commands(T, Target::Cursor(cursor), &Lines);
            assert_eq!(texts(&p), [want], "cursor {cursor}");
            assert_eq!(p.stmts[0].index, 0);
            assert_eq!(p.base, 0);
        }
        let p = plan_commands("  GET a", Target::Cursor(0), &Lines);
        assert_eq!(texts(&p), ["GET a"]);
        assert_eq!(p.stmts[0].range, (2, 7));
    }

    #[test]
    fn selection_plans_only_the_slice() {
        let p = plan_commands(T, Target::Selection(18, 6), &Lines);
        assert_eq!(texts(&p), ["SET b", "GET c"]);
        assert_eq!(p.source, "SET b\n\nGET c");
        assert_eq!(p.base, 6);
        let ranges: Vec<_> = p.stmts.iter().map(|s| s.range).collect();
        assert_eq!(ranges, [(6, 11), (13, 18)]);
        for s in &p.stmts {
            assert_eq!(&T[s.source_range.0..s.source_range.1], s.text);
        }
        assert_eq!(p.stmts[1].index, 1);
        assert_eq!(p.stmts[0].label, StmtKindLabel::Write);
        assert_eq!(p.stmts[1].label, StmtKindLabel::Read);
    }

    #[test]
    fn empty_input_gives_empty_plan() {
        for text in ["", "  \n\t\n"] {
            for target in [
                Target::All,
                Target::Cursor(0),
                Target::Cursor(3),
                Target::Selection(0, 4),
            ] {
                let p = plan_commands(text, target, &Lines);
                assert!(p.stmts.is_empty(), "{text:?} {target:?}");
                assert!(p.runnable());
            }
        }
    }

    #[test]
    fn ranges_are_utf16() {
        let text = "SET k \"héllo😀\"\nGET k";
        let get = text.find("GET").unwrap();
        let get16 = text[..get].encode_utf16().count() as u32;
        assert_eq!((get, get16), (19, 16));
        let p = plan_commands(text, Target::All, &Lines);
        assert_eq!(p.stmts[0].range, (0, 15));
        assert_eq!(p.stmts[1].range, (get16, get16 + 5));
        assert_eq!(p.stmts[1].source_range, (get, get + 5));
        let p = plan_commands(text, Target::Cursor(17), &Lines);
        assert_eq!(texts(&p), ["GET k"]);
        let p = plan_commands(text, Target::Selection(16, 21), &Lines);
        assert_eq!(texts(&p), ["GET k"]);
        assert_eq!(p.base, 16);
        assert_eq!(p.stmts[0].range, (16, 21));
        assert_eq!(p.stmts[0].source_range, (19, 24));
    }

    #[test]
    fn parse_error_blocks_the_whole_script() {
        let text = "GET a\nSET !\nGET b";
        let p = plan_commands(text, Target::All, &Lines);
        assert_eq!(
            verdicts(&p)[1],
            Verdict::Block,
            "fail-closed before the guard"
        );
        assert!(!p.runnable());

        let p = guarded(text, &ctx(Some(EnvTag::Local), false));
        assert_eq!(
            verdicts(&p),
            [Verdict::Allow, Verdict::Block, Verdict::Allow]
        );
        let bad = &p.stmts[1];
        assert_eq!(bad.label, StmtKindLabel::Other);
        assert_eq!(bad.confirm_name, None);
        assert_eq!(bad.parsed.as_ref().unwrap_err().offset, Some(4));
        assert!(!p.runnable());
        assert_eq!(p.guard, Some(ctx(Some(EnvTag::Local), false)));
    }

    #[test]
    fn unknown_on_production_needs_connection_name() {
        let p = guarded(
            "NUKE\nNUKE k1\nGET a",
            &ctx(Some(EnvTag::Production), false),
        );
        assert_eq!(
            verdicts(&p),
            [Verdict::Confirm, Verdict::Confirm, Verdict::Allow]
        );
        assert_eq!(p.stmts[0].confirm_name.as_deref(), Some("conn"));
        assert_eq!(p.stmts[1].confirm_name.as_deref(), Some("k1"));
        assert_eq!(p.stmts[2].confirm_name, None);
        assert_eq!(p.stmts[0].label, StmtKindLabel::Admin);
        assert!(p.runnable());

        let p = guarded("NUKE", &ctx(Some(EnvTag::Staging), false));
        assert_eq!(verdicts(&p), [Verdict::Allow]);
        assert_eq!(p.stmts[0].confirm_name, None);
    }

    #[test]
    fn read_only_blocks_writes() {
        for env in [Some(EnvTag::Production), Some(EnvTag::Local), None] {
            let p = guarded("GET a\nSET b\nNUKE", &ctx(env, true));
            assert_eq!(
                verdicts(&p),
                [Verdict::Allow, Verdict::Block, Verdict::Block],
                "{env:?}"
            );
            assert!(p.stmts.iter().all(|s| s.confirm_name.is_none()));
            assert!(!p.runnable());
        }
        assert!(guarded("GET a", &ctx(None, true)).runnable());
    }

    #[test]
    fn label_of_maps_classes() {
        use dnt_core::sql::TxControl;
        for (class, want) in [
            (CommandClass::Read, StmtKindLabel::Read),
            (CommandClass::Write, StmtKindLabel::Write),
            (CommandClass::Ddl, StmtKindLabel::Ddl),
            (
                CommandClass::Transaction(TxControl::Begin),
                StmtKindLabel::Transaction,
            ),
            (CommandClass::SessionChange, StmtKindLabel::Other),
            (CommandClass::Admin, StmtKindLabel::Admin),
            (CommandClass::Unknown, StmtKindLabel::Admin),
        ] {
            assert_eq!(label_of(&class), want, "{class:?}");
        }
        assert_eq!(
            serde_json::to_string(&StmtKindLabel::Admin).unwrap(),
            "\"admin\""
        );
    }
}
