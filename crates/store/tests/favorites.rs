//! Migration 3: favorites (saved queries) and user snippets.

use dnt_core::config::ConnectionConfig;
use dnt_core::error::DbError;
use dnt_core::secret::MemorySecretStore;
use dnt_core::test_util::sample_config;
use dnt_store::db::open_in_memory;
use dnt_store::favorites::{FavoriteInput, SnippetInput};
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use std::sync::Arc;
use uuid::Uuid;

fn repo() -> ConnectionRepo {
    ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::new(MemorySecretStore::default()),
    )
}

fn connection(repo: &ConnectionRepo) -> Uuid {
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        name: "a".into(),
        ..sample_config()
    };
    repo.create(
        cfg,
        NewSecrets {
            db_password: None,
            ssh_password: None,
            ssh_key_passphrase: None,
        },
    )
    .unwrap()
    .id
}

fn fav(name: &str, folder: Option<&str>) -> FavoriteInput {
    FavoriteInput {
        name: name.into(),
        folder: folder.map(str::to_string),
        sql: format!("SELECT '{name}';"),
        connection_id: None,
        database: None,
    }
}

fn snip(prefix: &str) -> SnippetInput {
    SnippetInput {
        prefix: prefix.into(),
        label: format!("Label {prefix}"),
        body: "SELECT ${1:col} FROM ${2:table};$0".into(),
        description: " Desc ".into(),
    }
}

fn field(err: &DbError) -> Option<&str> {
    match err {
        DbError::Config { field, .. } => field.as_deref(),
        _ => None,
    }
}

fn names(repo: &ConnectionRepo) -> Vec<String> {
    repo.favorites_list()
        .unwrap()
        .into_iter()
        .map(|f| match f.folder {
            Some(d) => format!("{d}:{}", f.name),
            None => f.name,
        })
        .collect()
}

#[test]
fn favorites_crud_keeps_text_verbatim() {
    let repo = repo();
    let conn = connection(&repo);
    let sql = "-- Tiếng Việt 😀\r\nSELECT * FROM `orders` WHERE id = ?;\n";
    let created = repo
        .favorite_create(&FavoriteInput {
            name: "  Orders  ".into(),
            folder: Some(" Reports / Daily ".into()),
            sql: sql.into(),
            connection_id: Some(conn),
            database: Some("shop".into()),
        })
        .unwrap();
    assert_eq!(created.name, "Orders");
    assert_eq!(created.folder.as_deref(), Some("Reports/Daily"));
    assert_eq!(created.sql, sql);
    assert_eq!(created.connection_id, Some(conn));
    assert_eq!(created.database.as_deref(), Some("shop"));
    assert_eq!(created.created_at, created.updated_at);
    assert_eq!(repo.favorites_list().unwrap(), vec![created.clone()]);

    let renamed = repo.favorite_rename(created.id, "Daily orders").unwrap();
    assert_eq!(renamed.name, "Daily orders");
    assert!(repo.favorite_rename(created.id, "  ").is_err());

    let updated = repo
        .favorite_update(
            created.id,
            &FavoriteInput {
                name: "All orders".into(),
                folder: None,
                sql: "SELECT 2".into(),
                connection_id: None,
                database: None,
            },
        )
        .unwrap();
    assert_eq!(updated.folder, None);
    assert_eq!(updated.connection_id, None);
    assert_eq!(updated.sql, "SELECT 2");
    assert_eq!(updated.created_at, created.created_at);

    repo.favorite_delete(created.id).unwrap();
    repo.favorite_delete(created.id).unwrap();
    assert!(repo.favorites_list().unwrap().is_empty());
    let err = repo.favorite_rename(created.id, "x").unwrap_err();
    assert_eq!(field(&err), Some("id"));
}

#[test]
fn favorites_order_and_move() {
    let repo = repo();
    let b = repo.favorite_create(&fav("b", None)).unwrap();
    repo.favorite_create(&fav("a", None)).unwrap();
    repo.favorite_create(&fav("z", Some("Reports"))).unwrap();
    repo.favorite_create(&fav("y", Some("Reports"))).unwrap();
    repo.favorite_create(&fav("x", Some("Admin"))).unwrap();
    // Root first, then folders by path; inside a folder in creation order.
    assert_eq!(
        names(&repo),
        ["b", "a", "Admin:x", "Reports:z", "Reports:y"]
    );

    let moved = repo.favorite_move(b.id, Some("Reports/")).unwrap();
    assert_eq!(moved.folder.as_deref(), Some("Reports"));
    assert_eq!(
        names(&repo),
        ["a", "Admin:x", "Reports:z", "Reports:y", "Reports:b"]
    );
    // Moving to the same folder keeps the position.
    let same = repo.favorite_move(b.id, Some("Reports")).unwrap();
    assert_eq!(same.sort, moved.sort);
    repo.favorite_move(b.id, Some(" ")).unwrap();
    assert_eq!(
        names(&repo),
        ["a", "b", "Admin:x", "Reports:z", "Reports:y"]
    );
}

