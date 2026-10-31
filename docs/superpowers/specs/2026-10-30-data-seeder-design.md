# Data seeder — sinh dữ liệu giả cho một bảng

- Ngày: 2026-10-30
- Trạng thái: thiết kế đã được người dùng duyệt từng phần trong phiên brainstorming (kiến trúc, generator +
  suy luận, UI + test); bản viết này chờ người dùng duyệt.
- Cơ sở: P5 spec `2026-10-25-p5-transfer-design.md` (jobs §4, import §6: plan dùng một lần, guard, pre-step,
  transaction, `outcome_unknown`, History), P4a (xác nhận Truncate), roadmap P3–P6 §2 (nguyên tắc an toàn).
- Văn xuôi tiếng Việt; tên định danh, câu SQL và mọi chữ trên UI là tiếng Anh.

## 1. Mục tiêu

Từ cây kết nối (menu chuột phải của một bảng → **Generate data…**), người dùng mở một dialog để đổ vào bảng
đó các dòng giả trông như thật, dùng cho phát triển và test (vài trăm đến vài trăm nghìn dòng). Mỗi cột có một
generator được suy luận từ tên và kiểu, sửa được ngay trong dialog, có preview 10 dòng mẫu. Việc ghi chạy như
một job nền trong Jobs panel (tiến độ, Stop, log, Run again), insert theo batch trong một transaction, qua đúng
guard như mọi thao tác ghi khác. Hỗ trợ MySQL/MariaDB và PostgreSQL.

Lời hứa P2/P3 giữ nguyên: không câu ghi nào tới server mà không qua preview, plan và guard backend; connection
read-only không bao giờ nhận câu ghi; Production hỏi một lần cho mỗi job, bằng cách gõ tên; không tự retry;
`outcome_unknown` hiển thị ở UI và History.

## 2. Các quyết định đã chốt

| # | Quyết định |
|---|---|
| D1 | Mục đích: dữ liệu dev/test. Ưu tiên trông thật và đúng ràng buộc; tối đa **1.000.000** dòng mỗi job. |
| D2 | Chỉ seed **một bảng**. Cột FK lấy ngẫu nhiên từ các khóa đang có của bảng cha (không seed bảng cha). |
| D3 | Dữ liệu giả chỉ có locale **English** (danh sách từ nhúng trong Rust, không thêm dependency). |
| D4 | Cột unique: generator tự đảm bảo không trùng **trong lần chạy**; trùng với dữ liệu cũ → server báo lỗi, job rollback toàn bộ. Không có chế độ "skip duplicates". |
| D5 | Không lưu cấu hình theo bảng. Mỗi lần mở dialog suy luận lại; "Run again" trong Jobs mở lại dialog với request cũ; ô **Seed** tái lập đúng dữ liệu. |
| D6 | Engine sinh dữ liệu viết bằng Rust (`dnt_transfer::seed`, thuần, không DB); job dùng lại pipeline import (`Writer`, `BatchBuilder`, pre-step, Jobs). Preview do cùng engine tính. Không sinh ở TypeScript, không sinh bằng SQL phía server. |
| D7 | Toàn bộ job là một transaction (như Import): lỗi hoặc Stop → rollback hết. MySQL `TRUNCATE` và bảng không transactional (MyISAM…) là ngoại lệ, có cảnh báo + xác nhận riêng, như Import. |
| D8 | Xác nhận: chỉ insert → như Import (Production gõ tên bảng, Staging "I understand", còn lại nút Generate). Có **Delete first / Truncate first** → luôn gõ tên bảng (như action Truncate hiện có), cộng ô xác nhận riêng khi pre-step không hoàn tác được. |

Ngoài phạm vi: seed nhiều bảng theo thứ tự FK; locale khác English; lưu/đặt tên preset; chế độ bỏ qua dòng
trùng; sinh giá trị cho kiểu array/geometry/range/kiểu người dùng tự định nghĩa (chỉ NULL / default / constant);
ràng buộc unique nhiều cột (dựa vào độ ngẫu nhiên, trùng thì server báo lỗi); CHECK phức tạp ngoài dạng `IN`.

