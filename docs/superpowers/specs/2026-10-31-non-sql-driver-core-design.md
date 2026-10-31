# Core cho driver không-SQL (MongoDB, Redis)

- Ngày: 2026-10-31
- Trạng thái: thiết kế đã được người dùng duyệt từng phần trong phiên brainstorming (trait core, ngôn ngữ lệnh +
  guard, metadata + cây, connection config, tầng app, giai đoạn + test); bản viết này chờ người dùng duyệt.
- Cơ sở: P1 core (`2026-10-19-p1-core-design.md`: layering, secret không bao giờ vào SQLite), P2 query
  (`2026-10-19-p2-query-design.md`: plan → guard → run, History, redact), roadmap P3–P6 §2 (nguyên tắc an toàn),
  P3 data view/edit, P5 transfer (migration rebuild bảng kiểu `jobs_next`).
- Văn xuôi tiếng Việt; tên định danh, lệnh và mọi chữ trên UI là tiếng Anh.

## 1. Mục tiêu

Hiện tại core là một "SQL core": trait `Session` gắn với `Dialect` và `StatementKind`, guard read-only/Production
chỉ hiểu SQL, metadata là mô hình quan hệ, `ConnectionConfig` chỉ có host/port/username, và khoảng 85 phép so
sánh `== DbKind::MySql` / `!= DbKind::Postgres` (không tính test) cùng 46 file UI dạng `kind === 'postgres' ? … : …`
sẽ âm thầm đưa một kind mới vào nhánh SQL.

Spec này refactor core và tầng app để thêm được driver **không-SQL** (MongoDB, Redis, và về sau các engine kiểu
lệnh khác) chỉ bằng cách tạo một crate `driver-*` mới, cài các trait và thêm một dòng vào `default_drivers()`
(`src-tauri/src/state.rs`). Spec **không** viết driver Mongo hay Redis; mỗi driver có spec riêng dựa trên các hợp
đồng ở đây. Mongo và Redis là hai "khách hàng mục tiêu" để kiểm tra rằng hợp đồng đủ dùng.

Lời hứa an toàn P2/P3 giữ nguyên cho mọi engine: không lệnh ghi nào tới server mà không qua plan và guard backend;
connection read-only không bao giờ nhận lệnh ghi; Production phải xác nhận; không tự retry; `outcome_unknown` hiển
thị ở UI và History; secret không bao giờ vào SQLite, log hay payload frontend.

## 2. Các quyết định đã chốt

| # | Quyết định |
|---|---|
| D1 | Mức hỗ trợ Mongo/Redis bản đầu: **kết nối, cây, xem dữ liệu, console lệnh, sửa dữ liệu** (document / key) qua plan + guard. Không có designer, DDL edit, object actions, import, dump/restore, seed, explain. Core chừa chỗ (capabilities) để bật các tính năng đó về sau. |
| D2 | Topology bản đầu: Mongo một host hoặc URI `mongodb+srv://` / nhiều host (replica set) nhưng **không** qua SSH tunnel khi nhiều host; Redis standalone, TLS, ACL user. Redis Cluster / Sentinel để sau. |
| D3 | Console Mongo dùng **tập con mongosh** (`db.users.find({…}).sort({…}).limit(10)`, `insertOne`, `updateMany`, `aggregate`, `db.runCommand({…})`, `ObjectId()`, `ISODate()`), parse bằng parser riêng, **không chạy JS**, để guard phân loại được từng lệnh trước khi gửi. |
| D4 | Kiến trúc **họ engine**: `Session` chung + `SqlSession` / `CommandSession`. Toàn bộ code SQL đã gia cố (`core::sql`, `query::guard/plan/run`) giữ nguyên logic; chỉ đổi kiểu đầu vào. |
| D5 | Chính sách guard (class × env × read_only → verdict) viết một lần trong `query::policy`, dùng trực tiếp cho họ Command. Guard SQL **không bị viết lại**; một test ma trận khẳng định hai bên cho cùng kết quả. |
| D6 | Fail-closed: lệnh không parse được bị Block; lệnh parse được nhưng không có trong bảng lệnh của driver là `Unknown`: chặn trên read-only, phải gõ tên trên Production (chặt hơn `Admin`, vốn chỉ cần Confirm như SQL `Other`). |
| D7 | Tính năng theo **capability do driver khai báo lúc connect**, không theo kind cố định. Backend kiểm tra capability ở mọi command (phòng thủ hai lớp); UI chỉ ẩn menu/nút. |
| D8 | Không còn phép so sánh hai nhánh theo kind (`==` / `!=` / `kind === '…' ? :`) ngoài module engine; thay bằng `match` / `switch` đầy đủ nhánh hoặc capability. Một test Rust và một luật ESLint giữ quy tắc này. |

Ngoài phạm vi: driver Mongo và Redis (spec riêng); Redis Cluster / Sentinel; SSH tunnel nhiều đích; Mongo
transaction trong console (API session của mongosh); `MONITOR` / `SUBSCRIBE` / change streams; autocomplete tên
field Mongo; import `.ncx` cho Mongo/Redis; import/export/dump/seed cho họ Command; explain cho họ Command.

## 3. Kiểu và trait trong core (`crates/core/src/driver.rs`, `server.rs`)

### 3.1 Kind, family, capabilities

