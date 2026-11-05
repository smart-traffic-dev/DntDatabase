# Gợi ý AI trong editor (FIM) — Design Spec

- Ngày: 2026-11-04
- Trạng thái: Đề xuất để review, chưa triển khai.
- Quan hệ với spec AI agent (`2026-10-21-ai-agent-design.md`): làm **trước** agent. Phần nền (Settings → AI, API key,
  opt-in theo connection, client HTTP) làm ở đây và agent dùng lại. Gợi ý chỉ chèn chữ vào editor, không chạy gì,
  nên không phụ thuộc R4a / R4b, ToolHost hay luồng duyệt.

## 1. Mục tiêu và những gì đã chốt

Ghost text kiểu VS Code trong query tab: người dùng gõ SQL, app gợi ý phần tiếp theo (một đoạn trong dòng hoặc vài
dòng), `Tab` để nhận.

Đã chốt:

1. **Chỉ hỗ trợ model FIM** (fill-in-the-middle): endpoint OpenAI-compatible `POST {base_url}/completions` có tham số
   `suffix`. Không giả lập FIM bằng model chat. Model chat để dành cho agent.
2. **Chỉ query tab SQL** (MySQL, PostgreSQL). Mongo / Redis sau R4b.
3. **Tắt tới khi người dùng đồng ý** (một lần, với host của endpoint); sau đó bật cho mọi connection SQL trừ
   Production, tắt / bật riêng từng connection được (§6.3).
4. Không gửi dữ liệu dòng. Chỉ gửi text editor (đã redact) và tên schema (bảng, cột, kiểu).

Không bao giờ: API key đi qua IPC (trừ lúc người dùng nhập để lưu), prompt ghi vào log, tự chạy SQL do AI gợi ý.

## 2. Model và endpoint

- Request: `POST {base_url}/completions`, body
  `{ model, prompt, suffix, max_tokens, temperature: 0.2, stop, stream: false }`, header `Authorization: Bearer <key>`
  khi có key (server local thường không cần key).
- Không stream: gợi ý ngắn (mặc định `max_tokens` = 128), nhận một lần cho đơn giản và dễ huỷ.
- **Chỉ một chuẩn: OpenAI-compatible.** Người dùng tự cấu hình provider (base URL), model và key; app không có code
  riêng cho provider nào, không có API native của bất kỳ server nào.
- Provider / model nào nhận `suffix` ở `/completions` thì dùng được (thường là model code như Qwen2.5-Coder,
  DeepSeek-Coder, StarCoder2 trên server OpenAI-compatible). Việc chọn provider phù hợp là của người dùng.
- Endpoint không hỗ trợ `suffix` thường trả 400 hoặc lờ đi tham số. Nút **Test** phát hiện cả hai (§6.2).
- Provider có đường FIM khác chuẩn (ví dụ `/fim/completions`): ngoài phạm vi.

## 3. Kiến trúc

```
ui/  ── query/ai/inlineProvider.ts  Monaco InlineCompletionsProvider (mysql, pgsql)
 │        gửi: ai_complete(req) với request_id · huỷ: ai_cancel(request_id)
 ▼  IPC
src-tauri/
 ├─ commands/ai.rs        ai_settings_get/set, ai_key_set/clear/has, ai_test, ai_complete, ai_cancel,
 │                        ai_consent_set, ai_connections, ai_connection_set
 ├─ ai_runtime.rs         AiManager: client dùng chung, bảng request đang chạy (huỷ), trạng thái tạm dừng
 │                        sau lỗi 401/429; dựng ngữ cảnh schema từ SchemaCache
crates/
 ├─ ai/   (MỚI, thuần: test không cần DB; HTTP qua trait để test không cần mạng)
 │   ├─ provider.rs   trait FimClient + OpenAiFimClient (reqwest); bản giả cho test
 │   ├─ prompt.rs     dựng prompt / suffix: cắt theo byte, header ngữ cảnh schema
 │   └─ post.rs       hậu xử lý gợi ý (§5.3)
 └─ store/   + migration: ai_consent, ai_connection; key settings trong app_settings
```

- `crates/ai` sau này chứa thêm client chat của agent. Spec agent đổi `crates/agent/provider.rs` thành dùng
  `crates/ai` (sửa spec agent khi viết plan agent).
- `reqwest` hiện chỉ là dependency gián tiếp; thêm trực tiếp vào `crates/ai` (rustls, không native-tls, giống phần
  còn lại của app).