## 3. Luồng người dùng

1. Cây kết nối → bảng → **Generate data…** (cạnh **Import data…**). Trên connection read-only mục này bị
   disable với hint `Read-only connection`.
2. Dialog mở ở bước **Configure**: backend đọc bảng (`seed_target`) và trả về các cột cùng generator suy luận.
   Người dùng sửa số dòng, batch, cách xử lý dòng có sẵn, seed, và generator / tùy chọn / NULL % / Unique từng
   cột. Preview 10 dòng cập nhật sau mỗi thay đổi (debounce 300 ms, `seed_preview`).
3. **Review…** gọi `seed_prepare`: backend kiểm tra request, đọc lại bảng, dựng template câu lệnh, chấm guard,
   lưu một plan dùng một lần và trả về bước **Review** (tóm tắt, pre-step, INSERT mẫu, cảnh báo, mức xác nhận).
4. **Generate** gọi `seed_start(plan_id, confirm)`: backend kiểm tra lại mọi thứ plan phụ thuộc rồi submit job
   `seed`. Dialog đóng, toast `Generating data… — see Jobs`. Jobs panel hiện tiến độ theo Rows, Stop, log.
5. Job xong: summary `Inserted 1,000 rows into orders in 1 batch`; một mục History (chỉ template, không giá
   trị). "Run again" mở lại dialog với request cũ, đi lại từ bước Configure.

## 4. Engine sinh dữ liệu — `crates/transfer/src/seed/`

Module thuần (không I/O, không DB), test được độc lập. Các file:

- `rng.rs` — PRNG `SplitMix64`; `cell_rng(seed, row, column) -> SplitMix64` trộn ba số bằng splitmix. Mỗi ô có
  RNG riêng nên đổi generator một cột không đổi dữ liệu các cột khác; cùng seed + cùng cấu hình → cùng dữ liệu.
- `words.rs` — danh sách từ tiếng Anh nhúng (`&[&str]`): first names, last names, street names/suffixes, cities,
  states (+ mã), countries, company words/suffixes, job titles, lorem-like words, domains, TLDs.
- `generator.rs` — kiểu `Generator`, `ColumnPlan`, hàm sinh một giá trị.
- `infer.rs` — suy luận generator từ cột (§4.3) và parse CHECK `IN`.
- `engine.rs` — `Seeder`: sinh từng dòng theo thứ tự, áp NULL %, unique, giới hạn kiểu.

### 4.1 Kiểu dữ liệu (serde + specta, tag `kind`, snake_case)

```rust
pub enum Generator {
    Skip,                                         // cột không có trong INSERT: default / auto
    Sequence { start: i64, step: i64 },
    IntRange { min: i64, max: i64 },
    DecimalRange { min: f64, max: f64, scale: u8 },
    Bool { true_percent: u8 },
    DateRange { from: String, to: String },       // 'YYYY-MM-DD'
    DateTimeRange { from: String, to: String },   // 'YYYY-MM-DD HH:MM:SS'
    TimeRange { from: String, to: String },       // 'HH:MM:SS'
    Pick { values: Vec<String> },
    ForeignKey { constraint: String },            // tên FK trong TableDetail.foreign_keys
    Fake { fake: FakeKind },
    Uuid,
    Constant { value: String },
    Pattern { pattern: String },
    JsonObject,
    RandomBytes { len: u32 },
}

pub enum FakeKind { FirstName, LastName, FullName, Email, Username, Phone, StreetAddress, City, State,
    ZipCode, Country, Company, JobTitle, Url, Domain, Ipv4, Slug, Word, Sentence, Paragraph, HexColor,
    PasswordHash }

pub struct ColumnPlan {
    pub name: String,
    pub generator: Generator,
    pub null_percent: u8,   // 0–100; chỉ có hiệu lực với cột nullable
    pub unique: bool,
}
```

`i64` đi qua IPC: `Sequence`/`IntRange` bị kẹp vào ±2^53 khi kiểm tra request (giá trị lớn hơn không thể biểu
diễn chính xác ở UI). Seed là `u32`.

### 4.2 Sinh giá trị và ràng buộc

