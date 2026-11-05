# Driver MongoDB (`crates/driver-mongo`)

- Ngày: 2026-11-05 (rev 2, sau review độc lập)
- Trạng thái: thiết kế đã được người dùng duyệt từng phần trong phiên brainstorming. Rev 2 sửa 6 lỗi Critical và 15 lỗi
  Important mà review tìm ra; người dùng đã chọn: `execute_atomic`, buffer rồi mới phát header, từ chối Verify CA, và
  các cắt giảm phạm vi. Bản viết này chờ người dùng duyệt.
- Spec này được ưu tiên trước Redis; plan Redis (`2026-11-05-redis-driver.md`) để chờ.
- Cơ sở:
  - Spec core không-SQL `2026-10-31-non-sql-driver-core-design.md`, gọi tắt là "spec core".
  - Các hand-off `2026-10-31-r2-followups.md`, `2026-10-31-r3-followups.md`, `2026-11-04-r4b-followups.md`.
  - Spec driver Redis `2026-11-05-redis-driver-design.md`.
- Văn xuôi tiếng Việt; tên định danh, lệnh và mọi chữ trên UI là tiếng Anh.

## 1. Mục tiêu

Cài một driver MongoDB thật cho các hợp đồng R2 (`Driver`, `Session`, `CommandSession`, `CommandLanguage`,
`Introspector`, `DataAccess`) và đăng ký nó bằng một dòng trong `default_drivers()`. Toàn bộ UI và backend R4a/R4b khi
đó chạy với MongoDB thật: dialog, cây `Collections`/`Views`, console, kết quả Table/JSON, data view và sửa document.
Để làm được, core và app cần vài thay đổi nhỏ, liệt kê ở §9.

**Phạm vi bản đầu:**

- MongoDB ≥ 6.0.
- Một host, URI `mongodb+srv://`, hoặc replica set nhiều host. Đường nhiều host và SRV không đi qua SSH tunnel.
- TLS (Off, Unverified, Verify full).
- Auth SCRAM với auth database.

**Ngoài phạm vi:**

- Auth x509, LDAP và Kerberos.
- TLS Verify CA (§5.1).
- Mongo transaction trong console.
- Change streams, GridFS.
- `bulkWrite`.
- Explain panel; console vẫn có `.explain()` dạng result.
- Gợi ý tên field, AI cho Mongo.
- Import/export.
- Data view cho view (view chỉ có tab DDL).

**Lời hứa an toàn của spec core giữ nguyên:**

- Không lệnh nào tới server mà không qua plan và guard.
- Connection read-only không bao giờ nhận lệnh ghi.
- Production phải xác nhận.
- Không tự retry, trừ ngoại lệ commit/abort transaction ở §6.4.
- Secret không vào History, log hay Favorites.

## 2. Các quyết định đã chốt

| # | Quyết định |
|---|---|
| M1 | **Crate `mongodb` 3.x chính thức** với `default-features = false` và `features = ["compat-3-0-0", "rustls-tls", "dns-resolver"]`. `rustls-tls` dùng ring; `dns-resolver` (hickory) cần cho SRV. BSON dùng bản re-export `mongodb::bson`, không thêm crate `bson` riêng. Driver tắt `retry_writes` và `retry_reads`. |
| M2 | **Console là tập con cú pháp mongosh, không chạy JS** (D3), gồm `show dbs/collections`, các modifier `sort/limit/skip/project/maxTimeMS/count/explain`, các helper `db.*` và `.explain()`. Literal hiểu được cả helper kiểu mongosh lẫn Extended JSON v2. |
| M3 | **Phân loại theo grammar**, không hỏi server. `runCommand`/`adminCommand` tra key đầu tiên trong một bảng viết tay, có targets, drops và `unbounded` riêng. Key lạ → `Unknown`. |
| M4 | **Cancel bằng `killOp`** theo `comment` gắn vào lệnh. Hết `cancel_grace` thì bỏ future của lệnh và dựng lại `Client` (`shutdown().immediate(true)`). |
| M5 | **Atomicity:** thêm method core `CommandSession::execute_atomic`. Backend gọi method này khi plan sửa dữ liệu là atomic. Driver bọc `start_transaction` → `commit`, và `abort` khi có lỗi, rồi báo `RolledBack`. Standalone thì sửa từng document một; R4a chặn kiểu sửa này trên Production. |
| M6 | **Cột của kết quả:** gom document tới giới hạn dòng, tính bộ cột cuối cùng, rồi gọi `result_start` đúng một lần. |
| M7 | **TLS:** không hỗ trợ Verify CA (crate rustls không tắt được kiểm tra hostname). Đi qua SSH tunnel thì chỉ cho Off/Unverified. Client cert phải là một file PEM gộp cert và key; driver không ghi file khoá tạm. |
| M8 | **Test với MongoDB 6.0** (standalone) **và 8.0** (replica set một node, TLS, keyFile). |

## 3. Cấu trúc crate

```
crates/driver-mongo/src/
  lib.rs          MongoDriver: Driver (kind Mongo, command_language(), connect)
  client.rs       ClientOptions từ ResolvedConfig + DriverOptions::Mongo; TLS; hello/buildInfo
  session.rs      MongoSession: Session + CommandSession; current db, read-only, ping, close, rebuild
  cancel.rs       MongoCanceller + trạng thái lệnh đang chạy (dùng chung với session)
  shell/
    lexer.rs      token kiểu JS: identifier, chuỗi ('…' "…" `…`), số, /regex/flags, dấu câu, comment
    literal.rs    literal kiểu JS + helper mongosh + Extended JSON v2 → Bson (§4.3)
    parse.rs      grammar §4.2 → ShellCommand
    classify.rs   ShellCommand → CommandClass, targets, unbounded, drops (§4.4)
    redact.rs     §4.5
    mod.rs        MongoLanguage: CommandLanguage (split, parse, redact)
  exec.rs         ShellCommand → lời gọi driver; allowlist read-only; buffer → RowSink
  rows.rs         document → cột (§5.4); Bson ↔ Value (§7)
  introspect.rs   Introspector (§6.1)
  data.rs         DataAccess (§6.2–6.3); execute_atomic (§6.4)
```

`Cargo.lock` không được có `aws-lc-rs`, `aws-lc-sys`, `openssl-sys` hay `native-tls`. Phải kiểm cả cây phụ thuộc của
hickory.

