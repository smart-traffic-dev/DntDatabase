use dnt_core::config::{ConnectionConfig, EnvTag, SecretMode, SslConfig, SslMode};
use dnt_core::error::DbError;
use dnt_core::secret::{MemorySecretStore, SecretKind, SecretStore};
use dnt_core::test_util::{sample_config, sample_ssh};
use dnt_store::db::{open, open_in_memory};
use dnt_store::repo::{ConnectionRepo, NewSecrets, NodeId, SecretChange, SecretUpdate};
use secrecy::{ExposeSecret, SecretString};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

fn setup() -> (ConnectionRepo, Arc<MemorySecretStore>) {
    let secrets = Arc::new(MemorySecretStore::default());
    let repo = ConnectionRepo::new(open_in_memory().unwrap(), secrets.clone());
    (repo, secrets)
}

fn cfg(name: &str) -> ConnectionConfig {
    ConnectionConfig {
        id: Uuid::nil(),
        name: name.into(),
        ..sample_config()
    }
}

fn pw(s: &str) -> Option<SecretString> {
    Some(SecretString::from(s))
}

fn no_secrets() -> NewSecrets {
    NewSecrets {
        db_password: None,
        ssh_password: None,
        ssh_key_passphrase: None,
    }
}

fn keep_all() -> SecretUpdate {
    SecretUpdate {
        db_password: SecretChange::Keep,
        ssh_password: SecretChange::Keep,
        ssh_key_passphrase: SecretChange::Keep,
    }
}

fn secret(store: &MemorySecretStore, id: Uuid, kind: SecretKind) -> Option<String> {
    store
        .get(id, kind)
        .unwrap()
        .map(|s| s.expose_secret().to_string())
}

#[test]
fn create_then_tree_lists_without_secrets() {
    let (repo, _secrets) = setup();
    let created = repo
        .create(
            cfg("shop-prod"),
            NewSecrets {
                db_password: pw("s3cr3t-pa55"),
                ..no_secrets()
            },
        )
        .unwrap();
    assert_ne!(created.id, Uuid::nil());

    let tree = repo.tree().unwrap();
    assert_eq!(tree.connections.len(), 1);
    let summary = &tree.connections[0];
    assert_eq!(summary.config, created);
    assert!(summary.has_db_password);
    assert!(!summary.has_ssh_secret);
    assert_eq!(summary.last_connected_at, None);

    let json = serde_json::to_string(&tree).unwrap();
    assert!(!json.contains("s3cr3t-pa55"));
}

#[test]
fn update_keep_set_clear_secrets() {
    let (repo, secrets) = setup();
    let c = repo
        .create(
            cfg("a"),
            NewSecrets {
                db_password: pw("a"),
                ..no_secrets()
            },
        )
        .unwrap();

    let mut renamed = c.clone();
    renamed.name = "a2".into();
    let updated = repo.update(renamed, keep_all()).unwrap();
    assert_eq!(updated.name, "a2");
    assert_eq!(repo.get(c.id).unwrap().name, "a2");
    assert_eq!(
        secret(&secrets, c.id, SecretKind::DbPassword).as_deref(),
        Some("a")
    );

    repo.update(
        c.clone(),
        SecretUpdate {
            db_password: SecretChange::Set(SecretString::from("b")),
            ..keep_all()
        },
    )
    .unwrap();
    assert_eq!(
        secret(&secrets, c.id, SecretKind::DbPassword).as_deref(),
        Some("b")
    );

    repo.update(
        c.clone(),
        SecretUpdate {
            db_password: SecretChange::Clear,
            ..keep_all()
        },
    )
    .unwrap();
    assert_eq!(secret(&secrets, c.id, SecretKind::DbPassword), None);
    assert!(!repo.tree().unwrap().connections[0].has_db_password);
}

#[test]
fn ask_mode_stores_no_secret() {
    let (repo, secrets) = setup();
    let mut ask = cfg("ask");
    ask.secret_mode = SecretMode::Ask;
    let c = repo
        .create(
            ask,
            NewSecrets {
                db_password: pw("x"),
                ssh_password: pw("y"),
                ssh_key_passphrase: pw("z"),
            },
        )
        .unwrap();
    for kind in SecretKind::ALL {
        assert_eq!(secret(&secrets, c.id, kind), None);
    }

    // Switching an existing saved connection to Ask removes its stored secrets.
    let saved = repo
        .create(
            cfg("saved"),
            NewSecrets {
                db_password: pw("p"),
                ..no_secrets()
            },
        )
        .unwrap();
    let mut to_ask = saved.clone();
    to_ask.secret_mode = SecretMode::Ask;
    repo.update(
        to_ask,
        SecretUpdate {
            ssh_password: SecretChange::Set(SecretString::from("q")),
            ..keep_all()
        },
    )
    .unwrap();
    for kind in SecretKind::ALL {
        assert_eq!(secret(&secrets, saved.id, kind), None);
    }
}

