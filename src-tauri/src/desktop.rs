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
//! reload. DevTools themselves are compiled out of release builds by Tauri.

use tauri::{Manager, Runtime};

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
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2Controller, ICoreWebView2Settings4};
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