## 4. Console

### 4.1 Tách lệnh (`split`)

- Một lệnh kết thúc bằng `;`, hoặc ở cuối dòng khi các ngoặc `( [ {` đã đóng hết.
- Có một ngoại lệ: nếu token đầu tiên của dòng kế tiếp (bỏ qua dòng trống và comment) bắt đầu bằng `.`, dòng đó viết
  tiếp lệnh trước. Như vậy `db.c.find({})⏎  .sort({a: 1})` vẫn là một lệnh.
- Không tách bên trong chuỗi, regex hay comment.
- `//` và `/* */` là comment.
- `range` của span tính theo byte.
- `split` không bao giờ panic và không cần session.

### 4.2 Grammar

```
stmt     := "use" dbName
          | "show" ("dbs"|"databases"|"collections"|"tables")
          | dbRef "." dbMethod "(" args? ")" modifier*
          | dbRef "." collRef "." collMethod "(" args? ")" modifier*
          | dbRef "." collRef ".explain(" verbosity? ")." collMethod "(" args? ")"
dbRef    := "db" | "db.getSiblingDB(" string ")"
collRef  := ident ("." ident)* | "[" string "]" | "getCollection(" string ")"
modifier := "." ("sort"|"limit"|"skip"|"project"|"projection"|"maxTimeMS"|"count"|"toArray"|"pretty"
                  |"explain") "(" args? ")"
dbName   := token không có khoảng trắng, hoặc string
args     := literal ("," literal)*
```

**Phân biệt method của db và collection:**

- `db.<name>(` là method của db. Nếu `<name>` không có trong danh sách db method thì báo lỗi `Unknown method <name>()`.
- `db.<a>.<b>…` là collection.
- Trong một chuỗi chấm `db.a.b.c(…)`, identifier ngay trước `(` là method, các phần trước đó ghép lại thành tên
  collection (`system.users`, `fs.files`).

**Method của collection:**

| Nhóm | Method |
|---|---|
| Đọc | `find`, `findOne`, `countDocuments`, `estimatedDocumentCount`, `distinct`, `aggregate`, `getIndexes`, `stats` |
| Ghi | `insertOne`, `insertMany`, `updateOne`, `updateMany`, `replaceOne`, `deleteOne`, `deleteMany`, `findOneAndUpdate`, `findOneAndReplace`, `findOneAndDelete` |
| Cấu trúc | `createIndex`, `createIndexes`, `dropIndex`, `dropIndexes`, `renameCollection`, `drop` |

**Method của db:** `getCollectionNames`, `stats`, `serverStatus`, `version`, `createCollection`, `dropDatabase`,
`runCommand`, `adminCommand`.

**Modifier:**

- Chỉ hợp lệ sau `find` hoặc `aggregate`.
- `count()` đổi `find` thành `countDocuments`.
- `toArray()` và `pretty()` không làm gì.
- Modifier đứng sai chỗ báo lỗi `.<m>() is not valid after <method>()`.

**Explain:**

- `verbosity` là `"queryPlanner"`, `"executionStats"` hoặc `"allPlansExecution"`; mặc định `queryPlanner`.
- Lệnh gốc khác Read thì chỉ nhận `queryPlanner`. Verbosity khác báo lỗi
  `Only "queryPlanner" explain is allowed for <method>()`.

**`use`:** tên db không hợp lệ (rỗng, dài hơn 64 byte, hoặc chứa `/\. "$*<>:|?`) báo lỗi parse `Invalid database name`.

**Từ chối JS:** cú pháp ngoài grammar (`var`, `let`, `const`, `=>`, `function`, `load(`, `eval(`, `.forEach(`, `.map(`,
dấu gán `=`, toán tử số học) báo lỗi parse kèm offset:
`Only MongoDB shell commands are supported; JavaScript is not executed`. Các lệnh quản lý user (`createUser`,
`changeUserPassword`, `db.auth`) không có trong grammar nên luôn bị chặn ở bước parse; muốn dùng thì phải gọi qua
`runCommand`.

### 4.3 Literal

**Cú pháp object kiểu JS:**

- Key không nháy (identifier hoặc `$op`), hoặc key trong nháy đơn/kép.
- Cho phép dấu phẩy thừa ở cuối.
- Array; chuỗi `'…'`, `"…"`, `` `…` `` (không có `${}`); `true`, `false`, `null`.

**Số:** giống mongosh.

- Số nguyên nằm trong khoảng Int32 → `Int32`.
- Các số khác (số nguyên lớn hơn, số thực, số mũ) → `Double`.
- Có số âm.

**Regex:** `/pattern/flags` → `Regex`.

**Helper:**

| Helper | Kết quả |
|---|---|
| `ObjectId("24hex")` | `ObjectId` |
| `ObjectId()`, `new ObjectId()` | `ObjectId` mới |
| `ISODate("…")`, `new Date("…")` | `DateTime` |
| `ISODate()`, `new Date()` | Thời điểm hiện tại |
| `NumberLong(n)`, `NumberLong("n")` | `Int64` |
| `NumberInt(n)` | `Int32` |
| `NumberDecimal("…")` | `Decimal128` |
| `UUID("…")` | `Binary` subtype 4 |
| `BinData(t, "b64")` | `Binary` |
| `Timestamp(t, i)` | `Timestamp` |
| `MinKey`, `MaxKey` | `MinKey`, `MaxKey` |

**Extended JSON v2**, cả dạng canonical lẫn relaxed: object có đúng một key wrapper được đọc thành kiểu BSON tương ứng.

| Wrapper | Kiểu BSON |
|---|---|
| `{"$oid"}` | `ObjectId` |
| `{"$date"}`: chuỗi ISO, số mili giây, hoặc `{"$numberLong"}` | `DateTime` |
| `{"$numberLong"}` | `Int64` |
| `{"$numberInt"}` | `Int32` |
| `{"$numberDouble"}`, gồm `"NaN"`, `"Infinity"` | `Double` |
| `{"$numberDecimal"}` | `Decimal128` |
| `{"$binary": {base64, subType}}` | `Binary` |
| `{"$uuid"}` | `Binary` subtype 4 |
| `{"$regularExpression"}` | `Regex` |
| `{"$timestamp"}` | `Timestamp` |
| `{"$minKey": 1}`, `{"$maxKey": 1}` | `MinKey`, `MaxKey` |

