# DntDatabase — P1: Lõi (Core) — Design Spec

- **Ngày:** 2026-10-19
- **Trạng thái:** Chờ review
- **Canvas UI:** https://claude.ai/artifact/7Uh7EGUTkNvKgBXUWcBB3B (5 màn: Workspace/Query, Table data, Table designer, New connection, Export wizard)

## 1. Bối cảnh và mục tiêu

DntDatabase là công cụ quản trị cơ sở dữ liệu desktop, **dùng nội bộ hằng ngày cho team để thay thế Navicat**. Viết bằng **Rust + Tauri 2**, frontend **React + TypeScript**.

**Ràng buộc đã chốt:**

- Nền tảng: **Windows trước**. macOS/Linux để giai đoạn sau. Code giữ trung lập nền tảng: không dùng thư viện C, keychain đi qua trait.
- CSDL: **MySQL 5.7, MySQL 8.x, MariaDB 10.3+, PostgreSQL 12–17**. Phần metadata được tách theo biến thể ngay từ đầu.
- Chia sẻ connection trong team bằng **file export/import**, mật khẩu mã hóa bằng passphrase. Không có server đồng bộ.
- Không có đăng nhập hay phân quyền trong app. Dữ liệu cục bộ nằm trên máy từng người.

### 1.1 Lộ trình tổng thể

| Giai đoạn | Nội dung |
|---|---|
| **P1 — Lõi (spec này)** | Kiến trúc, lớp driver MySQL/PG, quản lý connection, SSH tunnel/SSL, duyệt schema, export/import connection, import Navicat `.ncx` |
| P2 — Query | Query editor (Monaco, autocomplete, nhiều tab, nhiều câu), lưới kết quả, lịch sử, Explain, xác nhận ghi trên Production |
| P3 — Dữ liệu | Mở bảng, lọc/sắp xếp/phân trang, sửa ô, thêm/xóa dòng, xem trước SQL, commit theo transaction |
| P4 — Thiết kế bảng | Tạo/sửa bảng, cột, index, FK, check; sinh và xem trước DDL |
| P5 — Import/Export | CSV, JSON, Excel, SQL INSERT/dump; backup/restore |
| P6 — Nâng cao | So sánh schema, ER diagram, đồng bộ dữ liệu, dark mode |

Mỗi giai đoạn có spec, plan và bản triển khai riêng.

### 1.2 Phạm vi P1

**Có:**

- Cargo workspace, app shell Tauri, khung UI: toolbar, cây connection, vùng tab, status bar theo canvas.
- Trait `Driver` / `Session` / `RowSink` / `Introspector`, cùng cài đặt cho MySQL-family và PostgreSQL.
- Quản lý connection và nhóm (CRUD, kéo thả, nhân bản), dialog New/Edit connection (General / SSH / SSL / Advanced), nút Test.
- SSH tunnel (password, private key), SSL/TLS (5 chế độ), lưu secret trong Windows Credential Manager.
- Duyệt schema: cây theo biến thể; panel Info và DDL của object; tab Structure chỉ xem.
- SchemaCache chạy nền (chỉ có backend và command đọc, chưa có UI autocomplete).
- Tag môi trường, cờ read-only, hàm phân loại câu đọc/ghi trong `core`.
- Export/import file `.dntconn`; import Navicat `.ncx`.
- Xử lý lỗi, logging, bộ test 4 tầng.

**Không có** (thuộc giai đoạn sau): chạy query từ UI, lưới kết quả, sửa dữ liệu, thiết kế bảng, import/export dữ liệu, ssh-agent, dark mode, i18n (UI tiếng Anh).

## 2. Kiến trúc tổng thể