- Backend gọi model, không phải UI: key không lên webview, và CSP của webview không phải mở thêm origin.

## 4. Ngữ cảnh gửi cho model

### 4.1 Text editor

- `prefix` = text trước con trỏ, `suffix` = text sau con trỏ, lấy từ toàn bộ document của tab.
- **Redact trước khi cắt:** tách document thành câu (`statement_ranges`), chạy redact từng câu (dialect của
  connection), giữ bảng ánh xạ offset. Con trỏ nằm trong một đoạn bị redact (ví dụ đang gõ password trong
  `IDENTIFIED BY '...'`, kể cả chuỗi chưa đóng) → không gợi ý. `redact` đã tính span bên trong nhưng chỉ trả text;
  F1 thêm `redact_spans(stmt, dialect) -> Vec<(usize, usize)>` dùng chung phần quét đó.
- Cắt: `prefix` tối đa 6 KB, lấy ngược từ con trỏ, cắt ở ranh giới câu lệnh (`statement_ranges`) khi được;
  `suffix` tối đa 2 KB, cắt ở ranh giới câu lệnh sau câu hiện tại. Cắt ở ranh giới UTF-8 hợp lệ.

### 4.2 Header ngữ cảnh

`prompt` = header + `prefix`. Header là comment SQL để model FIM hiểu như một phần file:

```sql
-- Dialect: PostgreSQL 16
-- Database: shop, schema: public
-- Tables:
--   orders(id bigint, customer_id bigint, status text, created_at timestamptz)
--   customers(id bigint, email text, name text)
-- Other tables: invoices, products, refunds
```

`SchemaCache` chỉ có tên và kiểu cột (`ColumnBrief`), không có PK / FK: header không ghi khoá.

- Bảng đưa vào: (1) bảng nhắc tới trong câu hiện tại (khớp tên với `SchemaCache`, có tính schema / alias);
  (2) nếu còn chỗ, các bảng khác của database theo thứ tự tên, chỉ tên không cột. Tối đa 3 KB.
- Lấy từ `SchemaCache` (đã có cho completion), không hỏi server khi gõ. Cache chưa có → header chỉ có dialect và
  database; nạp cache ở nền như completion hiện tại.
- Không có comment cột / bảng (có thể chứa dữ liệu nhạy cảm và tốn token). Không có dữ liệu dòng, row estimate.

## 5. Luồng gợi ý

### 5.1 Khi nào gọi

- Setting `trigger`: `auto` (mặc định) / `manual` / `off`.
  - `auto`: sau 350 ms ngừng gõ. `manual`: chỉ khi bấm `Alt+\`. `Alt+\` dùng được ở cả hai chế độ.
- Không gọi khi: chưa đồng ý với endpoint hoặc connection đang tắt AI, đang có vùng chọn, popup completion hiện tại đang mở, nhiều con trỏ,
  con trỏ nằm giữa một từ (ký tự ngay sau là chữ / số / `_`), trong đoạn bị redact, đang tạm dừng vì lỗi (§7).
- Không có connection hoặc database → không gợi ý.

### 5.2 Huỷ và cache

- Gõ tiếp, di chuyển con trỏ hay đổi tab: Monaco huỷ token → UI gọi `ai_cancel(request_id)` → backend drop future
  reqwest (đóng kết nối HTTP).
- Tối đa 1 request đang chạy mỗi editor. Timeout 5 s (setting), quá hạn thì bỏ, không báo lỗi.
- Cache LRU 50 mục ở UI theo `(conn, db, hash(prefix), hash(suffix))`.
- Gõ đúng các ký tự đầu của ghost text đang hiện → ghost text co lại, không gọi lại (hành vi sẵn có của
  `InlineCompletionsProvider` khi trả range đúng).

### 5.3 Hậu xử lý (`post.rs`)

Theo thứ tự:

1. Bỏ nếu rỗng hoặc chỉ có khoảng trắng.
2. Cắt ở dòng trống đầu tiên và ở token đặc biệt FIM lọt ra (`<|fim_…|>`, `<fim_…>`, `<|endoftext|>`, `<EOT>`).
3. Tối đa 8 dòng (setting), cắt ở cuối dòng.
4. Bỏ phần cuối trùng với đầu `suffix` (model hay lặp lại `)` hoặc `;` đã có sau con trỏ).
5. Gợi ý nhiều câu lệnh: chỉ giữ đến hết câu đầu tiên (`;` đầu tiên ngoài chuỗi / comment, theo splitter của
   dialect).
6. Bỏ nếu sau các bước trên trùng y hệt đoạn ngay trước con trỏ (model lặp prefix).

`stop` gửi lên chỉ `["\n\n"]`: chuẩn OpenAI cho tối đa 4 mục, và stop sequence bị bỏ khỏi output (`";\n"` sẽ làm
mất dấu `;`). Token FIM và câu thứ hai do bước 2 và 5 xử lý.

### 5.4 Nhận gợi ý

- `Tab`: nhận cả gợi ý. `Ctrl+→`: nhận từng từ (`acceptNextWord` của Monaco). `Esc`: bỏ.
- `Tab` chỉ thuộc về ghost text khi ghost text đang hiện; còn lại giữ hành vi cũ (thụt lề, snippet).
- Text nhận vào là text thường: người dùng vẫn Run qua plan → guard → History như mọi câu khác.

## 6. Settings và opt-in

### 6.1 Settings → AI

| Mục | Ghi chú |
|---|---|
| Base URL | endpoint OpenAI-compatible, ví dụ `https://api.example.com/v1`; bắt buộc `http`/`https` |
| API key | Credential Manager, `dnt.ai.default`; ô nhập hiện `••••` khi đã có; Clear |
| Model | nhập tay; nút lấy danh sách qua `GET /models` khi endpoint hỗ trợ |
| Editor suggestions | `Automatic` / `Manual (Alt+\)` / `Off` |
| Max tokens, timeout, max lines | 128 / 5 s / 8 |
| Test | §6.2 |
| Sharing | đồng ý / thôi đồng ý với host (§6.3); danh sách tick các connection SQL |

