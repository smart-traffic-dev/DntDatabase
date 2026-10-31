//! The editor's header notes and the starting text of a new object (spec R14).

use super::EditTarget;
use crate::error::DbError;
use crate::meta::ObjectKind;
use crate::server::{ServerInfo, ServerVariant, SqlKind};
use crate::sql::ident::quote_ident;

const ONE_TRANSACTION: &str = "Runs in one transaction.";

/// Header notes for the editor of `t` on `server`.
pub fn notes(t: &EditTarget, server: &ServerInfo) -> Result<Vec<String>, DbError> {
    use ObjectKind::*;
    let pg = match server.kind.require_sql()? {
        SqlKind::MySql => false,
        SqlKind::Postgres => true,
    };
    if t.name.is_none() {
        return Ok(if pg {
            vec![ONE_TRANSACTION.into()]
        } else {
            Vec::new()
        });
    }
    if pg {
        let how = match t.kind {
            View => "Replace uses CREATE OR REPLACE VIEW, which can only add columns at the end; Drop and re-create is needed to remove, rename or retype columns.",
            Function => "Replace uses CREATE OR REPLACE FUNCTION, which keeps the argument list and return type; Drop and re-create is needed to change them.",
            Procedure => "Replace uses CREATE OR REPLACE PROCEDURE, which keeps the argument list; Drop and re-create is needed to change it.",
            MaterializedView => "PostgreSQL has no CREATE OR REPLACE MATERIALIZED VIEW: the app drops and re-creates it, then re-creates its indexes.",
            Trigger => "The app drops and re-creates the trigger on its table.",
            _ => return Ok(vec![ONE_TRANSACTION.into()]),
        };
        return Ok(vec![ONE_TRANSACTION.into(), how.into()]);
    }
    let note = match t.kind {
        View => "Applied with CREATE OR REPLACE VIEW in one statement.".to_string(),
        _ if server.variant == ServerVariant::MariaDb => {
            "Applied with CREATE OR REPLACE in one statement.".to_string()
        }
        Function | Procedure => {
            let (word, what) = if t.kind == Function {
                ("FUNCTION", "function")
            } else {
                ("PROCEDURE", "procedure")
            };
            format!(
                "MySQL has no CREATE OR REPLACE {word}: the app checks the text under a temporary name, then drops and re-creates the {what}. Between the DROP and the CREATE it does not exist."
            )
        }
        Trigger | Event => {
            let (word, what) = if t.kind == Trigger {
                ("TRIGGER", "trigger")
            } else {
                ("EVENT", "event")
            };
            format!(
                "MySQL has no CREATE OR REPLACE {word}: the app drops and re-creates the {what}. Between the DROP and the CREATE it does not exist."
            )
        }
        _ => return Ok(Vec::new()),
    };
    Ok(vec![note])
}

