//! The dump job against an in-process fake driver: every statement the job sends is
//! recorded, results are programmed per statement text (and first parameter), and a
//! statement can fail or take a while.
use super::dump::{normalise, PG_OWNED_SEQUENCES, PG_TABLE_KINDS, PG_TABLE_SEQUENCES};
use super::*;
use crate::jobs::{JobDetail, JobState};
use crate::session_manager::SessionManager;
use async_trait::async_trait;
use dnt_core::config::ConnectionConfig;
use dnt_core::driver::{
    CancelHandle, Canceller, CellMode, Driver, ExecOptions, Introspector, ResultSummary, RowSink,
    Session, StmtOutcome,
};
use dnt_core::error::DbError;
use dnt_core::meta::{
    ColumnDetail, DatabaseInfo, ForeignKeyDetail, IdentityKind, IndexDetail, ObjectKind, ObjectRef,
    ObjectSummary, SchemaInfo, Scope, TableColumns, TableDetail, TableOptions,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{DbKind, ServerInfo, ServerVariant, SqlKind};
use dnt_core::sql::{leading_blank_len, split, Dialect, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, FullValue, LogicalType, Value};
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_transfer::dump::DUMP_MAGIC;
use dnt_tunnel::KnownHosts;
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------
// Fake server
// ---------------------------------------------------------------------------------------

/// The result of a statement containing `contains` (and, when set, whose first parameter
/// is `param`).
struct Resp {
    contains: String,
    param: Option<String>,
    cols: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
}

#[derive(Default)]
struct Catalog {
    /// `(schema, kind)` → names.
    objects: HashMap<(Option<String>, ObjectKind), Vec<String>>,
    /// `(schema, name)` → detail.
    details: HashMap<(Option<String>, String), TableDetail>,
    /// `(schema, name, kind)` → DDL.
    ddl: HashMap<(Option<String>, String, ObjectKind), String>,
}

struct Fake {
    server: ServerInfo,
    catalog: Mutex<Catalog>,
    responses: Mutex<Vec<Resp>>,
    seen: Mutex<Vec<String>>,
    /// A statement containing the text fails with the error.
    fail: Mutex<Option<(String, DbError)>>,
    /// A statement containing the text takes this long.
    delay: Mutex<Option<(String, Duration)>>,
    /// Cancels sent through a session's cancel handle.
    cancels: AtomicUsize,
    /// `batch_size` of every read of complete values.
    full_batches: Mutex<Vec<usize>>,
}

impl Fake {
    fn sent(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    fn respond(
        &self,
        contains: &str,
        param: Option<&str>,
        cols: Vec<ColumnMeta>,
        rows: Vec<Vec<Value>>,
    ) {
        self.responses.lock().unwrap().push(Resp {
            contains: contains.into(),
            param: param.map(str::to_string),
            cols,
            rows,
        });
    }

    fn add_object(&self, schema: Option<&str>, kind: ObjectKind, name: &str) {
        self.catalog
            .lock()
            .unwrap()
            .objects
            .entry((schema.map(str::to_string), kind))
            .or_default()
            .push(name.into());
    }

    fn add_ddl(&self, schema: Option<&str>, kind: ObjectKind, name: &str, ddl: &str) {
        self.add_object(schema, kind, name);
        self.catalog
            .lock()
            .unwrap()
            .ddl
            .insert((schema.map(str::to_string), name.into(), kind), ddl.into());
    }

    fn add_table(&self, detail: TableDetail, ddl: Option<&str>) {
        let schema = detail.object.schema.clone();
        let name = detail.object.name.clone();
        match ddl {
            Some(ddl) => self.add_ddl(schema.as_deref(), ObjectKind::Table, &name, ddl),
            None => self.add_object(schema.as_deref(), ObjectKind::Table, &name),
        }
        self.catalog
            .lock()
            .unwrap()
            .details
            .insert((schema, name), detail);
    }
}

struct FakeDriver(Arc<Fake>);

#[async_trait]
impl Driver for FakeDriver {
    fn kind(&self) -> DbKind {
        self.0.server.kind
    }
    async fn connect(
        &self,
        _cfg: &dnt_core::config::ResolvedConfig,
    ) -> Result<Box<dyn Session>, DbError> {
        Ok(Box::new(FakeSession {
            fake: Arc::clone(&self.0),
            tx: Mutex::new(TxState::Idle),
        }))
    }
}

struct FakeSession {
    fake: Arc<Fake>,
    tx: Mutex<TxState>,
}

struct CountCancel(Arc<Fake>);

#[async_trait]
impl Canceller for CountCancel {
    async fn cancel(&self) -> Result<(), DbError> {
        self.0.cancels.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn meta(name: &str, logical: LogicalType) -> ColumnMeta {
    typed_meta(name, logical, "")
}

/// A column as the driver describes it, with its type name (`int4range[]`).
fn typed_meta(name: &str, logical: LogicalType, db_type: &str) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        db_type: db_type.into(),
        logical,
        nullable: Some(true),
        origin_table: None,
        origin_column: None,
    }
}

#[async_trait]
impl Session for FakeSession {
    fn server_info(&self) -> &ServerInfo {
        &self.fake.server
    }
    async fn execute_one(
        &self,
        stmt: &str,
        _kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        if opts.stop.is_stopped() {
            return Err(DbError::Cancelled {
                outcome_unknown: false,
                was_in_tx: false,
            });
        }
        self.fake.seen.lock().unwrap().push(stmt.to_string());
        if matches!(opts.cells, CellMode::Full { .. }) {
            self.fake.full_batches.lock().unwrap().push(opts.batch_size);
        }
        let delay = self.fake.delay.lock().unwrap().clone();
        if let Some((_, d)) = delay.filter(|(text, _)| stmt.contains(text.as_str())) {
            tokio::time::sleep(d).await;
        }
        let fail = self.fake.fail.lock().unwrap().clone();
        if let Some((_, e)) = fail.filter(|(text, _)| stmt.contains(text.as_str())) {
            return Err(e);
        }
        let upper = stmt.to_ascii_uppercase();
        if upper.starts_with("BEGIN") || upper.starts_with("START TRANSACTION") {
            *self.tx.lock().unwrap() = TxState::Active;
        } else if upper == "COMMIT" || upper == "ROLLBACK" {
            *self.tx.lock().unwrap() = TxState::Idle;
        }
        let summary = ResultSummary {
            affected: None,
            row_count: 0,
            truncated: false,
            elapsed_ms: 0,
        };
        let found = {
            let responses = self.fake.responses.lock().unwrap();
            responses
                .iter()
                .find(|r| {
                    stmt.contains(&r.contains)
                        && r.param.as_ref().is_none_or(|p| opts.params.join(".") == *p)
                })
                .map(|r| (r.cols.clone(), r.rows.clone()))
        };
        if let Some((cols, rows)) = found {
            sink.result_start(&cols);
            match opts.cells {
                CellMode::Full { .. } => sink.full_rows(
                    rows.into_iter()
                        .map(|row| {
                            row.into_iter()
                                .map(|v| FullValue::try_from_value(v).unwrap())
                                .collect()
                        })
                        .collect(),
                ),
                CellMode::Preview => sink.rows(rows),
            }
        } else if stmt.starts_with("SELECT") {
            sink.result_start(&[]);
        }
        sink.result_end(&summary);
        Ok(StmtOutcome {
            results: vec![summary],
            late_completion: false,
        })
    }
    fn tx_state(&self) -> TxState {
        *self.tx.lock().unwrap()
    }
    fn dialect(&self) -> Dialect {
        Dialect::for_kind(self.fake.server.kind.as_sql())
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(CountCancel(Arc::clone(&self.fake)))
    }
    fn introspector(&self) -> &dyn Introspector {
        self
    }
    async fn ping(&self) -> Result<(), DbError> {
        Ok(())
    }
    async fn close(self: Box<Self>) {}
}

#[async_trait]
impl Introspector for FakeSession {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        Ok(vec![])
    }
    async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        Ok(vec![])
    }
    async fn list_objects(
        &self,
        scope: &Scope,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        let cat = self.fake.catalog.lock().unwrap();
        let mut names = cat
            .objects
            .get(&(scope.schema.clone(), kind))
            .cloned()
            .unwrap_or_default();
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| ObjectSummary {
                name,
                kind,
                comment: None,
                row_estimate: None,
            })
            .collect())
    }
    async fn list_columns(&self, _scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        Ok(vec![])
    }
    async fn table_detail(&self, obj: &ObjectRef) -> Result<TableDetail, DbError> {
        self.fake
            .catalog
            .lock()
            .unwrap()
            .details
            .get(&(obj.schema.clone(), obj.name.clone()))
            .cloned()
            .ok_or(DbError::NotOpen)
    }
    async fn object_ddl(&self, obj: &ObjectRef) -> Result<String, DbError> {
        self.fake
            .catalog
            .lock()
            .unwrap()
            .ddl
            .get(&(obj.schema.clone(), obj.name.clone(), obj.kind))
            .cloned()
            .ok_or(DbError::NotOpen)
    }
}

