# Driver MongoDB (`crates/driver-mongo`)

- Ngày: 2026-11-05
- Trạng thái: thiết kế đã được người dùng duyệt từng phần trong phiên brainstorming. Bản viết này chờ người dùng duyệt.
  Spec này được ưu tiên trước Redis; plan Redis (`2026-11-05-redis-driver.md`) để chờ.
- Cơ sở:
  - Spec core không-SQL `2026-10-31-non-sql-driver-core-design.md`, gọi tắt là "spec core".
  - Các hand-off `2026-10-31-r2-followups.md`, `2026-10-31-r3-followups.md`, `2026-11-04-r4b-followups.md`.
  - Spec driver Redis `2026-11-05-redis-driver-design.md`, dùng cùng cách làm cho read-only và cancel.
- Văn xuôi tiếng Việt; tên định danh, lệnh và mọi chữ trên UI là tiếng Anh.

## 1. Mục tiêu

Cài một driver MongoDB thật cho các hợp đồng R2 (`Driver`, `Session`, `CommandSession`, `CommandLanguage`,
`Introspector`, `DataAccess`). Driver được đăng ký bằng một dòng trong `default_drivers()`. Sau đó toàn bộ UI và backend
R4a/R4b chạy được với MongoDB thật: dialog, cây `Collections`/`Views`, console, kết quả Table/JSON, data view, và sửa
document.

Phạm vi bản đầu theo D1/D2 của spec core:

- MongoDB ≥ 6.0.
- Kết nối một host, URI `mongodb+srv://`, hoặc replica set nhiều host. Đường nhiều host không đi qua SSH tunnel.
- TLS, auth database.

Ngoài phạm vi:

- Mongo transaction trong console.
- Change streams và GridFS.
- Explain panel. Console vẫn có `.explain()` trả về dạng result.
- Gợi ý tên field, AI cho Mongo.
- Import/export.

Lời hứa an toàn của spec core giữ nguyên:

- Không lệnh nào tới server mà không qua plan và guard.
- Connection read-only không bao giờ nhận lệnh ghi.
- Production phải xác nhận.
- Không tự retry.
- Secret không vào History, log hay Favorites.

## 2. Các quyết định đã chốt

| # | Quyết định |
|---|---|
| M1 | **Crate `mongodb` 3.x chính thức** với `default-features = false`, `features = ["rustls-tls", "compat-3-0-0"]`; feature `rustls-tls` dùng rustls với ring. Kèm crate `bson`. Crate này lo SCRAM, x509, SRV, replica set và OP_MSG. Driver tắt `retry_writes` và `retry_reads`. |
| M2 | **Console là tập con cú pháp mongosh, không chạy JS** (D3). Thêm `show …`, cursor modifier, các helper `db.*`, và `.explain()`. |
| M3 | **Phân loại theo grammar**, không hỏi server. `runCommand` tra key đầu tiên trong một bảng viết tay; lệnh không có trong bảng là `Unknown`. |
| M4 | **Cancel bằng `killOp`** (tìm op qua `comment` gắn vào mỗi lệnh) cộng `killCursors`. Nếu không có quyền `killOp` thì đóng `Client` rồi mở lại ở lần gọi sau. |
| M5 | **Atomicity theo topology:** replica set hoặc mongos thì mỗi lần apply chạy trong một transaction. Standalone thì sửa từng document một; R4a chặn kiểu sửa này trên Production. |
| M6 | **Test với MongoDB 6.0 (standalone) và 8.0** (replica set một node, có TLS). |

## 3. Cấu trúc crate

