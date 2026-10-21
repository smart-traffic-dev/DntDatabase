# P4b — Table designer và crate `schema`

- Ngày: 2026-10-21
- Trạng thái: Đã duyệt thiết kế trong chat (4 phần), chờ review bản viết.
- Cơ sở: roadmap P3–P6 §6.1, §6.3; spec P4a (`2026-10-21-p4a-object-actions-design.md`); canvas UX redesign board 10 "P4b Table designer + SQL preview".
- Nhánh: `feat/p4b-table-designer`, tách từ `feat/p4a-object-actions`. Dùng lại hạ tầng plan/DDL session/operation của P4a, nên chỉ merge sau P4a.

## 1. Mục tiêu

Người dùng tạo bảng mới và sửa bảng có sẵn (cột, primary key, index, foreign key, check, tùy chọn bảng, comment) bằng form, không tự viết DDL. Mọi thay đổi đi qua preview SQL, guard ở backend và kết quả rõ ràng. Không có đường nào âm thầm làm mất thuộc tính của bảng mà app không hiểu.

Local-first như P4a: không thêm CI, cloud hay dịch vụ ngoài.

## 2. Quyết định đã chốt

| Chủ đề | Quyết định |
|---|---|
| Phạm vi v1 | Một đợt: New table và sửa bảng có sẵn, đủ cột/PK/index/FK/check/options/comment |
| Dựng `TableDef` từ bảng cũ | Mở rộng introspection có cấu trúc + kiểm tra round-trip với DDL của server; lệch thì khoá đúng phần tử đó (§5) |
| Khoá bảng lâu | Ước lượng số dòng, cảnh báo thay đổi có thể rebuild bảng, chờ lock tối đa 5 s như P4a. Không có tùy chọn online DDL |
| Thay đổi phá dữ liệu | Preview đếm số dòng bị ảnh hưởng (truy vấn đọc, timeout) và tô đỏ |
| Xác nhận | Hai mức: Production gõ đúng tên bảng cho mọi thay đổi; Staging, Local, không tag chỉ bấm Apply |
| MySQL / MariaDB | Mọi thay đổi gộp vào một câu `ALTER TABLE` |
| PostgreSQL | Nhiều câu trong một transaction, thứ tự cố định |
| Xoá bảng | Không có trong designer; dùng thao tác Drop của P4a trên tree |

## 3. Phạm vi

| Thao tác | MySQL / MariaDB | PostgreSQL |
|---|---|---|
| New table | `CREATE TABLE` một câu | `CREATE TABLE` + `COMMENT ON` + `CREATE INDEX` trong transaction |
| Thêm / xoá / đổi tên cột | Có | Có |
| Sửa kiểu, độ dài, nullable, default, comment, collation, unsigned, auto increment | Có (`MODIFY COLUMN` / `CHANGE COLUMN`) | Có (`ALTER COLUMN … TYPE … [USING]`, `SET/DROP NOT NULL`, `SET/DROP DEFAULT`), identity thay cho auto increment |
| Đổi thứ tự cột | Có (`FIRST` / `AFTER`) | Không (ẩn điều khiển) |
| Primary key | Thêm / xoá / đổi cột | Thêm / xoá / đổi cột |
| Index thường, unique | Thêm / xoá / đổi tên / đổi cột; prefix length, ASC/DESC, visible (MySQL 8+) | Thêm / xoá / đổi tên; method (btree, hash, gin, gist, brin), ASC/DESC |
| Foreign key | Thêm / xoá; ON UPDATE / ON DELETE | Thêm / xoá; ON UPDATE / ON DELETE, deferrable |
| Check | MySQL 8.0.16+, MariaDB 10.2+; MySQL 5.7 → `Unsupported` | Có |
| Tùy chọn bảng | Engine, charset/collation, row format, comment, đổi tên bảng | Comment, đổi tên bảng |

Thay đổi một phần tử mà engine không hỗ trợ "sửa tại chỗ" (index, FK, check) được render thành drop + add trong cùng câu (MySQL) hoặc cùng transaction (PG).

Ngoài phạm vi: xoá bảng (P4a), duplicate bảng (P5), online DDL (`ALGORITHM`, `LOCK`, `CONCURRENTLY`), sửa partition và system versioning (chỉ xem), trigger (P4c), sửa nhiều bảng một lần, so sánh schema (P6, dùng lại crate này), đổi thứ tự cột trên PG, generated column và index biểu thức (hiển thị, khoá; tạo mới để sau).

## 4. Kiến trúc

### 4.1 Mở rộng introspection (`crates/core/src/meta.rs` + hai driver)

Thêm trường mới dạng `Option`/mặc định rỗng để code hiện có không đổi hành vi:

- `ColumnDetail`: `charset`, `on_update` (MySQL `ON UPDATE CURRENT_TIMESTAMP…`), `identity_kind` (PG `ALWAYS` / `BY DEFAULT`), `unsigned`, `enum_values`.
- `IndexDetail`: `parts: Vec<IndexPart>` với `IndexPart { column: Option<String>, expression: Option<String>, prefix: Option<u32>, descending: bool }`; `visible: bool` (MySQL 8+), `include: Vec<String>` và `nulls_not_distinct: bool` (PG 15+), `comment`.
- `ForeignKeyDetail`: `match_type`, `deferrable`, `initially_deferred`.
- `TableOptions`: `charset`, `row_format`, `auto_increment`, `extra: Vec<(String, String)>` cho tùy chọn còn lại đọc được.

Driver điền từ catalog (`information_schema`, `pg_catalog`). Không parse DDL để lấy các trường này.

### 4.2 Crate mới `crates/schema` (`dnt-schema`)

Thuần Rust, không gọi DB, phụ thuộc `dnt-core`. Mọi nhánh test được bằng unit test.

```rust
pub struct TableDef {
    pub original_name: Option<String>,   // None = bảng mới
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub primary_key: Option<KeyDef>,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<FkDef>,
    pub checks: Vec<CheckDef>,
    pub options: TableOptionsDef,
    pub comment: Option<String>,
}
pub struct ColumnDef {
    pub original: Option<String>,        // None = cột mới; đổi tên nhận biết qua đây, không đoán
    pub name: String,
    pub data_type: TypeSpec,             // base + length/precision/scale/unsigned/enum values, hoặc Custom(text)
    pub nullable: bool,
    pub default: Option<DefaultSpec>,    // Value(literal) | Expression(text)
    pub auto_increment: bool,            // MySQL
    pub identity: Option<IdentityKind>,  // PG
    pub on_update: Option<String>,       // MySQL
    pub collation: Option<String>,
    pub comment: Option<String>,
    pub using: Option<String>,           // PG: biểu thức USING khi đổi kiểu, do người dùng nhập
    pub locked: bool,
}
// IndexDef, FkDef, CheckDef, KeyDef có `original: Option<String>` và `locked: bool` tương tự.

pub fn from_detail(detail: &TableDetail, server: &ServerInfo) -> TableDef;
pub fn fidelity(def: &TableDef, server_ddl: &ServerDdl, server: &ServerInfo) -> Vec<Locked>;
pub fn validate(def: &TableDef, server: &ServerInfo) -> Vec<DraftError>;
pub fn diff(old: &TableDef, new: &TableDef) -> Result<Vec<Change>, DiffError>;
pub fn render(table: &TableRef, changes: &[Change], server: &ServerInfo) -> Result<Rendered, RenderError>;
pub fn render_create(table: &TableRef, def: &TableDef, server: &ServerInfo) -> Result<Rendered, RenderError>;
pub fn type_catalog(server: &ServerInfo) -> Vec<TypeInfo>;

pub struct Rendered {
    pub statements: Vec<String>,
    pub impacts: Vec<Impact>,            // phá dữ liệu, có thể rebuild bảng, không hoàn tác (MyISAM…)
    pub counts: Vec<ImpactCount>,        // truy vấn đọc để đếm dòng bị ảnh hưởng, gắn với Impact
}
```

- `diff` có thứ tự ổn định và từ chối thay đổi chạm phần tử `locked` (`DiffError::Locked`), kể cả gián tiếp: đổi tên/xoá cột mà một phần tử bị khoá tham chiếu.
- Identifier quote qua `core::sql::ident`; default dạng giá trị đi qua literal của N1 (`core::sql::build`).
- `Custom` type, default dạng biểu thức, `USING` và biểu thức check được kiểm tra bằng lexer chung: một biểu thức, không có `;` ở độ sâu 0, không có từ khoá ghi/DDL, ngoặc cân bằng. Server vẫn là bên kiểm tra cuối.
- Tính năng không có trên phiên bản đích → `RenderError::Unsupported { feature, min_version }`, không sinh câu sai.

### 4.3 Backend (`src-tauri/src/table_design.rs` + `commands/table_design.rs`)

Dùng lại cơ chế của P4a (`object_actions.rs`): plan trong bộ nhớ tiêu thụ một lần, TTL như plan query, session DDL riêng, operation id do UI cấp, cancel, lock timeout 5 s, History. Phần dùng chung được tách ra module chung khi cần, không sao chép.