Generator trả `FullValue` theo kiểu logic của cột: số nguyên → `Int`/`UInt`, decimal → `Decimal` (chuỗi đúng
scale), float → `Float`, bool → `Bool`, ngày giờ → `Text` (định dạng ISO như trên, literal builder lo phần
cast), uuid → `Uuid`, json → `Json`, bytes → `FullValue::bytes`, còn lại → `Text`. SQL chỉ được dựng bởi
`dnt_core::sql::build` (§6), engine không bao giờ tạo SQL.

- **Pattern**: `#` chữ số, `?` chữ hoa A–Z, `*` chữ hoa hoặc số, `\x` ký tự `x` nguyên văn; ký tự khác giữ nguyên.
- **Giới hạn kiểu** (luôn áp, kể cả khi người dùng nhập ngoài khoảng → lỗi theo cột ở preview/prepare):
  - số nguyên theo `db_type`: tinyint, smallint, mediumint (MySQL), int/integer, bigint; `UNSIGNED` → min ≥ 0;
  - `decimal(p,s)` / `numeric(p,s)`: |giá trị| < 10^(p−s), scale tối đa `s`;
  - `max_chars`: text bị cắt cho vừa; hậu tố unique (nếu có) được giữ và phần thân bị cắt;
  - `Pick` trên cột enum: mọi giá trị phải thuộc `enum_values`.
- **NULL %**: với cột nullable, ô là NULL với xác suất `null_percent`/100 (rút từ RNG của ô trước khi sinh). Cột
  NOT NULL: `null_percent` bị bỏ qua và UI ẩn ô này.
- **Unique**: engine giữ `HashSet` các giá trị đã sinh của cột (theo literal chuẩn hoá).
  - `Sequence`, `Uuid`: unique sẵn, không cần tập.
  - `Fake` dạng text (mọi `FakeKind` trừ `Ipv4`, `HexColor`): khi trùng, gắn số thứ tự dòng (1-based):
    Email `john.smith.4821@example.com` (chèn trước `@`), Username `jsmith4821`, còn lại `<value>-4821`.
  - Generator khác (`IntRange`, `DecimalRange`, `Pick`, `Pattern`, ngày giờ…): sinh lại tối đa 50 lần (RNG tiếp
    tục của ô); vẫn trùng → lỗi `Cannot generate {rows} unique values for `{col}`; widen the range.` Ở prepare,
    một khoảng rõ ràng nhỏ hơn số dòng (ví dụ `IntRange` 1–100 cho 1.000 dòng, `Pick` 3 giá trị) bị từ chối
    ngay; `Bool`/`Constant` + unique bị từ chối nếu số dòng > số giá trị khả dĩ.
- **ForeignKey**: engine nhận danh sách bộ khóa đã đọc (§5.3) và chọn `keys[rng % len]`; FK nhiều cột dùng một
  RNG chung cho cả nhóm (theo cột đầu tiên của FK) nên các cột nhận cùng một bộ khóa. Cột FK nullable vẫn áp
  NULL % (NULL cả nhóm). FK + unique: chọn không lặp lại (sinh lại tối đa 50 lần như trên).

### 4.3 Suy luận generator (`infer`)

Tên cột được tách token (snake_case, camelCase, không phân biệt hoa thường). Quy tắc theo thứ tự, quy tắc chỉ
áp dụng khi kiểu logic hợp với nó:

1. `auto_increment`, identity hoặc generated → `Skip` (generated luôn khóa `Skip`).
2. Thuộc một FK → `ForeignKey { constraint }`.
3. `enum_values` không rỗng, hoặc có CHECK một cột dạng `col IN ('a','b')` (MySQL) /
   `((col)::text = ANY ((ARRAY['a'::character varying, …])::text[]))` (PG) → `Pick { values }`. Parse best-effort;
   không parse được thì bỏ qua quy tắc.
4. Kiểu bool, hoặc tên bắt đầu `is_`/`has_` với `tinyint(1)` → `Bool { true_percent: 50 }`; kiểu uuid → `Uuid`.
5. Theo tên:

