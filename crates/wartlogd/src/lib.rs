//! wartlogd: wires sources to the store writer and serves the API (spec sections 5, 10, 12).

pub mod config;
pub mod ingest;
pub mod manager;
pub mod server;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{broadcast, mpsc, watch};
use wartlog_sources::{CursorStore, Sink, SourceStatus, StatusRegistry};
use wartlog_store::{Writer, WriterOptions};

pub struct RunOptions {
    pub config_path: PathBuf,
    /// overrides `[storage] path`
    pub store: Option<PathBuf>,
    /// overrides the API socket path
    pub api_socket: Option<PathBuf>,
    /// how often the config file is checked for changes
    pub reload_every: Duration,
    /// take over an external store (one a library writer created) instead of refusing it
    pub adopt: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions { config_path: config::default_path(), store: None, api_socket: None, reload_every: Duration::from_secs(2), adopt: false }
    }
}

/// Tells systemd (Type=notify) about readiness and status, when started by it.
pub fn sd_notify(msg: &str) {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else { return };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else { return };
    let p = path.to_string_lossy().to_string();
    let r = if let Some(name) = p.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).and_then(|a| sock.send_to_addr(msg.as_bytes(), &a))
    } else {
        sock.send_to(msg.as_bytes(), &p)
    };
    if let Err(e) = r {
        tracing::debug!("sd_notify: {e}");
    }
}

fn mtime(p: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Binds the API socket, refusing if a live daemon already answers on it.
async fn bind_api(path: &std::path::Path) -> Result<tokio::net::UnixListener> {
    if let Some(d) = path.parent() {
        wartlog_store::create_private_dir(d)?;
    }
    if path.exists() {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            anyhow::bail!("another wartlogd is already serving {}", path.display());
        }
        let _ = std::fs::remove_file(path);
    }
    let l = tokio::net::UnixListener::bind(path).with_context(|| format!("cannot bind {}", path.display()))?;
    wartlog_store::set_private(path);
    Ok(l)
}

/// Runs the daemon until `shutdown` turns true.
pub async fn run(opts: RunOptions, mut shutdown: watch::Receiver<bool>) -> Result<()> {
    let cfg = config::Config::load(&opts.config_path)?;
    let root = opts.store.clone().unwrap_or_else(|| cfg.storage.path.clone());
    wartlog_store::create_private_dir(&root)?;
    let wopts = WriterOptions {
        mode: wartlog_store::WriteMode::Daemon,
        adopt: opts.adopt,
        creator: format!("wartlogd {}", env!("CARGO_PKG_VERSION")),
        retention_max_bytes: cfg.storage.max_size,
        retention_max_age: cfg.storage.max_age,
        ..Default::default()
    };
    let writer = Writer::open(&root, wopts)?;
    let recovered = writer.stats().recovered_from_wal;
    tracing::info!("store {} opened ({} records recovered from the WAL)", root.display(), recovered);

    let api_path = opts.api_socket.clone().unwrap_or_else(wartlog_api::api_socket_path);
    let listener = bind_api(&api_path).await?;

    let (tx, rx) = mpsc::channel(64);
    let (tail, _) = broadcast::channel(16384);
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let stats = Arc::new(ingest::IngestStats::default());
    let ing = ingest::Ingest { writer, rx, cmds: cmd_rx, tail: tail.clone(), stats: stats.clone() };
    let rt = tokio::runtime::Handle::current();
    let writer_thread = std::thread::Builder::new().name("wartlog-writer".into()).spawn(move || ing.run(rt))?;

    let registry = StatusRegistry::default();
    let sink = Sink::new(tx, Arc::new(SourceStatus::default()));
    let mut mgr = manager::Manager::new(sink, CursorStore::open(&root), registry);
    mgr.apply(&cfg.sources);
    let sh = Arc::new(server::Shared {
        root: root.clone(),
        config_path: opts.config_path.clone(),
        config_error: Mutex::new(None),
        manager: tokio::sync::Mutex::new(mgr),
        ingest: stats,
        cmds: Mutex::new(cmd_tx),
        tail,
        started: Instant::now(),
        reload: tokio::sync::Notify::new(),
    });
    let api = tokio::spawn(server::serve(listener, sh.clone(), shutdown.clone()));
    tracing::info!("API listening on {}", api_path.display());
    sd_notify(&format!("READY=1\nSTATUS=ingesting into {}", root.display()));

    // config reload loop
    let mut seen = mtime(&opts.config_path);
    let mut storage = cfg.storage.clone();
    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { break },
            _ = tokio::time::sleep(opts.reload_every) => {}
            _ = sh.reload.notified() => {}
        }
        let now = mtime(&opts.config_path);
        if now == seen {
            continue;
        }
        seen = now;
        match config::Config::load(&opts.config_path) {
            Ok(c) => {
                tracing::info!("config reloaded");
                *sh.config_error.lock().unwrap() = None;
                if c.storage != storage {
                    tracing::warn!("[storage] changes take effect when wartlogd restarts");
                    storage = c.storage.clone();
                }
                sh.manager.lock().await.apply(&c.sources);
            }
            Err(e) => {
                tracing::error!("config not reloaded: {e:#}");
                *sh.config_error.lock().unwrap() = Some(format!("{e:#}"));
            }
        }
    }

    sd_notify("STOPPING=1");
    tracing::info!("shutting down");
    let handles = sh.manager.lock().await.stop_all();
    for h in handles {
        let ab = h.abort_handle();
        if tokio::time::timeout(Duration::from_secs(5), h).await.is_err() {
            ab.abort();
        }
    }
    let _ = sh.cmds.lock().unwrap().send(ingest::Cmd::Shutdown);
    let _ = api.await;
    let _ = std::fs::remove_file(&api_path);
    tokio::task::spawn_blocking(move || writer_thread.join()).await?.map_err(|_| anyhow::anyhow!("writer thread panicked"))??;
    tracing::info!("stopped");
    Ok(())
}
