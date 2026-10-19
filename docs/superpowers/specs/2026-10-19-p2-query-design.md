# DntDatabase — P2: Query — Design Spec

- **Ngày:** 2026-10-19
- **Trạng thái:** Chờ review
- **Dựa trên:** [P1 spec](2026-10-19-p1-core-design.md), [pre-P2 final review](../plans/2026-10-19-pre-p2-final-review.md)
- **Canvas UI:** https://claude.ai/artifact/7Uh7EGUTkNvKgBXUWcBB3B (artboard *Workspace – Query editor*). Hộp xác nhận Production, trạng thái Stopping… và tab Explain/History chưa có artboard; P2 dựng theo mô tả trong spec này.

## 1. Mục tiêu và phạm vi

Team dùng DntDatabase hằng ngày để viết và chạy SQL thay Navicat. P2 thành công khi người dùng chạy được script nhiều câu, xem kết quả lớn mượt, có autocomplete, lịch sử và Explain, và **không ai lỡ tay ghi lên Production hay vào connection read-only**.

**Có:**

- Tab query dùng Monaco: nhiều tab, nhiều câu mỗi tab, tự lưu và khôi phục, Open/Save `.sql`.
- Chạy câu tại con trỏ, vùng bôi đen hoặc cả tab; Stop; trạng thái Stopping….
- Lưới kết quả Glide Data Grid, stream qua `Channel`; tab Messages.
- Autocomplete từ SchemaCache, hỗ trợ đổi database, tiền tố `db.`/`schema.` và alias.
- Lịch sử query (SQLite, có redact mật khẩu).
- Explain (dựng cây) và Explain Analyze.
- Guard: xác nhận trên Production, chặn trên read-only.
- Các việc kế thừa từ review pre-P2: I4, M1, M3, M5, M6, `Other` trên Production, trạng thái PG transaction aborted, `sql_mode` của MySQL (§4.6).

**Không có** (giai đoạn sau): chế độ Manual commit (P3), sửa ô trong lưới (P3), Export/Copy as CSV/INSERT (P5), Format SQL, snippet, gợi ý routine/tham số, Visual Explain dạng sơ đồ (P6). Các nút tương ứng trên canvas (`Format`, `Export…`, `Copy as…`) ẩn trong P2.

## 2. Quyết định đã chốt

| # | Chủ đề | Quyết định |
|---|---|---|
| 1 | Phím chạy | `Ctrl+Enter`: câu tại con trỏ, hoặc vùng bôi đen nếu có. `Ctrl+Shift+Enter`: cả tab |
| 2 | Lỗi giữa script | Mặc định dừng. Công tắc "Continue on error" theo tab. Lỗi trong transaction **luôn** dừng |
| 3 | Transaction | Chỉ auto-commit. Hiển thị In transaction / Aborted, nút Rollback, cảnh báo khi đóng tab/app lúc transaction mở |
| 4 | Production | Một hộp xác nhận cho cả batch, hiện trước câu đầu tiên. `Other` cũng xác nhận, trừ danh sách trắng. Read-only áp cùng quy tắc nhưng chặn |
| 5 | Lịch sử | SQLite, redact literal mật khẩu trước khi lưu, giữ 10.000 mục. Log `debug` dùng chung hàm redact |
| 6 | Tab | Tự lưu vào SQLite, khôi phục nhưng không tự kết nối; Open/Save `.sql`. Nội dung editor **không** bị redact |
| 7 | Kết quả lớn | Giới hạn dòng theo tab: 1k / 10k (mặc định) / 100k / 1M. Nút Fetch all chỉ cho câu `Read` |
| 8 | Explain | Explain thuần dựng cây (MySQL 5.7: bảng). Explain Analyze chỉ cho `Read`, Production phải xác nhận |
| 9 | Timeout | Mặc định tắt; "Query timeout (s)" theo connection. Trạng thái Stopping… có đếm giây |
| 10 | Autocomplete | Từ khóa, hàm, bảng/view/cột của database đang chọn, tiền tố `db.`/`schema.`, alias. Làm mới cache sau `Ddl` |
| 11 | Kiến trúc | Tầng app điều khiển script (crate `query`), driver chạy từng câu (`execute_one`) |

## 3. Kiến trúc

```
crates/
├─ core/      # + TxState, StmtOutcome, ExecProgress, Session::execute_one, sql::session_effect,
│             #   Dialect mang cờ sql_mode
├─ query/     # MỚI, không phụ thuộc Tauri/SQLite: plan, guard, runner, redact, explain
├─ driver-mysql/, driver-pg/   # thân vòng lặp execute → execute_one
├─ store/     # + migration 2: query_tabs, query_history
src-tauri/    # + QuerySessions trong SessionManager, commands query_*, RunEvent qua Channel
ui/           # + Monaco, Glide Data Grid, tabs/QueryTab, store/query*.ts
```

**Luồng chạy:**

