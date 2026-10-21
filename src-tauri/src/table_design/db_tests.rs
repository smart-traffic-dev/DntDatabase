//! Server-side behaviour of the designer's helpers on the Docker matrix
//! (`DNT_TEST_DB=1`): slow impact counts are stopped on the server (ruling T10-1), and
//! `SHOW CREATE TABLE` is read with `ANSI_QUOTES` off (ruling T9-2).
use super::*;
use dnt_core::driver::Driver;
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_schema::{Impact, ImpactKind};
use dnt_testkit::{require_db, Target, FIXTURE_DB};
use std::time::Instant;

async fn connect(t: Target) -> Box<dyn Session> {
    let driver: Box<dyn Driver> = if t.kind() == DbKind::MySql {
        Box::new(MySqlDriver)
    } else {
        Box::new(PgDriver)
    };
    driver
        .connect(&t.resolved(Some(FIXTURE_DB)))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"))
}

fn count_impact(sql: String) -> Impact {
    Impact {
        kind: ImpactKind::DataLoss,
        element: ElementRef::Column("x".into()),
        message: "m".into(),
        count_sql: Some(sql),
    }
}

#[tokio::test]
async fn slow_count_is_stopped_on_the_server() {
    require_db!();
    for t in Target::all() {
        let pg = t.kind() == DbKind::Postgres;
        let s = connect(t).await;
        let probe = connect(t).await;
        // The same path counts a fast query.
        let fast = apply::count_impacts(
            s.as_ref(),
            vec![count_impact(
                "SELECT COUNT(*) FROM (SELECT 1 AS a) s".into(),
            )],
            Duration::from_millis(300),
            Duration::from_secs(1),
        )
        .await;
        assert!(
            fast[0].counted && fast[0].rows == Some(1.0),
            "{t:?}: {fast:?}"
        );
        let marker = format!("dnt_slow_{}", Uuid::new_v4().simple());
        let sql = if pg {
            format!("SELECT COUNT(*) FROM (SELECT pg_sleep(20) AS {marker}) s")
        } else {
            format!("SELECT COUNT(*) FROM (SELECT SLEEP(20) AS {marker}) s")
        };
        let started = Instant::now();
        let out = apply::count_impacts(
            s.as_ref(),
            vec![count_impact(sql)],
            Duration::from_millis(300),
            Duration::from_secs(1),
        )
        .await;
        assert!(!out[0].counted, "{t:?}");
        // It really ran until the bound (it did not fail up front).
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "{t:?}: {:?}",
            started.elapsed()
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{t:?}: {:?}",
            started.elapsed()
        );
        let running = if pg {
            format!(
                "SELECT count(*)::text FROM pg_stat_activity WHERE state = 'active' \
                 AND query LIKE '%{marker}%' AND pid <> pg_backend_pid()"
            )
        } else {
            format!(
                "SELECT CAST(COUNT(*) AS CHAR) FROM information_schema.processlist \
                 WHERE info LIKE '%{marker}%' AND id <> CONNECTION_ID()"
            )
        };
        // The server may take a moment to tear the statement down.
        let mut left = None;
        for _ in 0..20 {
            left = first_text(probe.as_ref(), &running).await.unwrap();
            if left.as_deref() == Some("0") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(left.as_deref(), Some("0"), "{t:?}: the count still runs");
        close_quietly(s).await;
        close_quietly(probe).await;
    }
}

/// On MySQL / MariaDB the bound is the server's own (`max_execution_time` /
/// `max_statement_time`): a slow count run with no client-side timeout at all is still
/// stopped by the server near the bound.
#[tokio::test]
async fn mysql_family_count_bound_is_enforced_by_the_server() {
    require_db!();
    let mut targets = Target::mysql_family().to_vec();
    if !targets.contains(&Target::MySql84) {
        targets.push(Target::MySql84);
    }
    for t in targets {
        let s = connect(t).await;
        let probe = connect(t).await;
        let limit = Duration::from_millis(300);
        let bound = apply::mysql_count_bound(s.server_info(), limit)
            .unwrap_or_else(|| panic!("{t:?}: no server bound"));
        crate::ddl_exec::execute(
            s.as_ref(),
            "START TRANSACTION READ ONLY",
            StopSignal::new(),
            Duration::from_secs(1),
            0,
        )
        .await
        .unwrap();
        crate::ddl_exec::execute(
            s.as_ref(),
            &bound,
            StopSignal::new(),
            Duration::from_secs(1),
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {bound}: {e}"));
        let marker = format!("dnt_bound_{}", Uuid::new_v4().simple());
        let started = Instant::now();
        // `first_text` runs it without any client-side timeout.
        let _ = first_text(
            s.as_ref(),
            &format!("SELECT CAST(COUNT(*) AS CHAR) FROM (SELECT SLEEP(20) AS {marker}) s"),
        )
        .await;
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "{t:?}: {took:?}");
        let running = format!(
            "SELECT CAST(COUNT(*) AS CHAR) FROM information_schema.processlist \
             WHERE info LIKE '%{marker}%' AND id <> CONNECTION_ID()"
        );
        assert_eq!(
            first_text(probe.as_ref(), &running)
                .await
                .unwrap()
                .as_deref(),
            Some("0"),
            "{t:?}"
        );
        close_quietly(s).await;
        close_quietly(probe).await;
    }
}

#[tokio::test]
async fn sql_mode_reads_and_ddl_is_read_without_ansi_quotes() {
    require_db!();
    let mut targets = Target::mysql_family().to_vec();
    if !targets.contains(&Target::MySql84) {
        targets.push(Target::MySql84);
    }
    for t in targets {
        let s = connect(t).await;
        let mode = first_text(s.as_ref(), "SELECT @@SESSION.sql_mode")
            .await
            .unwrap();
        assert!(mode.is_some(), "{t:?}: sql_mode came back unreadable");
        crate::ddl_exec::execute(
            s.as_ref(),
            "SET SESSION sql_mode = 'ANSI'",
            StopSignal::new(),
            Duration::from_secs(1),
            0,
        )
        .await
        .unwrap();
        assert!(
            matches!(s.dialect(), Dialect::MySql(m) if m.ansi_quotes),
            "{t:?}"
        );
        let ansi_mode = first_text(s.as_ref(), "SELECT @@SESSION.sql_mode")
            .await
            .unwrap()
            .unwrap();
        let obj = ObjectRef {
            database: FIXTURE_DB.into(),
            schema: None,
            name: "orders".into(),
            kind: ObjectKind::Table,
        };
        let ddl = read_ddl(s.as_ref(), &obj).await.unwrap().unwrap();
        assert!(ddl.starts_with("CREATE TABLE `orders`"), "{t:?}: {ddl}");
        assert!(
            matches!(s.dialect(), Dialect::MySql(m) if m.ansi_quotes),
            "{t:?}"
        );
        let restored = first_text(s.as_ref(), "SELECT @@SESSION.sql_mode")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored, ansi_mode, "{t:?}");
        close_quietly(s).await;
    }
}
