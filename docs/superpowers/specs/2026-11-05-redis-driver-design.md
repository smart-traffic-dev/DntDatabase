# Driver Redis (`crates/driver-redis`)

- Ngày: 2026-11-05
- Trạng thái: thiết kế đã được người dùng duyệt từng phần trong phiên brainstorming. Bản viết này chờ người dùng duyệt.
- Cơ sở:
  - Spec core không-SQL `2026-10-31-non-sql-driver-core-design.md` (gọi tắt là spec core).
  - Hand-off `2026-10-31-r2-followups.md`, `2026-10-31-r3-followups.md`, `2026-11-04-r4b-followups.md`.
  - Cách RedisInsight làm: `redisinsight/api/src/utils/cli-helper.ts`. RedisInsight dùng ioredis và port `sdssplitargs`, không phân loại lệnh, không có guard.
- Văn xuôi tiếng Việt. Tên định danh, lệnh và mọi chữ trên UI là tiếng Anh.

## 1. Mục tiêu

Cài một driver Redis thật cho các hợp đồng R2: `Driver`, `Session`, `CommandSession`, `CommandLanguage`, `Introspector`,
`DataAccess`, `Keyspace`. Đăng ký nó bằng một dòng trong `default_drivers()` (`src-tauri/src/state.rs`). Khi đó mọi
phần UI và backend R4a/R4b (console, cây `db0 · N keys`, key browser, sửa key) chạy được với Redis thật.

Phạm vi bản đầu theo D2 của spec core:

- Redis ≥ 6.0, standalone.
- TLS và ACL user.
- SSH tunnel một host.

Ngoài phạm vi:

- Redis Cluster và Sentinel.
- Pub/sub và `MONITOR`.
- Sửa giá trị JSON hoặc stream (chỉ đọc).
- Gợi ý AI cho Redis.
- Import, export, dump.

Lời hứa an toàn của spec core giữ nguyên:

- Không lệnh nào tới server mà không qua plan và guard.
- Connection read-only không bao giờ nhận lệnh ghi.
- Production phải xác nhận.
- Không tự retry.
- Secret không vào History, log hay Favorites.

## 2. Các quyết định đã chốt

| # | Quyết định |
|---|---|
| R1 | **Client RESP tự viết** trên tokio + tokio-rustls, chỉ một connection: gửi lệnh rồi đọc reply. Không pool, không pub/sub, không cluster. Không thêm crate Redis bên thứ ba. Lý do: cần byte thô, map RESP3, cancel theo `CLIENT ID`, và giữ ràng buộc rustls + ring. `redis-rs` có thể kéo theo aws-lc-rs và che mất reply thô. |
| R2 | **Phân loại lệnh theo `COMMAND INFO` của chính server**, đọc lúc connect, cộng một danh sách ngoại lệ nhỏ viết tay (§4.3). Không có bảng lệnh tĩnh sinh sẵn. Trước khi connect chỉ cần `split` và `redact`, hai việc này không cần phân loại. |
| R3 | **Không kiểm tra arity, không tách key theo key-spec.** Server tự báo lỗi tham số. Targets chỉ có cho `FLUSHDB`/`FLUSHALL`. |
| R4 | **Cancel:** `CLIENT UNBLOCK <id> ERROR` cho lệnh blocking. Mọi trường hợp khác thì đóng connection, rồi reconnect ở lần gọi sau. |
| R5 | **Tạo key và đổi tên không bao giờ phá key đã có:** `SET … NX`, `RENAMENX`, và một `EVAL` kiểm tra rồi tạo. Nếu key đã tồn tại thì báo `Key already exists`. |
| R6 | **Phân trang hash/set theo cursor** (`HSCAN`/`SSCAN`): `ValueRange` thêm `cursor`. **Xoá phần tử list theo index** bằng một `EVAL` nguyên tử có kiểm tra giá trị. |
| R7 | **Test với Redis 6.2** (mức tối thiểu: HELLO 3, ACL) **và Redis 8.x** (bản hiện hành, có sẵn JSON và Search trong core). |

