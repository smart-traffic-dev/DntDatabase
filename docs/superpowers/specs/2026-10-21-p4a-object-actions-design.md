# P4a — Quản lý object cơ bản, local-first

- Ngày: 2026-10-21
- Trạng thái: Đã duyệt và triển khai trên branch feat/p4a-object-actions.
- Cơ sở: code hiện tại đã có query guard, data edit plan, transaction, introspection và streaming export.
- Tài liệu này thu hẹp phần P4 của roadmap P3–P6 thành một đợt có thể kiểm chứng độc lập.

## 1. Mục tiêu

Người dùng tạo namespace, đổi tên bảng, xóa object và làm rỗng bảng từ connection tree mà không phải tự viết SQL. Mọi thao tác đều có preview SQL thực tế, kiểm tra backend và kết quả rõ ràng.

Local-first: toàn bộ lưu trữ, thực thi và kiểm chứng chạy trên máy người dùng. Không thêm CI, cloud, telemetry hay dịch vụ bên ngoài.

## 2. Các phương án và lựa chọn

| Phương án | Lợi ích | Chi phí / giới hạn |
|---|---|---|
| P4a object actions, sau đó P4b designer — đề xuất | Có giá trị sử dụng sớm; xây nền thực thi DDL trước khi làm diff | Chưa tạo/sửa bảng bằng form trong P4a |
| Làm cả P4 và designer ngay | Bao phủ quản lý cấu trúc rộng | Metadata hiện có chưa đủ để bảo toàn mọi thuộc tính; phạm vi lớn |
| Workspace trước | Cải thiện mở lại phiên làm việc | Chưa bổ sung thao tác quản lý DB |

Chọn phương án đầu. Workspace đầy đủ là một spec độc lập; không ghép vào P4a.

## 3. Phạm vi v1

| Thao tác | MySQL / MariaDB | PostgreSQL |
|---|---|---|
| Tạo namespace | CREATE DATABASE, dùng mặc định server | CREATE SCHEMA trong database đang chọn |
| Xóa namespace | DROP DATABASE | DROP SCHEMA, RESTRICT |
| Đổi tên bảng | Trong cùng database | Trong cùng schema |
| Xóa bảng | DROP TABLE | DROP TABLE, RESTRICT |
| Làm rỗng bảng | TRUNCATE TABLE | TRUNCATE TABLE, RESTRICT, CONTINUE IDENTITY |
| Xóa view | Có | Có, RESTRICT |
| Xóa materialized view | Không áp dụng | Có, RESTRICT |

Không tự thêm CASCADE, không tắt foreign key checks. Nếu server từ chối vì dependency hoặc quyền, trả lỗi có ngữ cảnh và giữ nguyên lựa chọn người dùng.

Ngoài phạm vi: tạo/xóa PostgreSQL database; routine/trigger/event/sequence/type; duplicate bảng; designer; sửa DDL tự do; import/dump/restore; schema diff; ER diagram; chỉnh quyền; driver mới.

Routine cần định danh overload chính xác trên PostgreSQL; không dùng tên hiển thị hiện tại để sinh DROP.

## 4. Luồng người dùng

1. Mở context menu tại namespace hoặc object, chọn thao tác tương ứng.
2. Với Create/Rename, nhập tên mới; tên là identifier đơn, không phải SQL hay đường dẫn có dấu chấm.
3. Backend dựng plan; dialog hiển thị connection, môi trường, database/schema, object đích và SQL chỉ đọc.
4. Hiển thị ảnh hưởng: mất dữ liệu, reset identity nếu có, khả năng rollback và nguy cơ chờ lock.
5. Người dùng xác nhận; backend tiêu thụ plan và thực thi đúng SQL đã preview.
6. UI hiển thị kết quả, refresh tree/cache/tab liên quan. Không tự chạy lại thao tác ghi.

Preview bắt buộc ở mọi môi trường. Nút xác nhận vô hiệu trong lúc đang chạy. Đổi input làm mất hiệu lực preview cũ; phải preview lại.

Nếu có staged data edit hoặc transaction đang mở trên connection, UI yêu cầu người dùng xử lý trước. Backend cũng chặn khi phát hiện các transaction do ứng dụng quản lý; không tự commit/rollback. Đây không phải bảo đảm loại bỏ race với session hoặc ứng dụng khác.

## 5. Guard và xác nhận

| Điều kiện | Hành vi |
|---|---|
| Connection read-only | Chặn toàn bộ thao tác P4a tại backend |
| Create/Rename, Local hoặc không tag | Preview + xác nhận thông thường |
| Drop/Truncate, mọi môi trường | Preview + gõ đúng tên object đích |
| Production, Create/Rename | Preview + gõ đúng tên namespace/object đích |
| Plan hết hạn hoặc context đổi | PlanStale, không chạy; yêu cầu preview lại |

