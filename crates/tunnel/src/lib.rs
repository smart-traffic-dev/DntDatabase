//! SSH local port-forward (russh) with an app-owned `known_hosts` file.

mod auth;
mod known_hosts;

pub use auth::KEY_PASSPHRASE_REQUIRED;
pub use known_hosts::{fingerprint, HostKeyStatus, KnownHosts};

use dnt_core::config::{SshAuth, SshConfig};
use dnt_core::error::{ConnectStage, DbError, SshStage};
use russh::client::{self, Handle};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::Disconnect;
use secrecy::{ExposeSecret, SecretString};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Notify};
use tokio::task::{JoinHandle, JoinSet};

/// Secrets needed to open a tunnel; only the one matching `SshConfig::auth` is used.
#[derive(Default)]
pub struct SshSecrets {
    pub password: Option<SecretString>,
    pub key_passphrase: Option<SecretString>,
}

/// Tuning knobs for [`Tunnel::open_with`]; [`Tunnel::open`] uses the defaults.
#[derive(Debug, Clone)]
pub struct TunnelOptions {
    /// Keepalive sent when the server has been silent this long (default 30 s).
    pub keepalive_interval: Duration,
}

impl Default for TunnelOptions {
    fn default() -> Self {
        TunnelOptions {
            keepalive_interval: Duration::from_secs(30),
        }
    }
}

/// Unanswered keepalives before the session is considered dead.
const KEEPALIVE_MAX: usize = 3;

/// Marks the tunnel dead when dropped. It lives inside the russh handler, which russh
/// drops when the session ends for any reason (disconnect, I/O error, keepalive timeout).
struct AliveGuard {
    alive: Arc<AtomicBool>,
    dead: Arc<Notify>,
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        self.dead.notify_one();
    }
}

struct Client {
    known: KnownHosts,
    host: String,
    port: u16,
    /// Why the server key was rejected; read by `open` after russh reports the failure.
    rejection: Arc<Mutex<Option<DbError>>>,
    _alive: AliveGuard,
}

impl Client {
    fn verify(&self, server_key: &PublicKeyOrCertificate) -> Result<(), DbError> {
        let key = server_key.public_key();
        match self.known.check(&self.host, self.port, &key)? {
            HostKeyStatus::Known => Ok(()),
            HostKeyStatus::Unknown { fingerprint } => Err(DbError::SshHostKeyUnknown {
                host: self.host.clone(),
                port: self.port,
                fingerprint,
                key_openssh: known_hosts::openssh_line(&key)?,
            }),
            HostKeyStatus::Mismatch { expected, actual } => Err(DbError::Ssh {
                stage: SshStage::HostKeyMismatch,
                message: format!(
                    "the host key of [{}]:{} changed: trusted {expected}, server sent {actual}",
                    self.host, self.port
                ),
            }),
        }
    }
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        match self.verify(server_public_key) {
            Ok(()) => Ok(true),
            Err(e) => {
                *self.rejection.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                Ok(false)
            }
        }
    }
}

fn ssh_err(stage: SshStage, message: impl Into<String>) -> DbError {
    DbError::Ssh {
        stage,
        message: message.into(),
    }
}