```
UI ──plan_run──▶ query::plan + Guard ──▶ PlanView (verdict từng câu)
UI (xác nhận nếu cần) ──run(plan_id, confirmed, channel)──▶ Guard kiểm tra lại
   ──▶ ScriptRunner ──execute_one──▶ Session (driver) ──▶ DB
 ▲                       │
 └── Channel<RunEvent> ◀─┘  (StatementStart, ResultStart, Rows, ResultEnd, …, Done)
```

**Quy tắc:**

- `query` chỉ phụ thuộc `core`. Mọi logic quyết định (câu nào chạy, có hỏi không, khi nào dừng) nằm ở đây và được test không cần DB.
- Ranh giới câu chỉ được tính **một lần**, bằng `core::sql::split`. Kết quả này dùng chung cho guard, chạy, highlight lỗi và history.
- Backend tự áp guard. Frontend chỉ hiển thị kết quả, không thể vượt guard.

## 4. Backend thực thi

### 4.1 Thay đổi trong `core`

```rust
pub enum TxState { Idle, Active, Aborted }           // Aborted chỉ xảy ra trên PG

pub struct StmtOutcome {
    pub results: Vec<ResultSummary>,  // CALL của MySQL có thể trả nhiều result set
    pub late_completion: bool,        // hoàn tất trong grace sau timeout (M1)
}

pub enum ExecProgress { CancelSent, GraceExpired }

pub struct ExecOptions {
    pub max_rows: usize, pub batch_size: usize, pub timeout: Option<Duration>,
    pub cancel_grace: Duration,       // mặc định 10 s; test đặt ngắn hơn (M6)
}

#[async_trait]
pub trait Session {
    async fn execute_one(&self, stmt: &str, kind: StatementKind, opts: &ExecOptions,
                         sink: &mut dyn RowSink) -> Result<StmtOutcome, DbError>;
    fn tx_state(&self) -> TxState;
    fn dialect(&self) -> Dialect;     // gồm cờ sql_mode hiện tại (MySQL)
    // execute(script) giữ lại: phương thức mặc định, split rồi gọi execute_one, dừng ở lỗi đầu
    // in_transaction() giữ lại: phương thức mặc định, tx_state() != Idle
    …
}

pub trait RowSink {
    …
    fn progress(&mut self, _p: ExecProgress) {}       // mặc định không làm gì
}
```

- Thân vòng lặp `execute` hiện có của mỗi driver chuyển nguyên vào `execute_one`: theo dõi transaction, timeout/grace, reconnect, retry `Read` và ghi chú reset session. Hành vi từng câu giữ nguyên như sau pre-P2 hardening. `execute_one` vẫn phải gọi `run.cancelled()` trước khi gửi câu.
- `DbError::Timeout` và `DbError::Cancelled` có thêm trường `was_in_tx: bool` (**I4**). Từ đó `retryable = !outcome_unknown && !was_in_tx`, áp cho cả hai driver: trên PG, transaction đã bị aborted; trên MySQL, chạy lại trong transaction dễ áp hai lần.
- `AppError` có thêm `tx_state: Option<String>` để UI cập nhật status bar ngay khi có lỗi.

### 4.2 Trạng thái transaction

| Sự kiện | MySQL | PostgreSQL |
|---|---|---|
| `BEGIN`/`START TRANSACTION` thành công, hoặc `SET autocommit=0` | → Active | → Active |
| `COMMIT`/`ROLLBACK`/`END`/`ABORT` thành công | → Idle | → Idle |
| Lỗi bất kỳ khi Active (kể cả timeout/cancel) | giữ Active | → **Aborted** |
| `ROLLBACK TO [SAVEPOINT]` thành công khi Aborted | — | → Active |
| `COMMIT` khi Aborted | — | → Idle (server tự rollback; Messages ghi "Transaction was rolled back") |
| Mất kết nối | → Idle (báo `was_in_tx`) | → Idle (báo `was_in_tx`) |

Đây là PG **aborted-tx**. Khi `tx_state = Aborted`, `ScriptRunner` không gửi thêm câu nào ngoài `ROLLBACK`, `ROLLBACK TO`, `COMMIT`/`END`/`ABORT`. Script dừng với `StopReason::TxAborted`, và status bar hiện "Transaction aborted" màu đỏ kèm nút **Rollback**. Người dùng không phải đợi lỗi `25P02` mới biết.

### 4.3 Câu hoàn tất muộn (M1)

