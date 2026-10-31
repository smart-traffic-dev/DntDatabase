//! Tests of `tools` (spec §7.2, R25–R28): discovery, argument vectors, secret files.

use super::*;
use dnt_core::config::{SslConfig, SslMode};
use dnt_core::server::{DbKind, ServerInfo, ServerVariant, SqlKind};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

const PASSWORD: &str = "Sup3r$ecret!";

fn info(tool: Tool, version: &str) -> ToolInfo {
    tool_info(
        tool,
        Path::new(r"C:\tools\bin").join(tool.file_name()).as_path(),
        version,
    )
}

fn mysql84() -> ToolInfo {
    info(
        Tool::MysqlDump,
        "mysqldump  Ver 8.4.3 for Win64 on x86_64 (MySQL Community Server - GPL)\n",
    )
}

fn maria_client() -> ToolInfo {
    info(
        Tool::MysqlDump,
        "mysqldump  Ver 10.19 Distrib 10.6.12-MariaDB, for Win64 (AMD64)",
    )
}

fn server(variant: ServerVariant, version: (u32, u32, u32)) -> ServerInfo {
    ServerInfo::new(
        match variant {
            ServerVariant::Postgres => DbKind::Postgres,
            _ => DbKind::MySql,
        },
        variant,
        version,
        String::new(),
    )
}

fn conn(kind: SqlKind, db: &str) -> ToolConn {
    ToolConn {
        kind,
        host: "127.0.0.1".into(),
        port: match kind {
            SqlKind::MySql => 3306,
            SqlKind::Postgres => 5432,
        },
        user: "app".into(),
        database: Some(db.into()),
        ssl: None,
        tunnelled: false,
    }
}

fn ssl(mode: SslMode) -> Option<SslConfig> {
    Some(SslConfig {
        version: 1,
        mode,
        ca_path: Some(PathBuf::from(r"C:\certs\ca.pem")),
        cert_path: Some(PathBuf::from(r"C:\certs\client.pem")),
        key_path: Some(PathBuf::from(r"C:\certs\client-key.pem")),
    })
}

fn mysql_opts(tables: &[&str]) -> MysqlDumpOptions {
    MysqlDumpOptions {
        tables: tables.iter().map(|t| t.to_string()).collect(),
        structure: true,
        data: true,
        routines: false,
        events: false,
        triggers: true,
        single_transaction: true,
    }
}

fn pg_opts() -> PgDumpOptions {
    PgDumpOptions {
        schemas: vec![],
        tables: vec![],
        structure: true,
        data: true,
        format: PgDumpFormat::Plain,
        no_owner: false,
        no_privileges: false,
    }
}

fn args(inv: &Invocation) -> Vec<String> {
    inv.args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn secret() -> PathBuf {
    PathBuf::from(r"C:\data\job-tmp\j1\client.cnf")
}

fn pgpass() -> PathBuf {
    PathBuf::from(r"C:\data\job-tmp\j1\pgpass.conf")
}

// ---------------------------------------------------------------------------------------
// Discovery (R25)
// ---------------------------------------------------------------------------------------

#[test]
fn find_tool_prefers_configured_dir_then_path() {
    let configured = tempfile::tempdir().unwrap();
    let on_path = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let exe = Tool::MysqlDump.file_name();
    std::fs::write(on_path.path().join(&exe), b"").unwrap();
    let path_env =
        std::env::join_paths([other.path(), on_path.path(), Path::new("relative\\bin")]).unwrap();
    // Only on PATH: found there.
    assert_eq!(
        find_tool(Tool::MysqlDump, Some(configured.path()), Some(&path_env)),
        Some(on_path.path().join(&exe))
    );
    // In the configured folder too: that one wins.
    std::fs::write(configured.path().join(&exe), b"").unwrap();
    assert_eq!(
        find_tool(Tool::MysqlDump, Some(configured.path()), Some(&path_env)),
        Some(configured.path().join(&exe))
    );
    // Not configured: PATH.
    assert_eq!(
        find_tool(Tool::MysqlDump, None, Some(&path_env)),
        Some(on_path.path().join(&exe))
    );
    // Nowhere.
    assert_eq!(
        find_tool(Tool::PgDump, Some(configured.path()), Some(&path_env)),
        None
    );
    assert_eq!(find_tool(Tool::MysqlDump, None, None), None);
    // A directory named like the tool is not a tool.
    std::fs::create_dir(other.path().join(Tool::Psql.file_name())).unwrap();
    assert_eq!(find_tool(Tool::Psql, None, Some(&path_env)), None);
    // The folders searched: configured first, PATH after, relative entries skipped.
    let dirs = search_dirs(Some(configured.path()), Some(&path_env));
    assert_eq!(
        dirs,
        vec![
            configured.path().to_path_buf(),
            other.path().to_path_buf(),
            on_path.path().to_path_buf()
        ]
    );
}

#[test]
fn relative_path_entries_are_never_searched() {
    let cwd_like = tempfile::tempdir().unwrap();
    std::fs::write(cwd_like.path().join(Tool::PgDump.file_name()), b"").unwrap();
    let path_env = OsString::from(".");
    assert_eq!(find_tool(Tool::PgDump, None, Some(&path_env)), None);
    // A relative configured folder is ignored too.
    assert_eq!(
        find_tool(Tool::PgDump, Some(Path::new("tools")), None),
        None
    );
}

#[test]
fn only_whitelisted_names() {
    let names: Vec<&str> = Tool::ALL.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        ["mysqldump", "mariadb-dump", "pg_dump", "pg_restore", "psql"]
    );
    for t in Tool::ALL {
        assert_eq!(Tool::from_name(t.name()), Some(t));
        if cfg!(windows) {
            assert_eq!(t.file_name(), format!("{}.exe", t.name()));
        }
    }
    for other in [
        "cmd",
        "cmd.exe",
        "powershell",
        "mysql",
        "MYSQLDUMP",
        "pg_dump.exe",
        "",
    ] {
        assert_eq!(Tool::from_name(other), None, "{other}");
    }
    assert_eq!(Tool::MysqlDump.kind(), SqlKind::MySql);
    assert_eq!(Tool::MariadbDump.kind(), SqlKind::MySql);
    assert_eq!(Tool::PgDump.kind(), SqlKind::Postgres);
    assert_eq!(Tool::PgRestore.kind(), SqlKind::Postgres);
    assert_eq!(Tool::Psql.kind(), SqlKind::Postgres);
}

