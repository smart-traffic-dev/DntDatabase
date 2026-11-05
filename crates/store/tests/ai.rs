//! AI consent (`ai_consent`) and each connection's own choice (`ai_connection`).

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
fn consent_grant_replace_revoke() {
    let repo = setup();
    assert_eq!(repo.ai_consent(AiScope::Editor).unwrap(), None);

    repo.ai_consent_grant(AiScope::Editor, "https://one.example")
        .unwrap();
    let c = repo.ai_consent(AiScope::Editor).unwrap().unwrap();
    assert_eq!(c.origin, "https://one.example");
    assert!(!c.granted_at.is_empty());

    repo.ai_consent_grant(AiScope::Editor, "https://two.example")
        .unwrap();
    assert_eq!(
        repo.ai_consent(AiScope::Editor).unwrap().unwrap().origin,
        "https://two.example"
    );

    repo.ai_consent_revoke(AiScope::Editor).unwrap();
    assert_eq!(repo.ai_consent(AiScope::Editor).unwrap(), None);
    // Revoking again is fine.
    repo.ai_consent_revoke(AiScope::Editor).unwrap();
}

#[test]
fn choices_set_and_replace() {
    let repo = setup();
    let a = create(&repo, "A");
    let b = create(&repo, "B");
    assert!(repo
        .ai_connection_choices(AiScope::Editor)
        .unwrap()
        .is_empty());

    repo.ai_connection_set(a, AiScope::Editor, false).unwrap();
    repo.ai_connection_set(b, AiScope::Editor, true).unwrap();
    repo.ai_connection_set(a, AiScope::Editor, true).unwrap();
    let mut choices = repo.ai_connection_choices(AiScope::Editor).unwrap();
    choices.sort();
    let mut expected = vec![(a, true), (b, true)];
    expected.sort();
    assert_eq!(choices, expected);
}

#[test]
fn deleting_the_connection_deletes_its_choice() {
    let repo = setup();
    let a = create(&repo, "A");
    repo.ai_connection_set(a, AiScope::Editor, false).unwrap();
    repo.delete(a).unwrap();
    assert!(repo
        .ai_connection_choices(AiScope::Editor)
        .unwrap()
        .is_empty());
}

#[test]
fn unknown_connection_is_not_found() {
    let repo = setup();
    let err = repo
        .ai_connection_set(Uuid::new_v4(), AiScope::Editor, true)
        .unwrap_err();
    match err {
        DbError::Internal { message } => assert!(message.contains("not found"), "{message}"),
        other => panic!("expected not found, got {other:?}"),
    }
}

#[test]
fn export_never_carries_consent_or_choices() {
    let repo = setup();
    let a = create(&repo, "A");
    repo.ai_consent_grant(AiScope::Editor, "https://api.example.com")
        .unwrap();
    repo.ai_connection_set(a, AiScope::Editor, false).unwrap();
    let text = export_dntconn(&repo, &[NodeId::Connection(a)], None).unwrap();
    assert!(!text.contains("api.example.com"), "{text}");
    let lower = text.to_ascii_lowercase();
    assert!(
        !lower.contains("ai_consent") && !lower.contains("ai_connection"),
        "{text}"
    );
}