```rust
pub enum DbKind { MySql, Postgres, Mongo, Redis }          // serde: "mysql" | "postgres" | "mongo" | "redis"
pub enum EngineFamily { Sql, Command }

impl DbKind {
    pub fn family(self) -> EngineFamily;                    // match đầy đủ nhánh
}

pub enum ServerVariant { MySql57, MySql8, MariaDb, Postgres, Mongo, Redis }

pub enum NamespaceModel {
    DbSchema,       // PostgreSQL: database → schema → object
    Db,             // MySQL, Mongo: database → object
    NumberedDb,     // Redis: db0…dbN → keyspace (không list từng key trong cây)
}

pub struct Capabilities {
    pub family: EngineFamily,
    pub namespaces: NamespaceModel,
    pub transactions: bool,     // SQL: true; Mongo: true khi replica set / sharded; Redis: false (MULTI không phải tx có rollback)
    pub data_edit: bool,
    pub designer: bool,
    pub ddl_edit: bool,
    pub object_actions: bool,
    pub import: bool,
    pub dump: bool,
    pub seed: bool,
    pub explain: bool,
    pub paged_objects: bool,    // Redis: object được duyệt qua `Keyspace::scan`
}

pub struct ServerInfo {
    pub kind: DbKind,
    pub variant: ServerVariant,
    pub version: (u32, u32, u32),
    pub version_string: String,
    pub capabilities: Capabilities,   // mới; UI đọc từ đây
}
```

- `Capabilities::sql_full(namespaces)` là hằng cho MySQL/PG (mọi cờ `true` trừ `paged_objects`); driver MySQL/PG
  dùng nó, nên hành vi hiện tại không đổi.
- `MySqlCapabilities` giữ nguyên; nhánh `ServerVariant::Mongo | Redis` trả `Self::default()`.
- `Capabilities` được export qua specta cùng `ServerInfo`.

### 3.2 Tách trait `Session`

```rust
#[async_trait]
pub trait Session: Send + Sync {
    fn server_info(&self) -> &ServerInfo;
    fn capabilities(&self) -> &Capabilities { &self.server_info().capabilities }
    fn engine(&self) -> Engine<'_>;
    fn introspector(&self) -> &dyn Introspector;
    fn keyspace(&self) -> Option<&dyn Keyspace> { None }
    fn cancel_handle(&self) -> CancelHandle;
    fn reconnects(&self) -> u64 { 0 }
    async fn ping(&self) -> Result<(), DbError>;
    async fn close(self: Box<Self>);
}

pub enum Engine<'a> {
    Sql(&'a dyn SqlSession),
    Command(&'a dyn CommandSession),
}

#[async_trait]
pub trait SqlSession: Send + Sync {
    fn dialect(&self) -> Dialect;
    fn tx_state(&self) -> TxState;
    fn in_transaction(&self) -> bool { self.tx_state() != TxState::Idle }
    async fn execute_one(&self, stmt: &str, kind: StatementKind, opts: &ExecOptions,
                         sink: &mut dyn RowSink) -> Result<StmtOutcome, DbError>;
    async fn execute(&self, sql: &str, opts: ExecOptions,
                     sink: &mut dyn RowSink) -> Result<ExecSummary, DbError>;   // default như hiện tại
}

#[async_trait]
pub trait CommandSession: Send + Sync {
    fn language(&self) -> &dyn CommandLanguage;
    fn tx_state(&self) -> TxState;                 // Mongo bản đầu: luôn Idle; Redis: Active giữa MULTI…EXEC
    fn data_access(&self) -> &dyn DataAccess;
    async fn execute_command(&self, cmd: &ParsedCommand, opts: &ExecOptions,
                             sink: &mut dyn RowSink) -> Result<StmtOutcome, DbError>;
}
```

- Driver MySQL/PG: tách `impl Session` hiện có thành `impl Session` + `impl SqlSession`; `engine()` trả
  `Engine::Sql(self)`. Logic bên trong không đổi.
- `Driver` giữ nguyên (`kind()`, `connect()`).
- `ExecOptions`, `CellMode`, `RowSink`, `SharedSink`, `StopSignal`, `ServerNotices`, `ExecProgress`, cancel grace,
  `StmtOutcome`, `ResultSummary` dùng chung cho cả hai họ. Hợp đồng `execute_command` về stop / timeout / cancel /
  `was_in_tx` y hệt `execute_one` (doc comment của `Session::execute_one` hiện tại chuyển sang cả hai).
- `ExecOptions.params` chỉ có nghĩa với họ SQL; `execute_command` bỏ qua nó (doc comment nói rõ).
- Helper trên `dyn Session`:

```rust
pub fn sql(&self) -> Result<&dyn SqlSession, DbError>;       // Command → DbError::Unsupported { feature: "SQL", .. }
pub fn command(&self) -> Result<&dyn CommandSession, DbError>;
```

  Mọi chỗ đang gọi `session.dialect()` / `execute_one` / `execute` / `tx_state` (`dnt_query::run`,
  `session_manager`, `query_sessions`, data, export, backup, import, seed, table_design, ddl_edit, object_actions)
  đi qua `session.sql()?`. Không có đường nào chạy SQL lên một session Command.

## 4. Ngôn ngữ lệnh và guard cho họ Command

### 4.1 Hợp đồng ngôn ngữ (`crates/core/src/command.rs`, mới)