## 3. Cấu trúc crate

```
crates/driver-redis/src/
  lib.rs         RedisDriver: Driver (kind Redis, command_language(), connect)
  resp.rs        RESP2/RESP3: Frame, incremental decoder, encoder
  conn.rs        Conn: một stream TCP/TLS; gửi lệnh / đọc reply; handshake (HELLO, SELECT, CLIENT ID)
  tls.rs         connector rustls + ring theo core::tls::command_tls; verifier như driver-pg
  session.rs     RedisSession: Session + CommandSession + Keyspace; reconnect, ping, close, read-only
  cancel.rs      RedisCanceller
  language/
    lexer.rs     port sdssplitargs; tách dòng
    classify.rs  cờ COMMAND INFO + ngoại lệ → CommandClass, drops, session_effect
    mod.rs       RedisLanguage: CommandLanguage (split, parse, redact)
  reply.rs       Frame → cột/dòng cho RowSink
  introspect.rs  Introspector
  keyspace.rs    Keyspace::scan / key_value
  edits.rs       DataAccess (build_key_edit; các method khác trả Unsupported)
```

Dependency:

- Mới: `tokio-rustls`. Nó đã có trong lockfile qua đường gián tiếp; đưa vào `[workspace.dependencies]` với
  `default-features = false` và feature ring.
- Còn lại dùng các crate workspace sẵn có: `rustls` với ring, `tokio`, `async-trait`, `secrecy`, `base64`, `tracing`,
  `dnt-core`.
- Sau khi thêm, `Cargo.lock` không được có `aws-lc-rs`, `aws-lc-sys`, `openssl-sys` hay `native-tls`.

## 4. Ngôn ngữ console (`RedisLanguage`)

### 4.1 Tách lệnh (`split`)

- Mỗi dòng không rỗng là một lệnh. Dòng mà ký tự đầu tiên không phải khoảng trắng là `#` thì là comment.
- Dòng bắt đầu bằng khoảng trắng được nối vào lệnh ngay trước nó, như `multilineCommandToOneLine` của RedisInsight.
  `range` của `CommandSpan` bao trọn các dòng đã nối.
- `split` không bao giờ panic. Nó không cần biết lệnh là gì, nên `Driver::command_language()` dùng được khi chưa có
  session.

### 4.2 Token (`lexer.rs`)

Port đúng `sdssplitargs()` của Redis:

- Khoảng trắng ngoài nháy phân tách token.
- `"…"` hỗ trợ escape `\xHH`, `\n`, `\r`, `\t`, `\b`, `\a`; các ký tự khác sau `\` giữ nguyên.
- `'…'` lấy nguyên văn, chỉ có escape `\'`.
- Dấu nháy đóng phải theo sau bằng khoảng trắng hoặc cuối dòng, nếu không là lỗi.
- Token là `Vec<u8>`, nên gõ được giá trị nhị phân.

Lỗi (`CommandParseError` kèm offset):

- `Unterminated quotes`.
- `Closing quote must be followed by a space`.

### 4.3 Phân tích (`parse`)

Tên lệnh không phân biệt hoa thường. Với lệnh container (`CONFIG`, `CLIENT`, `SCRIPT`, `ACL`, `MEMORY`, `OBJECT`,
`XINFO`, `FUNCTION`…), lệnh con là token thứ hai và tra theo dạng `CONFIG|GET`, như `COMMAND INFO` báo.

**Ngoại lệ viết tay.** Danh sách này được xét trước cờ server:

| Lệnh | Phân loại |
|---|---|
| `SELECT n` | `SessionChange`, `UseDatabase(n)` |
| `MULTI` / `EXEC` / `DISCARD` | `Transaction(Begin / Commit / Rollback)` |
| `FLUSHDB` | `Admin`, drop `Database` với tên rỗng (db hiện tại) |
| `FLUSHALL` | `Admin`, drop `AllDatabases` |
| `EVAL`, `EVALSHA`, `FCALL` | `Write` |
| `EVAL_RO`, `EVALSHA_RO`, `FCALL_RO` | `Read` |
| `MONITOR`, `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE`, `SYNC`, `PSYNC`, `CLIENT PAUSE`, `RESET`, `HELLO`, `AUTH`, `QUIT` | lỗi parse `<NAME> is not supported in the console` |

`HELLO`, `AUTH`, `RESET` và `QUIT` bị từ chối vì chúng làm session lệch khỏi trạng thái driver theo dõi (giao thức, user,
db, transaction).

**Cờ server** (`COMMAND INFO`, đọc lúc connect, cả lệnh con). Áp theo thứ tự:

1. Có cờ `admin` hoặc `dangerous` → `Admin`.
2. Có cờ `write` → `Write`.
3. Có cờ `readonly` → `Read`.
4. Không có cờ nào trong ba cờ trên (ví dụ `PING`, `ECHO`, `CLIENT SETNAME`) → `Read`.

Lệnh không có trong `COMMAND INFO` và không thuộc ngoại lệ → `Unknown`.

Hệ quả:

- `KEYS` và `SCAN` là `Read`.
- `DEL` và `UNLINK` là `Write`, không phải drop.
- Không lệnh nào có `unbounded`.
- `targets` chỉ có ở các drop: `FLUSHDB` mang `database: None`.

**`RedisLanguage` có hai dạng:**

- **Tĩnh** (từ `Driver::command_language()`): chỉ có ngoại lệ. Khi được gọi `parse`, mọi lệnh ngoài ngoại lệ là
  `Unknown`. Dạng này dùng cho `split` và `redact`; plan không bao giờ dùng nó.
- **Của session:** chứa bảng cờ đọc từ server. `CommandSession::language()` trả dạng này.

### 4.4 Redact

Che bằng `***`, áp không phân biệt hoa thường theo token:

- `AUTH <pass>` và `AUTH <user> <pass>`: mật khẩu.
- `HELLO … AUTH <user> <pass>`: mật khẩu.
- `CONFIG SET requirepass|masterauth|masteruser <v>`: giá trị.
- `ACL SETUSER`: mọi token bắt đầu bằng `>`, `<`, `#` hoặc `!`.
- `MIGRATE … AUTH <pass>` và `AUTH2 <user> <pass>`: mật khẩu.

Dòng không đọc được token (nháy chưa đóng) vẫn redact theo từ, để không bao giờ để lọt mật khẩu ra ngoài.

## 5. Kết nối và session

### 5.1 Connect

Toàn bộ chạy trong `connect_timeout`.

1. **TCP và TLS.** Mở TCP tới `host:port`. SSH tunnel, nếu có, đã được `SessionManager` mở trước. TLS theo
   `command_tls(cfg.ssl)`:

   | Kết quả `command_tls` | Hành vi |
   |---|---|
   | `Off` | Không TLS |
   | `Unverified` (từ `Prefer` và `Require`) | TLS, không kiểm chứng chứng chỉ |
   | `VerifyCa` | Kiểm chứng CA (`ca_path`, nếu trống thì dùng chứng chỉ hệ thống qua `rustls-platform-verifier`) |
   | `VerifyFull` | Kiểm chứng CA và hostname theo `tls_server_name` |

   Client cert lấy từ `cert_path` / `key_path`. Các verifier viết giống `driver-pg/src/tls.rs`.
2. **Handshake.** Gửi `HELLO 3 [AUTH <user> <pass>] SETNAME DntDatabase`. Không có username mà có password thì
   user là `default`.

   | Server trả | Kết quả |
   |---|---|
   | Lỗi unknown command | `DbError::Unsupported`: `Redis 6.0 or later is required` |
   | `WRONGPASS` / `NOAUTH` | `DbError::Auth` |
   | `NOPERM` | `DbError::Auth` kèm hint |