```
DntDatabase/
├─ crates/
│  ├─ core/          # Value, ColumnMeta, DbError, sql_split, statement classifier
│  │                 # trait Driver / Session / RowSink / Introspector / SecretStore
│  ├─ driver-mysql/  # mysql_async; MySQL 5.7 / 8 / MariaDB (MySqlCapabilities)
│  ├─ driver-pg/     # tokio-postgres (+ tokio-postgres-rustls)
│  ├─ tunnel/        # SSH local port-forward bằng russh
│  └─ store/         # SQLite (rusqlite + rusqlite_migration), keyring, .dntconn, .ncx
├─ src-tauri/        # app shell: Tauri commands, SessionManager, Channel streaming
├─ ui/               # React + TS + Vite, Zustand, Monaco (P2), Glide Data Grid (P2)
└─ docs/superpowers/specs/
```

**Luồng dữ liệu:**

```
UI ──invoke──▶ Tauri command ──▶ SessionManager ──▶ Session (driver) ──▶ DB
 ▲                                      │
 └──── tauri::ipc::Channel (lô dòng) ◀──┘
```

**Quy tắc:**

- `core` chỉ định nghĩa kiểu và trait. `driver-*` cài đặt trait và không phụ thuộc Tauri hay SQLite. `store` không phụ thuộc driver.
- `SessionManager` giữ `connection_id → ConnectionRuntime`. Một `ConnectionRuntime` gồm tunnel (nếu có), kết nối metadata và các session của tab. Frontend không bao giờ cầm kết nối hay secret.
- Kết quả được stream theo lô (mặc định 500 dòng) qua `Channel`, không trả về một khối JSON lớn.
- Kiểu TypeScript được sinh từ Rust bằng `tauri-specta`. Frontend chỉ gọi backend qua lớp `ui/src/api/`.
- Bundle identifier: `com.dnt.dntdatabase`. Dữ liệu app nằm ở `%APPDATA%\DntDatabase\`.

## 3. Driver và hệ kiểu dữ liệu

```rust
#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DbKind;                                   // MySql | Postgres
    async fn connect(&self, cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError>;
}

#[async_trait]
pub trait Session: Send + Sync {
    fn server_info(&self) -> &ServerInfo;                       // kind, variant, version
    async fn execute(&self, sql: &str, opts: ExecOptions,
                     sink: &mut dyn RowSink) -> Result<ExecSummary, DbError>;
    fn cancel_handle(&self) -> CancelHandle;
    fn introspector(&self) -> &dyn Introspector;
    async fn close(self: Box<Self>);
}

pub trait RowSink: Send {
    fn result_start(&mut self, cols: &[ColumnMeta]);
    fn rows(&mut self, batch: Vec<Vec<Value>>);
    fn result_end(&mut self, affected: Option<u64>, elapsed: Duration);
}
```

- `ResolvedConfig` là cấu hình đã ghép secret (`SecretString`) và địa chỉ tunnel cục bộ.
- `ServerVariant`: `MySql57 | MySql8 | MariaDb | Postgres`, kèm phiên bản dạng số.
- **Session:** mỗi tab query có một kết nối riêng (giữ `USE`, `search_path`, transaction, bảng tạm của tab đó). Mỗi connection có thêm một kết nối metadata dùng chung. Với PostgreSQL, mỗi database có kết nối metadata riêng, chỉ mở khi được bung ra trên cây.
- **Hủy query:**
  - PostgreSQL: dùng `CancelToken`.
  - MySQL/MariaDB: chạy `KILL QUERY <connection_id>` qua kết nối metadata.
- **`sql_split`** (trong `core`) tách câu theo dialect:
  - MySQL: hiểu `DELIMITER`, comment `--`, `#`, `/* */`, chuỗi `'`, `"` và backtick.
  - PostgreSQL: hiểu `$tag$…$tag$`, `E'…'`, comment.
- **Phân loại câu** (`classify_statement`): `Read | Write | Ddl | Other`, dựa vào từ khóa đầu tiên sau khi bỏ comment và `WITH …`. P2 dùng để xác nhận trước khi ghi trên Production.
- **`ExecOptions`:** `max_rows` (mặc định 10.000), `timeout`, `batch_size` (mặc định 500).

### 3.1 `Value`

