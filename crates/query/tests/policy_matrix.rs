//! Spec §4.3: the shared Command-family policy agrees with the SQL guard on every row
//! that has a SQL equivalent, for both dialects and every env × read_only ×
//! confirm_unbounded.

use dnt_core::command::{CommandDrop, CommandDropKind, CommandTarget};
use dnt_core::config::EnvTag;
use dnt_core::sql::Dialect;
use dnt_query::{guard_with, plan, policy, GuardCtx, PolicyCtx, RiskClass, RiskFacts, Target};

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

fn drop_facts(kind: CommandDropKind, name: &str) -> RiskFacts {
    RiskFacts {
        targets: vec![target(name)],
        drops: vec![CommandDrop {
            kind,
            target: target(name),
        }],
        ..facts(RiskClass::Ddl)
    }
}

fn cases() -> Vec<(&'static str, RiskFacts)> {
    vec![
        ("SELECT 1", facts(RiskClass::Read)),
        ("BEGIN", facts(RiskClass::Transaction)),
        ("UPDATE t SET a = 1 WHERE id = 1", facts(RiskClass::Write)),
        (
            "UPDATE t SET a = 1",
            RiskFacts {
                unbounded: true,
                targets: vec![target("t")],
                ..facts(RiskClass::Write)
            },
        ),
        ("CREATE TABLE t (a int)", facts(RiskClass::Ddl)),
        ("DROP TABLE t", drop_facts(CommandDropKind::Object, "t")),
        (
            "DROP DATABASE d",
            drop_facts(CommandDropKind::Database, "d"),
        ),
        ("GRANT SELECT ON t TO u", facts(RiskClass::Admin)),
    ]
}

#[test]
fn policy_matrix_matches_sql_guard() {
    let envs = [
        Some(EnvTag::Production),
        Some(EnvTag::Staging),
        Some(EnvTag::Local),
        None,
    ];
    let mut checked = 0;
    for (sql, f) in cases() {
        for dialect in [Dialect::Postgres, Dialect::MYSQL] {
            for env in envs {
                for read_only in [false, true] {
                    for confirm_unbounded in [false, true] {
                        let mut p = plan(sql, Target::All, dialect);
                        assert_eq!(p.stmts.len(), 1, "{sql}");
                        guard_with(
                            &mut p,
                            GuardCtx {
                                env,
                                read_only,
                                analyze: false,
                                confirm_unbounded,
                            },
                        );
                        let s = &p.stmts[0];
                        let out = policy(
                            &f,
                            &PolicyCtx {
                                env,
                                read_only,
                                confirm_unbounded,
                                connection_name: "conn".to_string(),
                            },
                        );
                        assert_eq!(
                            (out.verdict, out.confirm_name.clone()),
                            (s.verdict, s.confirm_name.clone()),
                            "{sql} {dialect:?} env={env:?} read_only={read_only} \
                             confirm_unbounded={confirm_unbounded}"
                        );
                        checked += 1;
                    }
                }
            }
        }
    }
    assert_eq!(checked, 8 * 2 * 4 * 2 * 2);
}
