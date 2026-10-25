//! N7 external tools, the process half (spec §7.2, R25–R29): finding and probing the
//! whitelisted tools, writing their secret file, spawning them and plumbing their
//! stdin/stdout/stderr, Stop (kill) and the job log. Arguments come from
//! `dnt_transfer::tools` only.
//!
//! - **Discovery** ([`locate`], [`list_tools`]): `dnt_transfer::tools::find_tool` (the
//!   configured folder, then each absolute `PATH` entry), then `<tool> --version`
//!   bounded by [`VERSION_TIMEOUT`].
//! - **Secrets** (R27): the option file / pgpass file is written `create_new` into the
//!   job's temporary folder (`<data_dir>/job-tmp/<job id>/`, inside the user's profile)
//!   just before the spawn and removed as soon as the tool exited or was killed (the job
//!   folder itself is removed when the job ends, and leftovers at the next startup). The
//!   password is read from the runtime's `ResolvedConfig` here only; it never reaches an
//!   argument, an environment value, the shown command line or a log line (stderr lines
//!   are masked with [`dnt_transfer::tools::mask`]).
//! - **Process**: `tokio::process::Command` with `kill_on_drop`, `CREATE_NO_WINDOW` on
//!   Windows, a cleaned environment, stdin fed from the restore file (decompressed when
//!   gzip) or closed, stdout into the output file / captured (bounded) / drained, stderr
//!   line by line into the job log as server-like lines (persisted as `Server error`
//!   only, R6: a tool can echo statement text).
//! - **Stop** = kill: the job's kill hook wakes the runner, which kills the child.
use crate::ddl_exec::stopped;
use crate::jobs::{JobCtx, JobLogLevel, ProgressUnit};
use crate::result_store::block_outside_executor;
use crate::session_manager::{config_error, with_database, ConnectionRuntime};
use async_trait::async_trait;
use dnt_core::config::ResolvedConfig;
use dnt_core::error::DbError;
use dnt_transfer::tools::{
    find_tool, is_scrubbed_env, mask, mysql_option_file, open_input, pgpass_line, search_dirs,
    tool_info, Invocation, SecretFileSpec, Tool, ToolError, ToolInfo, ToolOutput,
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;

/// Bound of `<tool> --version` (R25).
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Most of a captured stdout (`pg_restore --list`) kept.
pub const MAX_LIST_BYTES: usize = 4 * 1024 * 1024;
/// Longest stderr line logged (characters); longer ones are cut with `…`.
pub const MAX_LINE_CHARS: usize = 4000;
/// The secret files inside the job's temporary folder.
pub const MYSQL_SECRET_FILE: &str = "client.cnf";
pub const PGPASS_FILE: &str = "pgpass.conf";

const CHUNK: usize = 64 * 1024;
/// `CREATE_NO_WINDOW`: a console tool started from the GUI opens no console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// What `tools_list` returns: the tools found (with their version), the ones missing,
/// and the folders searched in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ToolsList {
    pub tools: Vec<ToolInfo>,
    pub missing: Vec<Tool>,
    pub searched: Vec<String>,
}

pub(crate) fn tool_error(e: ToolError) -> DbError {
    config_error("tool", &e.to_string())
}

/// A command for a whitelisted tool: killed when dropped, no console window, and an
/// environment without the database clients' variables (`PG*`, `MYSQL_*`, `LIBMYSQL_*`,
/// `MARIADB_*`; `SystemRoot`, `PATH`, `TEMP`… are kept).
fn base_command(program: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    for (k, _) in std::env::vars_os() {
        if is_scrubbed_env(&k) {
            cmd.env_remove(k);
        }
    }
    for k in dnt_transfer::tools::ENV_REMOVE {
        cmd.env_remove(k);
    }
    cmd
}

/// Most of `--version`'s output read (the first line is all that is used).
const MAX_PROBE_BYTES: u64 = 64 * 1024;