```rust
pub trait CommandLanguage: Send + Sync {
    /// Các lệnh của một script, theo thứ tự; range là byte trong `text`. Không bao giờ panic.
    fn split(&self, text: &str) -> Vec<CommandSpan>;
    /// Một lệnh. Lỗi cú pháp là `Err`, không bao giờ là một lệnh "Other" được cho qua.
    fn parse(&self, text: &str) -> Result<ParsedCommand, CommandParseError>;
    /// Bản text an toàn để lưu History / log / Favorites.
    fn redact(&self, text: &str) -> Redacted;
}

pub struct CommandSpan { pub range: (usize, usize), pub text: String }

pub struct ParsedCommand {
    pub name: String,                  // "find", "updateMany", "HSET"
    pub class: CommandClass,
    pub targets: Vec<CommandTarget>,   // collection / key / pattern bị tác động
    pub unbounded: bool,               // tác động mọi document / key của target (≈ UPDATE không WHERE)
    pub drops: Vec<CommandDrop>,       // drop / dropDatabase / FLUSHDB / FLUSHALL (§4.3)
    pub session_effect: Option<CommandSessionEffect>,   // Redis `SELECT n`, Mongo `use db`
    pub payload: CommandPayload,       // dạng đã parse, opaque với core; driver đọc lại khi execute
}

pub enum CommandClass { Read, Write, Ddl, Admin, SessionChange, Transaction(TxControl), Unknown }

pub struct CommandTarget { pub database: Option<String>, pub name: String }

pub struct CommandParseError { pub message: String, pub offset: Option<usize> }
```

- `CommandPayload` là `Arc<dyn Any + Send + Sync>`; core không đọc nó. `execute_command` downcast sang kiểu của
  driver; sai kiểu là `DbError::Internal`.
- Struct `Redacted` chuyển từ `dnt_query::redact` sang `dnt_core::redact` (core không được phụ thuộc query);
  `dnt_query` re-export nó nên code hiện có không đổi đường dẫn.
- `TxControl` dùng lại kiểu sẵn có trong `core::sql`.

### 4.2 Yêu cầu phân loại cho driver (ràng buộc cho spec driver)

Redis (bảng lệnh nhúng, Redis ≥ 6, theo cờ `readonly` / `write` / `admin` / `dangerous`):

- lệnh `readonly` → `Read`; `write` → `Write`; `admin` / `dangerous` → `Admin`.
- `EVAL`, `EVALSHA`, `FCALL` → `Write`; `EVAL_RO`, `EVALSHA_RO`, `FCALL_RO` → `Read`.
- `SELECT n` → `SessionChange`; `MULTI` → `Transaction(Begin)`, `EXEC` → `Transaction(Commit)`,
  `DISCARD` → `Transaction(Rollback)`.
- `FLUSHDB` / `FLUSHALL` → `Admin`, `drops` = `Database` (db hiện tại / mọi db). `DEL` / `UNLINK` → `Write`,
  `targets` = các key. `KEYS` → `Read` (driver có thể cảnh báo về hiệu năng).
- `MONITOR`, `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE`, `SYNC`, `PSYNC`, `CLIENT PAUSE` → `CommandParseError`
  `"<NAME> is not supported in the console"`.
- Lệnh không có trong bảng → `Unknown`.

Mongo (parser tập con mongosh):

- `find`, `findOne`, `countDocuments`, `estimatedDocumentCount`, `distinct`, `getIndexes`, `aggregate` → `Read`;
  `aggregate` có stage `$out` hoặc `$merge` → `Write`, target = collection đích.
- `insertOne`, `insertMany`, `updateOne`, `updateMany`, `replaceOne`, `deleteOne`, `deleteMany`,
  `findOneAndUpdate`, `findOneAndReplace`, `findOneAndDelete`, `bulkWrite` → `Write`; `updateMany` / `deleteMany`
  với filter rỗng `{}` → `unbounded`.
- `createCollection`, `createIndex`, `createIndexes`, `dropIndex`, `dropIndexes`, `renameCollection` → `Ddl`;
  `drop` / `dropDatabase` → `Ddl` với `drops`.
- `db.runCommand({...})` / `db.adminCommand({...})` → phân loại theo key đầu tiên với bảng lệnh của driver
  (`find`, `count`, `listCollections`… → `Read`; `insert`, `update`, `delete` → `Write`; `create`, `drop` → `Ddl`;
  còn lại có trong bảng → `Admin`); không có trong bảng → `Unknown`.
- `use <db>` → `SessionChange`.
- Mọi cú pháp JS ngoài tập con (biến, vòng lặp, hàm, `load()`, `eval`) → `CommandParseError`.

### 4.3 Chính sách dùng chung (`crates/query/src/policy.rs`, mới)

```rust
pub enum RiskClass { Read, Write, Ddl, Admin, SessionChange, Transaction, Unknown }

pub struct RiskFacts {
    pub class: RiskClass,
    pub unbounded: bool,
    pub drops: Vec<CommandDrop>,     // từ ParsedCommand.drops
}

pub struct PolicyCtx {
    pub env: Option<EnvTag>,
    pub read_only: bool,
    pub confirm_unbounded: bool,     // setting "Confirm UPDATE/DELETE without WHERE", như GuardCtx
    pub connection_name: String,
}

pub struct PolicyOutcome { pub verdict: Verdict, pub confirm_name: Option<String> }

pub fn policy(facts: &RiskFacts, ctx: &PolicyCtx) -> PolicyOutcome;
```