| Tên (token) | Generator |
|---|---|
| email | Fake Email |
| first_name, firstname, given_name, fname | Fake FirstName |
| last_name, lastname, surname, family_name, lname | Fake LastName |
| name, full_name, fullname, display_name, *_name (người: customer, contact, author) | Fake FullName |
| username, user_name, login, handle | Fake Username |
| phone, mobile, tel, telephone, fax | Fake Phone |
| address, street, address1, address_line1, line1 | Fake StreetAddress |
| city, town | Fake City |
| state, province, region | Fake State |
| zip, zipcode, postal, postal_code, postcode | Fake ZipCode |
| country | Fake Country |
| company, organization, organisation, employer | Fake Company |
| job_title, position (text) | Fake JobTitle |
| url, website, homepage, link | Fake Url |
| domain, host, hostname | Fake Domain |
| ip, ip_address | Fake Ipv4 |
| slug | Fake Slug |
| color, colour | Fake HexColor |
| password, password_hash, pass_hash | Fake PasswordHash |
| title, subject, headline | Fake Sentence |
| description, bio, notes, note, body, content, summary, about, comment | Fake Paragraph (Sentence nếu `max_chars` < 200) |
| price, amount, total, cost, salary, balance (decimal/float) | DecimalRange 1–1000, scale theo cột (mặc định 2) |
| qty, quantity, stock, count (integer) | IntRange 0–100 |
| age | IntRange 18–80 |
| rating, score, stars | IntRange 1–5 |
| lat, latitude | DecimalRange −90–90, scale 6 |
| lng, lon, longitude | DecimalRange −180–180, scale 6 |
| code, sku (text) | Pattern `???-#####` |
| birth, birthday, birthdate, dob, date_of_birth | DateRange 1950-01-01 → 2005-12-31 |
| *_at, *_date, created, updated, deleted, date, time | ngày giờ trong 2 năm gần nhất (theo kiểu cột) |

6. Theo kiểu: Integer → IntRange 1–1000 (kẹp theo kiểu); Decimal/Float → DecimalRange 0–1000; Text → Fake Word
   (`max_chars` ≥ 30: Sentence; `max_chars` ≤ 10: Pattern `?` × `max_chars`); Date/DateTime/Time → 2 năm gần
   nhất; Json → JsonObject; Binary → RandomBytes 16.
7. Kiểu khác (Array, Other…): nullable → `Constant { value: "" }` với `null_percent = 100` (luôn NULL); có default →
   `Skip`; còn lại `Constant { value: "" }` và prepare báo `Choose a generator for `{col}`.`

Cột thuộc primary key hoặc unique index **một cột** (không partial) → `unique = true` mặc định.
Cột NOT NULL không default mà người dùng chọn `Skip` → lỗi `{col} has no default; choose a generator.`
"Ngày gần nhất" tính theo ngày giờ lúc `seed_target`; request lưu chuỗi ngày cụ thể nên Run again tái lập.

## 5. Backend — `src-tauri/src/seed_data/`

Cấu trúc như `import_data`: `mod.rs` (kiểu, normalise, Layout), `target.rs`, `plan.rs` (preview, prepare,
start), `run.rs` (job), `tests.rs`.

### 5.1 IPC (tauri-specta, `src-tauri/src/commands/seed.rs`)