pub struct Tunnel {
    local_addr: SocketAddr,
    alive: Arc<AtomicBool>,
    /// Why the SSH server last refused to open a forwarding channel (set by `forward`).
    channel_error: Arc<Mutex<Option<String>>>,
    handle: Arc<Handle<Client>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Tunnel {
    /// Connects to the SSH server, verifies its host key against `known`, authenticates and
    /// starts forwarding `127.0.0.1:<random port>` to `target` (as resolved by the server).
    pub async fn open(
        cfg: &SshConfig,
        secrets: SshSecrets,
        target: (String, u16),
        known: &KnownHosts,
        timeout: Duration,
    ) -> Result<Tunnel, DbError> {
        Self::open_with(
            cfg,
            secrets,
            target,
            known,
            timeout,
            TunnelOptions::default(),
        )
        .await
    }

    pub async fn open_with(
        cfg: &SshConfig,
        secrets: SshSecrets,
        target: (String, u16),
        known: &KnownHosts,
        timeout: Duration,
        opts: TunnelOptions,
    ) -> Result<Tunnel, DbError> {
        let alive = Arc::new(AtomicBool::new(true));
        let dead = Arc::new(Notify::new());
        let session = Self::connect(cfg, secrets, known, &opts, &alive, &dead);
        let handle =
            tokio::time::timeout(timeout, session)
                .await
                .map_err(|_| DbError::Timeout {
                    stage: ConnectStage::OpenTunnel,
                })??;
        let handle = Arc::new(handle);

        let listener = match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await {
            Ok(l) => l,
            Err(e) => {
                disconnect(&handle).await;
                return Err(ssh_err(
                    SshStage::Channel,
                    format!("cannot listen on a local port: {e}"),
                ));
            }
        };
        let local_addr = listener.local_addr().map_err(|e| DbError::Internal {
            message: format!("local tunnel address: {e}"),
        })?;
        let (stop, stopped) = oneshot::channel();
        let channel_error = Arc::new(Mutex::new(None));
        let task = tokio::spawn(accept_loop(
            listener,
            Arc::clone(&handle),
            target,
            stopped,
            dead,
            Arc::clone(&channel_error),
        ));
        Ok(Tunnel {
            local_addr,
            alive,
            channel_error,
            handle,
            stop: Some(stop),
            task: Some(task),
        })
    }

    async fn connect(
        cfg: &SshConfig,
        secrets: SshSecrets,
        known: &KnownHosts,
        opts: &TunnelOptions,
        alive: &Arc<AtomicBool>,
        dead: &Arc<Notify>,
    ) -> Result<Handle<Client>, DbError> {
        // Load the key before dialing so file/passphrase problems surface immediately.
        let key = match cfg.auth {
            SshAuth::Key => Some(auth::load_private_key(
                cfg.key_path.as_deref(),
                secrets.key_passphrase.as_ref(),
            )?),
            SshAuth::Password => None,
        };
        let rejection = Arc::new(Mutex::new(None));
        let handler = Client {
            known: known.clone(),
            host: cfg.host.clone(),
            port: cfg.port,
            rejection: Arc::clone(&rejection),
            _alive: AliveGuard {
                alive: Arc::clone(alive),
                dead: Arc::clone(dead),
            },
        };
        let config = Arc::new(client::Config {
            keepalive_interval: Some(opts.keepalive_interval),
            keepalive_max: KEEPALIVE_MAX,
            inactivity_timeout: None,
            ..Default::default()
        });
        let mut handle = client::connect(config, (cfg.host.as_str(), cfg.port), handler)
            .await
            .map_err(|e| {
                rejection
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take()
                    .unwrap_or_else(|| ssh_err(SshStage::Connect, e.to_string()))
            })?;

        let result = match key {
            Some(key) => {
                let hash = if key.algorithm().is_rsa() {
                    handle
                        .best_supported_rsa_hash()
                        .await
                        .ok()
                        .flatten()
                        .flatten()
                } else {
                    None
                };
                let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
                handle.authenticate_publickey(&cfg.user, key).await
            }
            None => {
                let Some(password) = secrets.password.as_ref() else {
                    disconnect(&handle).await;
                    return Err(auth::auth_err("an SSH password is required"));
                };
                handle
                    .authenticate_password(&cfg.user, password.expose_secret())
                    .await
            }
        };
        match result {
            Ok(r) if r.success() => Ok(handle),
            Ok(_) => {
                disconnect(&handle).await;
                Err(auth::auth_err(format!(
                    "the server rejected the SSH credentials of user {}",
                    cfg.user
                )))
            }
            Err(e) => Err(auth::auth_err(e.to_string())),
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Why the SSH server last refused a forwarding channel (e.g. the database host or
    /// port is unreachable from it), if it did since the tunnel opened.
    pub fn last_channel_error(&self) -> Option<String> {
        self.channel_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// False once the SSH session has ended (server gone, keepalive failure, closed).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && !self.handle.is_closed()
    }

    /// Stops accepting, drops forwarded connections and disconnects the SSH session.
    pub async fn close(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        disconnect(&self.handle).await;
        self.alive.store(false, Ordering::SeqCst);
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // Not closed explicitly: stop the accept loop; the session ends with its handle.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn disconnect(handle: &Handle<Client>) {
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
}

async fn accept_loop(
    listener: TcpListener,
    handle: Arc<Handle<Client>>,
    target: (String, u16),
    mut stop: oneshot::Receiver<()>,
    dead: Arc<Notify>,
    channel_error: Arc<Mutex<Option<String>>>,
) {
    // Dropping the set (when the loop ends) aborts every in-flight forward.
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = dead.notified() => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok((tcp, peer)) => {
                    conns.spawn(forward(
                        Arc::clone(&handle),
                        target.clone(),
                        tcp,
                        peer,
                        Arc::clone(&channel_error),
                    ));
                }
                // Transient (e.g. out of sockets): back off instead of spinning.
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            },
        }
    }
}

/// Copies one accepted connection over its own `direct-tcpip` channel. A refused channel
/// is recorded in `channel_error` before the client's connection is dropped.
async fn forward(
    handle: Arc<Handle<Client>>,
    (host, port): (String, u16),
    mut tcp: TcpStream,
    peer: SocketAddr,
    channel_error: Arc<Mutex<Option<String>>>,
) {
    let channel = match handle
        .channel_open_direct_tcpip(
            host,
            u32::from(port),
            peer.ip().to_string(),
            u32::from(peer.port()),
        )
        .await
    {
        Ok(c) => c,
        // Dropping `tcp` closes the client's connection, which it reports as an error.
        Err(e) => {
            *channel_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
            return;
        }
    };
    let mut stream = channel.into_stream();
    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
}