`ParsedCommand.drops` là `Vec<CommandDrop>` với `CommandDrop { kind: CommandDropKind, target: CommandTarget }`,
`CommandDropKind = Object | Database` (drop collection ≈ `DROP TABLE`; `dropDatabase`, `FLUSHDB`, `FLUSHALL` ≈
`DROP DATABASE`). `DEL` / `UNLINK` key **không** là drop (key ≈ dòng), chỉ là `Write`.

Bảng chính sách, khớp với `guard::stmt_verdict` hiện tại (read-only thắng Production; Staging / Local cho phép mọi
thứ trừ các luật rủi ro bên dưới):

| class | read_only | Production | Staging | Local / không tag |
|---|---|---|---|---|
| Read, SessionChange, Transaction | Allow | Allow | Allow | Allow |
| Write, Ddl, Admin (≈ SQL `Other` ngoài whitelist) | Block | Confirm | Allow | Allow |
| Unknown | Block | Confirm + gõ tên | Allow | Allow |

Luật rủi ro, áp lên kết quả `Allow` ở trên khi không read-only (như `stmt_verdict`):

- `unbounded` → Confirm trên Production / Staging; trên Local / không tag chỉ khi `confirm_unbounded`.
- drop `Object` → Confirm trên Staging (Production đã Confirm vì là Ddl).
- drop `Database` → Confirm ở mọi env.

`confirm_name` (chỉ trên Production, verdict Confirm): `unbounded` hoặc có `drops` → tên target đầu tiên (như
`risk::confirm_name`); `Unknown` không có target → `connection_name`. Luật "Unknown phải gõ tên trên Production" là
luật riêng của họ Command (SQL không có lệnh "không parse được" — `split` / `classify` luôn cho ra một kind), nên
nằm ngoài test ma trận.

Guard SQL **không** gọi `policy`. Test `policy_matrix_matches_sql_guard` (trong `crates/query/tests/`) chạy, cho
mỗi hàng có tương đương SQL, một câu SQL đại diện (`SELECT 1`, `BEGIN`, `UPDATE t SET a = 1 WHERE id = 1`,
`UPDATE t SET a = 1`, `CREATE TABLE t (a int)`, `DROP TABLE t`, `DROP DATABASE d`, một câu `Other` ngoài whitelist)
qua `guard_with` của cả hai dialect, và khẳng định verdict + `confirm_name` trùng với `policy` trên mọi tổ hợp env ×
read_only × confirm_unbounded. Nếu hai bên lệch nhau, sửa `policy` cho khớp guard SQL, không sửa guard SQL.

### 4.4 Plan / guard / run cho họ Command (`crates/query/src/command.rs`, mới)

```rust
pub fn plan_commands(text: &str, target: Target, lang: &dyn CommandLanguage) -> CommandPlan;
pub fn guard_commands(plan: &mut CommandPlan, ctx: &PolicyCtx);        // §4.3
pub async fn run_commands(session: &dyn CommandSession, plan: &CommandPlan, cfg: RunConfig,
                          sink: &mut dyn RowSink) -> RunReport;
```

- `Target` (Cursor / Selection / All) và quy tắc chọn lệnh theo con trỏ y hệt `plan::plan`; range đổi sang UTF-16
  bằng `Utf16Map`.
- Lệnh có `CommandParseError` nằm trong plan với `verdict = Block` và thông báo lỗi kèm offset; script không chạy
  lệnh nào nếu có một lệnh Block (như SQL).
- `CommandPlan` chiếu ra **cùng DTO UI** với `ScriptPlan` (`StmtKindLabel`, `Verdict`, range, `confirm_name`,
  `unbounded`, `drops`). `StmtKindLabel` thêm `Admin` (hiển thị `Admin`); `Unknown` hiển thị như `Admin`.
- `run_commands` dùng lại `RunConfig`, `RunReport`, `StmtReport`, `StopReason`, `RunCancel`, `reconnect_notice`,
  `error_offset`; dừng ở lỗi đầu tiên; `SessionChange` cập nhật database hiện tại của tab; `Transaction` cập nhật
  `tx_state` hiển thị.
- Không auto-begin, không manual-tx mode cho họ Command bản đầu (nút Manual transaction ẩn khi
  `capabilities.transactions == false` hoặc family là Command).

## 5. Metadata và cây kết nối

### 5.1 `Introspector`

Giữ nguyên các method. Bổ sung `ObjectKind::Collection` (Mongo; view Mongo dùng `ObjectKind::View`) và
`ObjectKind::Key` (chỉ dùng trong `ObjectRef` để mở tab key, không bao giờ do `list_objects` trả về).

Hợp đồng cho driver Command:

| Method | Mongo | Redis |
|---|---|---|
| `list_databases` | `listDatabases` (nameOnly) | `db0…dbN` có key, theo `INFO keyspace`; luôn có db của connection |
| `list_schemas` | `[]` | `[]` |
| `list_objects(Collection / View)` | `listCollections` | `[]` |
| `list_objects(khác)` | `[]` | `[]` |
| `list_columns` | `[]` (bản đầu) | `[]` |
| `table_detail` | suy từ ≤ 100 document mẫu (`$sample`) + `listIndexes` | `Unsupported` |
| `object_ddl` | options + validator của collection, JSON có thụt dòng | `Unsupported` |