#[test]
fn version_output_is_parsed() {
    let m = mysql84();
    assert_eq!(
        m.version,
        "mysqldump  Ver 8.4.3 for Win64 on x86_64 (MySQL Community Server - GPL)"
    );
    assert!(!m.mariadb);
    assert_eq!(client_version(&m), Some((8, 4, 3)));
    let old = info(
        Tool::MysqlDump,
        "mysqldump  Ver 10.13 Distrib 5.7.44, for Win64 (x86_64)",
    );
    assert_eq!(client_version(&old), Some((5, 7, 44)));
    let maria = maria_client();
    assert!(maria.mariadb);
    assert_eq!(client_version(&maria), Some((10, 6, 12)));
    let md = info(
        Tool::MariadbDump,
        "mariadb-dump from 11.4.2-MariaDB, client 10.19 for Win64 (AMD64)",
    );
    assert!(md.mariadb);
    assert_eq!(client_version(&md), Some((11, 4, 2)));
    let pg = info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2\n");
    assert_eq!(pg.version, "pg_dump (PostgreSQL) 17.2");
    assert!(!pg.mariadb);
    assert_eq!(client_version(&pg), Some((17, 2, 0)));
    // mariadb-dump is MariaDB whatever it prints.
    assert!(info(Tool::MariadbDump, "").mariadb);
    assert_eq!(info(Tool::PgDump, "\n\n").version, "");
}

// ---------------------------------------------------------------------------------------
// mysqldump / mariadb-dump (R26)
// ---------------------------------------------------------------------------------------

#[test]
fn mysqldump_args_mysql8_client_against_mysql8() {
    let inv = mysql_dump_args(
        &mysql84(),
        &conn(SqlKind::MySql, "shop"),
        &server(ServerVariant::MySql8, (8, 0, 36)),
        &mysql_opts(&["customers", "orders"]),
        &secret(),
    )
    .unwrap();
    assert_eq!(
        inv.program,
        Path::new(r"C:\tools\bin").join(Tool::MysqlDump.file_name())
    );
    assert_eq!(
        args(&inv),
        [
            r"--defaults-file=C:\data\job-tmp\j1\client.cnf",
            "--protocol=TCP",
            "--host=127.0.0.1",
            "--port=3306",
            "--user=app",
            "--ssl-mode=DISABLED",
            "--get-server-public-key",
            "--default-character-set=utf8mb4",
            "--hex-blob",
            "--no-tablespaces",
            "--single-transaction",
            "--triggers",
            "--set-gtid-purged=OFF",
            "--",
            "shop",
            "customers",
            "orders",
        ]
    );
    assert_eq!(inv.secret, SecretFileSpec::MysqlOptionFile(secret()));
    assert_eq!(inv.env_remove, ENV_REMOVE.to_vec());
    assert_eq!(
        inv.env_set,
        vec![(
            "MYSQL_TEST_LOGIN_FILE",
            OsString::from(r"C:\data\job-tmp\j1\no-login-path.cnf")
        )]
    );
    assert!(!inv.stdin_file);
    assert_eq!(inv.stdout, StdoutUse::File);
}

#[test]
fn mysqldump_args_mysql8_client_against_mariadb_and_57() {
    for srv in [
        server(ServerVariant::MariaDb, (10, 6, 28)),
        server(ServerVariant::MySql57, (5, 7, 44)),
    ] {
        let o = MysqlDumpOptions {
            routines: true,
            events: true,
            triggers: false,
            data: false,
            single_transaction: false,
            ..mysql_opts(&[])
        };
        let inv = mysql_dump_args(
            &mysql84(),
            &conn(SqlKind::MySql, "shop"),
            &srv,
            &o,
            &secret(),
        )
        .unwrap();
        assert_eq!(
            args(&inv)[5..],
            [
                "--ssl-mode=DISABLED",
                "--get-server-public-key",
                "--default-character-set=utf8mb4",
                "--hex-blob",
                "--no-tablespaces",
                "--routines",
                "--events",
                "--skip-triggers",
                "--no-data",
                "--column-statistics=0",
                "--set-gtid-purged=OFF",
                "--",
                "shop",
            ],
            "{srv:?}"
        );
    }
}

#[test]
fn mysqldump_args_mariadb_client() {
    let o = MysqlDumpOptions {
        structure: false,
        ..mysql_opts(&["t"])
    };
    let srv = server(ServerVariant::MariaDb, (11, 4, 2));
    let inv = mysql_dump_args(
        &maria_client(),
        &conn(SqlKind::MySql, "shop"),
        &srv,
        &o,
        &secret(),
    )
    .unwrap();
    // No --column-statistics, no --set-gtid-purged, no --get-server-public-key (MySQL
    // client options), --skip-ssl style TLS.
    assert_eq!(
        args(&inv)[5..],
        [
            "--skip-ssl",
            "--default-character-set=utf8mb4",
            "--hex-blob",
            "--no-tablespaces",
            "--single-transaction",
            "--triggers",
            "--no-create-info",
            "--",
            "shop",
            "t",
        ]
    );
    // mariadb-dump: the same options.
    let md = info(Tool::MariadbDump, "mariadb-dump from 11.4.2-MariaDB");
    let inv2 = mysql_dump_args(&md, &conn(SqlKind::MySql, "shop"), &srv, &o, &secret()).unwrap();
    assert_eq!(args(&inv2), args(&inv));
    assert_eq!(
        inv2.program,
        Path::new(r"C:\tools\bin").join(Tool::MariadbDump.file_name())
    );
}

