//! The guard policy shared by the Command family (spec §4.3): the verdict of one
//! parsed command, from what it does and the connection it runs on. The SQL guard
//! ([`crate::guard`]) does not call it; `tests/policy_matrix.rs` proves the two agree
//! on every row that has a SQL equivalent.
//!
//! | class                            | read_only | Production       | Staging | Local / no tag |
//! |----------------------------------|-----------|------------------|---------|----------------|
//! | Read, SessionChange, Transaction | Allow     | Allow            | Allow   | Allow          |
//! | Write, Ddl, Admin                | Block     | Confirm          | Allow   | Allow          |
//! | Unknown                          | Block     | Confirm + name   | Allow   | Allow          |
//!
//! Risk rules, applied to an `Allow` from the table (as `guard::stmt_verdict`; a
//! read-only Block stays Block):
//!
//! - read-only: a command that is `unbounded` or drops anything → Block, whatever its
//!   class.
//! - `unbounded` → Confirm on Production / Staging; on Local / no tag only when
//!   `confirm_unbounded`.
//! - a drop of an `Object` → Confirm on Staging (Production confirms it as Ddl).
//! - a drop of a `Database` or of `AllDatabases` → Confirm on every connection.
//!
//! `confirm_name` (Production, verdict Confirm only): a command that drops
//! `AllDatabases`, or a `Database` whose name is empty (unknown when planned), gives the
//! connection's name; else a command that is `unbounded` or drops something gives its
//! target's name (like [`crate::risk::confirm_name`]: a dropped database first, else the
//! first target, else a dropped object); an `Unknown` command gives its first target's
//! name, or the connection's name when it has none.

use dnt_core::command::{CommandClass, CommandDrop, CommandDropKind, CommandTarget, ParsedCommand};
use dnt_core::config::EnvTag;

use crate::plan::Verdict;

/// What a command does, for the policy ([`CommandClass`] without the transaction step).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskClass {
    Read,
    Write,
    Ddl,
    Admin,
    SessionChange,
    Transaction,
    Unknown,
}

impl From<CommandClass> for RiskClass {
    fn from(class: CommandClass) -> Self {
        match class {
            CommandClass::Read => Self::Read,
            CommandClass::Write => Self::Write,
            CommandClass::Ddl => Self::Ddl,
            CommandClass::Admin => Self::Admin,
            CommandClass::SessionChange => Self::SessionChange,
            CommandClass::Transaction(_) => Self::Transaction,
            CommandClass::Unknown => Self::Unknown,
        }
    }
}

/// What the policy judges a command by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskFacts {
    pub class: RiskClass,
    /// It acts on every document / key of its target.
    pub unbounded: bool,
    /// The collections / keys it acts on (from `ParsedCommand.targets`); the name a
    /// Production confirmation is typed with.
    pub targets: Vec<CommandTarget>,
    /// From `ParsedCommand.drops`.
    pub drops: Vec<CommandDrop>,
}

impl From<&ParsedCommand> for RiskFacts {
    fn from(cmd: &ParsedCommand) -> Self {
        Self {
            class: cmd.class.clone().into(),
            unbounded: cmd.unbounded,
            targets: cmd.targets.clone(),
            drops: cmd.drops.clone(),
        }
    }
}

/// The connection a command is judged for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyCtx {
    pub env: Option<EnvTag>,
    pub read_only: bool,
    /// The "Confirm UPDATE/DELETE without WHERE" setting, as in `GuardCtx`.
    pub confirm_unbounded: bool,
    pub connection_name: String,
}

/// The policy's verdict, and the name a Production confirmation must be typed with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyOutcome {
    pub verdict: Verdict,
    pub confirm_name: Option<String>,
}

/// The verdict of one command (see the module table and risk rules).
pub fn policy(facts: &RiskFacts, ctx: &PolicyCtx) -> PolicyOutcome {
    let verdict = verdict(facts, ctx);
    let confirm_name = if ctx.env == Some(EnvTag::Production) && verdict == Verdict::Confirm {
        confirm_name(facts, ctx)
    } else {
        None
    };
    PolicyOutcome {
        verdict,
        confirm_name,
    }
}

fn verdict(facts: &RiskFacts, ctx: &PolicyCtx) -> Verdict {
    let v = match facts.class {
        RiskClass::Read | RiskClass::SessionChange | RiskClass::Transaction => Verdict::Allow,
        RiskClass::Write | RiskClass::Ddl | RiskClass::Admin | RiskClass::Unknown => {
            if ctx.read_only {
                Verdict::Block
            } else if ctx.env == Some(EnvTag::Production) {
                Verdict::Confirm
            } else {
                Verdict::Allow
            }
        }
    };
    if v != Verdict::Allow {
        return v;
    }
    if ctx.read_only && (facts.unbounded || !facts.drops.is_empty()) {
        return Verdict::Block;
    }
    let tagged = matches!(ctx.env, Some(EnvTag::Production | EnvTag::Staging));
    if facts.unbounded && (ctx.confirm_unbounded || tagged) {
        return Verdict::Confirm;
    }
    let staging_object = ctx.env == Some(EnvTag::Staging)
        && facts
            .drops
            .iter()
            .any(|d| d.kind == CommandDropKind::Object);
    let database = facts.drops.iter().any(|d| {
        matches!(
            d.kind,
            CommandDropKind::Database | CommandDropKind::AllDatabases
        )
    });
    if staging_object || database {
        return Verdict::Confirm;
    }
    v
}