/// `<tool> --version` (bounded by [`VERSION_TIMEOUT`], output by [`MAX_PROBE_BYTES`])
/// → its [`ToolInfo`].
pub async fn probe(tool: Tool, path: &Path) -> Result<ToolInfo, DbError> {
    let mut cmd = base_command(path);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let name = tool.name();
    let run = async {
        let mut child = cmd
            .spawn()
            .map_err(|e| config_error("tool", &format!("Could not start {name}: {e}.")))?;
        let mut out = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            // A tool printing more than the cap gets a closed pipe and exits.
            let _ = stdout.take(MAX_PROBE_BYTES).read_to_end(&mut out).await;
        }
        let status = child
            .wait()
            .await
            .map_err(|e| config_error("tool", &format!("Could not run {name}: {e}.")))?;
        Ok::<_, DbError>((status, out))
    };
    let (status, out) = tokio::time::timeout(VERSION_TIMEOUT, run)
        .await
        .map_err(|_| {
            config_error(
                "tool",
                &format!("{name} did not answer --version within 5 seconds."),
            )
        })??;
    if !status.success() {
        return Err(config_error(
            "tool",
            &format!("{name} --version failed ({status})."),
        ));
    }
    Ok(tool_info(tool, path, &String::from_utf8_lossy(&out)))
}

/// The tool found in `configured` / `path_env`, probed.
pub async fn locate(
    tool: Tool,
    configured: Option<&Path>,
    path_env: Option<&OsStr>,
) -> Result<ToolInfo, DbError> {
    let Some(path) = find_tool(tool, configured, path_env) else {
        return Err(config_error(
            "tool",
            &format!(
                "{} was not found. Install it, or set the External tools folder in Settings.",
                tool.name()
            ),
        ));
    };
    probe(tool, &path).await
}

/// Every whitelisted tool: found and answering `--version`, or missing. The tools are
/// probed concurrently (the list waits at most about one [`VERSION_TIMEOUT`]).
pub async fn list_tools(configured: Option<&Path>, path_env: Option<&OsStr>) -> ToolsList {
    let mut list = ToolsList {
        tools: Vec::new(),
        missing: Vec::new(),
        searched: search_dirs(configured, path_env)
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect(),
    };
    let probed = futures_util::future::join_all(
        Tool::ALL
            .into_iter()
            .map(|tool| async move { (tool, locate(tool, configured, path_env).await) }),
    )
    .await;
    for (tool, found) in probed {
        match found {
            Ok(info) => list.tools.push(info),
            Err(e) => {
                if find_tool(tool, configured, path_env).is_some() {
                    tracing::warn!(tool = tool.name(), error = %e, "external tool found but not usable");
                }
                list.missing.push(tool);
            }
        }
    }
    list
}

impl ConnectionRuntime {
    /// Where a tool connects now (the tunnel's local port through SSH, reopening a dead
    /// tunnel first) on `database`, with the password; and whether it is tunnelled.
    pub(crate) async fn tool_endpoint(
        &self,
        database: Option<String>,
    ) -> Result<(ResolvedConfig, bool), DbError> {
        let mut link = self.link.lock().await;
        self.ensure_open(&link)?;
        if self.tunnel_dead() {
            self.replace_dead_tunnel(&mut link).await?;
        }
        Ok((
            with_database(&link.resolved, database),
            self.cfg.ssh.is_some(),
        ))
    }
}

// ---------------------------------------------------------------------------------------
// Spawning
// ---------------------------------------------------------------------------------------

pub type ReadPipe = Pin<Box<dyn AsyncRead + Send>>;
pub type WritePipe = Pin<Box<dyn AsyncWrite + Send>>;

/// A running tool.
#[async_trait]
pub trait ToolChild: Send {
    fn take_stdin(&mut self) -> Option<WritePipe>;
    fn take_stdout(&mut self) -> Option<ReadPipe>;
    fn take_stderr(&mut self) -> Option<ReadPipe>;
    /// The exit code once it exited (`None`: ended by a signal).
    async fn wait(&mut self) -> io::Result<Option<i32>>;
    async fn kill(&mut self) -> io::Result<()>;
}

/// Starts a tool; [`OsSpawner`] in the app, a fake in tests.
pub trait Spawner: Send + Sync {
    fn spawn(&self, inv: &Invocation) -> io::Result<Box<dyn ToolChild>>;
}

/// Real processes (`tokio::process`).
pub struct OsSpawner;

struct OsChild(tokio::process::Child);