```rust
pub enum ExistingRows { Keep, Delete, Truncate }   // tag snake_case

pub struct SeedRequest {              // cũng là tham số "Run again" (JobParams::Seed)
    pub connection_id: Uuid,
    pub table: ObjectRef,
    pub rows: u32,                    // 1–1.000.000
    pub batch_rows: u32,              // 0 = 1.000; tối đa 100.000; luôn bị giới hạn thêm bởi 1 MiB SQL/batch
    pub existing: ExistingRows,
    pub seed: u32,
    pub columns: Vec<ColumnPlan>,     // đủ mọi cột của bảng, theo thứ tự bảng
}

pub struct SeedColumn {               // một dòng của bảng cột trong dialog
    pub column: TargetColumn,
    pub primary: bool,
    pub unique_index: bool,
    pub foreign_key: Option<SeedForeignKey>,   // { constraint, ref_table, columns, ref_columns }
    pub check_values: Vec<String>,
    pub locked: bool,                 // generated: chỉ Skip
    pub plan: ColumnPlan,             // giá trị suy luận
}

pub struct SeedTarget {
    pub columns: Vec<SeedColumn>,
    pub env: Option<EnvTag>,
    pub read_only: bool,
    pub server: ServerInfo,
    pub transactional: bool,
    pub seed: u32,                    // seed ngẫu nhiên gợi ý
}

pub struct ColumnIssue { pub column: String, pub message: String }

pub struct SeedSample {
    pub columns: Vec<String>,         // các cột có trong INSERT
    pub rows: Vec<Vec<Value>>,        // ≤ 10 dòng, FullValue::to_display
    pub sql: Option<String>,          // các dòng đó như một INSERT (đã qua redact cho hiển thị)
    pub issues: Vec<ColumnIssue>,
    pub fk_keys: Vec<(String, u32)>,  // FK → số bộ khóa đọc được
}

pub struct SeedPreview {              // bước Review; xuất TS là `SeedPreview`
    pub plan_id: Uuid,
    pub table: ObjectRef,
    pub rows: u32,
    pub batches: u32,                 // ước lượng theo batch_rows
    pub transactional: bool,
    pub pre_step: Option<PreStep>,    // dùng lại kiểu của import
    pub insert_head: String,
    pub insert_tail: String,
    pub sample: Option<String>,
    pub warnings: Vec<String>,
    pub confirm: ConfirmLevel,        // dùng lại kiểu của import
    pub irreversible_name: Option<String>,
    pub expires_in_seconds: u32,
}
```

Commands: `seed_target(connection_id, table) -> SeedTarget`, `seed_preview(request) -> SeedSample`,
`seed_prepare(request) -> SeedPreview`, `seed_start(plan_id, confirm: ImportConfirm) -> Uuid`. `seed_preview`
trả lỗi theo cột trong `issues` (không phải `Err`) để UI hiển thị tại dòng; lỗi cả request (bảng không tồn tại,
connection đóng) vẫn là `Err`.

### 5.2 Kiểm tra request (`normalise`)

- `table.kind == Table`, có database/name; MySQL không có schema, PG bắt buộc schema (như import).
- `rows` trong 1–1.000.000; `batch_rows` chuẩn hoá như import.
- `columns` phải khớp đúng tập cột của bảng (tên, thứ tự); thiếu/thừa → `PlanStale` "The table changed…".
- Mỗi `ColumnPlan` kiểm tra theo cột (§4.2): khoảng min ≤ max, ngày hợp lệ, `from ≤ to`, pattern không rỗng, Pick
  không rỗng, giá trị Pick/Constant chuyển được sang kiểu cột (dùng `dnt_transfer::convert` như import), cột
  generated phải `Skip`, FK `constraint` tồn tại, NOT NULL không default không được `Skip`, unique khả thi.
- Ít nhất một cột không `Skip`, trừ khi bảng cho phép dòng toàn default (MySQL `INSERT … () VALUES ()`; PG
  `DEFAULT VALUES` không đi qua builder → PG yêu cầu ít nhất một cột).

### 5.3 Khóa của bảng cha

`SELECT DISTINCT <ref cols> FROM <parent> WHERE <ref cols> IS NOT NULL ORDER BY <ref cols> LIMIT 10000`, dựng chỉ
từ `quote_ident` / `TableRef::relation`, chạy trên session riêng của lệnh (preview/prepare) hoặc của job. Đây là
câu đọc, gửi qua `execute_one` với `classify` như `types_sql` của import (không qua `Writer`). `ORDER BY` giúp
thứ tự tất định nên preview tái lập.

- Preview/prepare dùng cache trong bộ nhớ `SeedKeyCache` theo `(connection, tunnel generation, parent, ref
  cols)`, sống 60 giây, tối đa 32 mục, để preview debounce không mở session liên tục. Job luôn đọc lại trên session
  của nó trước khi ghi (dữ liệu bảng cha có thể đã đổi: preview khi đó có thể khác dòng thật — chấp nhận, ghi
  trong cảnh báo của Review).