#[test]
fn duplicate_copies_config_and_secrets_with_new_id() {
    let (repo, secrets) = setup();
    let mut c = cfg("shop-prod");
    c.ssh = Some(sample_ssh());
    let orig = repo
        .create(
            c,
            NewSecrets {
                db_password: pw("db"),
                ssh_password: pw("ssh"),
                ssh_key_passphrase: None,
            },
        )
        .unwrap();

    let dup = repo.duplicate(orig.id).unwrap();
    assert_eq!(dup.name, "shop-prod copy");
    assert_ne!(dup.id, orig.id);
    assert_eq!(dup.ssh, orig.ssh);
    assert_eq!(dup.host, orig.host);
    assert_eq!(repo.get(dup.id).unwrap(), dup);
    for kind in SecretKind::ALL {
        assert_eq!(
            secret(&secrets, dup.id, kind),
            secret(&secrets, orig.id, kind)
        );
    }
    assert_eq!(
        secret(&secrets, dup.id, SecretKind::SshPassword).as_deref(),
        Some("ssh")
    );
    assert_eq!(repo.tree().unwrap().connections.len(), 2);
}

#[test]
fn delete_removes_secrets() {
    let (repo, secrets) = setup();
    let c = repo
        .create(
            cfg("gone"),
            NewSecrets {
                db_password: pw("a"),
                ssh_password: pw("b"),
                ssh_key_passphrase: pw("c"),
            },
        )
        .unwrap();
    repo.delete(c.id).unwrap();
    for kind in SecretKind::ALL {
        assert_eq!(secret(&secrets, c.id, kind), None);
    }
    assert!(repo.tree().unwrap().connections.is_empty());
    assert!(repo.get(c.id).is_err());
}

#[test]
fn delete_group_moves_connections_to_root_and_deletes_subgroups() {
    let (repo, _secrets) = setup();
    let g1 = repo.create_group("G1", None).unwrap();
    let g2 = repo.create_group("G2", Some(g1.id)).unwrap();
    assert_eq!(g2.parent_id, Some(g1.id));
    let mut a = cfg("A");
    a.group_id = Some(g1.id);
    let a = repo.create(a, no_secrets()).unwrap();
    let mut b = cfg("B");
    b.group_id = Some(g2.id);
    let b = repo.create(b, no_secrets()).unwrap();

    repo.delete_group(g1.id).unwrap();

    let tree = repo.tree().unwrap();
    assert!(tree.groups.is_empty());
    assert_eq!(repo.get(a.id).unwrap().group_id, None);
    assert_eq!(repo.get(b.id).unwrap().group_id, None);
}

#[test]
fn move_node_reorders() {
    let (repo, _secrets) = setup();
    let a = repo.create(cfg("A"), no_secrets()).unwrap();
    let b = repo.create(cfg("B"), no_secrets()).unwrap();
    let c = repo.create(cfg("C"), no_secrets()).unwrap();

    repo.move_node(NodeId::Connection(c.id), None, 0).unwrap();

    let tree = repo.tree().unwrap();
    let order: Vec<(String, i64)> = tree
        .connections
        .iter()
        .map(|s| (s.config.name.clone(), s.config.sort_order))
        .collect();
    assert_eq!(
        order,
        vec![("C".into(), 0), ("A".into(), 1), ("B".into(), 2)]
    );
    assert_eq!(repo.get(a.id).unwrap().sort_order, 1);
    assert_eq!(repo.get(b.id).unwrap().sort_order, 2);
}

#[test]
fn move_node_into_group_and_rejects_group_cycles() {
    let (repo, _secrets) = setup();
    let g1 = repo.create_group("G1", None).unwrap();
    let g2 = repo.create_group("G2", Some(g1.id)).unwrap();
    let a = repo.create(cfg("A"), no_secrets()).unwrap();

    // Index past the end appends.
    repo.move_node(NodeId::Connection(a.id), Some(g2.id), 99)
        .unwrap();
    let moved = repo.get(a.id).unwrap();
    assert_eq!((moved.group_id, moved.sort_order), (Some(g2.id), 0));

    let err = repo
        .move_node(NodeId::Group(g1.id), Some(g2.id), 0)
        .unwrap_err();
    assert!(
        matches!(err, DbError::Config { field: Some(ref f), .. } if f == "parent_id"),
        "{err:?}"
    );
    let err = repo
        .move_node(NodeId::Group(g1.id), Some(g1.id), 0)
        .unwrap_err();
    assert!(matches!(err, DbError::Config { .. }));

    repo.rename_group(g2.id, "Renamed").unwrap();
    repo.move_node(NodeId::Group(g2.id), None, 0).unwrap();
    let tree = repo.tree().unwrap();
    let names: Vec<(&str, Option<Uuid>, i64)> = tree
        .groups
        .iter()
        .map(|g| (g.name.as_str(), g.parent_id, g.sort_order))
        .collect();
    assert_eq!(names, vec![("Renamed", None, 0), ("G1", None, 1)]);
}