```
crates/driver-mongo/src/
  lib.rs          MongoDriver: Driver (kind Mongo, command_language(), connect)
  client.rs       ClientOptions từ ResolvedConfig + DriverOptions::Mongo; TLS; hello/buildInfo
  session.rs      MongoSession: Session + CommandSession; db hiện tại, read-only, ping, close
  cancel.rs       MongoCanceller
  shell/
    lexer.rs      token kiểu JS: identifier, chuỗi ('…' "…" `…`), số, /regex/flags, dấu câu, comment
    literal.rs    object literal kiểu JS → Bson (các helper ở §4.3)
    parse.rs      grammar §4.2 → ShellCommand
    classify.rs   ShellCommand → CommandClass, targets, unbounded, drops (§4.4)
    redact.rs     §4.5
    mod.rs        MongoLanguage: CommandLanguage (split, parse, redact)
  exec.rs         ShellCommand → lời gọi driver; batch cursor → RowSink
  rows.rs         document → cột (§5.3); Bson ↔ Value (§7)
  introspect.rs   Introspector (§6.1)
  data.rs         DataAccess (§6.2–6.4)
```

Dependency mới: `mongodb` và `bson`. Sau khi thêm, `Cargo.lock` không được có `aws-lc-rs`, `aws-lc-sys`, `openssl-sys`
hay `native-tls`.

## 4. Console

### 4.1 Tách lệnh (`split`)

- Một lệnh kết thúc bằng `;`, hoặc ở cuối dòng khi các ngoặc `( [ {` đã đóng hết.
- Không tách bên trong chuỗi, regex hay comment.
- `//` và `/* */` là comment.
- Dòng trống được bỏ qua.
- `range` của `CommandSpan` tính theo byte trong text.
- `split` không bao giờ panic và không cần session.

### 4.2 Grammar

```
stmt     := "use" ident
          | "show" ("dbs"|"databases"|"collections"|"tables"|"users"|"roles")
          | dbRef "." collRef "." method "(" args? ")" modifier*
          | dbRef "." dbMethod "(" args? ")" modifier*
dbRef    := "db" | "db.getSiblingDB(" string ")"
collRef  := ident | "[" string "]" | "getCollection(" string ")"
modifier := "." ("sort"|"limit"|"skip"|"project"|"projection"|"hint"|"collation"|"maxTimeMS"
                  |"batchSize"|"comment"|"count"|"toArray"|"pretty"|"explain") "(" args? ")"
args     := literal ("," literal)*
```

**Method của collection:**

- Đọc: `find`, `findOne`, `countDocuments`, `estimatedDocumentCount`, `distinct`, `aggregate`, `getIndexes`,
  `stats`.
- Ghi: `insertOne`, `insertMany`, `updateOne`, `updateMany`, `replaceOne`, `deleteOne`, `deleteMany`,
  `findOneAndUpdate`, `findOneAndReplace`, `findOneAndDelete`, `bulkWrite`.
- Cấu trúc: `createIndex`, `createIndexes`, `dropIndex`, `dropIndexes`, `renameCollection`, `drop`.

**Method của db:**

- `getCollectionNames`, `stats`, `serverStatus`, `version`, `createCollection`, `dropDatabase`, `runCommand`,
  `adminCommand`.

**Modifier:**

- Chỉ hợp lệ khi đi sau `find` hoặc `aggregate`.
- `count()` đổi `find` thành `countDocuments`.
- `toArray()` và `pretty()` không làm gì.
- Modifier đi sau method khác → lỗi `.<m>() is not valid after <method>()`.

**Lỗi:**

- Cú pháp ngoài grammar (`var`, `let`, `const`, `=>`, `function`, `load(`, `eval(`, `.forEach(`, `.map(`, gán `=`, toán
  tử số học) → lỗi parse kèm offset: `Only MongoDB shell commands are supported; JavaScript is not executed`.
- Method không có trong danh sách → `Unknown method <name>()`.

### 4.3 Literal

- Object literal kiểu JS:
  - key không nháy (identifier hoặc `$op`), key trong nháy đơn hoặc kép;
  - dấu phẩy thừa ở cuối;
  - array, chuỗi (`'…'`, `"…"`, `` `…` `` không có `${}`), `true`, `false`, `null`;
  - số nguyên → `Int32`, hoặc `Int64` khi vượt `Int32`; số thực / số mũ → `Double`; số âm.
