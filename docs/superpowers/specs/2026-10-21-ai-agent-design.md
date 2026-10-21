# AI agent trong DntDatabase — Design Spec

- Ngày: 2026-10-21
- Trạng thái: Đề xuất để review, chưa triển khai.
- Thay đổi so với roadmap P3–P6: roadmap ghi "tính năng AI" ngoài phạm vi. Spec này đưa AI vào thành một
  giai đoạn riêng (P7), dùng lại guard, plan và History hiện có. Không đổi policy chạy SQL tự do của query tab.
- Mockup: canvas "DntDatabase UX redesign", hàng "AI agent" (board 17–19).

## 1. Mục tiêu và những gì đã chốt

Một agent harness chạy trong app, chuyên cho database: người dùng hỏi bằng ngôn ngữ tự nhiên, agent tự gọi
tool (đọc schema, chạy SELECT, explain, đề xuất hoặc chạy câu ghi), trong giới hạn policy.

Đã chốt với người dùng:

1. **Model do người dùng cấu hình**, chuẩn OpenAI-compatible: base URL, API key, tên model. Dùng được cả cloud
   lẫn local (Ollama, LM Studio, vLLM…). App không gửi gì đi đâu ngoài endpoint người dùng nhập.
2. **Opt-in theo connection.** AI tắt mặc định trên mọi connection. Connection chưa opt-in thì agent không thấy,
   kể cả tên.
3. **Khi đã opt-in, agent có toàn quyền đọc** trên connection đó (metadata, SELECT, EXPLAIN, DDL). Chỉ có giới
   hạn kỹ thuật về số dòng / kích thước để vừa context.
4. **Quyền ghi theo môi trường:** Local tự chạy; Staging hỏi từng câu, người dùng duyệt; Production chỉ đề xuất.
   Connection chưa gắn tag xử lý như Staging.
5. **Kiến trúc A:** agent loop ở backend Rust, UI chỉ hiển thị và gửi quyết định duyệt.

Không bao giờ: agent có quyền cao hơn người dùng; frontend vượt policy; API key đi qua IPC.

## 2. Kiến trúc

```
ui/  ── AgentPanel (chat, thẻ tool call, Approve / Reject / Stop)
 │      gửi: message, decide(approval_id, approve|reject), stop · nhận: stream event
 ▼  IPC (Channel cho mỗi lượt chạy, giống run_events)
src-tauri/
 ├─ commands/agent.rs     agent_send, agent_decide, agent_stop, agent_sessions_*, agent_settings_*
 ├─ agent_runtime.rs      AgentManager: phiên đang chạy, hàng đợi duyệt, cancel;
 │                        cài ToolHost bằng các *_impl có sẵn (browse.rs, query.rs, …)
crates/
 ├─ agent/   (MỚI, thuần: test không cần DB, không cần mạng)
 │   ├─ provider.rs  client OpenAI-compatible: POST /chat/completions, stream SSE, tool_calls;
 │   │               trait LlmClient (có bản giả cho test)
 │   ├─ tools.rs     catalog: tên, JSON schema tham số, mô tả
 │   ├─ policy.rs    decide(...) → Allow | Ask | Propose | Deny
 │   ├─ loop.rs      model → tool calls → policy → kết quả → model …
 │   └─ context.rs   cắt kết quả theo dòng/byte, tóm tắt schema
 └─ store/   + migration: agent_settings, agent_sessions, agent_messages, ai_opt_in theo connection
```

- `crates/agent` không biết Tauri hay driver. Nó gọi trait `ToolHost`; `src-tauri` cài đặt host thật. Sau này
  bọc thành MCP server chỉ cần một host khác, không viết lại tool hay policy.
- Tool gọi đúng các hàm `*_impl` hiện có (`list_objects_impl`, `table_detail_impl`, `object_ddl_impl`,
  `query_plan_impl`, `query_run_impl`, `query_explain_impl`). Không có đường chạy SQL thứ hai: guard và History
  dùng chung.
- Mỗi phiên chat có **session DB riêng** (không dùng session của query tab hay data edit), chạy auto-commit, áp
  dụng SSH/TLS, timeout và chính sách connection như thường.
- **Một phiên chat gắn với đúng một connection.** Đổi database/schema trong phiên được; đổi connection thì mở
  phiên mới.
- API key lưu trong Windows Credential Manager (cùng cơ chế password connection), chỉ backend đọc.

## 3. Danh mục tool v1

Mọi tool nhận `database`/`schema` tường minh.

