//! Tool dumps (N7, spec §7.2) with the local `mysqldump` against the MySQL-family
//! servers of the Docker matrix (`DNT_TEST_DB=1`; skipped when `mysqldump` is not on
//! `PATH`). Each server gets a `p5_tools` database and a `p5_tools_user` account whose
//! password holds every character the option file escapes; both are dropped before (if
//! left over) and after the test.

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::secret::MemorySecretStore;
use dnt_core::sql::classify;
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_store::repo::NewSecrets;
use dnt_testkit::{require_db, Target};
use dnt_transfer::tools::{find_tool, MysqlDumpOptions, Tool};
use dntdatabase::backup::ToolDumpRequest;
use dntdatabase::commands::backup::{tool_dump_start_impl, tools_list_impl};
use dntdatabase::file_picks::FileAccess;
use dntdatabase::jobs::JobState;
use dntdatabase::state::{AppPaths, AppState, JOB_TMP_DIR};
use secrecy::SecretString;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const DB: &str = "p5_tools";
const USER: &str = "p5_tools_user";
/// `"`, `\`, `#`, `'`, `:`, `$`, `!` and a space: what the option file must carry.
const PASSWORD: &str = "p5\"Sup3r$ecret!\\x#'y :z";

struct Sink;

impl RowSink for Sink {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

async fn exec(s: &dyn Session, sql: &str) {
    s.execute_one(
        sql,
        classify(sql, s.dialect()),
        &ExecOptions::default(),
        &mut Sink,
    )
    .await
    .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

/// A MySQL string literal (default `sql_mode`: backslash escapes).
fn sql_string(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

async fn drop_scope(s: &dyn Session) {
    exec(s, &format!("DROP DATABASE IF EXISTS {DB}")).await;
    exec(s, &format!("DROP USER IF EXISTS '{USER}'@'%'")).await;
}

async fn setup(s: &dyn Session) {
    drop_scope(s).await;
    exec(s, &format!("CREATE DATABASE {DB} CHARACTER SET utf8mb4")).await;
    exec(
        s,
        &format!(
            "CREATE TABLE {DB}.p5_tools_a (id INT PRIMARY KEY, name VARCHAR(50), bin VARBINARY(4)) \
             ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
        ),
    )
    .await;
    exec(
        s,
        &format!(
            "CREATE TABLE {DB}.p5_tools_b (id INT PRIMARY KEY, a_id INT, \
             FOREIGN KEY (a_id) REFERENCES {DB}.p5_tools_a (id)) ENGINE=InnoDB"
        ),
    )
    .await;
    exec(
        s,
        &format!(
            "INSERT INTO {DB}.p5_tools_a VALUES (1, 'Nguyễn Văn A', 0x00ff), (2, 'it''s', NULL)"
        ),
    )
    .await;
    exec(s, &format!("INSERT INTO {DB}.p5_tools_b VALUES (10, 1)")).await;
    exec(
        s,
        &format!(
            "CREATE USER '{USER}'@'%' IDENTIFIED BY {}",
            sql_string(PASSWORD)
        ),
    )
    .await;
    exec(
        s,
        &format!("GRANT SELECT, SHOW VIEW, TRIGGER, LOCK TABLES ON {DB}.* TO '{USER}'@'%'"),
    )
    .await;
}

fn config(t: Target) -> ConnectionConfig {
    ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: format!("{t:?} tools"),
        sort_order: 0,
        kind: t.kind(),
        host: "127.0.0.1".into(),
        port: t.host_port(),
        username: USER.into(),
        default_db: Some(DB.into()),
        env_tag: None,
        read_only: true,
        secret_mode: SecretMode::Saved,
        ssh: None,
        ssl: None,
        options: ConnOptions::default(),
    }
}

fn read_dump(path: &Path, gzip: bool) -> String {
    let bytes = std::fs::read(path).unwrap();
    if gzip {
        assert_eq!(&bytes[..2], &[0x1f, 0x8b], "gzip magic");
        let mut s = String::new();
        flate2::read::MultiGzDecoder::new(bytes.as_slice())
            .read_to_string(&mut s)
            .unwrap();
        s
    } else {
        String::from_utf8(bytes).unwrap()
    }
}

/// Runs one tool dump job to the end; its detail.
async fn dump(
    state: &AppState,
    conn: Uuid,
    path: &Path,
    compress: bool,
) -> dntdatabase::jobs::JobDetail {
    let admitted = state
        .file_picks
        .admit(FileAccess::DumpWrite, path.to_path_buf())
        .unwrap();
    let request = ToolDumpRequest {
        connection_id: conn,
        database: DB.into(),
        tool: Tool::MysqlDump,
        mysql: Some(MysqlDumpOptions {
            tables: vec![],
            structure: true,
            data: true,
            routines: false,
            events: false,
            triggers: true,
            single_transaction: true,
        }),
        pg: None,
        compress,
        path: admitted,
    };
    let id = tool_dump_start_impl(state, request).await.unwrap();
    assert!(
        state.sessions.jobs.wait(id, Duration::from_secs(120)).await,
        "the tool dump did not finish"
    );
    state.sessions.jobs.detail(id).unwrap()
}

async fn mysqldump_job_on(t: Target) {
    let root = MySqlDriver.connect(&t.resolved(None)).await.unwrap();
    setup(root.as_ref()).await;

    let dir = tempfile::tempdir().unwrap();
    let state = AppState::open(
        AppPaths::in_dir(dir.path().join("data")),
        Arc::new(MemorySecretStore::default()),
    )
    .unwrap();
    let conn = state
        .repo
        .create(
            config(t),
            NewSecrets {
                db_password: Some(SecretString::from(PASSWORD.to_string())),
                ssh_password: None,
                ssh_key_passphrase: None,
            },
        )
        .unwrap()
        .id;
    state.sessions.open(conn, None).await.unwrap();

    for compress in [false, true] {
        let name = if compress { "out.sql.gz" } else { "out.sql" };
        let path = dir.path().join(name);
        let detail = dump(&state, conn, &path, compress).await;
        let log: Vec<String> = detail.log.iter().map(|l| l.message.clone()).collect();
        assert_eq!(
            detail.job.state,
            JobState::Done,
            "{t:?} compress={compress}: {:?}\n{}",
            detail.job.error,
            log.join("\n")
        );
        assert_eq!(
            detail.job.result_path.as_deref(),
            Some(path.to_str().unwrap())
        );
        let text = read_dump(&path, compress);
        assert!(text.contains("CREATE TABLE `p5_tools_a`"), "{t:?}: {text}");
        assert!(text.contains("CREATE TABLE `p5_tools_b`"), "{t:?}");
        assert!(text.contains("INSERT INTO `p5_tools_a`"), "{t:?}");
        assert!(text.contains("Nguyễn Văn A"), "{t:?}: UTF-8 data");
        assert!(text.contains("0x00FF"), "{t:?}: --hex-blob");
        assert!(text.contains("INSERT INTO `p5_tools_b`"), "{t:?}");
        // No part file left; the job's secret folder is gone.
        let part = dir.path().join(format!("{name}.dntpart"));
        assert!(!part.exists(), "{t:?}: {part:?} left");
        let tmp = dir
            .path()
            .join("data")
            .join(JOB_TMP_DIR)
            .join(detail.job.id.to_string());
        for _ in 0..50 {
            if !tmp.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!tmp.exists(), "{t:?}: {tmp:?} left");
        // The command line is logged; the password is nowhere (log, params, summary).
        assert!(
            log.iter()
                .any(|l| l.starts_with("Command: ") && l.contains("--defaults-file=")),
            "{log:?}"
        );
        for l in &log {
            assert!(!l.contains(PASSWORD), "{l}");
        }
        let params = serde_json::to_string(&detail.params).unwrap();
        assert!(!params.contains("Sup3r"), "{params}");
        assert!(!detail
            .job
            .summary
            .clone()
            .unwrap_or_default()
            .contains("Sup3r"));
        eprintln!(
            "{t:?} compress={compress}: {} ({} bytes)",
            detail.job.summary.unwrap_or_default(),
            std::fs::metadata(&path).unwrap().len()
        );
    }
    state.sessions.close(conn).await;
    drop(state);
    drop_scope(root.as_ref()).await;
    root.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysqldump_job_writes_dump() {
    require_db!();
    let path_env = std::env::var_os("PATH");
    if find_tool(Tool::MysqlDump, None, path_env.as_deref()).is_none() {
        eprintln!("skipped: mysqldump not found on PATH");
        return;
    }
    for t in Target::mysql_family() {
        mysqldump_job_on(t).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_list_reports_mysqldump() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::open(
        AppPaths::in_dir(dir.path().join("data")),
        Arc::new(MemorySecretStore::default()),
    )
    .unwrap();
    let list = tools_list_impl(&state).await.unwrap();
    let path_env = std::env::var_os("PATH");
    let installed = find_tool(Tool::MysqlDump, None, path_env.as_deref()).is_some();
    assert_eq!(
        list.tools.iter().any(|t| t.tool == Tool::MysqlDump),
        installed,
        "{list:?}"
    );
    eprintln!("{list:#?}");
}