3. **Chọn db.** Gửi `SELECT <db>`, với db lấy từ `options.driver.db`. Nếu server từ chối thì trả `DbError::Config`
   trên field `options.driver.db`: `Database <n> does not exist on this server`. Không lặng lẽ về db 0.
4. **Đọc thông tin server.**
   - `CLIENT ID`: dùng cho cancel.
   - `INFO server`: lấy `redis_version` và `redis_mode`. Nếu `redis_mode` là `cluster` thì trả `Unsupported`:
     `Redis Cluster is not supported yet`.
   - `COMMAND INFO`: dựng bảng cờ cho language của session. Nếu ACL chặn lệnh này thì bảng rỗng, mọi lệnh ngoài
     ngoại lệ là `Unknown`, và log `warn` một lần.
5. **`ServerInfo`.** Kind và variant `Redis`, version từ `redis_version`, capabilities `Capabilities::for_kind(Redis)`.

### 5.2 Session

- **Khoá connection.** Một `Conn` nằm sau `tokio::sync::Mutex`. Mỗi lệnh là một lượt gửi/đọc. Pipeline (ví dụ
  `TYPE` + `PTTL` cho một trang key) gửi theo lô trong một lượt giữ khoá.
- **`tx_state`.**
  - Chuyển `Active` khi `MULTI` thành công.
  - Về `Idle` khi `EXEC` hoặc `DISCARD` xong, hoặc khi `EXEC` trả `EXECABORT`. Trường hợp `EXECABORT` báo
    `DbError::Server` với message `Transaction aborted: <msg>`.
- **`current_database()`.** Cập nhật sau mỗi `SELECT` thành công; reconnect sẽ `SELECT` lại db này.
- **Read-only.** Phòng thủ hai lớp. Trên connection read-only, `execute_command` từ chối mọi lệnh có class khác
  `Read`, `SessionChange` và `Transaction`, với `DbError::Config`: `This connection is read-only`. Việc từ chối áp cả
  khi plan đã qua guard.
- **Lỗi IO giữa chừng.**
  - Lệnh có class `Read` → `ConnectionLost`.
  - Lệnh có class khác → `ConnectionLost` với `outcome_unknown`.
  - Không retry. Lần gọi sau mở lại connection theo §5.1 bước 1–4, `SELECT` lại db hiện tại và tăng `reconnects()`.
  - Nếu đang giữa `MULTI`, transaction coi như mất: `tx_state` về `Idle` và gửi notice
    `The connection was lost; the open transaction was discarded`.
- **Ping và close.** `ping` gửi `PING`. `close` gửi `QUIT` (không chờ quá 1 s) rồi đóng socket.
- **`keyspace()`** trả `Some(self)`.

### 5.3 Cancel

`RedisCanceller` giữ `client_id`, tên lệnh đang chạy và cấu hình kết nối.

- **Lệnh blocking.** Danh sách: `BLPOP`, `BRPOP`, `BLMOVE`, `BRPOPLPUSH`, `BLMPOP`, `BZPOPMIN`, `BZPOPMAX`,
  `BZMPOP`, `XREAD`, `XREADGROUP`, `WAIT`, `WAITAOF`.
  1. Mở một connection phụ (cùng TLS và auth, timeout 5 s).
  2. Gửi `CLIENT UNBLOCK <id> ERROR`.
  3. Lệnh trên connection chính kết thúc với `DbError::Cancelled`. Connection chính vẫn dùng được.
- **Mọi trường hợp khác**, kể cả khi connection phụ thất bại hoặc hết `cancel_grace`:
  - Đóng socket chính. Lệnh đang chạy kết thúc với `Cancelled`.
  - Nếu lệnh có class khác `Read` thì kèm `outcome_unknown`.
  - Lần gọi sau reconnect theo §5.2.

## 6. Kết quả console (`reply.rs`)