- Lưu trong `app_settings` (key `ai.base_url`, `ai.model`, …), không có key.
- `SecretStore` hiện chỉ có khoá theo `(conn, SecretKind)`. Thêm API cho secret cấp app (ví dụ
  `get_app` / `set_app` / `delete_app` với tên cố định) thay vì giả một `conn` id.
- Ghi chú dưới mục Model: "Requires a code model with fill-in-the-middle support (for example Qwen2.5-Coder,
  DeepSeek-Coder). Chat models are not supported."

### 6.2 Test

Gửi một FIM nhỏ: `prompt = "SELECT id, name\nFROM users\nWHERE "`, `suffix = "\nORDER BY id;"`.

- HTTP lỗi → báo đúng loại (§7).
- 400 nhắc tới `suffix` → "This endpoint does not support fill-in-the-middle."
- 200 nhưng gợi ý lặp lại `ORDER BY` hoặc chứa nội dung sau `suffix` → cảnh báo "The model may be ignoring the
  suffix. Use a code model with FIM support." (không chặn lưu).
- OK → hiện gợi ý và thời gian phản hồi.

### 6.3 Đồng ý một lần, tắt theo connection

(Sửa 2026-11-05: bản đầu bắt đồng ý riêng từng connection, rối khi dùng: nhiều chỗ bật, đổi host phải đồng ý lại
từng cái, cấu hình xong vẫn không thấy gợi ý.)

- **Đồng ý một lần cho app**, gắn với origin của base URL: Settings → AI → Sharing ("Turn on AI…"), tự hỏi ngay
  khi Save một endpoint mới chưa được đồng ý; hoặc từ status bar của query tab ("AI off · Turn on AI…").
- Dialog đồng ý nói rõ: text trong editor (đã ẩn mật khẩu) và tên bảng, cột sẽ được gửi tới `<host của base URL>`;
  không gửi dữ liệu dòng; AI bật cho mọi connection MySQL / PostgreSQL trừ connection gắn tag Production. Nút
  `Turn on AI` chỉ bật khi tick "I am allowed to share these databases' schemas and queries with this endpoint";
  Enter = Cancel.
- **Mỗi connection có thể có lựa chọn riêng** (bật / tắt), không cần dialog: status bar, chip "AI suggestions" trong
  dialog connection (áp dụng khi Save, chỉ hiện khi đã đồng ý), danh sách tick trong Settings → AI. Không có lựa chọn
  riêng → bật, trừ Production (tắt). Đổi tag trong dialog thì chip đổi theo ngay.
- Bảng `ai_consent(scope, origin, granted_at)` và `ai_connection(connection_id, scope, enabled)`, `scope = 'editor'`.
  Agent sau này thêm `scope = 'agent'` (gửi dữ liệu dòng) với dialog đồng ý riêng.