| Nhóm | Tool | Hành vi |
|---|---|---|
| Schema | `list_namespaces()` | database (MySQL) hoặc database + schema (PG) |
| | `list_objects(scope, kind?, filter?)` | tên, loại, row estimate, size |
| | `describe_table(scope, name)` | cột (kiểu, null, default, comment), PK, index, FK vào/ra |
| | `get_ddl(scope, name, kind)` | DDL bảng, view, routine, trigger |
| | `search_schema(text)` | tên bảng/cột khớp, tìm trong SchemaCache (không hỏi server) |
| Đọc | `run_select(sql, max_rows?)` | chỉ nhận câu `classify` = `Read`; mặc định 200 dòng, tối đa 1 000; ô > 2 KB bị cắt; tổng ~32 KB; luôn báo "đã cắt, tổng N dòng"; timeout 30 s, Stop huỷ phía server |
| | `explain(sql, analyze=false)` | `analyze=true` chạy thật câu lệnh nên theo verdict guard (Production → Ask) |
| | `server_activity()` | process list khi P6 có; chỉ đọc |
| Ghi | `execute_write(sql, reason)` | `query_plan` + `guard` như query tab, rồi policy §4 |
| | `propose_sql(sql, title, reason)` | mở query tab mới chứa SQL, không chạy; dùng được mọi connection |
| Workspace | `read_attached_context()` | chỉ những gì người dùng đính kèm: query đang mở, vùng chọn, lỗi vừa gặp, dòng đã chọn |
| | `open_object(scope, name, subtab?)` | mở tab cho người dùng xem; không trả dữ liệu cho model |

Không có tool nào đọc password/secret, đổi setting, đổi env tag / read-only, đọc file máy, hay gọi mạng khác.

Để sau v1: `propose_table_change` (Table designer, P4b), `propose_import` (P5), so sánh schema, ER.

## 4. Policy và luồng duyệt

### 4.1 Hàm quyết định

`policy::decide(opt_in, env, read_only, tool, verdicts)` đọc cấu hình connection **mới nhất ở mỗi lần gọi tool**
(đổi env tag hay read-only giữa phiên có hiệu lực ngay).

| Connection | Tool đọc | `execute_write` |
|---|---|---|
| Chưa opt-in | Deny (connection vô hình) | Deny |
| Read-only (mọi tag) | Allow | Deny |
| Local | Allow | guard Allow → **Allow** (tự chạy); guard Confirm → **Ask** |
| Staging, chưa gắn tag | Allow | **Ask** |
| Production | Allow (trừ `explain analyze` → Ask) | **Propose** (chuyển thành `propose_sql`) |

Mọi verdict guard `Block` → Deny. Policy agent chỉ chặn thêm, không bao giờ nới guard.

Một lời gọi `execute_write` chứa nhiều câu: quyết định là mức chặt nhất trong các câu; duyệt là duyệt cả plan.

### 4.2 Ask

- Backend dựng plan bằng `query_plan` và giữ `plan_id`. Thẻ duyệt hiển thị SQL đúng như sẽ gửi, verdict guard,
  cảnh báo (không WHERE, DROP, số dòng ước tính khi có EXPLAIN), connection + env.
- Approve gửi `approval_id`; backend chạy đúng `plan_id` đó. Model không sửa được SQL sau khi đã đưa ra duyệt.
- Plan hết hạn hoặc cấu hình connection đổi → stale: không chạy, agent phải gọi lại, người dùng duyệt lại.
- Reject: kết quả trả cho model là "người dùng từ chối" kèm ghi chú (nếu có); model không được hỏi lại cùng
  câu trong lượt đó.

### 4.3 Propose (Production)

Mở query tab mới với SQL + comment đầu tab (`-- Đề xuất bởi AI: <reason>`). Người dùng chạy theo luồng thường
(guard, gõ tên). Kết quả trả cho model: "đã mở để người dùng tự chạy", không có kết quả chạy.

### 4.4 Giới hạn mỗi lượt

- Tối đa 25 tool call / lượt (setting). Hết thì dừng và hỏi người dùng có tiếp tục không.
- Local: tối đa 10 câu ghi tự chạy / lượt, sau đó chuyển sang Ask.
- Stop luôn dừng được: huỷ request model đang stream và huỷ câu SQL đang chạy phía server.

### 4.5 Opt-in và đồng ý

- Bật AI cho một connection mở dialog đồng ý: nói rõ dữ liệu dòng sẽ được gửi tới `<host của base URL>`, mức
  quyền ghi theo env của connection.
- Đồng ý gắn với origin của endpoint. Đổi base URL sang origin khác → mọi connection đã opt-in phải đồng ý lại.
- Cờ opt-in không đi theo file export connection (`.dntconn`); import về luôn tắt.

### 4.6 Dữ liệu không tin cậy

Kết quả tool (dữ liệu dòng, comment, DDL) là dữ liệu, có thể chứa chữ nhắm vào model. Prompt hệ thống nói rõ
điều đó, nhưng an toàn không dựa vào prompt: mọi quyết định ở §4.1 nằm ở backend, nên nội dung trong DB không thể
nâng quyền của agent.