`table_detail` của Mongo: mỗi field cấp một là một `ColumnDetail` (thứ tự: `_id`, rồi theo lần xuất hiện đầu
tiên trong mẫu); `db_type` là tên kiểu BSON (`objectId`, `string`, `int`, `long`, `double`, `decimal`, `bool`,
`date`, `object`, `array`, `binData`…) hoặc `mixed(string|int)` khi mẫu có nhiều kiểu; `nullable = true` trừ `_id`;
index `_id_` là `primary`. `ObjectSummary.row_estimate` lấy từ `estimatedDocumentCount` khi rẻ.

### 5.2 `Keyspace` (cho `capabilities.paged_objects`)

```rust
#[async_trait]
pub trait Keyspace: Send + Sync {
    async fn scan(&self, db: &str, req: &ScanRequest) -> Result<KeyPage, DbError>;
    async fn key_value(&self, db: &str, key: &KeyName, range: &ValueRange) -> Result<KeyValue, DbError>;
}

pub struct ScanRequest { pub cursor: Option<String>, pub pattern: Option<String>,
                         pub key_type: Option<KeyType>, pub count: u32 }
pub struct KeyPage { pub keys: Vec<KeySummary>, pub next_cursor: Option<String> }   // None: hết
pub struct KeySummary { pub name: KeyName, pub key_type: KeyType, pub ttl_ms: Option<i64> }
pub struct KeyName { pub display: String, pub raw: Vec<u8> }   // raw serialize base64; display thay byte lạ bằng \xNN
pub enum KeyType { String, Hash, List, Set, ZSet, Stream, Json, Other(String) }
pub struct ValueRange { pub offset: u64, pub count: u32 }      // trang cho hash/list/set/zset/stream
pub enum KeyValue {
    String { value: Value },
    Hash { fields: Vec<(Value, Value)>, total: u64, next: Option<String> },
    List { items: Vec<Value>, total: u64 },
    Set { members: Vec<Value>, total: u64, next: Option<String> },
    ZSet { members: Vec<(Value, f64)>, total: u64 },
    Stream { entries: Vec<StreamEntry>, total: u64 },
    Other { type_name: String, text: String },
}
```

`key_value` cũng trả `ttl_ms` và `memory_bytes: Option<u64>` (best-effort, `MEMORY USAGE`). `scan` với
`count ≤ 1000`; `pattern` là glob của Redis, không bao giờ bị đổi thành `KEYS`.

### 5.3 Map giá trị

Không thêm variant vào `Value`. BSON → `Value`: `ObjectId`, `Decimal128`, `Timestamp`, `MinKey`, `MaxKey`,
`RegularExpression` → `Value::Other { type_name, text }`; `DateTime` → `Value::DateTime` (ISO 8601 UTC);
`Binary` → `Value::Bytes` (UUID subtype 4 → `Value::Uuid`); `Int32`/`Int64` → `Value::Int`; `Double` →
`Value::Float`; document / array lồng nhau → `Value::Json` (relaxed Extended JSON). Reply Redis: bulk string UTF-8 →
`Value::Text`, không phải UTF-8 → `Value::Bytes`; integer → `Value::Int`; double (RESP3) → `Value::Float`;
nil → `Value::Null`.

### 5.4 Cây (UI)

- `treeModel` thêm node kind `'keyspace-db'`. Folder của database lấy theo `NamespaceModel`: `DbSchema` → schema;
  `Db` + Sql → `Tables` / `Views` / …; `Db` + Command → `Collections` / `Views`; `NumberedDb` → node `'keyspace-db'`
  (nhãn `db0 · 1,234 keys`), không có con; mở nó là mở tab Key browser.
- Menu chuột phải dựng từ capabilities (xem §7.2).

## 6. Connection config, store, tunnel

### 6.1 `DriverOptions`

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriverOptions {
    None,
    Mongo { srv: bool, auth_source: Option<String>, replica_set: Option<String>,
            direct_connection: bool, extra_hosts: Vec<HostPort> },
    Redis { db: u16 },
}

pub struct ConnOptions {   // đã có
    …,
    #[serde(default)]       // JSON cũ không có field này → DriverOptions::None
    pub driver: DriverOptions,
}
```

Lưu trong cột `options_json` sẵn có; không thêm cột. `ResolvedConfig` thêm `driver: DriverOptions`; driver tự dựng
URI từ `host`, `port`, `username`, `password`, `database`, `ssl`, `driver`.

### 6.2 Validate theo kind

`ConnectionConfig::validate` thành `match self.kind` đầy đủ nhánh:

- MySQL / PG: như hiện tại.
- Mongo: `host` bắt buộc; `port` bắt buộc trừ khi `srv`; `username` tùy chọn; `options.driver` phải là `Mongo`;
  `srv` hoặc `extra_hosts` không rỗng mà có `ssh` → lỗi field `ssh`, message `SSH tunnel needs a single host`;
  `srv` và `extra_hosts` không đi cùng nhau.
- Redis: `host`, `port` bắt buộc; `username` tùy chọn (ACL); `options.driver` phải là `Redis`.
- `options.driver` không khớp kind → lỗi field `options.driver`.

Mongo qua SSH tunnel: driver đặt `directConnection=true`.

### 6.3 URI

`conn_string.rs` thêm parse `mongodb://`, `mongodb+srv://`, `redis://`, `rediss://` thành các field của
`ConnectionConfig` + mật khẩu. Mật khẩu (và mọi tham số chứa secret) được tách ra và đi vào secret store qua luồng
lưu connection hiện có; URI thô không được lưu. `rediss://` → `ssl.mode = Require` trở lên. Tham số URI không có
field tương ứng → lỗi `Unsupported URI option: <name>` (không lặng lẽ bỏ qua). Ngược lại, có `to_uri(cfg)` không
kèm mật khẩu để hiển thị và copy.