Wrapper sai dạng là lỗi, không bị coi là một document con.

**Lỗi:** helper sai tham số, hoặc số vượt khoảng mà wrapper cho phép, báo lỗi kèm offset.

**Dùng chung:** parser này còn đọc `raw_json` của data view (§6.2) và giá trị ô JSON khi sửa (§6.3).

### 4.4 Phân loại

| Lệnh | Class | targets / drops / unbounded |
|---|---|---|
| Method đọc, `show …`, `db.getCollectionNames`/`stats`/`serverStatus`/`version` | Read | |
| `aggregate` không có `$out`/`$merge` ở bất kỳ độ sâu nào (§4.4.2) | Read | |
| `aggregate` có `$out` | Write | target là collection đích; drops `Object(dest)` vì `$out` thay toàn bộ collection đích |
| `aggregate` có `$merge` | Write | target là collection đích |
| `insert*`, `update*`, `replaceOne`, `delete*`, `findOneAnd*` | Write | |
| `updateMany`/`deleteMany` với filter `{}` | Write | `unbounded` |
| `createCollection`, `createIndex(es)`, `dropIndex(es)` | Ddl | |
| `renameCollection(to)` | Ddl | |
| `renameCollection(to, true)` | Ddl | drops `Object(to)` |
| `drop` | Ddl | drops `Object(coll)` |
| `dropDatabase` | Ddl | drops `Database(<db của lệnh>)` |
| `use db` | SessionChange | `UseDatabase(db)` |
| `.explain(…)` / `.explain().<m>(…)` | Class của lệnh gốc | targets/drops/unbounded của lệnh gốc |
| `runCommand` / `adminCommand` | §4.4.1 | §4.4.1 |

**Db của lệnh** (dùng cho cả targets và drops):

- Tham số của `getSiblingDB(…)` nếu lệnh có.
- `"admin"` với `adminCommand`.
- Còn lại: `None`, nghĩa là db hiện tại lúc chạy.

**Target:** mọi lệnh trên collection có `targets = [{ database: <db của lệnh>, name: coll }]`. Theo `policy`, tên này là
`confirm_name` trên Production cho các lệnh `unbounded`, các lệnh có drop, và các lệnh `Unknown`.

#### 4.4.1 Bảng `runCommand`

Tên lệnh là key đầu tiên của document.

**Target và drop:**

- Target là `{ database: <db của lệnh>, name: <giá trị chuỗi của key đầu> }`.
- `dropDatabase` có target và drop là `Database(<db của lệnh>)`. Ví dụ: `db.adminCommand({dropDatabase: 1})` drop
  `admin`.

**Phân loại:**

| Class | Lệnh |
|---|---|
| Read | `ping`, `hello`, `isMaster`, `buildInfo`, `serverStatus`, `dbStats`, `collStats`, `listDatabases`, `listCollections`, `listIndexes`, `find`, `aggregate` (không có `$out`/`$merge`), `count`, `distinct`, `getMore`, `connectionStatus`, `hostInfo`, `getParameter`, `currentOp`, `dataSize` |
| Như lệnh bên trong | `explain: {…}`: class, targets và drops của lệnh bên trong; lệnh bên trong không phải Read thì phải có `verbosity: "queryPlanner"` |
| Write | `insert`, `update`, `delete`, `findAndModify` |
| Write | `aggregate` có `$out`/`$merge` (drop như bảng trên) |
| Ddl | `create`, `createIndexes`, `dropIndexes`, `collMod` |
| Ddl + drop | `drop` → `Object`; `dropDatabase` → `Database`; `renameCollection` → `Object(to)` khi có `dropTarget: true` |
| Admin | `validate`, `killOp`, `killCursors`, `setParameter`, `createUser`, `updateUser`, `dropUser`, `grantRolesToUser`, `revokeRolesFromUser`, `createRole`, `dropRole`, `shutdown`, `fsync`, `compact`, `replSetStepDown`, `replSetReconfig` |
| Unknown | Mọi key khác |

**`unbounded`** bật cho:

- `delete` có phần tử `q` là `{}` hoặc thiếu `q`, với `limit: 0`;
- `update` có phần tử `q` là `{}` hoặc thiếu `q`, với `multi: true`.

#### 4.4.2 Tìm `$out` và `$merge`

Driver duyệt đệ quy mọi stage của pipeline, gồm cả các pipeline lồng trong `$facet`, `$lookup.pipeline` và
`$unionWith.pipeline`. Chỉ cần thấy `$out` hoặc `$merge` ở bất kỳ đâu là lệnh thành Write. Exec dùng lại hàm này ở lớp
read-only (§5.3).

### 4.5 Redact

Che bằng `***`:

- Giá trị của mọi key có tên khớp `pwd`, `password` hoặc `secret` (không phân biệt hoa thường, ở mọi độ sâu), kể cả
  trong `setParameter` (ví dụ `ldapQueryPassword`).
- Phần mật khẩu trong chuỗi `mongodb://user:pass@` và `mongodb+srv://user:pass@`. Mật khẩu được tính tới dấu `@` cuối
  cùng trước host, nên mật khẩu có chứa `@` vẫn bị che hết.
- Tham số thứ hai của `db.auth(…)` và `db.changeUserPassword(…)`. Grammar vẫn chặn hai lệnh này, nhưng chúng vẫn được
  redact trước khi vào History.

Dòng không parse được vẫn được redact theo tên key và mẫu URI.

## 5. Kết nối, session, chạy lệnh

### 5.1 Connect

Cả bước này chạy trong `connect_timeout`.

**Dựng `ClientOptions`:**

- Không SRV: dựng trực tiếp từ `ResolvedConfig` và `DriverOptions::Mongo`.
- Có SRV: gọi `ClientOptions::parse("mongodb+srv://<host>/")`, chuỗi này không chứa user hay mật khẩu, rồi gán các
  field bên dưới lên options vừa parse. Bản ghi TXT có thể cung cấp sẵn `authSource` và `replicaSet`; giá trị người dùng
  nhập được ưu tiên hơn.

Không bao giờ dựng chuỗi URI có mật khẩu.