- Bảng cha rỗng: cột FK nullable → toàn NULL + cảnh báo `customers has no rows: customer_id is always NULL.`;
  NOT NULL → lỗi theo cột `customers has no rows to reference.` (prepare từ chối).
- FK tự tham chiếu: chỉ lấy khóa đã có trước khi job bắt đầu (không tham chiếu dòng vừa sinh).

### 5.4 Thứ tự câu lệnh của job

Dùng lại `import_data::run::Writer` (mỗi câu qua `verdict()` của guard ngay trước khi gửi, không gửi sau Stop,
re-check connection trước mỗi câu ghi), `Layout`/`BatchBuilder` (chỉ dựng SQL qua `dnt_core::sql::build`),
`pre_step()` của import. Thay đổi nhỏ ở import: `Writer` nhận đơn vị tiến độ (`Bytes` cho import, `Rows` cho
seed); `pump` nhận nguồn `Msg` chung, seed cung cấp luồng sinh dòng (spawn_blocking, cùng `Item::Row` / `End`,
`bytes` = số dòng đã sinh, `total` = `rows`).

- PostgreSQL: `SET standard_conforming_strings = on`, `BEGIN`, [`DELETE FROM t` | `TRUNCATE t`], đọc khóa FK,
  batches, `COMMIT`.
- MySQL, bảng transactional: [`TRUNCATE t` (không hoàn tác, autocommit)], `START TRANSACTION`, [`DELETE FROM t`],
  đọc khóa FK, batches, `COMMIT`.
- MySQL, bảng khác (MyISAM, có trigger không rõ atomicity): [`DELETE FROM t` | `TRUNCATE t` (không hoàn tác)],
  batches autocommit; cảnh báo `Not transactional: batches already written stay if the job stops.`

Trước câu ghi đầu tiên job đọc lại `table_detail` và so fingerprint với plan (`PlanStale` nếu khác), judge lại các
template. Lỗi/Stop: dùng lại logic `failure()` của import (ROLLBACK, `COMMIT` không được trả lời →
`outcome_unknown`). Lỗi unique từ server: thông điệp server + `The job was rolled back; no row was inserted.`

Summary thành công: `Inserted {n} rows into {table} in {b} batches`. Tiến độ phase `Generating`, unit `Rows`.

### 5.5 Plan, xác nhận, read-only

- `seed_prepare`: read-only → `Blocked`; đọc bảng, kiểm tra request, đọc khóa FK (cache), dựng Layout từ các cột
  không `Skip`, pre-step theo `ExistingRows` (`Delete` ≙ `Replace { truncate: false }`, `Truncate` ≙
  `Replace { truncate: true }`), sample = 10 dòng đầu như một INSERT (cùng builder, cùng giới hạn batch),
  judge template (`Confirm` được bao bởi xác nhận của job), lưu plan (tối đa 64, sống `plan_ttl`).
- `confirm`: `existing != Keep` → `TypeName { name: table }` ở mọi môi trường (D8); ngược lại Production
  `TypeName`, Staging `Click`, còn lại `None`. Pre-step không hoàn tác → `irreversible_ack` bắt buộc.
- `seed_start`: như `import_start` — plan dùng một lần; kiểm tra connection/settings/read-only/generation, xác
  nhận, fingerprint bảng trên session mới; rồi submit `JobSpec { kind: Seed, title: "Generate data for
  {table}", params: JobParams::Seed(request), rerunnable: true }`.
- Cảnh báo (Review): không transactional; trigger trên MySQL; TRUNCATE (MySQL reset AUTO_INCREMENT / PG bảng
  con + ON TRUNCATE triggers); pre-step không hoàn tác; FK bảng cha rỗng; khóa FK có thể đổi trước khi job chạy;
  bảng có ràng buộc unique nhiều cột.

### 5.6 History, Jobs, store

- `JobKind::Seed` (`"seed"`), `JobParams::Seed(serde_json::Value)`; `HistoryOrigin::Seed` (`"seed"`).
- Một mục History mỗi job đã gửi câu ghi: `-- Generate {n} rows into {table}: seed {seed}, {existing}` + pre-step
  + `head…tail`, qua `redact`. Không giá trị nào được ghi.