#[test]
fn mysql_tls_arguments() {
    let srv = server(ServerVariant::MySql8, (8, 0, 36));
    let tls_args = |client: &ToolInfo, mode: SslMode| {
        let c = ToolConn {
            ssl: ssl(mode),
            ..conn(SqlKind::MySql, "shop")
        };
        let inv = mysql_dump_args(client, &c, &srv, &mysql_opts(&[]), &secret()).unwrap();
        let a = args(&inv);
        let start = 5;
        let end = a
            .iter()
            .position(|x| x == "--default-character-set=utf8mb4")
            .unwrap();
        a[start..end].to_vec()
    };
    assert_eq!(
        tls_args(&mysql84(), SslMode::Disable),
        ["--ssl-mode=DISABLED", "--get-server-public-key"]
    );
    assert_eq!(
        tls_args(&mysql84(), SslMode::Prefer),
        [
            "--ssl-mode=PREFERRED",
            r"--ssl-cert=C:\certs\client.pem",
            r"--ssl-key=C:\certs\client-key.pem",
            "--get-server-public-key"
        ]
    );
    assert_eq!(
        tls_args(&mysql84(), SslMode::Require),
        [
            "--ssl-mode=REQUIRED",
            r"--ssl-cert=C:\certs\client.pem",
            r"--ssl-key=C:\certs\client-key.pem",
        ]
    );
    assert_eq!(
        tls_args(&mysql84(), SslMode::VerifyCa),
        [
            "--ssl-mode=VERIFY_CA",
            r"--ssl-ca=C:\certs\ca.pem",
            r"--ssl-cert=C:\certs\client.pem",
            r"--ssl-key=C:\certs\client-key.pem",
        ]
    );
    assert_eq!(
        tls_args(&mysql84(), SslMode::VerifyFull),
        [
            "--ssl-mode=VERIFY_IDENTITY",
            r"--ssl-ca=C:\certs\ca.pem",
            r"--ssl-cert=C:\certs\client.pem",
            r"--ssl-key=C:\certs\client-key.pem",
        ]
    );
    let maria = maria_client();
    assert_eq!(tls_args(&maria, SslMode::Disable), ["--skip-ssl"]);
    assert_eq!(
        tls_args(&maria, SslMode::Require),
        [
            "--ssl",
            "--skip-ssl-verify-server-cert",
            r"--ssl-cert=C:\certs\client.pem",
            r"--ssl-key=C:\certs\client-key.pem",
        ]
    );
    assert_eq!(
        tls_args(&maria, SslMode::VerifyCa),
        [
            "--ssl",
            "--skip-ssl-verify-server-cert",
            r"--ssl-ca=C:\certs\ca.pem",
            r"--ssl-cert=C:\certs\client.pem",
            r"--ssl-key=C:\certs\client-key.pem",
        ]
    );
    assert_eq!(
        tls_args(&maria, SslMode::VerifyFull),
        [
            "--ssl",
            "--ssl-verify-server-cert",
            r"--ssl-ca=C:\certs\ca.pem",
            r"--ssl-cert=C:\certs\client.pem",
            r"--ssl-key=C:\certs\client-key.pem",
        ]
    );
}

#[test]
fn tunnel_with_verify_full_is_refused() {
    let srv = server(ServerVariant::MySql8, (8, 0, 36));
    let c = ToolConn {
        ssl: ssl(SslMode::VerifyFull),
        tunnelled: true,
        ..conn(SqlKind::MySql, "shop")
    };
    assert_eq!(
        mysql_dump_args(&mysql84(), &c, &srv, &mysql_opts(&[]), &secret()),
        Err(ToolError::VerifyFullThroughTunnel)
    );
    let pg = ToolConn {
        ssl: ssl(SslMode::VerifyFull),
        tunnelled: true,
        ..conn(SqlKind::Postgres, "app")
    };
    let pgd = info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2");
    assert_eq!(
        pg_dump_args(&pgd, &pg, &pg_opts(), &pgpass()),
        Err(ToolError::VerifyFullThroughTunnel)
    );
    let psql = info(Tool::Psql, "psql (PostgreSQL) 17.2");
    assert_eq!(
        psql_args(
            &psql,
            &pg,
            &PsqlOptions {
                single_transaction: true
            },
            &pgpass()
        ),
        Err(ToolError::VerifyFullThroughTunnel)
    );
    // verify-ca through the tunnel is fine (the chain is checked, not the name).
    let ca = ToolConn {
        ssl: ssl(SslMode::VerifyCa),
        ..pg.clone()
    };
    assert!(pg_dump_args(&pgd, &ca, &pg_opts(), &pgpass()).is_ok());
    // Without a tunnel verify-full is allowed.
    let direct = ToolConn {
        tunnelled: false,
        ..pg
    };
    assert!(pg_dump_args(&pgd, &direct, &pg_opts(), &pgpass()).is_ok());
}