- Câu hết timeout nhưng hoàn tất trong thời gian grace được giữ nguyên kết quả, và `late_completion = true`. `ScriptRunner` dừng script với `StopReason::LateCompletion`. Messages ghi: "Statement finished after the timeout; the remaining statements were not run."
- Driver PG gửi cancel request bằng tay (gói 16 byte, đi cùng đường TLS/SSH như kết nối chính) rồi **đọc socket cancel tới EOF**, giới hạn bởi `cancel_grace`. Postmaster gửi `SIGINT` tới backend trước khi đóng socket, nên khi thấy EOF thì tín hiệu đã nằm sẵn ở backend. Backend sẽ bỏ qua nó khi đang đọc lệnh, nên không có `57014` lạc nào rơi vào câu sau. pid và secret key lấy từ `BackendKeyData` lúc kết nối.
- Không thấy EOF (hết thời gian hoặc lỗi IO) thì tín hiệu có thể vẫn đang trên đường. Kết nối bị đánh dấu "phải thay" và được thay trước câu kế tiếp, giống ca `Stuck`. Nếu session còn state riêng thì câu kế tiếp nhận `ConnectionLost` kèm ghi chú reset.
- Việc này phối hợp với session `pg-stray-cancel`: nếu nhánh đó merge phần đọc EOF trước, P2 chỉ cần thêm cờ `late_completion`.

### 4.4 Trạng thái phiên (M3) và tách từ (M5)

- Hàm `core::sql::session_effect(stmt, dialect) -> Option<SessionEffect>` thay cho các bản riêng trong từng driver (`search_path_change`, phần dò `session_custom`). Hàm này đọc **cả hai cách hiểu** của `/*!NNNNN … */`, như `classify` đang làm. Vì vậy `/*!40101 SET NAMES utf8 */` được nhận ra là thay đổi state của session (**M3**).
- Lexer trong `core` coi mọi byte không phải ASCII là ký tự của định danh, đúng với cả MySQL lẫn PG. Cả hai driver và `classify` dùng chung hàm `is_ident_byte` này (**M5**).

### 4.5 Crate `query`

```rust
pub enum Target { Cursor(u32), Selection(u32, u32), All }   // offset UTF-16, khớp Monaco

pub struct PlannedStmt { pub index: u32, pub range: (u32, u32), pub text: String,
                         pub kind: StatementKind, pub verdict: Verdict }
pub enum Verdict { Allow, Confirm, Block }

pub fn plan(text: &str, target: Target, dialect: Dialect) -> ScriptPlan;
pub fn guard(plan: &mut ScriptPlan, env: Option<EnvTag>, read_only: bool, analyze: bool);

pub enum StopReason { Error { index: u32 }, Cancelled, TxAborted, LateCompletion,
                      DialectChanged { index: u32 } }

pub async fn run(session: &dyn Session, plan: &ScriptPlan, opts: &ExecOptions,
                 continue_on_error: bool, cancel: &RunCancel,
                 sink: &mut dyn RunSink) -> RunReport;   // RunReport { stop: Option<StopReason>, per-stmt outcomes }
```

**`plan`:**

- `All`: tách cả văn bản.
- `Selection`: chỉ tách phần được bôi đen, giống Navicat. `DELIMITER` phải nằm trong vùng chọn.
- `Cursor`: tách cả văn bản và lấy câu chứa con trỏ. Nếu con trỏ nằm ở khoảng trắng hoặc comment giữa hai câu thì lấy câu gần nhất kết thúc trước con trỏ. Văn bản rỗng cho ra plan rỗng.
- Range của câu được đổi giữa byte và UTF-16 tại một chỗ duy nhất, ở crate này.

**`run`:**

- Chạy lần lượt từng câu. Trước mỗi câu, kiểm tra cancel và `tx_state`.
- Câu lỗi:
  - Đang trong transaction (`was_in_tx`) hoặc `tx_state ≠ Idle` sau lỗi: dừng.
  - Ngược lại: dừng, hoặc chạy tiếp nếu `continue_on_error`.
- `Cancelled` luôn dừng.
- Hai lỗi `ConnectionLost` và `outcome_unknown` luôn dừng, kể cả khi đang bật Continue on error.

### 4.6 `sql_mode` của MySQL

- `Dialect::MySql` mang thêm cờ `{ ansi_quotes, no_backslash_escapes }`. `split`, `classify` và `strip_comments` đọc theo cờ này.
- Session đọc `@@sql_mode` khi kết nối và sau mọi câu có `SessionEffect::SqlMode`. `Session::dialect()` trả về cờ hiện tại.
- Một câu đổi `sql_mode` giữa script làm cờ thay đổi. Khi đó `run` tách lại phần còn lại theo cờ mới. Nếu ranh giới câu khác plan cũ thì dừng với `StopReason::DialectChanged`: "sql_mode changed; run the remaining statements again". Lý do là không chạy câu nào mà guard chưa xét theo đúng ranh giới.

### 4.7 Session của tab trong `SessionManager`

- `QuerySessions: HashMap<TabId, QuerySession { connection_id, database, session, run: Option<RunHandle> }>` nằm trong `ConnectionRuntime`, dùng chung tunnel.
- Session mở lười khi chạy lần đầu, và cũng mở luôn khi gọi `plan_run`, vì cần `dialect()`.
- **Đổi database của tab:**
  - MySQL chạy `USE`.
  - PG mở session mới cho database mới rồi đóng session cũ.
  - Nếu session cũ đang `Active`/`Aborted` thì UI hỏi trước: Commit / Rollback / Cancel.