| Field | Giá trị |
|---|---|
| `hosts` | `host:port` cộng `extra_hosts` (khi không SRV) |
| `credential` | Chỉ gán khi `username` không rỗng: SCRAM, `source` = `auth_source`. Nếu `auth_source` rỗng thì dùng `database`; `database` cũng rỗng thì dùng `admin`. |
| `repl_set_name` | `replica_set` |
| `direct_connection` | `true` khi đi qua SSH tunnel; còn lại theo `direct_connection` |
| `app_name` | `cfg.application_name`, hoặc `DntDatabase` khi rỗng |
| `connect_timeout`, `server_selection_timeout` | `connect_timeout` |
| `max_pool_size` | 2 |
| `retry_writes`, `retry_reads` | `false` |

**TLS:**

| `command_tls` | Không qua SSH | Qua SSH tunnel |
|---|---|---|
| `Off` | Không TLS | Không TLS |
| `Unverified` | `allow_invalid_certificates = true` | như cột bên |
| `VerifyCa` | `Config { field: Some("ssl.mode"), message: "Verify CA is not available for MongoDB; use Verify full" }` | như cột bên |
| `VerifyFull` | Mặc định: kiểm chứng CA và hostname | `Config { field: Some("ssl.mode"), message: "Through an SSH tunnel MongoDB can only use Require; the host name cannot be verified" }` |

- **CA:** `ca_path` → `ca_file_path`. Khi trống, crate tin bộ root Mozilla nhúng sẵn (webpki-roots). Driver PG thì chỉ
  tin `ca_path`.
- **Client cert:** `cert_path` → `cert_key_file_path`. File phải là một PEM chứa cả `CERTIFICATE` và `PRIVATE KEY`, nếu
  không thì `Config` trên `ssl.cert_path`: `The client certificate must be one PEM file with the certificate and its
  private key`. Nếu `key_path` có giá trị thì báo `Config` trên `ssl.key_path`: `MongoDB takes the key inside the
  certificate file`. Không hỗ trợ khoá có mật khẩu.
- **SRV ngầm định TLS:** SRV mà SSL mode là `Disable` thì báo `Config` trên `ssl.mode`:
  `mongodb+srv connections use TLS; choose Require or Verify full`.

**Kiểm tra server:**

1. Gọi `hello`, rồi `buildInfo`.
2. Version < 6.0 → `Unsupported { feature: "MongoDB before 6.0", version: <version> }`.
3. Server là mongos (`hello.msg == "isdbgrid"`) vẫn được phép. Tài liệu ghi rõ sharded cluster chưa được test; driver
   không phát notice lúc connect.

**Lỗi lúc connect:**

| Lỗi | `DbError` |
|---|---|
| `AuthenticationFailed` | `Auth` |
| Hết thời gian chọn server | `Connect`, kèm danh sách host |
| SRV không phân giải được | `Connect`: `SRV lookup failed for <host>` |

**`ServerInfo`:**

- kind và variant `Mongo`; version lấy từ `buildInfo`.
- `capabilities = for_kind(Mongo)`, với `transactions` bật khi `hello` có `setName` hoặc là mongos.

**Db ban đầu:** `cfg.database`, hoặc `test` khi trống.

**Chi phí:** mỗi session là một `Client` riêng. Với replica set, mỗi `Client` có thêm connection giám sát (SDAM) tới
từng member. Đây là chi phí đã biết; pool tối đa 2 connection cho lệnh.

### 5.2 Session

- **Trạng thái:** `MongoSession` giữ một `Arc<RwLock<Client>>` (có thể dựng lại), `current_db`, `read_only`,
  `server_info`, và một `Arc` trạng thái lệnh đang chạy (`Running { uuid, class }`) dùng chung với canceller.
- **`current_database()`** trả `Some(current_db)`. `use` đổi giá trị này. Khi `Client` được dựng lại, giá trị vẫn giữ
  nguyên, để `DatabaseChanged` của `run_commands` luôn so đúng.
- **`tx_state`:** luôn `Idle`. Transaction chỉ tồn tại bên trong một lần gọi `execute_atomic`.
- **Lỗi mạng giữa lệnh:**
  - lệnh `Read` → `ConnectionLost`;
  - lệnh khác → `ConnectionLost` với `outcome_unknown`.

  Không retry. `Client` tự mở connection cho lệnh kế tiếp. `reconnects()` tăng khi driver thấy lỗi mạng hoặc dựng lại
  `Client`.
- **`ping`:** `runCommand({ping: 1})` trên `admin`.
- **`close`:** `Client::shutdown().immediate(true)`.
- **`keyspace()`:** `None`.

### 5.3 Read-only: hai lớp độc lập

**Lớp 1** là guard của `run_commands`. Nó dựa trên class do §4.4 tính ra.

**Lớp 2** nằm trong `exec.rs` và không đọc `cmd.class`. Trên connection read-only, exec chỉ cho phép đúng các lời gọi
driver sau (allowlist):

- `find`, `findOne`;
- `countDocuments`, `estimatedDocumentCount`, `distinct`;
- `aggregate`, sau khi tự duyệt lại pipeline bằng §4.4.2 và không thấy `$out`/`$merge`;
- `listDatabases`, `listCollections`, `listIndexes`;
- `buildInfo`, `serverStatus`, `dbStats`, `$collStats`, `ping`, `hello`;
- explain với verbosity `queryPlanner` của các lệnh trên;
- `runCommand` với key thuộc nhóm Read của §4.4.1;
- `use`, sau khi đã kiểm tra quyền.

Mọi lời gọi khác, hoặc payload có drops hay `unbounded`, bị từ chối với
`Config { field: None, message: "This connection is read-only" }` trước khi gửi.

### 5.4 Chạy lệnh (`exec.rs`)

**Comment.** Mỗi lệnh gửi kèm `comment: "dnt:<uuid>"`. Với `run_command`, `comment` được đặt vào trong document. `uuid`
ghi vào trạng thái `Running`.

**Cursor** (`find`, `aggregate`, `show collections`, `listIndexes`, `distinct`):

1. Kéo theo batch và gom document vào bộ đệm cho tới khi đủ `opts.max_rows` + 1, hết cursor, hoặc `opts.stop` được bật.
2. Tính bộ cột: `_id` đầu tiên, các field khác theo lần xuất hiện đầu tiên trong toàn bộ bộ đệm.
3. Gọi `result_start` đúng một lần, rồi `rows` theo lô, rồi `result_end`. `truncated` bật khi còn document chưa đọc.
4. Cursor chưa hết thì được drop; crate tự gửi `killCursors`.

Bộ nhớ bị chặn bởi `max_rows`. Data view truyền `limit + 1`, nên chỉ đọc đúng một trang.

