# DntDatabase — Lộ trình tính năng core P3–P6 — Design Spec (đề xuất)

- **Ngày:** 2026-10-20
- **Trạng thái:** Đề xuất, bản 2 (đã sửa theo review Codex, xem §12), chờ review
- **Dựa trên:** [P1 spec](2026-10-19-p1-core-design.md), [P2 spec](2026-10-19-p2-query-design.md),
  [UX/UI review 2026-10-20](../reviews/2026-10-20-ux-ui-review.md), mã nguồn `main` tại `5b2438a`
- **Phạm vi tài liệu:** đây là spec **khung** cho P3–P6. Nó chốt các khối nền dùng chung (§4) và phạm vi
  từng giai đoạn (§5–§8). Mỗi giai đoạn sẽ có spec chi tiết và plan riêng trước khi code, như P2. AI nằm
  ngoài tài liệu này (§10).

## 1. Hiện trạng và vấn đề

Trên `main`, app đã làm tốt phần **đọc và chạy SQL**:

| Mảng | Đã có |
|---|---|
| Kết nối | SSH, TLS, nhóm, env tag, read-only (2 lớp: `SET SESSION … READ ONLY` + guard), import `.ncx`/`.dntconn` |
| Duyệt object | Database/schema/bảng/view/MV/function/procedure/trigger/event/sequence/type; Info, DDL, Structure |
| Query | Monaco, nhiều câu, guard Production/read-only, Stop, Explain, History, format, snippet, autocomplete |
| Data tab | Phân trang và sort phía server (`table_data`, `table_row_count`), session riêng (`data_sessions`) |
| Lưới | Copy, find, go to row, stats, record view, lưu CSV |

Nhưng app **chưa quản lý được DB**: mọi thao tác ghi chỉ đi qua việc người dùng tự viết SQL. Theo review
UX, các P0 chặn việc thay Navicat là: không sửa được dữ liệu, Data tab không lọc phía server, không tạo/sửa
object, không cảnh báo `UPDATE`/`DELETE` thiếu `WHERE`.

Nguyên nhân sâu hơn là core còn thiếu các **khối nền** mà nhiều tính năng dùng chung. Nếu từng tính năng
tự làm phần này, ta sẽ có 4 cách sinh SQL, 3 cách chạy tác vụ dài và nhiều chỗ thoát chuỗi khác nhau.

## 2. Mục tiêu, nguyên tắc, ngoài phạm vi

**Mục tiêu:** đến hết P6, team làm được trọn vòng công việc Navicat hằng ngày trong DntDatabase: mở bảng,
lọc, sửa, thiết kế bảng, import/export, backup, xem tiến trình server, so sánh schema. Và vẫn giữ lời hứa của
P2: **không ai lỡ tay ghi lên Production hay vào connection read-only.**

**Nguyên tắc (áp cho mọi giai đoạn):**

1. **Mọi câu ghi đều có xem trước.** SQL người dùng thấy ở preview chính là SQL được gửi đi, từng byte.
2. **Mọi câu ghi đều đi qua guard ở backend.** Kể cả SQL do app sinh ra (sửa ô, designer, import). Frontend
   không vượt được guard.
3. **Một chỗ sinh SQL cho mỗi việc.** Quote identifier, render literal, sinh DML/DDL đều nằm trong `core`/crate
   thuần, test được không cần DB.
4. **Tác vụ dài là job.** Có tiến độ, Stop, log, kết quả; không chặn UI, không chạy trên session của tab.
5. **Bám phiên bản server.** Câu sinh ra phải chạy được trên toàn ma trận test (MySQL 5.7/8/8.4, MariaDB
   10.6/11.4, PostgreSQL 12/17), hoặc bị tắt kèm lý do khi server không hỗ trợ.
6. **Mọi thay đổi được ghi lại.** SQL ghi do app sinh và đã chạy được lưu vào History với `origin` tương ứng.
7. **Không hứa điều server không bảo đảm.** Chỉ nói "đã rollback" khi chắc chắn; bảng không transactional và
   kết quả không xác định (`outcome_unknown`) được báo đúng như vậy, không tự retry.
8. **Không ghi từ dữ liệu thiếu.** Giá trị đã bị cắt để hiển thị không bao giờ được dùng để sinh câu ghi hay
   file export.

**Ngoài phạm vi:** driver mới (SQLite, SQL Server, Oracle), đồng bộ cloud/chia sẻ workspace, tính năng AI.

## 3. Lộ trình tổng quan

| Giai đoạn | Tên | Khối nền | Tính năng chính | Cỡ |
|---|---|---|---|---|
| **P3** | Dữ liệu an toàn | N0 Giá trị đầy đủ, N1 Builder, N2 Phiên ghi + plan bất biến, N3 Guard nội dung | Guard no-WHERE, lọc Data tab, sửa/thêm/xóa dòng, Manual commit, xem/sửa giá trị lớn, Copy as | L |
| **P4** | Object | N4 Mô hình schema + diff | Thao tác tree, table designer, sửa/apply DDL view/routine/trigger | L |
| **P5** | Import/Export | N5 Job, N6 Split dạng stream, N7 Công cụ ngoài | Export, import, dump/restore | L |
| **P6** | Quản trị và nâng cao | (dùng lại N4, N5) | Process list, biến/dung lượng, user/quyền, so sánh schema, ER diagram, workspace | M–L |

Thứ tự phụ thuộc: N0 → N1 → N2 → N3 trước mọi tính năng ghi; N4 cần N1; N5 độc lập nhưng P5 cần N0 (export
nguyên vẹn), N1 (import), N6 và N7 (restore). P6 so sánh schema cần N4. Giai đoạn sau có thể bắt đầu spec khi giai đoạn trước đang làm,
nhưng không merge trước khi khối nền nó dùng đã merge.

