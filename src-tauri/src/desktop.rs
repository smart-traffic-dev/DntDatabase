//! Makes the main webview behave like a desktop app rather than a browser tab.
//!
//! `tauri.conf.json` already turns off what Tauri exposes (zoom hotkeys and pinch zoom,
//! browser extensions, general autofill; the status bar and swipe navigation are off by
//! default). WebView2's browser accelerator keys (F5 / Ctrl+R reload, Ctrl+P print,
//! Ctrl+F find, Ctrl+Shift+I / F12 DevTools, Alt+Left back, ...) and password autosave are
//! not, so they are switched off here through the WebView2 settings. The UI's keydown
//! guard (`ui/src/desktop/browserGuard.ts`) is the second layer.
//!
//! DevTools: compiled out of release builds by Tauri. Debug builds keep them on F12 /
//! Ctrl+Shift+I, handled here because the accelerator keys that normally open them are off.

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
            tracing::warn!(error = %e, "could not disable the WebView2 browser keys");
        }
    }) {
        tracing::warn!(error = %e, "could not reach the main webview");
    }
    #[cfg(not(windows))]
    let _ = window;
}

#[cfg(windows)]
mod webview2 {
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2Controller, ICoreWebView2Settings3, ICoreWebView2Settings4};
    use windows_core::Interface;

    pub(super) unsafe fn apply(controller: &ICoreWebView2Controller) -> windows_core::Result<()> {
        let core = controller.CoreWebView2()?;
        let settings = core.Settings()?;
        settings.cast::<ICoreWebView2Settings3>()?.SetAreBrowserAcceleratorKeysEnabled(false)?;
        if let Ok(settings4) = settings.cast::<ICoreWebView2Settings4>() {
            settings4.SetIsPasswordAutosaveEnabled(false)?;
        }
        #[cfg(debug_assertions)]
        devtools_keys(controller, core)?;
        Ok(())
    }

    /// Debug builds: F12 / Ctrl+Shift+I open DevTools (their browser shortcut is off above).
    #[cfg(debug_assertions)]
    unsafe fn devtools_keys(
        controller: &ICoreWebView2Controller,
        core: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2,
    ) -> windows_core::Result<()> {
        use webview2_com::AcceleratorKeyPressedEventHandler;
        use webview2_com::Microsoft::Web::WebView2::Win32::{COREWEBVIEW2_KEY_EVENT_KIND, COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN};

        const VK_F12: u32 = 0x7B;
        const VK_I: u32 = 0x49;
        const VK_CONTROL: i32 = 0x11;
        const VK_SHIFT: i32 = 0x10;

        #[link(name = "user32")]
        extern "system" {
            fn GetKeyState(key: i32) -> i16;
        }

        let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else { return Ok(()) };
            let mut kind = COREWEBVIEW2_KEY_EVENT_KIND::default();
            args.KeyEventKind(&mut kind)?;
            if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN {
                return Ok(());
            }
            let mut key = 0u32;
            args.VirtualKey(&mut key)?;
            let down = |vk: i32| GetKeyState(vk) < 0;
            if key == VK_F12 || (key == VK_I && down(VK_CONTROL) && down(VK_SHIFT)) {
                args.SetHandled(true)?;
                core.OpenDevToolsWindow()?;
            }
            Ok(())
        }));
        let mut token = 0i64;
        controller.add_AcceleratorKeyPressed(&handler, &mut token)
    }
}