**Giá trị ô:** theo `opts.cells`, giống SQL. Với `Preview`, chuỗi và binary dài bị cắt theo quy tắc `Value` của core.
Với `Full { max_cell_bytes }`, giới hạn đó được tôn trọng.

**Kết quả các lệnh khác:**

| Lệnh | Kết quả |
|---|---|
| Trả một document (`findOne`, `runCommand`, `stats`, `explain`, `serverStatus`, `findOneAnd*`) | Một dòng; không có document thì không có dòng |
| `countDocuments`, `estimatedDocumentCount`, `version` | Một cột `value` |
| `distinct` | Cột `value`, mỗi giá trị một dòng |
| `getCollectionNames`, `show collections` | Một cột `name` |
| `show dbs` | Hai cột `name`, `sizeOnDisk` (gọi `listDatabases` không có `nameOnly`) |
| `db.c.stats()` | Gửi `aggregate([{$collStats: {storageStats: {}}}])`, vì lệnh `collStats` đã bị deprecate từ 6.2 |

**Lệnh ghi:**

| Lệnh | `affected` |
|---|---|
| `insertOne` | 1 |
| `insertMany` | Số document đã chèn |
| `updateOne` / `updateMany` / `replaceOne` | `matchedCount` (backend hiểu `affected` là số dòng khớp) |
| `delete*` | `deletedCount` |
| `findOneAnd*` | 1 nếu có document, 0 nếu không |

- Mỗi lệnh ghi còn trả thêm một dòng ở cột `result` (`Json`) chứa `acknowledged`, `matchedCount`, `modifiedCount`,
  `deletedCount`, `insertedIds` và `upsertedId`.
- `insertMany` ordered lỗi giữa chừng → `Server` với message `Inserted <n> of <m> before the error: <msg>`, kèm
  `outcome_unknown: false`, vì số đã chèn là chính xác.

**Timeout:**

- Lệnh đọc gửi `maxTimeMS = min(<giá trị người dùng đặt>, opts.timeout)`. Server báo `MaxTimeMSExpired` → `Timeout`.
- Lệnh ghi hết `opts.timeout` thì cancel như §5.5.

**`use x`:**

1. Chạy `listCollections {nameOnly: true, authorizedCollections: true}` trên `x`, để báo lỗi quyền ngay lúc đổi db.
2. Thành công thì đặt `current_db = x`. Db chưa tồn tại vẫn hợp lệ, giống mongosh.

### 5.5 Cancel

**Phần backend làm:** bật `opts.stop`, rồi gọi `Canceller::cancel()`.

**Phần `MongoCanceller` làm** (canceller không thấy `opts`):

1. Đọc `Running.uuid`. Không có lệnh đang chạy thì không làm gì.
2. Trên `Client` hiện tại, chạy
   `aggregate([{$currentOp: {allUsers: false}}, {$match: {"command.comment": "dnt:<uuid>"}}])` trên `admin`, rồi
   `killOp` từng `opid`. `getMore` mang theo comment của lệnh gốc, nên cursor cũng được tìm thấy.
3. Không tìm thấy op nào (cancel rơi vào giữa hai lệnh) thì không làm gì.
4. Canceller không giữ `Client` riêng. Nó đọc qua `Arc<RwLock<Client>>` dùng chung, nên vẫn dùng được sau khi `Client`
   được dựng lại.

**Phần `execute_command` làm:**

1. Khi thấy `opts.stop` được bật, phát `ExecProgress::CancelSent` và chờ tối đa `opts.cancel_grace` để lệnh trả lỗi
   `Interrupted`.
   - Lệnh đọc → `Cancelled { outcome_unknown: false }`.
   - Lệnh ghi → `Cancelled { outcome_unknown: true }`.
2. Hết grace mà lệnh chưa trả:
   1. Phát `ExecProgress::GraceExpired`.
   2. Bỏ future của lệnh.
   3. Thay `Client` bằng một bản mới dựng lại, rồi gọi `shutdown().immediate(true)` trên bản cũ ở background.
   4. Tăng `reconnects()`.
   5. Báo `Cancelled` với `outcome_unknown` như bước 1.
3. Lệnh đã hoàn tất thành công sau khi stop được bật → trả kết quả, kèm `late_completion: true`.

Trên mongod, user luôn kill được op của chính mình, kể cả khi không có quyền `killOp`. Vì vậy đường dựng lại `Client`
chỉ xảy ra khi hết grace.

### 5.6 Lỗi

| Lỗi | `DbError` |
|---|---|
| Lỗi lệnh của server | `Server { code: Some(codeName), … }`; thiếu `codeName` thì dùng mã số |
| `Unauthorized` | `Server`, hint `The user lacks the role for this command` |
| `DuplicateKey` (11000) | `Server`, hint `A unique index rejected a duplicate value`; message của server có tên index |
| `NotWritablePrimary` | `Server`, hint `Connected to a secondary; writes need the primary` |
| `DocumentValidationFailure` | `Server`, kèm `errInfo` trong message |
| `MaxTimeMSExpired` | `Timeout` |
| Lỗi mạng hoặc I/O | `ConnectionLost` (§5.2) |
| Payload sai kiểu | `Internal` |

Các hint là chuỗi tĩnh, thêm vào `static_hint` của core theo `code`.

## 6. Cây, data view, sửa document

### 6.1 `Introspector`

**`list_databases`:** `listDatabases {nameOnly: true, authorizedDatabases: true}`. Driver trả hết; app lọc db hệ thống
(§9).

**`list_objects`:**

| Kind | Lệnh |
|---|---|
| `Collection` | `listCollections {filter: {type: {$in: ["collection", "timeseries"]}}, nameOnly: true, authorizedCollections: true}` |
| `View` | `listCollections {filter: {type: "view"}, …}` |
| Kind khác | `[]` |

- Collection `system.*` vẫn được trả; app lọc khi "Show system objects" tắt (§9).
- Timeseries được liệt kê như collection. Sửa document trên timeseries bị server giới hạn; lỗi của server hiện như mọi
  lỗi apply khác.
- `row_estimate` để `None`.

**`list_schemas`, `list_columns`:** `[]`.

**`table_detail`:**

- **Mẫu cố định:** `aggregate([{$sort: {_id: -1}}, {$limit: 100}])`, tức 100 document có `_id` lớn nhất. Không dùng
  `$sample`, để preview và apply luôn thấy cùng một schema.