### 6.4 TLS

Dùng lại `SslConfig`. Driver map: `Disable` → không TLS; `Prefer` → Mongo/Redis không có "prefer", coi như
`Require` nhưng không kiểm chứng chứng chỉ (dialog hiện hint); `Require` → TLS, không kiểm chứng; `VerifyCa` /
`VerifyFull` → kiểm chứng CA / CA + hostname. Ràng buộc workspace giữ nguyên: chỉ rustls với ring, không OpenSSL,
không aws-lc-rs.

### 6.5 Store

- Migration mới rebuild bảng `connections` (cách làm như `jobs_next`: tạo `connections_next`, copy, xóa, đổi tên,
  tạo lại index / FK) với `CHECK (kind IN ('mysql','postgres','mongo','redis'))`. Các bảng tham chiếu
  `connections(id)` giữ nguyên FK (chạy trong transaction với `PRAGMA foreign_keys` theo đúng mẫu đã dùng).
- `.dntconn` export/import mang kind mới và `options.driver`; bản cũ đọc file mới báo `unknown kind` rõ ràng.
- `.ncx`: mục MongoDB / Redis bị bỏ qua và ghi vào danh sách `skipped` với lý do, như kind lạ hiện nay.

### 6.6 Dialog connection

`connectionDefaults.ts` thêm 27017 / 6379. Dialog dựng field theo kind: Mongo có ô `Connection string` (dán URI →
điền field), `SRV`, `Auth database`, `Replica set`, `Additional hosts`; Redis có `Database index`. Field không dùng
cho kind đó bị ẩn (ví dụ `Charset` chỉ MySQL). Chữ trên UI tiếng Anh.

## 7. Tầng app

### 7.1 Dọn nhánh theo kind

- Rust: mọi `== DbKind::X` / `!= DbKind::X` / `matches!(…, DbKind::X)` ngoài test đổi thành `match` đầy đủ nhánh
  (nhánh Mongo / Redis trả `Unsupported` hoặc giá trị đúng) hoặc kiểm tra capability. Các `match` có `_ =>` gộp
  kind phải liệt kê kind rõ ràng.
- Test `no_binary_kind_branches` (trong `src-tauri/tests/`) quét `crates/*/src/**/*.rs` và `src-tauri/src/**/*.rs`,
  bỏ qua file `*tests*.rs`, `tests/` và khối `#[cfg(test)]` theo heuristic dòng, và fail nếu gặp
  `== DbKind::`, `!= DbKind::` hoặc `matches!(` có `DbKind::`. Danh sách ngoại lệ được ghi ngay trong test (rỗng
  sau R1).
- UI: module mới `ui/src/engine/` gồm `familyOf(kind)`, `capsOf(serverInfo)`, `quoteIdent(kind, name)`,
  `editorLanguage(kind)`, `engineChip(kind)` (`MY` / `PG` / `MG` / `RD`), `namespaceLabel(kind)`… viết bằng
  `switch` đầy đủ nhánh với `assertNever`. Luật ESLint `no-restricted-syntax` cấm so sánh `kind` / `.kind` với
  literal `'mysql'` | `'postgres'` | `'mongo'` | `'redis'` ngoài `ui/src/engine/` và file test.

### 7.2 Gate theo capability

- Backend: helper `require_cap(state, id, Cap::Designer)?` (và `require_family(…, EngineFamily::Sql)?`) ở đầu mọi
  command của designer, ddl_edit, object_actions, import_data, backup (dump / restore), seed, explain, data (đường
  SQL), export (đường SQL). Thiếu capability → `DbError::Unsupported { feature }`.
- UI: menu cây, toolbar tab, nút Explain, Manual transaction, Import / Dump / Generate data… ẩn theo
  `capsOf(serverInfo)`; khi chưa connect (chưa có `ServerInfo`) dùng `Capabilities` mặc định của kind
  (`defaultCaps(kind)` trong `ui/src/engine/`).

### 7.3 Query tab

- Editor: ngôn ngữ Monaco theo `editorLanguage(kind)`: `mysql`, `pgsql`, `mongo` (Monarch tokenizer kiểu JS: từ
  khóa `db`, tên method, toán tử `$…`, chuỗi, số, comment), `redis` (tên lệnh ở đầu dòng, chuỗi có nháy).
  Statement ranges lấy từ `plan_commands` như SQL.
- `query_run` / `query_plan` rẽ nhánh theo family: Sql → pipeline hiện có; Command → `query::command`. Hộp xác
  nhận, Messages panel, History dùng lại nguyên vẹn nhờ DTO chung.
- Kết quả họ Command đi qua `RowSink` như SQL:
  - Mongo: mỗi batch document được làm phẳng thành cột theo field cấp một (`_id` trước, rồi theo lần xuất hiện đầu
    tiên trong result; field xuất hiện ở batch sau được thêm cột mới, ô cũ là `Null`). Giá trị lồng nhau là
    `Value::Json`. Grid có toggle `Table` / `JSON` cho mỗi result; JSON view hiện từng document dạng relaxed EJSON.
  - Redis: scalar → một cột `value`; array → cột `#`, `value`; map (RESP3) → `field`, `value`; nil → `Null`.
    Lệnh ghi trả `affected` khi reply là số đếm (`DEL`, `HSET`…).