Theo spec core §7.3. Mỗi reply ánh xạ sang result như sau.

| Reply | Cột | Dòng |
|---|---|---|
| Scalar: simple/bulk string, integer, double, boolean, big number, verbatim | `value` | 1 |
| Nil | `value` | 1 dòng `Null` |
| Array, set, push | `#`, `value` | 1 dòng cho mỗi phần tử |
| Map, attribute | `field`, `value` | 1 dòng cho mỗi cặp |
| Lỗi | — | `DbError::Server` |

**Map giá trị:**

| RESP | `Value` |
|---|---|
| Bulk string UTF-8 | `Text` |
| Bulk string không phải UTF-8 | `Bytes` |
| Integer | `Int` |
| Double | `Float` |
| Boolean | `Bool` |
| Big number | `Text` |
| Phần tử lồng nhau | `Json`, với bytes không phải UTF-8 viết dạng `\xNN` |

**Trường hợp đặc biệt:**

- **`affected`.** Lệnh class `Write` trả integer thì điền `affected` bằng số đó, vẫn kèm dòng `value`.
- **Text dễ đọc.** `INFO`, `CLIENT LIST`, `CLIENT INFO`, `MEMORY DOCTOR`, `MEMORY MALLOC-STATS`, `LATENCY DOCTOR`,
  `LATENCY GRAPH`, `CLUSTER INFO`, `LOLWUT` (danh sách của RedisInsight) có kết quả tách theo dòng, một cột `value`.
- **`MULTI` / `EXEC`.** Lệnh trong `MULTI` trả `QUEUED` thành một dòng. `EXEC` trả mỗi lệnh đã xếp hàng thành một
  result riêng, theo thứ tự.
- **Giới hạn dòng.** `opts.max_rows` cắt mảng dài và đặt `truncated`.
- **Ô lớn.** `opts.cells` (`Preview` / `Full`) áp như SQL.

## 7. Cây, key browser, sửa key

### 7.1 Introspector

- **`list_databases`.** Đọc `INFO keyspace` và lấy các `dbN` có key, mỗi db kèm `key_count`. Thêm db của connection
  nếu nó chưa có trong danh sách (`key_count` = 0). Sắp theo số db. Không gọi `CONFIG GET databases`, vì ACL hay chặn
  lệnh này.
- **Các method còn lại.** `list_schemas`, `list_objects` và `list_columns` trả `[]`. `table_detail` và `object_ddl`
  trả `Unsupported`.

### 7.2 `Keyspace::scan`

1. Gửi `SCAN <cursor|0> MATCH <pattern|*> COUNT <n>`, kèm `TYPE <t>` khi lọc theo type. `n` được kẹp trong 1–1000.
2. Pipeline `TYPE` (bỏ qua khi đã lọc theo type) và `PTTL` cho các key trong trang.
3. Key có `TYPE` là `none` (đã bị xoá giữa hai lệnh) bị loại khỏi trang.
4. Cursor `0` từ server → `next_cursor = None`.

Ánh xạ `KeyType`:

| `TYPE` | `KeyType` |
|---|---|
| `string`, `hash`, `list`, `set`, `zset`, `stream` | Biến thể cùng tên |
| `ReJSON-RL` | `Json` |
| Khác | `Other(name)` |

### 7.3 `Keyspace::key_value`

**Thay đổi core:** `ValueRange` thêm `#[serde(default)] pub cursor: Option<String>`. UI truyền lại `next` đã nhận cho
hash và set.

Mỗi lần đọc gửi `TYPE`, `PTTL` và `MEMORY USAGE <key>` trong một pipeline:

- `MEMORY USAGE` lỗi (ví dụ bị ACL chặn) → `memory_bytes = None`.
- `TYPE` trả `none` → `DbError::KeyNotFound`. Đây là reply bình thường, connection không bị coi là bẩn.

Đọc theo kiểu:

| Kiểu | Lệnh | Kết quả |
|---|---|---|
| string | `STRLEN`, rồi `GETRANGE 0 <limit-1>` | `limit` là giới hạn ô của `ExecOptions` mặc định. Dài hơn thì trả `Bytes` dạng preview cùng `len` thật, nên UI chỉ cho xoá. |
| hash | `HLEN`, `HSCAN <cursor> COUNT <count>` | `next` = cursor kế, `None` khi hết |
| set | `SCARD`, `SSCAN <cursor> COUNT <count>` | `next` = cursor kế, `None` khi hết |
| list | `LLEN`, `LRANGE <offset> <offset+count-1>` | — |
| zset | `ZCARD`, `ZRANGE <offset> <offset+count-1> WITHSCORES` | — |
| stream | `XLEN`, `XRANGE <start> + COUNT <count>` | `start` là `-` hoặc id cuối trang trước, đi qua `cursor` (exclusive) |
| `ReJSON-RL` | `JSON.GET <key>` | `KeyValue::Other { type_name: "json", text }`, chỉ đọc |
| khác | — | `Other { type_name, text: "" }` |

### 7.4 `DataAccess`

- `page`, `count` và `build_edits` trả `Unsupported { feature: "collections" }`, vì Redis không có collection.
- `build_key_edit(db, edit)` trả một `ParsedCommand` (text là lệnh redis-cli tương đương, payload là argv đã dựng) cho
  mỗi `KeyEdit`:

| `KeyEdit` | Lệnh | Class |
|---|---|---|
| `SetString { key, value }` | `SET key value KEEPTTL` | Write |
| `HashSet` / `HashDelete` | `HSET` / `HDEL` | Write |
| `ListPush { head }` | `LPUSH` / `RPUSH` | Write |
| `ListSet { index }` | `LSET` | Write |
| `ListRemove { key, index, value }` (**đổi shape**, trước là `{ key, value, count }`) | `EVAL` script xoá theo index (§7.5) | Write |
| `SetAdd` / `SetRemove` | `SADD` / `SREM` | Write |
| `ZSetAdd` / `ZSetRemove` | `ZADD` / `ZREM` | Write |
| `Expire { ttl_ms: Some(n) }` | `PEXPIRE key n` | Write |
| `Expire { ttl_ms: None }` | `PERSIST key` | Write |
| `Rename { key, to }` | `RENAMENX key to` | Write |
| `Delete { key }` | `UNLINK key` | Write |
| `Create { key, value }` (**mới**) | `SET key v NX`, hoặc `EVAL` script tạo key (§7.5) | Write |

Mọi lệnh có `targets = [{ database: Some(db), name: key.display }]` và đi qua `guard_commands`, xác nhận theo env
rồi `run_commands`, như hiện tại.

### 7.5 Tạo key, đổi tên, xoá phần tử list

**Kiểu mới trong core** (`crates/core/src/data_access.rs`):

```rust
pub enum NewKeyValue {
    String { value: KeyBytes },
    Hash { field: KeyBytes, value: KeyBytes },
    List { value: KeyBytes },
    Set { member: KeyBytes },
    ZSet { member: KeyBytes, score: f64 },
}
```

`KeyEdit` gửi qua serde với tag `op`, nên variant mới có `op` là `create`.

**Tạo key** (`Create`). Kết quả dù đi theo nhánh nào cũng giống nhau: key đã tồn tại thì báo
`DbError::Server { message: "Key already exists" }` và không thay đổi gì.

- String → `SET key v NX`. Reply nil nghĩa là key đã tồn tại.
- Các kiểu khác → script sau, với `ARGV[1]` là tên lệnh tạo (`HSET`, `RPUSH`, `SADD`, `ZADD`):

  ```lua
  if redis.call('EXISTS', KEYS[1]) == 1 then return redis.error_reply('Key already exists') end
  return redis.call(ARGV[1], KEYS[1], unpack(ARGV, 2))
  ```

