//! The N7 runner's plumbing through a fake [`Spawner`] (no real process, no database),
//! plus one real `mysqldump --version` when the tool is installed.

use super::*;
use dnt_transfer::tools::{
    find_tool, mysql_option_file, open_input, pgpass_line, Invocation, PgPassScope, SecretFileSpec,
    StdoutUse, Tool, ToolOutput, ENV_REMOVE,
};
use secrecy::SecretString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{watch, Notify};

const PASSWORD: &str = "Sup3r$ecret!";

/// What the fake tool does once spawned.
#[derive(Clone, Default)]
struct Script {
    stdout: Vec<Vec<u8>>,
    stderr: Vec<u8>,
    code: i32,
    /// Runs until killed.
    hang: bool,
    /// Reads stdin to the end before exiting.
    read_stdin: bool,
}

#[derive(Default)]
struct Seen {
    spawned: AtomicUsize,
    /// The secret file's contents at spawn time.
    secret_at_spawn: Mutex<Option<String>>,
    stdin: Mutex<Vec<u8>>,
    /// The fake read its stdin to the end (saw it closed).
    stdin_eof: AtomicBool,
    killed: AtomicBool,
}

struct FakeSpawner {
    script: Script,
    seen: Arc<Seen>,
}

struct FakeChild {
    stdin: Option<DuplexStream>,
    stdout: Option<DuplexStream>,
    stderr: Option<DuplexStream>,
    exit: watch::Receiver<Option<i32>>,
    kill: Arc<Notify>,
    seen: Arc<Seen>,
}

fn secret_path(spec: &SecretFileSpec) -> Option<&Path> {
    match spec {
        SecretFileSpec::None => None,
        SecretFileSpec::MysqlOptionFile(p) | SecretFileSpec::PgPass(p) => Some(p),
    }
}