#[test]
fn unsafe_names_refused() {
    let srv = server(ServerVariant::MySql8, (8, 0, 36));
    for bad in ["-x", "--all-databases", "a\nb", "a\rb", "a\0b", ""] {
        assert!(
            matches!(
                mysql_dump_args(
                    &mysql84(),
                    &conn(SqlKind::MySql, "shop"),
                    &srv,
                    &mysql_opts(&[bad]),
                    &secret()
                ),
                Err(ToolError::UnsafeName { .. })
            ),
            "table {bad:?}"
        );
        assert!(
            matches!(
                mysql_dump_args(
                    &mysql84(),
                    &conn(SqlKind::MySql, bad),
                    &srv,
                    &mysql_opts(&[]),
                    &secret()
                ),
                Err(ToolError::UnsafeName { .. } | ToolError::NoDatabase)
            ),
            "database {bad:?}"
        );
        let pgd = info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2");
        let o = PgDumpOptions {
            schemas: vec![bad.to_string()],
            ..pg_opts()
        };
        assert!(
            matches!(
                pg_dump_args(&pgd, &conn(SqlKind::Postgres, "app"), &o, &pgpass()),
                Err(ToolError::UnsafeName { .. })
            ),
            "schema {bad:?}"
        );
        let o = PgDumpOptions {
            tables: vec![("public".into(), bad.to_string())],
            ..pg_opts()
        };
        assert!(
            matches!(
                pg_dump_args(&pgd, &conn(SqlKind::Postgres, "app"), &o, &pgpass()),
                Err(ToolError::UnsafeName { .. })
            ),
            "table {bad:?}"
        );
    }
    // A line break in the host or user is refused too.
    let c = ToolConn {
        user: "a\nb".into(),
        ..conn(SqlKind::MySql, "shop")
    };
    assert!(matches!(
        mysql_dump_args(&mysql84(), &c, &srv, &mysql_opts(&[]), &secret()),
        Err(ToolError::UnsafeName { .. })
    ));
    // Names with spaces, quotes and dashes inside are fine (non-ASCII: see
    // `non_ascii_refused_on_windows`).
    let ok = mysql_dump_args(
        &mysql84(),
        &conn(SqlKind::MySql, "shop"),
        &srv,
        &mysql_opts(&["don hang", "a\"b", "x-y"]),
        &secret(),
    )
    .unwrap();
    assert_eq!(
        args(&ok)[args(&ok).len() - 3..],
        ["don hang", "a\"b", "x-y"]
    );
}

#[test]
fn mysqldump_needs_structure_or_data_and_the_right_tool() {
    let srv = server(ServerVariant::MySql8, (8, 0, 36));
    let o = MysqlDumpOptions {
        structure: false,
        data: false,
        ..mysql_opts(&[])
    };
    assert_eq!(
        mysql_dump_args(
            &mysql84(),
            &conn(SqlKind::MySql, "shop"),
            &srv,
            &o,
            &secret()
        ),
        Err(ToolError::NothingToDump)
    );
    let pgd = info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2");
    assert!(matches!(
        mysql_dump_args(
            &pgd,
            &conn(SqlKind::MySql, "shop"),
            &srv,
            &mysql_opts(&[]),
            &secret()
        ),
        Err(ToolError::WrongTool { .. })
    ));
    assert!(matches!(
        mysql_dump_args(
            &mysql84(),
            &conn(SqlKind::Postgres, "shop"),
            &srv,
            &mysql_opts(&[]),
            &secret()
        ),
        Err(ToolError::WrongTool { .. })
    ));
    let c = ToolConn {
        database: None,
        ..conn(SqlKind::MySql, "shop")
    };
    assert_eq!(
        mysql_dump_args(&mysql84(), &c, &srv, &mysql_opts(&[]), &secret()),
        Err(ToolError::NoDatabase)
    );
}

// ---------------------------------------------------------------------------------------
// PostgreSQL tools
// ---------------------------------------------------------------------------------------

#[test]
fn pg_dump_args_each_format() {
    let pgd = info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2");
    for (format, flag) in [
        (PgDumpFormat::Plain, "--format=p"),
        (PgDumpFormat::Custom, "--format=c"),
        (PgDumpFormat::Tar, "--format=t"),
    ] {
        let o = PgDumpOptions {
            format,
            ..pg_opts()
        };
        let inv = pg_dump_args(&pgd, &conn(SqlKind::Postgres, "app"), &o, &pgpass()).unwrap();
        assert_eq!(
            args(&inv),
            [
                "--no-password",
                flag,
                "--encoding=UTF8",
                "--dbname=host='127.0.0.1' port='5432' user='app' dbname='app' sslmode='disable'",
            ]
        );
        assert_eq!(inv.secret, SecretFileSpec::PgPass(pgpass()));
        assert_eq!(inv.env_remove, ENV_REMOVE.to_vec());
        assert_eq!(
            inv.env_set,
            vec![(
                "PGPASSFILE",
                OsString::from(r"C:\data\job-tmp\j1\pgpass.conf")
            )]
        );
        assert_eq!(inv.stdout, StdoutUse::File);
        assert!(!inv.stdin_file);
    }
}

