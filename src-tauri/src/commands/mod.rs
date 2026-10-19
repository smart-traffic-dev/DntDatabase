//! Tauri commands. Every command is a thin `#[tauri::command] #[specta::specta]` wrapper
//! over a `pub async fn <name>_impl(&AppState, …) -> Result<T, DbError>` (callable from
//! tests without a Tauri runtime), run through [`guard`].

// `AppError` is the IPC error payload: it is serialized as soon as a command returns, so
// its size on the stack does not matter (and the commands' signature is fixed by the UI).
#![allow(clippy::result_large_err)]

pub mod browse;
pub mod connections;
pub mod settings;
pub mod transfer;

use dnt_core::error::{AppError, DbError};
use dnt_core::value::{ColumnMeta, Value};
use futures_util::FutureExt;
use specta_typescript::Typescript;
use std::cell::RefCell;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Once;

/// Where debug builds write the bindings (relative to `src-tauri`).
pub const BINDINGS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../ui/src/bindings.ts");

/// Logged (with the panic location) when a command panics.
pub const PANIC_LOG_MESSAGE: &str = "a command panicked";

thread_local! {
    /// `file:line` of the last panic on this thread, recorded by the panic hook.
    static PANIC_LOCATION: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Chains a hook in front of the current panic hook that records where a panic happened,
/// so the guards can log the location without the payload (which may hold user data).
fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()));
            PANIC_LOCATION.with(|slot| *slot.borrow_mut() = location);
            previous(info);
        }));
    });
}

fn clear_panic_location() {
    PANIC_LOCATION.with(|slot| slot.borrow_mut().take());
}

/// Logs a caught panic of this thread (location only) and returns the error to report.
fn panicked() -> DbError {
    let location = PANIC_LOCATION.with(|slot| slot.borrow_mut().take());
    tracing::error!(
        location = location.as_deref().unwrap_or("unknown"),
        "{PANIC_LOG_MESSAGE}"
    );
    DbError::Internal {
        message: "the command failed unexpectedly; see the log for details".into(),
    }
}

/// Runs a command body: errors become `AppError`, and a panic becomes
/// `AppError { kind: "internal" }` (logged) instead of tearing down the IPC call.
pub async fn guard<T, F>(fut: F) -> Result<T, AppError>
where
    F: Future<Output = Result<T, DbError>>,
{
    install_panic_hook();
    clear_panic_location();
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(result) => result.map_err(AppError::from),
        // The payload is dropped unread: panic messages can contain user data.
        Err(_payload) => Err(AppError::from(panicked())),
    }
}

/// Runs blocking or CPU-heavy work (SQLite, keychain, Argon2id, file I/O) on the blocking
/// pool. A panic there is logged like in [`guard`] and reported as `Internal`.
pub async fn blocking<T, F>(f: F) -> Result<T, DbError>
where
    F: FnOnce() -> Result<T, DbError> + Send + 'static,
    T: Send + 'static,
{
    install_panic_hook();
    tauri::async_runtime::spawn_blocking(move || {
        clear_panic_location();
        std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_payload| Err(panicked()))
    })
    .await
    .map_err(|e| DbError::Internal {
        message: format!("a background task failed: {e}"),
    })?
}

/// Every command, plus the result types the UI needs that no command returns yet.
pub fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new()
        .commands(tauri_specta::collect_commands![
            connections::list_tree,
            connections::create_connection,
            connections::update_connection,
            connections::duplicate_connection,
            connections::delete_connection,
            connections::create_group,
            connections::rename_group,
            connections::delete_group,
            connections::move_node,
            connections::test_connection,
            connections::open_connection,
            connections::close_connection,
            connections::connection_state,
            connections::trust_host_key,
            browse::list_databases,
            browse::list_schemas,
            browse::list_objects,
            browse::table_detail,
            browse::object_ddl,
            browse::refresh_node,
            browse::get_schema_cache,
            transfer::export_connections,
            transfer::preview_import,
            transfer::apply_import,
            transfer::import_needs_passphrase,
            settings::get_log_level,
            settings::set_log_level,
            settings::open_log_folder,
        ])
        .typ::<Value>()
        .typ::<ColumnMeta>()
}

/// The TypeScript exporter. 64-bit integers must carry an explicit `#[specta(type = …)]`
/// override; the default `BigInt` behaviour (fail) stays on so a missing one is caught.
fn typescript() -> Typescript {
    Typescript::default().header("// Generated by tauri-specta from src-tauri. Do not edit.")
}

/// Writes the TypeScript bindings of every command and type to `path`.
pub fn export_bindings(path: &Path) -> Result<(), DbError> {
    specta_builder()
        .export(typescript(), path)
        .map_err(|e| DbError::Internal {
            message: format!("cannot export the TypeScript bindings: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// Collects formatted log output.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn capture_logs() -> (Captured, tracing::subscriber::DefaultGuard) {
        let out = Captured::default();
        let writer = out.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        (out, tracing::subscriber::set_default(subscriber))
    }

    const USER_DATA: &str = "SELECT secret_column FROM customers";

    // Current-thread runtime: the log subscriber set for this thread covers the guard.
    #[tokio::test]
    async fn guard_converts_panic_to_internal() {
        let (logs, _sub) = capture_logs();
        let err = guard(async {
            if true {
                panic!("boom: {USER_DATA}");
            }
            Ok::<(), DbError>(())
        })
        .await
        .unwrap_err();
        assert_eq!(err.kind, "internal");
        assert!(!format!("{err:?}").contains("boom"));

        let logs = logs.text();
        assert!(logs.contains(PANIC_LOG_MESSAGE), "{logs}");
        // The location is logged, the payload is not.
        assert!(
            logs.contains("commands") && logs.contains("mod.rs:"),
            "{logs}"
        );
        assert!(
            !logs.contains("boom") && !logs.contains(USER_DATA),
            "{logs}"
        );
    }

    #[tokio::test]
    async fn blocking_converts_panic_to_internal() {
        let err = blocking(|| -> Result<(), DbError> { panic!("boom: {USER_DATA}") })
            .await
            .unwrap_err();
        assert!(matches!(&err, DbError::Internal { message } if !message.contains("boom")));
        assert_eq!(blocking(|| Ok::<_, DbError>(5)).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn guard_maps_errors_and_passes_values() {
        assert_eq!(guard(async { Ok::<_, DbError>(7) }).await.unwrap(), 7);
        let err = guard(async { Err::<(), _>(DbError::Cancelled) })
            .await
            .unwrap_err();
        assert_eq!(err.kind, "cancelled");
    }
}
