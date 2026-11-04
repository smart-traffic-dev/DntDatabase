//! The key browser of a Command connection (the fake Command driver of `dnt_testkit`,
//! registered as Redis): scanning keys, reading a value, and key edits through the
//! shared edit store and `data_apply` (R4a Task 7).

use dnt_core::config::{ConnectionConfig, DriverOptions, EnvTag, SecretMode};
use dnt_core::data_access::KeyEdit;
use dnt_core::driver::Driver;
use dnt_core::error::DbError;
use dnt_core::keyspace::{KeyBytes, KeyName, KeyType, KeyValue, ScanRequest, ValueRange};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::DbKind;
use dnt_core::test_util::sample_config;
use dnt_core::value::Value;
use dnt_query::edit::Protection;
use dnt_query::Verdict;
use dnt_store::db::open_in_memory;
use dnt_store::history::{HistoryEntry, HistoryFilter, HistoryOrigin, HistoryStatus};
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_testkit::fake_command::{collection_key, FakeCommandDriver, FakeStore, FakeValue};
use dnt_tunnel::KnownHosts;
use dntdatabase::commands::data::{data_apply_impl, Atomicity};
use dntdatabase::commands::keys::{key_edit_preview_impl, key_value_impl, keys_scan_impl};
use dntdatabase::edit_plans::ApplyOutcome;
use dntdatabase::export_paths::ExportPaths;
use dntdatabase::file_picks::FilePicks;
use dntdatabase::session_manager::SessionManager;
use dntdatabase::state::{AppPaths, AppState};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use uuid::Uuid;

struct Fixture {
    state: AppState,
    id: Uuid,
    _dir: tempfile::TempDir,
}

/// An `AppState` whose only driver is `driver` (registered as `cfg.kind`), with `cfg`
/// saved and open.
async fn fixture(driver: Arc<dyn Driver>, cfg: ConnectionConfig) -> Fixture {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let dir = tempfile::tempdir().unwrap();
    let sessions = SessionManager::new(
        Arc::clone(&repo),
        Arc::clone(&secrets),
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(cfg.kind, driver)]),
    );
    let new_secrets = NewSecrets {
        db_password: None,
        ssh_password: None,
        ssh_key_passphrase: None,
    };
    let id = repo.create(cfg, new_secrets).unwrap().id;
    sessions.open(id, None).await.unwrap();
    let state = AppState {
        repo,
        secrets,
        sessions,
        paths: AppPaths::in_dir(dir.path().join("data")),
        export_paths: ExportPaths::default(),
        file_picks: FilePicks::default(),
    };
    Fixture {
        state,
        id,
        _dir: dir,
    }
}

/// The hash `h:users` of db "2": `u1 = alice`, `u2 = bob`, `u3 = carol`.
fn original_users() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("u1".into(), "alice".into()),
        ("u2".into(), "bob".into()),
        ("u3".into(), "carol".into()),
    ])
}

/// The fake Command driver as Redis, open on db "0", with the hash `h:users` and the
/// strings `a`, `b`, `c` in db "2". `setup` runs on the store before the connection opens.
async fn redis_fixture_with(
    env_tag: Option<EnvTag>,
    read_only: bool,
    setup: impl FnOnce(&FakeStore),
) -> (Fixture, FakeStore) {
    let driver = FakeCommandDriver::with_kind(DbKind::Redis);
    let store = driver.store();
    let mut cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::Redis,
        name: "Fake Redis".into(),
        port: 6379,
        default_db: Some("0".into()),
        secret_mode: SecretMode::Saved,
        env_tag,
        read_only,
        ..sample_config()
    };
    cfg.options.driver = DriverOptions::Redis { db: 0 };
    store.set(
        "2",
        &collection_key("users"),
        FakeValue::Hash(original_users()),
    );
    for k in ["a", "b", "c"] {
        store.set("2", k.as_bytes(), FakeValue::Str(format!("value of {k}")));
    }
    setup(&store);
    (fixture(Arc::new(driver), cfg).await, store)
}

async fn redis_fixture() -> (Fixture, FakeStore) {
    redis_fixture_with(None, false, |_| {}).await
}

fn key(name: &str) -> KeyName {
    KeyName::from_bytes(name.as_bytes().to_vec())
}

