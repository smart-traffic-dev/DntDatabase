//! Seeds the **dev build's** connection list with the Docker test matrix
//! (`testdata/docker-compose.test.yml`), so a manual test starts with every server one click away.
//!
//! ```text
//! cargo run -p dnt-testkit --bin seed_dev            # add what is missing
//! cargo run -p dnt-testkit --bin seed_dev -- --reset # drop the seeded groups, then add again
//! ```
//!
//! Writes to `$DNT_DATA_DIR` when set, otherwise `%APPDATA%/DntDatabase-dev` (the dev build's
//! own directory; the installed app's `DntDatabase` is never touched). Passwords go to the
//! keychain service the app reads (`$DNT_KEYRING_SERVICE`, default `DntDatabase`). Idempotent:
//! a connection whose name already exists is skipped. Restart the dev app (or reload its
//! window) to see the new entries.

use dnt_core::config::{
    ConnOptions, ConnectionConfig, EnvTag, SecretMode, SshAuth, SshConfig, SslConfig, SslMode,
};
use dnt_core::server::SqlKind;
use dnt_store::keyring_store::KeyringSecretStore;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use secrecy::SecretString;
use std::collections::HashSet;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

/// The dev build's data directory name (`src-tauri/src/state.rs`, `APP_DIR` in debug builds).
const DEV_APP_DIR: &str = "DntDatabase-dev";
const DATA_DIR_ENV: &str = "DNT_DATA_DIR";

/// Root password of every server in the matrix, and of the `dnt_limited` user.
const PASSWORD: &str = "dnt_test";
/// Passphrase of `testdata/ssh/id_ed25519_enc` (`gen_fixtures`).
const KEY_PASSPHRASE: &str = "dnt_pass";

const GROUP_MYSQL: &str = "Docker · MySQL / MariaDB";
const GROUP_PG: &str = "Docker · PostgreSQL";
const GROUP_SCENARIOS: &str = "Docker · Scenarios";

struct Seed {
    group: &'static str,
    cfg: ConnectionConfig,
    password: Option<&'static str>,
    ssh_password: Option<&'static str>,
    key_passphrase: Option<&'static str>,
}

fn base(name: &str, kind: SqlKind, port: u16) -> ConnectionConfig {
    ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: name.into(),
        sort_order: 0,
        kind: kind.into(),
        host: "127.0.0.1".into(),
        port,
        username: match kind {
            SqlKind::MySql => "root",
            SqlKind::Postgres => "postgres",
        }
        .into(),
        default_db: Some("dnt_fixture".into()),
        env_tag: Some(EnvTag::Local),
        read_only: false,
        secret_mode: SecretMode::Saved,
        ssh: None,
        ssl: None,
        options: ConnOptions::default(),
    }
}

fn seed(group: &'static str, cfg: ConnectionConfig) -> Seed {
    Seed {
        group,
        cfg,
        password: Some(PASSWORD),
        ssh_password: None,
        key_passphrase: None,
    }
}

/// A tunnel through the matrix's SSH server (port 2222) to a server on the Docker network.
fn via_ssh(name: &str, target: &str, auth: SshAuth, key: Option<&str>) -> ConnectionConfig {
    ConnectionConfig {
        host: target.into(),
        port: 3306,
        ssh: Some(SshConfig {
            version: 1,
            host: "127.0.0.1".into(),
            port: 2222,
            user: "dnt".into(),
            auth,
            key_path: key.map(|k| dnt_testkit::testdata("ssh").join(k)),
        }),
        ..base(name, SqlKind::MySql, 3306)
    }
}