- Đổi base URL sang origin khác → đồng ý cũ mất hiệu lực, phải đồng ý lại một lần; lựa chọn của từng connection giữ
  nguyên. Cùng origin, khác path → vẫn hiệu lực.
- Migration từ `ai_opt_in`: connection đã opt-in thành lựa chọn "bật"; đồng ý cấp app không được suy ra (người dùng
  đồng ý lại một lần).
- Backend kiểm tra cả hai trước khi gửi: đồng ý với origin hiện tại, và connection đang bật.
- Không đi theo `.dntconn` / `.ncx`; xoá connection xoá lựa chọn của nó.
- Production và read-only vẫn bật được (tick riêng): gợi ý không chạy gì. Badge env vẫn hiện như thường.

## 7. Lỗi

| Tình huống | Hành vi |
|---|---|
| Mất mạng, timeout, 5xx | Bỏ gợi ý đó, không báo. 3 lần liên tiếp → chấm đỏ ở chỉ báo AI, tooltip nêu lỗi |
| 401 / 403 | Tạm dừng gợi ý tự động đến khi đổi settings; toast một lần "AI: the API key was rejected" + link Settings |
| 429 | Tạm dừng theo `Retry-After` (mặc định 30 s), không báo |
| 400 về `suffix` / model không có | Tạm dừng; toast một lần với thông báo từ server |
| JSON hỏng / không có `choices` | Bỏ gợi ý; ghi log `warn` (không có prompt) |

Status bar của query tab có chỉ báo `AI` nhỏ: xám (tắt cho connection), bình thường, đang chạy (chấm động), lỗi
(chấm đỏ). Bấm vào mở menu: bật / tắt cho connection, đổi trigger, mở Settings → AI.

## 8. Log và bảo mật

- Không log prompt, suffix, gợi ý ở bất kỳ level nào. `debug` chỉ ghi độ dài, thời gian, mã HTTP, model.
- API key chỉ đọc ở backend, ngay trước request; không nằm trong struct `Debug`.
- Không theo redirect (`reqwest` redirect policy `none`), để key không đi nơi khác. 3xx báo lỗi kèm host đích, người
  dùng sửa base URL.
- Không có telemetry.

## 9. Kiểm chứng

1. `crates/ai` unit: dựng prompt (cắt byte, UTF-8, ranh giới câu), header schema (khớp bảng có schema / alias,
   giới hạn 3 KB), từng bước hậu xử lý §5.3.
2. Provider với HTTP server giả trong test (tokio): body đúng, header key có / không, timeout, huỷ đóng kết nối,
   401 / 429 / 400, fixture response theo chuẩn OpenAI `/completions` (có và không có `usage`, `finish_reason`
   khác nhau, `choices` rỗng).
3. Redact: con trỏ trong span bị redact → không gọi; text gửi đi không chứa password.
4. Store: migration `ai_opt_in`, đổi origin làm opt-in mất hiệu lực, xoá connection xoá opt-in, `.dntconn` không mang
   opt-in, secret cấp app.
5. UI (vitest): provider không gọi khi chưa opt-in / có vùng chọn / popup completion mở / giữa từ; huỷ khi gõ tiếp;
   cache; `Tab` giữ hành vi cũ khi không có ghost text; dialog đồng ý (Enter = Cancel, nút bị khoá khi chưa tick).
6. e2e-web: backend giả trả gợi ý cố định; gõ → thấy ghost text → `Tab` → text nằm trong editor.

## 10. Ngoài phạm vi v1

Model chat, `Ctrl+K` (viết / sửa SQL theo yêu cầu bằng lời — cần model chat, làm cùng hoặc sau agent), Mongo /
Redis, API riêng của provider (ngoài chuẩn OpenAI), stream gợi ý, nhiều gợi ý để chọn, gợi ý trong data filter / designer, nhiều profile model.

## 11. Thứ tự

1. **F1** — `crates/ai` (provider, prompt, post) + `redact_spans`.
2. **F2** — `src-tauri`: settings, secret cấp app, migration `ai_opt_in`, `AiManager`, commands; Settings → AI với
   Test; dialog đồng ý.
3. **F3** — `ui`: `InlineCompletionsProvider`, phím, chỉ báo status bar, e2e-web.

Mỗi giai đoạn một plan, kết thúc với toàn bộ test pass.