#[test]
fn invalid_config_rejected() {
    let (repo, _secrets) = setup();
    let mut bad = cfg("bad");
    bad.port = 0;
    let err = repo
        .create(
            bad,
            NewSecrets {
                db_password: pw("x"),
                ..no_secrets()
            },
        )
        .unwrap_err();
    assert!(
        matches!(err, DbError::Config { field: Some(ref f), .. } if f == "port"),
        "{err:?}"
    );
    assert!(repo.tree().unwrap().connections.is_empty());
}

#[test]
fn json_columns_roundtrip_ssh_ssl_options() {
    let (repo, _secrets) = setup();
    let mut c = cfg("json");
    c.env_tag = Some(EnvTag::Staging);
    c.read_only = true;
    c.ssh = Some(sample_ssh());
    c.ssl = Some(SslConfig {
        version: 1,
        mode: SslMode::VerifyFull,
        ca_path: Some(PathBuf::from("C:\\certs\\ca.pem")),
        cert_path: Some(PathBuf::from("client.pem")),
        key_path: None,
    });
    c.options.connect_timeout_s = 42;
    c.options.charset = Some("utf8mb4".into());
    c.options.application_name = Some("dnt-test".into());
    let created = repo.create(c, no_secrets()).unwrap();
    assert_eq!(repo.get(created.id).unwrap(), created);
}

#[test]
fn reopen_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let secrets = Arc::new(MemorySecretStore::default());
    let (created, group) = {
        let repo = ConnectionRepo::new(open(&path).unwrap(), secrets.clone());
        let g = repo.create_group("Prod", None).unwrap();
        let mut c = cfg("persisted");
        c.group_id = Some(g.id);
        let c = repo.create(c, no_secrets()).unwrap();
        repo.touch_connected(c.id).unwrap();
        (c, g)
    };

    let repo = ConnectionRepo::new(open(&path).unwrap(), secrets);
    assert_eq!(repo.get(created.id).unwrap(), created);
    let tree = repo.tree().unwrap();
    assert_eq!(tree.groups, vec![group]);
    let ts = tree.connections[0].last_connected_at.clone().unwrap();
    assert!(ts.ends_with('Z'), "{ts}");
}

#[test]
fn explicit_id_is_kept_and_node_id_serde_shape() {
    let (repo, _secrets) = setup();
    let id = Uuid::new_v4();
    let c = ConnectionConfig {
        id,
        ..cfg("imported")
    };
    assert_eq!(repo.create(c, no_secrets()).unwrap().id, id);
    let j = serde_json::to_value(NodeId::Connection(id)).unwrap();
    assert_eq!(j["type"], "connection");
    assert_eq!(j["id"], id.to_string());
}

/// Wraps a `MemorySecretStore`: reading a secret value always fails (the tree must only
/// check presence), and every access for the `broken` connection fails.
struct FlakyStore {
    inner: MemorySecretStore,
    broken: std::sync::Mutex<Option<Uuid>>,
}

impl FlakyStore {
    fn check(&self, conn: Uuid) -> Result<(), DbError> {
        if *self.broken.lock().unwrap() == Some(conn) {
            return Err(DbError::Secret {
                message: "read db_password failed: credential manager unavailable".into(),
            });
        }
        Ok(())
    }
}

impl SecretStore for FlakyStore {
    fn get(&self, _conn: Uuid, _kind: SecretKind) -> Result<Option<SecretString>, DbError> {
        Err(DbError::Secret {
            message: "the tree must not read secret values".into(),
        })
    }
    fn has(&self, conn: Uuid, kind: SecretKind) -> Result<bool, DbError> {
        self.check(conn)?;
        self.inner.has(conn, kind)
    }
    fn set(&self, conn: Uuid, kind: SecretKind, value: &SecretString) -> Result<(), DbError> {
        self.inner.set(conn, kind, value)
    }
    fn delete(&self, conn: Uuid, kind: SecretKind) -> Result<(), DbError> {
        self.inner.delete(conn, kind)
    }
    fn delete_all(&self, conn: Uuid) -> Result<(), DbError> {
        self.inner.delete_all(conn)
    }
}

#[test]
fn tree_survives_a_failing_keychain_entry_and_never_reads_values() {
    let store = Arc::new(FlakyStore {
        inner: MemorySecretStore::default(),
        broken: std::sync::Mutex::new(None),
    });
    let repo = ConnectionRepo::new(open_in_memory().unwrap(), store.clone());
    let with_pw = || NewSecrets {
        db_password: pw("pw"),
        ..no_secrets()
    };
    let a = repo.create(cfg("a"), with_pw()).unwrap().id;
    let b = repo.create(cfg("b"), with_pw()).unwrap().id;
    *store.broken.lock().unwrap() = Some(b);

    let tree = repo.tree().unwrap();
    let flag = |id: Uuid| {
        tree.connections
            .iter()
            .find(|c| c.config.id == id)
            .unwrap()
            .has_db_password
    };
    assert!(flag(a));
    // The unreadable entry is reported as absent instead of failing the whole tree.
    assert!(!flag(b));
}
