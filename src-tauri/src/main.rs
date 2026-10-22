// No console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use dnt_store::keyring_store::KeyringSecretStore;
use dntdatabase::commands;
use dntdatabase::desktop;
use dntdatabase::logging;
use dntdatabase::state::{AppPaths, AppState};
use std::error::Error;
use std::sync::Arc;

fn main() {
    if let Err(e) = run() {
        tracing::error!(error = %e, "DntDatabase failed to start");
        eprintln!("DntDatabase failed to start: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let paths = AppPaths::resolve()?;
    std::fs::create_dir_all(&paths.logs_dir)?;
    let log = logging::init(&paths.logs_dir);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "DntDatabase starting");

    let state = AppState::open(paths, Arc::new(KeyringSecretStore::from_env()))?;
    // The level chosen in Settings (Debug logging) survives restarts.
    let level = commands::settings::saved_log_level(&state.repo);
    if level != logging::LogLevel::Info {
        if let Err(e) = log.set_level(level) {
            tracing::warn!(error = %e, "could not apply the saved log level");
        }
    }
    let builder = commands::specta_builder();

    #[cfg(debug_assertions)]
    if let Err(e) = commands::export_bindings(std::path::Path::new(commands::BINDINGS_PATH)) {
        tracing::warn!(error = %e, "could not export the TypeScript bindings");
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .manage(log)
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            desktop::harden_main_webview(app);
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}