- `/pattern/flags` → `Regex`.
- Các helper:

| Helper | Kết quả |
|---|---|
| `ObjectId("24hex")` | `ObjectId` |
| `ISODate("…")`, `new Date("…")` | `DateTime` |
| `NumberLong(n)` | `Int64` |
| `NumberInt(n)` | `Int32` |
| `NumberDecimal("…")` | `Decimal128` |
| `UUID("…")` | `Binary` subtype 4 |
| `BinData(t, "b64")` | `Binary` |
| `Timestamp(t, i)` | `Timestamp` |
| `MinKey`, `MaxKey` | `MinKey`, `MaxKey` |

- Helper sai tham số → lỗi kèm offset.
- Cũng parser này đọc `raw_json` của data view (§6.2) và giá trị ô JSON khi sửa (§6.3).

### 4.4 Phân loại

| Lệnh | Class | Ghi chú |
|---|---|---|
| Method đọc, `show …`, `db.getCollectionNames/stats/serverStatus/version` | Read | |
| `aggregate` không có `$out`/`$merge` | Read | |
| `aggregate` có `$out`/`$merge` | Write | target là collection đích, cùng db hoặc `{db, coll}` |
| `insert*`, `update*`, `replaceOne`, `delete*`, `findOneAnd*`, `bulkWrite` | Write | |
| `updateMany` hoặc `deleteMany` với filter `{}` | Write | `unbounded` |
| `bulkWrite` có `updateMany`/`deleteMany` với filter `{}` | Write | `unbounded` |
| `createCollection`, `createIndex(es)`, `dropIndex(es)`, `renameCollection` | Ddl | |
| `drop` | Ddl | drop `Object` (collection) |
| `dropDatabase` | Ddl | drop `Database` (db hiện tại, tên rỗng) |
| `use db` | SessionChange | `UseDatabase(db)` |
| `.explain()` | Class của lệnh gốc | Luôn gửi verbosity `queryPlanner`, nên server không chạy lệnh ghi |
| `runCommand` / `adminCommand` | Theo key đầu tiên (§4.4.1) | |

- `targets`: mọi lệnh trên collection có `[{ database: <db từ getSiblingDB, hoặc None = db hiện tại>, name: coll }]`.
  Vì vậy `confirm_name` trên Production là tên collection.

#### 4.4.1 Bảng `runCommand`

- **Read:** `ping`, `hello`, `isMaster`, `buildInfo`, `serverStatus`, `dbStats`, `collStats`, `listDatabases`,
  `listCollections`, `listIndexes`, `find`, `aggregate` (không có `$out`/`$merge`), `count`, `distinct`, `getMore`,
  `explain`, `connectionStatus`, `usersInfo`, `rolesInfo`, `hostInfo`, `getParameter`, `currentOp`, `validate`,
  `dataSize`.
- **Write:** `insert`, `update`, `delete`, `findAndModify`, `aggregate` có `$out`/`$merge`.
- **Ddl:** `create`, `drop` (drop `Object`), `dropDatabase` (drop `Database`), `createIndexes`, `dropIndexes`,
  `renameCollection`, `collMod`.
- **Admin:** `killOp`, `killCursors`, `setParameter`, `createUser`, `updateUser`, `dropUser`, `grantRolesToUser`,
  `revokeRolesFromUser`, `createRole`, `dropRole`, `shutdown`, `fsync`, `compact`, `replSetStepDown`,
  `replSetReconfig`.
- **Unknown:** mọi key khác.

### 4.5 Redact

Che bằng `***`:

- giá trị của key `pwd` và `password` ở mọi độ sâu trong `createUser`, `updateUser` và `runCommand` tương ứng;
- tham số thứ hai của `db.auth("u", "p")`;
- phần mật khẩu trong mọi chuỗi `mongodb://user:pass@` hoặc `mongodb+srv://user:pass@`.

Dòng không parse được vẫn được redact theo từ khoá và mẫu URI.

## 5. Kết nối, session, chạy lệnh