- **Field:** mỗi field cấp một là một `ColumnDetail`.
  - Thứ tự: `_id` đầu tiên, các field khác theo lần xuất hiện đầu tiên.
  - `db_type` là tên kiểu BSON: `objectId`, `string`, `int`, `long`, `double`, `decimal`, `bool`, `date`, `object`,
    `array`, `binData`, `uuid`, `timestamp`, `regex`, `null`. Field có nhiều kiểu thì `db_type` là `mixed(a|b)`, các
    kiểu xếp theo chữ cái.
  - `nullable` luôn `true`, trừ `_id`.
- **Index:** từ `listIndexes`. Index `_id_` là primary; `unique` lấy từ option của index.
- **Collection rỗng:** chỉ có `_id: objectId`.

**`object_ddl`:**

- Collection: lấy `options` (gồm `validator`) từ `listCollections {filter: {name}}`, trả JSON có thụt dòng.
- View: trả `{ viewOn, pipeline }`.

### 6.2 `DataAccess::page` và `count`

- `page`: `find(filter).sort(sort + {_id: 1}).skip(skip).limit(limit)`. `_id` luôn là khoá sort cuối, để phân trang ổn
  định.
- `count`: `countDocuments(filter)`.
- Cả hai trả class `Read` và qua được `check_read`.

**Filter có cấu trúc:**

| Toán tử | Mongo |
|---|---|
| `Eq` / `Ne` | `$eq` / `$ne` |
| `Lt` / `Le` / `Gt` / `Ge` | `$lt` / `$lte` / `$gt` / `$gte` |
| `Like` / `NotLike` | `$regex: "^…$"` / `$not` (`%` → `.*`, `_` → `.`, phần còn lại escape) |
| `Contains` / `StartsWith` / `EndsWith` | `$regex` có neo, nội dung đã escape |
| `In` | `$in` |
| `Between` | `$gte` + `$lte` |
| `IsNull` | `$eq: null` |
| `IsNotNull` | `$ne: null` |

**Ép kiểu giá trị** theo `db_type` đã suy:

| `db_type` | Ép thành |
|---|---|
| `int` / `long` / `double` / `decimal` | Số tương ứng |
| `objectId` | `ObjectId`, khi chuỗi có đúng 24 ký tự hex |
| `bool` | Boolean |
| `date` | DateTime (ISO 8601) |
| `uuid` | UUID |
| `string`, `mixed(…)` | Giữ là chuỗi |

Ép không được → `Config { field: Some("filter"), message: "Value \"<v>\" is not a valid <type>" }`.

**Filter raw:**

- `raw_json` là một document parse bằng §4.3. Cú pháp mongosh và Extended JSON đều được, ví dụ
  `{_id: ObjectId("…")}` và `{"_id": {"$oid": "…"}}`.
- Driver tự kiểm tra: kết quả phải là document, và không chứa `$where`, `$function` hay `$accumulator` ở bất kỳ độ sâu
  nào. Message lỗi giống R4a.
- App bỏ bước kiểm tra `check_raw_json` dựa trên serde cho họ Command, để driver nhận được cú pháp mongosh (§9).
- `raw_where` trên Mongo vẫn là `Config` (R4a).

### 6.3 `DataAccess::build_edits`

| `RowEdit` | Lệnh |
|---|---|
| `Update` | `updateOne({_id}, {$set: {…}, $unset: {…}})` |
| `Insert` | `insertOne({...})`; thiếu `_id` thì server tự sinh |
| `Delete` | `deleteOne({_id})` |

**`_id`:** lấy từ `RowRef.key` và dựng ngược theo §7. `_id` kiểu `Binary` giữ đủ subtype và byte nhờ cách map ở §7,
nên document có `_id` nhị phân vẫn sửa và xoá được.

**Cách map `CellInput`:**

| `CellInput` | Update | Insert |
|---|---|---|
| `Null` | `$set: null` | field = `null` |
| `Text` | Ép theo `db_type` như §6.2. Với `mixed(…)`: nếu text parse được thành literal §4.3 thì dùng literal đó, không thì giữ là chuỗi. Field `object`/`array`: parse §4.3 và thay cả field. | như Update |
| `Value` | Map theo §7 | như Update |
| `Base64` | `Binary` subtype 0 | như Update |
| `Unset` | `$unset` | Bỏ field |
| `Default` | Lỗi `Default values are not available for MongoDB` | như Update |
| `Loaded` | Lỗi `Cut cells cannot be edited for MongoDB` | như Update |

**Điều kiện từ chối:**

- `_id` nằm trong `set` của Update → lỗi `_id cannot be changed`.
- Tên field có `.` hoặc bắt đầu bằng `$` → lỗi
  `Fields whose name contains "." or starts with "$" cannot be edited here; use the console`.
- Ô đã bị cắt ở `PAGE_CELL_CAP` (64 KiB) không sửa được: `data_cell_fetch` hiện chỉ chạy cho SQL. UI không cho Edit ô
  preview của họ Command (§9).

**`text` của mỗi `ParsedCommand`:** dạng mongosh, ví dụ `db.orders.updateOne({_id: ObjectId("…")}, {$set: {…}})`.
`parse(text)` phải cho lại đúng payload (có test). Phần hiển thị trong preview và History được rút gọn như §9.

### 6.4 `execute_atomic` và apply

Core thêm method sau (§9):

```rust
// CommandSession, default: Err(Unsupported { feature: "transactions" })
async fn execute_atomic(&self, cmds: &[ParsedCommand], opts: &ExecOptions)
    -> Result<Vec<StmtOutcome>, AtomicFailure>;
pub struct AtomicFailure { pub index: usize, pub error: DbError, pub rolled_back: bool }
```

**Driver Mongo:**

1. Mở một `ClientSession` và `start_transaction`.
2. Chạy lần lượt từng lệnh trong transaction đó.
3. Lỗi ở lệnh nào thì `abort` và trả `AtomicFailure { index, error, rolled_back: true }`.
4. Hết lệnh thì `commit_transaction`.

`commit` gặp `UnknownTransactionCommitResult` thì retry thêm một lần; `abort` cũng retry một lần. Đây là ngoại lệ duy
nhất của luật "không tự retry". Đặc tả transaction của MongoDB bắt buộc làm vậy, và commit là idempotent theo
`txnNumber`. Nếu commit vẫn không xác định được kết quả thì trả `ConnectionLost { outcome_unknown: true }`.