#[test]
fn pg_dump_args_schemas_tables_and_switches() {
    let pgd = info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2");
    let o = PgDumpOptions {
        schemas: vec!["public".into(), "a\"b".into()],
        tables: vec![
            ("public".into(), "orders".into()),
            ("we ird".into(), "x\"y.z*".into()),
        ],
        structure: true,
        data: false,
        format: PgDumpFormat::Custom,
        no_owner: true,
        no_privileges: true,
    };
    let c = ToolConn {
        user: "o'neil".into(),
        database: Some(r"d\b x".into()),
        ssl: ssl(SslMode::VerifyFull),
        host: "db.example.com".into(),
        ..conn(SqlKind::Postgres, "app")
    };
    let inv = pg_dump_args(&pgd, &c, &o, &pgpass()).unwrap();
    assert_eq!(
        args(&inv),
        [
            "--no-password",
            "--format=c",
            "--encoding=UTF8",
            "--schema-only",
            "--no-owner",
            "--no-privileges",
            "--schema=\"public\"",
            "--schema=\"a\"\"b\"",
            "--table=\"public\".\"orders\"",
            "--table=\"we ird\".\"x\"\"y.z*\"",
            r"--dbname=host='db.example.com' port='5432' user='o\'neil' dbname='d\\b x' sslmode='verify-full' sslrootcert='C:\\certs\\ca.pem' sslcert='C:\\certs\\client.pem' sslkey='C:\\certs\\client-key.pem'",
        ]
    );
    let data_only = PgDumpOptions {
        structure: false,
        data: true,
        ..pg_opts()
    };
    let inv = pg_dump_args(&pgd, &conn(SqlKind::Postgres, "app"), &data_only, &pgpass()).unwrap();
    assert!(args(&inv).contains(&"--data-only".to_string()));
    let none = PgDumpOptions {
        structure: false,
        data: false,
        ..pg_opts()
    };
    assert_eq!(
        pg_dump_args(&pgd, &conn(SqlKind::Postgres, "app"), &none, &pgpass()),
        Err(ToolError::NothingToDump)
    );
}

#[test]
fn pg_sslmodes_in_conninfo() {
    let pgd = info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2");
    let mode_of = |mode: Option<SslMode>| {
        let c = ToolConn {
            ssl: mode.and_then(ssl),
            ..conn(SqlKind::Postgres, "app")
        };
        let a = args(&pg_dump_args(&pgd, &c, &pg_opts(), &pgpass()).unwrap());
        a.last().unwrap().clone()
    };
    assert!(mode_of(None).ends_with("sslmode='disable'"));
    assert!(mode_of(Some(SslMode::Disable)).ends_with("sslmode='disable'"));
    // Prefer / require check no certificate: no root certificate is passed.
    assert!(mode_of(Some(SslMode::Prefer)).ends_with(
        r"sslmode='prefer' sslcert='C:\\certs\\client.pem' sslkey='C:\\certs\\client-key.pem'"
    ));
    assert!(mode_of(Some(SslMode::Require)).contains("sslmode='require' sslcert="));
    assert!(mode_of(Some(SslMode::VerifyCa))
        .contains(r"sslmode='verify-ca' sslrootcert='C:\\certs\\ca.pem'"));
}

#[test]
fn psql_args_exact() {
    let psql = info(Tool::Psql, "psql (PostgreSQL) 17.2");
    let inv = psql_args(
        &psql,
        &conn(SqlKind::Postgres, "app"),
        &PsqlOptions {
            single_transaction: true,
        },
        &pgpass(),
    )
    .unwrap();
    assert_eq!(
        args(&inv),
        [
            "-X",
            "--no-password",
            "-v",
            "ON_ERROR_STOP=1",
            "--single-transaction",
            "--dbname=host='127.0.0.1' port='5432' user='app' dbname='app' sslmode='disable'",
            "-f",
            "-",
        ]
    );
    assert!(inv.stdin_file);
    assert_eq!(inv.stdout, StdoutUse::Discard);
    assert_eq!(inv.secret, SecretFileSpec::PgPass(pgpass()));
    let inv = psql_args(
        &psql,
        &conn(SqlKind::Postgres, "app"),
        &PsqlOptions {
            single_transaction: false,
        },
        &pgpass(),
    )
    .unwrap();
    assert!(!args(&inv).contains(&"--single-transaction".to_string()));
    let pgd = info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2");
    assert!(matches!(
        psql_args(
            &pgd,
            &conn(SqlKind::Postgres, "app"),
            &PsqlOptions {
                single_transaction: false
            },
            &pgpass()
        ),
        Err(ToolError::WrongTool { .. })
    ));
}

#[test]
fn pg_restore_args_exact() {
    let pr = info(Tool::PgRestore, "pg_restore (PostgreSQL) 17.2");
    let file = Path::new(r"C:\dumps\app.dump");
    let all = PgRestoreOptions {
        single_transaction: true,
        clean: true,
        no_owner: true,
        no_privileges: true,
    };
    let inv = pg_restore_args(&pr, &conn(SqlKind::Postgres, "app"), &all, &pgpass(), file).unwrap();
    assert_eq!(
        args(&inv),
        [
            "--no-password",
            "--exit-on-error",
            "--single-transaction",
            "--clean",
            "--if-exists",
            "--no-owner",
            "--no-privileges",
            "--dbname=host='127.0.0.1' port='5432' user='app' dbname='app' sslmode='disable'",
            r"C:\dumps\app.dump",
        ]
    );
    assert!(!inv.stdin_file);
    assert_eq!(inv.stdout, StdoutUse::Discard);
    let none = PgRestoreOptions {
        single_transaction: false,
        clean: false,
        no_owner: false,
        no_privileges: false,
    };
    let inv =
        pg_restore_args(&pr, &conn(SqlKind::Postgres, "app"), &none, &pgpass(), file).unwrap();
    assert_eq!(
        args(&inv),
        [
            "--no-password",
            "--exit-on-error",
            "--dbname=host='127.0.0.1' port='5432' user='app' dbname='app' sslmode='disable'",
            r"C:\dumps\app.dump",
        ]
    );
    // A file argument that looks like an option is refused.
    assert!(matches!(
        pg_restore_args(
            &pr,
            &conn(SqlKind::Postgres, "app"),
            &none,
            &pgpass(),
            Path::new("-f")
        ),
        Err(ToolError::UnsafeName { .. })
    ));
}