// ---------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------

fn column(name: &str, ordinal: u32, db_type: &str) -> ColumnDetail {
    ColumnDetail {
        name: name.into(),
        ordinal,
        db_type: db_type.into(),
        nullable: true,
        ..ColumnDetail::default()
    }
}

fn detail(schema: Option<&str>, name: &str, columns: Vec<ColumnDetail>) -> TableDetail {
    TableDetail {
        object: ObjectRef {
            database: "shop".into(),
            schema: schema.map(str::to_string),
            name: name.into(),
            kind: ObjectKind::Table,
        },
        columns,
        primary_key: None,
        indexes: vec![],
        foreign_keys: vec![],
        checks: vec![],
        triggers: vec![],
        options: TableOptions::default(),
        row_estimate: Some(2),
        size_bytes: None,
        comment: None,
    }
}

fn fk(
    name: &str,
    col: &str,
    ref_schema: Option<&str>,
    table: &str,
    definition: Option<&str>,
) -> ForeignKeyDetail {
    ForeignKeyDetail {
        name: name.into(),
        columns: vec![col.into()],
        ref_schema: ref_schema.map(str::to_string),
        ref_table: table.into(),
        ref_columns: vec!["id".into()],
        on_update: "NO ACTION".into(),
        on_delete: "NO ACTION".into(),
        definition: definition.map(str::to_string),
        ..ForeignKeyDetail::default()
    }
}

fn int(n: i64) -> Value {
    Value::Int(n)
}

fn txt(s: &str) -> Value {
    Value::Text(s.into())
}

struct Fx {
    sm: Arc<SessionManager>,
    fake: Arc<Fake>,
    conn: Uuid,
    dir: tempfile::TempDir,
}

async fn fixture(kind: DbKind, read_only: bool) -> Fx {
    let server = match kind {
        DbKind::MySql => ServerInfo::new(kind, ServerVariant::MySql8, (8, 0, 36), "8.0.36"),
        DbKind::Postgres => ServerInfo::new(kind, ServerVariant::Postgres, (17, 0, 0), "17.0"),
    };
    let fake = Arc::new(Fake {
        server,
        catalog: Mutex::new(Catalog::default()),
        responses: Mutex::new(Vec::new()),
        seen: Mutex::new(Vec::new()),
        fail: Mutex::new(None),
        delay: Mutex::new(None),
        cancels: AtomicUsize::new(0),
        full_batches: Mutex::new(Vec::new()),
    });
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let driver: Arc<dyn Driver> = Arc::new(FakeDriver(Arc::clone(&fake)));
    let dir = tempfile::tempdir().unwrap();
    let sm = SessionManager::new(
        Arc::clone(&repo),
        secrets,
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(kind, driver)]),
    );
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind,
        default_db: None,
        read_only,
        ..sample_config()
    };
    let conn = repo
        .create(
            cfg,
            NewSecrets {
                db_password: None,
                ssh_password: None,
                ssh_key_passphrase: None,
            },
        )
        .unwrap()
        .id;
    let sm = Arc::new(sm);
    sm.open(conn, None).await.unwrap();
    Fx {
        sm,
        fake,
        conn,
        dir,
    }
}