#[async_trait]
impl ToolChild for OsChild {
    fn take_stdin(&mut self) -> Option<WritePipe> {
        self.0.stdin.take().map(|p| Box::pin(p) as WritePipe)
    }
    fn take_stdout(&mut self) -> Option<ReadPipe> {
        self.0.stdout.take().map(|p| Box::pin(p) as ReadPipe)
    }
    fn take_stderr(&mut self) -> Option<ReadPipe> {
        self.0.stderr.take().map(|p| Box::pin(p) as ReadPipe)
    }
    async fn wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self.0.wait().await?.code())
    }
    async fn kill(&mut self) -> io::Result<()> {
        self.0.kill().await
    }
}

impl Spawner for OsSpawner {
    fn spawn(&self, inv: &Invocation) -> io::Result<Box<dyn ToolChild>> {
        let mut cmd = base_command(&inv.program);
        cmd.args(&inv.args)
            .stdin(if inv.stdin_file {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for k in &inv.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &inv.env_set {
            cmd.env(k, v);
        }
        Ok(Box::new(OsChild(cmd.spawn()?)))
    }
}

// ---------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------

/// What the runner reports to (a job, or a fake in tests).
pub trait RunCtx: Send + Sync {
    /// A stderr line (masked).
    fn tool_line(&self, level: JobLogLevel, line: String);
    /// The kill hook (`None` once the tool exited); called at once when already stopped.
    fn set_kill(&self, kill: Option<Box<dyn Fn() + Send + Sync>>);
    fn is_stopped(&self) -> bool;
    /// Bytes of stdout written so far.
    fn bytes_out(&self, bytes: u64);
}

impl RunCtx for JobCtx {
    fn tool_line(&self, level: JobLogLevel, line: String) {
        self.log_server(level, line, None);
    }
    fn set_kill(&self, kill: Option<Box<dyn Fn() + Send + Sync>>) {
        JobCtx::set_kill(self, kill);
    }
    fn is_stopped(&self) -> bool {
        JobCtx::is_stopped(self)
    }
    fn bytes_out(&self, bytes: u64) {
        self.progress("Writing", bytes, None, ProgressUnit::Bytes);
    }
}

/// The file fed to the tool's stdin.
pub struct StdinFeed {
    pub path: PathBuf,
    pub gzip: bool,
}

/// Where the tool's stdout goes.
pub enum StdoutSink {
    File(Box<ToolOutput>),
    Capture { limit: usize },
    Discard,
}

pub struct ToolRun<'a> {
    pub inv: &'a Invocation,
    /// Written to the secret file only.
    pub password: Option<&'a SecretString>,
    pub stdin: Option<StdinFeed>,
    pub stdout: StdoutSink,
}

#[derive(Debug, Default)]
pub struct RunReport {
    pub code: Option<i32>,
    /// Killed by Stop.
    pub killed: bool,
    pub captured: Vec<u8>,
    pub captured_truncated: bool,
    /// Bytes written to the output file.
    pub out_bytes: u64,
    /// Writing the output file failed (the tool was killed then).
    pub out_error: Option<String>,
    pub in_bytes: u64,
    /// Reading the input file failed (stdin was closed then).
    pub in_error: Option<String>,
}

/// The secret file of one run, removed when dropped.
struct SecretFile(Option<PathBuf>);

impl SecretFile {
    fn write(inv: &Invocation, password: Option<&str>) -> Result<SecretFile, DbError> {
        let (path, contents) = match &inv.secret {
            SecretFileSpec::None => return Ok(SecretFile(None)),
            SecretFileSpec::MysqlOptionFile(p) => (
                p,
                match password {
                    Some(pw) => mysql_option_file(pw).map_err(tool_error)?,
                    None => "[client]\n".to_string(),
                },
            ),
            SecretFileSpec::PgPass(p) => (
                p,
                match (password, &inv.pgpass_scope) {
                    (Some(pw), Some(scope)) => pgpass_line(scope, pw).map_err(tool_error)?,
                    (Some(_), None) => {
                        return Err(DbError::Internal {
                            message: "a pgpass file without the server it is for".into(),
                        })
                    }
                    (None, _) => String::new(),
                },
            ),
        };
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(path).map_err(|e| {
            config_error(
                "tool",
                &format!("Could not create the tool's password file: {e}."),
            )
        })?;
        let guard = SecretFile(Some(path.clone()));
        file.write_all(contents.as_bytes())
            .and_then(|_| file.flush())
            .map_err(|e| {
                config_error(
                    "tool",
                    &format!("Could not write the tool's password file: {e}."),
                )
            })?;
        Ok(guard)
    }
}

impl Drop for SecretFile {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn level_of(line: &str) -> JobLogLevel {
    let l = line.to_ascii_lowercase();
    if l.contains("error") || l.contains("fatal") {
        JobLogLevel::Error
    } else if l.contains("warning") {
        JobLogLevel::Warn
    } else {
        JobLogLevel::Info
    }
}

fn cut(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let mut s: String = line.chars().take(MAX_LINE_CHARS).collect();
    s.push('…');
    s
}

/// `bytes` without a trailing part that begins the password (a chunk cut inside the
/// password), and whether one was removed.
fn strip_password_prefix<'a>(bytes: &'a [u8], password: &str) -> (&'a [u8], bool) {
    let pw = password.as_bytes();
    for k in (1..pw.len()).rev() {
        if bytes.ends_with(&pw[..k]) {
            return (&bytes[..bytes.len() - k], true);
        }
    }
    (bytes, false)
}

/// One stderr line for the log: masked first (so a password crossing the cut is still
/// masked), then cut to [`MAX_LINE_CHARS`]. `partial`: the bytes are a forced chunk of
/// a longer line, whose end may hold the beginning of the password.
fn log_line(bytes: &[u8], password: &str, partial: bool) -> Option<String> {
    let (bytes, stripped) = if partial && !password.is_empty() {
        strip_password_prefix(bytes, password)
    } else {
        (bytes, false)
    };
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches(['\r', '\n']);
    let mut masked = mask(text, password);
    if stripped {
        masked.push_str("******");
    }
    if masked.trim().is_empty() {
        return None;
    }
    Some(cut(&masked))
}

/// Logs stderr line by line, masked; a line longer than four times
/// [`MAX_LINE_CHARS`] bytes is logged cut and the rest of it skipped.
async fn read_stderr(mut pipe: ReadPipe, ctx: &dyn RunCtx, password: &str) {
    let max_pending = MAX_LINE_CHARS * 4;
    let mut pending: Vec<u8> = Vec::new();
    let mut skipping = false;
    let mut buf = vec![0u8; 8 * 1024];
    let emit = |bytes: &[u8], partial: bool| {
        if let Some(line) = log_line(bytes, password, partial) {
            ctx.tool_line(level_of(&line), line);
        }
    };
    loop {
        let n = match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            if skipping {
                skipping = false;
            } else {
                emit(&line, false);
            }
        }
        if pending.len() > max_pending {
            if !skipping {
                emit(&pending, true);
                skipping = true;
            }
            pending.clear();
        }
    }
    if !skipping && !pending.is_empty() {
        emit(&pending, false);
    }
}