### 5.1 Connect

Cả bước này chạy trong `connect_timeout`.

**`ClientOptions`** dựng trực tiếp từ `ResolvedConfig` và `DriverOptions::Mongo`, không qua URI. Mật khẩu chỉ nằm trong
`SecretString` rồi vào `Credential`.

| Option | Lấy từ |
|---|---|
| `hosts` | `host:port`, cộng `extra_hosts` |
| SRV | Khi `srv`, phân giải `_mongodb._tcp.<host>` bằng resolver của crate |
| `credential` | username, password, `auth_source` (mặc định `admin`) |
| `repl_set_name` | `replica_set` |
| `direct_connection` | `true` khi đi qua SSH tunnel, hoặc khi `direct_connection` được đặt |
| `app_name` | `DntDatabase` |
| `connect_timeout`, `server_selection_timeout` | `connect_timeout` của config |
| `max_pool_size` | 2 |
| `retry_writes`, `retry_reads` | `false` |

**TLS** theo `command_tls`:

| `command_tls` | Cấu hình |
|---|---|
| `Off` | Không TLS |
| `Unverified` | `allow_invalid_certificates = true` |
| `VerifyCa` | `allow_invalid_hostnames = true` |
| `VerifyFull` | Mặc định |

`ca_path`, `cert_path` và `key_path` vào `TlsOptions`. Crate yêu cầu cert và key ở một file PEM chung, nên khi cả hai đều
được đặt, driver ghép chúng vào một file tạm, đặt quyền chỉ chủ sở hữu được đọc, và xoá file khi `Client` drop.

**Kiểm tra server:** gọi `hello`, rồi `buildInfo`.

- Version < 6.0 → `Unsupported`: `MongoDB 6.0 or later is required`.
- `hello.msg == "isdbgrid"` (mongos) → server notice `Sharded clusters are untested`.

**Lỗi lúc connect:**

| Lỗi | `DbError` |
|---|---|
| `AuthenticationFailed` | `Auth` |
| Server selection timeout | `Connect`, kèm danh sách host đã thử |
| SRV không phân giải được | `Connect`: `SRV lookup failed for <host>` |

**`ServerInfo`:**

- kind và variant `Mongo`, version lấy từ `buildInfo`;
- capabilities: `Capabilities::for_kind(Mongo)`, với `transactions` = có `setName` hoặc là mongos.

**Db ban đầu:** `cfg.database`, hoặc `test` khi trống.

### 5.2 Session

- **Trạng thái:** `MongoSession` giữ `Client`, `current_db` (đổi khi `use`, không reconnect), `read_only`, và
  `server_info`.
- **`tx_state`:** luôn `Idle`, vì console không có transaction.
- **Read-only:** phòng thủ hai lớp. `execute_command` từ chối mọi class khác `Read` và `SessionChange` với
  `Config { field: None, message: "This connection is read-only" }`, trước khi gửi.
- **Lỗi mạng giữa lệnh:**
  - lệnh `Read` → `ConnectionLost`;
  - lệnh khác → `ConnectionLost` với `outcome_unknown`.
  - Không retry. `Client` tự mở connection mới cho lệnh kế tiếp; `reconnects()` tăng khi driver thấy lỗi mạng.
- **`ping`:** `runCommand({ping: 1})` trên `admin`.
- **`close`:** `Client::shutdown` có timeout 2 s.
- **`keyspace()`:** `None`.

### 5.3 Chạy lệnh (`exec.rs`)

**Gắn comment.** Mỗi lệnh gửi kèm `comment: "dnt:<uuid>"` để cancel tìm được. Lệnh mà người dùng đã gắn `comment` riêng
thì ghép thành `"<comment> dnt:<uuid>"`.

**Cursor** (`find`, `aggregate`, `show collections`, `listIndexes`…):

1. Kéo theo từng batch, cỡ batch là `opts.batch_size`.
2. Mỗi batch được trải thành cột và đẩy vào `RowSink`.
3. Dừng khi đủ `opts.max_rows` (đặt `truncated`) hoặc khi `opts.stop` được bật.
4. Cursor chưa hết thì `killCursors`.

