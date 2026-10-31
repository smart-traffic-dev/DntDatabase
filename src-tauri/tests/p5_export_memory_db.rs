//! Memory of a 1,000,000-row table export to CSV (framework completion criterion: an
//! export of a million rows must not grow the process memory by more than 200 MB),
//! against MySQL 8 of the Docker test matrix (`DNT_TEST_DB=1`).
//!
//! The table is filled server-side in the `p5_big` database (dropped first if left over,
//! and dropped again by a guard when the test ends, also on failure). A background
//! thread samples this process's working set every 250 ms while
//! `export_table_impl` runs; the baseline is taken after the table is filled. Ignored
//! by default (it takes a while); run with
//! `$env:DNT_TEST_DB = "1"; cargo test -p dntdatabase --test p5_export_memory_db -- --ignored --nocapture`.

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink};
use dnt_core::meta::{ObjectKind, ObjectRef};
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dntdatabase::commands::connections::{
    close_connection_impl, create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::commands::export::export_table_impl;
use dntdatabase::export_format::{ExportFormat, ExportOptions};
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

const DB: &str = "p5_big";
const ROWS: u64 = 1_000_000;
const T: Target = Target::MySql8;

// ------------------------------------------------------------------ working set

#[cfg(windows)]
mod mem {
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(
            process: isize,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    fn counters() -> ProcessMemoryCounters {
        let mut c = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            ..Default::default()
        };
        // SAFETY: the pseudo handle of the current process and a properly sized struct.
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        assert!(ok != 0, "K32GetProcessMemoryInfo failed");
        c
    }

    /// Current working set in bytes.
    pub fn working_set() -> u64 {
        counters().working_set_size as u64
    }

    /// Private (commit) bytes.
    pub fn private_bytes() -> u64 {
        counters().pagefile_usage as u64
    }
}

#[cfg(not(windows))]
mod mem {
    fn status(key: &str) -> u64 {
        let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        s.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .unwrap_or(0)
    }
    pub fn working_set() -> u64 {
        status("VmRSS:")
    }
    pub fn private_bytes() -> u64 {
        status("VmData:")
    }
}

fn mb(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

// ------------------------------------------------------------------ admin helpers

struct NullSink;

impl RowSink for NullSink {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

async fn admin(sql: &str) {
    let session = MySqlDriver
        .connect(&T.resolved(Some(FIXTURE_DB)))
        .await
        .unwrap();
    session
        .sql()
        .unwrap()
        .execute(sql, ExecOptions::default(), &mut NullSink)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    session.close().await;
}

/// Drops `p5_big` when the test ends, also on failure (on its own runtime, since the
/// test's runtime may be unwinding).
struct DropDb;

impl Drop for DropDb {
    fn drop(&mut self) {
        let _ = std::thread::spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(admin(&format!("DROP DATABASE IF EXISTS `{DB}`")));
        })
        .join();
    }
}

async fn fill() {
    admin(&format!("DROP DATABASE IF EXISTS `{DB}`")).await;
    admin(&format!("CREATE DATABASE `{DB}`")).await;
    admin(&format!(
        "CREATE TABLE `{DB}`.big (\
           id INT PRIMARY KEY, \
           name VARCHAR(64) NOT NULL, \
           amount DECIMAL(12,2) NOT NULL, \
           created_at DATETIME NOT NULL, \
           note TEXT NOT NULL, \
           qty INT NOT NULL)"
    ))
    .await;
    admin(&format!("CREATE TABLE `{DB}`.digits (d INT PRIMARY KEY)")).await;
    admin(&format!(
        "INSERT INTO `{DB}`.digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)"
    ))
    .await;
    // Ten chunks of 100,000 rows: five digit tables cross-joined, offset by the chunk.
    for chunk in 0..10u64 {
        admin(&format!(
            "INSERT INTO `{DB}`.big (id, name, amount, created_at, note, qty) \
             SELECT n, CONCAT('name_', n), (n % 100000) / 100 + 0.25, \
                    TIMESTAMPADD(SECOND, n, '2024-01-01 00:00:00'), \
                    CONCAT(REPEAT(MD5(n), 3), '_note'), n % 1000 \
             FROM (SELECT {base} + a.d + 10*b.d + 100*c.d + 1000*e.d + 10000*f.d + 1 AS n \
                   FROM `{DB}`.digits a, `{DB}`.digits b, `{DB}`.digits c, \
                        `{DB}`.digits e, `{DB}`.digits f) s",
            base = chunk * 100_000
        ))
        .await;
    }
}

fn app_state(dir: &std::path::Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(dnt_core::secret::MemorySecretStore::default()),
    )
    .unwrap()
}