**Document không còn:** một `updateOne`/`deleteOne` có `matchedCount = 0` là lỗi
`Server { code: Some("DocumentNotFound"), message: "Document <_id> no longer exists" }`.

- Trong `execute_atomic`: transaction bị abort.
- Khi apply từng lệnh một (standalone): dừng ở lệnh đó.

**Backend:** `command_edit_apply` gọi `execute_atomic` khi `atomic == Yes`. Thất bại map sang `RolledBack`; lỗi
`DocumentNotFound` được đánh dấu `conflict`. Khi `atomic != Yes`, backend vẫn gọi từng `execute_command` như hiện nay.

### 6.5 Fingerprint

Theo spec core §7.5: fingerprint của họ Command chỉ tính kiểu `_id` và danh sách index. Hiện R4a dùng fingerprint chung
(băm mọi cột), nên app phải sửa (§9). Mẫu cố định ở §6.1 là lớp bảo vệ thứ hai.

## 7. Map giá trị

| BSON → `Value` | `Value` → BSON khi sửa |
|---|---|
| `ObjectId` → `Other{"objectId", hex}` | `ObjectId` |
| `Decimal128` → `Other{"decimal", text}` | `Decimal128` |
| `Timestamp` → `Other{"timestamp", "t,i"}` | `Timestamp` |
| `MinKey` / `MaxKey` → `Other{"minKey"/"maxKey", ""}` | như cũ |
| `Regex` → `Other{"regex", "/p/f"}` | `Regex` |
| `Binary` subtype 4 → `Uuid` | `Binary` subtype 4 |
| `Binary` khác → `Other{"binData", "<subtype hex>:<base64>"}` (đủ byte) | `Binary` đúng subtype |
| `DateTime` trong khoảng hiển thị được → `DateTime` (UTC) | `DateTime` |
| `DateTime` ngoài khoảng → `Other{"date", "<millis>"}` | `DateTime` |
| `Int32` / `Int64` → `Int` | Theo `db_type`: `int` → `Int32`, `long` → `Int64`; còn lại thì `Int32` nếu vừa, không thì `Int64` |
| `Double` hữu hạn → `Float`; `NaN` / `±Infinity` → `Other{"double", "NaN"/"Infinity"/"-Infinity"}` | `Double` |
| `Boolean` → `Bool` | `Boolean` |
| `Null` / `Undefined` → `Null` | `Null` |
| `Symbol` → `Text` | `String` |
| `JavaScriptCode`, `CodeWithScope`, `DbPointer` → `Other{"javascript"/"codeWithScope"/"dbPointer", text}` | Chỉ đọc |
| Document / Array → `Json`, dạng **canonical Extended JSON** | Parse lại bằng §4.3, giữ đúng kiểu |

Ô JSON dùng canonical Extended JSON (`{"$numberLong": "5"}`, `{"$date": {"$numberLong": "…"}}`), nên khi lưu lại không
mất kiểu.

## 8. Test

### 8.1 Unit (không cần server)

- **Lexer và literal:**
  - mọi dạng literal, helper (kể cả gọi không tham số và dùng `new`), wrapper Extended JSON;
  - số theo quy tắc mongosh;
  - lỗi có offset; wrapper sai dạng.
- **Grammar:**
  - mọi dạng `stmt` và modifier;
  - `db.system.users.find()`, `db["a b"]`, `getCollection`, `getSiblingDB`;
  - `db.stats()` khác `db.stats.find()`;
  - `.explain()` ở cả hai vị trí; từ chối verbosity khác `queryPlanner` cho lệnh ghi;
  - modifier sai chỗ; JS bị từ chối đúng message; tên db không hợp lệ.
- **`split`:** lệnh nhiều dòng, chain `.sort()` ở dòng sau, `;`, comment, chuỗi hoặc regex chứa `;` và xuống dòng.
- **Phân loại:**
  - mọi hàng ở §4.4 và §4.4.1;
  - `$out` có drop; `$out` nằm trong `$facet` hoặc `$lookup`;
  - `renameCollection` có `dropTarget`;
  - `adminCommand({dropDatabase: 1})` drop `admin`; `getSiblingDB("x").dropDatabase()` drop `x`;
  - `runCommand` `delete` với `q: {}, limit: 0` là `unbounded`;
  - `explain` bọc lệnh ghi; `validate` là Admin.
- **Redact:** mọi mẫu ở §4.5, kể cả mật khẩu có `@` và dòng không parse được.
- **Lớp read-only của exec:** mọi lời gọi ngoài allowlist bị từ chối, kể cả khi `class` bị cố tình đặt sai thành `Read`.
- **Trải cột:** bộ đệm nhiều batch; field mới ở batch sau vẫn ra một `result_start` duy nhất với đủ cột.
- **Map giá trị:** round trip BSON → `Value` → BSON cho mọi hàng ở §7, gồm document lồng có ObjectId/Date/Long, đi qua
  canonical Extended JSON.
- **Filter:** mọi toán tử, ép kiểu thành công và thất bại, escape regex, `_id` là khoá sort cuối.
- **`build_edits`:** mọi hàng `CellInput`, `_id` nhị phân, các điều kiện từ chối; `parse(text)` cho lại đúng payload.

### 8.2 Tích hợp (`DNT_TEST_DB=1`)

**Hạ tầng:**

- `docker-compose.test.yml` thêm hai service:
  - `mongo60`: `mongo:6.0`, standalone, auth, cổng `27060`.
  - `mongo80`: `mongo:8.0`, replica set một node `rs0`, keyFile cho xác thực nội bộ, TLS dùng `testdata/certs`, auth,
    cổng `27080`. Script init chạy `rs.initiate`, tạo user và seed dữ liệu.
- User:
  - `root` / `dnt_test`;
  - `dnt` / `dnt_test` với role `readWrite` trên `dnt_fixture`.
- `testkit::Target` thêm `Mongo60` và `Mongo80`.
- Seed db `dnt_fixture`:
  - collection `orders`: đủ các kiểu ở §7, một field `mixed`, một document có `_id` nhị phân;
  - view `paid_orders`.

**Kết nối:**