## 5. UI

- **Agent panel** neo bên phải workspace (`Ctrl+I`), kéo rộng 320–640 px, nhớ trạng thái.
  - Header: tên connection, badge env, chip chế độ ("Local · tự chạy", "Staging · hỏi từng câu",
    "Production · chỉ đề xuất", "Read-only · chỉ đọc"), chọn phiên, phiên mới.
  - Tin nhắn; thẻ tool call thu gọn được (tên tool, tóm tắt tham số, thời gian, số dòng, "đã cắt").
  - Thẻ duyệt (Ask), thẻ đề xuất (Propose), thẻ bị chặn (Deny, kèm lý do).
  - Composer: chip ngữ cảnh đính kèm (Query, Vùng chọn, Lỗi, N dòng), tên model, Send / Stop.
- Lối vào khác: "Hỏi AI" trên lỗi trong Messages, "Giải thích bằng AI" ở Explain view, "Hỏi AI về bảng này"
  trong context menu tree. Tất cả mở panel với chip ngữ cảnh tương ứng.
- Connection chưa opt-in: panel hiện empty state "AI đang tắt cho connection này" + nút "Bật AI…".
- Status bar có chỉ báo khi agent đang chạy hoặc đang chờ duyệt.
- **Settings → AI**: base URL, API key, model (lấy danh sách qua `/models` khi endpoint hỗ trợ, hoặc nhập tay),
  max tokens, nút Test (gọi `/models` và một tool call nhỏ). Danh sách connection đã opt-in.

## 6. Lưu trữ và History

- `agent_settings`: base URL, model, giới hạn (không có key). Key trong Credential Manager (`dnt.ai.default`).
- `agent_sessions` (id, connection_id, title, model, created/updated) và `agent_messages` (role, nội dung,
  tool_calls, kết quả tool **đúng như đã gửi cho model**, đã cắt).
- SQL agent chạy được ghi vào History với origin `agent` + session id, qua cơ chế redaction hiện có.
- Người dùng xoá được từng phiên hoặc tất cả. Xoá connection xoá luôn các phiên của nó.

## 7. Lỗi

| Tình huống | Hành vi |
|---|---|
| Endpoint 401/403 | Dừng lượt, panel báo "API key không hợp lệ", link Settings → AI |
| 429 | Dừng, hiện thời gian chờ nếu có `Retry-After`; không tự retry |
| 5xx, mất mạng giữa stream | Dừng lượt, giữ phần đã nhận, nút "Thử lại lượt này" |
| Model không hỗ trợ tool calling | Báo khi Test hoặc ở lượt đầu; không giả lập tool qua prompt |
| Tham số tool sai / JSON hỏng | Trả lỗi cho model; 2 lần liên tiếp thì dừng lượt |
| Lỗi DB | Trả nguyên thông báo server cho model (đã redact) |
| Câu ghi `outcome_unknown` | Dừng lượt, báo người dùng refresh; không retry |
| Vượt giới hạn tool call | Dừng, hỏi người dùng tiếp tục hay không |

Không bao giờ tự retry một câu ghi.

## 8. Kiểm chứng

1. Unit: bảng §4.1 đầy đủ (opt-in × env × read-only × tool × verdict), kể cả đổi cấu hình giữa phiên.
2. Loop với `LlmClient` giả theo kịch bản: nhiều tool call, Ask/Approve/Reject, stale, Stop, giới hạn lượt.
3. Parser provider: fixture SSE thật từ OpenAI, Ollama, LM Studio, vLLM (tool_calls dạng delta).
4. Context: cắt dòng/byte/ô, thông điệp "đã cắt" chính xác.
5. Integration `DNT_TEST_DB=1`: `run_select` từ chối câu ghi; Local tự chạy; Staging chờ duyệt; Production chỉ mở
   tab; read-only Deny; History có origin `agent`.
6. UI: thẻ tool, thẻ duyệt (SQL hiển thị = SQL chạy), empty state chưa opt-in, dialog đồng ý, Settings Test.
7. e2e-web: backend giả phát stream event của agent.

## 9. Ngoài phạm vi v1

MCP server (sau, dùng chung `crates/agent`), một phiên nhiều connection, RAG/embedding, ảnh, bộ nhớ dài hạn tự
động, agent chạy nền theo lịch, nhiều profile model cùng lúc.

## 10. Thứ tự

Phụ thuộc: guard, query plan/run, History (đã có). Có thể bắt đầu sau P4a. `propose_table_change` và
`propose_import` chờ P4b / P5.

1. `crates/agent`: provider + policy + loop với test giả.
2. `src-tauri`: ToolHost, AgentManager, commands, store migration, Credential Manager.
3. UI: Settings → AI, opt-in + dialog đồng ý, Agent panel, các lối vào.