#[test]
fn pg_restore_list_args_exact() {
    let pr = info(Tool::PgRestore, "pg_restore (PostgreSQL) 17.2");
    let inv = pg_restore_list_args(&pr, Path::new(r"C:\dumps\app.dump")).unwrap();
    assert_eq!(args(&inv), ["--list", r"C:\dumps\app.dump"]);
    assert_eq!(inv.secret, SecretFileSpec::None);
    assert_eq!(inv.stdout, StdoutUse::List);
    assert!(inv.env_set.is_empty());
    assert_eq!(inv.env_remove, ENV_REMOVE.to_vec());
    let pgd = info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2");
    assert!(matches!(
        pg_restore_list_args(&pgd, Path::new(r"C:\dumps\app.dump")),
        Err(ToolError::WrongTool { .. })
    ));
}

// ---------------------------------------------------------------------------------------
// Secrets (R27)
// ---------------------------------------------------------------------------------------

#[test]
fn option_file_and_pgpass_escaping() {
    assert_eq!(
        mysql_option_file("p\"a\\ss:w").unwrap(),
        "[client]\npassword=\"p\\\"a\\\\ss:w\"\n"
    );
    assert_eq!(
        mysql_option_file("a#b'c d\te\nf\rg").unwrap(),
        "[client]\npassword=\"a#b'c d\\te\\nf\\rg\"\n"
    );
    assert_eq!(mysql_option_file("").unwrap(), "[client]\npassword=\"\"\n");
    assert_eq!(mysql_option_file("a\0b"), Err(ToolError::UnsafePassword));
    let local = scope("127.0.0.1", 5432, "app");
    assert_eq!(
        pgpass_line(&local, "p\"a\\ss:w").unwrap(),
        "127.0.0.1:5432:*:app:p\"a\\\\ss\\:w\n"
    );
    assert_eq!(
        pgpass_line(&local, "plain").unwrap(),
        "127.0.0.1:5432:*:app:plain\n"
    );
    // Every field is escaped (an IPv6 host has colons, a user may hold `\`).
    assert_eq!(
        pgpass_line(&scope("::1", 6432, r"dom\u:x"), "pw").unwrap(),
        r"\:\:1:6432:*:dom\\u\:x:pw".to_string() + "\n"
    );
    for bad in ["a\nb", "a\rb", "a\0b"] {
        assert_eq!(
            pgpass_line(&local, bad),
            Err(ToolError::UnsafePassword),
            "{bad:?}"
        );
    }
    assert!(matches!(
        pgpass_line(&scope("a\nb", 1, "u"), "pw"),
        Err(ToolError::UnsafeName { .. })
    ));
}

fn scope(host: &str, port: u16, user: &str) -> PgPassScope {
    PgPassScope {
        host: host.into(),
        port,
        user: user.into(),
    }
}

/// The pgpass line is valid only for the server the tool was started for (a `\connect`
/// elsewhere in a restored file gets no password); through a tunnel that is the local
/// end.
#[test]
fn pgpass_scope_is_the_tool_server() {
    let tunnelled = ToolConn {
        host: "127.0.0.1".into(),
        port: 50123,
        user: "app".into(),
        tunnelled: true,
        ..conn(SqlKind::Postgres, "app")
    };
    let invs = [
        pg_dump_args(
            &info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2"),
            &tunnelled,
            &pg_opts(),
            &pgpass(),
        )
        .unwrap(),
        psql_args(
            &info(Tool::Psql, "psql (PostgreSQL) 17.2"),
            &tunnelled,
            &PsqlOptions {
                single_transaction: false,
            },
            &pgpass(),
        )
        .unwrap(),
        pg_restore_args(
            &info(Tool::PgRestore, "pg_restore (PostgreSQL) 17.2"),
            &tunnelled,
            &PgRestoreOptions {
                single_transaction: false,
                clean: false,
                no_owner: false,
                no_privileges: false,
            },
            &pgpass(),
            Path::new(r"C:\d.dump"),
        )
        .unwrap(),
    ];
    for inv in &invs {
        assert_eq!(inv.pgpass_scope, Some(scope("127.0.0.1", 50123, "app")));
        let line = pgpass_line(inv.pgpass_scope.as_ref().unwrap(), PASSWORD).unwrap();
        assert!(line.starts_with("127.0.0.1:50123:*:app:"), "{line}");
        assert!(!line.starts_with("*:"));
    }
    let mysql = mysql_dump_args(
        &mysql84(),
        &conn(SqlKind::MySql, "shop"),
        &server(ServerVariant::MySql8, (8, 0, 36)),
        &mysql_opts(&[]),
        &secret(),
    )
    .unwrap();
    assert_eq!(mysql.pgpass_scope, None);
}

#[test]
fn scrubbed_environment() {
    for name in [
        "PGHOST",
        "PGPASSWORD",
        "PGSSLMODE",
        "PGSERVICEFILE",
        "pgpassfile",
        "MYSQL_PWD",
        "MYSQL_HOST",
        "mysql_test_login_file",
        "LIBMYSQL_ENABLE_CLEARTEXT_PLUGIN",
        "MARIADB_PWD",
    ] {
        assert!(is_scrubbed_env(OsStr::new(name)), "{name}");
    }
    for name in [
        "SystemRoot",
        "PATH",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "APPDATA",
        "MYSQLX",
    ] {
        assert!(!is_scrubbed_env(OsStr::new(name)), "{name}");
    }
}

