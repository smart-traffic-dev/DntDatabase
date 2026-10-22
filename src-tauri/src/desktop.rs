//! Makes the main webview behave like a desktop app rather than a browser tab.
//!
//! `tauri.conf.json` already turns off what Tauri exposes (zoom hotkeys and pinch zoom,
//! browser extensions, general autofill; the status bar and swipe navigation are off by
//! default). WebView2's browser accelerator keys (F5 / Ctrl+R reload, Ctrl+P print,
//! Ctrl+F find, Ctrl+Shift+I / F12 DevTools, Alt+Left back, ...) and password autosave are
//! not, so they are switched off here through the WebView2 settings. The UI's keydown
//! guard (`ui/src/desktop/browserGuard.ts`) is the second layer.
//!
//! Release builds only turn the accelerator keys off: debug builds keep them natively, so
//! F12 opens DevTools and (with the Vite dev server, where the UI guard allows it) F5 / Ctrl+R
//! reload. The F12 / Ctrl+Shift+I accelerator keys stay off in release builds; DevTools are
//! still compiled in (the `devtools` feature of `tauri`) and open only from Tools > Developer
//! tools (the `open_devtools` command) or the app's own Ctrl+Shift+I shortcut.

use tauri::{Manager, Runtime, WebviewWindowBuilder};

/// Creates the `main` window from `tauri.conf.json` (which has `"create": false` so it can be
/// built here). Debug builds give it their own WebView2 data folder: builds sharing one folder
/// share one WebView2 browser process, owned by whichever started first, so stopping a dev
/// build (`cargo run`, E2E) would otherwise kill a running release build's webview and leave
/// its window black and unresponsive. Release builds keep Tauri's default folder.
pub fn create_main_window<R: Runtime>(app: &tauri::App<R>) -> tauri::Result<()> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == "main")
        .ok_or_else(|| tauri::Error::WindowNotFound)?
        .clone();
    #[allow(unused_mut)]
    let mut builder = WebviewWindowBuilder::from_config(app, &config)?;
    // WebView2 adds the `EBWebView` folder itself (as for the release default).
    #[cfg(debug_assertions)]
    if let Some(local) = dirs::data_local_dir() {
        builder = builder.data_directory(local.join(crate::state::APP_DIR));
    }
    builder.build()?;
    Ok(())
}

/// Applies the WebView2 settings to the `main` window. Failures are logged, never fatal:
/// the app works without them (the UI guard still blocks the browser keys).
pub fn harden_main_webview<R: Runtime, M: Manager<R>>(manager: &M) {
    let Some(window) = manager.get_webview_window("main") else {
        tracing::warn!("no main window: browser keys left enabled");
        return;
    };
    #[cfg(windows)]
    if let Err(e) = window.with_webview(|webview| {
        // SAFETY: COM calls on the WebView2 controller, run by Tauri on the main (UI) thread.
        if let Err(e) = unsafe { webview2::apply(&webview.controller()) } {
            tracing::warn!(error = %e, "could not apply the WebView2 settings");
        }
    }) {
        tracing::warn!(error = %e, "could not reach the main webview");
    }
    #[cfg(not(windows))]
    let _ = window;
}

#[cfg(windows)]
mod webview2 {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Controller, ICoreWebView2Settings4,
    };
    use windows_core::Interface;

    pub(super) unsafe fn apply(controller: &ICoreWebView2Controller) -> windows_core::Result<()> {
        let settings = controller.CoreWebView2()?.Settings()?;
        #[cfg(not(debug_assertions))]
        settings
            .cast::<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings3>()?
            .SetAreBrowserAcceleratorKeysEnabled(false)?;
        if let Ok(settings4) = settings.cast::<ICoreWebView2Settings4>() {
            settings4.SetIsPasswordAutosaveEnabled(false)?;
        }
        Ok(())
    }
}