**Đổi tên** (`RENAMENX`). Reply 0 → `Key already exists`.

**Xoá phần tử list theo index** (`ListRemove`). Script nguyên tử:

```lua
if redis.call('LINDEX', KEYS[1], ARGV[1]) ~= ARGV[2] then
  return redis.error_reply('The list changed; refresh and try again')
end
redis.call('LSET', KEYS[1], ARGV[1], ARGV[3])
return redis.call('LREM', KEYS[1], 1, ARGV[3])
```

`ARGV[3]` là một sentinel ngẫu nhiên 16 byte, sinh mới cho mỗi lệnh.

**Phía UI:**

- Hộp "New key…" gửi `Create`.
- "Remove item" gửi `ListRemove { index, value }`.
- `keysTabs` truyền `cursor` cho hash và set.
- Thông báo `Key already exists` hiện trong hộp preview như các lỗi apply khác.

## 8. Lỗi

| Reply hoặc điều kiện | `DbError` |
|---|---|
| `-ERR …`, `-WRONGTYPE …`, các lỗi `-<CODE> …` khác | `Server { code: <tiền tố>, message }` |
| `-NOPERM …` | `Server`, hint `The ACL user lacks permission for this command` |
| `-READONLY …` | `Server`, hint `This server is a read-only replica` |
| `-LOADING …` / `-BUSY …` | `Server`, hint `The server is loading data or running a script; try again later` |
| `-MOVED …` / `-ASK …` | `Unsupported`: `Redis Cluster is not supported yet` |
| `-EXECABORT …` | `Server`: `Transaction aborted: <msg>` (§5.2) |
| IO / EOF | `ConnectionLost` (§5.2) |
| Payload sai kiểu khi `execute_command` | `Internal` |

## 9. Test

### 9.1 Unit (không cần server)

- **Codec.** Encode và decode RESP2/RESP3 cho mọi kiểu frame, kể cả decode từng phần khi buffer bị cắt ở mọi vị trí.
- **Lexer.** Bảng ví dụ của `sdssplitargs`: escape, nháy, lỗi nháy, byte nhị phân. Nối dòng thụt đầu.
- **Phân loại.**
  - Bảng cờ giả lập.
  - Mọi ngoại lệ ở §4.3.
  - Lệnh con (`CONFIG|SET` là admin).
  - `Unknown` khi không có trong bảng.
  - Dạng tĩnh của language.
- **Redact.** Mọi mẫu ở §4.4, cả khi dòng có nháy chưa đóng.
- **Reply.** Mọi hàng ở bảng §6, `affected`, `truncated`, `EXEC`.
- **`build_key_edit`.** Mỗi variant ra đúng argv và text, gồm `Create` cho từng kiểu và `ListRemove`.

### 9.2 Tích hợp (`DNT_TEST_DB=1`, thật)

**Hạ tầng:**

- `testdata/docker-compose.test.yml` thêm hai service:
  - `redis62`: cổng `63062`.
  - `redis8`: cổng `63080`, cổng TLS `63443` dùng `testdata/certs`, ACL user `dnt` / `dnt_test` với quyền giới hạn
    (cấm `FLUSHALL`, `CONFIG`, `DEBUG`).
- Cả hai service đều bật `enable-debug-command local`, để test cancel dùng được `DEBUG SLEEP`.
- `testkit::Target` thêm `Redis62` và `Redis8`.

**Ca kiểm:**

- **Kết nối:**
  - Connect thường, TLS `VerifyFull`, TLS `Unverified`.
  - ACL user đúng mật khẩu; sai mật khẩu → `Auth`.
  - `options.driver.db` ngoài phạm vi → `Config` trên field `options.driver.db`.
  - Qua SSH tunnel bằng service `ssh` sẵn có.