- **MIGRATION_9** trong `crates/store/src/db.rs`: dựng lại `jobs` (thêm `'seed'` vào CHECK `kind`) và
  `query_history` (thêm `'seed'` vào CHECK `origin`) theo cách MIGRATION_7 làm (bảng `_next`, copy, đổi tên, tạo
  lại index), giữ id và dữ liệu. Test migration từ DB ở phiên bản 8.

## 6. UI

### 6.1 Lối vào

- `TreeContextMenu.tsx`: `seedActionItems(node)` cạnh `importActionItems` — **Generate data…**, disabled +
  hint `Read-only connection` khi `cfg.read_only`; mở `requestDialog({ kind: 'seedData', connectionId, table })`.
- `ui/src/store/dialogs` + `DialogHost.tsx`: kind `seedData` (có `request?: SeedRequest` cho Run again).
- `registerRerun('seed', …)` mở dialog với request cũ.

### 6.2 `ui/src/transfer/seed/SeedDialog.tsx`

Modal lớn (~960 px), phong cách modals-v2: tab phẳng, focus = viền 1px accent, token `--dnt-*`, badge môi trường.

**Bước Configure**

```
Generate data — shop.orders                                            [Production]
 Rows [1000]  Batch size [1000]  Existing rows [Keep ▾]  Seed [48213] [🎲]
 Columns
 Column        Type             Generator           Options                   NULL %  Unique
 id            int · PK · auto  Skip (default)      —                          —       —
 customer_id   int · FK         Foreign key         customers.id · 12 keys     —       —
 email         varchar(120)     Fake: Email     ▾   —                          [10]    [x]
 status        enum             Pick            ▾   new, paid, shipped         —       [ ]
 total         decimal(12,2)    Decimal range   ▾   [1] – [1000] · scale 2     —       [ ]
 Preview — first 10 rows                                                      [Refresh]
 (lưới chỉ đọc; NULL nghiêng; cột Skip hiển thị "(default)")
                                                               [Cancel]  [Review…]
```

- Tùy chọn inline theo generator: Sequence (start, step), ranges (min/max hoặc from/to; ngày dùng input date),
  Bool (true %), Pick (danh sách phân tách bằng dấu phẩy), Constant / Pattern (ô text), RandomBytes (len), Fake
  (dropdown kind trong chính select generator: nhóm "Fake data").
- Cột locked (generated): select disabled + tooltip `Generated column: the server computes it.`
- NULL % chỉ hiện với cột nullable; Unique chỉ hiện với generator hỗ trợ.
- `seed_preview` debounce 300 ms sau mỗi thay đổi; `issues` hiển thị ngay dưới dòng cột (màu danger); `Review…`
  disabled khi còn issue. 🎲 đổi seed ngẫu nhiên; Refresh gọi lại preview (không đổi seed).
- Trạng thái tách vào `seedState.ts` (reducer thuần: cập nhật plan cột, đổi generator giữ tùy chọn hợp lệ, dựng
  request) để test bằng vitest.

**Bước Review**: tóm tắt `Insert 1,000 rows into shop.orders in 1 transaction (1 batch).`; pre-step (khối SQL,
nhãn `Cannot be undone` khi không hoàn tác); INSERT mẫu; cảnh báo; xác nhận theo `confirm` (`TypeName` → ô gõ
tên, `Click` → checkbox `I understand`), ô `I understand existing rows are deleted even if the job fails.` khi
có `irreversible_name`/pre-step không hoàn tác; nút **Back** / **Generate**. Hết hạn plan (`PlanExpired`) → quay
lại Configure với thông báo. Generate → `seed_start`, đóng dialog, toast `Generating data… — see Jobs`.

### 6.3 Jobs panel

`jobFormat.ts`: nhãn kind `Generate data`, đơn vị Rows (đã có). Không cần thay đổi khác.

## 7. Test

