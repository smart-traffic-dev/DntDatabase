//! Rendered MySQL / MariaDB `ALTER TABLE`s run on the Docker matrix (`DNT_TEST_DB=1`):
//! the statement is accepted and the table ends up as the draft says.

use dnt_core::driver::Session;
use dnt_core::server::DbKind;
use dnt_core::sql::build::TableRef;
use dnt_driver_mysql::{MySqlDriver, MySqlSession};
use dnt_schema::*;
use dnt_testkit::{require_db, Target};

const DB: &str = "dnt_render_test";

fn targets() -> Vec<Target> {
    let mut t = Target::mysql_family().to_vec();
    t.push(Target::MySql84);
    t
}

async fn connect(t: Target) -> MySqlSession {
    let s = MySqlDriver
        .connect_session(&t.resolved(None))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"));
    exec(&s, &format!("CREATE DATABASE IF NOT EXISTS `{DB}`")).await;
    s
}

async fn exec(s: &MySqlSession, sql: &str) {
    s.query_row_text(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
}

async fn text(s: &MySqlSession, sql: &str) -> String {
    s.query_row_text(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"))
        .and_then(|row| row.into_iter().next().flatten())
        .unwrap_or_default()
}

fn ty(base: &str, length: Option<u32>) -> TypeSpec {
    TypeSpec::Known {
        base: base.into(),
        length,
        precision: None,
        scale: None,
        unsigned: false,
        values: vec![],
    }
}

fn col(name: &str, data_type: TypeSpec, nullable: bool) -> ColumnDef {
    ColumnDef {
        original: Some(name.into()),
        name: name.into(),
        data_type,
        nullable,
        default: None,
        auto_increment: false,
        identity: None,
        on_update: None,
        charset: None,
        collation: None,
        comment: None,
        using: None,
        generated: None,
        locked: false,
    }
}

/// `CREATE TABLE <table> (id int NOT NULL, a int, b varchar(10), c int, PK id)` in
/// utf8mb4 / utf8mb4_unicode_ci, and its draft.
async fn create(s: &MySqlSession, table: &str) -> TableDef {
    exec(s, &format!("DROP TABLE IF EXISTS `{DB}`.`{table}`")).await;
    exec(
        s,
        &format!(
            "CREATE TABLE `{DB}`.`{table}` (id int NOT NULL, a int NULL, b varchar(10) NULL, \
             c int NULL, PRIMARY KEY (id)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 \
             COLLATE=utf8mb4_unicode_ci"
        ),
    )
    .await;
    TableDef {
        original_name: Some(table.into()),
        name: table.into(),
        columns: vec![
            col("id", ty("int", None), false),
            col("a", ty("int", None), true),
            col("b", ty("varchar", Some(10)), true),
            col("c", ty("int", None), true),
        ],
        primary_key: Some(KeyDef {
            original: Some("PRIMARY".into()),
            name: Some("PRIMARY".into()),
            columns: vec!["id".into()],
            locked: false,
        }),
        indexes: vec![],
        foreign_keys: vec![],
        checks: vec![],
        options: TableOptionsDef {
            engine: Some("InnoDB".into()),
            charset: Some("utf8mb4".into()),
            collation: Some("utf8mb4_unicode_ci".into()),
            row_format: None,
            locked: false,
        },
        comment: None,
        read_only_reason: None,
    }
}

/// Renders `old` → `new`, runs the one statement, returns it.
async fn apply(s: &MySqlSession, old: &TableDef, new: &TableDef) -> String {
    let changes = diff(old, new).expect("diff");
    let table = TableRef {
        kind: DbKind::MySql.as_sql(),
        database: DB.into(),
        schema: None,
        name: old.name.clone(),
    };
    let r = render(&table, old, &changes, s.server_info(), s.dialect()).expect("render");
    assert_eq!(r.statements.len(), 1, "{:?}", r.statements);
    exec(s, &r.statements[0]).await;
    r.statements[0].clone()
}

async fn column_order(s: &MySqlSession, table: &str) -> String {
    text(
        s,
        &format!(
            "SELECT GROUP_CONCAT(COLUMN_NAME ORDER BY ORDINAL_POSITION SEPARATOR ',') \
             FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = '{DB}' \
             AND TABLE_NAME = '{table}'"
        ),
    )
    .await
}

#[tokio::test]
async fn moves_that_depend_on_each_other_land_in_draft_order() {
    require_db!();
    for t in targets() {
        let s = connect(t).await;
        // Draft id, c, b, a: `b` moves after `c`, then `a` (altered) after `b`.
        let old = create(&s, "moves").await;
        let mut new = old.clone();
        new.columns = vec![
            old.columns[0].clone(),
            old.columns[3].clone(),
            old.columns[2].clone(),
            old.columns[1].clone(),
        ];
        new.columns[3].comment = Some("moved".into());
        let sql = apply(&s, &old, &new).await;
        assert_eq!(column_order(&s, "moves").await, "id,c,b,a", "{t:?}: {sql}");
        let comment = text(
            &s,
            &format!(
                "SELECT COLUMN_COMMENT FROM information_schema.COLUMNS WHERE \
                 TABLE_SCHEMA = '{DB}' AND TABLE_NAME = 'moves' AND COLUMN_NAME = 'a'"
            ),
        )
        .await;
        assert_eq!(comment, "moved", "{t:?}");

        // Same with `a` renamed instead.
        let old = create(&s, "moves").await;
        let mut new = old.clone();
        new.columns = vec![
            old.columns[0].clone(),
            old.columns[3].clone(),
            old.columns[2].clone(),
            old.columns[1].clone(),
        ];
        new.columns[3].name = "a2".into();
        let sql = apply(&s, &old, &new).await;
        assert_eq!(column_order(&s, "moves").await, "id,c,b,a2", "{t:?}: {sql}");
        exec(&s, &format!("DROP TABLE `{DB}`.`moves`")).await;
    }
}

#[tokio::test]
async fn table_charset_change_keeps_rewritten_columns() {
    require_db!();
    for t in targets() {
        let s = connect(t).await;
        let old = create(&s, "charset").await;
        let mut new = old.clone();
        new.options.charset = Some("latin1".into());
        new.options.collation = Some("latin1_swedish_ci".into());
        new.columns[2].comment = Some("kept".into());
        let sql = apply(&s, &old, &new).await;
        let charset = text(
            &s,
            &format!(
                "SELECT CONCAT(CHARACTER_SET_NAME, ' ', COLLATION_NAME) FROM \
                 information_schema.COLUMNS WHERE TABLE_SCHEMA = '{DB}' \
                 AND TABLE_NAME = 'charset' AND COLUMN_NAME = 'b'"
            ),
        )
        .await;
        assert_eq!(charset, "utf8mb4 utf8mb4_unicode_ci", "{t:?}: {sql}");
        let table = text(
            &s,
            &format!(
                "SELECT TABLE_COLLATION FROM information_schema.TABLES WHERE \
                 TABLE_SCHEMA = '{DB}' AND TABLE_NAME = 'charset'"
            ),
        )
        .await;
        assert_eq!(table, "latin1_swedish_ci", "{t:?}");
        exec(&s, &format!("DROP TABLE `{DB}`.`charset`")).await;
    }
}