```
crates/
├─ core/        # + FullValue, CellMode (N0), sql::build (N1), sql::risk (N3), sql::split dạng stream (N6)
├─ query/       # + guard đọc RiskFlag (N3); plan nhận câu do app sinh (origin)
├─ schema/      # MỚI (P4): TableDef, diff, render DDL theo engine (N4)
├─ transfer/    # MỚI (P5): định dạng CSV/JSON/XLSX/SQL, reader/writer stream
├─ driver-*/    # + affected rows chính xác, origin_table/origin_column (P3), process/kill (P6)
├─ store/       # + migration 4+: edit log, jobs, workspace tabs
src-tauri/      # + edit plans (N2), JobManager (N5), ExternalTool (N7), commands data_*/ddl_*/job_*/admin_*
ui/             # + EditableGrid, FilterBar, TableDesigner, Jobs panel, wizards
```

## 4. Khối nền

### 4.0 N0 — Tách giá trị hiển thị khỏi giá trị đầy đủ

**Vấn đề:** `Value` là giá trị **để hiển thị**. `Value::Bytes` chỉ giữ `len` + 64 byte preview
(`core/src/value.rs`), và driver gọi `Value::bytes(full)` ngay khi decode, nên phần còn lại bị bỏ trước khi
tới `RowSink`. Data view còn cắt text/JSON ở 64 KB (`truncated_cells`). Từ những giá trị này không thể sửa,
export hay dump nguyên vẹn.

**Quyết định đề xuất:**

- Thêm kiểu **`FullValue`** trong `core::value`: cùng các biến thể như `Value`, nhưng `Bytes(Arc<[u8]>)` và
  text/JSON không bị cắt. `FullValue` **không** export sang TypeScript và không đi qua IPC theo dạng lưới.
- `ExecOptions` thêm `cells: CellMode`:
  - `Preview` (mặc định, như hiện tại): driver sinh `Value`.
  - `Full { max_cell_bytes }`: driver sinh `FullValue` qua một sink riêng (`FullRowSink`). Ô vượt
    `max_cell_bytes` → lỗi rõ ràng, không cắt lặng lẽ.
- Mọi API ghi (N1) và mọi writer export/dump (P5) **chỉ nhận `FullValue`**. Không có chuyển đổi
  `Value → FullValue` cho `Bytes` hay ô đã bị cắt, nên trình biên dịch tự chặn việc render từ preview.
- Giá trị người dùng nhập ở UI (sửa ô, value editor) được gửi lên dạng có kiểu riêng (`CellInput`: text,
  số dạng chuỗi đã kiểm tra, NULL, DEFAULT, base64 cho binary, hoặc tham chiếu file) và backend parse thành
  `FullValue` theo kiểu cột.
- Ô đang hiển thị dạng bị cắt (`Bytes` preview hoặc text > 64 KB) chỉ được sửa sau khi tải đầy đủ
  (`data_cell_fetch`, §5.3).

### 4.1 N1 — `core::sql::build`: sinh DML/SELECT và render literal

**Vấn đề:** `Session::execute_one` chỉ nhận text. `core::sql::ident` đã có `quote_ident`, `qualified_name`,
`select_page_sql`, `count_sql`, nhưng chưa có cách đưa **giá trị** vào câu lệnh.

**Quyết định đề xuất: render literal, không dùng bind param ở v1.**

| | Render literal (đề xuất) | Bind param |
|---|---|---|
| Preview = câu thật | Có, đúng nguyên tắc 1 | Không (preview phải tự ghép) |
| Thay đổi trait | Không | Thêm `execute_params`, map kiểu PG theo OID cho cả hai driver |
| Rủi ro | Lỗi thoát chuỗi = injection | Thấp hơn |
| Giảm rủi ro | Một hàm duy nhất, property test + round-trip trên server thật | — |

Bind param để mở sau (import tốc độ cao có thể dùng `COPY` của PG thay vì bind, xem §7.3).

**API (phác thảo):**

```rust
/// Literal an toàn cho một FullValue (N0), theo dialect (kể cả sql_mode MySQL: NO_BACKSLASH_ESCAPES, ANSI_QUOTES).
pub fn literal(v: &FullValue, col: &ColumnTarget, d: &Dialect) -> Result<String, BuildError>;

pub struct ColumnTarget { pub name: String, pub db_type: String, pub logical: LogicalType }

pub enum RowKey {                           // cách xác định một dòng
    Columns(Vec<(ColumnTarget, FullValue)>),// PK, hoặc unique index NOT NULL
}

pub enum RowGuard {                         // điều kiện chống xung đột (§5.2)
    PgXmin(String),                         // PG: phiên bản dòng
    Snapshot(Vec<(ColumnTarget, FullValue)>),// các cột so sánh được, giá trị lúc tải
}

pub fn insert_sql(t: &TableRef, cols: &[ColumnTarget], rows: &[Vec<FullValue>], d: &Dialect) -> Result<String, BuildError>;
pub fn update_sql(t: &TableRef, key: &RowKey, set: &[(ColumnTarget, FullValue)], guard: &RowGuard, d: &Dialect) -> Result<String, BuildError>;
pub fn delete_sql(t: &TableRef, key: &RowKey, guard: &RowGuard, d: &Dialect) -> Result<String, BuildError>;
pub fn select_sql(t: &TableRef, filter: Option<&Filter>, order: &OrderBy, page: Page, d: &Dialect) -> Result<String, BuildError>;
```

**Quy tắc literal:**

- Chuỗi: MySQL thoát `\` và `'` (trừ khi `NO_BACKSLASH_ESCAPES`, khi đó chỉ nhân đôi `'`); PG luôn dùng
  chuỗi chuẩn `'…'` nhân đôi `'`, không bao giờ `E'…'`. Ký tự NUL trong text bị từ chối ở PG.
- Số: từ `FullValue` số, không bao giờ từ text người dùng chưa parse. Decimal giữ nguyên chuỗi đã kiểm tra.
- Binary: MySQL `X'…'`, PG `'\x…'::bytea`.
- PG thêm cast tường minh `'…'::<db_type>` cho kiểu không phải text/số (json, uuid, timestamp, enum, array,
  inet…) để server không phải đoán kiểu.
- Kiểu chưa hỗ trợ (geometry, range, composite) → `BuildError::Unsupported`, UI giữ ô đó read-only.

