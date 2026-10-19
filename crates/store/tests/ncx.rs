use dnt_core::config::{SecretMode, SshAuth, SslMode};
use dnt_core::error::DbError;
use dnt_core::server::DbKind;
use dnt_store::ncx::parse_ncx;
use dnt_store::transfer::ImportSource;
use secrecy::ExposeSecret;

const SAMPLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/ncx/sample.ncx");

fn sample() -> dnt_store::transfer::ImportBundle {
    let xml = std::fs::read_to_string(SAMPLE).unwrap();
    parse_ncx(&xml).unwrap()
}

#[test]
fn parse_sample() {
    let b = sample();
    assert_eq!(b.source, ImportSource::Navicat);
    assert!(b.groups.is_empty());
    // MySQL, MariaDB, PostgreSQL and the broken-password MySQL are valid; SQLSERVER is skipped.
    assert_eq!(b.connections.len(), 4);
    assert_eq!(b.skipped.len(), 1);
    assert_eq!(b.skipped[0].name, "Legacy MSSQL");
    assert_eq!(b.skipped[0].reason, "Unsupported type SQLSERVER");

    let by_name = |n: &str| b.connections.iter().find(|c| c.name == n).unwrap();

    let my = by_name("Local MySQL via SSH");
    assert_eq!(my.kind, DbKind::MySql);
    assert_eq!((my.host.as_str(), my.port), ("10.0.0.5", 3306));
    assert_eq!(my.username, "root");
    assert_eq!(my.default_db.as_deref(), Some("shop"));
    assert_eq!(my.secret_mode, SecretMode::Saved);
    let ssh = my.ssh.as_ref().unwrap();
    assert_eq!(ssh.auth, SshAuth::Key);
    assert_eq!((ssh.host.as_str(), ssh.port), ("bastion.example.com", 2222));
    assert_eq!(ssh.user, "deploy");
    assert_eq!(
        ssh.key_path.as_deref().unwrap().to_str().unwrap(),
        r"C:\Users\dev\.ssh\id_rsa"
    );
    assert!(my.ssl.is_none());
    let s = &b.secrets[&my.id];
    assert_eq!(s.db_password.as_ref().unwrap().expose_secret(), "P@ssw0rd!");
    assert_eq!(s.ssh_password.as_ref().unwrap().expose_secret(), "đèn-xanh");
    assert!(s.ssh_key_passphrase.is_none());

    let maria = by_name("Maria");
    assert_eq!(maria.kind, DbKind::MySql);
    assert_eq!(maria.port, 3306);
    assert!(maria.ssh.is_none());

    let pg = by_name("Warehouse PG");
    assert_eq!(pg.kind, DbKind::Postgres);
    assert_eq!(pg.port, 5433);
    assert_eq!(pg.secret_mode, SecretMode::Ask);
    assert!(!b.secrets.contains_key(&pg.id));
    let ssl = pg.ssl.as_ref().unwrap();
    assert_eq!(ssl.mode, SslMode::VerifyFull);
    assert!(ssl.ca_path.is_some() && ssl.cert_path.is_some() && ssl.key_path.is_some());

    let broken = by_name("Broken password");
    assert!(!b.secrets.contains_key(&broken.id));
    assert_eq!(
        b.warnings[&broken.id],
        vec!["Password could not be decrypted".to_string()]
    );
    assert_eq!(broken.secret_mode, SecretMode::Ask);
    assert!(b.warnings.len() == 1);
}

#[test]
fn malformed_xml_is_config_error() {
    for bad in [
        "<Connections><Connection ConnectionName=\"x\"",
        "not xml <<<",
        "",
    ] {
        assert!(
            matches!(parse_ncx(bad), Err(DbError::Config { .. })),
            "{bad:?}"
        );
    }
}