/// The starting text of a new object of `t.kind` (spec R14): qualified with the
/// target database (MySQL) or schema (PostgreSQL); empty for a kind the engine
/// cannot edit.
pub fn template(t: &EditTarget, server: &ServerInfo) -> Result<String, DbError> {
    use ObjectKind::*;
    let table = t.table.as_deref().unwrap_or("table_name");
    Ok(match server.kind.require_sql()? {
        SqlKind::MySql => {
            let q = |n: &str| quote_ident(SqlKind::MySql, n);
            let db = q(&t.database);
            match t.kind {
                View => format!("CREATE VIEW {db}.`new_view` AS\nSELECT 1 AS `id`"),
                Function => format!(
                    "CREATE FUNCTION {db}.`new_function`(p_value INT)\nRETURNS INT\nDETERMINISTIC\nBEGIN\n  RETURN p_value;\nEND"
                ),
                Procedure => format!(
                    "CREATE PROCEDURE {db}.`new_procedure`(IN p_id INT)\nBEGIN\n  SELECT p_id;\nEND"
                ),
                Trigger => format!(
                    "CREATE TRIGGER {db}.`new_trigger` BEFORE INSERT ON {} FOR EACH ROW\nBEGIN\n  -- Change NEW.<column> here\nEND",
                    q(table)
                ),
                Event => format!(
                    "CREATE EVENT {db}.`new_event`\nON SCHEDULE EVERY 1 DAY\nDISABLE\nDO\nBEGIN\n  -- Work to run on schedule\nEND"
                ),
                _ => String::new(),
            }
        }
        SqlKind::Postgres => {
            let q = |n: &str| quote_ident(SqlKind::Postgres, n);
            let s = q(t.schema.as_deref().unwrap_or("public"));
            match t.kind {
                View => format!("CREATE VIEW {s}.\"new_view\" AS\nSELECT 1 AS id"),
                MaterializedView => format!(
                    "CREATE MATERIALIZED VIEW {s}.\"new_materialized_view\" AS\nSELECT 1 AS id\nWITH DATA"
                ),
                Function => format!(
                    "CREATE FUNCTION {s}.\"new_function\"(p_value integer)\nRETURNS integer\nLANGUAGE sql\nIMMUTABLE\nAS $$\n  SELECT p_value;\n$$"
                ),
                Procedure => format!(
                    "CREATE PROCEDURE {s}.\"new_procedure\"(p_id integer)\nLANGUAGE plpgsql\nAS $$\nBEGIN\n  RAISE NOTICE 'id %', p_id;\nEND;\n$$"
                ),
                Trigger => format!(
                    "CREATE TRIGGER \"new_trigger\" BEFORE UPDATE ON {s}.{} FOR EACH ROW\nEXECUTE FUNCTION {s}.\"trigger_function\"()",
                    q(table)
                ),
                _ => String::new(),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_servers::*;
    use super::super::{build, check_target, isolate, parse_header, BuildInput, CatalogFacts};
    use super::*;
    use crate::meta::ObjectKind;
    use crate::sql::lexer::Dialect;

    fn new_target(kind: ObjectKind, database: &str, schema: Option<&str>) -> EditTarget {
        EditTarget {
            kind,
            database: database.into(),
            schema: schema.map(Into::into),
            name: None,
            table: None,
        }
    }

    fn mysql_kinds() -> [ObjectKind; 5] {
        use ObjectKind::*;
        [View, Function, Procedure, Trigger, Event]
    }

    fn pg_kinds() -> [ObjectKind; 5] {
        use ObjectKind::*;
        [View, MaterializedView, Function, Procedure, Trigger]
    }

    #[test]
    fn templates_exact_texts() {
        use ObjectKind::*;
        let my = |k| template(&new_target(k, "db", None), &mysql8()).unwrap();
        assert_eq!(my(View), "CREATE VIEW `db`.`new_view` AS\nSELECT 1 AS `id`");
        assert_eq!(
            my(Function),
            "CREATE FUNCTION `db`.`new_function`(p_value INT)\nRETURNS INT\nDETERMINISTIC\nBEGIN\n  RETURN p_value;\nEND"
        );
        assert_eq!(
            my(Procedure),
            "CREATE PROCEDURE `db`.`new_procedure`(IN p_id INT)\nBEGIN\n  SELECT p_id;\nEND"
        );
        assert_eq!(
            my(Trigger),
            "CREATE TRIGGER `db`.`new_trigger` BEFORE INSERT ON `table_name` FOR EACH ROW\nBEGIN\n  -- Change NEW.<column> here\nEND"
        );
        assert_eq!(
            my(Event),
            "CREATE EVENT `db`.`new_event`\nON SCHEDULE EVERY 1 DAY\nDISABLE\nDO\nBEGIN\n  -- Work to run on schedule\nEND"
        );
        let mut on = new_target(Trigger, "db", None);
        on.table = Some("orders".into());
        assert!(template(&on, &mariadb())
            .unwrap()
            .contains(" ON `orders` FOR EACH ROW"));

        let pg = |k| template(&new_target(k, "app", Some("s")), &pg17()).unwrap();
        assert_eq!(
            pg(View),
            "CREATE VIEW \"s\".\"new_view\" AS\nSELECT 1 AS id"
        );
        assert_eq!(
            pg(MaterializedView),
            "CREATE MATERIALIZED VIEW \"s\".\"new_materialized_view\" AS\nSELECT 1 AS id\nWITH DATA"
        );
        assert_eq!(
            pg(Function),
            "CREATE FUNCTION \"s\".\"new_function\"(p_value integer)\nRETURNS integer\nLANGUAGE sql\nIMMUTABLE\nAS $$\n  SELECT p_value;\n$$"
        );
        assert_eq!(
            pg(Procedure),
            "CREATE PROCEDURE \"s\".\"new_procedure\"(p_id integer)\nLANGUAGE plpgsql\nAS $$\nBEGIN\n  RAISE NOTICE 'id %', p_id;\nEND;\n$$"
        );
        assert_eq!(
            pg(Trigger),
            "CREATE TRIGGER \"new_trigger\" BEFORE UPDATE ON \"s\".\"table_name\" FOR EACH ROW\nEXECUTE FUNCTION \"s\".\"trigger_function\"()"
        );
        let mut on = new_target(Trigger, "app", Some("s"));
        on.table = Some("orders".into());
        assert!(template(&on, &pg12())
            .unwrap()
            .contains(" ON \"s\".\"orders\" FOR EACH ROW"));
    }

    fn round_trip(t: &EditTarget, server: &ServerInfo) {
        let d = Dialect::for_kind(server.kind.require_sql().unwrap());
        let text = template(t, server).unwrap();
        let stmt = isolate(&text, t.kind, d, server).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        assert_eq!(stmt, text);
        let header = parse_header(&stmt, d).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        check_target(&header, t, server).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        let b = build(&BuildInput {
            target: t,
            stmt: &stmt,
            header: &header,
            strategy: None,
            server,
            facts: &CatalogFacts::default(),
            rehearsal_name: "_dnt_p4c_check_00000000",
        })
        .unwrap_or_else(|e| panic!("{text}: {e:?}"));
        assert_eq!(b.statements, vec![text.clone()], "{text}");
        assert!(!b.gap);
    }

    #[test]
    fn templates_round_trip() {
        for server in [mysql57(), mysql8(), mariadb()] {
            for k in mysql_kinds() {
                round_trip(&new_target(k, "db", None), &server);
                let mut on = new_target(k, "db", None);
                on.table = Some("orders".into());
                round_trip(&on, &server);
            }
        }
        for server in [pg12(), pg17()] {
            for k in pg_kinds() {
                round_trip(&new_target(k, "app", Some("public")), &server);
                let mut on = new_target(k, "app", Some("public"));
                on.table = Some("orders".into());
                round_trip(&on, &server);
            }
        }
    }

    #[test]
    fn template_quotes_hostile_scope() {
        let my = new_target(ObjectKind::Procedure, "we`ird", None);
        let text = template(&my, &mysql8()).unwrap();
        assert!(
            text.starts_with("CREATE PROCEDURE `we``ird`.`new_procedure`("),
            "{text}"
        );
        for k in mysql_kinds() {
            let mut t = new_target(k, "we`ird", None);
            t.table = Some("or`ders".into());
            round_trip(&t, &mysql8());
        }
        let pg = new_target(ObjectKind::Trigger, "app", Some("Sales \"x\""));
        let text = template(&pg, &pg17()).unwrap();
        assert_eq!(
            text,
            "CREATE TRIGGER \"new_trigger\" BEFORE UPDATE ON \"Sales \"\"x\"\"\".\"table_name\" FOR EACH ROW\nEXECUTE FUNCTION \"Sales \"\"x\"\"\".\"trigger_function\"()"
        );
        for k in pg_kinds() {
            let mut t = new_target(k, "app", Some("Sales \"x\""));
            t.table = Some("Or\"ders".into());
            round_trip(&t, &pg17());
        }
    }

    #[test]
    fn notes_per_engine_and_kind() {
        use ObjectKind::*;
        let edit = |k, s: Option<&str>| EditTarget {
            name: Some("x".into()),
            ..new_target(k, "db", s)
        };
        assert_eq!(
            notes(&edit(Procedure, None), &mysql8()).unwrap(),
            vec!["MySQL has no CREATE OR REPLACE PROCEDURE: the app checks the text under a temporary name, then drops and re-creates the procedure. Between the DROP and the CREATE it does not exist.".to_string()]
        );
        assert!(notes(&edit(Function, None), &mysql57()).unwrap()[0]
            .contains("CREATE OR REPLACE FUNCTION"));
        assert!(notes(&edit(Trigger, None), &mysql8()).unwrap()[0]
            .contains("drops and re-creates the trigger"));
        assert!(notes(&edit(Event, None), &mysql8()).unwrap()[0]
            .contains("drops and re-creates the event"));
        for k in mysql_kinds() {
            if k != View {
                assert_eq!(
                    notes(&edit(k, None), &mariadb()).unwrap(),
                    vec!["Applied with CREATE OR REPLACE in one statement.".to_string()]
                );
            }
        }
        for s in [mysql57(), mysql8(), mariadb()] {
            assert_eq!(
                notes(&edit(View, None), &s).unwrap(),
                vec!["Applied with CREATE OR REPLACE VIEW in one statement.".to_string()]
            );
        }
        for k in pg_kinds() {
            let n = notes(&edit(k, Some("public")), &pg17()).unwrap();
            assert_eq!(n[0], "Runs in one transaction.");
            assert_eq!(n.len(), 2, "{k:?}");
        }
        assert!(
            notes(&edit(View, Some("public")), &pg17()).unwrap()[1].contains("Drop and re-create")
        );
        assert!(notes(&edit(Function, Some("public")), &pg17()).unwrap()[1]
            .contains("Drop and re-create"));
        // New objects.
        assert!(notes(&new_target(View, "db", None), &mysql8())
            .unwrap()
            .is_empty());
        assert_eq!(
            notes(&new_target(View, "app", Some("public")), &pg17()).unwrap(),
            vec!["Runs in one transaction.".to_string()]
        );
    }
}