**Trải document thành cột** (spec core §7.3):

- `_id` đứng đầu, các field còn lại theo thứ tự xuất hiện đầu tiên.
- Field mới xuất hiện ở batch sau thì thêm cột: `RowSink` nhận `result_start` mới với bộ cột đầy đủ. Các dòng đã gửi
  không đổi; UI hiển thị ô thiếu là `Null`.
- Giá trị lồng nhau → `Json`.

**Kết quả các loại lệnh khác:**

| Loại lệnh | Kết quả |
|---|---|
| Trả một document (`findOne`, `runCommand`, `stats`, `explain`, `serverStatus`, `findOneAnd*`) | Một dòng trải như trên. Không có document → không có dòng nào. |
| Trả giá trị đơn (`countDocuments`, `estimatedDocumentCount`, `version`) | Một cột `value` |
| `distinct` | Một cột `value`, mỗi giá trị một dòng |
| `getCollectionNames`, `show dbs` | Một cột `name` |
| `show dbs` | Thêm cột `sizeOnDisk` |

**`affected` của lệnh ghi:**

| Lệnh | `affected` |
|---|---|
| `insertOne` | 1 |
| `insertMany` | số document đã chèn |
| `update*` / `replaceOne` | `modifiedCount` |
| `delete*` | `deletedCount` |
| `bulkWrite` | tổng các số trên |

Lệnh ghi còn trả thêm một dòng ở cột `result` (`Json`) chứa `acknowledged`, `matchedCount`, `modifiedCount`,
`deletedCount`, `insertedIds` và `upsertedId`.

**Timeout.** `opts.timeout` được gửi thành `maxTimeMS` cho lệnh đọc, trừ khi người dùng đã tự đặt. Lệnh ghi hết thời
gian thì cancel như §5.4.

**Đổi db.** `use x` đặt `current_db = x` sau khi chạy `listCollections` với `nameOnly`. Bước này để báo lỗi ngay khi
user không có quyền trên db đó. Db chưa tồn tại vẫn hợp lệ, giống mongosh.

### 5.4 Cancel

1. Bật `opts.stop`. Cursor đang kéo dở sẽ dừng ở batch kế tiếp và gửi `killCursors`.
2. Cancel chạy trên một connection riêng của cùng `Client`:
   `aggregate([{$currentOp: {}}, {$match: {"command.comment": /dnt:<uuid>$/}}])` trên `admin`, rồi `killOp` từng
   `opid` tìm được.
3. Kết quả:
   - lệnh đọc → `Cancelled { outcome_unknown: false }`;
   - lệnh ghi → `Cancelled { outcome_unknown: true }`.
4. Khi `currentOp` hoặc `killOp` trả `Unauthorized`, hoặc hết `cancel_grace`, driver gọi `Client::shutdown` và đánh dấu
   session cần mở lại. Lệnh kế tiếp dựng lại `Client` và tăng `reconnects()`. Lệnh ghi bị huỷ theo đường này vẫn báo
   `outcome_unknown`.

### 5.5 Lỗi

| Lỗi | `DbError` |
|---|---|
| Lỗi lệnh của server | `Server { code: Some(codeName), sqlstate: None, message, position: None }` |
| `Unauthorized` | `Server`, hint `The user lacks the role for this command` |
| `DuplicateKey` (11000) | `Server`, hint `A unique index rejected a duplicate value: <index>` |
| `NotWritablePrimary` | `Server`, hint `Connected to a secondary; writes need the primary` |
| `DocumentValidationFailure` | `Server`, kèm `errInfo` trong message |
| `MaxTimeMSExpired` | `Timeout` |
| Lỗi mạng hoặc I/O | `ConnectionLost` (§5.2) |
| Payload sai kiểu | `Internal` |

Các hint được thêm vào `static_hint` của core, theo `code`.

## 6. Cây, data view, sửa document