- Đóng tab thì đóng session. `close_connection` đóng mọi session của tab thuộc connection đó.
- Mỗi tab chỉ có **một run** tại một thời điểm. Gọi `plan_run` hay `run` khi tab đang chạy sẽ nhận lỗi `Busy`.

### 4.8 Tauri commands (P2)

| Command | Mô tả |
|---|---|
| `query_plan(tab_id, conn_id, database, text, target, analyze?)` | Trả `PlanView { plan_id, stmts: [{index, range, kind, verdict, preview}] }`. Backend giữ plan 2 phút, gắn với tab |
| `query_run(plan_id, confirmed, opts, channel)` | Kiểm tra guard lại: có `Block` thì từ chối; có `Confirm` mà `confirmed = false` thì từ chối. Trả `run_id`; mọi kết quả đi qua `channel` |
| `query_cancel(run_id)` | Gửi cancel và phát `Stopping` |
| `query_explain(tab_id, conn_id, database, text, cursor, analyze)` | Dựng plan cho câu tại con trỏ rồi chạy như `query_run`. Kết quả là `PlanTree` (§8.2) |
| `query_tx(tab_id, action: Commit\|Rollback)` | Dùng cho nút Rollback/Commit trên status bar và khi đóng tab |
| `query_close_tab(tab_id)` | Đóng session của tab |
| `statement_ranges(text, dialect)` | Thuần, không cần session. Dùng để highlight câu tại con trỏ (debounce 150 ms) |
| `completion_scope(conn_id, database)`, `completion_objects(conn_id, database, schema?)` | §7.3 |
| `history_list(filter)`, `history_delete(ids)`, `history_clear()` | §8.1 |
| `tabs_load()`, `tabs_save(tab)`, `tabs_delete(id)`, `file_read(path)`, `file_write(path, text)` | §7.2 |

## 5. Guard: xác nhận và chặn

### 5.1 Quy tắc

| Loại câu | Production (không read-only) | Read-only (mọi tag) | Còn lại |
|---|---|---|---|
| `Read`, `Transaction(_)` | Allow | Allow | Allow |
| `Write`, `Ddl` | **Confirm** | **Block** | Allow |
| `Other` nằm trong danh sách trắng | Allow | Allow | Allow |
| `Other` khác | **Confirm** | **Block** | Allow |
| Explain thuần (mọi loại trừ `Ddl`) | Allow | Allow | Allow |
| Explain Analyze (chỉ `Read`) | **Confirm** | Allow | Allow |

Danh sách trắng cho `Other` được khớp theo từ khóa đầu và từ khóa thứ hai, cùng lexer với `classify`:

- **Chung:** `SAVEPOINT`, `RELEASE [SAVEPOINT]`, `ROLLBACK TO`, `PREPARE`, `DEALLOCATE`.
- **MySQL:** `USE`; `SET` chỉ ở phạm vi session. Không thuộc danh sách trắng: `SET GLOBAL`, `SET PERSIST`, `SET PERSIST_ONLY`, `SET @@global.`, `SET @@persist.`.
- **PostgreSQL:** `SET`, `RESET`, `DISCARD`, `LISTEN`, `UNLISTEN`, `DECLARE`, `FETCH`, `MOVE`, `CLOSE`.
- **Ngoại lệ trên connection read-only:** mọi câu gỡ chế độ read-only của session đều **Block**. Các câu này gồm `SET [SESSION] TRANSACTION … READ WRITE`, `SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE`, `SET transaction_read_only`/`tx_read_only`/`default_transaction_read_only` sang giá trị tắt, và `RESET`/`RESET ALL`/`DISCARD ALL` của PG.
- **Không bao giờ thuộc danh sách trắng:** MySQL `RESET`, `FLUSH`, `PURGE`, `KILL`, `LOCK TABLES`, `HANDLER`, `CHANGE MASTER/REPLICATION SOURCE`, `START/STOP SLAVE/REPLICA`; và mọi `Other` không có trong danh sách.

Read-only còn được server tự bảo vệ bằng `SET SESSION … READ ONLY` (P1 §4.5). Guard là lớp thứ hai, và cho thông báo rõ hơn.

### 5.2 Hộp xác nhận Production

- Hiện **một lần trước khi chạy**, liệt kê mọi câu có verdict `Confirm`. Mỗi dòng gồm số thứ tự, nhãn loại (`WRITE`, `DDL`, `OTHER`) và tối đa 120 ký tự đầu của câu. `Ddl` được tô đỏ.
- Tiêu đề: "Run N statements on **<tên connection>** (Production)?". Có viền đỏ theo màu tag.
- Hai nút: **Cancel** (mặc định có focus; `Esc` cũng là Cancel) và **Run N statements**. Cancel thì không chạy câu nào.
- Nếu có câu `Block` (read-only), hộp chặn hiện danh sách câu bị chặn và chỉ có nút Close. Không câu nào trong batch được chạy.