fn users_key() -> KeyName {
    KeyName::from_bytes(collection_key("users"))
}

fn users_hash(store: &FakeStore) -> BTreeMap<String, String> {
    match store.get("2", &collection_key("users")) {
        Some(FakeValue::Hash(h)) => h,
        other => panic!("expected the users hash, got {other:?}"),
    }
}

fn scan(cursor: Option<&str>, count: u32) -> ScanRequest {
    ScanRequest {
        cursor: cursor.map(str::to_string),
        pattern: None,
        key_type: None,
        count,
    }
}

fn history(f: &Fixture) -> Vec<HistoryEntry> {
    let mut rows = f
        .state
        .repo
        .history_list(&HistoryFilter {
            connection_id: Some(f.id),
            search: None,
            origin: None,
            limit: 1000,
        })
        .unwrap();
    rows.sort_by_key(|r| r.id);
    rows
}

fn set_alicia() -> KeyEdit {
    KeyEdit::HashSet {
        key: users_key(),
        field: KeyBytes::from("u1"),
        value: KeyBytes::from("alicia"),
    }
}

fn names(page: &dnt_core::keyspace::KeyPage) -> Vec<String> {
    page.keys.iter().map(|k| k.name.display.clone()).collect()
}

#[tokio::test]
async fn keys_scan_pages_through_keys() {
    let (f, store) = redis_fixture_with(None, false, |s| {
        for i in 0..1005 {
            s.set(
                "3",
                format!("k{i:04}").as_bytes(),
                FakeValue::Str("x".into()),
            );
        }
    })
    .await;
    let first = keys_scan_impl(&f.state, f.id, "2".into(), scan(None, 2))
        .await
        .unwrap();
    assert_eq!(names(&first), ["a", "b"]);
    assert_eq!(first.keys[0].key_type, KeyType::String);
    let cursor = first.next_cursor.clone().expect("more keys");
    let rest = keys_scan_impl(&f.state, f.id, "2".into(), scan(Some(&cursor), 2))
        .await
        .unwrap();
    assert_eq!(names(&rest), ["c", "h:users"]);
    assert_eq!(rest.keys[1].key_type, KeyType::Hash);
    assert_eq!(rest.next_cursor, None);

    // A pattern and a type narrow the scan.
    let req = ScanRequest {
        pattern: Some("h:*".into()),
        key_type: Some(KeyType::Hash),
        ..scan(None, 10)
    };
    let hashes = keys_scan_impl(&f.state, f.id, "2".into(), req)
        .await
        .unwrap();
    assert_eq!(names(&hashes), ["h:users"]);

    // `count` is clamped to 1..=1000.
    let big = keys_scan_impl(&f.state, f.id, "3".into(), scan(None, 5000))
        .await
        .unwrap();
    assert_eq!(big.keys.len(), 1000);
    assert_eq!(big.next_cursor.as_deref(), Some("1000"));
    let one = keys_scan_impl(&f.state, f.id, "3".into(), scan(None, 0))
        .await
        .unwrap();
    assert_eq!(names(&one), ["k0000"]);

    // Browsing sends no command.
    assert!(store.executed().is_empty());
}