Tên cần gõ do backend trả về, giữ nguyên hoa/thường và Unicode; không trim hoặc normalize giá trị xác nhận. Với Rename, gõ tên cũ; với Create, gõ tên mới. Dialog luôn hiển thị đầy đủ scope để tránh nhầm object trùng tên.

Guard P4a chỉ được tăng mức bảo vệ so với query guard hiện tại. Query tab giữ hành vi hiện có; spec này không thay đổi policy chạy SQL tự do.

## 6. Kiến trúc

### 6.1 Sinh SQL thuần

Thêm module DDL trong `crates/core/src/sql`, dùng quote identifier hiện có. Đầu vào có kiểu: action, scope, object kind, tên mới và server capabilities. Không nhận SQL tự do từ UI.

Mỗi plan v1 chỉ có một câu DDL. Identifier được quote tại backend; tên rỗng, NUL, vượt giới hạn server hoặc object không hỗ trợ bị từ chối. Dấu quote bên trong tên phải escape đúng dialect. Giới hạn tên theo engine và đơn vị server sử dụng, không chỉ đếm ký tự JavaScript.

Chưa tạo crate schema chỉ để chứa các action đơn giản. P4b sẽ thêm mô hình TableDef, diff và render ALTER khi thực sự cần.

### 6.2 Plan và thực thi

Command đề xuất:

- `object_action_preview(connection_id, action)` → plan id, SQL, scope, warnings, confirmation, thời điểm hết hạn.
- `object_action_apply(plan_id, confirmation, operation_id, channel)` → kết quả cuối; UI cấp UUID operation_id trước khi gọi, channel gửi tiến độ và trạng thái.
- `object_action_cancel(operation_id)` → yêu cầu hủy operation thuộc connection/session tương ứng; không tự retry.

Plan nằm trong bộ nhớ backend, gắn với connection/runtime generation, database/schema, env/read-only, dialect và fingerprint metadata liên quan. TTL dùng cùng chính sách với plan query/edit hiện có. Lưu SQL cố định và tiêu thụ đúng một lần, kể cả khi Apply thất bại.

Trước khi chạy: đọc lại connection policy, capability, metadata và dialect trên session thực thi; kiểm tra tên đích còn phù hợp. Sai khác làm plan stale. Với Create, kiểm tra namespace đích chưa tồn tại; với Rename, kiểm tra tên đích chưa tồn tại. Không dùng IF EXISTS/IF NOT EXISTS để che thay đổi ngoài ứng dụng.

Kiểm tra metadata không thể loại bỏ hoàn toàn race giữa kiểm tra và DDL. P4a không hứa chặn mọi thay đổi đồng thời từ ứng dụng khác; lỗi thực tế của server là kết quả cuối cùng. Không khóa bảng dài hạn trong lúc người dùng đọc preview.

Thực thi bằng session DDL riêng, không dùng session query tab hoặc data edit. Session này vẫn áp dụng SSH/TLS, timeout, dialect và chính sách connection hiện có. Không tái sử dụng edit plan bằng cách giả lập DDL thành row edit.

### 6.3 Transaction, lock và hủy

- PostgreSQL: chạy DDL trong transaction riêng. Chỉ báo committed sau phản hồi COMMIT; chỉ báo rolled_back sau xác nhận rollback hoặc kết quả chắc chắn từ server.
- MySQL/MariaDB: DDL có thể implicit commit; không hứa rollback. TRUNCATE cảnh báo có thể reset AUTO_INCREMENT. PostgreSQL dùng CONTINUE IDENTITY.
- Mỗi lần chờ lấy lock tối đa 5 giây qua cơ chế phù hợp phiên bản: PostgreSQL lock_timeout, MySQL/MariaDB session lock_wait_timeout cho metadata lock. Không đổi global settings.
- Timeout câu lệnh dùng chính sách thực thi hiện có; hết hạn hoặc Stop phải yêu cầu cancel phía server, không chỉ bỏ future phía client.
- UI tạo operation id trước khi Apply; backend đăng ký id trước khi gửi DDL. Stop dùng command cancel riêng khi Apply đang chờ. Cancel đến trước lúc đăng ký phải được ghi nhận để operation không gửi DDL; id đang dùng bị từ chối. Có thể tái sử dụng hạ tầng run/cancel nếu bảo toàn định danh operation và session.
- Mất kết nối, timeout hoặc cancel sau khi đã gửi DDL/COMMIT mà không xác nhận được kết quả: outcome_unknown; không retry và không ghi thông báo thành công.

UI giải thích Stop không bảo đảm hoàn tác DDL đã gửi. Khi không rõ kết quả, yêu cầu refresh/introspect lại trước thao tác tiếp theo.

## 7. Đồng bộ UI và lưu lịch sử

