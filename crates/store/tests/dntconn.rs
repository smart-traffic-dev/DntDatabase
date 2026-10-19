use dnt_core::config::{ConnectionConfig, SecretMode, SshAuth, SshConfig};
use dnt_core::error::DbError;
use dnt_core::secret::{MemorySecretStore, SecretKind, SecretStore};
use dnt_core::test_util::{sample_config, sample_ssh};
use dnt_store::db::open_in_memory;
use dnt_store::dntconn::{dntconn_has_secrets, export_dntconn, parse_dntconn};
use dnt_store::repo::{ConnectionRepo, Group, NewSecrets, NodeId};
use dnt_store::transfer::{
    apply, preview, ConflictKind, ConflictPolicy, ImportBundle, ImportResult, ImportSource,
    SkippedItem,
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

const PASSPHRASE: &str = "correct horse";

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

fn pass(s: &str) -> SecretString {
    SecretString::from(s)
}

fn no_secrets() -> NewSecrets {
    NewSecrets {
        db_password: None,
        ssh_password: None,
        ssh_key_passphrase: None,
    }
}

fn db_pw(s: &str) -> NewSecrets {
    NewSecrets {
        db_password: Some(SecretString::from(s)),
        ..no_secrets()
    }
}

fn secret(store: &MemorySecretStore, id: Uuid, kind: SecretKind) -> Option<String> {
    store
        .get(id, kind)
        .unwrap()
        .map(|s| s.expose_secret().to_string())
}

fn exposed(s: &Option<SecretString>) -> Option<&str> {
    s.as_ref().map(|s| s.expose_secret())
}

fn empty_bundle() -> ImportBundle {
    ImportBundle {
        source: ImportSource::Dntconn,
        groups: vec![],
        connections: vec![],
        secrets: HashMap::new(),
        skipped: vec![],
        warnings: HashMap::new(),
    }
}

/// A repo holding one connection "shop-prod" (host old-host, password old-pw).
fn repo_with_shop_prod() -> (ConnectionRepo, Arc<MemorySecretStore>, ConnectionConfig) {
    let (repo, secrets) = setup();
    let mut c = cfg("shop-prod");
    c.host = "old-host".into();
    let c = repo.create(c, db_pw("old-pw")).unwrap();
    (repo, secrets, c)
}

/// Bundle with `existing.id` changed to host new-host / password new-pw, plus a new connection
/// with a different id but the same name.
fn conflicting_bundle(existing: &ConnectionConfig, other_id: Uuid) -> ImportBundle {
    let mut same_id = existing.clone();
    same_id.host = "new-host".into();
    let same_name = ConnectionConfig {
        id: other_id,
        host: "other-host".into(),
        ..existing.clone()
    };
    let mut bundle = empty_bundle();
    bundle.secrets.insert(existing.id, db_pw("new-pw"));
    bundle.connections = vec![same_id, same_name];
    bundle
}

fn names_and_hosts(repo: &ConnectionRepo) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = repo
        .tree()
        .unwrap()
        .connections
        .into_iter()
        .map(|s| (s.config.name, s.config.host))
        .collect();
    v.sort();
    v
}