#[test]
fn favorites_refuse_secrets_and_bad_input() {
    let repo = repo();
    for sql in [
        "CREATE USER 'u'@'%' IDENTIFIED BY 's3cret';",
        "ALTER ROLE app PASSWORD 'hunter2'",
        "SET PASSWORD FOR u = 'x'",
    ] {
        let err = repo
            .favorite_create(&FavoriteInput {
                sql: sql.into(),
                ..fav("secret", None)
            })
            .unwrap_err();
        assert_eq!(field(&err), Some("sql"), "{sql}");
        assert!(err.to_string().contains("secret"), "{err}");
    }
    assert!(repo.favorites_list().unwrap().is_empty());

    // The update path refuses too, and leaves the row unchanged.
    let ok = repo.favorite_create(&fav("ok", None)).unwrap();
    let err = repo
        .favorite_update(
            ok.id,
            &FavoriteInput {
                sql: "CREATE USER u IDENTIFIED BY 'x'".into(),
                ..fav("ok", None)
            },
        )
        .unwrap_err();
    assert_eq!(field(&err), Some("sql"));
    assert_eq!(repo.favorites_list().unwrap(), vec![ok]);

    let err = repo.favorite_create(&fav("  ", None)).unwrap_err();
    assert_eq!(field(&err), Some("name"));
    let err = repo
        .favorite_create(&FavoriteInput {
            sql: " \n".into(),
            ..fav("empty", None)
        })
        .unwrap_err();
    assert_eq!(field(&err), Some("sql"));
}

#[test]
fn deleting_connection_keeps_favorite_unbound() {
    let repo = repo();
    let conn = connection(&repo);
    let f = repo
        .favorite_create(&FavoriteInput {
            connection_id: Some(conn),
            database: Some("shop".into()),
            ..fav("bound", None)
        })
        .unwrap();
    repo.delete(conn).unwrap();
    let got = &repo.favorites_list().unwrap()[0];
    assert_eq!(got.id, f.id);
    assert_eq!(got.connection_id, None);
    assert_eq!(got.database.as_deref(), Some("shop"));
    // An unknown connection is stored as NULL rather than failing.
    let other = repo
        .favorite_create(&FavoriteInput {
            connection_id: Some(Uuid::new_v4()),
            ..fav("ghost", None)
        })
        .unwrap();
    assert_eq!(other.connection_id, None);
}

#[test]
fn snippets_crud_unique_prefix_and_order() {
    let repo = repo();
    let sel = repo.snippet_create(&snip("sel")).unwrap();
    assert_eq!(sel.description, "Desc");
    assert_eq!(sel.body, "SELECT ${1:col} FROM ${2:table};$0");
    repo.snippet_create(&snip("Ins")).unwrap();
    repo.snippet_create(&snip("del")).unwrap();
    let prefixes: Vec<String> = repo
        .snippets_list()
        .unwrap()
        .into_iter()
        .map(|s| s.prefix)
        .collect();
    assert_eq!(prefixes, ["del", "Ins", "sel"]);

    // Unique, case-insensitively, on create and on update.
    let err = repo.snippet_create(&snip("SEL")).unwrap_err();
    assert_eq!(field(&err), Some("prefix"));
    assert!(err.to_string().contains("already exists"), "{err}");
    let err = repo.snippet_update(sel.id, &snip("ins")).unwrap_err();
    assert_eq!(field(&err), Some("prefix"));

    let updated = repo
        .snippet_update(
            sel.id,
            &SnippetInput {
                label: "Select all".into(),
                ..snip("sel")
            },
        )
        .unwrap();
    assert_eq!(updated.label, "Select all");
    assert_eq!(updated.created_at, sel.created_at);

    let err = repo.snippet_create(&snip("has space")).unwrap_err();
    assert_eq!(field(&err), Some("prefix"));
    let err = repo
        .snippet_create(&SnippetInput {
            body: " ".into(),
            ..snip("empty")
        })
        .unwrap_err();
    assert_eq!(field(&err), Some("body"));

    repo.snippet_delete(sel.id).unwrap();
    repo.snippet_delete(sel.id).unwrap();
    assert_eq!(repo.snippets_list().unwrap().len(), 2);
    let err = repo.snippet_update(sel.id, &snip("sel")).unwrap_err();
    assert_eq!(field(&err), Some("id"));
}