Sau thành công, invalidate schema cache và completion cache đúng scope; refresh tree và metadata của tab liên quan.

- Rename: cập nhật định danh object tab, bỏ page/token/plan cũ và tải lại. Query SQL của người dùng không tự sửa.
- Drop/Truncate: tab liên quan chuyển sang trạng thái cần reload hoặc object không còn tồn tại; không tiếp tục hiển thị dữ liệu cũ như dữ liệu hiện hành.
- Outcome unknown: đánh dấu scope cần kiểm tra lại; không suy đoán rename/drop đã thành công.
- Lỗi refresh sau DDL thành công: báo “thao tác đã thành công, tải lại metadata thất bại”, không báo Apply thất bại.

Lưu History bằng cơ chế redaction hiện có với origin `object_action`, SQL thực tế, thời gian, trạng thái thực thi và lỗi. Việc ghi History thất bại sau khi DDL thành công phải được báo riêng, không đổi kết quả DDL và không gây retry.

Phân biệt lỗi đầu vào, thiếu quyền, dependency, lock timeout, PlanStale, cancel và outcome_unknown. UI giữ ngữ cảnh thao tác khi có lỗi.

## 8. Kiểm chứng local và tiêu chí nghiệm thu

Không thêm workflow CI. Dùng testkit/Docker và toolchain hiện có trên máy.

1. Unit test builder: dialect, quote/Unicode, tên không hợp lệ, unsupported kind và capabilities.
2. Mock session: policy đổi, generation đổi, metadata đổi, plan dùng lại/hết hạn, read-only, sai typed name; không câu DDL nào được gửi khi bị chặn.
3. Fault injection: lỗi DDL, lỗi rollback, mất kết nối trong COMMIT, cancel khi chờ lock, History/refresh thất bại sau thành công.
4. Integration thực sự bật `DNT_TEST_DB=1`: MySQL 5.7/8.0, MariaDB 10.6/11.4, PostgreSQL 12/17; chỉ chạy action hỗ trợ trên mỗi engine.
5. Kiểm tra Create/Rename/Drop/Truncate bằng introspection và đọc dữ liệu sau chạy; kiểm tra dependency không bị xóa qua CASCADE; kiểm tra identity theo policy từng engine.
6. UI tests: preview, typed confirmation, stale/repreview, disabled khi đang chạy, Stop và từng trạng thái kết quả.
7. Desktop smoke MySQL 8 và PostgreSQL 17: tạo namespace thử riêng, tạo bảng qua query, rename, truncate, drop; cleanup chỉ tài nguyên do test tạo.
8. Chạy fmt, clippy, Rust tests, UI lint/tests và desktop smoke local; ghi rõ môi trường, lệnh và kết quả vào final review. DB test bị skip không được tính là đã kiểm chứng server.

Hoàn thành khi tất cả luồng trong bảng phạm vi chạy được trên engine tương ứng; preview trùng SQL gửi đi; guard không vượt được qua IPC; không tự retry ghi; kết quả unknown/partial được giữ nguyên đến UI và History; cleanup không đụng dữ liệu có sẵn của người dùng.

## 9. Sau P4a

P4b có spec riêng cho table designer: Columns, PK, index, FK, checks, options, comment và SQL preview. Trước khi cho sửa bảng cũ phải chứng minh introspection bảo toàn metadata ngoài phạm vi chỉnh sửa; thuộc tính không hiểu phải chặn chỉnh sửa liên quan, không âm thầm loại bỏ khi render lại DDL.

Thứ tự đề xuất tiếp theo: P4b designer → import dữ liệu/dump-restore → workspace đầy đủ và quản trị. Có thể đảo import lên trước P4b khi nhu cầu chuyển dữ liệu trở thành ưu tiên.

## 10. Ghi chú triển khai

- Giữ nguyên phạm vi local-first, không thêm CI.
- Apply dùng UUID operation do UI tạo trước, có channel tiến độ và command Stop riêng.
- MySQL/MariaDB báo Unknown một cách thận trọng khi DDL đã gửi nhưng trả lỗi; không hứa rollback và không tự retry. PostgreSQL chỉ báo rollback khi nhận xác nhận ROLLBACK.
- TRUNCATE PostgreSQL còn tác động bảng con và trigger ON TRUNCATE; preview có cảnh báo. Tham chiếu: https://www.postgresql.org/docs/12/sql-truncate.html
- Mỗi lần chờ lock metadata được giới hạn bằng session lock_wait_timeout ở MySQL/MariaDB và lock_timeout ở PostgreSQL. Tham chiếu: https://dev.mysql.com/doc/refman/8.0/en/server-system-variables.html#sysvar_lock_wait_timeout
- Recheck metadata không loại bỏ hoàn toàn race với session bên ngoài.
- Đã tích hợp editor groups, toast stack và History sidebar từ main; xem final integration review cùng ngày.