/// MySQL `shop`: `customers (id, name)`, `orders (id, customer_id → customers, tax
/// generated)`, views `v_orders` (uses `v_base`) and `v_base`, plus a trigger, a
/// procedure and an event (skipped).
async fn mysql_shop(read_only: bool) -> Fx {
    let fx = fixture(DbKind::MySql, read_only).await;
    let f = &fx.fake;
    let mut orders = detail(
        None,
        "orders",
        vec![
            column("id", 1, "int"),
            column("customer_id", 2, "int"),
            ColumnDetail {
                generated: Some("`id` * 2".into()),
                ..column("tax", 3, "int")
            },
        ],
    );
    orders.foreign_keys = vec![
        fk("orders_customer", "customer_id", None, "customers", None),
        fk(
            "orders_account",
            "customer_id",
            Some("billing"),
            "accounts",
            None,
        ),
    ];
    f.add_table(
        orders,
        Some(
            "CREATE TABLE `orders` (\n  `id` int NOT NULL,\n  `customer_id` int,\n  \
             `tax` int GENERATED ALWAYS AS ((`id` * 2)) VIRTUAL,\n  PRIMARY KEY (`id`),\n  \
             CONSTRAINT `orders_customer` FOREIGN KEY (`customer_id`) REFERENCES `customers` (`id`)\n\
             ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='a; b'",
        ),
    );
    f.add_table(
        detail(
            None,
            "customers",
            vec![
                column("id", 1, "int"),
                column("name", 2, "varchar(20)"),
                column("pt", 3, "point"),
            ],
        ),
        Some(
            "CREATE TABLE `customers` (\n  `id` int NOT NULL,\n  `name` varchar(20),\n  \
             `pt` point,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB",
        ),
    );
    f.add_ddl(
        None,
        ObjectKind::View,
        "v_orders",
        "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v_orders` AS \
         select `shop`.`v_base`.`id` AS `id` from `shop`.`v_base`",
    );
    f.add_ddl(
        None,
        ObjectKind::View,
        "v_base",
        "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v_base` AS \
         select `shop`.`orders`.`id` AS `id`, '`shop`.x' AS `s` from `shop`.`orders`",
    );
    f.add_object(None, ObjectKind::Trigger, "orders_bi");
    f.add_object(None, ObjectKind::Procedure, "refill");
    f.add_object(None, ObjectKind::Event, "nightly");
    // The spatial column is read as its bytes.
    f.respond(
        "CAST(`pt` AS BINARY)",
        None,
        vec![
            meta("id", LogicalType::Integer),
            meta("name", LogicalType::Text),
            meta("pt", LogicalType::Binary),
        ],
        vec![
            vec![
                int(1),
                txt("Ann"),
                Value::Bytes {
                    len: 3,
                    preview: vec![0, 1, 0xff],
                },
            ],
            vec![int(2), txt("Bo'b; --"), Value::Null],
        ],
    );
    f.respond(
        "FROM `shop`.`customers`",
        None,
        vec![
            meta("id", LogicalType::Integer),
            meta("name", LogicalType::Text),
            meta("pt", LogicalType::Other),
        ],
        vec![],
    );
    f.respond(
        "VIEW_TABLE_USAGE",
        Some("shop"),
        vec![],
        vec![
            vec![txt("v_orders"), txt("shop"), txt("v_base")],
            vec![txt("v_base"), txt("shop"), txt("orders")],
            vec![txt("v_base"), txt("billing"), txt("accounts")],
        ],
    );
    f.respond(
        "FROM `shop`.`orders`",
        None,
        vec![
            meta("id", LogicalType::Integer),
            meta("customer_id", LogicalType::Integer),
        ],
        vec![vec![int(10), int(1)], vec![int(11), Value::Null]],
    );
    fx
}