| Variant | Quy tắc |
|---|---|
| `Null`, `Bool`, `Float(f64)` | |
| `Int(i64)`, `UInt(u64)` | Gửi lên UI dạng **string** khi \|x\| > 2⁵³ |
| `Decimal(String)` | Giữ nguyên chuỗi |
| `Text`, `Json`, `Uuid` (String) | |
| `Bytes { len, preview }` | `preview` tối đa 64 byte (hex). Lấy đầy đủ qua command riêng (P3) |
| `Date`, `Time`, `DateTime`, `TimestampTz` (String) | Chuỗi ISO 8601, giữ micro giây và offset |
| `Array(Vec<Value>)` | Mảng của PostgreSQL |
| `Other { type_name, text }` | Mọi kiểu không nhận diện được |

### 3.2 `ColumnMeta`

`name`, `db_type` (tên gốc, ví dụ `varchar(255)`, `int4`), `logical` (`Integer | Decimal | Float | Text | Binary | Bool | Date | Time | DateTime | Json | Uuid | Array | Enum | Other`), `nullable: Option<bool>`, `origin_table: Option<ObjectRef>`, `origin_column: Option<String>`. Hai trường `origin_*` dành cho P3.

## 4. Vòng đời kết nối

**Các trạng thái:**

- `Disconnected → Connecting → Connected`
- `Connecting → Failed`
- `Connected → Reconnecting → Connected` hoặc `→ Failed`
- `* → Disconnected` khi đóng kết nối

**`Connecting` chạy lần lượt 4 bước.** Bước lỗi được ghi vào `DbError::…{ stage }`:

1. **ResolveSecrets:** đọc `SecretStore`. Nếu `secret_mode = ask` thì hiện prompt; secret chỉ nằm trong RAM (`zeroize` khi đóng).
2. **OpenTunnel** (nếu cấu hình SSH).
3. **ConnectDb**, có TLS nếu cấu hình.
4. **DetectServer:**
   - MySQL-family: `SELECT VERSION()`. Chuỗi chứa `MariaDB` → `MariaDb`, kể cả dạng `5.5.5-10.x-MariaDB`; `5.7.*` → `MySql57`; `8.*` trở lên → `MySql8`.
   - PostgreSQL: `SHOW server_version_num`.

Timeout mặc định: SSH 10 giây, DB 10 giây, chỉnh được trong tab Advanced.

### 4.1 SSH tunnel (`tunnel`, russh)

- **Xác thực:** password, hoặc private key (OpenSSH/PEM; ed25519, rsa, ecdsa; có hoặc không passphrase). ssh-agent làm ở giai đoạn sau.
- **Forward:** bind `127.0.0.1:0`, mỗi kết nối TCP vào mở một kênh `direct-tcpip` tới `db_host:db_port`. Mọi session của một connection dùng chung một tunnel. Keepalive 30 giây.
- **Host key:**
  - App có file `known_hosts` riêng tại `%APPDATA%\DntDatabase\known_hosts`.
  - Host chưa biết: trả `SshHostKeyUnknown { fingerprint }`, UI hiện hộp xác nhận, người dùng đồng ý thì lưu lại.
  - Fingerprint bị đổi: trả `Ssh { stage: HostKeyMismatch }` và chặn kết nối.
- **Khi tunnel chết:** mọi session của connection bị đánh dấu hỏng; thao tác tiếp theo sẽ mở lại tunnel.

### 4.2 SSL/TLS (rustls)

- Chế độ chung: `Disable | Prefer | Require | VerifyCa | VerifyFull`; tùy chọn `ca_path`, `cert_path`, `key_path`.
- Mỗi driver ánh xạ sang tùy chọn riêng: `mysql_async` dùng `SslOpts`; `tokio-postgres` dùng `tokio-postgres-rustls` + `sslmode`.

### 4.3 Secret

- Trait `SecretStore { get, set, delete }`.
  - Cài đặt thật: crate `keyring` (Windows Credential Manager).
  - Cài đặt cho test: `MemorySecretStore`.
- Key: `DntDatabase/<conn_uuid>/{db_password|ssh_password|ssh_key_passphrase}`.
- SQLite không bao giờ chứa secret. Secret không bao giờ được gửi lên frontend: frontend chỉ nhận `has_db_password: bool`, `has_ssh_secret: bool`.
- Mọi secret trong RAM dùng `secrecy::SecretString`.