struct StdoutDone {
    captured: Vec<u8>,
    truncated: bool,
    bytes: u64,
    error: Option<String>,
}

/// Reads stdout to the end into `sink`. A write error kills the tool (through `abort`)
/// and keeps draining so it never blocks on a full pipe.
async fn read_stdout(
    mut pipe: ReadPipe,
    sink: StdoutSink,
    ctx: &dyn RunCtx,
    abort: &Notify,
) -> StdoutDone {
    let mut done = StdoutDone {
        captured: Vec::new(),
        truncated: false,
        bytes: 0,
        error: None,
    };
    let (mut file, capture) = match sink {
        StdoutSink::File(out) => (Some(*out), None),
        StdoutSink::Capture { limit } => (None, Some(limit)),
        StdoutSink::Discard => (None, None),
    };
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match pipe.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                done.error.get_or_insert_with(|| e.to_string());
                abort.notify_one();
                break;
            }
        };
        let data = &buf[..n];
        if done.error.is_some() {
            continue;
        }
        if let Some(out) = file.as_mut() {
            match block_outside_executor(|| out.write(data)) {
                Ok(()) => {
                    done.bytes += n as u64;
                    ctx.bytes_out(done.bytes);
                }
                Err(e) => {
                    done.error = Some(e.to_string());
                    abort.notify_one();
                }
            }
        } else if let Some(limit) = capture {
            let room = limit.saturating_sub(done.captured.len());
            if n > room {
                done.truncated = true;
            }
            done.captured.extend_from_slice(&data[..n.min(room)]);
        }
    }
    if let Some(out) = file {
        if let Err(e) = block_outside_executor(|| out.finish()) {
            done.error.get_or_insert_with(|| e.to_string());
        }
    }
    done
}