/// PostgreSQL `shop`, schemas `public` and `sales`: enum `public.mood`, stand-alone
/// sequence `public.counter`, `public.customers (id identity ALWAYS, name, mood)`,
/// `sales.orders (id serial, customer_id → public.customers, tax generated)`, view
/// `public.v`; a partitioned table, a materialized view and a function (skipped).
async fn pg_shop() -> Fx {
    let fx = fixture(DbKind::Postgres, false).await;
    let f = &fx.fake;
    let p = Some("public");
    let s = Some("sales");
    f.add_ddl(
        p,
        ObjectKind::Type,
        "mood",
        "CREATE TYPE public.mood AS ENUM ('ok', 'sad');",
    );
    f.add_ddl(
        p,
        ObjectKind::Sequence,
        "counter",
        "CREATE SEQUENCE public.counter AS bigint INCREMENT BY 1 MINVALUE 1 MAXVALUE 99 START WITH 1 CACHE 1 NO CYCLE;",
    );
    // A column's own sequence: listed but not offered on its own.
    f.add_ddl(
        s,
        ObjectKind::Sequence,
        "orders_id_seq",
        "CREATE SEQUENCE sales.orders_id_seq AS integer INCREMENT BY 1 MINVALUE 1 MAXVALUE 2147483647 START WITH 1 CACHE 1 NO CYCLE;",
    );
    f.add_table(
        detail(
            p,
            "customers",
            vec![
                ColumnDetail {
                    identity: Some("ALWAYS".into()),
                    identity_kind: Some(IdentityKind::Always),
                    nullable: false,
                    ..column("id", 1, "integer")
                },
                column("name", 2, "text"),
                column("mood", 3, "public.mood"),
                column("span", 4, "int4range"),
                column("spans", 5, "int4range[]"),
            ],
        ),
        None,
    );
    // A domain (listed and defined by the dump's own catalog queries).
    f.respond(
        "t.typtype = 'd'",
        Some("public"),
        vec![],
        vec![vec![txt("pos_int")]],
    );
    f.respond(
        "pg_get_constraintdef(k.oid)",
        Some("public.pos_int"),
        vec![],
        vec![vec![
            txt("integer"),
            Value::Null,
            Value::Null,
            txt("1"),
            Value::Bool(true),
            txt("CONSTRAINT pos CHECK ((VALUE > 0))"),
        ]],
    );
    // The types the customers' columns use (one from a schema not dumped).
    f.respond(
        "WITH RECURSIVE u(oid)",
        Some("\"public\".\"customers\""),
        vec![],
        vec![
            vec![txt("ext"), txt("money2"), txt("d")],
            vec![txt("public"), txt("mood"), txt("e")],
            vec![txt("public"), txt("pos_int"), txt("d")],
        ],
    );
    f.respond(
        "a.attcollation <> t.typcollation",
        Some("\"public\".\"customers\""),
        vec![],
        vec![vec![txt("name"), txt("pg_catalog"), txt("C")]],
    );
    // A descending identity.
    f.respond(
        "s.seqrelid = $1::regclass",
        Some("\"public\".\"customers_id_seq\""),
        vec![],
        vec![vec![
            int(-1),
            int(-1),
            int(-2147483648),
            int(-1),
            int(1),
            Value::Bool(false),
        ]],
    );
    f.respond(
        "pg_rewrite",
        Some("\"public\".\"v\""),
        vec![],
        vec![
            vec![txt("public"), txt("customers")],
            vec![txt("public"), txt("elsewhere")],
        ],
    );
    let mut orders = detail(
        s,
        "orders",
        vec![
            ColumnDetail {
                default: Some("nextval('sales.orders_id_seq'::regclass)".into()),
                nullable: false,
                ..column("id", 1, "integer")
            },
            column("customer_id", 2, "integer"),
            ColumnDetail {
                generated: Some("(id * 2)".into()),
                ..column("tax", 3, "integer")
            },
        ],
    );
    orders.indexes = vec![IndexDetail {
        name: "orders_customer_idx".into(),
        definition: Some(
            "CREATE INDEX orders_customer_idx ON sales.orders USING btree (customer_id)".into(),
        ),
        ..IndexDetail::default()
    }];
    orders.foreign_keys = vec![fk(
        "orders_customer_fk",
        "customer_id",
        Some("public"),
        "customers",
        Some("FOREIGN KEY (customer_id) REFERENCES public.customers(id)"),
    )];
    f.add_table(orders, None);
    let mut part = detail(p, "events", vec![column("at", 1, "date")]);
    part.options.partitioned_by = Some("RANGE (at)".into());
    f.add_table(part, None);
    f.add_ddl(
        p,
        ObjectKind::View,
        "v",
        "CREATE OR REPLACE VIEW public.v AS\n SELECT customers.name FROM public.customers;",
    );
    f.add_object(p, ObjectKind::MaterializedView, "mv");
    f.add_object(s, ObjectKind::Function, "total");

    f.respond(
        "c.relispartition",
        Some("public"),
        vec![],
        vec![
            vec![
                txt("customers"),
                txt("r"),
                Value::Bool(false),
                Value::Bool(false),
            ],
            vec![
                txt("events"),
                txt("p"),
                Value::Bool(false),
                Value::Bool(false),
            ],
        ],
    );
    f.respond(
        "c.relispartition",
        Some("sales"),
        vec![],
        vec![vec![
            txt("orders"),
            txt("r"),
            Value::Bool(false),
            Value::Bool(false),
        ]],
    );
    f.respond(
        "SELECT DISTINCT c.relname::text",
        Some("sales"),
        vec![],
        vec![vec![txt("orders_id_seq")]],
    );
    f.respond(
        "pg_attrdef",
        Some("\"public\".\"customers\""),
        vec![],
        vec![vec![
            txt("public"),
            txt("customers_id_seq"),
            txt("id"),
            txt("i"),
        ]],
    );
    f.respond(
        "pg_attrdef",
        Some("\"sales\".\"orders\""),
        vec![],
        vec![
            vec![txt("sales"), txt("orders_id_seq"), txt("id"), txt("a")],
            vec![txt("sales"), txt("orders_id_seq"), Value::Null, Value::Null],
        ],
    );
    f.respond(
        "FROM \"public\".\"counter\"",
        None,
        vec![],
        vec![vec![int(5), Value::Bool(true)]],
    );
    f.respond(
        "FROM \"sales\".\"orders_id_seq\"",
        None,
        vec![],
        vec![vec![int(11), Value::Bool(true)]],
    );
    f.respond(
        "FROM \"public\".\"customers_id_seq\"",
        None,
        vec![],
        vec![vec![int(2), Value::Bool(false)]],
    );
    f.respond(
        "FROM ONLY \"public\".\"customers\"",
        None,
        vec![
            meta("id", LogicalType::Integer),
            meta("name", LogicalType::Text),
            meta("mood", LogicalType::Enum),
            typed_meta("span", LogicalType::Other, "int4range"),
            typed_meta("spans", LogicalType::Array, "int4range[]"),
        ],
        // `span` and `spans` arrive as the server's text (`::text`).
        vec![
            vec![
                int(1),
                txt("Ann"),
                txt("ok"),
                txt("[1,5)"),
                txt("{\"[1,5)\",\"[7,9)\"}"),
            ],
            vec![int(2), Value::Null, txt("sad"), Value::Null, Value::Null],
        ],
    );
    f.respond(
        "FROM ONLY \"sales\".\"orders\"",
        None,
        vec![
            meta("id", LogicalType::Integer),
            meta("customer_id", LogicalType::Integer),
        ],
        vec![vec![int(10), int(1)], vec![int(11), int(2)]],
    );
    fx
}

impl Fx {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn request(&self, objects: Vec<DumpObject>, schemas: &[&str], file: &str) -> DumpRequest {
        DumpRequest {
            connection_id: self.conn,
            database: "shop".into(),
            schemas: schemas.iter().map(|s| s.to_string()).collect(),
            objects,
            include_drop: false,
            compress: false,
            path: self.path(file).to_string_lossy().into_owned(),
        }
    }

    async fn all_objects(&self, schemas: &[&str]) -> DumpObjects {
        self.sm
            .dump_objects(
                self.conn,
                "shop".into(),
                schemas.iter().map(|s| s.to_string()).collect(),
            )
            .await
            .unwrap()
    }

    async fn run(&self, req: DumpRequest) -> JobDetail {
        let id = self.sm.dump_start(req).await.unwrap();
        assert!(self.sm.jobs.wait(id, Duration::from_secs(10)).await);
        self.sm.jobs.detail(id).unwrap()
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "known_hosts")
            .collect();
        names.sort();
        names
    }
}