/// On Windows the clients read argv and the environment in the ANSI code page, so
/// non-ASCII names, users and paths are refused before anything runs; elsewhere they
/// pass through.
#[test]
fn non_ascii_refused_on_windows() {
    let srv = server(ServerVariant::MySql8, (8, 0, 36));
    let pgd = info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2");
    let mysql_table = mysql_dump_args(
        &mysql84(),
        &conn(SqlKind::MySql, "shop"),
        &srv,
        &mysql_opts(&["đơn_hàng"]),
        &secret(),
    );
    let mysql_db = mysql_dump_args(
        &mysql84(),
        &conn(SqlKind::MySql, "cửa_hàng"),
        &srv,
        &mysql_opts(&[]),
        &secret(),
    );
    let mysql_user = mysql_dump_args(
        &mysql84(),
        &ToolConn {
            user: "người_dùng".into(),
            ..conn(SqlKind::MySql, "shop")
        },
        &srv,
        &mysql_opts(&[]),
        &secret(),
    );
    let pg_schema = pg_dump_args(
        &pgd,
        &conn(SqlKind::Postgres, "app"),
        &PgDumpOptions {
            schemas: vec!["bán_hàng".into()],
            ..pg_opts()
        },
        &pgpass(),
    );
    let pg_table = pg_dump_args(
        &pgd,
        &conn(SqlKind::Postgres, "app"),
        &PgDumpOptions {
            tables: vec![("public".into(), "đơn_hàng".into())],
            ..pg_opts()
        },
        &pgpass(),
    );
    let pg_db = pg_dump_args(
        &pgd,
        &conn(SqlKind::Postgres, "dữ_liệu"),
        &pg_opts(),
        &pgpass(),
    );
    let secret_path = mysql_dump_args(
        &mysql84(),
        &conn(SqlKind::MySql, "shop"),
        &srv,
        &mysql_opts(&[]),
        Path::new(r"C:\Users\Đức\job-tmp\j1\client.cnf"),
    );
    let pgpass_path = pg_dump_args(
        &pgd,
        &conn(SqlKind::Postgres, "app"),
        &pg_opts(),
        Path::new(r"C:\Users\Đức\job-tmp\j1\pgpass.conf"),
    );
    let restore_file = pg_restore_list_args(
        &info(Tool::PgRestore, "pg_restore (PostgreSQL) 17.2"),
        Path::new(r"D:\Sao lưu\app.dump"),
    );
    if ARGV_ASCII_ONLY {
        for r in [
            mysql_table,
            mysql_db,
            mysql_user,
            pg_schema,
            pg_table,
            pg_db,
        ] {
            let e = r.unwrap_err();
            assert!(matches!(e, ToolError::NonAsciiName { .. }), "{e:?}");
            assert!(e
                .to_string()
                .starts_with("External tools on Windows cannot receive non-ASCII names"));
            assert!(e.to_string().contains("use the built-in dump"));
        }
        for r in [secret_path, pgpass_path] {
            assert!(matches!(r, Err(ToolError::NonAsciiPath { .. })), "{r:?}");
        }
        assert!(matches!(restore_file, Err(ToolError::NonAsciiPath { .. })));
    } else {
        for r in [
            mysql_table,
            mysql_db,
            mysql_user,
            pg_schema,
            pg_table,
            pg_db,
            secret_path,
            pgpass_path,
        ] {
            assert!(r.is_ok(), "{r:?}");
        }
        assert!(restore_file.is_ok());
    }
}

#[test]
fn secrets_never_in_args_env_or_log() {
    let mysql = mysql_dump_args(
        &mysql84(),
        &conn(SqlKind::MySql, "shop"),
        &server(ServerVariant::MySql8, (8, 0, 36)),
        &mysql_opts(&["a"]),
        &secret(),
    )
    .unwrap();
    let pg_conn = ToolConn {
        ssl: ssl(SslMode::VerifyCa),
        ..conn(SqlKind::Postgres, "app")
    };
    let pgd = pg_dump_args(
        &info(Tool::PgDump, "pg_dump (PostgreSQL) 17.2"),
        &pg_conn,
        &pg_opts(),
        &pgpass(),
    )
    .unwrap();
    let psql = psql_args(
        &info(Tool::Psql, "psql (PostgreSQL) 17.2"),
        &pg_conn,
        &PsqlOptions {
            single_transaction: true,
        },
        &pgpass(),
    )
    .unwrap();
    let pr = pg_restore_args(
        &info(Tool::PgRestore, "pg_restore (PostgreSQL) 17.2"),
        &pg_conn,
        &PgRestoreOptions {
            single_transaction: true,
            clean: true,
            no_owner: true,
            no_privileges: true,
        },
        &pgpass(),
        Path::new(r"C:\d.dump"),
    )
    .unwrap();
    for inv in [&mysql, &pgd, &psql, &pr] {
        for a in &inv.args {
            assert!(!a.to_string_lossy().contains(PASSWORD), "{a:?}");
            assert!(!a
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("password="));
        }
        for (k, v) in &inv.env_set {
            assert!(!v.to_string_lossy().contains(PASSWORD), "{k}");
            assert_ne!(*k, "PGPASSWORD");
            assert_ne!(*k, "MYSQL_PWD");
        }
        for k in ["PGPASSWORD", "MYSQL_PWD", "PGSERVICE", "PGSERVICEFILE"] {
            assert!(inv.env_remove.contains(&k), "{k}");
        }
        let shown = shown_command(inv);
        assert!(!shown.contains(PASSWORD), "{shown}");
        assert!(shown.contains(&inv.program.to_string_lossy().into_owned()));
    }
    // The secret lives only in the file contents.
    assert!(mysql_option_file(PASSWORD).unwrap().contains(PASSWORD));
    assert!(pgpass_line(&scope("127.0.0.1", 5432, "app"), PASSWORD)
        .unwrap()
        .contains(PASSWORD));
    // Stderr lines are masked.
    let line = format!(
        "mysqldump: Got error: 1045: Access denied (using password: {PASSWORD}) {PASSWORD}"
    );
    let masked = mask(&line, PASSWORD);
    assert!(!masked.contains(PASSWORD));
    assert_eq!(
        masked,
        "mysqldump: Got error: 1045: Access denied (using password: ******) ******"
    );
    assert_eq!(mask("nothing here", PASSWORD), "nothing here");
    assert_eq!(
        mask("an empty password masks nothing", ""),
        "an empty password masks nothing"
    );
}