#[tokio::test]
async fn key_value_reads_a_hash() {
    let (f, store) = redis_fixture().await;
    let view = key_value_impl(
        &f.state,
        f.id,
        "2".into(),
        users_key(),
        ValueRange {
            offset: 1,
            count: 1,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        view.value,
        KeyValue::Hash {
            fields: vec![(Value::Text("u2".into()), Value::Text("bob".into()))],
            total: 3,
            next: Some("2".into()),
        }
    );
    let s = key_value_impl(
        &f.state,
        f.id,
        "2".into(),
        key("a"),
        ValueRange {
            offset: 0,
            count: 10,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        s.value,
        KeyValue::String {
            value: Value::Text("value of a".into())
        }
    );
    assert!(store.executed().is_empty());
}

#[tokio::test]
async fn key_edit_round_trip() {
    let (f, store) = redis_fixture().await;
    let p = key_edit_preview_impl(&f.state, f.id, "2".into(), set_alicia())
        .await
        .unwrap();
    assert_eq!(p.verdict, Verdict::Allow, "{p:?}");
    assert_eq!(p.statements.len(), 1);
    assert!(p.statements[0].starts_with("HSET"), "{p:?}");
    // One command is applied as a unit.
    assert_eq!(p.atomic, Atomicity::Yes);
    assert_eq!(p.protections, [Protection::KeyOnly]);
    assert_eq!(p.reason, None);
    assert!(store.executed().is_empty(), "a preview sends nothing");

    let r = data_apply_impl(&f.state, p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed, "{r:?}");
    assert_eq!(r.statements.len(), 1);
    assert!(r.statements[0].error.is_none());
    assert_eq!(store.executed(), ["HSET"]);
    assert_eq!(users_hash(&store)["u1"], "alicia");
    let view = key_value_impl(
        &f.state,
        f.id,
        "2".into(),
        users_key(),
        ValueRange {
            offset: 0,
            count: 1,
        },
    )
    .await
    .unwrap();
    match view.value {
        KeyValue::Hash { fields, .. } => {
            assert_eq!(
                fields,
                [(Value::Text("u1".into()), Value::Text("alicia".into()))]
            )
        }
        other => panic!("expected a hash, got {other:?}"),
    }

    let rows = history(&f);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].sql_text, p.statements[0]);
    assert_eq!(rows[0].origin, HistoryOrigin::DataEdit);
    assert_eq!(rows[0].status, HistoryStatus::Ok);
    assert_eq!(rows[0].database.as_deref(), Some("2"));

    // A plan runs once: never sent again.
    let e = data_apply_impl(&f.state, p.plan_id, false)
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::PlanExpired), "{e:?}");
    assert_eq!(store.executed().len(), 1);
}

#[tokio::test]
async fn key_edit_blocked_read_only() {
    let (f, store) = redis_fixture_with(None, true, |_| {}).await;
    let p = key_edit_preview_impl(&f.state, f.id, "2".into(), set_alicia())
        .await
        .unwrap();
    assert_eq!(p.verdict, Verdict::Block);
    assert_eq!(p.reason.as_deref(), Some("the connection is read-only"));
    let e = data_apply_impl(&f.state, p.plan_id, true)
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::Blocked { count: 1 }), "{e:?}");
    assert!(store.executed().is_empty(), "nothing is sent");
    assert_eq!(users_hash(&store), original_users());
    assert!(history(&f).is_empty());
}

#[tokio::test]
async fn key_edit_confirms_on_production() {
    let (f, store) = redis_fixture_with(Some(EnvTag::Production), false, |_| {}).await;
    let delete = || KeyEdit::Delete { key: key("a") };
    let p = key_edit_preview_impl(&f.state, f.id, "2".into(), delete())
        .await
        .unwrap();
    assert_eq!(p.verdict, Verdict::Confirm, "{p:?}");
    assert_eq!(p.reason, None);
    let e = data_apply_impl(&f.state, p.plan_id, false)
        .await
        .unwrap_err();
    assert!(
        matches!(e, DbError::ConfirmationRequired { count: 1 }),
        "{e:?}"
    );
    assert!(store.executed().is_empty());
    assert!(store.get("2", b"a").is_some());

    let p = key_edit_preview_impl(&f.state, f.id, "2".into(), delete())
        .await
        .unwrap();
    let r = data_apply_impl(&f.state, p.plan_id, true).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed, "{r:?}");
    assert_eq!(store.executed(), ["DEL"]);
    assert!(store.get("2", b"a").is_none());
}

#[tokio::test]
async fn keys_refused_without_paged_objects() {
    let (f, store) = redis_fixture_with(None, false, FakeStore::without_paged_objects).await;
    let unsupported = |e: DbError| match e {
        DbError::Unsupported { feature, .. } => feature,
        other => panic!("expected Unsupported, got {other:?}"),
    };
    let e = keys_scan_impl(&f.state, f.id, "2".into(), scan(None, 10))
        .await
        .unwrap_err();
    assert_eq!(unsupported(e), "key browser");
    let e = key_value_impl(
        &f.state,
        f.id,
        "2".into(),
        users_key(),
        ValueRange {
            offset: 0,
            count: 10,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(unsupported(e), "key browser");
    assert!(store.executed().is_empty());
}