fn seeds() -> Vec<Seed> {
    let certs = dnt_testkit::testdata("certs");
    vec![
        seed(GROUP_MYSQL, base("MySQL 5.7", SqlKind::MySql, 33057)),
        seed(GROUP_MYSQL, base("MySQL 8.0", SqlKind::MySql, 33080)),
        seed(
            GROUP_MYSQL,
            base("MySQL 8.4 (explain profile)", SqlKind::MySql, 33084),
        ),
        seed(GROUP_MYSQL, base("MariaDB 10.6", SqlKind::MySql, 33106)),
        seed(GROUP_MYSQL, base("MariaDB 11.4", SqlKind::MySql, 33114)),
        seed(GROUP_PG, base("PostgreSQL 12", SqlKind::Postgres, 54312)),
        seed(GROUP_PG, base("PostgreSQL 17", SqlKind::Postgres, 54317)),
        // Behaviour worth clicking through by hand.
        seed(
            GROUP_SCENARIOS,
            ConnectionConfig {
                env_tag: Some(EnvTag::Production),
                ..base("shop-prod (MySQL 8 · PROD)", SqlKind::MySql, 33080)
            },
        ),
        seed(
            GROUP_SCENARIOS,
            ConnectionConfig {
                env_tag: Some(EnvTag::Staging),
                read_only: true,
                ..base("reporting-ro (MySQL 8 · read-only)", SqlKind::MySql, 33080)
            },
        ),
        seed(
            GROUP_SCENARIOS,
            ConnectionConfig {
                username: "dnt_limited".into(),
                ..base("limited user (PG 17)", SqlKind::Postgres, 54317)
            },
        ),
        seed(
            GROUP_SCENARIOS,
            ConnectionConfig {
                ssl: Some(SslConfig {
                    version: 1,
                    mode: SslMode::VerifyCa,
                    ca_path: Some(certs.join("ca.pem")),
                    cert_path: None,
                    key_path: None,
                }),
                ..base("PG 17 · SSL verify CA", SqlKind::Postgres, 54317)
            },
        ),
        Seed {
            ssh_password: Some(PASSWORD),
            ..seed(
                GROUP_SCENARIOS,
                via_ssh(
                    "MySQL 8 via SSH (password)",
                    "mysql8",
                    SshAuth::Password,
                    None,
                ),
            )
        },
        seed(
            GROUP_SCENARIOS,
            via_ssh(
                "MySQL 8 via SSH (key)",
                "mysql8",
                SshAuth::Key,
                Some("id_ed25519"),
            ),
        ),
        Seed {
            key_passphrase: Some(KEY_PASSPHRASE),
            ..seed(
                GROUP_SCENARIOS,
                via_ssh(
                    "MySQL 8 via SSH (key + passphrase)",
                    "mysql8",
                    SshAuth::Key,
                    Some("id_ed25519_enc"),
                ),
            )
        },
        Seed {
            password: None,
            ..seed(
                GROUP_SCENARIOS,
                ConnectionConfig {
                    secret_mode: SecretMode::Ask,
                    ..base("asks for password (PG 17)", SqlKind::Postgres, 54317)
                },
            )
        },
        Seed {
            password: Some("x"),
            ..seed(
                GROUP_SCENARIOS,
                ConnectionConfig {
                    env_tag: None,
                    ..base("dead server (port 33999)", SqlKind::MySql, 33999)
                },
            )
        },
    ]
}

fn data_dir() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::data_dir()
        .ok_or("cannot locate the user data directory")?
        .join(DEV_APP_DIR))
}

fn secret(s: Option<&str>) -> Option<SecretString> {
    s.map(|v| SecretString::from(v.to_owned()))
}

fn main() -> Result<(), Box<dyn Error>> {
    let reset = std::env::args().any(|a| a == "--reset");
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir)?;
    let db_file = dir.join("app.db");
    let repo = ConnectionRepo::new(
        dnt_store::db::open(&db_file)?,
        Arc::new(KeyringSecretStore::from_env()),
    );
    let seeds = seeds();
    let ours: HashSet<&str> = [GROUP_MYSQL, GROUP_PG, GROUP_SCENARIOS].into();

    if reset {
        let tree = repo.tree()?;
        let groups: HashSet<Uuid> = tree
            .groups
            .iter()
            .filter(|g| g.parent_id.is_none() && ours.contains(g.name.as_str()))
            .map(|g| g.id)
            .collect();
        for c in tree
            .connections
            .iter()
            .filter(|c| c.config.group_id.is_some_and(|g| groups.contains(&g)))
        {
            repo.delete(c.config.id)?;
        }
        for g in &groups {
            repo.delete_group(*g)?;
        }
        println!("reset: removed {} seeded group(s)", groups.len());
    }

    let tree = repo.tree()?;
    let existing: HashSet<String> = tree
        .connections
        .iter()
        .map(|c| c.config.name.clone())
        .collect();
    let mut group_ids = std::collections::HashMap::new();
    for g in tree.groups.iter().filter(|g| g.parent_id.is_none()) {
        group_ids.insert(g.name.clone(), g.id);
    }

    let (mut added, mut skipped) = (0, 0);
    for s in seeds {
        if existing.contains(&s.cfg.name) {
            skipped += 1;
            continue;
        }
        let group = match group_ids.get(s.group) {
            Some(id) => *id,
            None => {
                let id = repo.create_group(s.group, None)?.id;
                group_ids.insert(s.group.to_owned(), id);
                id
            }
        };
        let name = s.cfg.name.clone();
        repo.create(
            ConnectionConfig {
                group_id: Some(group),
                ..s.cfg
            },
            NewSecrets {
                db_password: secret(s.password),
                ssh_password: secret(s.ssh_password),
                ssh_key_passphrase: secret(s.key_passphrase),
            },
        )?;
        println!("  + {name}");
        added += 1;
    }
    println!(
        "{added} added, {skipped} already there · {}",
        db_file.display()
    );
    println!("Servers: docker compose -f testdata/docker-compose.test.yml up -d --wait");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::server::DbKind;

    #[test]
    fn base_logs_in_as_each_engines_superuser() {
        let my = base("m", SqlKind::MySql, 3306);
        assert_eq!((my.kind, my.username.as_str()), (DbKind::MySql, "root"));
        let pg = base("p", SqlKind::Postgres, 5432);
        assert_eq!(
            (pg.kind, pg.username.as_str()),
            (DbKind::Postgres, "postgres")
        );
    }
}