### 4.4 Tự kết nối lại

- Lỗi mất kết nối khi **không** có transaction mở: tự kết nối lại một lần, rồi chạy lại câu lệnh nếu `classify_statement` cho kết quả `Read`.
- Lỗi mất kết nối khi **có** transaction mở (theo dõi bằng cờ trong session, cập nhật theo `BEGIN/START TRANSACTION/COMMIT/ROLLBACK` và trạng thái autocommit): trả `ConnectionLost { in_transaction: true }`, không retry.

### 4.5 Tag môi trường và read-only

- `env_tag`: `production | staging | local | null`, hiển thị bằng màu trên cây, tab và status bar. Production dùng màu đỏ.
- `read_only`:
  - PostgreSQL: chạy `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY` sau khi kết nối.
  - MySQL/MariaDB: chạy `SET SESSION TRANSACTION READ ONLY`.
  - App cũng chặn câu `Write`/`Ddl` (áp dụng thực tế từ P2).
- Hộp xác nhận ghi trên Production thuộc P2.

## 5. Quản lý connection và export/import

### 5.1 SQLite (`%APPDATA%\DntDatabase\app.db`)

```sql
CREATE TABLE groups (
  id TEXT PRIMARY KEY, parent_id TEXT NULL REFERENCES groups(id) ON DELETE CASCADE,
  name TEXT NOT NULL, sort_order INTEGER NOT NULL);

CREATE TABLE connections (
  id TEXT PRIMARY KEY, group_id TEXT NULL REFERENCES groups(id) ON DELETE SET NULL,
  name TEXT NOT NULL, sort_order INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('mysql','postgres')),
  host TEXT NOT NULL, port INTEGER NOT NULL, username TEXT NOT NULL,
  default_db TEXT NULL, env_tag TEXT NULL, read_only INTEGER NOT NULL DEFAULT 0,
  secret_mode TEXT NOT NULL CHECK (secret_mode IN ('saved','ask')),
  ssh_json TEXT NULL, ssl_json TEXT NULL, options_json TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_connected_at TEXT NULL);

CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

- `ssh_json`: `{version, host, port, user, auth: "password"|"key", key_path}`.
- `ssl_json`: `{version, mode, ca_path, cert_path, key_path}`.
- `options_json`: `{version, connect_timeout_s, ssh_timeout_s, charset, application_name}`.
- Migration dùng `rusqlite_migration`, mỗi giai đoạn sau thêm migration mới.
- **Xóa nhóm:** các connection bên trong chuyển về gốc (`ON DELETE SET NULL`), nhóm con bị xóa theo. UI yêu cầu xác nhận trước khi xóa.

### 5.2 Tauri commands (P1)

| Command | Mô tả |
|---|---|
| `list_tree` | Danh sách nhóm và connection (không chứa secret) |
| `create_connection`, `update_connection`, `duplicate_connection`, `delete_connection` | Xóa connection thì xóa luôn secret |
| `create_group`, `rename_group`, `delete_group`, `move_node` | Kéo thả: đổi `group_id`/`parent_id` và `sort_order` |
| `test_connection` | Nhận cấu hình **chưa lưu** và secret tạm; trả `ServerInfo` + thời gian từng bước |
| `open_connection`, `close_connection` | Mở/đóng `ConnectionRuntime` |
| `trust_host_key` | Ghi fingerprint vào `known_hosts` |
| `list_databases`, `list_schemas`, `list_objects`, `table_detail`, `object_ddl`, `refresh_node` | Duyệt schema |
| `get_schema_cache` | Đọc SchemaCache (P2 dùng) |
| `export_connections`, `preview_import`, `apply_import` | `.dntconn` và `.ncx` |

### 5.3 File `.dntconn`

```json
{
  "format": "dntdatabase-connections", "version": 1, "exported_at": "<RFC3339>",
  "groups": [ { "id", "parent_id", "name", "sort_order" } ],
  "connections": [ { "...các cột trong bảng connections, không có secret..." } ],
  "secrets": null | {
    "kdf": "argon2id", "m": 65536, "t": 3, "p": 1, "salt": "<b64>",
    "cipher": "xchacha20poly1305", "nonce": "<b64>", "ciphertext": "<b64>"
  }
}
```

- Bản rõ của `ciphertext` là JSON `{ "<conn_id>": { "db_password"?, "ssh_password"?, "ssh_key_passphrase"? } }`.
- **Export:**
  - Người dùng chọn connection hoặc nhóm (chọn nhóm thì kèm luôn nhóm cha để giữ cấu trúc).
  - "Kèm mật khẩu" bắt buộc passphrase tối thiểu 8 ký tự, nhập 2 lần.
  - Private key SSH chỉ xuất `key_path`.
- **Import:** gồm `preview_import` rồi `apply_import`.
  - Trùng `id`: người dùng chọn `Skip | Overwrite | Copy` (copy thì cấp `id` mới).
  - Trùng tên nhưng khác `id`: thêm hậu tố ` (2)`, ` (3)`…
  - Sai passphrase: lỗi AEAD trả `Config` với thông báo "Passphrase không đúng"; người dùng nhập lại hoặc nhập không kèm mật khẩu.
  - `key_path` không tồn tại trên máy: hiện cảnh báo trong bảng preview.
  - Secret giải mã xong được ghi thẳng vào `SecretStore`.

### 5.4 Import Navicat `.ncx`

- Parse XML `<Connections><Connection …/></Connections>`, chỉ nhận `ConnType` thuộc MySQL, MariaDB, PostgreSQL. Loại khác được liệt kê là "bỏ qua" trong bảng preview.
- Ánh xạ các trường: tên connection, host, port, user, database mặc định, SSH (host, port, user, phương thức xác thực, đường dẫn key), SSL (chế độ và đường dẫn cert).
- Mật khẩu: giải mã theo định dạng Navicat 12+ (AES-128-CBC với key/IV cố định đã được công khai). Nếu giải mã thất bại thì nhập connection không kèm mật khẩu và đánh dấu cảnh báo.
- Dùng chung luồng `preview_import` / `apply_import` và cách xử lý trùng lặp với `.dntconn`.
- Fixture test là file `.ncx` xuất từ Navicat thật, đã thay dữ liệu nhạy cảm bằng giá trị giả.

## 6. Duyệt schema

```rust
#[async_trait]
pub trait Introspector: Send + Sync {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError>;
    async fn list_schemas(&self, db: &str) -> Result<Vec<SchemaInfo>, DbError>;
    async fn list_objects(&self, scope: &Scope, kind: ObjectKind) -> Result<Vec<ObjectSummary>, DbError>;
    async fn table_detail(&self, obj: &ObjectRef) -> Result<TableDetail, DbError>;
    async fn object_ddl(&self, obj: &ObjectRef) -> Result<String, DbError>;
}
```

- **Cấu trúc cây:**
  - MySQL-family: Connection → Database → { Tables, Views, Functions, Procedures, Triggers, Events (+ Sequences trên MariaDB) }.
  - PostgreSQL: Connection → Database → Schema → { Tables, Views, Materialized views, Functions, Procedures, Sequences, Types }.
- `TableDetail`: columns (name, type, nullable, default, auto_increment/identity, generated, comment), primary key, indexes, foreign keys, checks, triggers, options (engine, collation, tablespace, partition), số dòng ước lượng, dung lượng, comment.

### 6.1 MySQL-family

- Một cài đặt chung, khác biệt được gom vào `MySqlCapabilities` dựng từ phiên bản:

| Cờ | MySQL 5.7 | MySQL 8 | MariaDB 10.3+ |
|---|---|---|---|
| `check_constraints` | ✗ | ✓ (≥ 8.0.16) | ✓ (cột khác) |
| `functional_index` | ✗ | ✓ | ✗ |
| `json_is_alias` | ✗ | ✗ | ✓ (`LONGTEXT` + `json_valid()` hiển thị là JSON) |
| `sequences` | ✗ | ✗ | ✓ |
| `invisible_columns` | ✗ | ✓ | ✓ (≥ 10.3) |
| `system_versioned` | ✗ | ✗ | ✓ |

- Danh sách bảng/view dùng `SHOW FULL TABLES`. Chi tiết dùng `information_schema`, chỉ lấy cho bảng đang mở.
- DDL dùng `SHOW CREATE TABLE/VIEW/FUNCTION/PROCEDURE/TRIGGER/EVENT`.

### 6.2 PostgreSQL

- Đọc `pg_catalog`, không dùng `information_schema`.
- Dùng `pg_get_indexdef`, `pg_get_constraintdef`, `pg_get_functiondef`, `pg_get_triggerdef`, `pg_get_viewdef`.
- DDL bảng do app tự dựng từ catalog: cột, default, identity/generated, constraint, index, comment, partition.
- Khác biệt theo phiên bản dựa vào `server_version_num` (`prokind`, `attgenerated`, partition).

### 6.3 Tải lười, cache, lọc

- Mỗi nút chỉ tải khi được bung ra. Cache nằm trong `ConnectionRuntime`. `refresh_node` xóa cache của nút đó và các nút con (UI: F5).
- Mặc định ẩn schema hệ thống (`information_schema`, `mysql`, `performance_schema`, `sys`, `pg_catalog`, `pg_toast`), có công tắc để hiện. Ô lọc chạy trên các nút đã tải.
- Cây trên UI được ảo hóa, phải mượt với 5.000 bảng.
- **SchemaCache:** sau khi kết nối, task nền tải tên bảng/view và cột của database mặc định (PostgreSQL: các schema trong `search_path`). Đọc qua `get_schema_cache`.

## 7. Xử lý lỗi

```rust
pub enum DbError {
    Config { field: Option<String>, message: String },
    Secret { message: String },
    Ssh { stage: SshStage, message: String },          // Connect | Auth | HostKeyMismatch | Channel
    SshHostKeyUnknown { host: String, fingerprint: String },
    Tls { message: String },
    Connect { message: String },
    Timeout { stage: ConnectStage },
    Auth { message: String },
    Server { code: Option<String>, sqlstate: Option<String>, message: String, position: Option<u32> },
    ConnectionLost { in_transaction: bool },
    Cancelled,
    Unsupported { feature: String, version: String },
    Internal { message: String },
}
```

- Lỗi gửi lên frontend dưới dạng `AppError { kind, message, detail, stage?, server_code?, sqlstate?, position?, retryable }`. `detail` giữ nguyên thông báo gốc.
- **Cách UI hiển thị:**
  - Lỗi kết nối: hiện trong dialog, đánh dấu bước lỗi.
  - `SshHostKeyUnknown`: hộp xác nhận fingerprint.
  - `ConnectionLost`: thanh báo kèm nút Reconnect.
  - `Internal`: toast kèm "Open log folder".
  - `Cancelled`: không coi là lỗi.
- **Gợi ý theo mã lỗi** (bảng tra trong `core`), tối thiểu gồm: `ECONNREFUSED`, MySQL `1045`, `1049`, `2003`, PG `28P01`, `28000`, `3D000`, SSH `Auth` khi key có passphrase.
- Mọi Tauri command chạy trong lớp bọc bắt panic, trả `Internal`. Task nền gặp lỗi chỉ ghi log và cập nhật trạng thái. Frontend có error boundary cho từng panel.
- **Log:**
  - `tracing` + `tracing-appender`, file xoay theo ngày tại `%APPDATA%\DntDatabase\logs`, giữ 7 ngày.
  - Mức mặc định `info` không ghi nội dung SQL.
  - Mức `debug` bật trong Settings.
  - Secret luôn là `[REDACTED]`.

## 8. UI trong P1

Theo canvas, chỉ dựng phần thuộc P1:

- **Khung chính:** toolbar (các nút của giai đoạn sau hiện ở trạng thái disabled), sidebar cây connection có ô lọc, vùng tab, status bar (connection, phiên bản, có SSH hay không).
- **Cây connection:** nhãn engine (MY/PG/MA), phiên bản, tag SSH, màu theo tag môi trường; menu chuột phải gồm Open/Close, Edit, Duplicate, Delete, New group, Refresh, Copy name.
- **Dialog New/Edit connection:** chọn engine, các tab General/SSH/SSL/Advanced, Test connection hiện thời gian từng bước.
- **Tab object** (mở khi double-click bảng/view): các tab con Info, DDL (chỉ đọc, có nút Copy), Structure (chỉ xem cột, index, FK).
- **Dialog Export/Import connection,** gồm bảng preview và lựa chọn xử lý trùng lặp.

## 9. Testing

1. **Unit** (`cargo test`, không cần DB):
   - `sql_split`, `classify_statement`.
   - Serialize `Value`.
   - Nhận diện biến thể từ chuỗi phiên bản thật, gồm `5.7.44-log`, `8.0.36`, `10.6.16-MariaDB-1:10.6.16+maria~ubu2004`, `5.5.5-10.11.6-MariaDB`.
   - Mã hóa rồi giải mã `.dntconn`; sai passphrase; file hỏng.
   - Parser `.ncx` và giải mã mật khẩu với cặp giá trị đã biết.
   - Migration SQLite.
   - Bảng gợi ý lỗi.
   - `MemorySecretStore`.
2. **Integration** (Docker, bật bằng `DNT_TEST_DB=1`):
   - `docker-compose.test.yml` gồm mysql:5.7, mysql:8.0, mariadb:10.6, mariadb:11.4, postgres:12, postgres:17, openssh-server.
   - Schema mẫu có check, functional index, generated column, partition, enum, array, JSON, sequence, system-versioned table.
   - Ca test chạy qua mọi biến thể:
     - Kết nối và nhận diện phiên bản.
     - Liệt kê object.
     - Snapshot `table_detail`/DDL bằng `insta`.
     - Ghi rồi đọc lại mọi kiểu dữ liệu.
     - Hủy `SLEEP(10)`/`pg_sleep(10)` trong dưới 1 giây.
     - Tự kết nối lại sau `KILL`; có transaction mở thì phải báo lỗi.
     - SSH bằng key và password; host key bị đổi phải bị chặn.
     - TLS `Require`/`VerifyCa`.
   - Test với Credential Manager thật đặt `#[ignore]`, chạy tay.
