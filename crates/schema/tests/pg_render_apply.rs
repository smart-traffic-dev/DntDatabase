//! Rendered PostgreSQL statements run on the Docker matrix (`DNT_TEST_DB=1`) inside one
//! transaction, as the backend sends them: they are accepted and the table ends up as
//! the draft says. Each test works in its own scratch schema, dropped afterwards also
//! when an assertion fails.

use dnt_core::driver::Session;
use dnt_core::meta::IdentityKind;
use dnt_core::server::DbKind;
use dnt_core::sql::build::TableRef;
use dnt_driver_pg::{PgDriver, PgSession};
use dnt_schema::*;
use dnt_testkit::{require_db, Target};
use std::future::Future;

/// One target's session and the scratch schema of the running test.
struct Ctx {
    t: Target,
    s: PgSession,
    sch: &'static str,
}

async fn connect(t: Target) -> PgSession {
    PgDriver
        .connect_session(&t.resolved(None))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"))
}

async fn exec(s: &PgSession, sql: &str) {
    s.query_row_text(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
}

async fn text(s: &PgSession, sql: &str) -> String {
    s.query_row_text(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"))
        .and_then(|row| row.into_iter().next().flatten())
        .unwrap_or_default()
}

/// Runs `body` on each PG target in a fresh scratch schema `sch`, dropping it
/// afterwards even when `body` panics (the panic is re-raised after the cleanup).
async fn each_target<F, Fut>(sch: &'static str, body: F)
where
    F: Fn(Ctx) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    for t in Target::pg() {
        let s = connect(t).await;
        exec(&s, &format!("DROP SCHEMA IF EXISTS {sch} CASCADE")).await;
        exec(&s, &format!("CREATE SCHEMA {sch}")).await;
        let outcome = tokio::spawn(body(Ctx { t, s, sch })).await;
        let cleanup = connect(t).await;
        exec(&cleanup, &format!("DROP SCHEMA IF EXISTS {sch} CASCADE")).await;
        if let Err(e) = outcome {
            std::panic::resume_unwind(e.into_panic());
        }
    }
}

impl Ctx {
    fn table(&self, name: &str) -> TableRef {
        TableRef {
            kind: DbKind::Postgres,
            database: "postgres".into(),
            schema: Some(self.sch.into()),
            name: name.into(),
        }
    }

    async fn exec(&self, sql: &str) {
        exec(&self.s, sql).await
    }

    async fn text(&self, sql: &str) -> String {
        text(&self.s, sql).await
    }

    /// Sends the statements in one `BEGIN … COMMIT`, as the backend does.
    async fn run(&self, statements: &[String]) {
        self.exec("BEGIN").await;
        for sql in statements {
            self.exec(sql).await;
        }
        self.exec("COMMIT").await;
    }

    /// Renders `old` → `new`, runs it, returns the statements.
    async fn apply(&self, old: &TableDef, new: &TableDef) -> Vec<String> {
        let changes = diff(old, new).expect("diff");
        let r = render(
            &self.table(&old.name),
            old,
            &changes,
            self.s.server_info(),
            self.s.dialect(),
        )
        .expect("render");
        self.run(&r.statements).await;
        r.statements
    }

    /// `customers(id)` and `orders`, with one row each, and the draft of `orders`.
    async fn create(&self) -> TableDef {
        let sch = self.sch;
        self.exec(&format!(
            "CREATE TABLE {sch}.customers (id bigint PRIMARY KEY); \
             INSERT INTO {sch}.customers VALUES (1); \
             CREATE TABLE {sch}.orders (\
               id bigint GENERATED ALWAYS AS IDENTITY NOT NULL, \
               qty integer NOT NULL DEFAULT 0, \
               total numeric(12,2), \
               note text, \
               customer_id bigint, \
               code varchar(40), \
               CONSTRAINT orders_pkey PRIMARY KEY (id), \
               CONSTRAINT fk_customer FOREIGN KEY (customer_id) \
                 REFERENCES {sch}.customers(id)); \
             INSERT INTO {sch}.orders (qty, total, note, customer_id, code) \
               VALUES (5, 12.75, '42', 1, 'abc')"
        ))
        .await;
        orders()
    }

    async fn column_type(&self, table: &str, column: &str) -> String {
        let sch = self.sch;
        self.text(&format!(
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = '{sch}.{table}'::regclass AND attname = '{column}'"
        ))
        .await
    }

    /// `<contype> <pg_get_constraintdef>`, empty when there is no such constraint.
    async fn constraint_def(&self, name: &str) -> String {
        let sch = self.sch;
        self.text(&format!(
            "SELECT contype::text || ' ' || pg_get_constraintdef(c.oid) FROM pg_constraint c \
             JOIN pg_namespace n ON n.oid = c.connamespace \
             WHERE n.nspname = '{sch}' AND conname = '{name}'"
        ))
        .await
    }

    async fn index_def(&self, name: &str) -> String {
        let sch = self.sch;
        self.text(&format!("SELECT pg_get_indexdef('{sch}.{name}'::regclass)"))
            .await
    }
}

fn ty(base: &str, length: Option<u32>, precision: Option<u32>, scale: Option<u32>) -> TypeSpec {
    TypeSpec::Known {
        base: base.into(),
        length,
        precision,
        scale,
        unsigned: false,
        values: vec![],
    }
}

fn known(base: &str) -> TypeSpec {
    ty(base, None, None, None)
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

fn part(column: &str) -> IndexPartDef {
    IndexPartDef {
        column: Some(column.into()),
        expression: None,
        prefix: None,
        descending: false,
    }
}

fn index(name: &str, unique: bool, constraint: bool, cols: &[&str]) -> IndexDef {
    IndexDef {
        original: Some(name.into()),
        name: name.into(),
        unique,
        parts: cols.iter().map(|c| part(c)).collect(),
        method: Some("btree".into()),
        include: vec![],
        visible: true,
        comment: None,
        constraint,
        locked: false,
    }
}

/// The draft of the `orders` table [`Ctx::create`] makes.
fn orders() -> TableDef {
    TableDef {
        original_name: Some("orders".into()),
        name: "orders".into(),
        columns: vec![
            ColumnDef {
                auto_increment: true,
                identity: Some(IdentityKind::Always),
                ..col("id", known("bigint"), false)
            },
            ColumnDef {
                default: Some(DefaultSpec::Value { text: "0".into() }),
                ..col("qty", known("integer"), false)
            },
            col("total", ty("numeric", None, Some(12), Some(2)), true),
            col("note", known("text"), true),
            col("customer_id", known("bigint"), true),
            col("code", ty("varchar", Some(40), None, None), true),
        ],
        primary_key: Some(KeyDef {
            original: Some("orders_pkey".into()),
            name: Some("orders_pkey".into()),
            columns: vec!["id".into()],
            locked: false,
        }),
        indexes: vec![],
        foreign_keys: vec![FkDef {
            original: Some("fk_customer".into()),
            name: "fk_customer".into(),
            columns: vec!["customer_id".into()],
            ref_schema: None,
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "NO ACTION".into(),
            match_type: None,
            deferrable: false,
            initially_deferred: false,
            locked: false,
        }],
        checks: vec![],
        options: TableOptionsDef::default(),
        comment: None,
        read_only_reason: None,
    }
}

#[tokio::test]
async fn statement_order_is_fixed() {
    require_db!();
    each_target("dnt_render_pg_order", |c| async move {
        let (t, sch) = (c.t, c.sch);
        let old = c.create().await;
        let mut new = old.clone();
        new.foreign_keys.clear();
        new.columns[3].name = "memo".into();
        new.columns[3].comment = Some("Ghi chú của khách".into());
        new.columns[1].data_type = known("bigint");
        new.columns.push(ColumnDef {
            original: None,
            ..col("channel", ty("varchar", Some(20), None, None), true)
        });
        new.indexes.push(IndexDef {
            original: None,
            ..index("ix_channel", false, false, &["channel"])
        });
        let sql = c.apply(&old, &new).await;
        assert_eq!(sql.len(), 6, "{t:?}: {sql:?}");
        assert_eq!(c.column_type("orders", "qty").await, "bigint", "{t:?}");
        assert_eq!(
            c.text(&format!(
                "SELECT qty::text || ' ' || memo FROM {sch}.orders"
            ))
            .await,
            "5 42",
            "{t:?}"
        );
        assert_eq!(
            c.text(&format!(
                "SELECT col_description('{sch}.orders'::regclass, attnum) \
                 FROM pg_attribute WHERE attrelid = '{sch}.orders'::regclass \
                 AND attname = 'memo'"
            ))
            .await,
            "Ghi chú của khách",
            "{t:?}"
        );
        assert_eq!(c.constraint_def("fk_customer").await, "", "{t:?}");
        assert_eq!(
            c.index_def("ix_channel").await,
            format!("CREATE INDEX ix_channel ON {sch}.orders USING btree (channel)"),
            "{t:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn type_change_auto_using() {
    require_db!();
    each_target("dnt_render_pg_using", |c| async move {
        let (t, sch) = (c.t, c.sch);
        let old = c.create().await;
        let mut new = old.clone();
        new.columns[2].data_type = known("integer"); // numeric(12,2) -> integer
        new.columns[3].data_type = known("integer"); // text '42' -> integer
        new.columns[5].data_type = known("text"); // varchar(40) -> text
        let sql = c.apply(&old, &new).await;
        assert!(sql.iter().all(|q| q.contains(" USING ")), "{t:?}: {sql:?}");
        for (column, ty) in [("total", "integer"), ("note", "integer"), ("code", "text")] {
            assert_eq!(c.column_type("orders", column).await, ty, "{t:?}");
        }
        assert_eq!(
            c.text(&format!(
                "SELECT total::text || ' ' || note::text || ' ' || code FROM {sch}.orders"
            ))
            .await,
            "13 42 abc",
            "{t:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn unique_constraint_kept_as_constraint() {
    require_db!();
    each_target("dnt_render_pg_unique", |c| async move {
        let (t, sch) = (c.t, c.sch);
        let mut old = c.create().await;
        c.exec(&format!(
            "ALTER TABLE {sch}.orders ADD CONSTRAINT uq_code UNIQUE (code); \
             CREATE UNIQUE INDEX ux_note ON {sch}.orders (note)"
        ))
        .await;
        old.indexes = vec![
            index("uq_code", true, true, &["code"]),
            index("ux_note", true, false, &["note"]),
        ];
        let mut new = old.clone();
        new.indexes[0].parts.push(part("qty"));
        new.indexes[1].parts.push(part("qty"));
        let sql = c.apply(&old, &new).await;
        assert_eq!(
            c.constraint_def("uq_code").await,
            "u UNIQUE (code, qty)",
            "{t:?}: {sql:?}"
        );
        assert_eq!(c.constraint_def("ux_note").await, "", "{t:?}");
        assert_eq!(
            c.index_def("ux_note").await,
            format!("CREATE UNIQUE INDEX ux_note ON {sch}.orders USING btree (note, qty)"),
            "{t:?}"
        );

        // Renaming the constraint's index renames the constraint.
        let old = new;
        let mut new = old.clone();
        new.indexes[0].name = "uq_code2".into();
        c.apply(&old, &new).await;
        assert_eq!(c.constraint_def("uq_code").await, "", "{t:?}");
        assert_eq!(
            c.constraint_def("uq_code2").await,
            "u UNIQUE (code, qty)",
            "{t:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn column_alterations_run_in_order() {
    require_db!();
    each_target("dnt_render_pg_alter", |c| async move {
        let (t, sch) = (c.t, c.sch);
        let old = c.create().await;
        let mut new = old.clone();
        new.name = "orders2".into();
        new.columns[0].identity = None;
        new.columns[1].nullable = true;
        new.columns[4].identity = Some(IdentityKind::ByDefault);
        new.columns[4].nullable = false;
        new.columns[1].data_type = known("bigint");
        new.columns[1].default = Some(DefaultSpec::Value { text: "7".into() });
        new.columns[5].collation = Some("C".into());
        new.columns[5].name = "sku".into();
        let sql = c.apply(&old, &new).await;
        assert_eq!(
            c.text(&format!(
                "SELECT string_agg(attname || ':' || attidentity::text || ':' || \
                 attnotnull::text || ':' || format_type(atttypid, atttypmod) || ':' || \
                 coalesce(pg_get_expr(d.adbin, d.adrelid), '') || ':' || \
                 coalesce(co.collname::text, ''), ',' ORDER BY attnum) \
                 FROM pg_attribute a \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 LEFT JOIN pg_collation co ON co.oid = a.attcollation \
                   AND a.attcollation <> 100 \
                 WHERE attrelid = '{sch}.orders2'::regclass AND attnum > 0"
            ))
            .await,
            "id::true:bigint::,qty::false:bigint:7:,total::false:numeric(12,2)::,\
             note::false:text::,customer_id:d:true:bigint::,\
             sku::false:character varying(40)::C",
            "{t:?}: {sql:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn create_table_full_pg() {
    require_db!();
    each_target("dnt_render_pg_create", |c| async move {
        let (t, sch) = (c.t, c.sch);
        let mut def = c.create().await;
        def.original_name = None;
        def.name = "orders_new".into();
        for col in &mut def.columns {
            col.original = None;
        }
        def.columns[1].comment = Some("Số lượng".into());
        def.columns[5].collation = Some("C".into());
        def.columns.push(ColumnDef {
            original: None,
            default: Some(DefaultSpec::Expression {
                text: "now()".into(),
            }),
            ..col("created_at", ty("timestamptz", None, Some(3), None), false)
        });
        let pk = def.primary_key.as_mut().unwrap();
        pk.original = None;
        pk.name = Some("orders_new_pkey".into());
        let fk = &mut def.foreign_keys[0];
        fk.original = None;
        fk.name = "fk_new_customer".into();
        fk.on_delete = "CASCADE".into();
        fk.match_type = Some("FULL".into());
        fk.deferrable = true;
        fk.initially_deferred = true;
        def.indexes = vec![
            IndexDef {
                original: None,
                include: vec!["note".into()],
                ..index("uq_new_code", true, true, &["code"])
            },
            IndexDef {
                original: None,
                include: vec!["total".into()],
                comment: Some("lookup".into()),
                parts: vec![
                    part("customer_id"),
                    IndexPartDef {
                        descending: true,
                        ..part("created_at")
                    },
                ],
                ..index("ix_new_customer", false, false, &[])
            },
        ];
        def.checks.push(CheckDef {
            original: None,
            name: "ck_new_qty".into(),
            expression: "qty >= 0".into(),
            locked: false,
        });
        def.comment = Some("Đơn hàng".into());
        let r = render_create(
            &c.table("orders_new"),
            &def,
            c.s.server_info(),
            c.s.dialect(),
        )
        .expect("render_create");
        assert_eq!(r.statements.len(), 5, "{t:?}: {:?}", r.statements);
        c.run(&r.statements).await;

        let rel = format!("'{sch}.orders_new'::regclass");
        assert_eq!(
            c.text(&format!(
                "SELECT string_agg(attname || ':' || attidentity::text || ':' || \
                 attnotnull::text || ':' || coalesce(co.collname::text, ''), ',' \
                 ORDER BY attnum) FROM pg_attribute a \
                 LEFT JOIN pg_collation co ON co.oid = a.attcollation \
                   AND a.attcollation <> 100 \
                 WHERE attrelid = {rel} AND attnum > 0"
            ))
            .await,
            "id:a:true:,qty::true:,total::false:,note::false:,customer_id::false:,\
             code::false:C,created_at::true:",
            "{t:?}"
        );
        assert_eq!(
            c.constraint_def("orders_new_pkey").await,
            "p PRIMARY KEY (id)",
            "{t:?}"
        );
        assert_eq!(
            c.constraint_def("uq_new_code").await,
            "u UNIQUE (code) INCLUDE (note)",
            "{t:?}"
        );
        assert_eq!(
            c.constraint_def("fk_new_customer").await,
            format!(
                "f FOREIGN KEY (customer_id) REFERENCES {sch}.customers(id) MATCH FULL \
                 ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED"
            ),
            "{t:?}"
        );
        assert_eq!(
            c.constraint_def("ck_new_qty").await,
            "c CHECK ((qty >= 0))",
            "{t:?}"
        );
        assert_eq!(
            c.index_def("ix_new_customer").await,
            format!(
                "CREATE INDEX ix_new_customer ON {sch}.orders_new USING btree \
                 (customer_id, created_at DESC) INCLUDE (total)"
            ),
            "{t:?}"
        );
        assert_eq!(
            c.text(&format!(
                "SELECT obj_description({rel}, 'pg_class') || '|' || \
                 col_description({rel}, 2) || '|' || \
                 obj_description('{sch}.ix_new_customer'::regclass, 'pg_class')"
            ))
            .await,
            "Đơn hàng|Số lượng|lookup",
            "{t:?}"
        );
    })
    .await;
}