impl Spawner for FakeSpawner {
    fn spawn(&self, inv: &Invocation) -> std::io::Result<Box<dyn ToolChild>> {
        self.seen.spawned.fetch_add(1, Ordering::SeqCst);
        if let Some(p) = secret_path(&inv.secret) {
            *self.seen.secret_at_spawn.lock().unwrap() = std::fs::read_to_string(p).ok();
        }
        let (stdin_parent, mut stdin_child) = tokio::io::duplex(1024);
        let (mut stdout_child, stdout_parent) = tokio::io::duplex(1024);
        let (mut stderr_child, stderr_parent) = tokio::io::duplex(1024);
        let (exit_tx, exit_rx) = watch::channel(None);
        let kill = Arc::new(Notify::new());
        let script = self.script.clone();
        let seen = Arc::clone(&self.seen);
        let killed = Arc::clone(&kill);
        tokio::spawn(async move {
            let work = async {
                for chunk in &script.stdout {
                    stdout_child.write_all(chunk).await.unwrap();
                }
                stderr_child.write_all(&script.stderr).await.unwrap();
                if script.read_stdin {
                    let mut buf = vec![0u8; 4096];
                    loop {
                        let n = stdin_child.read(&mut buf).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        seen.stdin.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                    seen.stdin_eof.store(true, Ordering::SeqCst);
                }
                if script.hang {
                    std::future::pending::<()>().await;
                }
            };
            let code = tokio::select! {
                _ = work => script.code,
                _ = killed.notified() => 1,
            };
            drop(stdout_child);
            drop(stderr_child);
            drop(stdin_child);
            let _ = exit_tx.send(Some(code));
        });
        Ok(Box::new(FakeChild {
            stdin: Some(stdin_parent),
            stdout: Some(stdout_parent),
            stderr: Some(stderr_parent),
            exit: exit_rx,
            kill,
            seen: Arc::clone(&self.seen),
        }))
    }
}

#[async_trait::async_trait]
impl ToolChild for FakeChild {
    fn take_stdin(&mut self) -> Option<WritePipe> {
        self.stdin.take().map(|p| Box::pin(p) as WritePipe)
    }
    fn take_stdout(&mut self) -> Option<ReadPipe> {
        self.stdout.take().map(|p| Box::pin(p) as ReadPipe)
    }
    fn take_stderr(&mut self) -> Option<ReadPipe> {
        self.stderr.take().map(|p| Box::pin(p) as ReadPipe)
    }
    async fn wait(&mut self) -> std::io::Result<Option<i32>> {
        let got = self.exit.wait_for(Option::is_some).await.unwrap();
        Ok(*got)
    }
    async fn kill(&mut self) -> std::io::Result<()> {
        self.seen.killed.store(true, Ordering::SeqCst);
        self.kill.notify_one();
        Ok(())
    }
}

#[derive(Default)]
struct FakeCtx {
    lines: Mutex<Vec<(JobLogLevel, String)>>,
    stopped: AtomicBool,
    kill: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    kill_set: AtomicUsize,
    bytes: Mutex<u64>,
}

impl FakeCtx {
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(k) = self.kill.lock().unwrap().as_ref() {
            k();
        }
    }
    fn text(&self) -> String {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .map(|(_, l)| l.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl RunCtx for FakeCtx {
    fn tool_line(&self, level: JobLogLevel, line: String) {
        self.lines.lock().unwrap().push((level, line));
    }
    fn set_kill(&self, kill: Option<Box<dyn Fn() + Send + Sync>>) {
        self.kill_set.fetch_add(1, Ordering::SeqCst);
        let late = self.stopped.load(Ordering::SeqCst);
        if let (true, Some(k)) = (late, kill.as_ref()) {
            k();
        }
        *self.kill.lock().unwrap() = kill;
    }
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
    fn bytes_out(&self, bytes: u64) {
        *self.bytes.lock().unwrap() = bytes;
    }
}

fn invocation(secret: SecretFileSpec, stdout: StdoutUse, stdin_file: bool) -> Invocation {
    Invocation {
        program: PathBuf::from(r"C:\tools\mysqldump.exe"),
        args: vec!["--version".into()],
        env_remove: ENV_REMOVE.to_vec(),
        env_set: vec![],
        secret,
        pgpass_scope: Some(scope()),
        stdin_file,
        stdout,
    }
}

fn scope() -> PgPassScope {
    PgPassScope {
        host: "127.0.0.1".into(),
        port: 54317,
        user: "app".into(),
    }
}

fn password() -> SecretString {
    SecretString::from(PASSWORD.to_string())
}

fn output(path: &Path, compress: bool) -> ToolOutput {
    ToolOutput::new(Box::new(std::fs::File::create(path).unwrap()), compress)
}

fn read_back(path: &Path, compress: bool) -> String {
    let mut s = String::new();
    open_input(path, compress)
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_writes_stdout_masks_stderr_and_removes_secret() {
    for compress in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("client.cnf");
        let out_path = dir.path().join("out.sql.dntpart");
        let seen = Arc::new(Seen::default());
        let spawner = FakeSpawner {
            script: Script {
                stdout: vec![b"-- MySQL dump\n".to_vec(), b"CREATE TABLE t (id int);\n".to_vec()],
                stderr: format!(
                    "mysqldump: [Warning] password {PASSWORD} in a line\nmysqldump: Got error: 1045: denied\r\nlast line without newline"
                )
                .into_bytes(),
                code: 0,
                ..Script::default()
            },
            seen: Arc::clone(&seen),
        };
        let ctx = FakeCtx::default();
        let inv = invocation(
            SecretFileSpec::MysqlOptionFile(secret.clone()),
            StdoutUse::File,
            false,
        );
        let report = run_tool(
            &spawner,
            &ctx,
            ToolRun {
                inv: &inv,
                password: Some(&password()),
                stdin: None,
                stdout: StdoutSink::File(Box::new(output(&out_path, compress))),
            },
        )
        .await
        .unwrap();
        assert_eq!(report.code, Some(0));
        assert!(!report.killed);
        assert!(report.out_error.is_none());
        assert_eq!(report.out_bytes, 14 + 25);
        assert_eq!(
            read_back(&out_path, compress),
            "-- MySQL dump\nCREATE TABLE t (id int);\n"
        );
        // The secret file held the option file at spawn time and is gone now.
        assert_eq!(
            seen.secret_at_spawn.lock().unwrap().as_deref(),
            Some(mysql_option_file(PASSWORD).unwrap().as_str())
        );
        assert!(!secret.exists());
        // Stderr lines, masked, one per line; an error line is logged as an error.
        let lines = ctx.lines.lock().unwrap().clone();
        assert_eq!(
            lines,
            vec![
                (
                    JobLogLevel::Warn,
                    "mysqldump: [Warning] password ****** in a line".to_string()
                ),
                (
                    JobLogLevel::Error,
                    "mysqldump: Got error: 1045: denied".to_string()
                ),
                (JobLogLevel::Info, "last line without newline".to_string()),
            ]
        );
        assert!(!ctx.text().contains(PASSWORD));
        // The kill hook was installed, then cleared.
        assert_eq!(ctx.kill_set.load(Ordering::SeqCst), 2);
        assert!(ctx.kill.lock().unwrap().is_none());
        assert_eq!(*ctx.bytes.lock().unwrap(), 39);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pgpass_secret_and_missing_password() {
    let dir = tempfile::tempdir().unwrap();
    for (pw, expected) in [
        (Some(password()), pgpass_line(&scope(), PASSWORD).unwrap()),
        (None, String::new()),
    ] {
        let secret = dir.path().join("pgpass.conf");
        let seen = Arc::new(Seen::default());
        let spawner = FakeSpawner {
            script: Script::default(),
            seen: Arc::clone(&seen),
        };
        let inv = invocation(
            SecretFileSpec::PgPass(secret.clone()),
            StdoutUse::Discard,
            false,
        );
        let report = run_tool(
            &spawner,
            &FakeCtx::default(),
            ToolRun {
                inv: &inv,
                password: pw.as_ref(),
                stdin: None,
                stdout: StdoutSink::Discard,
            },
        )
        .await
        .unwrap();
        assert_eq!(report.code, Some(0));
        assert_eq!(
            seen.secret_at_spawn.lock().unwrap().as_deref(),
            Some(expected.as_str())
        );
        assert!(!secret.exists());
    }
    // MySQL without a password: an option file with no password line.
    let secret = dir.path().join("client.cnf");
    let seen = Arc::new(Seen::default());
    let spawner = FakeSpawner {
        script: Script::default(),
        seen: Arc::clone(&seen),
    };
    let inv = invocation(
        SecretFileSpec::MysqlOptionFile(secret.clone()),
        StdoutUse::Discard,
        false,
    );
    run_tool(
        &spawner,
        &FakeCtx::default(),
        ToolRun {
            inv: &inv,
            password: None,
            stdin: None,
            stdout: StdoutSink::Discard,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        seen.secret_at_spawn.lock().unwrap().as_deref(),
        Some("[client]\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_kills_the_tool_and_removes_secret() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("client.cnf");
    let out_path = dir.path().join("out.dntpart");
    let seen = Arc::new(Seen::default());
    let spawner = FakeSpawner {
        script: Script {
            stdout: vec![b"partial".to_vec()],
            hang: true,
            ..Script::default()
        },
        seen: Arc::clone(&seen),
    };
    let ctx = Arc::new(FakeCtx::default());
    let inv = invocation(
        SecretFileSpec::MysqlOptionFile(secret.clone()),
        StdoutUse::File,
        false,
    );
    let stopper = {
        let ctx = Arc::clone(&ctx);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            ctx.stop();
        })
    };
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        run_tool(
            &spawner,
            ctx.as_ref(),
            ToolRun {
                inv: &inv,
                password: Some(&password()),
                stdin: None,
                stdout: StdoutSink::File(Box::new(output(&out_path, false))),
            },
        ),
    )
    .await
    .expect("the runner must end after Stop")
    .unwrap();
    stopper.await.unwrap();
    assert!(report.killed);
    assert!(seen.killed.load(Ordering::SeqCst));
    assert!(!secret.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn stopped_before_spawn_never_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("client.cnf");
    let seen = Arc::new(Seen::default());
    let spawner = FakeSpawner {
        script: Script::default(),
        seen: Arc::clone(&seen),
    };
    let ctx = FakeCtx::default();
    ctx.stop();
    let inv = invocation(
        SecretFileSpec::MysqlOptionFile(secret.clone()),
        StdoutUse::Discard,
        false,
    );
    let err = run_tool(
        &spawner,
        &ctx,
        ToolRun {
            inv: &inv,
            password: Some(&password()),
            stdin: None,
            stdout: StdoutSink::Discard,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, DbError::Cancelled { .. }), "{err:?}");
    assert_eq!(seen.spawned.load(Ordering::SeqCst), 0);
    assert!(!secret.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_file_is_create_new() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("client.cnf");
    std::fs::write(&secret, "planted").unwrap();
    let seen = Arc::new(Seen::default());
    let spawner = FakeSpawner {
        script: Script::default(),
        seen: Arc::clone(&seen),
    };
    let inv = invocation(
        SecretFileSpec::MysqlOptionFile(secret.clone()),
        StdoutUse::Discard,
        false,
    );
    let err = run_tool(
        &spawner,
        &FakeCtx::default(),
        ToolRun {
            inv: &inv,
            password: Some(&password()),
            stdin: None,
            stdout: StdoutSink::Discard,
        },
    )
    .await
    .unwrap_err();
    assert!(!format!("{err:?}").contains(PASSWORD));
    assert_eq!(seen.spawned.load(Ordering::SeqCst), 0);
    // Left as it was (not overwritten, not removed).
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "planted");
}

#[tokio::test(flavor = "multi_thread")]
async fn stdin_is_fed_decompressed() {
    let dir = tempfile::tempdir().unwrap();
    let text: String = (0..20_000)
        .map(|i| format!("INSERT INTO t VALUES ({i});\n"))
        .collect();
    for gzip in [false, true] {
        let input = dir.path().join(format!("in-{gzip}"));
        {
            let f = std::fs::File::create(&input).unwrap();
            if gzip {
                let mut e = flate2::write::GzEncoder::new(f, flate2::Compression::default());
                e.write_all(text.as_bytes()).unwrap();
                e.finish().unwrap();
            } else {
                let mut f = f;
                f.write_all(text.as_bytes()).unwrap();
            }
        }
        let seen = Arc::new(Seen::default());
        let spawner = FakeSpawner {
            script: Script {
                read_stdin: true,
                ..Script::default()
            },
            seen: Arc::clone(&seen),
        };
        let inv = invocation(SecretFileSpec::None, StdoutUse::Discard, true);
        let report = run_tool(
            &spawner,
            &FakeCtx::default(),
            ToolRun {
                inv: &inv,
                password: None,
                stdin: Some(StdinFeed { path: input, gzip }),
                stdout: StdoutSink::Discard,
            },
        )
        .await
        .unwrap();
        assert_eq!(report.code, Some(0));
        assert!(report.in_error.is_none());
        assert_eq!(report.in_bytes, text.len() as u64);
        assert_eq!(
            seen.stdin.lock().unwrap().as_slice(),
            text.as_bytes(),
            "gzip {gzip}"
        );
    }
}

/// A file that stops decompressing part-way must not reach the tool as a complete
/// input (psql would commit a partial single-transaction restore at EOF): the tool is
/// killed while its stdin is still open.
#[tokio::test(flavor = "multi_thread")]
async fn unreadable_input_kills_the_tool_before_closing_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let text: String = (0..200_000)
        .map(|i| format!("INSERT INTO t VALUES ({i}, 'row {i}');\n"))
        .collect();
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(text.as_bytes()).unwrap();
    let mut gz = e.finish().unwrap();
    gz.truncate(gz.len() / 2);
    let input = dir.path().join("cut.sql.gz");
    std::fs::write(&input, &gz).unwrap();
    // The input fails to read at the start too (a missing file).
    for (path, gzip) in [(input, true), (dir.path().join("missing.sql"), false)] {
        let seen = Arc::new(Seen::default());
        let spawner = FakeSpawner {
            script: Script {
                read_stdin: true,
                ..Script::default()
            },
            seen: Arc::clone(&seen),
        };
        let inv = invocation(SecretFileSpec::None, StdoutUse::Discard, true);
        let report = tokio::time::timeout(
            Duration::from_secs(20),
            run_tool(
                &spawner,
                &FakeCtx::default(),
                ToolRun {
                    inv: &inv,
                    password: None,
                    stdin: Some(StdinFeed { path, gzip }),
                    stdout: StdoutSink::Discard,
                },
            ),
        )
        .await
        .expect("the runner must end")
        .unwrap();
        assert!(report.in_error.is_some(), "gzip {gzip}");
        assert!(seen.killed.load(Ordering::SeqCst), "gzip {gzip}");
        assert!(!seen.stdin_eof.load(Ordering::SeqCst), "gzip {gzip}");
        assert_ne!(report.code, Some(0));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn list_capture_is_bounded_and_exit_code_kept() {
    let seen = Arc::new(Seen::default());
    let spawner = FakeSpawner {
        script: Script {
            stdout: vec![b"0123456789".to_vec(), b"abcdefghijklmno".to_vec()],
            code: 2,
            ..Script::default()
        },
        seen,
    };
    let inv = invocation(SecretFileSpec::None, StdoutUse::List, false);
    let report = run_tool(
        &spawner,
        &FakeCtx::default(),
        ToolRun {
            inv: &inv,
            password: None,
            stdin: None,
            stdout: StdoutSink::Capture { limit: 12 },
        },
    )
    .await
    .unwrap();
    assert_eq!(report.code, Some(2));
    assert!(!report.killed);
    assert_eq!(report.captured, b"0123456789ab");
    assert!(report.captured_truncated);
}

#[tokio::test(flavor = "multi_thread")]
async fn long_stderr_lines_are_cut() {
    let seen = Arc::new(Seen::default());
    let long = "x".repeat(MAX_LINE_CHARS * 3);
    let spawner = FakeSpawner {
        script: Script {
            stderr: format!("{long}\nshort\n").into_bytes(),
            ..Script::default()
        },
        seen,
    };
    let ctx = FakeCtx::default();
    let inv = invocation(SecretFileSpec::None, StdoutUse::Discard, false);
    run_tool(
        &spawner,
        &ctx,
        ToolRun {
            inv: &inv,
            password: None,
            stdin: None,
            stdout: StdoutSink::Discard,
        },
    )
    .await
    .unwrap();
    let lines = ctx.lines.lock().unwrap().clone();
    assert_eq!(
        lines.len(),
        2,
        "{:?}",
        lines.iter().map(|l| l.1.len()).collect::<Vec<_>>()
    );
    assert_eq!(lines[0].1.chars().count(), MAX_LINE_CHARS + 1);
    assert!(lines[0].1.ends_with('…'));
    assert_eq!(lines[1].1, "short");
}

/// The password is masked before a line is cut: one crossing the cut leaves no prefix.
#[test]
fn mask_before_cut() {
    let line = format!("{}{PASSWORD} tail", "x".repeat(MAX_LINE_CHARS - 4));
    let logged = log_line(line.as_bytes(), PASSWORD, false).unwrap();
    assert!(!logged.contains("Sup3"), "{logged}");
    assert!(
        logged.ends_with("xxxx**…") || logged.contains("**"),
        "{logged}"
    );
    assert_eq!(logged.chars().count(), MAX_LINE_CHARS + 1);
}

/// A forced chunk of an over-long stderr line (no newline within the limit) that ends
/// inside the password: the password's beginning is masked too, and the rest of the
/// line (holding the password's end) is never logged.
#[tokio::test(flavor = "multi_thread")]
async fn forced_chunk_masks_a_split_password() {
    // The fake's pipe delivers 1 KiB pieces, so the forced flush (past four times
    // MAX_LINE_CHARS bytes) happens at 16 KiB: the password straddles it.
    let mut stderr = "x".repeat(16 * 1024 - 5).into_bytes();
    stderr.extend_from_slice(PASSWORD.as_bytes());
    stderr.extend_from_slice(b" more\nnext\n");
    let seen = Arc::new(Seen::default());
    let spawner = FakeSpawner {
        script: Script {
            stdout: vec![],
            stderr,
            ..Script::default()
        },
        seen,
    };
    let ctx = FakeCtx::default();
    let inv = invocation(SecretFileSpec::None, StdoutUse::Discard, false);
    run_tool(
        &spawner,
        &ctx,
        ToolRun {
            inv: &inv,
            password: Some(&password()),
            stdin: None,
            stdout: StdoutSink::Discard,
        },
    )
    .await
    .unwrap();
    let text = ctx.text();
    for k in 3..=PASSWORD.len() {
        assert!(!text.contains(&PASSWORD[..k]), "prefix {k} leaked");
        assert!(
            !text.contains(&PASSWORD[PASSWORD.len() - k..]),
            "suffix {k} leaked"
        );
    }
    // The long line once (cut), its rest skipped, then the next line.
    let lines = ctx.lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1].1, "next");
    // Directly: a chunk ending with the password's beginning.
    let chunk = format!("abc {}", &PASSWORD[..6]);
    assert_eq!(
        log_line(chunk.as_bytes(), PASSWORD, true).unwrap(),
        "abc ******"
    );
    assert_eq!(
        log_line(chunk.as_bytes(), PASSWORD, false).unwrap(),
        chunk,
        "a whole line ending like that is just text"
    );
}

/// A real process through [`OsSpawner`]: `mysqldump --version` (skipped when the tool
/// is not installed).
#[tokio::test(flavor = "multi_thread")]
async fn real_mysqldump_version_when_installed() {
    let path_env = std::env::var_os("PATH");
    let Some(path) = find_tool(Tool::MysqlDump, None, path_env.as_deref()) else {
        eprintln!("skipped: mysqldump not found on PATH");
        return;
    };
    let info = probe(Tool::MysqlDump, &path).await.unwrap();
    assert!(info.version.contains("Ver"), "{}", info.version);
    let inv = Invocation {
        program: path.clone(),
        args: vec!["--version".into()],
        env_remove: ENV_REMOVE.to_vec(),
        env_set: vec![],
        secret: SecretFileSpec::None,
        pgpass_scope: None,
        stdin_file: false,
        stdout: StdoutUse::List,
    };
    let ctx = FakeCtx::default();
    let report = run_tool(
        &OsSpawner,
        &ctx,
        ToolRun {
            inv: &inv,
            password: None,
            stdin: None,
            stdout: StdoutSink::Capture {
                limit: MAX_LIST_BYTES,
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(report.code, Some(0));
    assert!(String::from_utf8_lossy(&report.captured).contains("Ver"));
    // The tool list finds it with its version.
    let list = list_tools(None, path_env.as_deref()).await;
    let found = list
        .tools
        .iter()
        .find(|t| t.tool == Tool::MysqlDump)
        .expect("mysqldump listed");
    assert_eq!(found.version, info.version);
    assert!(!list.missing.contains(&Tool::MysqlDump));
    assert!(!list.searched.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_tool_is_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let path_env = std::env::join_paths([dir.path()]).unwrap();
    let err = locate(Tool::PgDump, None, Some(&path_env))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("pg_dump was not found"), "{text}");
    let list = list_tools(Some(dir.path()), Some(&path_env)).await;
    assert!(list.tools.is_empty());
    assert_eq!(list.missing, Tool::ALL.to_vec());
    assert_eq!(
        list.searched,
        vec![dir.path().to_string_lossy().into_owned()]
    );
}