### 6.1 `Introspector`

**`list_databases`:** `listDatabases {nameOnly: true, authorizedDatabases: true}`. Các db `admin`, `local` và `config`
là db hệ thống; chúng chỉ hiện khi bật "Show system objects", qua `filter_system` sẵn có của R4a nhưng dùng danh sách
riêng của Mongo.

**`list_objects`:**

| Kind | Lệnh | Ghi chú |
|---|---|---|
| `Collection` | `listCollections {filter: {type: {$in: ["collection", "timeseries"]}}}` | Bỏ `system.*` trừ khi bật "Show system objects" |
| `View` | `listCollections {filter: {type: "view"}}` | |
| Các kind khác | — | `[]` |

`row_estimate` để `None`.

**`list_schemas`, `list_columns`:** `[]`.

**`table_detail`:**

- Lấy mẫu: `aggregate([{$sample: {size: 100}}])`.
- Mỗi field cấp một là một `ColumnDetail`:
  - thứ tự: `_id` đứng đầu, rồi theo thứ tự xuất hiện đầu tiên;
  - `db_type` là tên kiểu BSON: `objectId`, `string`, `int`, `long`, `double`, `decimal`, `bool`, `date`, `object`,
    `array`, `binData`, `uuid`, `timestamp`, `regex`, `null`;
  - mẫu có nhiều kiểu thì `db_type` là `mixed(a|b)`, các kiểu xếp theo bảng chữ cái;
  - `nullable` luôn `true`, trừ `_id`.
- Index từ `listIndexes`: index `_id_` là primary, `unique` lấy theo option của index.
- Collection rỗng trả `[ _id: objectId ]`.

**`object_ddl`:**

- Collection: `listCollections {filter: {name}}`, trả `options` (gồm `validator`) dạng JSON có thụt dòng.
- View: trả `{ viewOn, pipeline }`.

### 6.2 `DataAccess::page` và `count`

- **`page`:** `find(filter).sort(sort).skip(skip).limit(limit)`. Không có sort thì sort theo `{_id: 1}`.
- **`count`:** `countDocuments(filter)`.
- Cả hai trả `ParsedCommand` class `Read`, nên qua được `check_read`.

**Filter có cấu trúc:** các toán tử map đúng như spec core §7.5.

| Toán tử | Mongo |
|---|---|
| `Eq` / `Ne` | `$eq` / `$ne` |
| `Lt` / `Le` / `Gt` / `Ge` | `$lt` / `$lte` / `$gt` / `$gte` |
| `Like` / `NotLike` | `$regex` (`%` → `.*`, `_` → `.`, phần còn lại escape) / `$not` |
| `Contains` / `StartsWith` / `EndsWith` | `$regex` có neo, nội dung đã escape |
| `In` | `$in` |
| `Between` | `$gte` + `$lte` |
| `IsNull` | `$eq: null` |

Giá trị ép theo `db_type` đã suy:

- `int`, `long`, `double`, `decimal` → số tương ứng;
- `objectId` + đúng 24 ký tự hex → `ObjectId`;
- `bool` → `true`/`false`;
- `date` → ISO 8601;
- `uuid` → UUID;
- `mixed(…)` và `string` → chuỗi.

Ép không được → `Config { field: Some("filter"), message: "Value \"<v>\" is not a valid <type>" }`.

**Filter raw:** `raw_json` được parse bằng parser literal của §4.3, sau khi `check_raw_json` của R4a đã chặn `$where`,
`$function` và `$accumulator`. Kết quả phải là document, nếu không thì `The filter must be a JSON object`.

### 6.3 `DataAccess::build_edits`

| `RowEdit` | Lệnh |
|---|---|
| `Update` | `updateOne({_id}, {$set: {…}, $unset: {…}})`. Ô `Null` vào `$set: null`; `CellInput::Unset` vào `$unset`. |
| `Insert` | `insertOne({...})`. Không có `_id` thì server tự sinh. |
| `Delete` | `deleteOne({_id})` |