async fn open(state: &AppState) -> Uuid {
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: "p5_big".into(),
        sort_order: 0,
        kind: T.kind(),
        host: "127.0.0.1".into(),
        port: T.host_port(),
        username: T.admin_user().into(),
        default_db: Some(DB.into()),
        env_tag: None,
        read_only: false,
        secret_mode: SecretMode::Saved,
        ssh: None,
        ssl: None,
        options: ConnOptions::default(),
    };
    let secrets: SecretsInput =
        serde_json::from_value(json!({ "db_password": TEST_PASSWORD })).unwrap();
    let id = create_connection_impl(state, cfg, secrets)
        .await
        .unwrap()
        .id;
    open_connection_impl(state, id, None).await.unwrap();
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "1M-row export memory measurement; run with --ignored"]
async fn export_million_rows_memory() {
    require_db!();
    let _guard = DropDb;
    let t0 = Instant::now();
    fill().await;
    println!("fill: {:.1}s", t0.elapsed().as_secs_f64());

    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let id = open(&state).await;
    let path = dir.path().join("big.csv");

    let baseline = mem::working_set();
    let baseline_private = mem::private_bytes();

    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(baseline));
    let peak_private = Arc::new(AtomicU64::new(baseline_private));
    let samples = Arc::new(AtomicU64::new(0));
    let sampler = {
        let (stop, peak, peak_private, samples) = (
            Arc::clone(&stop),
            Arc::clone(&peak),
            Arc::clone(&peak_private),
            Arc::clone(&samples),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(mem::working_set(), Ordering::Relaxed);
                peak_private.fetch_max(mem::private_bytes(), Ordering::Relaxed);
                samples.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(250));
            }
        })
    };

    let started = Instant::now();
    let summary = export_table_impl(
        &state,
        id,
        ObjectRef {
            database: DB.into(),
            schema: None,
            name: "big".into(),
            kind: ObjectKind::Table,
        },
        vec![],
        None,
        ExportOptions {
            format: ExportFormat::Csv,
            delimiter: ",".into(),
            header: true,
            quote_all: false,
            bom: false,
            table_name: "big".into(),
            formula_guard: false,
        },
        path.to_string_lossy().into(),
        "export:big".into(),
        Box::new(|_| {}),
    )
    .await;
    let elapsed = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    let summary = summary.unwrap();

    let peak = peak.load(Ordering::Relaxed).max(mem::working_set());
    let peak_private = peak_private.load(Ordering::Relaxed);
    let size = std::fs::metadata(&path).unwrap().len();
    let delta = peak.saturating_sub(baseline);
    println!(
        "rows={} file_bytes={} file_mb={:.1} elapsed_s={:.2} samples={}",
        summary.rows,
        size,
        mb(size),
        elapsed.as_secs_f64(),
        samples.load(Ordering::Relaxed)
    );
    println!(
        "working_set: baseline_mb={:.1} peak_mb={:.1} delta_mb={:.1}",
        mb(baseline),
        mb(peak),
        mb(delta)
    );
    println!(
        "private_bytes: baseline_mb={:.1} peak_mb={:.1} delta_mb={:.1}",
        mb(baseline_private),
        mb(peak_private),
        mb(peak_private.saturating_sub(baseline_private))
    );

    close_connection_impl(&state, id).await.unwrap();
    assert_eq!(summary.rows, ROWS);
    assert!(
        mb(delta) < 200.0,
        "working set grew {:.1} MB during the export",
        mb(delta)
    );
}