## 6. Streaming và lưới kết quả

### 6.1 `RunEvent` qua `Channel`

```ts
type RunEvent =
  | { t: "statement_start"; index: number; range: [number, number] }
  | { t: "result_start"; result_id: number; index: number; columns: ColumnMeta[] }
  | { t: "rows"; result_id: number; rows: Value[][] }
  | { t: "result_end"; result_id: number; affected: number | null; row_count: number;
      truncated: boolean; elapsed_ms: number }
  | { t: "statement_error"; index: number; error: AppError }   // position đã đổi sang offset trong editor
  | { t: "stopping"; reason: "user" | "timeout" }
  | { t: "abandoning" }                                           // ExecProgress::GraceExpired
  | { t: "tx_state"; state: "idle" | "active" | "aborted" }
  | { t: "done"; stop: StopReason | null; elapsed_ms: number };
```

- Lô `rows` dài tối đa `batch_size` (500). Backend gộp lô sao cho mỗi result có tối đa một event mỗi 50 ms, tránh làm ngập WebView.
- `Value` dùng quy tắc serialize của P1 §3.1.
- Vị trí lỗi:
  - PG có `position` (1-based, tính trong câu), đổi thành offset trong editor bằng `range.start`.
  - MySQL có "near '…' at line N", đổi thành dòng trong editor.

### 6.2 Frontend

- `store/queryRun.ts` (Zustand) theo từng tab. Trạng thái run: `idle → running(index/n, elapsed) → stopping(since) → abandoning → idle`.
  - Run và Explain bị khóa khi tab không ở `idle`.
  - Stop chỉ bật khi đang `running`.
- **Result tabs:** mỗi result set một tab ("Result 1", "Result 2", …), tối đa 50 tab mỗi lần chạy. Kết quả vượt quá chỉ có dòng tóm tắt trong Messages.
- **Messages:** mỗi câu một dòng gồm số thứ tự, dòng đầu của câu, rows/affected, thời gian và lỗi. Cuối danh sách là `StopReason` dưới dạng câu văn.
- **Lưới:**
  - Dùng `@glideapps/glide-data-grid`. Dòng được giữ theo khối 500 như khi nhận.
  - Header gồm tên cột và `db_type`.
  - Số căn phải; `NULL` hiện chữ xám nghiêng.
  - `Bytes` hiện dạng `0x…` kèm độ dài; JSON hiện trên một dòng.
  - Chọn ô hoặc vùng; `Ctrl+C` chép ra TSV.
- **Giới hạn dòng:** toolbar có ô chọn 1.000 / 10.000 / 100.000 / 1.000.000, lưu theo tab, gửi đi dưới dạng `max_rows`.
  - Khi `truncated`, lưới hiện dải "Showing first N rows" kèm nút **Fetch all**. Nút này chỉ có với câu `Read`: nó lập plan riêng cho câu đó (`Target::Selection(range)`), chạy với `max_rows = 1.000.000` và đi qua guard như thường.
  - Phần vượt giới hạn vẫn được driver đọc hết rồi bỏ, như hiện tại. Lý do không dừng sớm: MySQL phải đọc cạn luồng; PG muốn dừng sớm thì cần portal trong transaction hoặc gửi cancel, mà cancel thì kéo theo rủi ro tín hiệu lạc như M1.
  - Trên 100.000 dòng thì hiện cảnh báo về bộ nhớ.
- **Status bar:**
  - Hiện connection, phiên bản, số dòng và thời gian, như canvas.
  - "Auto-commit" được thay bằng trạng thái transaction: `Auto-commit` / `In transaction` (cam, có nút Commit và Rollback) / `Transaction aborted` (đỏ, có nút Rollback).
  - Khi `stopping`/`abandoning`, status bar hiện "Stopping… 3 s" hoặc "Server not responding — replacing connection… 12 s".

## 7. Editor, tab và autocomplete

### 7.1 Monaco

- Dùng `monaco-editor` cùng `@monaco-editor/react`. Monaco và worker được **đóng gói cục bộ** qua Vite (`loader.config({ monaco })`), không tải từ CDN, vì app phải chạy offline.
- Ngôn ngữ dùng `mysql` hoặc `pgsql` của Monaco, theo engine của tab.
- Phím tắt:
  - `Ctrl+Enter`: Run (câu tại con trỏ hoặc vùng chọn).
  - `Ctrl+Shift+Enter`: Run all.
  - `Ctrl+Shift+E`: Explain.
  - `Esc` khi đang chạy: Stop.
  - `Ctrl+S` / `Ctrl+O`: Save / Open.
  - `Ctrl+T`: New query.
- Câu mà `Ctrl+Enter` sẽ chạy được tô nền nhạt, lấy từ `statement_ranges`. Câu lỗi có marker đỏ ở vị trí lỗi; câu đã chạy xong có dấu ở gutter.
- Toolbar của tab giống canvas: Run, Run selection, Stop, Explain, cùng ô chọn Connection và Database. Cạnh đó thêm ô chọn giới hạn dòng và công tắc Continue on error.