fn confirm_name(facts: &RiskFacts, ctx: &PolicyCtx) -> Option<String> {
    let named = |name: &str| (!name.is_empty()).then(|| name.to_string());
    let dropped = |kind: CommandDropKind| {
        facts
            .drops
            .iter()
            .filter(|d| d.kind == kind)
            .find_map(|d| named(&d.target.name))
    };
    // Every database, or one whose name stayed unknown: the connection's name.
    let connection_wide = facts.drops.iter().any(|d| match d.kind {
        CommandDropKind::AllDatabases => true,
        CommandDropKind::Database => d.target.name.is_empty(),
        CommandDropKind::Object => false,
    });
    let first_target = || facts.targets.iter().find_map(|t| named(&t.name));
    let risky = || {
        connection_wide
            .then(|| ctx.connection_name.clone())
            .or_else(|| dropped(CommandDropKind::Database))
            .or_else(|| facts.unbounded.then(first_target).flatten())
            .or_else(|| dropped(CommandDropKind::Object))
    };
    if facts.unbounded || !facts.drops.is_empty() {
        if let Some(name) = risky() {
            return Some(name);
        }
    }
    if facts.class == RiskClass::Unknown {
        return Some(first_target().unwrap_or_else(|| ctx.connection_name.clone()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENVS: [Option<EnvTag>; 4] = [
        Some(EnvTag::Production),
        Some(EnvTag::Staging),
        Some(EnvTag::Local),
        None,
    ];

    fn target(name: &str) -> CommandTarget {
        CommandTarget {
            database: None,
            name: name.to_string(),
        }
    }

    fn facts(class: RiskClass) -> RiskFacts {
        RiskFacts {
            class,
            unbounded: false,
            targets: Vec::new(),
            drops: Vec::new(),
        }
    }

    fn dropping(class: RiskClass, kind: CommandDropKind, name: &str) -> RiskFacts {
        RiskFacts {
            targets: vec![target(name)],
            drops: vec![CommandDrop {
                kind,
                target: target(name),
            }],
            ..facts(class)
        }
    }

    fn ctx(env: Option<EnvTag>, read_only: bool, confirm_unbounded: bool) -> PolicyCtx {
        PolicyCtx {
            env,
            read_only,
            confirm_unbounded,
            connection_name: "conn".to_string(),
        }
    }

    fn run(f: &RiskFacts, env: Option<EnvTag>, read_only: bool, unb: bool) -> PolicyOutcome {
        policy(f, &ctx(env, read_only, unb))
    }

    fn outcome(verdict: Verdict, name: Option<&str>) -> PolicyOutcome {
        PolicyOutcome {
            verdict,
            confirm_name: name.map(str::to_string),
        }
    }

    #[test]
    fn read_allowed_everywhere() {
        for class in [
            RiskClass::Read,
            RiskClass::SessionChange,
            RiskClass::Transaction,
        ] {
            for env in ENVS {
                for read_only in [false, true] {
                    for unb in [false, true] {
                        assert_eq!(
                            run(&facts(class), env, read_only, unb),
                            outcome(Verdict::Allow, None),
                            "{class:?} {env:?} {read_only}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn write_blocked_read_only_confirm_production_allow_staging_local() {
        for class in [RiskClass::Write, RiskClass::Ddl, RiskClass::Admin] {
            for env in ENVS {
                assert_eq!(
                    run(&facts(class), env, true, true).verdict,
                    Verdict::Block,
                    "{class:?} {env:?}"
                );
            }
            assert_eq!(
                run(&facts(class), Some(EnvTag::Production), false, true),
                outcome(Verdict::Confirm, None)
            );
            for env in [Some(EnvTag::Staging), Some(EnvTag::Local), None] {
                assert_eq!(
                    run(&facts(class), env, false, true),
                    outcome(Verdict::Allow, None),
                    "{class:?} {env:?}"
                );
            }
        }
    }

    #[test]
    fn unbounded_confirms_on_staging_and_local_when_set() {
        let f = RiskFacts {
            unbounded: true,
            targets: vec![target("users")],
            ..facts(RiskClass::Write)
        };
        assert_eq!(
            run(&f, Some(EnvTag::Production), false, false),
            outcome(Verdict::Confirm, Some("users"))
        );
        for unb in [false, true] {
            assert_eq!(
                run(&f, Some(EnvTag::Staging), false, unb),
                outcome(Verdict::Confirm, None)
            );
        }
        for env in [Some(EnvTag::Local), None] {
            assert_eq!(run(&f, env, false, true).verdict, Verdict::Confirm);
            assert_eq!(run(&f, env, false, false).verdict, Verdict::Allow);
        }
        // Read-only Block stays Block.
        assert_eq!(
            run(&f, Some(EnvTag::Local), true, true).verdict,
            Verdict::Block
        );
    }

    #[test]
    fn drop_object_confirms_on_staging() {
        let f = dropping(RiskClass::Ddl, CommandDropKind::Object, "orders");
        assert_eq!(
            run(&f, Some(EnvTag::Production), false, true),
            outcome(Verdict::Confirm, Some("orders"))
        );
        assert_eq!(
            run(&f, Some(EnvTag::Staging), false, false),
            outcome(Verdict::Confirm, None)
        );
        for env in [Some(EnvTag::Local), None] {
            assert_eq!(run(&f, env, false, true), outcome(Verdict::Allow, None));
        }
        assert_eq!(
            run(&f, Some(EnvTag::Staging), true, true).verdict,
            Verdict::Block
        );
    }

    #[test]
    fn drop_database_confirms_everywhere() {
        // FLUSHDB-like: Admin dropping the current database.
        let f = dropping(RiskClass::Admin, CommandDropKind::Database, "db0");
        assert_eq!(
            run(&f, Some(EnvTag::Production), false, false),
            outcome(Verdict::Confirm, Some("db0"))
        );
        for env in [Some(EnvTag::Staging), Some(EnvTag::Local), None] {
            assert_eq!(
                run(&f, env, false, false),
                outcome(Verdict::Confirm, None),
                "{env:?}"
            );
        }
        for env in ENVS {
            assert_eq!(run(&f, env, true, true).verdict, Verdict::Block);
        }
    }

    #[test]
    fn nameless_database_drop_needs_connection_name() {
        // FLUSHDB whose database stayed unknown at planning.
        let f = RiskFacts {
            drops: vec![CommandDrop {
                kind: CommandDropKind::Database,
                target: target(""),
            }],
            ..facts(RiskClass::Admin)
        };
        assert_eq!(
            run(&f, Some(EnvTag::Production), false, false),
            outcome(Verdict::Confirm, Some("conn"))
        );
    }

    #[test]
    fn all_databases_drop_needs_connection_name() {
        // FLUSHALL-like.
        let f = dropping(RiskClass::Admin, CommandDropKind::AllDatabases, "db0");
        assert_eq!(
            run(&f, Some(EnvTag::Production), false, false),
            outcome(Verdict::Confirm, Some("conn"))
        );
        for env in [Some(EnvTag::Staging), Some(EnvTag::Local), None] {
            assert_eq!(
                run(&f, env, false, false),
                outcome(Verdict::Confirm, None),
                "{env:?}"
            );
        }
        for env in ENVS {
            assert_eq!(run(&f, env, true, false).verdict, Verdict::Block);
        }
    }

    #[test]
    fn read_only_blocks_any_drop_or_unbounded() {
        let drop_db = CommandDrop {
            kind: CommandDropKind::Database,
            target: target("d"),
        };
        for class in [
            RiskClass::Read,
            RiskClass::SessionChange,
            RiskClass::Transaction,
        ] {
            let dropping = RiskFacts {
                drops: vec![drop_db.clone()],
                ..facts(class)
            };
            let unbounded = RiskFacts {
                unbounded: true,
                ..facts(class)
            };
            for env in ENVS {
                for unb in [false, true] {
                    for f in [&dropping, &unbounded] {
                        assert_eq!(
                            run(f, env, true, unb),
                            outcome(Verdict::Block, None),
                            "{f:?} {env:?} {unb}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn unknown_needs_connection_name_on_production() {
        let f = facts(RiskClass::Unknown);
        assert_eq!(
            run(&f, Some(EnvTag::Production), false, true),
            outcome(Verdict::Confirm, Some("conn"))
        );
        // With a target, that target's name.
        let keyed = RiskFacts {
            targets: vec![target("k1")],
            ..facts(RiskClass::Unknown)
        };
        assert_eq!(
            run(&keyed, Some(EnvTag::Production), false, true),
            outcome(Verdict::Confirm, Some("k1"))
        );
        for env in ENVS {
            assert_eq!(
                run(&f, env, true, true),
                outcome(Verdict::Block, None),
                "{env:?}"
            );
        }
        for env in [Some(EnvTag::Staging), Some(EnvTag::Local), None] {
            assert_eq!(run(&f, env, false, true), outcome(Verdict::Allow, None));
        }
    }

    #[test]
    fn risk_class_from_command_class() {
        use dnt_core::sql::TxControl;
        assert_eq!(
            RiskClass::from(CommandClass::Transaction(TxControl::Begin)),
            RiskClass::Transaction
        );
        assert_eq!(RiskClass::from(CommandClass::Unknown), RiskClass::Unknown);
    }
}