### 7.4 `DataAccess` (driver dựng lệnh cho data view / edit)

```rust
pub trait DataAccess: Send + Sync {
    fn page(&self, obj: &ObjectRef, spec: &PageSpec) -> Result<ParsedCommand, DbError>;   // class phải là Read
    fn count(&self, obj: &ObjectRef, filter: Option<&DataFilter>) -> Result<ParsedCommand, DbError>;
    fn build_edits(&self, obj: &ObjectRef, detail: &TableDetail, edits: &EditSet)
        -> Result<Vec<ParsedCommand>, EditError>;
    fn build_key_edit(&self, db: &str, edit: &KeyEdit) -> Result<Vec<ParsedCommand>, EditError>;
}

pub struct PageSpec { pub sort: Vec<SortSpec>, pub filter: Option<DataFilter>, pub limit: u32, pub skip: u64 }
```

Backend luôn kiểm tra lại `class` của lệnh driver trả về (`page` / `count` phải `Read`) và đưa mọi lệnh qua
`guard_commands`; lệnh của driver không được miễn guard.

### 7.5 Data view và sửa dữ liệu Mongo

- `table_data_impl`, `table_row_count_impl`, `data_editable_impl`, `data_preview_impl`, `data_apply_impl`,
  `data_cell_fetch_impl` rẽ nhánh theo family. Đường Command dùng `DataAccess` + `run_commands`.
- `DataFilter` có cấu trúc map sang Mongo: `Eq` → `$eq`, `Ne` → `$ne`, `Lt/Le/Gt/Ge` → `$lt/$lte/$gt/$gte`,
  `Like` / `NotLike` → `$regex` / `$not` (chuyển `%`, `_` sang regex, phần còn lại escape), `Contains` /
  `StartsWith` / `EndsWith` → `$regex` escape có neo, `In` → `$in`, `Between` → `$gte` + `$lte`, `IsNull` →
  `$eq: null`. Giá trị (chuỗi) được ép theo `db_type` đã suy của field (`int` / `long` / `double` → số, `objectId`
  + 24 hex → `ObjectId`, `bool`, `date` → ISO); không ép được → lỗi field rõ ràng, không đoán.
- `DataFilter` thêm `#[serde(default)] raw_json: Option<String>`: chế độ raw của họ Command (một document filter
  JSON, relaxed EJSON). `raw_where` và `raw_json` loại trừ nhau; `raw_where` trên họ Command / `raw_json` trên họ
  SQL → `DbError::Config`. Filter raw chặn `$where`, `$function`, `$accumulator` ở mọi độ sâu.
- Sửa dữ liệu: `EditSet` giữ nguyên; `RowRef` dùng `_id` (duy nhất, luôn có). `build_edits`: sửa ô →
  `updateOne({_id}, {$set: {...}})` (ô đặt `Null` → `$set: null`; xóa field → `$unset`, thêm `CellInput::Unset`
  chỉ dùng cho họ Command), thêm dòng → `insertOne`, xóa dòng → `deleteOne({_id})`. Field lồng nhau sửa bằng ô
  JSON (cả giá trị field được thay).
- Atomicity: `capabilities.transactions` → một transaction cho cả lần apply; không → từng document một, dùng
  `Atomicity` sẵn có với cảnh báo `Changes are applied one document at a time`.
- `fingerprint` cho họ Command chỉ tính index và kiểu `_id` (schema suy từ mẫu có thể đổi giữa các lần đọc).

### 7.6 Tab Key browser (Redis, `capabilities.paged_objects`)

- Bên trái: danh sách key từ `Keyspace::scan` (pattern glob, bộ lọc type, `Load more` theo cursor, số key của db).
- Bên phải: giá trị theo `KeyValue` (string: editor text / JSON / hex; hash: grid field–value; list: grid
  index–value; set: danh sách; zset: member–score; stream: entry id + field), TTL, memory.
- Thao tác: sửa string; set / xóa field hash; push / set / xóa phần tử list; thêm / xóa member set, zset; đặt TTL /
  persist; rename; delete key; thêm key mới. Mỗi thao tác là một `KeyEdit` → `DataAccess::build_key_edit` →
  `guard_commands` → xác nhận theo env (cùng component xác nhận của data edit, hiện text lệnh) → `run_commands`.
- Không tự refresh định kỳ; nút `Refresh`. Key đã hết hạn / bị xóa khi đang mở → thông báo `Key no longer exists`.

### 7.7 History, Favorites, redact

History dùng lại bảng `query_history` với `origin` `query` / `data_edit`; `sql_text` lưu text lệnh đã qua
`CommandLanguage::redact`; `kind` lưu `StmtKindLabel`. Favorites: hàm lưu favorite trong `dnt-store` nhận thêm
tham số `redact: &dyn Fn(&str) -> Redacted` thay vì tự gọi `dnt_query::redact` với `Dialect`; `src-tauri` truyền
`redact` của SQL hoặc của `CommandLanguage` theo family. Log `debug` cũng redact theo family.

## 8. Giai đoạn