- **`_id`:** lấy từ `RowRef.key`. `Value::Other{ObjectId}` được dựng lại thành `ObjectId`; các kiểu khác dựng ngược theo
  §7.
- **Giá trị ô:** `CellInput::Text` ép theo `db_type` như §6.2. Ô `object` / `array` được parse bằng parser literal và
  thay nguyên cả field.
- **`text` của mỗi `ParsedCommand`:** dạng mongosh, ví dụ `db.orders.updateOne({_id: ObjectId("…")}, {$set: {…}})`.

**Khi apply:**

- `capabilities.transactions` bật: mọi lệnh chạy trong một `ClientSession` với `start_transaction` → `commit`. Lỗi ở bất
  kỳ lệnh nào thì `abort`.
- Không có transaction: chạy lần lượt, dừng ở lỗi đầu tiên.
- Một `updateOne` hoặc `deleteOne` có `matchedCount = 0` là lỗi `Document <_id> no longer exists`; nếu đang trong
  transaction thì `abort`.

### 6.4 Fingerprint

Theo spec core §7.5, fingerprint của họ Command chỉ tính index và kiểu `_id`.

## 7. Map giá trị

| BSON → `Value` | `Value` → BSON khi sửa |
|---|---|
| `ObjectId` → `Other{"objectId", hex}` | `ObjectId` |
| `Decimal128` → `Other{"decimal", text}` | `Decimal128` |
| `Timestamp` → `Other{"timestamp", "t,i"}` | `Timestamp` |
| `MinKey` / `MaxKey` → `Other{"minKey"/"maxKey", ""}` | như cũ |
| `Regex` → `Other{"regex", "/p/f"}` | `Regex` |
| `JavaScriptCode` → `Other{"javascript", code}` | chỉ đọc |
| `DateTime` → `DateTime` (UTC) | `DateTime` |
| `Binary` subtype 4 → `Uuid` | `Binary` subtype 4 |
| `Binary` khác → `Bytes` | `Binary` subtype 0 |
| `Int32` / `Int64` → `Int` | giữ kiểu gốc theo `db_type` (`int` → `Int32`, `long` → `Int64`) |
| `Double` → `Float` | `Double` |
| `Boolean` → `Bool` | `Boolean` |
| `Null` / `Undefined` → `Null` | `Null` |
| Document / Array → `Json` (relaxed EJSON) | parse lại bằng parser literal |

## 8. Test

### 8.1 Unit (không cần server)

- **Lexer và literal → BSON:** mọi dạng literal và helper ở §4.3, số âm và số mũ, các lỗi có offset.
- **Grammar:** mọi dạng `stmt` và modifier; `db["a b"]`, `getCollection`, `getSiblingDB`; modifier sai chỗ; JS bị từ
  chối với đúng message.
- **`split`:** lệnh nhiều dòng, `;`, comment, chuỗi hoặc regex có chứa `;` và xuống dòng.
- **Phân loại:** mọi hàng của bảng §4.4 và §4.4.1, gồm `unbounded`, `$out`/`$merge`, `bulkWrite`, và `explain` của lệnh
  ghi.
- **Redact:** mọi mẫu ở §4.5, kể cả dòng không parse được.
- **Trải cột:** `_id` đứng đầu; field mới ở batch sau; giá trị lồng nhau.
- **Map giá trị:** round trip BSON → `Value` → BSON cho mọi hàng của §7.
- **Filter:** mọi toán tử, ép kiểu thành công và thất bại, escape regex.
- **`build_edits`:** `$set`, `$unset`, insert có và không có `_id`, delete; `text` dạng mongosh.

### 8.2 Tích hợp (`DNT_TEST_DB=1`)

**Hạ tầng:**

- `testdata/docker-compose.test.yml` thêm hai service:
  - `mongo60`: `mongo:6.0`, standalone, auth, cổng `27060`.
  - `mongo80`: `mongo:8.0`, replica set một node `rs0`, TLS bằng `testdata/certs`, auth, cổng `27080`. Service này khởi
    tạo replica set bằng `rs.initiate` trong healthcheck.