### 7.2 Tab và file

- "New query" mở tab gắn với connection và database đang chọn trên cây. Tab mới tên "Query N".
- Bảng mới (migration 2):

```sql
CREATE TABLE query_tabs (
  id TEXT PRIMARY KEY, title TEXT NOT NULL, sort_order INTEGER NOT NULL,
  connection_id TEXT NULL REFERENCES connections(id) ON DELETE SET NULL,
  database TEXT NULL, sql_text TEXT NOT NULL, cursor INTEGER NOT NULL DEFAULT 0,
  file_path TEXT NULL, dirty INTEGER NOT NULL DEFAULT 0,
  continue_on_error INTEGER NOT NULL DEFAULT 0, row_limit INTEGER NOT NULL DEFAULT 10000,
  updated_at TEXT NOT NULL);
```

- **Tự lưu:**
  - Debounce 1 giây sau mỗi thay đổi, và lưu thêm khi đóng app.
  - Mở lại app thì khôi phục tab và nội dung nhưng **không tự kết nối**; kết nối chỉ mở khi chạy.
  - Tab gắn file mà có thay đổi chưa lưu ra file thì hiện `●`. Nội dung chưa lưu vẫn nằm trong `query_tabs`.
- **Ngoại lệ về secret (đã chốt):** nội dung editor là bản nháp của người dùng, không phải secret của app, nên được lưu nguyên văn. Lịch sử (§8.1) và log thì **luôn** redact.
- **Đóng tab** khi session đang có transaction mở: hỏi "Commit / Rollback / Cancel".
- **Đóng app:** một hộp liệt kê mọi tab đang có transaction mở, với các lựa chọn Rollback all & quit / Cancel.

### 7.3 Autocomplete

- **SchemaCache theo từng database:**
  - Thay một cache cho database mặc định bằng map `(database) → SchemaCache`, tải lười khi một tab chọn database đó.
  - `TableColumns` có thêm kiểu cột (`column_type` của MySQL, `format_type` của PG), vẫn trong một truy vấn duy nhất.
  - Sau khi một câu `Ddl` chạy thành công trong tab, cache của database đó được làm mới ở nền.
- **Provider** (TypeScript, `CompletionItemProvider` của Monaco) đọc token của câu hiện tại và gợi ý như sau:

| Ngữ cảnh | Gợi ý |
|---|---|
| Mặc định | Từ khóa và hàm dựng sẵn theo dialect (danh sách tĩnh trong TS), bảng/view của database đang chọn |
| Sau `FROM`/`JOIN`/`UPDATE`/`INTO`/`TABLE` | Bảng/view |
| `x.` với `x` là alias hoặc tên bảng trong `FROM`/`JOIN`/`UPDATE` của câu hiện tại | Cột của bảng đó |
| `x.` với `x` là database (MySQL) hoặc schema (PG) | Object trong đó, tải lười qua `completion_objects` (dùng cache duyệt cây) |
| Sau `SELECT`/`WHERE`/`ON`/`BY` | Cột của các bảng có trong câu, rồi từ khóa |

Alias được nhận theo mẫu `tên [AS] alias`, có hỗ trợ định danh trong quote. Provider không dựng cây cú pháp đầy đủ.

## 8. Lịch sử và Explain

### 8.1 Lịch sử

```sql
CREATE TABLE query_history (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
  database TEXT NULL, sql_text TEXT NOT NULL, redacted INTEGER NOT NULL,
  kind TEXT NOT NULL, started_at TEXT NOT NULL, elapsed_ms INTEGER NULL,
  status TEXT NOT NULL CHECK (status IN ('ok','error','cancelled')),
  row_count INTEGER NULL, affected INTEGER NULL,
  error_kind TEXT NULL, error_code TEXT NULL);
CREATE INDEX query_history_conn_time ON query_history(connection_id, started_at DESC);
```

- Mỗi câu **đã gửi tới server** có một dòng.
- Không lưu thông báo lỗi của server: thông báo đó có thể trích lại literal của câu. Chỉ lưu `error_kind`, mã lỗi và SQLSTATE.
- Giữ 10.000 dòng mới nhất. Phần cũ được xóa ngay sau mỗi lần ghi.
- **`query::redact(stmt, dialect) -> Redacted { text, changed }`** dùng lexer của `core`. Nó thay literal chuỗi đứng sau các mẫu dưới đây bằng `'[REDACTED]'`:
  - `IDENTIFIED BY`, `IDENTIFIED WITH … BY`, `IDENTIFIED VIA … USING`;
  - `PASSWORD`, kể cả `SET PASSWORD … =` và `PASSWORD(…)`;
  - `MASTER_PASSWORD`, `SOURCE_PASSWORD`;
  - `ENCRYPTED PASSWORD`, và `OPTIONS (… password '…')` của `CREATE/ALTER SERVER` và `USER MAPPING`;
  - `ENCRYPTION_KEY`.

  Mọi kiểu quote đều được nhận: `'…'`, `"…"`, `E'…'`, `$tag$…$tag$`.