- **Session:**
  - `SELECT 2` rồi giết connection phía server (`CLIENT KILL` từ connection phụ). Lệnh tiếp theo báo `ConnectionLost`;
    lệnh sau nữa reconnect và đang ở db 2.
  - `MULTI` / `EXEC` trả từng result. Một lệnh sai cú pháp trong `MULTI` → `EXECABORT`, `tx_state` về `Idle`.
  - Read-only: `SET` bị từ chối ngay trong driver, server không nhận gì (kiểm bằng `DBSIZE`).
- **Cancel:**
  - `BLPOP k 0` bị huỷ qua `CLIENT UNBLOCK`; connection vẫn dùng được.
  - `DEBUG SLEEP 5` bị huỷ bằng cách drop; lần gọi sau reconnect.
- **Phân loại thật:**
  - `GET` → Read, `SET` → Write, `CONFIG SET` → Admin.
  - `JSON.SET` → Write trên Redis 8; trên Redis 6.2 không có module nên là `Unknown`.
  - Với ACL user bị chặn `COMMAND`, mọi lệnh ngoài ngoại lệ là `Unknown`.
- **Key browser:**
  - `scan` theo pattern và theo type, đi hết cursor.
  - `key_value` cho đủ 6 kiểu, kèm `HSCAN` / `SSCAN` theo cursor và `XRANGE` sang trang.
  - `KeyNotFound`.
  - `MEMORY USAGE` bị ACL chặn → `memory_bytes = None`.
- **Sửa key:**
  - `Create` cho từng kiểu; `Create` lên key đã có → `Key already exists`, giá trị cũ còn nguyên.
  - `RENAMENX` lên key đã có → lỗi.
  - `ListRemove` theo index đúng phần tử khi list có giá trị trùng; list bị đổi giữa preview và apply → lỗi, không xoá.
  - `SetString` giữ TTL.
- **Reply:** `INFO` tách dòng; `HGETALL` (map RESP3) ra `field` / `value`.

### 9.3 App và UI

- `default_drivers()` thêm `RedisDriver`.
- Test `src-tauri`: `available_engines` có Redis, và `language_for(Redis)` là `Some`.
- Các test R4a dùng `FakeCommandDriver` giữ nguyên.
- `FakeCommandDriver` theo kịp thay đổi core: `ValueRange.cursor`, `KeyEdit::Create`, và `ListRemove` mới.
- UI vitest:
  - "New key…" gửi `Create`.
  - "Remove item" gửi `ListRemove { index, value }`.
  - Hash / set truyền `cursor`.
  - `Key already exists` hiện trong preview.
- e2e-web: backend giả nhận các shape mới.

## 10. Rủi ro và cách giữ

| Rủi ro | Cách giữ |
|---|---|
| Server báo cờ lệnh sai, hoặc module tự khai cờ thiếu | Ngoại lệ viết tay xét trước. Lệnh không có cờ `readonly` / `write` / `admin` mới là `Read`. Lệnh của module thiếu cờ `readonly` vẫn là `Read`; đây là chấp nhận có ý thức, vì RedisInsight còn không phân loại. Read-only vẫn có lớp phòng thủ trong driver (§5.2). |
| ACL chặn `COMMAND` | Mọi lệnh là `Unknown`: bị chặn trên read-only, phải gõ tên trên Production. An toàn hơn, không lỏng hơn. |
| TLS kéo theo aws-lc-rs | Chỉ thêm `tokio-rustls` với `default-features = false` và ring. Plan có bước kiểm `Cargo.lock`. |
| Reconnect sai db | `current_database` là nguồn duy nhất; reconnect luôn `SELECT` lại db đó. Có test tích hợp. |
| Script `EVAL` bị ACL chặn | Lỗi `NOPERM` hiện như mọi lỗi apply. Thao tác bị từ chối, không có thay đổi một nửa. |
| Giá trị rất lớn | `GETRANGE` có giới hạn, `HSCAN` / `SSCAN` có `COUNT`, `LRANGE` / `ZRANGE` / `XRANGE` theo trang. Không đọc cả key trong một lệnh. |