- User: `root` / `dnt_test`, cùng user `dnt` / `dnt_test` chỉ có role `readWrite` trên `dnt_fixture`, không có quyền
  `killOp`.
- `testkit::Target` thêm `Mongo60` và `Mongo80`.
- Seed db `dnt_fixture` gồm collection `orders`, có đủ các kiểu ở §7 và một field `mixed`, và view `paid_orders`.

**Kết nối:**

- auth đúng, sai mật khẩu → `Auth`;
- TLS `VerifyFull` và `Unverified`;
- `replica_set: rs0` trên `mongo80`;
- qua SSH tunnel với `directConnection`, dùng service `ssh`.

**Console:**

- `find` + `sort`/`limit`/`project`;
- `aggregate` có `$out` ghi được;
- `insert*`/`update*`/`delete*` ra đúng `affected`;
- `use other` rồi `find`;
- `show dbs`, `show collections`;
- `.explain()`;
- `runCommand({ping: 1})`;
- read-only chặn `insertOne` ngay trong driver: `countDocuments` không đổi.

**Cancel:**

- Một `find` chậm (`$where` có `sleep`, chỉ dùng trong test) bị `killOp`; lệnh kế tiếp vẫn chạy được.
- User `dnt` không có quyền `killOp` thì rơi vào đường drop, `reconnects()` tăng.

**Cây và data view:**

- `listCollections` có cả view;
- `table_detail` suy ra `mixed(…)`;
- `page` với filter có cấu trúc, filter raw có `ObjectId(…)`, và `count`.

**Sửa:**

- Sửa ô, `Unset`, thêm, xoá.
- Trên `mongo80`, một lần apply có lệnh thứ hai lỗi thì không lệnh nào được ghi.
- Document bị xoá từ chỗ khác → `Document … no longer exists`.

### 8.3 App, UI, e2e

- `default_drivers()` thêm `MongoDriver`.
- Test `src-tauri`:
  - `available_engines` có Mongo;
  - một ca end-to-end qua `AppState`: mở connection, plan và run `db.orders.find({})`, mở data view, sửa một ô có
    transaction.
- Backend giả của e2e đổi dòng Mongo sang dạng trải theo field cấp một (follow-up của R4b). Các spec `command-query` và
  `command-data` cập nhật theo.

## 9. Rủi ro và cách giữ

| Rủi ro | Cách giữ |
|---|---|
| Crate `mongodb` kéo theo aws-lc-rs hoặc OpenSSL | `default-features = false`, chỉ bật `rustls-tls` (ring). Plan có bước kiểm tra `Cargo.lock`. |
| Parser nhận nhầm JS thành lệnh | Grammar đóng: mọi token ngoài grammar đều là lỗi parse. Có test từ chối cho từng cấu trúc JS hay gặp. |
| `$out` hoặc `$merge` nằm sâu trong pipeline | Duyệt mọi stage của `aggregate`, kể cả trong `$facet`/`$lookup.pipeline`/`$unionWith.pipeline`, để tìm `$out`/`$merge`. |
| `explain` của lệnh ghi lại chạy thật lệnh ghi | Luôn gửi verbosity `queryPlanner`. Class vẫn là của lệnh ghi, nên guard vẫn áp. |
| Retry ngầm của driver | Tắt `retry_writes` và `retry_reads`. Có test bằng cách đếm lệnh phía server qua `currentOp` hoặc profiler. |
| Mật khẩu lọt ra qua chuỗi URI | Dựng `ClientOptions` trực tiếp; không bao giờ tạo URI chứa mật khẩu. File PEM tạm chỉ chứa cert và key, chỉ chủ sở hữu được đọc. |
| Schema suy từ mẫu sai lệch | `mixed(…)` không ép kiểu filter; khi ghi, `db_type` chỉ ảnh hưởng cách ép giá trị gõ vào, không đổi kiểu field đã có. |
