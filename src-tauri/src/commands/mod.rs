//! Tauri commands. Every command is a thin `#[tauri::command] #[specta::specta]` wrapper
//! over a `pub async fn <name>_impl(&AppState, …) -> Result<T, DbError>` (callable from
//! tests without a Tauri runtime), run through [`guard`].

// `AppError` is the IPC error payload: it is serialized as soon as a command returns, so
// its size on the stack does not matter (and the commands' signature is fixed by the UI).
#![allow(clippy::result_large_err)]

pub mod browse;
pub mod connections;
pub mod data;
pub mod export;
pub mod favorites;
pub mod import_data;
pub mod jobs;
pub mod objects;
pub mod query;
pub mod settings;
pub mod table_design;
pub mod transfer;
pub mod update;
pub mod window;
pub mod workspace;

pub mod ddl_edit;

pub mod backup;

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
///
/// The body is boxed at once, so a command's future holds a pointer rather than the body's
/// whole state machine. Tauri builds every async command's future on the main thread (1 MB
/// stack on Windows) before handing it to the runtime, and release builds inline that into
/// frames holding several copies of it: with the bodies inline, running a query overflowed
/// the main thread's stack and the app died (`thread 'main' has overflowed its stack`).
pub fn guard<'a, T, F>(fut: F) -> impl Future<Output = Result<T, AppError>> + 'a
where
    F: Future<Output = Result<T, DbError>> + 'a,
    T: 'a,
{
    let fut = Box::pin(fut);
    async move {
        install_panic_hook();
        clear_panic_location();
        match AssertUnwindSafe(fut).catch_unwind().await {
            Ok(result) => result.map_err(AppError::from),
            // The payload is dropped unread: panic messages can contain user data.
            Err(_payload) => Err(AppError::from(panicked())),
        }
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
            connections::parse_connection_string,
            objects::object_action_preview,
            objects::object_action_apply,
            objects::object_action_cancel,
            table_design::table_design_open,
            table_design::table_design_render,
            table_design::table_design_preview,
            table_design::table_design_apply,
            table_design::table_design_cancel,
            table_design::table_design_close,
            browse::list_databases,
            browse::list_schemas,
            browse::list_objects,
            browse::table_detail,
            browse::object_ddl,
            browse::refresh_node,
            browse::get_schema_cache,
            query::completion_scope,
            query::completion_objects,
            query::query_plan,
            query::statement_ranges,
            query::query_tx,
            query::query_tx_mode,
            query::query_tx_info,
            query::query_close_tab,
            query::query_run,
            query::query_cancel,
            query::query_result_page,
            query::query_result_values,
            query::query_result_find,
            query::query_explain,
            query::query_plan_statement,
            transfer::export_connections,
            transfer::preview_import,
            transfer::apply_import,
            transfer::import_needs_passphrase,
            settings::get_log_level,
            settings::set_log_level,
            settings::open_log_folder,
            settings::get_table_page_size,
            settings::set_table_page_size,
            settings::get_confirm_unbounded_writes,
            settings::set_confirm_unbounded_writes,
            settings::get_idle_tx_warn_minutes,
            settings::set_idle_tx_warn_minutes,
            data::table_data,
            data::table_row_count,
            data::table_data_cancel,
            data::data_editable,
            data::data_preview,
            data::data_apply,
            data::data_cell_fetch,
            data::data_cell_load_file,
            data::data_cell_save_file,
            data::data_copy_sql,
            export::export_rows,
            export::export_table,
            export::export_query,
            export::export_query_result,
            export::export_query_result_cancel,
            export::reveal_export,
            export::export_pick_path,
            window::open_devtools,
            update::update_check,
            update::update_install,
            workspace::tabs_load,
            workspace::tabs_save,
            workspace::tabs_delete,
            workspace::file_read,
            workspace::file_write,
            workspace::file_pick_open,
            workspace::file_pick_save,
            workspace::history_list,
            workspace::history_delete,
            workspace::history_clear,
            favorites::favorites_list,
            favorites::favorites_create,
            favorites::favorites_update,
            favorites::favorites_rename,
            favorites::favorites_move,
            favorites::favorites_delete,
            favorites::snippets_list,
            favorites::snippets_create,
            favorites::snippets_update,
            favorites::snippets_delete,
            ddl_edit::ddl_edit_open,
            ddl_edit::ddl_edit_preview,
            ddl_edit::ddl_edit_cancel,
            ddl_edit::ddl_edit_close,
            ddl_edit::ddl_edit_apply,
            jobs::job_list,
            jobs::job_detail,
            jobs::job_cancel,
            jobs::job_active_count,
            jobs::job_stop_all,
            jobs::job_reveal_result,
            jobs::job_clear_history,
            settings::get_jobs_per_connection,
            settings::set_jobs_per_connection,
            settings::get_external_tools_dir,
            settings::set_external_tools_dir,
            import_data::import_pick_file,
            import_data::import_inspect,
            import_data::import_target,
            import_data::import_auto_map,
            import_data::import_check_mapping,
            import_data::import_dry_run,
            import_data::import_prepare,
            import_data::import_start,
            backup::dump_objects,
            backup::dump_pick_path,
            backup::dump_start,
            backup::tools_list,
            backup::tool_dump_start,
            backup::restore_pick_file,
            backup::restore_preview,
            backup::restore_start,
        ])
        // No events; registering (none) drops the stray `TAURI_CHANNEL` type that
        // `Channel` arguments add, which the bindings import from `@tauri-apps/api`.
        .events(tauri_specta::collect_events![])
        .typ::<Value>()
        .typ::<ColumnMeta>()
        // Payload of the `job://progress` app event (emitted by `jobs::TauriJobEvents`).
        .typ::<crate::jobs::JobEvent>()
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

    /// A body holding 256 KB across an await: what `guard` returns (and so every command's
    /// future, built on the main thread) stays pointer-sized.
    pub(crate) async fn big_body() -> Result<u8, DbError> {
        let buf = [7u8; 256 * 1024];
        tokio::task::yield_now().await;
        Ok(std::hint::black_box(&buf)[1000])
    }

    #[tokio::test]
    async fn guard_boxes_the_body() {
        let body = big_body();
        assert!(std::mem::size_of_val(&body) >= 256 * 1024);
        let guarded = guard(body);
        let size = std::mem::size_of_val(&guarded);
        assert!(size < 256, "guard's future is {size} bytes");
        assert_eq!(guarded.await.unwrap(), 7);
    }

    #[tokio::test]
    async fn guard_maps_errors_and_passes_values() {
        assert_eq!(guard(async { Ok::<_, DbError>(7) }).await.unwrap(), 7);
        let err = guard(async { Err::<(), _>(DbError::cancelled()) })
            .await
            .unwrap_err();
        assert_eq!(err.kind, "cancelled");
    }
}
