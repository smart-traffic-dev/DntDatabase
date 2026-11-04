//! Per-connection AI consent (`ai_opt_in`).

use dnt_core::config::ConnectionConfig;
use dnt_core::error::DbError;
use dnt_core::secret::MemorySecretStore;
use dnt_core::test_util::sample_config;
use dnt_store::ai::AiScope;
use dnt_store::db::open_in_memory;
use dnt_store::dntconn::export_dntconn;
use dnt_store::repo::{ConnectionRepo, NewSecrets, NodeId};
use std::sync::Arc;
use uuid::Uuid;

fn setup() -> ConnectionRepo {
    ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::new(MemorySecretStore::default()),
    )
}

fn create(repo: &ConnectionRepo, name: &str) -> Uuid {
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        name: name.into(),
        ..sample_config()
    };
    let secrets = NewSecrets {
        db_password: None,
        ssh_password: None,
        ssh_key_passphrase: None,
    };
    repo.create(cfg, secrets).unwrap().id
}

#[test]
fn grant_list_revoke() {
    let repo = setup();
    let a = create(&repo, "A");
    let b = create(&repo, "B");
    assert!(repo.ai_opt_ins(AiScope::Editor).unwrap().is_empty());

    repo.ai_opt_in_grant(a, AiScope::Editor, "https://api.example.com")
        .unwrap();
    repo.ai_opt_in_grant(b, AiScope::Editor, "http://localhost:8000")
        .unwrap();
    let rows = repo.ai_opt_ins(AiScope::Editor).unwrap();
    let ids: Vec<_> = rows.iter().map(|r| r.connection_id).collect();
    assert_eq!(rows.len(), 2);
    assert!(ids.contains(&a) && ids.contains(&b));
    let row_a = rows.iter().find(|r| r.connection_id == a).unwrap();
    assert_eq!(row_a.origin, "https://api.example.com");
    assert_eq!(row_a.scope, AiScope::Editor);
    assert!(!row_a.granted_at.is_empty());

    repo.ai_opt_in_revoke(a, AiScope::Editor).unwrap();
    let rows = repo.ai_opt_ins(AiScope::Editor).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].connection_id, b);
    // Revoking again is fine.
    repo.ai_opt_in_revoke(a, AiScope::Editor).unwrap();
}

#[test]
fn grant_replaces_origin() {
    let repo = setup();
    let a = create(&repo, "A");
    repo.ai_opt_in_grant(a, AiScope::Editor, "https://one.example")
        .unwrap();
    repo.ai_opt_in_grant(a, AiScope::Editor, "https://two.example")
        .unwrap();
    let rows = repo.ai_opt_ins(AiScope::Editor).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].origin, "https://two.example");
}

#[test]
fn deleting_the_connection_deletes_consent() {
    let repo = setup();
    let a = create(&repo, "A");
    repo.ai_opt_in_grant(a, AiScope::Editor, "https://api.example.com")
        .unwrap();
    repo.delete(a).unwrap();
    assert!(repo.ai_opt_ins(AiScope::Editor).unwrap().is_empty());
}

#[test]
fn unknown_connection_is_not_found() {
    let repo = setup();
    let err = repo
        .ai_opt_in_grant(Uuid::new_v4(), AiScope::Editor, "https://x.example")
        .unwrap_err();
    match err {
        DbError::Internal { message } => assert!(message.contains("not found"), "{message}"),
        other => panic!("expected not found, got {other:?}"),
    }
}

#[test]
fn export_never_carries_consent() {
    let repo = setup();
    let a = create(&repo, "A");
    repo.ai_opt_in_grant(a, AiScope::Editor, "https://api.example.com")
        .unwrap();
    let text = export_dntconn(&repo, &[NodeId::Connection(a)], None).unwrap();
    assert!(!text.contains("api.example.com"), "{text}");
    assert!(!text.to_ascii_lowercase().contains("ai_opt_in"), "{text}");
}