| Command | Việc |
|---|---|
| `table_design_open(connection_id, object)` | Đọc `TableDetail` + DDL server, dựng `TableDef`, chạy `fidelity`, trả `TableDef`, danh sách khoá, `type_catalog`, `fingerprint`. `object = None` (kèm scope) là New table |
| `table_design_render(connection_id, fingerprint, draft)` | `validate` + `diff` + `render`. Không tạo plan, không đếm dòng. Dùng cho SQL preview tự cập nhật |
| `table_design_preview(connection_id, fingerprint, draft)` | Render lại từ draft, chạy truy vấn đếm (read-only, timeout 2 s mỗi câu), guard, lưu plan. Trả `plan_id`, SQL, impacts kèm số đếm, mức xác nhận |
| `table_design_apply(plan_id, confirmation, operation_id, channel)` | Kiểm tra lại rồi chạy đúng SQL đã lưu (§6) |
| `table_design_cancel(operation_id)` | Như `object_action_cancel` |

`fingerprint` = hash của `TableDetail` đã chuẩn hoá + DDL server đã chuẩn hoá + danh sách khoá.

### 4.4 UI (`ui/src/tabs/designer/`)

Theo board 10:

- Tab "Design · {table}" mở từ context menu của bảng ("Design table") và của database/schema ("New table…"); dấu chấm khi có thay đổi chưa lưu, guard đóng tab như query tab.
- Header một dòng: tên bảng, `connection › database` · engine, segmented sub-tab Columns / Indexes / Foreign keys / Checks / Options / Comment (kèm số đếm), chip "n thay đổi chưa lưu", Discard, "Review & apply…".
- Lưới Columns: kéo thả đổi thứ tự (ẩn trên PG), cột PK, Name, Type, Length, Null, Default, Comment; dòng sửa / mới / sẽ xoá có dấu hiệu riêng, "Hoàn tác" từng dòng; nút Add column (`Ctrl+Enter`).
- Inspector bên phải cho phần tử đang chọn: các trường theo `TypeInfo` (precision/scale, unsigned, collation chỉ khi kiểu cho phép), giá trị trước khi sửa.
- Ngăn SQL preview phía dưới: SQL tự cập nhật (debounce 300 ms), Copy, Open in query; cột Ảnh hưởng bên cạnh.
- Phần tử bị khoá: icon khoá, tooltip lý do + DDL gốc của server, không sửa được.
- Draft ở frontend (store riêng theo tab), Undo/Redo trong designer (`Ctrl+Z` / `Ctrl+Shift+Z`). Backend không nhận SQL từ UI, chỉ nhận `TableDef`.
- Toàn bộ chữ trên UI bằng tiếng Anh.

## 5. Kiểm tra round-trip và khoá

Khi mở designer cho bảng có sẵn:

1. Đọc DDL chuẩn của server: MySQL/MariaDB `SHOW CREATE TABLE`; PG `format_type`, `pg_get_expr` (default), `pg_get_indexdef`, `pg_get_constraintdef`, `col_description`/`obj_description`.
2. Sinh định nghĩa từng phần tử từ `TableDef` (cùng hàm render với `render_create`).
3. Chuẩn hoá cả hai: khoảng trắng, quote, hoa/thường của từ khoá, giá trị mặc định server tự thêm (`DEFAULT NULL`, `CHARSET`/`COLLATE` trùng của bảng, `USING BTREE` mặc định, độ rộng hiển thị số nguyên của MySQL 5.7…).
4. So theo từng phần tử (cột, PK, index, FK, check, tùy chọn bảng). Phần lệch → `Locked { element, reason, server_text }`, `locked = true` trong `TableDef`.

Khoá cả bảng (designer mở chế độ chỉ xem, có lý do): bảng partition, system versioning, bảng PG có inheritance hoặc là partition con, không đọc được DDL server.

Phần tử bị khoá không bao giờ xuất hiện trong `diff`, nên SQL sinh ra không chạm vào nó. Với MySQL, câu `ALTER` chỉ nhắc tới phần tử được sửa, nên phần tử khoá giữ nguyên trên server.

## 6. Luồng Save và thực thi

1. **Open** → `TableDef` + khoá + fingerprint.
2. **Sửa** draft; `table_design_render` sau mỗi thay đổi. `DraftError` (tên trùng/rỗng, bảng không cột, thiếu tham số kiểu, FK không khớp số cột) và `Unsupported` hiện tại ô gây lỗi; nút Review bị vô hiệu khi còn lỗi.
3. **Review & apply** → `table_design_preview`. Hộp xác nhận hiện SQL chỉ đọc, impacts (phá dữ liệu tô đỏ kèm số dòng hoặc "không đếm được"), cảnh báo rebuild với ước lượng số dòng, cảnh báo không hoàn tác khi engine không transactional.
4. **Xác nhận** theo §7.
5. **Apply**: trước câu đầu tiên đọc lại `TableDetail` + DDL server và so fingerprint; kiểm tra lại connection/generation, env, read-only, dialect. Lệch → `PlanStale`, không chạy câu nào; designer báo "Table changed", cho mở lại trên cấu trúc mới và giữ draft cũ để đối chiếu.
6. Sau khi chạy (thành công hay không), tải lại cấu trúc thật vào designer; invalidate schema cache, completion cache; refresh tree và các tab của bảng (đổi tên bảng cập nhật định danh tab như Rename của P4a).

