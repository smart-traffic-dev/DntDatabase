//! Window-level commands driven by the UI's menu bar.

/// Opens the webview's DevTools. The `devtools` feature of `tauri` keeps this available in
/// release builds too; the F12 / Ctrl+Shift+I accelerator keys stay off there (see
/// `desktop.rs`), so this command (Tools > Developer tools) is the way in.
#[tauri::command]
#[specta::specta]
pub fn open_devtools(window: tauri::WebviewWindow) {
    window.open_devtools();
}