3. **Frontend** (Vitest + Testing Library): lớp `api/` với `invoke` giả; cây connection (tải lười, lọc, lỗi); dialog kết nối (validate, bước lỗi); luồng export/import.
4. **E2E smoke** (`tauri-driver` + WebdriverIO, Windows): tạo connection MySQL → Test → Save → bung cây → mở DDL.

**CI** (nếu dùng GitHub Actions):

- `windows-latest`: `cargo fmt --check`, `clippy -D warnings`, unit test, vitest, `tauri build`.
- `ubuntu-latest`: integration test với service container.

**Môi trường dev:** rustup (stable, MSVC), Node 22, Docker Desktop, WebView2.

## 10. Tiêu chí hoàn thành P1

- Tạo, sửa, xóa, nhân bản và kéo thả connection và nhóm; dữ liệu còn nguyên sau khi khởi động lại app.
- Kết nối thành công tới cả 6 server trong ma trận test, trực tiếp và qua SSH tunnel; TLS `Require` hoạt động.
- Nhận diện đúng biến thể; cây schema hiển thị đúng object theo biến thể; Info, DDL và Structure hiển thị đúng (khớp snapshot).
- Mật khẩu chỉ nằm trong Credential Manager (kiểm tra `app.db` và log không chứa secret).
- Export rồi import `.dntconn` ra đúng dữ liệu cũ, có và không kèm mật khẩu; import `.ncx` từ Navicat thật thành công.
- Mọi lỗi kết nối hiển thị đúng bước kèm gợi ý; app không crash khi gặp lỗi.
- Tất cả test tầng 1–3 xanh; E2E smoke xanh trên Windows.