#[test]
fn export_without_secrets_has_null_secrets() {
    let (repo, _secrets) = setup();
    let c = repo.create(cfg("a"), db_pw("pw-a-123")).unwrap();
    let json = export_dntconn(&repo, &[NodeId::Connection(c.id)], None).unwrap();
    let v: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["format"], "dntdatabase-connections");
    assert_eq!(v["version"], 1);
    assert!(v["secrets"].is_null());
    assert!(v["exported_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(v["connections"].as_array().unwrap().len(), 1);
    assert_eq!(v["connections"][0]["id"], c.id.to_string());
    assert!(!dntconn_has_secrets(&json).unwrap());
}

#[test]
fn roundtrip_with_passphrase() {
    let (repo, _secrets) = setup();
    let mut c = cfg("shop-prod");
    c.ssh = Some(sample_ssh());
    let c = repo
        .create(
            c,
            NewSecrets {
                db_password: Some(pass("db-secret-1")),
                ssh_password: Some(pass("ssh-secret-2")),
                ssh_key_passphrase: None,
            },
        )
        .unwrap();
    let plain = repo.create(cfg("no-secret"), no_secrets()).unwrap();

    let json = export_dntconn(
        &repo,
        &[NodeId::Connection(c.id), NodeId::Connection(plain.id)],
        Some(&pass(PASSPHRASE)),
    )
    .unwrap();
    assert!(dntconn_has_secrets(&json).unwrap());
    let v: Value = serde_json::from_str(&json).unwrap();
    let s = &v["secrets"];
    assert_eq!(s["kdf"], "argon2id");
    assert_eq!(
        (s["m"].clone(), s["t"].clone(), s["p"].clone()),
        (65536.into(), 3.into(), 1.into())
    );
    assert_eq!(s["cipher"], "xchacha20poly1305");
    for field in ["salt", "nonce", "ciphertext"] {
        assert!(!s[field].as_str().unwrap().is_empty(), "{field}");
    }

    let bundle = parse_dntconn(&json, Some(&pass(PASSPHRASE))).unwrap();
    assert_eq!(bundle.source, ImportSource::Dntconn);
    assert_eq!(bundle.connections, vec![c.clone(), plain.clone()]);
    assert!(bundle.skipped.is_empty());
    let got = &bundle.secrets[&c.id];
    assert_eq!(exposed(&got.db_password), Some("db-secret-1"));
    assert_eq!(exposed(&got.ssh_password), Some("ssh-secret-2"));
    assert_eq!(exposed(&got.ssh_key_passphrase), None);
    assert!(!bundle.secrets.contains_key(&plain.id));
}

#[test]
fn wrong_passphrase_is_config_error() {
    let (repo, _secrets) = setup();
    let c = repo.create(cfg("a"), db_pw("pw-a-123")).unwrap();
    let json = export_dntconn(&repo, &[NodeId::Connection(c.id)], Some(&pass(PASSPHRASE))).unwrap();
    let err = parse_dntconn(&json, Some(&pass("wrong horse battery"))).unwrap_err();
    assert!(
        matches!(
            err,
            DbError::Config { field: Some(ref f), ref message }
                if f == "passphrase" && message == "Passphrase is incorrect."
        ),
        "{err:?}"
    );
}

#[test]
fn short_passphrase_rejected() {
    let (repo, _secrets) = setup();
    let c = repo.create(cfg("a"), db_pw("pw-a-123")).unwrap();
    let err = export_dntconn(&repo, &[NodeId::Connection(c.id)], Some(&pass("short"))).unwrap_err();
    assert!(
        matches!(err, DbError::Config { field: Some(ref f), .. } if f == "passphrase"),
        "{err:?}"
    );
}

#[test]
fn parse_without_passphrase_drops_secrets() {
    let (repo, _secrets) = setup();
    let c = repo.create(cfg("a"), db_pw("pw-a-123")).unwrap();
    let json = export_dntconn(&repo, &[NodeId::Connection(c.id)], Some(&pass(PASSPHRASE))).unwrap();
    let bundle = parse_dntconn(&json, None).unwrap();
    assert_eq!(bundle.connections, vec![c]);
    assert!(bundle.secrets.is_empty());
}

#[test]
fn corrupt_file_is_error_not_panic() {
    let (repo, _secrets) = setup();
    let c = repo.create(cfg("a"), db_pw("pw-a-123")).unwrap();
    let json = export_dntconn(&repo, &[NodeId::Connection(c.id)], Some(&pass(PASSPHRASE))).unwrap();
    let v: Value = serde_json::from_str(&json).unwrap();
    let is_config = |r: Result<ImportBundle, DbError>| matches!(r, Err(DbError::Config { .. }));

    // One ciphertext byte changed (first base64 char swapped).
    let mut tampered = v.clone();
    let ct = tampered["secrets"]["ciphertext"]
        .as_str()
        .unwrap()
        .to_string();
    let first = if ct.starts_with('A') { "B" } else { "A" };
    tampered["secrets"]["ciphertext"] = Value::from(format!("{first}{}", &ct[1..]));
    assert!(is_config(parse_dntconn(
        &tampered.to_string(),
        Some(&pass(PASSPHRASE))
    )));

    // Truncated JSON.
    assert!(is_config(parse_dntconn(&json[..json.len() / 2], None)));
    assert!(matches!(
        dntconn_has_secrets(&json[..json.len() / 2]),
        Err(DbError::Config { .. })
    ));

    // Invalid base64 nonce.
    let mut bad_nonce = v.clone();
    bad_nonce["secrets"]["nonce"] = Value::from("!!!not base64!!!");
    assert!(is_config(parse_dntconn(
        &bad_nonce.to_string(),
        Some(&pass(PASSPHRASE))
    )));

    // Unknown cipher.
    let mut bad_cipher = v.clone();
    bad_cipher["secrets"]["cipher"] = Value::from("rot13");
    assert!(is_config(parse_dntconn(
        &bad_cipher.to_string(),
        Some(&pass(PASSPHRASE))
    )));

    // Wrong format marker.
    let mut wrong_format = v.clone();
    wrong_format["format"] = Value::from("something-else");
    assert!(is_config(parse_dntconn(&wrong_format.to_string(), None)));

    // Future version.
    let mut v2 = v.clone();
    v2["version"] = Value::from(2);
    let err = parse_dntconn(&v2.to_string(), None).unwrap_err();
    assert!(
        matches!(err, DbError::Unsupported { ref feature, ref version }
            if feature == "dntconn version" && version == "2"),
        "{err:?}"
    );
}

#[test]
fn invalid_connection_in_file_is_skipped() {
    let (repo, _secrets) = setup();
    let c = repo.create(cfg("broken"), db_pw("pw-a-123")).unwrap();
    let json = export_dntconn(&repo, &[NodeId::Connection(c.id)], Some(&pass(PASSPHRASE))).unwrap();
    let mut v: Value = serde_json::from_str(&json).unwrap();
    v["connections"][0]["port"] = Value::from(0);
    let bundle = parse_dntconn(&v.to_string(), Some(&pass(PASSPHRASE))).unwrap();
    assert!(bundle.connections.is_empty());
    assert!(bundle.secrets.is_empty());
    assert_eq!(
        bundle.skipped,
        vec![SkippedItem {
            name: "broken".into(),
            reason: "port must be between 1 and 65535".into(),
        }]
    );
}

#[test]
fn nil_and_duplicate_connection_ids_are_skipped() {
    let (repo, _secrets) = setup();
    let c = repo.create(cfg("first"), db_pw("pw-a-123")).unwrap();
    let json = export_dntconn(&repo, &[NodeId::Connection(c.id)], Some(&pass(PASSPHRASE))).unwrap();
    let mut v: Value = serde_json::from_str(&json).unwrap();
    let mut dup = v["connections"][0].clone();
    dup["name"] = Value::from("second");
    let mut nil = v["connections"][0].clone();
    nil["name"] = Value::from("nil-id");
    nil["id"] = Value::from(Uuid::nil().to_string());
    v["connections"].as_array_mut().unwrap().extend([dup, nil]);

    let bundle = parse_dntconn(&v.to_string(), Some(&pass(PASSPHRASE))).unwrap();
    assert_eq!(bundle.connections, vec![c.clone()]);
    assert_eq!(
        exposed(&bundle.secrets[&c.id].db_password),
        Some("pw-a-123")
    );
    assert_eq!(
        bundle.skipped,
        vec![
            SkippedItem {
                name: "second".into(),
                reason: "Duplicate connection id".into(),
            },
            SkippedItem {
                name: "nil-id".into(),
                reason: "Invalid connection id".into(),
            },
        ]
    );
}

#[test]
fn over_cap_kdf_cost_is_config_error() {
    let (repo, _secrets) = setup();
    let c = repo.create(cfg("a"), db_pw("pw-a-123")).unwrap();
    let json = export_dntconn(&repo, &[NodeId::Connection(c.id)], Some(&pass(PASSPHRASE))).unwrap();
    let mut v: Value = serde_json::from_str(&json).unwrap();
    v["secrets"]["m"] = Value::from((1u64 << 20) + 1);
    let err = parse_dntconn(&v.to_string(), Some(&pass(PASSPHRASE))).unwrap_err();
    assert!(matches!(err, DbError::Config { .. }), "{err:?}");
}

#[test]
fn overwrite_with_ask_config_clears_stored_secrets() {
    let (repo, secrets, existing) = repo_with_shop_prod();
    let mut ask = existing.clone();
    ask.secret_mode = SecretMode::Ask;
    let mut bundle = empty_bundle();
    bundle.connections = vec![ask];
    apply(&repo, bundle, ConflictPolicy::Overwrite).unwrap();
    for kind in SecretKind::ALL {
        assert_eq!(secret(&secrets, existing.id, kind), None);
    }
    assert!(!repo.tree().unwrap().connections[0].has_db_password);
}

#[test]
fn overwrite_with_saved_config_without_secrets_keeps_stored_password() {
    let (repo, secrets, existing) = repo_with_shop_prod();
    let mut changed = existing.clone();
    changed.host = "new-host".into();
    let mut bundle = empty_bundle();
    bundle.connections = vec![changed];
    apply(&repo, bundle, ConflictPolicy::Overwrite).unwrap();
    assert_eq!(repo.get(existing.id).unwrap().host, "new-host");
    assert_eq!(
        secret(&secrets, existing.id, SecretKind::DbPassword).as_deref(),
        Some("old-pw")
    );
}

#[test]
fn exporting_group_includes_ancestors() {
    let (repo, _secrets) = setup();
    let g1 = repo.create_group("G1", None).unwrap();
    let g2 = repo.create_group("G2", Some(g1.id)).unwrap();
    let g3 = repo.create_group("G3", Some(g2.id)).unwrap();
    let other = repo.create_group("Other", None).unwrap();
    let in_group = |name: &str, g: Option<Uuid>| ConnectionConfig {
        group_id: g,
        ..cfg(name)
    };
    let a = repo
        .create(in_group("A", Some(g2.id)), no_secrets())
        .unwrap();
    let b = repo
        .create(in_group("B", Some(g3.id)), no_secrets())
        .unwrap();
    let c = repo
        .create(in_group("C", Some(g1.id)), no_secrets())
        .unwrap();
    repo.create(in_group("D", None), no_secrets()).unwrap();
    repo.create(in_group("E", Some(other.id)), no_secrets())
        .unwrap();

    let json = export_dntconn(&repo, &[NodeId::Group(g2.id)], None).unwrap();
    let bundle = parse_dntconn(&json, None).unwrap();
    let group_ids: HashSet<Uuid> = bundle.groups.iter().map(|g| g.id).collect();
    assert_eq!(group_ids, HashSet::from([g1.id, g2.id, g3.id]));
    let conn_ids: HashSet<Uuid> = bundle.connections.iter().map(|c| c.id).collect();
    assert_eq!(conn_ids, HashSet::from([a.id, b.id]));

    // A single connection brings its group's ancestor chain.
    let json = export_dntconn(&repo, &[NodeId::Connection(c.id)], None).unwrap();
    let bundle = parse_dntconn(&json, None).unwrap();
    assert_eq!(bundle.groups, vec![g1.clone()]);
    assert_eq!(bundle.connections, vec![c]);

    // Unknown nodes are rejected.
    assert!(export_dntconn(&repo, &[NodeId::Group(Uuid::new_v4())], None).is_err());
    assert!(export_dntconn(&repo, &[NodeId::Connection(Uuid::new_v4())], None).is_err());
}

#[test]
fn preview_flags_conflicts_and_missing_key() {
    let (repo, _secrets, existing) = repo_with_shop_prod();
    let imported_group = Group {
        id: Uuid::new_v4(),
        parent_id: None,
        name: "Imported".into(),
        sort_order: 0,
    };
    let missing_key = PathBuf::from("Z:\\definitely\\missing\\id_ed25519");
    let fresh = ConnectionConfig {
        id: Uuid::new_v4(),
        group_id: Some(imported_group.id),
        name: "fresh".into(),
        ssh: Some(SshConfig {
            auth: SshAuth::Key,
            key_path: Some(missing_key.clone()),
            ..sample_ssh()
        }),
        ..sample_config()
    };
    let other_id = Uuid::new_v4();
    let mut bundle = conflicting_bundle(&existing, other_id);
    bundle.connections.push(fresh.clone());
    bundle.groups.push(imported_group);
    bundle.secrets.insert(fresh.id, db_pw("fresh-pw"));
    bundle.warnings.insert(
        fresh.id,
        vec!["Password could not be decrypted".to_string()],
    );
    bundle.skipped.push(SkippedItem {
        name: "redis-cache".into(),
        reason: "Unsupported connection type".into(),
    });

    let p = preview(&repo, &bundle).unwrap();
    assert_eq!(p.skipped, bundle.skipped);
    assert_eq!(p.rows.len(), 3);

    let same_id = &p.rows[0];
    assert_eq!(same_id.id, existing.id);
    assert_eq!(same_id.conflict, Some(ConflictKind::SameId));
    assert!(same_id.has_password);
    assert_eq!(same_id.host, "new-host");
    assert_eq!(same_id.group_path, "");

    let same_name = &p.rows[1];
    assert_eq!(same_name.id, other_id);
    assert_eq!(same_name.conflict, Some(ConflictKind::SameName));
    assert!(!same_name.has_password);
    assert!(same_name.warnings.is_empty());

    let row = &p.rows[2];
    assert_eq!(row.name, "fresh");
    assert_eq!(row.kind, fresh.kind);
    assert_eq!(row.conflict, None);
    assert!(row.has_password);
    assert_eq!(row.group_path, "Imported");
    assert_eq!(
        row.warnings,
        vec![
            "Password could not be decrypted".to_string(),
            format!("SSH key not found: {}", missing_key.display()),
        ]
    );

    // Preview writes nothing.
    assert_eq!(repo.tree().unwrap().connections.len(), 1);
    assert!(repo.tree().unwrap().groups.is_empty());
}

#[test]
fn apply_policies() {
    let other_id = Uuid::new_v4();

    // Skip keeps the existing connection; the same-name newcomer gets a suffix.
    let (repo, secrets, existing) = repo_with_shop_prod();
    let r = apply(
        &repo,
        conflicting_bundle(&existing, other_id),
        ConflictPolicy::Skip,
    )
    .unwrap();
    assert_eq!(
        r,
        ImportResult {
            imported: 1,
            overwritten: 0,
            skipped: 1
        }
    );
    assert_eq!(repo.get(existing.id).unwrap(), existing);
    assert_eq!(
        secret(&secrets, existing.id, SecretKind::DbPassword).as_deref(),
        Some("old-pw")
    );
    assert_eq!(repo.get(other_id).unwrap().name, "shop-prod (2)");

    // Overwrite replaces config and secret in place.
    let (repo, secrets, existing) = repo_with_shop_prod();
    let r = apply(
        &repo,
        conflicting_bundle(&existing, other_id),
        ConflictPolicy::Overwrite,
    )
    .unwrap();
    assert_eq!(
        r,
        ImportResult {
            imported: 1,
            overwritten: 1,
            skipped: 0
        }
    );
    let over = repo.get(existing.id).unwrap();
    assert_eq!(
        (over.name.as_str(), over.host.as_str()),
        ("shop-prod", "new-host")
    );
    assert_eq!(
        secret(&secrets, existing.id, SecretKind::DbPassword).as_deref(),
        Some("new-pw")
    );
    assert_eq!(
        names_and_hosts(&repo),
        vec![
            ("shop-prod".into(), "new-host".into()),
            ("shop-prod (2)".into(), "other-host".into()),
        ]
    );

    // Copy keeps the original and imports under a new id, secrets following the new id.
    let (repo, secrets, existing) = repo_with_shop_prod();
    let r = apply(
        &repo,
        conflicting_bundle(&existing, other_id),
        ConflictPolicy::Copy,
    )
    .unwrap();
    assert_eq!(
        r,
        ImportResult {
            imported: 2,
            overwritten: 0,
            skipped: 0
        }
    );
    assert_eq!(repo.get(existing.id).unwrap(), existing);
    assert_eq!(
        secret(&secrets, existing.id, SecretKind::DbPassword).as_deref(),
        Some("old-pw")
    );
    let tree = repo.tree().unwrap();
    let copy = tree
        .connections
        .iter()
        .find(|s| s.config.host == "new-host")
        .unwrap();
    assert!(copy.config.id != existing.id && copy.config.id != other_id);
    assert_eq!(copy.config.name, "shop-prod (2)");
    assert_eq!(
        secret(&secrets, copy.config.id, SecretKind::DbPassword).as_deref(),
        Some("new-pw")
    );
    assert_eq!(repo.get(other_id).unwrap().name, "shop-prod (3)");
}

#[test]
fn apply_creates_groups_once_and_counts_parse_skips() {
    let (repo, _secrets) = setup();
    let parent = Group {
        id: Uuid::new_v4(),
        parent_id: None,
        name: "Parent".into(),
        sort_order: 0,
    };
    let child = Group {
        id: Uuid::new_v4(),
        parent_id: Some(parent.id),
        name: "Child".into(),
        sort_order: 0,
    };
    let conn = ConnectionConfig {
        id: Uuid::new_v4(),
        group_id: Some(child.id),
        name: "in-child".into(),
        ..sample_config()
    };
    let bundle = || {
        let mut b = empty_bundle();
        // Child listed before its parent: apply must order them.
        b.groups = vec![child.clone(), parent.clone()];
        b.connections = vec![conn.clone()];
        b.skipped = vec![SkippedItem {
            name: "x".into(),
            reason: "y".into(),
        }];
        b
    };
    let r = apply(&repo, bundle(), ConflictPolicy::Skip).unwrap();
    assert_eq!(
        r,
        ImportResult {
            imported: 1,
            overwritten: 0,
            skipped: 1
        }
    );
    let tree = repo.tree().unwrap();
    let mut groups: Vec<(Uuid, Option<Uuid>, String)> = tree
        .groups
        .iter()
        .map(|g| (g.id, g.parent_id, g.name.clone()))
        .collect();
    groups.sort();
    let mut expected = vec![
        (parent.id, None, "Parent".to_string()),
        (child.id, Some(parent.id), "Child".to_string()),
    ];
    expected.sort();
    assert_eq!(groups, expected);
    assert_eq!(repo.get(conn.id).unwrap().group_id, Some(child.id));

    // Re-importing reuses the existing groups instead of duplicating them.
    let r = apply(&repo, bundle(), ConflictPolicy::Skip).unwrap();
    assert_eq!(r.skipped, 2);
    assert_eq!(repo.tree().unwrap().groups.len(), 2);
}

#[test]
fn export_parse_apply_into_new_repo() {
    let (src, _src_secrets) = setup();
    let g = src.create_group("Prod", None).unwrap();
    let c = src
        .create(
            ConnectionConfig {
                group_id: Some(g.id),
                ..cfg("shop-prod")
            },
            db_pw("db-secret-1"),
        )
        .unwrap();
    let json = export_dntconn(&src, &[NodeId::Group(g.id)], Some(&pass(PASSPHRASE))).unwrap();

    let (dst, dst_secrets) = setup();
    let bundle = parse_dntconn(&json, Some(&pass(PASSPHRASE))).unwrap();
    apply(&dst, bundle, ConflictPolicy::Skip).unwrap();
    assert_eq!(dst.get(c.id).unwrap(), c);
    assert_eq!(dst.tree().unwrap().groups, vec![g]);
    assert_eq!(
        secret(&dst_secrets, c.id, SecretKind::DbPassword).as_deref(),
        Some("db-secret-1")
    );
}

#[test]
fn exported_file_never_contains_plaintext_secret() {
    let (repo, _secrets) = setup();
    let mut c = cfg("a");
    c.ssh = Some(sample_ssh());
    let c = repo
        .create(
            c,
            NewSecrets {
                db_password: Some(pass("db-PLAINTEXT-111")),
                ssh_password: Some(pass("ssh-PLAINTEXT-222")),
                ssh_key_passphrase: Some(pass("key-PLAINTEXT-333")),
            },
        )
        .unwrap();
    for passphrase in [None, Some(pass(PASSPHRASE))] {
        let json = export_dntconn(&repo, &[NodeId::Connection(c.id)], passphrase.as_ref()).unwrap();
        for s in [
            "db-PLAINTEXT-111",
            "ssh-PLAINTEXT-222",
            "key-PLAINTEXT-333",
            PASSPHRASE,
        ] {
            assert!(!json.contains(s), "{s} leaked");
        }
    }
}