**Test:** unit cho từng nhánh; property test (chuỗi ngẫu nhiên gồm `'`, `\`, `\0`, unicode, `$$`); integration
round-trip: `INSERT literal` rồi `SELECT` phải trả đúng giá trị trên cả 6 server.

### 4.2 N2 — Phiên ghi và transaction

**Vấn đề:** session của Data tab là pool chỉ đọc, bị đóng khi cancel. Query tab chỉ có auto-commit.

**Đề xuất năm phần:**

**a) Capability `atomic` của một gói thay đổi.** Backend tính trước khi preview, không tin UI:

| Trường hợp | `atomic` |
|---|---|
| PG, chỉ DML | Có (lưu ý: `nextval` của sequence không rollback; preview ghi chú khi có cột identity/serial) |
| MySQL, mọi bảng đích có engine transactional (InnoDB, MyRocks, NDB; theo `TableOptions.engine`) và không bảng nào có trigger | Có |
| MySQL, bảng đích có trigger | **Không xác định được**: trigger có thể ghi vào bảng không transactional mà app không phân tích được |
| MySQL, có bảng MyISAM/MEMORY/ARCHIVE/CSV/Aria (không transactional) | Không |
| Có câu DDL trên MySQL (commit ngầm) | Không |
| Engine không đọc được | Không xác định được |

Chính sách:

- `atomic = Có`: Apply nhiều câu trong một transaction như (b).
- `atomic = Không` hoặc `Không xác định được`: preview ghi rõ **"không thể hoàn tác: các câu đã chạy trước
  lỗi vẫn giữ nguyên"**. Data tab chỉ cho Apply **từng câu một** (mỗi câu là một Apply riêng, báo kết quả
  từng câu). Trên `Production`: chặn sửa dữ liệu qua Data tab khi `atomic ≠ Có`; người dùng tự viết SQL trong
  query tab (đi qua guard như thường).
- Designer và thao tác DDL trên MySQL luôn là `atomic = Không`: chạy tuần tự, dừng ở câu lỗi đầu tiên, báo
  câu nào đã chạy, tải lại cấu trúc thật.

**b) Apply một gói khi `atomic = Có`:** chạy trên một session lấy riêng từ pool cho lần Apply:

```
BEGIN
  câu 1 … câu n            (mỗi câu kiểm tra affected = số dòng mong đợi)
COMMIT                     (lỗi hoặc affected sai trước COMMIT → ROLLBACK cả gói, báo dòng nào xung đột)
```

Session dùng xong được trả pool chỉ khi đã `COMMIT`/`ROLLBACK` sạch, nếu không thì đóng.

**c) Plan bất biến: preview và Apply là cùng một thứ.** Theo đúng mô hình `query_plan` → `query_run(plan_id)`
đã có:

1. `*_preview(…)` dựng plan ở backend và lưu lại: danh sách câu (chuỗi cố định), verdict guard, `atomic`,
   cùng **ngữ cảnh** lúc dựng: connection id và generation của runtime/tunnel, database/schema, env tag và
   `read_only`, `Dialect` (gồm cờ `sql_mode`), dấu vân tay cấu trúc bảng (hash cột, kiểu, khóa, engine,
   trigger). Trả `plan_id` + câu SQL + verdict cho UI.
2. `*_apply(plan_id, confirmed)` **tiêu thụ plan đúng một lần** (dùng xong hoặc lỗi đều xóa). Trước khi gửi câu
   đầu tiên, kiểm tra lại: connection/generation, env/read-only, guard, và `Dialect` thực tế trên session
   dùng để chạy (đọc lại `sql_mode` như driver MySQL đã làm). Lệch bất kỳ điều gì → `PlanStale`, không chạy câu
   nào, UI yêu cầu preview lại.
3. Plan hết hạn sau thời gian giống plan query. Sửa thêm vào edit set ở UI làm plan cũ vô hiệu.
4. Câu gửi đi là chuỗi đã lưu trong plan, không dựng lại. Vì vậy preview đúng từng byte với câu chạy thật.

**d) Kết quả không xác định.** Mất kết nối hoặc timeout **không** đồng nghĩa với rollback:

| Lúc sự cố | Báo cho người dùng |
|---|---|
| Trong transaction, trước khi gửi `COMMIT` | "Chưa commit. Server sẽ rollback transaction khi phát hiện mất kết nối." |
| Khi đã gửi `COMMIT` mà chưa nhận phản hồi | **"Không rõ kết quả"** (`outcome_unknown`): có thể đã commit |
| Câu đang chạy trên bảng không transactional / ngoài transaction | **"Không rõ kết quả"** với câu đó; các câu trước đã áp dụng |

- Dùng lại `DbError::{Timeout, ConnectionLost, Cancelled}.outcome_unknown` của core; N2 và job giữ nguyên trạng
  thái này đến UI và History, **không tự retry** câu ghi, giữ edit set (không xóa stage) và đề nghị "Tải lại
  dữ liệu để đối soát".

**e) Manual commit cho query tab (đã hẹn P3 trong spec P2):**

- Công tắc theo tab: Auto / Manual. Manual: app gửi `BEGIN` trước câu ghi đầu tiên (PG) hoặc
  `SET autocommit = 0` (MySQL), tab giữ session đến khi Commit/Rollback.
- Status bar hiện "Transaction mở · n câu · mm:ss", nút Commit/Rollback (đã có phần hiển thị `TxState`).
- Cảnh báo khi transaction mở quá **N phút** (mặc định 5, có setting) và khi đóng tab/app (đã có guard đóng).
- Manual + câu `Ddl` trên MySQL: hộp cảnh báo "câu này sẽ commit ngầm transaction đang mở".
- Mất kết nối khi transaction mở: báo theo bảng ở (d). Chỉ nói "chưa commit" khi chắc chắn `COMMIT` chưa được
  gửi; không tự reconnect lặng lẽ vào một transaction mới.

### 4.3 N3 — Guard nội dung (`core::sql::risk`)

**Vấn đề:** guard hiện chỉ dựa trên `StatementKind` × env × read-only. Nó không biết một `DELETE` có `WHERE`
hay không.

**Đề xuất:** phân tích token ở độ sâu 0 (dùng lexer chung, giống `classify`) và trả về danh sách cờ rủi ro:

```rust
pub enum RiskFlag {
    NoWhere,              // UPDATE/DELETE không WHERE (cả MySQL multi-table, PG DELETE … USING)
    TrivialWhere,         // WHERE 1=1, WHERE true, WHERE 1
    Truncate,
    DropDatabase, DropObject,
    AlterTable,           // có thể khóa bảng lâu
    UnboundedLimitless,   // UPDATE/DELETE … LIMIT bỏ trống trên bảng lớn (chỉ cảnh báo)
}
```

Guard nâng verdict theo bảng sau (`EnvTag` hiện có: `Local`, `Staging`, `Production`, hoặc không gắn):

| Cờ | Local / không tag | Staging | Production |
|---|---|---|---|
| `NoWhere`, `TrivialWhere` | Confirm | Confirm | Confirm, phải gõ tên bảng |
| `Truncate`, `DropObject` | Allow | Confirm | Confirm, phải gõ tên object |
| `DropDatabase` | Confirm | Confirm | Confirm, phải gõ tên database |
| `AlterTable` | Allow | Allow | Confirm (đã có), thêm ước lượng số dòng |

- **Ước lượng số dòng ảnh hưởng:** khi có cờ trên Production/Staging, `query_plan` chạy best-effort
  `EXPLAIN` (không ANALYZE) cho `UPDATE`/`DELETE` với timeout 2 s và hiện "≈ N dòng" trong hộp xác nhận.
  Lỗi hoặc hết giờ → "không ước lượng được", không chặn.
- Có setting tắt cảnh báo `NoWhere` cho Local / không tag (mặc định bật).
- Câu do app sinh (sửa ô, designer) luôn có `WHERE` theo khóa nên không bật `NoWhere`; designer bật
  `AlterTable`/`DropObject` như câu người dùng viết.

## 5. P3 — Dữ liệu an toàn

### 5.1 Lọc phía server cho Data tab

- **FilterBar** kiểu Navicat: nhiều điều kiện `cột · toán tử · giá trị`, nối AND/OR một cấp. Toán tử:
  `=`, `≠`, `<`, `≤`, `>`, `≥`, `contains`, `starts with`, `ends with`, `is null`, `is not null`, `in (…)`,
  `between`.
- **Chế độ WHERE thô:** ô nhập câu `WHERE` tự do. Nó được kiểm tra bằng lexer: không có `;` ở độ sâu 0,
  không có từ khóa ghi, ngoặc cân bằng. Chạy trên session read-only của data view (với PG: trong
  transaction `READ ONLY`; với MySQL: `START TRANSACTION READ ONLY`), nên kể cả hàm có tác dụng phụ cũng bị
  server chặn.
- Filter cấu trúc sinh SQL qua N1 (`select_sql`), giá trị đi qua `literal`. `contains` dùng `LIKE` có thoát
  `%`/`_`; PG dùng `ILIKE` khi chọn "không phân biệt hoa thường".
- Đếm dòng (`table_row_count`) tôn trọng filter. Sort phía server giữ nguyên như hiện tại.
- Filter và sort lưu theo object tab và được khôi phục cùng workspace (§8.6).

### 5.2 Sửa, thêm, xóa dòng

**Điều kiện sửa được:** object là bảng (không phải view/MV) và có khóa xác định dòng: PK, hoặc unique index
toàn cột NOT NULL. Không có khóa → lưới read-only, có dòng giải thích và gợi ý thêm PK. Connection read-only
→ read-only.

**Luồng:**

1. Người dùng sửa ô, thêm dòng (`Ctrl+N`/nút +), xóa dòng (`Ctrl+Delete`). Thay đổi được **stage** ở UI:
   ô sửa tô màu, dòng mới/xóa có đánh dấu. Có Undo từng thay đổi.
2. **Preview** (luôn chạy trước Apply; trên Production hiện bắt buộc): `data_preview(edit_set)` → backend
   dựng câu từ `EditSet` qua N1 (không tin SQL từ UI), tính `atomic` và guard, lưu plan (N2c) → trả `plan_id`,
   câu SQL, verdict, mức bảo vệ xung đột.
3. **Apply** → `data_apply(plan_id, confirmed)` → chạy đúng các câu đã lưu theo N2b, hoặc từng câu khi
   `atomic ≠ Có` (N2a). `PlanStale` → UI tự preview lại và hỏi lại.
4. **Discard** bỏ toàn bộ stage.

**Phát hiện xung đột (optimistic) — áp dụng như nhau cho UPDATE và DELETE.** Lúc tải trang, mỗi dòng sửa được
mang theo một `RowGuard` (N1):

| Engine | `RowGuard` | Mức bảo vệ |
|---|---|---|
| PG | `xmin` (đọc kèm trang dưới dạng `xmin::text`, ẩn khỏi lưới). Mọi `UPDATE` vào dòng đều đổi `xmin` | **Đầy đủ**: mọi thay đổi trên dòng từ lúc tải đều bị phát hiện |
| MySQL | `Snapshot`: giá trị lúc tải của **mọi cột so sánh được** trong dòng (không chỉ cột đã sửa), so sánh null-safe `<=>` | **Theo cột**: thay đổi chỉ nằm ở cột không so sánh được sẽ không bị phát hiện |

- Cột **không so sánh được** (bị loại khỏi `Snapshot`): float/double (làm tròn), ô đang bị cắt (text > 64 KB,
  `Bytes` mới có preview) trừ khi đã tải đầy đủ, JSON của MySQL (so sánh theo ngữ nghĩa), spatial, kiểu chưa
  hỗ trợ.
- Preview và UI ghi rõ mức bảo vệ của từng dòng. Với MySQL thì liệt kê các cột không được kiểm tra, ví dụ
  "Không kiểm tra xung đột trên: `price` (double), `payload` (đã cắt)".
- Không còn cột nào so sánh được ngoài khóa (MySQL): DELETE/UPDATE vẫn chạy được nhưng preview cảnh báo
  "chỉ kiểm tra theo khóa". Trên `Production` phải xác nhận riêng.
- Affected ≠ 1 → rollback cả gói (nếu `atomic = Có`) hoặc dừng ở câu đó (nếu không), đánh dấu dòng xung đột,
  cho "Tải lại dòng" hoặc "Ghi đè". Ghi đè là một preview mới bỏ `RowGuard`, phải xác nhận.

**Giá trị đặc biệt:** NULL (`Ctrl+0` hoặc menu), DEFAULT (dòng mới), chuỗi rỗng phân biệt với NULL; cột
auto-increment/identity/generated không sửa được; sau khi INSERT, đọc lại dòng (MySQL `LAST_INSERT_ID()`,
PG `RETURNING`) để hiện giá trị server gán.

**Sửa trong lưới kết quả query (P3b, sau 5.2 chính):** khi mọi cột của kết quả đến từ **một** bảng và có đủ
cột khóa. Cần driver điền `ColumnMeta.origin_table/origin_column` (P1 đã để dành): MySQL có `org_table`/
`org_name` trong metadata, PG có OID bảng + attnum. Không thỏa → read-only như hiện tại.

### 5.3 Xem và sửa giá trị lớn

- Hiện cell text/JSON bị cắt ở 64 KB, `Bytes` chỉ có preview 64 byte (P1).
- Command `data_cell_fetch(object, key, column)` lấy giá trị đầy đủ theo khóa dòng (`CellMode::Full`, N0),
  giới hạn 64 MB. Ô đã tải đầy đủ mới được sửa và mới được đưa vào `Snapshot` so sánh xung đột.
- **Value editor** (panel bên hoặc dialog): Text (wrap, đếm ký tự), JSON (format + kiểm tra hợp lệ),
  Hex/Image cho binary, lưu ra file / nạp từ file. Sửa ở đây cũng chỉ stage vào `EditSet`.

### 5.4 Copy và xuất nhanh từ lưới

- Copy as: TSV (có header), CSV, JSON, Markdown, `INSERT` (qua N1 `insert_sql`), `UPDATE` theo khóa.
- Áp dụng cho vùng chọn, dòng chọn hoặc toàn bộ đã tải. Toàn bộ bảng (chưa tải) dùng Export job (§7.1).

### 5.5 Commands P3 (phác thảo)

| Command | Mục đích |
|---|---|
| `table_data(…, filter)` | Như hiện tại + `Filter` |
| `table_row_count(…, filter)` | Đếm theo filter |
| `data_editable(object)` | Khóa dòng dùng được, cột sửa được, `atomic`, mức bảo vệ xung đột, lý do nếu không |
| `data_preview(edit_set)` | Lưu plan bất biến (N2c), trả `plan_id` + câu SQL + verdict + `atomic` |
| `data_apply(plan_id, confirmed)` | Tiêu thụ plan một lần; trả kết quả từng câu, dòng xung đột, `outcome_unknown` |
| `data_cell_fetch(object, key, column)` | Giá trị đầy đủ của một ô |
| `query_tx_mode(tab, mode)` / `query_commit` / `query_rollback` | Manual commit (N2b) |

### 5.6 Tiêu chí hoàn thành P3

- Trên cả 6 server: lọc, sửa, thêm, xóa dòng ở bảng có PK; bảng không PK hiện read-only có lý do.
- Xung đột được phát hiện cho cả UPDATE và DELETE (test: hai session sửa cùng dòng; PG qua `xmin`, MySQL qua
  `Snapshot`), mức bảo vệ hiển thị đúng.
- Bảng MyISAM: Apply từng câu, preview cảnh báo không hoàn tác; Production chặn sửa qua Data tab.
- Preview rồi đổi `sql_mode`/cấu trúc bảng/kết nối lại → Apply trả `PlanStale`, không chạy câu nào.
- Ngắt kết nối giả lập trong lúc `COMMIT` → UI hiện "Không rõ kết quả", không retry.
- Không có đường nào sinh câu ghi từ ô bị cắt (test ở mức kiểu: không có `From<Value> for FullValue` cho `Bytes`).
- Không câu ghi nào chạy mà chưa qua guard; `DELETE FROM t` thiếu WHERE trên Production cần gõ tên bảng.
- Literal round-trip đúng mọi kiểu đã hỗ trợ, kể cả `NO_BACKSLASH_ESCAPES`.
- Manual commit: commit/rollback đúng, cảnh báo DDL ngầm commit trên MySQL, cảnh báo transaction mở lâu.

## 6. P4 — Object

### 6.1 N4 — Crate `schema`: mô hình, diff, render DDL

```rust
pub struct TableDef {                 // dựng từ TableDetail, chỉnh trong designer
    pub name: String, pub columns: Vec<ColumnDef>, pub primary_key: Option<KeyDef>,
    pub indexes: Vec<IndexDef>, pub foreign_keys: Vec<FkDef>, pub checks: Vec<CheckDef>,
    pub options: TableOptionsDef, pub comment: Option<String>,
}
pub struct ColumnDef { pub original: Option<String>, /* None = cột mới */ pub name: String, … }

pub enum Change { CreateTable, DropTable, RenameTable, AddColumn, DropColumn, RenameColumn,
                  AlterColumn { type_, nullable, default, comment }, AddIndex, DropIndex,
                  AddFk, DropFk, AddCheck, DropCheck, SetOptions, SetComment }

pub fn diff(old: &TableDef, new: &TableDef) -> Vec<Change>;
pub fn render(changes: &[Change], target: &ServerInfo) -> Result<Vec<String>, RenderError>;
```

- **Đổi tên được nhận biết qua `original`**, không đoán (tránh biến rename thành drop + add làm mất dữ liệu).
- **Render theo engine và phiên bản:** MySQL gộp vào một `ALTER TABLE …, …`; PG tách câu và chạy trong một
  transaction. Tính năng không có trên phiên bản đích (CHECK trên MySQL 5.7, index hàm, cột invisible…) →
  `RenderError::Unsupported` kèm lý do, không sinh câu sai.
- **Thay đổi phá dữ liệu** (drop cột, thu hẹp kiểu, NOT NULL trên cột có NULL) được gắn cờ trong preview.
- Dùng lại ở P6 cho so sánh schema.

### 6.2 Thao tác nhanh trên tree

Tạo/xóa database (MySQL) / schema (PG); drop, truncate, rename, duplicate (cấu trúc, hoặc cấu trúc + dữ
liệu) cho bảng; drop view/routine/trigger/sequence. Mỗi thao tác: preview lưu plan (N2c) + guard (N3) →
apply theo `plan_id` (N2a/b) → refresh node và SchemaCache.

### 6.3 Table designer

- Tab Designer với các sub-tab Columns, Indexes, Foreign keys, Checks, Options, Comment, **SQL Preview**.
- Mở từ bảng có sẵn (dựng `TableDef` từ `table_detail`) hoặc "New table".
- Chọn kiểu theo engine và phiên bản (danh sách kiểu + độ dài/precision/unsigned/collation).
- Save = diff → render → preview lưu plan (N2c; dấu vân tay gồm cấu trúc bảng lúc mở designer) → guard →
  apply theo `plan_id`. Bảng đã bị người khác đổi cấu trúc → `PlanStale`, mở lại designer trên cấu trúc mới.
  Lỗi giữa chừng trên MySQL (DDL không transaction, `atomic = Không`): báo rõ câu nào đã chạy, tải lại cấu
  trúc thật.

### 6.4 Sửa và apply DDL cho view, routine, trigger

- DDL tab cho phép **Edit** → mở editor với DDL hiện tại → Apply:
  - PG: `CREATE OR REPLACE` khi được; nếu không thì drop + create trong transaction.
  - MySQL: drop + create (không transaction); preview cảnh báo khoảng hở.
- Tạo mới từ template theo engine.

### 6.5 Tiêu chí hoàn thành P4

Designer tạo và sửa bảng (cột, index, FK, check) trên cả 6 server; mỗi câu render được chạy thật trong
integration test; rename không mất dữ liệu; thao tác tree đều qua preview và guard.

## 7. P5 — Import/Export

### 7.1 N5 — Khung job

```
JobManager (src-tauri)
  ├─ job: { id, kind, connection, params, state: Queued|Running|Done|Failed|Cancelled, progress, log }
  ├─ chạy trên session riêng (không dùng session tab / data pool)
  ├─ giới hạn: 2 job đồng thời mỗi connection (setting)
  ├─ tiến độ: app event `job://progress` (job sống lâu hơn tab, nên không dùng Channel của tab)
  ├─ Stop: cancel của driver + cờ dừng giữa các batch
  └─ lưu SQLite (migration mới): lịch sử job, log rút gọn, file kết quả
```

UI: panel **Jobs** (status bar có bộ đếm), xem log, mở file kết quả, chạy lại với cùng tham số. Đóng app khi
job đang chạy → hỏi.

### 7.2 Export

- Nguồn: bảng/view (toàn bộ hoặc theo filter), nhiều bảng một lần, hoặc một câu SELECT.
- Định dạng: CSV/TSV (delimiter, quote, encoding, BOM, header), JSON / JSON Lines, XLSX, SQL `INSERT`
  (gộp n dòng/câu, tùy chọn kèm `CREATE TABLE`), Markdown.
- Stream: `execute_one` với `max_rows` không giới hạn và `CellMode::Full` (N0); `FullRowSink` ghi thẳng ra
  writer, bộ nhớ không phụ thuộc số dòng. BLOB/text dài được ghi nguyên vẹn (CSV/JSON: base64 hoặc hex theo
  tùy chọn; SQL: literal N1). XLSX có giới hạn 1.048.576 dòng/sheet (tự sang sheet mới hoặc báo).

### 7.3 Import

- Nguồn: CSV/TSV, JSON/JSONL, XLSX. Wizard: chọn file → xem trước 100 dòng → map cột (tự khớp theo tên,
  chỉnh tay) → chế độ (append / thay toàn bộ / upsert theo khóa) → xử lý lỗi (dừng / bỏ qua dòng lỗi và ghi
  log) → **chạy thử** → chạy thật (preview + plan như N2c, gắn với file hash + đích + tùy chọn).
- **Chạy thử không bao giờ đụng vào bảng đích.** Nó gồm:
  1. Parse toàn bộ file, chuyển từng ô sang `FullValue` theo kiểu cột đích, báo dòng/ô lỗi (kiểu, độ dài,
     NOT NULL, enum).
  2. Nếu người dùng chọn "kiểm tra trên server": tạo **bảng tạm** cùng cấu trúc (MySQL
     `CREATE TEMPORARY TABLE tmp LIKE t`, không gây commit ngầm; PG `CREATE TEMP TABLE tmp (LIKE t INCLUDING
     DEFAULTS INCLUDING CONSTRAINTS)`), insert vào đó để bắt lỗi constraint/kiểu của server, rồi drop. Ràng
     buộc FK/unique so với dữ liệu sẵn có không kiểm tra được theo cách này, và wizard ghi rõ điều đó.
  - Không chạy `TRUNCATE`/`DELETE` nào trong chạy thử.
- **Chế độ "thay toàn bộ" khi chạy thật:**
  - PG, hoặc MySQL với bảng transactional: `DELETE FROM t` + insert trong **cùng một transaction** (rollback
    được khi lỗi). Tùy chọn `TRUNCATE` cho bảng lớn: PG vẫn trong transaction (TRUNCATE của PG rollback được).
  - MySQL `TRUNCATE` (commit ngầm) hoặc bảng không transactional: là **một bước riêng, không hoàn tác được**.
    Wizard tách thành bước xác nhận riêng (Production: gõ tên bảng), ghi rõ "dữ liệu cũ sẽ mất ngay cả khi
    import sau đó lỗi".
- Lỗi giữa chừng khi không chạy trong một transaction: báo số batch đã ghi; batch đang chạy lúc mất kết nối
  là `outcome_unknown` (N2d).
- Ghi bằng multi-row `INSERT` qua N1, batch theo số byte (mặc định 1 MB) và số dòng. Upsert: MySQL
  `ON DUPLICATE KEY UPDATE`, PG `ON CONFLICT … DO UPDATE`.
- Tối ưu sau (không thuộc v1): PG `COPY FROM STDIN` qua `tokio_postgres::copy_in`.
- Đi qua guard: import vào Production phải xác nhận một lần cho cả job.

### 7.4 N6 — Tách câu dạng stream

`core::sql::split` hiện nhận cả `&str`, không chạy được với file dump vài GB. Cần **splitter tăng dần**: nhận
từng chunk, giữ trạng thái lexer (chuỗi, comment, dollar-quote, `DELIMITER`), trả câu khi đủ. Bắt buộc phải
cho kết quả giống hệt `split` trên cùng input (test so sánh với mọi fixture hiện có + fuzz cắt chunk ngẫu
nhiên).

N6 **không** phải parser của psql. Nó chỉ cần nhận diện hai thứ nằm ngoài SQL để báo chuyển đường (§7.5): câu
`COPY … FROM stdin` (theo sau là khối dữ liệu kết thúc bằng `\.`) và lệnh client bắt đầu bằng `\` ở đầu câu.

### 7.5 Dump và restore

**Dump:**

- **Dump tích hợp:** DDL (`object_ddl`) + dữ liệu (`INSERT` qua N1, giá trị `FullValue`) theo thứ tự phụ thuộc
  FK; chọn object; nén `.sql.gz`. Chỉ đọc, chạy được mọi nơi không cần cài thêm. File có header nhận diện
  (`-- DntDatabase dump v1`) và chỉ chứa SQL mà runner của app chạy được.
- **Dump bằng công cụ ngoài** (`mysqldump`/`mariadb-dump`/`pg_dump`): đi qua N7. Dump là thao tác đọc nên được
  phép cả trên connection read-only.

**Restore: ba đường, chọn theo loại file, không có đường thứ tư:**

| File | Đường chạy | Guard |
|---|---|---|
| Dump của app, hoặc SQL thuần không có lệnh của client (kể cả output `mysqldump`, vốn dùng `DELIMITER` mà runner đã hỗ trợ) | N6 + `query::run` dưới dạng job | Guard từng câu như query tab; Production xác nhận một lần cho cả job |
| SQL thuần của `pg_dump` (có `COPY … FROM stdin` và/hoặc lệnh `\…` của psql) | `psql` qua N7 | Policy N7 |
| Định dạng custom/directory/tar của PG | `pg_restore` qua N7 | Policy N7 |

- N6 **nhận diện** (không cố chạy) `COPY … FROM stdin` và dòng bắt đầu bằng `\` ở đầu câu. Gặp một trong hai
  → báo "file này cần psql", đề nghị đường N7 nếu tìm thấy `psql`, nếu không thì từ chối. Không bao giờ bỏ qua
  im lặng hay chạy được nửa file.
- Hỗ trợ `COPY FROM STDIN` trực tiếp qua `tokio_postgres::copy_in` là tối ưu sau (P5b), không thuộc v1.

### 7.6 N7 — Đường chạy công cụ ngoài

Công cụ ngoài không đi qua `query::run`, nên đây là một **đường chạy riêng có policy bắt buộc trước khi
spawn**, nằm trong backend:

1. **Danh sách trắng:** chỉ `mysqldump`, `mariadb-dump`, `pg_dump`, `pg_restore`, `psql`, tìm trong PATH hoặc
   đường dẫn cấu hình. Tham số do app dựng từ một tập tùy chọn có kiểu, **không** nhận tham số tự do từ UI.
2. **Phân loại:** dump = `Read`; restore (`psql`, `pg_restore`) = ghi + DDL, tương đương câu `Ddl` mạnh nhất.
3. **Policy:**
   - Connection `read_only`: **chặn** mọi restore. Dump chạy được (`pg_dump` vốn chỉ đọc; `mysqldump` dùng
     `--single-transaction` trên InnoDB).
   - `Production`: restore phải xác nhận và gõ tên database đích. `Staging`: xác nhận.
4. **Preview:** `pg_restore --list` (mục lục các object sẽ tạo/ghi), với `psql` là thống kê câu theo loại từ
   lần quét N6; kèm dòng lệnh đầy đủ (che mật khẩu), đích, tùy chọn.
5. **Xác nhận gắn với đúng phiên chạy:** plan N7 lưu SHA-256 của file, connection + generation, database
   đích, tùy chọn. Apply kiểm tra lại cả bốn; file đổi sau preview → `PlanStale`.
6. **Chạy an toàn nhất có thể:** `psql --single-transaction -v ON_ERROR_STOP=1 -X` (không đọc `.psqlrc`),
   `pg_restore --single-transaction --exit-on-error` khi tùy chọn cho phép. Không thể dùng một transaction
   (ví dụ restore có `CREATE DATABASE`, hoặc tắt theo tùy chọn) → preview ghi "chạy dở sẽ để lại trạng thái
   một phần".
7. **Bí mật:** mật khẩu qua file tạm quyền chỉ user hiện tại (`--defaults-extra-file` cho MySQL, `PGPASSFILE`
   cho PG), xóa khi xong, **không** qua dòng lệnh hay biến môi trường. Kết nối SSH: công cụ trỏ vào cổng local
   của tunnel sẵn có.
8. **Kết quả:** exit code + stderr (đã redact) vào log job. Thoát khác 0, bị Stop hoặc mất kết nối giữa chừng:
   khi có `--single-transaction` thì báo "đã rollback" **chỉ khi** công cụ báo lỗi trước khi commit; còn lại
   là "chạy dở / không rõ kết quả" (N2d), đề nghị kiểm tra đích.

### 7.7 Tiêu chí hoàn thành P5

- Export 1 triệu dòng không tăng bộ nhớ quá 200 MB; BLOB 10 MB export rồi import lại đúng từng byte.
- Import CSV có map cột, upsert; chạy thử để bảng đích nguyên vẹn (test: đếm dòng + checksum trước/sau, kể
  cả chế độ "thay toàn bộ" trên MySQL).
- Dump tích hợp rồi restore lại cho kết quả giống hệt (checksum từng bảng) trên cả 6 server.
- File `pg_dump` plain: N6 nhận diện và chuyển sang `psql`; không có `psql` → từ chối, không chạy câu nào.
- Restore qua N7 bị chặn trên connection read-only; file đổi sau preview → `PlanStale`.

## 8. P6 — Quản trị và nâng cao

### 8.1 Process list

MySQL: `information_schema.PROCESSLIST` (hoặc `performance_schema` khi có); PG: `pg_stat_activity`. Tự làm
mới theo chu kỳ, lọc, xem câu đầy đủ. Kill query / kill connection (`KILL QUERY`, `pg_cancel_backend`,
`pg_terminate_backend`) đi qua guard (Production xác nhận).

### 8.2 Server: biến, trạng thái, dung lượng

Biến server (lọc, so sánh session/global), status/uptime, dung lượng theo database/bảng/index (đã có
`size_bytes`, `row_estimate` trong `TableDetail`). Chỉ đọc ở v1.

### 8.3 User và quyền

v1 chỉ đọc: danh sách user/role, quyền theo object. v2: tạo user, grant/revoke qua preview + guard. Mật khẩu
user mới không bao giờ vào History (đã có `redact`; cần thêm test cho `CREATE USER … IDENTIFIED BY`).

### 8.4 So sánh schema

Chọn nguồn và đích (hai connection/database bất kỳ cùng engine) → introspect → `TableDef` (N4) → `diff` →
script đồng bộ → mở trong query tab mới. **Không tự chạy.** Khác engine: chỉ báo khác biệt, không sinh script.

### 8.5 ER diagram

Dựng từ `foreign_keys` của một schema; bố cục tự động (thư viện layout đồ thị trong UI), kéo thả, lưu vị
trí, xuất PNG/SVG. Chỉ đọc ở v1.

### 8.6 Workspace

- Khôi phục object tab (bảng, view, filter, sort, sub-tab) cùng query tab khi mở lại app.
- Quick open `Ctrl+P`: tìm object xuyên các connection đang mở (SchemaCache) + favorites + gần đây.
- Favorites và snippets: đã có migration 3 trên `main`; nối vào quick open.

## 9. Kiểm thử (mọi giai đoạn)

| Lớp | Cách | Bắt buộc cho |
|---|---|---|
| Thuần Rust | unit + property test, không DB | N0, N1, N2 (plan, `atomic`, `PlanStale`), N3, N4, N6, N7 (dựng tham số, policy), định dạng transfer |
| Lỗi giả lập | session/driver giả gây mất kết nối ở từng bước (trước câu, giữa gói, trong `COMMIT`) | N2d, job, N7 |
| Integration | `DNT_TEST_DB=1`, ma trận 6 server trong `docker-compose.test.yml` | Mọi câu sinh ra phải chạy thật ít nhất một lần trên mỗi server hỗ trợ |
| UI | Vitest + `pnpm --dir ui lint` | Stage edit, filter, designer state, wizard |
| Browser E2E | `e2e-web` với backend giả | Luồng sửa/apply/xung đột, designer, import wizard |
| Smoke E2E | tauri-driver trên Windows | Một kịch bản cho mỗi giai đoạn |

CI đang tạm dừng (chạy tay); trước khi merge mỗi giai đoạn phải chạy đủ các lớp trên và ghi kết quả vào
final review như P1/P2.

## 10. Ngoài phạm vi và liên hệ với AI

Spec AI viết sau, nhưng nó sẽ **dựa vào** đúng các khối nền ở đây: N1 (AI sinh thay đổi có cấu trúc thay vì
text tự do), N2 (session có transaction và read-only ép buộc), N3 (guard theo nội dung), N5 (tác vụ dài có
kiểm soát) và History có `origin`. Không thiết kế gì riêng cho AI ở P3–P6.

## 11. Câu hỏi cần chốt khi viết spec chi tiết

1. **Literal hay bind param (N1):** đề xuất literal ở v1 (§4.1). Đồng ý?
2. **Bảng không có khóa:** đề xuất read-only. Có cần cho sửa PG qua `ctid` + `xmin` không (`ctid` đổi sau
   update/`VACUUM FULL`, nhưng `xmin` sẽ báo xung đột thay vì sửa nhầm dòng)?
3. **Bảng mức guard (§4.3):** các mức Confirm/gõ tên theo `Local`/`Staging`/`Production` có hợp với quy
   trình team không? Connection không gắn tag có nên xử lý như `Local` không?
4. **Apply trên Data tab:** commit ngay khi Apply (như Navicat), hay theo Manual commit của connection?
   Đề xuất: commit ngay.
5. **Dump:** ưu tiên công cụ ngoài hay dump tích hợp cho v1? Đề xuất làm cả hai, công cụ ngoài trước.
6. **Thứ tự P5/P6:** team có cần Import/Export gấp hơn Designer (P4) không? Nếu có, đảo P4 và P5 được vì
   chúng chỉ chung N1.

## 12. Lịch sử thay đổi

**Bản 2 (2026-10-20), sửa theo review Codex:**

| # | Vấn đề | Sửa ở |
|---|---|---|
| 1 | Chạy thử import có `TRUNCATE` (MySQL commit ngầm) có thể xóa dữ liệu thật | §7.3: chạy thử không đụng bảng đích, dùng bảng tạm; "thay toàn bộ" tách bước không hoàn tác |
| 2 | N2 hứa rollback cả gói nhưng không kiểm tra engine/trigger | §4.2a: capability `atomic`, Apply từng câu khi không atomic, Production chặn |
| 3 | `Value` đã bị cắt nên không đủ để ghi/export | §4.0 N0: `FullValue`, `CellMode`, API ghi chỉ nhận `FullValue` |
| 4 | Preview và Apply có thể khác nhau | §4.2c: plan bất biến `plan_id`, tiêu thụ một lần, `PlanStale` |
| 5 | DELETE không có giá trị gốc để so xung đột | §5.2: `RowGuard` (PG `xmin`, MySQL snapshot mọi cột so sánh được), báo mức bảo vệ |
| 6 | Mất kết nối không đồng nghĩa rollback | §4.2d: phân biệt trước/trong `COMMIT`, giữ `outcome_unknown`, không retry |
| 7 | Restore qua công cụ ngoài không đi qua guard | §7.6 N7: đường chạy riêng, policy trước khi spawn, plan gắn file hash |
| 8 | N6 không chạy được output plain của `pg_dump` | §7.4–7.5: N6 nhận diện `COPY … FROM stdin`/lệnh `\`, chuyển sang `psql` qua N7 hoặc từ chối |