- auth đúng, sai mật khẩu → `Auth`;
- TLS Verify full; Unverified; Verify CA → lỗi `Config`;
- client cert PEM gộp; `key_path` có giá trị → lỗi;
- `replica_set: rs0`;
- qua SSH tunnel với `directConnection` và TLS Unverified; Verify full qua tunnel → lỗi.
- Phần SRV có unit test cho options dựng được, không cần DNS thật.

**Console:**

- `find` có `sort`/`limit`/`project`; một `find` có `batchSize` nhỏ trên các document khác field nhau ra một result với
  đủ cột;
- `aggregate` có `$out` ghi được;
- `insert*`/`update*`/`delete*` ra đúng `affected`;
- `use other` rồi `find`;
- `show dbs`, `show collections`;
- `.explain()`;
- `runCommand({ping: 1})`.

**Guard qua `run_commands`:**

- `updateMany({})` trên Production bắt gõ tên collection;
- `adminCommand({dropDatabase: 1})` bắt gõ `admin`;
- read-only chặn `insertOne` (`countDocuments` không đổi).

**Retry tắt:** bật profiler level 2 trên một db test, cho lệnh ghi lỗi mạng giữa chừng (kill connection từ phía
server), rồi kiểm profiler chỉ có một bản ghi.

**Timeout và cancel:**

- `maxTimeMS` → `Timeout`;
- `find` chậm (`$where` có `sleep`, chỉ dùng trong test) bị `killOp`, rồi lệnh kế tiếp chạy được;
- lệnh ghi bị cancel → `outcome_unknown: true`;
- grace rất nhỏ → `Client` được dựng lại, `reconnects()` tăng, `current_database()` vẫn đúng.

**Cây và data view:**

- `listCollections` có cả view;
- `table_detail` suy ra `mixed(…)` và ổn định giữa hai lần gọi;
- `page` với filter có cấu trúc và filter raw dạng mongosh lẫn Extended JSON; `count`.

**Sửa:**

- sửa ô, `Unset`, thêm, xoá, document có `_id` nhị phân;
- sửa ô JSON lồng giữ đúng kiểu;
- trên `mongo80`, lệnh thứ hai của `execute_atomic` lỗi thì không lệnh nào được ghi, kết quả `rolled_back`;
- trên `mongo60`, apply không atomic, và bị chặn trên Production;
- document bị xoá ở nơi khác → `DocumentNotFound`;
- `close` khi cursor còn mở không bị treo.

### 8.3 App, UI, e2e

- `default_drivers()` thêm `MongoDriver`.
- Test `src-tauri`:
  - `available_engines` có Mongo;
  - một ca end-to-end qua `AppState`: mở connection, plan và run `db.orders.find({})`, mở data view, sửa một ô bằng
    `execute_atomic` trên `mongo80`, và kiểm History đã redact `createUser` gửi qua `runCommand`.
- Các thay đổi app ở §9 có test riêng. Fake driver cài `execute_atomic` cho các test R4a.
- Backend giả của e2e đổi dòng Mongo sang dạng trải theo field cấp một; spec `command-query` và `command-data` cập
  nhật theo.

## 9. Thay đổi ngoài crate

| Chỗ | Thay đổi |
|---|---|
| `crates/core/src/command.rs` | Thêm `CommandSession::execute_atomic` và `AtomicFailure` (§6.4); mặc định `Unsupported`. |
| `crates/core/src/error.rs` | `static_hint` cho `Unauthorized`, `DuplicateKey`, `NotWritablePrimary`. |
| `crates/core/src/conn_string.rs` | URI Mongo: các option chỉ đổi hành vi driver (`retryWrites`, `w`, `appName`, `journal`, `wtimeoutMS`, `maxPoolSize`, `readPreference`) được nhận và bỏ qua thay vì báo lỗi. `authSource` thiếu thì lấy db trong path. URI Atlas dán vào là dùng được. |
| `crates/testkit/src/fake_command.rs` | Cài `execute_atomic`: chạy lần lượt, lỗi thì hoàn tác trong store. |
| `src-tauri/src/edit_plans.rs` | `atomic == Yes` → gọi `execute_atomic`; thất bại → `RolledBack`; `DocumentNotFound` → `conflict`. Fingerprint riêng cho họ Command: kiểu `_id` và danh sách index (§6.5). Text lệnh trong preview và History rút gọn như `display_sql` (giữ 4 KiB, thêm `…`, đánh dấu đã rút gọn). |
| `src-tauri/src/commands/data.rs` | Họ Command bỏ `check_raw_json` dựa trên serde; driver tự kiểm tra (§6.2). |
| `src-tauri/src/commands/browse.rs` | Lọc db và collection hệ thống của họ Command theo "Show system objects": Mongo ẩn `admin`, `local`, `config` và collection `system.*`. |
| UI data view | Họ Command không cho Edit ô preview đã bị cắt. |
| e2e-web fake | Dòng Mongo trải theo field. |

## 10. Rủi ro và cách giữ

| Rủi ro | Cách giữ |
|---|---|
| Crate `mongodb` hoặc hickory kéo theo aws-lc-rs hoặc OpenSSL | `default-features = false`; chỉ bật `rustls-tls` (ring) và `dns-resolver`. Plan có bước kiểm `Cargo.lock`. |
| Parser nhận nhầm JS | Grammar đóng; có test từ chối cho từng cấu trúc JS hay gặp. |
| Lệnh ghi lọt thành Read | Duyệt đệ quy `$out`/`$merge`; `runCommand` lạ là `Unknown`; lớp read-only của exec dùng allowlist riêng, không dựa vào `class`. |
| Explain chạy thật lệnh ghi | Lệnh ghi chỉ nhận verbosity `queryPlanner`. Class vẫn là của lệnh ghi. |
| Retry ngầm | Tắt `retry_writes`/`retry_reads`, có test profiler. Ngoại lệ commit/abort được ghi rõ ở §6.4. |
| Mật khẩu lọt ra | Không dựng URI có mật khẩu; không có file khoá tạm; redact theo tên key ở mọi độ sâu. |
| Mất kiểu khi sửa | Ô JSON dùng canonical Extended JSON; parser đọc được wrapper; có test round trip. |
| Apply báo "structure changed" ngẫu nhiên | Fingerprint của họ Command chỉ tính `_id` và index; mẫu `table_detail` cố định. |
| Cancel làm treo | `shutdown().immediate(true)` chạy ở background; future của lệnh bị bỏ sau grace. |
