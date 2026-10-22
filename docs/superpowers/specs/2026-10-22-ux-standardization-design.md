# Chuẩn hóa UX/UI: primitives, patterns, enforcement

- Ngày: 2026-10-22
- Trạng thái: đề xuất, chưa implement.
- Kế thừa: `2026-10-21-ux-redesign-design.md` (tokens 3 lớp, focus 1px, flat tabs, density). Spec này không đổi
  các quyết định ở đó; nó đóng các chỗ code đã trôi khỏi chúng và thêm các pattern còn thiếu.
- Nguồn: 5 lượt review code (shell, dialogs, query, tree/data/designer, overlays) và một lượt audit chạy UI thật
  trong `e2e-web` (Chromium, fake backend, light + dark, 1440 và 1024 px).

## 1. Vấn đề

Token đã sạch: không còn tên legacy, không có font-size hay radius viết cứng, không có màu viết cứng ngoài phần
được phép. Phần lệch nằm ở tầng trên token: mỗi màn hình tự viết lại component của riêng nó.

| Hạng mục | Hiện trạng đo được | Hệ quả |
|---|---|---|
| Button | 11 class: `btn`, `icon-btn`, `qt-btn`, `ta-btn`, `ex-btn`, `gt-btn`, `status-btn`, `titlebar-btn`, `menubar-btn`, `dz-btn`, `dz-pk-btn` | `.btn` cao 30px, `.btn.small` 20px, `qt-btn`/`ta-btn` 28px, `ex-btn` 24px, `gt-btn` theo padding. Hover/disabled/focus mỗi nơi một kiểu; `status-btn` không có hover hay disabled. |
| Chip / badge / tag | ~30 class (`chip`, `dv-chip`, `de-chip`, `dz-chip`, `ta-chip*`, `qt-tx-chip`, `env-tag`, `env-badge`, `ro-badge`, `ro-tag`, `ex-tag`, `ex-hotpill`, `qr-badge`, `engine-badge`, `ssh-tag`, …) | Môi trường hiện 3 kiểu (`env-tag`, `qt-env`, `status-env`); Staging và Local trùng màu trong tree. |
| Banner / note / hint | 7 class banner + 15 class hint/note | Warning có nơi là banner, có nơi là `hint` xám (ObjectActionDialog). Error có nơi `field-error`, có nơi `<p role=alert>` trơn. |
| Empty state | 12 class (`empty`, `dv-empty`, `tree-empty`, `fav-empty`, `palette-empty`, …) | Không cùng bố cục, không cùng giọng văn; History luôn báo "No history." kể cả khi đang lọc. |
| Header / toolbar | 12 class `*-head` + 2 `toolbar` | Chiều cao 32/34/36/40px tùy nơi; `.rs-head` 34px viết cứng thay vì `--dnt-tab-h`. |
| Spacing / size viết cứng | ~158 padding/margin/gap px ở 15 file, ~52 height px ở 10 file | Density Comfortable chỉ có tác dụng một phần. |
| z-index | 8 giá trị viết cứng (`50`, `5`, `3`, `2`, `1`) | `.context-menu` (50) nằm dưới palette (200) và modal (300). |
| Menu | 2 cài đặt `role="menu"`: `ActionMenu` và một menu riêng trong `QueryToolbar` | Bàn phím, Esc, radio group khác nhau. |
| Icon | SVG inline ở 6 file ngoài `icons.tsx`; ký tự `› × ▾ ⋯ ⚠ ✓` làm icon ở ≥12 file | Kích thước và nét icon không đồng nhất. |
| Font weight | 400/500/600/700 viết số, không có token | 45 chỗ 600 và 5 chỗ 700 lẫn nhau cho cùng vai trò "tiêu đề". |

Bên cạnh đó là các lỗi hành vi lặp lại ở nhiều nơi, do không có pattern chung: dialog nguy hiểm thiếu
`alertdialog` và nút danger, dialog đang chạy (busy) vẫn đóng được, Esc làm mất dữ liệu chưa lưu, live region
không đọc, và không có phím để vào menu bar hay thoát khỏi Monaco.

## 2. Nguyên tắc

1. Một vai trò, một component. Màn hình mới không được định nghĩa class button, chip, banner hay empty state
   riêng; nếu thiếu biến thể thì thêm vào primitive.
2. Kích thước chỉ đi qua token. Chiều cao control, chiều cao hàng, spacing và z-index đều đọc `--dnt-*`.
3. Giữ quyết định của spec 2026-10-21: focus 1px, không ring hay glow, disabled không dùng opacity, một accent.
4. Hành vi an toàn là một phần của component chứ không để từng màn hình tự nhớ: destructive, busy và dirty đều do
   primitive xử lý.
5. Mọi quy tắc làm được bằng test thì phải có test (§6).