- Log ở mức `debug` ghi SQL sau khi đã qua `redact`. Mức `info` vẫn không ghi SQL, như P1.
- **UI (tab History trong panel kết quả):**
  - Có ô tìm kiếm và bộ lọc theo connection (mặc định là connection của tab).
  - Mỗi dòng có thời điểm, trạng thái, thời gian chạy và dòng đầu của câu.
  - Double-click chèn câu vào editor tại con trỏ.
  - Câu đã redact có nhãn "redacted" và được chèn kèm `[REDACTED]`.
  - Có Delete và Clear.

### 8.2 Explain

- Explain thuần không thực thi câu. Nó áp cho câu tại con trỏ, với mọi loại trừ `Ddl`; với `Ddl` thì báo "Explain is not available for this statement".

| Engine | Explain | Explain Analyze (chỉ `Read`) |
|---|---|---|
| PG 12–17 | `EXPLAIN (FORMAT JSON)` → cây | `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` → cây có thời gian thực |
| MySQL 8 | `EXPLAIN FORMAT=JSON` → cây | ≥ 8.0.18: `EXPLAIN ANALYZE` → cây dạng text (`FORMAT=TREE`) |
| MariaDB | `EXPLAIN FORMAT=JSON` → cây | `ANALYZE FORMAT=JSON` → cây |
| MySQL 5.7 | `EXPLAIN` dạng bảng | Không hỗ trợ (`Unsupported`, nút bị khóa) |

- `query::explain` chuẩn hóa JSON hoặc text của từng engine thành `PlanNode { label, detail, est_cost, est_rows, actual_rows?, actual_ms?, loops?, children }`. Phần này test bằng fixture.
- UI tab Explain:
  - Cây thu gọn được.
  - Node có self cost cao nhất được tô cam. Self cost bằng tổng cost của node trừ cost của các node con.
  - Nút "Raw" hiện output gốc.
- Explain chạy trên session của tab (nên dùng đúng `search_path`, bảng tạm…), đi qua guard và có đủ trạng thái Stopping như một run thường.

## 9. Lỗi, dừng và các yêu cầu kế thừa

| Mục | Cách xử lý trong P2 |
|---|---|
| **I4** timeout trong transaction PG | `was_in_tx` trên `Timeout`/`Cancelled`; không retryable (§4.1); `tx_state = Aborted` (§4.2) |
| **M1** cancel lạc | Đọc cancel socket tới EOF; `late_completion` → `StopReason::LateCompletion`; không thấy EOF thì thay kết nối (§4.3) |
| **M3** `/*!40101 SET */` | `core::sql::session_effect` dùng chung (§4.4) |
| **M5** tách từ non-ASCII | `is_ident_byte` dùng chung (§4.4) |
| **M6** grace cố định | `ExecOptions.cancel_grace`; trạng thái Stopping/abandoning (§6.2) |
| `Other` trên Production | Xác nhận, trừ danh sách trắng (§5.1) |
| PG aborted-tx | `TxState::Aborted`, dừng script, nút Rollback (§4.2) |
| `NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES` | Cờ trong `Dialect::MySql`, `DialectChanged` (§4.6) |

**Trường hợp xấu nhất khi dừng:** timeout (hoặc bấm Stop) + `cancel_grace` 10 s + thay kết nối, tối đa bằng connect timeout 10 s. UI tiến qua các trạng thái sau:

1. `running`
2. `stopping`: "Stopping… N s"
3. `abandoning` (sau `GraceExpired`): "Server not responding — replacing connection… N s"
4. `idle`: Messages hiện lỗi cuối cùng, ví dụ `ConnectionLost` kèm ghi chú reset session.

Run bị khóa suốt quá trình. Không có nút "force", vì bỏ kết nối sớm hơn grace sẽ làm tăng khả năng kết quả là `outcome_unknown`.

**Hiển thị lỗi khi chạy:**

- Lỗi của câu: marker đỏ trong editor, dòng đỏ trong Messages, và tab Messages tự được chọn nếu không có result nào.
- `outcome_unknown`: Messages hiện dòng cảnh báo vàng "may have been applied". Không có nút Retry.
- `retryable = true`: Messages có nút **Retry statement**, chạy lại đúng câu đó qua `query_plan` với `Target::Selection`, và vẫn qua guard.
- `ConnectionLost`: thanh báo có nút Reconnect, như P1.
- `Cancelled` (do người dùng bấm): không coi là lỗi, chỉ ghi vào Messages.

**Cài đặt mới:** `options_json.query_timeout_s` (mặc định 0, tức là tắt), nằm trong tab Advanced của dialog connection.

## 10. Testing