**MySQL / MariaDB:** một câu `ALTER TABLE t …, …` (gồm `RENAME TO`, `FIRST`/`AFTER`, drop + add phần tử). Trên InnoDB, server áp dụng cả hoặc không. Mất kết nối / timeout sau khi gửi → `outcome_unknown`, không retry. New table: một câu `CREATE TABLE`.

**PostgreSQL:** `BEGIN`, rồi theo thứ tự: drop FK / check / index / PK → rename bảng → rename cột → sửa cột → thêm cột → thêm PK / unique / index / check / FK → `COMMENT ON` → `COMMIT`. Lỗi bất kỳ → rollback cả gói; chỉ báo "rolled back" khi server xác nhận. Đổi kiểu tự thêm `USING col::newtype` chỉ với cặp kiểu nằm trong danh sách an toàn; ngoài danh sách, người dùng nhập `USING` trong inspector.

**Guard:** câu do designer sinh mang cờ N3 như câu người dùng viết (`AlterTable`; `CREATE TABLE` là `Ddl`). Connection read-only chặn toàn bộ ở backend. Guard chỉ được tăng mức so với query guard.

## 7. Xác nhận

| Môi trường | Mọi thay đổi |
|---|---|
| Production | Gõ đúng tên bảng (tên do backend trả, giữ hoa/thường, không trim) |
| Staging, Local, không tag | Bấm Apply |

Hộp review luôn hiện SQL và impacts; bước review không bao giờ bị bỏ qua. Mức xác nhận do backend tính và kiểm tra lại khi apply.

## 8. Lỗi

| Loại | UI |
|---|---|
| `DraftError` | Tại ô gây lỗi, chặn Review |
| `RenderError::Unsupported` | Lý do + phiên bản tối thiểu |
| `DiffError::Locked` | Lý do + phần tử khoá liên quan |
| `PlanStale` | "Table changed", mở lại, giữ draft cũ |
| Lỗi server (quyền, dữ liệu vi phạm NOT NULL/unique/FK, lock timeout) | Nguyên văn thông báo server, giữ draft |
| `outcome_unknown` | Không báo thành công, bắt buộc tải lại cấu trúc |
| Apply xong nhưng tải lại / ghi History thất bại | Báo riêng, không đổi kết quả apply, không retry |

History ghi origin `table_design`, SQL thực tế, trạng thái, lỗi, qua cơ chế redaction hiện có.

## 9. Kiểm thử và tiêu chí hoàn thành

1. Unit `dnt-schema`: `diff` từng loại thay đổi (gồm rename qua `original`, phần tử khoá, tham chiếu gián tiếp), thứ tự ổn định, `render` theo engine và phiên bản (gồm `Unsupported`), `validate`, bộ chuẩn hoá DDL với fixture `SHOW CREATE TABLE` / `pg_get_*def` thật của 6 server, kiểm tra biểu thức bằng lexer.
2. Round-trip trên server thật (`DNT_TEST_DB=1`; MySQL 5.7/8.0/8.4, MariaDB 10.6/11.4, PostgreSQL 12/17): bảng fixture → `from_detail` → `render_create` vào bảng mới → introspect lại ra `TableDef` tương đương, `fidelity` rỗng; bảng fixture "lạ" (index biểu thức, generated column, partition) khoá đúng phần tử.
3. Integration từng loại thay đổi trên mỗi engine hỗ trợ, kiểm tra cấu trúc và dữ liệu sau apply; rename cột giữ dữ liệu; phần tử khoá giữ nguyên sau khi sửa phần tử khác.
4. Fault injection: plan stale, lỗi giữa chừng PG rollback cả gói, lỗi ALTER MySQL, lock timeout, mất kết nối khi `COMMIT` → `outcome_unknown`, đếm dòng hết giờ.
5. UI: sửa draft, Undo/Redo, SQL preview tự cập nhật, lỗi tại ô, phần tử khoá, hai mức xác nhận, stale và mở lại, ẩn đổi thứ tự cột trên PG, guard đóng tab.
6. Desktop smoke MySQL 8 và PostgreSQL 17: New table, sửa cột/index/FK, apply.
7. fmt, clippy, Rust tests, UI lint/tests, e2e web; ghi môi trường, lệnh và kết quả vào final review. DB test bị skip không tính là đã kiểm chứng server.

Hoàn thành khi designer tạo và sửa được bảng trên cả 6 server; mọi câu render đã chạy thật trong integration test; preview trùng từng byte với SQL gửi đi; phần tử khoá không bao giờ bị đổi; không câu nào chạy mà chưa qua guard; Production bắt gõ tên bảng; không tự retry; `outcome_unknown` giữ nguyên đến UI và History.