fn log_text(d: &JobDetail) -> String {
    d.log
        .iter()
        .map(|l| l.message.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The statements of a dump as the app runner reads them: `split` with the default
/// dialect, the comments leading a statement left out.
fn statements(text: &str, d: Dialect) -> Vec<String> {
    split(text, d)
        .into_iter()
        .map(|s| s.text[leading_blank_len(&s.text, d)..].to_string())
        .collect()
}

/// `text` holds exactly `stmts`, each ending with `;\n`.
fn assert_terminated(text: &str, stmts: &[String]) {
    for s in stmts {
        assert!(text.contains(&format!("{s};\n")), "not terminated: {s}");
    }
    let count = format!("-- End of dump: {} statements\n", stmts.len());
    assert!(text.ends_with(&count), "{text}");
}

/// What the dump job sent after `mark` (the background metadata reads left out).
fn job_sent(fx: &Fx, mark: usize) -> Vec<String> {
    fx.fake.sent()[mark..]
        .iter()
        .filter(|s| !s.starts_with("SELECT unnest"))
        .cloned()
        .collect()
}

fn index_of(stmts: &[String], prefix: &str) -> usize {
    stmts
        .iter()
        .position(|s| s.starts_with(prefix))
        .unwrap_or_else(|| panic!("no statement starts with {prefix}: {stmts:#?}"))
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn mysql_objects_list_what_is_skipped() {
    let fx = mysql_shop(false).await;
    let listed = fx.all_objects(&[]).await;
    let names: Vec<(ObjectKind, &str, bool)> = listed
        .objects
        .iter()
        .map(|o| (o.kind, o.name.as_str(), o.data))
        .collect();
    assert_eq!(
        names,
        [
            (ObjectKind::Table, "customers", true),
            (ObjectKind::Table, "orders", true),
            (ObjectKind::View, "v_base", false),
            (ObjectKind::View, "v_orders", false),
        ]
    );
    let skipped: Vec<(ObjectKind, &str)> = listed
        .skipped
        .iter()
        .map(|s| (s.kind, s.name.as_str()))
        .collect();
    assert_eq!(
        skipped,
        [
            (ObjectKind::Procedure, "refill"),
            (ObjectKind::Trigger, "orders_bi"),
            (ObjectKind::Event, "nightly"),
        ]
    );
    assert!(listed
        .skipped
        .iter()
        .all(|s| s.reason.contains("external tool")));
}

/// Read-only connections may dump (a read). Order: session statements, tables in FK
/// order, data, views in dependency order; generated columns omitted; views portable.
#[tokio::test]
async fn mysql_dump_order_and_content() {
    let fx = mysql_shop(true).await;
    let listed = fx.all_objects(&[]).await;
    let mark = fx.fake.sent().len();
    let d = fx.run(fx.request(listed.objects, &[], "shop.sql")).await;
    assert_eq!(
        d.job.state,
        JobState::Done,
        "{:?} {}",
        d.job.error,
        log_text(&d)
    );
    assert_eq!(fx.files(), ["shop.sql"]);
    let path = fx.path("shop.sql");
    assert_eq!(
        d.job.result_path.as_deref(),
        Some(path.to_string_lossy().as_ref())
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with(&format!(
        "{DUMP_MAGIC}\n-- Source: MySQL 8.0.36, database shop\n-- Created: "
    )));
    assert!(
        text.contains("\n-- Objects: 2 tables (2 with data), 2 views\n"),
        "{text}"
    );
    let stmts = statements(&text, Dialect::MYSQL);
    assert_terminated(&text, &stmts);
    let shapes: Vec<String> = stmts
        .iter()
        .map(|s| s.lines().next().unwrap().chars().take(40).collect())
        .collect();
    assert_eq!(
        shapes,
        [
            "SET NAMES utf8mb4",
            "SET SESSION sql_mode = 'NO_AUTO_VALUE_ON",
            "SET time_zone = '+00:00'",
            "SET FOREIGN_KEY_CHECKS = 0",
            "CREATE TABLE `customers` (",
            "CREATE TABLE `orders` (",
            "INSERT INTO `customers` (`id`, `name`, `",
            "INSERT INTO `orders` (`id`, `customer_id",
            "CREATE ALGORITHM=UNDEFINED SQL SECURITY ",
            "CREATE ALGORITHM=UNDEFINED SQL SECURITY ",
            "SET FOREIGN_KEY_CHECKS = 1",
        ]
    );
    assert_eq!(
        stmts[6],
        "INSERT INTO `customers` (`id`, `name`, `pt`) VALUES (1, _utf8mb4'Ann', X'0001ff'), \
         (2, _utf8mb4'Bo\\'b; --', NULL)"
    );
    assert_eq!(
        stmts[7],
        "INSERT INTO `orders` (`id`, `customer_id`) VALUES (10, 1), (11, NULL)"
    );
    assert_eq!(
        stmts[8],
        "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v_base` AS \
         select `orders`.`id` AS `id`, '`shop`.x' AS `s` from `orders`"
    );
    assert_eq!(
        stmts[9],
        "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v_orders` AS \
         select `v_base`.`id` AS `id` from `v_base`"
    );
    // The job's session: snapshot first, reads inside it, then COMMIT.
    let sent = job_sent(&fx, mark);
    let at = |prefix: &str| index_of(&sent, prefix);
    assert_eq!(
        at("SET SESSION sql_mode = 'NO_AUTO_VALUE_ON_ZERO'"),
        0,
        "{sent:#?}"
    );
    assert_eq!(at("SET time_zone = '+00:00'"), 1);
    assert_eq!(
        at("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ"),
        2
    );
    assert_eq!(
        at("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY"),
        3
    );
    // The driver's types first (`WHERE 1 = 0`), then the rows; spatial values as bytes,
    // a few rows per batch.
    let probe = at("SELECT `id`, `name`, `pt` FROM `shop`.`customers` WHERE 1 = 0");
    assert!(probe > 3);
    assert!(at("SELECT `id`, `name`, CAST(`pt` AS BINARY) FROM `shop`.`customers`") > probe);
    assert_eq!(sent.last().map(String::as_str), Some("COMMIT"));
    assert!(sent.iter().all(|s| !s.starts_with("INSERT")), "{sent:#?}");
    // Skipped objects are listed in the log (R22), and what is outside the dump.
    let log = log_text(&d);
    assert!(
        log.contains("Foreign key orders_account of orders references billing.accounts"),
        "{log}"
    );
    assert!(
        log.contains("View v_base reads billing.accounts, which is not in the dump"),
        "{log}"
    );
    assert!(!log.contains("View v_orders reads"), "{log}");
    let batches = fx.fake.full_batches.lock().unwrap().clone();
    assert_eq!(batches, [16, 16], "few rows per batch");
    assert!(log.contains("Skipped trigger orders_bi: Triggers"), "{log}");
    assert!(log.contains("Skipped event nightly"), "{log}");
    assert!(log.contains("Skipped procedure refill"), "{log}");
}

#[tokio::test]
async fn mysql_drop_statements_and_selection() {
    let fx = mysql_shop(false).await;
    let objects = vec![DumpObject {
        schema: None,
        name: "orders".into(),
        kind: ObjectKind::Table,
        data: false,
    }];
    let mut req = fx.request(objects, &[], "orders.sql");
    req.include_drop = true;
    let d = fx.run(req).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job.error);
    let text = std::fs::read_to_string(fx.path("orders.sql")).unwrap();
    assert!(
        text.contains("-- Objects: 1 tables (0 with data), 0 views\n"),
        "{text}"
    );
    let stmts = statements(&text, Dialect::MYSQL);
    assert_eq!(stmts[4], "DROP TABLE IF EXISTS `orders`");
    assert!(stmts[5].starts_with("CREATE TABLE `orders`"));
    assert!(!text.contains("INSERT"), "{text}");
    assert!(fx
        .fake
        .sent()
        .iter()
        .all(|s| !s.contains("FROM `shop`.`orders`")));
}

#[tokio::test]
async fn pg_objects_list_what_is_skipped() {
    let fx = pg_shop().await;
    let listed = fx.all_objects(&["public", "sales"]).await;
    let names: Vec<String> = listed
        .objects
        .iter()
        .map(|o| format!("{:?} {}.{}", o.kind, o.schema.as_deref().unwrap(), o.name))
        .collect();
    assert_eq!(
        names,
        [
            "Table public.customers",
            "View public.v",
            "Sequence public.counter",
            "Type public.mood",
            "Type public.pos_int",
            "Table sales.orders",
        ]
    );
    let skipped: Vec<String> = listed
        .skipped
        .iter()
        .map(|s| format!("{:?} {}", s.kind, s.name))
        .collect();
    assert_eq!(
        skipped,
        ["Table events", "MaterializedView mv", "Function total"]
    );
    assert!(listed.skipped[0].reason.starts_with("Partitioned tables"));
    // The listing queries take the schema as a parameter.
    let sent = fx.fake.sent();
    assert!(sent.iter().any(|s| s == PG_TABLE_KINDS));
    assert!(sent.iter().any(|s| s == PG_OWNED_SEQUENCES));
}

/// Order: types → sequences → tables (no FKs) → data → setval → FKs → views; generated
/// columns omitted; identity ALWAYS gets OVERRIDING SYSTEM VALUE.
#[tokio::test]
async fn pg_dump_order_and_content() {
    let fx = pg_shop().await;
    let listed = fx.all_objects(&["public", "sales"]).await;
    let mark = fx.fake.sent().len();
    let d = fx
        .run(fx.request(listed.objects, &["public", "sales"], "shop.sql"))
        .await;
    assert_eq!(
        d.job.state,
        JobState::Done,
        "{:?} {}",
        d.job.error,
        log_text(&d)
    );
    let text = std::fs::read_to_string(fx.path("shop.sql")).unwrap();
    assert!(text.starts_with(&format!(
        "{DUMP_MAGIC}\n-- Source: PostgreSQL 17.0, database shop, schemas public, sales\n"
    )));
    assert!(
        text.contains("\n-- Objects: 2 tables (2 with data), 1 views, 2 sequences, 2 types\n"),
        "{text}"
    );
    let stmts = statements(&text, Dialect::Postgres);
    assert_terminated(&text, &stmts);
    let expected = [
        "SET client_encoding = 'UTF8'",
        "SET standard_conforming_strings = on",
        "CREATE SCHEMA IF NOT EXISTS \"sales\"",
        "CREATE TYPE public.mood AS ENUM ('ok', 'sad')",
        "CREATE DOMAIN \"public\".\"pos_int\" AS integer DEFAULT 1 NOT NULL CONSTRAINT pos CHECK ((VALUE > 0))",
        "CREATE SEQUENCE public.counter AS bigint",
        "SELECT pg_catalog.setval('\"public\".\"counter\"', 5, TRUE)",
        "CREATE SEQUENCE sales.orders_id_seq AS integer",
        "CREATE TABLE public.customers (",
        "CREATE TABLE sales.orders (",
        "CREATE INDEX orders_customer_idx ON sales.orders USING btree (customer_id)",
        "ALTER SEQUENCE \"sales\".\"orders_id_seq\" OWNED BY \"sales\".\"orders\".\"id\"",
        "INSERT INTO \"public\".\"customers\" (\"id\", \"name\", \"mood\", \"span\", \"spans\") OVERRIDING SYSTEM VALUE VALUES ",
        "INSERT INTO \"sales\".\"orders\" (\"id\", \"customer_id\") VALUES ",
        "SELECT pg_catalog.setval(pg_catalog.pg_get_serial_sequence('\"sales\".\"orders\"', 'id'), 11, TRUE)",
        "SELECT pg_catalog.setval(pg_catalog.pg_get_serial_sequence('\"public\".\"customers\"', 'id'), 2, FALSE)",
        "ALTER TABLE \"sales\".\"orders\" ADD CONSTRAINT \"orders_customer_fk\" FOREIGN KEY (customer_id) REFERENCES public.customers(id)",
        "CREATE OR REPLACE VIEW public.v AS",
    ];
    assert_eq!(stmts.len(), expected.len(), "{stmts:#?}");
    for (s, e) in stmts.iter().zip(expected) {
        assert!(s.starts_with(e), "{s}\n  should start with\n{e}");
    }
    // Types the driver cannot decode are read as the server's text and cast back.
    assert_eq!(
        stmts[12],
        "INSERT INTO \"public\".\"customers\" (\"id\", \"name\", \"mood\", \"span\", \"spans\") \
         OVERRIDING SYSTEM VALUE VALUES (1, 'Ann', 'ok'::public.mood, '[1,5)'::int4range, \
         '{\"[1,5)\",\"[7,9)\"}'::int4range[]), (2, NULL, 'sad'::public.mood, NULL, NULL)"
    );
    assert_eq!(
        stmts[13],
        "INSERT INTO \"sales\".\"orders\" (\"id\", \"customer_id\") VALUES (10, 1), (11, 2)"
    );
    // The identity keeps its sequence's name and options; a collation is kept.
    assert!(
        stmts[8].contains(
            "id integer GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME \"public\".\"customers_id_seq\" \
             START WITH -1 INCREMENT BY -1 MINVALUE -2147483648 MAXVALUE -1 CACHE 1 NO CYCLE) \
             NOT NULL"
        ),
        "{}",
        stmts[8]
    );
    assert!(
        stmts[8].contains("name text COLLATE \"pg_catalog\".\"C\""),
        "{}",
        stmts[8]
    );
    // Tables carry no foreign key; the generated column keeps its expression.
    assert!(!stmts[9].contains("FOREIGN KEY"), "{}", stmts[9]);
    assert!(
        stmts[9].contains("GENERATED ALWAYS AS ((id * 2)) STORED"),
        "{}",
        stmts[9]
    );
    // The job's session: row security off and an empty search_path before the snapshot;
    // data read with ONLY; sequence values after data.
    let sent = job_sent(&fx, mark);
    assert_eq!(sent[0], "SET row_security = off");
    assert_eq!(
        sent[1],
        "SELECT pg_catalog.set_config('search_path', '', false)"
    );
    assert_eq!(sent[2], "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY");
    assert!(sent.iter().any(|s| s == PG_TABLE_SEQUENCES));
    let at = |prefix: &str| index_of(&sent, prefix);
    assert!(
        at("SELECT \"id\", \"name\", \"mood\", \"span\"::text, \"spans\"::text FROM ONLY \"public\".\"customers\"")
            > at("SELECT \"id\", \"name\", \"mood\", \"span\", \"spans\" FROM ONLY \"public\".\"customers\" WHERE 1 = 0")
    );
    let log = log_text(&d);
    assert!(
        log.contains("Table public.customers uses type ext.money2, whose schema is not dumped"),
        "{log}"
    );
    assert!(
        log.contains("View public.v reads public.elsewhere, which is not in the dump"),
        "{log}"
    );
    assert!(
        at("SELECT last_value, is_called FROM \"sales\".\"orders_id_seq\"")
            > at("SELECT \"id\", \"customer_id\" FROM ONLY \"sales\".\"orders\"")
    );
    assert_eq!(sent.last().map(String::as_str), Some("COMMIT"));
    assert!(
        log.contains("Skipped table public.events: Partitioned"),
        "{log}"
    );
    assert!(log.contains("Skipped materialized view public.mv"), "{log}");
}

#[tokio::test]
async fn pg_compressed_dump_with_drops() {
    let fx = pg_shop().await;
    let listed = fx.all_objects(&["public", "sales"]).await;
    let mut req = fx.request(listed.objects, &["public", "sales"], "shop.sql.gz");
    req.compress = true;
    req.include_drop = true;
    let d = fx.run(req).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job.error);
    assert_eq!(fx.files(), ["shop.sql.gz"]);
    let bytes = std::fs::read(fx.path("shop.sql.gz")).unwrap();
    let mut text = String::new();
    flate2::read::MultiGzDecoder::new(&bytes[..])
        .read_to_string(&mut text)
        .unwrap();
    assert!(text.starts_with(DUMP_MAGIC));
    let stmts = statements(&text, Dialect::Postgres);
    assert_terminated(&text, &stmts);
    // Sequences and types are dropped without CASCADE (a column of a table outside the
    // dump must not go with them).
    assert_eq!(
        &stmts[2..9],
        [
            "DROP VIEW IF EXISTS \"public\".\"v\" CASCADE",
            "DROP TABLE IF EXISTS \"sales\".\"orders\" CASCADE",
            "DROP TABLE IF EXISTS \"public\".\"customers\" CASCADE",
            "DROP SEQUENCE IF EXISTS \"public\".\"counter\"",
            "DROP SEQUENCE IF EXISTS \"sales\".\"orders_id_seq\"",
            "DROP TYPE IF EXISTS \"public\".\"pos_int\"",
            "DROP TYPE IF EXISTS \"public\".\"mood\"",
        ]
    );
}

/// Only the tables asked for: the types their columns use come along (log line), a
/// sequence only a default uses is created if missing and never dropped.
#[tokio::test]
async fn pg_tables_bring_their_types_and_sequences() {
    let fx = pg_shop().await;
    fx.fake.respond(
        "pg_attrdef",
        Some("\"public\".\"customers\""),
        vec![],
        vec![vec![
            txt("public"),
            txt("counter"),
            Value::Null,
            Value::Null,
        ]],
    );
    // The earlier, identity-only answer for customers must not win.
    fx.fake.responses.lock().unwrap().retain(|r| {
        !(r.contains == "pg_attrdef"
            && r.param.as_deref() == Some("\"public\".\"customers\"")
            && r.rows.len() == 1
            && r.rows[0].get(3) == Some(&txt("i")))
    });
    let objects = vec![DumpObject {
        schema: Some("public".into()),
        name: "customers".into(),
        kind: ObjectKind::Table,
        data: false,
    }];
    let mut req = fx.request(objects, &["public"], "c.sql");
    req.include_drop = true;
    let d = fx.run(req).await;
    assert_eq!(
        d.job.state,
        JobState::Done,
        "{:?} {}",
        d.job.error,
        log_text(&d)
    );
    let log = log_text(&d);
    assert!(
        log.contains("Type public.mood is dumped too: table public.customers uses it."),
        "{log}"
    );
    assert!(log.contains("Type public.pos_int is dumped too"), "{log}");
    let text = std::fs::read_to_string(fx.path("c.sql")).unwrap();
    assert!(
        text.contains("-- Objects: 1 tables (0 with data), 0 views, 1 sequences, 2 types\n"),
        "{text}"
    );
    let stmts = statements(&text, Dialect::Postgres);
    assert!(
        !stmts.iter().any(|s| s.starts_with("DROP SEQUENCE")),
        "{stmts:#?}"
    );
    assert!(
        stmts
            .iter()
            .any(|s| s.starts_with("CREATE SEQUENCE IF NOT EXISTS public.counter")),
        "{stmts:#?}"
    );
    let mood = index_of(&stmts, "CREATE TYPE public.mood");
    let domain = index_of(&stmts, "CREATE DOMAIN \"public\".\"pos_int\"");
    assert!(mood < domain && domain < index_of(&stmts, "CREATE TABLE public.customers"));
}

/// A value that cannot be written fails the dump and cancels the running read.
#[tokio::test]
async fn an_unwritable_value_cancels_the_read() {
    let fx = pg_shop().await;
    fx.fake.responses.lock().unwrap().insert(
        0,
        Resp {
            contains: "\"span\"::text".into(),
            param: None,
            cols: vec![
                meta("id", LogicalType::Integer),
                meta("name", LogicalType::Text),
                meta("mood", LogicalType::Enum),
                meta("span", LogicalType::Text),
                meta("spans", LogicalType::Text),
            ],
            rows: vec![vec![
                int(1),
                txt("a"),
                txt("ok"),
                txt("nul\0here"),
                Value::Null,
            ]],
        },
    );
    let listed = fx.all_objects(&["public", "sales"]).await;
    let d = fx
        .run(fx.request(listed.objects, &["public", "sales"], "shop.sql"))
        .await;
    assert_eq!(d.job.state, JobState::Failed);
    assert!(d
        .job
        .summary
        .unwrap_or_default()
        .contains("public.customers cannot be dumped"),);
    assert!(fx.fake.cancels.load(Ordering::SeqCst) >= 1);
    assert!(fx.files().is_empty());
}

#[tokio::test]
async fn failure_removes_the_part_file() {
    let fx = mysql_shop(false).await;
    let listed = fx.all_objects(&[]).await;
    *fx.fake.fail.lock().unwrap() = Some((
        "FROM `shop`.`orders`".into(),
        DbError::Server {
            code: Some("1146".into()),
            sqlstate: None,
            message: "gone".into(),
            position: None,
        },
    ));
    let d = fx.run(fx.request(listed.objects, &[], "shop.sql")).await;
    assert_eq!(d.job.state, JobState::Failed);
    assert!(fx.files().is_empty(), "{:?}", fx.files());
    // The snapshot was rolled back.
    assert_eq!(
        job_sent(&fx, 0).last().map(String::as_str),
        Some("ROLLBACK")
    );
}

#[tokio::test]
async fn stop_removes_the_part_file() {
    let fx = mysql_shop(false).await;
    let listed = fx.all_objects(&[]).await;
    *fx.fake.delay.lock().unwrap() =
        Some(("FROM `shop`.`customers`".into(), Duration::from_millis(500)));
    let id = fx
        .sm
        .dump_start(fx.request(listed.objects, &[], "shop.sql"))
        .await
        .unwrap();
    // Wait until the data read is running, then stop.
    for _ in 0..100 {
        if fx
            .fake
            .sent()
            .iter()
            .any(|s| s.contains("FROM `shop`.`customers`"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(fx.sm.jobs.cancel(id).await);
    assert!(fx.sm.jobs.wait(id, Duration::from_secs(10)).await);
    let d = fx.sm.jobs.detail(id).unwrap();
    assert_eq!(d.job.state, JobState::Cancelled);
    assert!(fx.files().is_empty(), "{:?}", fx.files());
    assert!(
        fx.fake
            .sent()
            .iter()
            .all(|s| !s.contains("FROM `shop`.`orders`")),
        "nothing is read after Stop"
    );
}

#[tokio::test]
async fn an_existing_part_file_is_left_alone() {
    let fx = mysql_shop(false).await;
    let listed = fx.all_objects(&[]).await;
    std::fs::write(fx.path("shop.sql.dntpart"), "someone else's").unwrap();
    let d = fx.run(fx.request(listed.objects, &[], "shop.sql")).await;
    assert_eq!(d.job.state, JobState::Failed);
    assert_eq!(
        std::fs::read_to_string(fx.path("shop.sql.dntpart")).unwrap(),
        "someone else's"
    );
    assert_eq!(fx.files(), ["shop.sql.dntpart"]);
}

#[tokio::test]
async fn a_vanished_object_fails_the_dump() {
    let fx = mysql_shop(false).await;
    let mut objects = fx.all_objects(&[]).await.objects;
    objects.push(DumpObject {
        schema: None,
        name: "nope".into(),
        kind: ObjectKind::Table,
        data: true,
    });
    let d = fx.run(fx.request(objects, &[], "shop.sql")).await;
    assert_eq!(d.job.state, JobState::Failed);
    assert!(d.job.summary.unwrap_or_default().contains("nope"));
    assert!(fx.files().is_empty());
}

#[test]
fn requests_are_checked() {
    let obj = |schema: Option<&str>, kind| DumpObject {
        schema: schema.map(str::to_string),
        name: "t".into(),
        kind,
        data: true,
    };
    let req = |objects: Vec<DumpObject>| DumpRequest {
        connection_id: Uuid::nil(),
        database: "shop".into(),
        schemas: vec![],
        objects,
        include_drop: false,
        compress: false,
        path: "C:\\x\\shop.sql".into(),
    };
    let field = |e: DbError| match e {
        DbError::Config { field, .. } => field.unwrap_or_default(),
        e => panic!("{e:?}"),
    };
    assert_eq!(
        field(normalise(req(vec![]), SqlKind::MySql).unwrap_err()),
        "objects"
    );
    for (kind, bad) in [
        (SqlKind::MySql, obj(None, ObjectKind::Sequence)),
        (SqlKind::MySql, obj(Some("s"), ObjectKind::Table)),
        (SqlKind::Postgres, obj(None, ObjectKind::Table)),
        (SqlKind::Postgres, obj(Some("s"), ObjectKind::Function)),
    ] {
        assert_eq!(
            field(normalise(req(vec![bad]), kind).unwrap_err()),
            "objects"
        );
    }
    let twice = vec![obj(None, ObjectKind::Table), obj(None, ObjectKind::Table)];
    assert!(normalise(req(twice), SqlKind::MySql).is_err());
    // Data only for tables; PostgreSQL schemas come from the objects.
    let ok = normalise(
        req(vec![
            obj(Some("b"), ObjectKind::View),
            obj(Some("a"), ObjectKind::Table),
        ]),
        SqlKind::Postgres,
    )
    .unwrap();
    assert_eq!(ok.schemas, ["a", "b"]);
    assert!(!ok.objects[0].data && ok.objects[1].data);
    let mut outside = req(vec![obj(Some("b"), ObjectKind::Table)]);
    outside.schemas = vec!["a".into()];
    assert_eq!(
        field(normalise(outside, SqlKind::Postgres).unwrap_err()),
        "schemas"
    );
    let mut no_path = req(vec![obj(None, ObjectKind::Table)]);
    no_path.path = " ".into();
    assert_eq!(
        field(normalise(no_path, SqlKind::MySql).unwrap_err()),
        "path"
    );
}

/// Listing the dumpable objects needs a schema on PostgreSQL only.
#[tokio::test]
async fn dump_objects_needs_a_schema_on_postgres_only() {
    let pg = fixture(DbKind::Postgres, false).await;
    let err = pg
        .sm
        .dump_objects(pg.conn, "shop".into(), vec![])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, DbError::Config { field: Some(f), message } if f == "schemas" && message == "Choose at least one schema."),
        "{err:?}"
    );
    let my = mysql_shop(false).await;
    assert!(!my.all_objects(&[]).await.objects.is_empty());
}