1. **Unit, không cần DB:**
   - `query::plan`:
     - Target Cursor ở đầu câu, cuối câu, khoảng trắng giữa hai câu, trong comment, ở dòng cuối không có `;`.
     - Selection có `DELIMITER`.
     - Offset UTF-16 với tiếng Việt và emoji.
   - `query::guard`: ma trận đầy đủ của §5.1, gồm danh sách trắng theo dialect, `SET GLOBAL`/`SET PERSIST`, `/*! … */` và Explain Analyze.
   - `query::run` với `FakeSession` có kịch bản:
     - dừng khi lỗi, và Continue on error;
     - lỗi trong transaction luôn dừng;
     - `TxAborted`, rồi `ROLLBACK TO` vẫn được phép;
     - `LateCompletion`;
     - cancel giữa hai câu;
     - `ConnectionLost` và `outcome_unknown` luôn dừng;
     - `DialectChanged`.
   - `query::redact`: mọi mẫu ở §8.1, mọi kiểu quote, mật khẩu chứa `'`/`\`/`$`, câu không có mật khẩu giữ nguyên từng byte.
   - `query::explain`: fixture JSON/text thật của PG 12 và 17, MySQL 8.0 và 8.4, MariaDB 10.6 và 11.4, MySQL 5.7.
   - `core`:
     - Bảng chuyển `TxState` của §4.2.
     - `session_effect` với `/*!40101 SET … */`.
     - Lexer non-ASCII.
     - `Dialect` có `ansi_quotes`/`no_backslash_escapes`, test cả `split` lẫn `classify`.
     - `AppError.retryable` khi có `was_in_tx`.
2. **Integration** (Docker, `DNT_TEST_DB=1`), chạy qua mọi biến thể:
   - `execute_one` với `CALL` trả nhiều result set.
   - I4: timeout trong transaction cho ra `was_in_tx` và không retryable; PG chuyển sang `Aborted`.
   - PG: `SAVEPOINT` → lỗi → `ROLLBACK TO` đưa về `Active`; `COMMIT` khi `Aborted` đưa về `Idle`.
   - M1: lặp 200 lần một câu kết thúc sát timeout rồi chạy `SELECT 1`. Không được có `57014` lạc nào.
   - Thứ tự `ExecProgress` là `CancelSent → GraceExpired`, với `cancel_grace` ngắn.
   - `sql_mode` có `ANSI_QUOTES` → split đúng; `DialectChanged` xảy ra khi đổi `sql_mode` giữa script.
   - Connection read-only: guard chặn, và server cũng chặn khi gọi driver trực tiếp.
   - Sau khi chạy `CREATE USER … IDENTIFIED BY 'x'`, `app.db` và file log không chứa `x`.
3. **Frontend** (Vitest):
   - Máy trạng thái run.
   - Hộp xác nhận và hộp chặn.
   - Store kết quả nhận event theo lô, truncated, Fetch all.
   - Provider autocomplete với alias và tiền tố.
   - Khôi phục tab.
   - Hộp đóng tab/app khi có transaction mở.
4. **E2E smoke** (Windows):
   - Mở connection → New query → chạy `SELECT` → lưới có dữ liệu.
   - Gắn tag Production → chạy `DELETE` → hộp xác nhận hiện → Cancel → không có dòng nào bị xóa.
5. **Hiệu năng** (chạy tay, ghi số vào PR):
   - `SELECT` 1.000.000 dòng × 10 cột: dòng đầu hiện trong dưới 500 ms, cuộn mượt, ghi lại RAM của WebView.
   - Editor 10.000 dòng: gõ không giật.

## 11. Tiêu chí hoàn thành P2

- Chạy được câu tại con trỏ, vùng chọn và cả tab trên cả 6 server của ma trận test. Kết quả, Messages và vị trí lỗi đều đúng.
- Production: mọi câu `Write`/`Ddl`/`Other` ngoài danh sách trắng đều phải xác nhận trước, và Cancel thì không chạy gì. Read-only: các câu đó bị chặn ở cả UI lẫn backend. Gọi `query_run` thẳng mà không xác nhận thì bị từ chối.
- Stop dừng được `SLEEP`/`pg_sleep` trong dưới 1 giây. Câu bỏ qua cancel thì đi qua Stopping → abandoning và kết thúc trong khoảng grace cộng connect timeout.
- Các mục I4, M1, M3, M5, M6 và PG aborted-tx có test chứng minh, theo §10.
- Lưới 1.000.000 dòng dùng được. Fetch all chỉ có với câu `Read`.
- Autocomplete gợi ý đúng bảng, cột và alias theo database của tab.
- Tab khôi phục được sau khi khởi động lại; Open/Save `.sql` hoạt động.
- Lịch sử không chứa literal mật khẩu (kiểm tra `app.db` và log).
- Explain hiện cây trên PG, MySQL 8 và MariaDB; bảng trên MySQL 5.7.
- Test tầng 1–3 xanh; E2E smoke xanh trên Windows.