#[test]
fn shown_command_quotes_arguments() {
    let inv = Invocation {
        program: PathBuf::from(r"C:\Program Files\PostgreSQL\17\bin\pg_dump.exe"),
        args: vec![
            OsString::from("--no-password"),
            OsString::from("--schema=\"a b\""),
            OsString::from(""),
        ],
        env_remove: ENV_REMOVE.to_vec(),
        env_set: vec![("PGPASSFILE", OsString::from(r"C:\tmp\pgpass.conf"))],
        secret: SecretFileSpec::PgPass(PathBuf::from(r"C:\tmp\pgpass.conf")),
        pgpass_scope: None,
        stdin_file: false,
        stdout: StdoutUse::File,
    };
    assert_eq!(
        shown_command(&inv),
        r#"PGPASSFILE=C:\tmp\pgpass.conf "C:\Program Files\PostgreSQL\17\bin\pg_dump.exe" --no-password "--schema=\"a b\"" """#
    );
}

// ---------------------------------------------------------------------------------------
// Output, input and outcomes
// ---------------------------------------------------------------------------------------

#[test]
fn tool_output_plain_and_gzip() {
    let dir = tempfile::tempdir().unwrap();
    for compress in [false, true] {
        let path = dir.path().join(format!("out-{compress}"));
        let file = std::fs::File::create(&path).unwrap();
        let mut out = ToolOutput::new(Box::new(file), compress);
        out.write(b"CREATE TABLE t (id int);\n").unwrap();
        out.write("-- đơn hàng\n".as_bytes()).unwrap();
        assert_eq!(out.finish().unwrap(), 25 + "-- đơn hàng\n".len() as u64);
        let mut text = String::new();
        open_input(&path, compress)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "CREATE TABLE t (id int);\n-- đơn hàng\n");
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(raw.starts_with(&[0x1f, 0x8b]), compress);
    }
}

#[test]
fn tool_input_reads_every_gzip_member() {
    // `cat a.gz b.gz > all.gz` is a valid gzip file; every member is restored.
    use flate2::write::GzEncoder;
    use std::io::Write;
    let member = |s: &str| {
        let mut e = GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(s.as_bytes()).unwrap();
        e.finish().unwrap()
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.sql.gz");
    let mut bytes = member("CREATE TABLE a (id int);\n");
    bytes.extend(member("CREATE TABLE b (id int);\n"));
    std::fs::write(&path, bytes).unwrap();
    let mut text = String::new();
    open_input(&path, true)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "CREATE TABLE a (id int);\nCREATE TABLE b (id int);\n");
}

#[test]
fn restore_outcomes() {
    use Tool::{PgRestore, Psql};
    for t in [Psql, PgRestore] {
        assert_eq!(restore_outcome(t, Some(0), false, true), ToolOutcome::Done);
        assert_eq!(restore_outcome(t, Some(0), false, false), ToolOutcome::Done);
        assert_eq!(
            restore_outcome(t, Some(0), true, true),
            ToolOutcome::Partial
        );
        assert_eq!(restore_outcome(t, None, false, true), ToolOutcome::Partial);
        assert_eq!(restore_outcome(t, None, true, false), ToolOutcome::Partial);
    }
    // psql: only exit 3 (ON_ERROR_STOP) under --single-transaction rolled back.
    assert_eq!(
        restore_outcome(Psql, Some(3), false, true),
        ToolOutcome::FailedRolledBack
    );
    assert_eq!(
        restore_outcome(Psql, Some(3), false, false),
        ToolOutcome::Partial
    );
    assert_eq!(
        restore_outcome(Psql, Some(1), false, true),
        ToolOutcome::Partial
    );
    assert_eq!(
        restore_outcome(Psql, Some(2), false, true),
        ToolOutcome::Partial
    );
    assert_eq!(
        restore_outcome(Psql, Some(3), true, true),
        ToolOutcome::Partial
    );
    // pg_restore: every failure may have left part of the restore.
    assert_eq!(
        restore_outcome(PgRestore, Some(1), false, true),
        ToolOutcome::Partial
    );
    assert_eq!(
        restore_outcome(PgRestore, Some(3), false, true),
        ToolOutcome::Partial
    );
}

#[test]
fn tool_conn_from_resolved() {
    let r = dnt_core::config::ResolvedConfig {
        kind: DbKind::MySql,
        host: "127.0.0.1".into(),
        port: 50123,
        tls_server_name: "db.internal".into(),
        username: "app".into(),
        password: Some(secrecy::SecretString::from(PASSWORD.to_string())),
        database: Some("shop".into()),
        ssl: ssl(SslMode::VerifyCa),
        connect_timeout: std::time::Duration::from_secs(10),
        read_only: true,
        application_name: "DntDatabase".into(),
    };
    let c = ToolConn::from_resolved(&r, true).unwrap();
    assert_eq!(c.kind, SqlKind::MySql);
    assert_eq!(c.host, "127.0.0.1");
    assert_eq!(c.port, 50123);
    assert_eq!(c.user, "app");
    assert_eq!(c.database.as_deref(), Some("shop"));
    assert_eq!(c.ssl, ssl(SslMode::VerifyCa));
    assert!(c.tunnelled);
    assert!(!format!("{c:?}").contains(PASSWORD));
}