- **cargo `dnt-transfer`** (`crates/transfer/tests/seed.rs` + unit test trong module):
  - `cell_rng` tất định; cùng seed + cấu hình → cùng dòng; đổi generator cột A không đổi cột B.
  - Từng generator: định dạng (email, phone, ipv4, uuid, hex color, ngày trong khoảng), Pattern, Pick.
  - Giới hạn kiểu: tinyint/smallint/int/bigint, unsigned, decimal(p,s), `max_chars` cắt nhưng giữ hậu tố unique.
  - Unique: hậu tố cho text giả; khoảng quá hẹp → lỗi; FK nhiều cột cùng bộ khóa.
  - Bảng suy luận (tên + kiểu → generator), parse CHECK MySQL và PG `= ANY (ARRAY[...])`.
  - Proptest: mọi giá trị sinh ra nằm trong giới hạn kiểu và `max_chars`.
- **cargo `src-tauri`** (`seed_data/tests.rs`, dùng session giả của `import_data/tests.rs`):
  - Thứ tự câu lệnh PG / MySQL transactional / MySQL non-transactional, có và không pre-step.
  - Read-only → `Blocked` ở prepare/start và trong job (connection đổi sang read-only khi đang chạy).
  - Production thiếu/sai tên → `ConfirmationRequired`; `existing = Delete` trên môi trường thường cũng cần tên.
  - Stop giữa chừng → `ROLLBACK`, không `COMMIT`; bảng đổi giữa prepare và start → `PlanStale`.
  - Khóa FK: đọc mẫu, bảng cha rỗng + NOT NULL → issue; nullable → cảnh báo.
  - 10 dòng của `seed_preview` = 10 dòng đầu của batch đầu tiên của job.
  - Store: MIGRATION_9 giữ dữ liệu, chấp nhận `seed` cho `jobs.kind` và `query_history.origin`.
- **vitest `ui`**: `seedState.test.ts` (reducer); `SeedDialog.test.tsx` (render cột suy luận, đổi generator, NULL
  %, debounce preview, issue theo cột chặn Review, các mức xác nhận, Delete first buộc gõ tên, PlanExpired quay
  lại Configure); `TreeContextMenu` (mục disabled khi read-only); rerun `seed` mở dialog với request.
- **e2e-web** `e2e-web/specs/seed.spec.ts` + fake IPC trong `e2e-web/src/backend.ts` (`seed_target`,
  `seed_preview`, `seed_prepare`, `seed_start`, job giả có tiến độ Rows rồi Done): tree → menu → đổi generator →
  preview cập nhật → Review → Generate → job Done trong Jobs panel; một case Production gõ tên bảng. Chạy với
  `$env:E2E_WEB_PORT='5176'`.

## 8. File chạm tới

Mới: `crates/transfer/src/seed/{mod,rng,words,generator,infer,engine}.rs`, `crates/transfer/tests/seed.rs`,
`src-tauri/src/seed_data/{mod,target,plan,run,tests}.rs`, `src-tauri/src/commands/seed.rs`,
`ui/src/transfer/seed/{SeedDialog.tsx,seedState.ts,seed.css,*.test.ts(x)}`, `e2e-web/specs/seed.spec.ts`.

Sửa (thêm vào, không đổi hành vi cũ): `crates/transfer/src/lib.rs`, `crates/store/src/{db,history,jobs}.rs`
(MIGRATION_9, origin/kind), `src-tauri/src/jobs/model.rs` (`JobKind::Seed`, `JobParams::Seed`),
`src-tauri/src/import_data/run.rs` (đơn vị tiến độ của `Writer`, nguồn `Msg` chung; `pub(crate)` cho phần
dùng lại), `src-tauri/src/session_manager.rs` + `state.rs` (plan store, key cache), `src-tauri/src/commands/mod.rs`
(đăng ký command), `ui/src/bindings.ts` (sinh lại), `ui/src/components/TreeContextMenu.tsx`,
`ui/src/dialogs/DialogHost.tsx`, `ui/src/store/dialogs.ts`, `ui/src/jobs/jobFormat.ts`, `e2e-web/src/backend.ts`.

Lưu ý làm việc song song: `src-tauri/src/commands/mod.rs`, `ui/src/bindings.ts` và `e2e-web/src/backend.ts` đang
có thay đổi chưa commit của phiên khác — chỉ thêm phần của seeder, không đụng phần khác.