struct StdinDone {
    bytes: u64,
    error: Option<String>,
}

/// Feeds the file (decompressed when gzip) to the tool's stdin, then closes it. A read
/// error closes stdin early and is reported; a write error means the tool stopped
/// reading (it exited), which its exit code reports.
async fn feed_stdin(mut pipe: WritePipe, feed: StdinFeed) -> StdinDone {
    let mut done = StdinDone {
        bytes: 0,
        error: None,
    };
    let opened = block_outside_executor(|| open_input(&feed.path, feed.gzip));
    let mut input = match opened {
        Ok(r) => r,
        Err(e) => {
            done.error = Some(e.to_string());
            return done;
        }
    };
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = match block_outside_executor(|| input.read(&mut buf)) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                done.error = Some(e.to_string());
                break;
            }
        };
        if pipe.write_all(&buf[..n]).await.is_err() {
            break;
        }
        done.bytes += n as u64;
    }
    let _ = pipe.shutdown().await;
    done
}

/// Runs one invocation: writes its secret file, spawns it, plumbs its pipes until it
/// exits (or Stop kills it), removes the secret file. Nothing is spawned after Stop.
pub async fn run_tool(
    spawner: &dyn Spawner,
    ctx: &dyn RunCtx,
    run: ToolRun<'_>,
) -> Result<RunReport, DbError> {
    if ctx.is_stopped() {
        return Err(stopped());
    }
    let ToolRun {
        inv,
        password,
        stdin,
        stdout,
    } = run;
    let password = password.map(|p| p.expose_secret());
    let secret = SecretFile::write(inv, password)?;
    let program = inv
        .program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut child = spawner
        .spawn(inv)
        .map_err(|e| config_error("tool", &format!("Could not start {program}: {e}.")))?;
    let wake = Arc::new(Notify::new());
    let stop_flag = Arc::new(AtomicBool::new(false));
    {
        let wake = Arc::clone(&wake);
        let stop_flag = Arc::clone(&stop_flag);
        ctx.set_kill(Some(Box::new(move || {
            stop_flag.store(true, Ordering::SeqCst);
            wake.notify_one();
        })));
    }
    let stdin_pipe = child.take_stdin();
    let stdout_pipe = child.take_stdout();
    let stderr_pipe = child.take_stderr();
    let abort: &Notify = &wake;
    let stdin_fut = async move {
        match (stdin_pipe, stdin) {
            (Some(pipe), Some(feed)) => Some(feed_stdin(pipe, feed).await),
            // No input: stdin is closed at once.
            (Some(mut pipe), None) => {
                let _ = pipe.shutdown().await;
                None
            }
            _ => None,
        }
    };
    let stdout_fut = async move {
        match stdout_pipe {
            Some(pipe) => Some(read_stdout(pipe, stdout, ctx, abort).await),
            None => None,
        }
    };
    let stderr_fut = async move {
        if let Some(pipe) = stderr_pipe {
            read_stderr(pipe, ctx, password.unwrap_or("")).await;
        }
    };
    let wait_fut = async {
        let status = tokio::select! {
            status = child.wait() => status,
            _ = wake.notified() => {
                let _ = child.kill().await;
                child.wait().await
            }
        };
        ctx.set_kill(None);
        status
    };
    let (status, stdin_done, stdout_done, ()) =
        tokio::join!(wait_fut, stdin_fut, stdout_fut, stderr_fut);
    drop(secret);
    let code =
        status.map_err(|e| config_error("tool", &format!("Waiting for {program} failed: {e}.")))?;
    let mut report = RunReport {
        code,
        killed: stop_flag.load(Ordering::SeqCst),
        ..RunReport::default()
    };
    if let Some(d) = stdout_done {
        report.captured = d.captured;
        report.captured_truncated = d.truncated;
        report.out_bytes = d.bytes;
        report.out_error = d.error;
    }
    if let Some(d) = stdin_done {
        report.in_bytes = d.bytes;
        report.in_error = d.error;
    }
    Ok(report)
}

#[cfg(test)]
#[path = "ext_tools_tests.rs"]
mod tests;