| Giai đoạn | Nội dung | Thay đổi hành vi người dùng |
|---|---|---|
| R1 | §3 (tách trait, `Capabilities`, `Engine`, `DbKind::{Mongo, Redis}` chưa đăng ký driver), §7.1 phía Rust + test `no_binary_kind_branches`, `require_cap` / `require_family` (§7.2 backend). | Không |
| R2 | §4 (`CommandLanguage`, `ParsedCommand`, `query::policy` + test ma trận, `query::command`), §5.1–5.3 kiểu (`Collection`, `Key`, `Keyspace`, `KeyValue`), §7.4 trait `DataAccess`. `FakeCommandDriver` trong testkit (§9). | Không (chỉ test) |
| R3 | §6 (`DriverOptions`, validate theo kind, URI, TLS map, migration store, `.dntconn`, `.ncx`). | Lưu được connection Mongo/Redis; connect báo `no driver registered for Mongo` rõ ràng |
| R4 | §5.4, §6.6, §7.1 phía UI + ESLint, §7.2 UI, §7.3, §7.5, §7.6, §7.7 với e2e-web backend giả cho cả hai họ. | UI sẵn sàng; chưa có driver thật |

Mỗi giai đoạn là một implementation plan riêng (R1 trước, mỗi giai đoạn kết thúc với toàn bộ test pass), vì cả
spec quá lớn cho một plan.

Sau R4, mỗi driver (spec riêng) gồm: crate `crates/driver-mongo` / `crates/driver-redis`, parser / bảng lệnh,
cài `Session` + `CommandSession` + `Introspector` + `DataAccess` (+ `Keyspace` cho Redis), một dòng trong
`default_drivers()`, test tích hợp qua testkit với server thật.

## 9. Test

- **Không regression:** toàn bộ test hiện có pass; chỉ sửa cơ học những test bị ảnh hưởng bởi việc tách trait
  (`impl Session` của các fake → `impl Session` + `impl SqlSession`). Snapshot insta của driver MySQL/PG không đổi.
- **Policy:** `policy_matrix_matches_sql_guard` (§4.3) trên mọi tổ hợp class × env (Production / Staging / Local /
  không tag) × read_only, cả hai dialect.
- **`FakeCommandDriver` (crate `dnt-testkit`):** ngôn ngữ kiểu Redis rút gọn — `GET`, `SET`, `DEL`,
  `SCAN`, `HSET`, `HGETALL`, `SELECT`, `MULTI` / `EXEC`, `FLUSHDB`, một lệnh admin, một lệnh lạ; lưu dữ liệu trong
  bộ nhớ; cài `Keyspace` và `DataAccess`; có hook làm chậm / treo một lệnh để test stop / timeout / cancel grace.
  Test đi trọn luồng: plan (Cursor / Selection / All, range UTF-16), guard (fail-closed: `Unknown` bị Block trên
  read-only và có `confirm_name` trên Production; parse error → Block), run (dừng ở lỗi đầu, `SessionChange`,
  `tx_state` giữa `MULTI` / `EXEC`, stop, timeout), History (đã redact), data edit, key edit.
- **Store:** migration chuyển một DB tạo bằng schema cũ (có connection MySQL / PG, history, jobs tham chiếu) sang
  schema mới, dữ liệu và FK nguyên vẹn; insert được kind `mongo` / `redis`; `options_json = '{}'` cũ parse ra
  `DriverOptions::None`.
- **Config / URI:** validate từng kind (mọi field lỗi có tên field); parse URI Mongo (một host, nhiều host, SRV,
  authSource, replicaSet, tls) và Redis (`redis://`, `rediss://`, db index, ACL user) với mật khẩu được tách ra và
  không xuất hiện trong `Debug`, JSON hay `to_uri`; tham số lạ → lỗi.
- **Kind branches:** `no_binary_kind_branches` (Rust) và ESLint (UI) chạy trong CI hiện có.
- **UI (Vitest):** `ui/src/engine/` (mọi kind, `assertNever`), gate menu / toolbar theo capabilities, cây
  `'keyspace-db'`, grid document làm phẳng (cột mới ở batch sau), JSON view, Key browser (scan, load more, edit →
  preview lệnh), dialog connection theo kind (dán URI điền field, mật khẩu không hiện trong field URI).
- **e2e-web:** `e2e-web/src/backend.ts` thêm connection giả họ Command (dựa trên cùng mô hình với
  `FakeCommandDriver`): mở console, chạy lệnh đọc / ghi trên Production (phải gõ tên), connection read-only chặn
  ghi, mở Key browser và sửa một key.

## 10. Rủi ro và cách giữ

| Rủi ro | Cách giữ |
|---|---|
| Tách trait làm lệch hành vi SQL tinh tế (đổi `sql_mode`, implicit commit, PG aborted tx) | `SqlSession` mang đúng các method cũ, logic không đổi; R1 là refactor không hành vi, chạy toàn bộ test + snapshot trước khi sang R2. |
| Chính sách guard hai họ lệch nhau theo thời gian | Một `query::policy` duy nhất cho họ Command + test ma trận đối chiếu guard SQL. |
| Driver trả lệnh "đọc" thực ra ghi (lỗi phân loại) | Backend kiểm tra lại `class` của mọi lệnh driver dựng; fail-closed `Unknown`; spec driver phải có test bảng phân loại đầy đủ. |
| Một kind mới lại bị nhánh "else" nuốt | `match` đầy đủ nhánh + `no_binary_kind_branches` + ESLint. |
| Migration bảng `connections` làm mất dữ liệu / FK | Mẫu rebuild đã dùng cho `jobs_next`, trong transaction; test migration từ DB schema cũ. |
| Secret lọt qua URI | URI chỉ được parse rồi bỏ; mật khẩu vào secret store; `to_uri` không kèm mật khẩu; test `Debug` / JSON. |
