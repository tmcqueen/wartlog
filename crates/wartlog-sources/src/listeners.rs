//! NDJSON over stream sockets: UNIX socket and TCP. One record per line.

use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::net::{TcpListener, UnixListener};
use wartlog_model::time::now_ns;

use crate::{ndjson, shutdown_signal, Receiver, Shutdown, Sink};

/// Longest accepted line; longer lines are cut (and counted as errors).
pub const MAX_LINE: usize = 1 << 20;
const BATCH: usize = 512;

/// First line of a connection that names its source.
pub const PREAMBLE: &str = "@wartlog source=";

/// Reads NDJSON lines from a connection until EOF, batching into the sink.
///
/// A connection may start with a preamble line `@wartlog source=NAME` (what `wl send --source`
/// writes) to name the source of its lines.
pub async fn pump<R: AsyncRead + Unpin>(r: R, source: &str, sink: &Sink) -> anyhow::Result<u64> {
    let mut rd = BufReader::with_capacity(64 * 1024, r);
    let mut batch = Vec::with_capacity(BATCH);
    let mut line = Vec::new();
    let mut n = 0u64;
    let mut source = source.to_string();
    let mut first = true;
    loop {
        line.clear();
        let got = (&mut rd).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line).await?;
        if got == 0 {
            break;
        }
        if line.len() > MAX_LINE {
            sink.status.error("line longer than 1 MiB was cut");
            // skip the rest of the long line
            let mut rest = Vec::new();
            rd.read_until(b'\n', &mut rest).await?;
            line.truncate(MAX_LINE);
        }
        let text = String::from_utf8_lossy(&line);
        if std::mem::take(&mut first) {
            if let Some(name) = text.trim_end().strip_prefix(PREAMBLE) {
                source = name.trim().to_string();
                continue;
            }
        }
        if text.trim().is_empty() {
            continue;
        }
        batch.push(ndjson::normalize(&text, &source, now_ns()));
        n += 1;
        if batch.len() >= BATCH || rd.buffer().is_empty() {
            sink.send(std::mem::take(&mut batch)).await?;
        }
    }
    sink.send(batch).await?;
    Ok(n)
}

use tokio::io::AsyncReadExt as _;

pub struct UnixReceiver {
    pub name: String,
    pub path: PathBuf,
    /// source name for records that do not name one
    pub source: String,
}

#[async_trait::async_trait]
impl Receiver for UnixReceiver {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(self: Box<Self>, sink: Sink, shutdown: Shutdown) -> anyhow::Result<()> {
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
            }
        }
        let _ = std::fs::remove_file(&self.path);
        let l = UnixListener::bind(&self.path)?;
        tracing::info!("NDJSON receiver listening on {}", self.path.display());
        let stop = shutdown_signal(shutdown);
        tokio::pin!(stop);
        loop {
            tokio::select! {
                _ = &mut stop => break,
                acc = l.accept() => {
                    let (s, _) = acc?;
                    let sink = sink.clone();
                    let src = self.source.clone();
                    tokio::spawn(async move {
                        if let Err(e) = pump(s, &src, &sink).await {
                            sink.status.error(e);
                        }
                    });
                }
            }
        }
        let _ = std::fs::remove_file(&self.path);
        Ok(())
    }
}

pub struct TcpReceiver {
    pub name: String,
    pub listen: String,
    pub source: String,
}

#[async_trait::async_trait]
impl Receiver for TcpReceiver {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(self: Box<Self>, sink: Sink, shutdown: Shutdown) -> anyhow::Result<()> {
        let l = TcpListener::bind(&self.listen).await.map_err(|e| anyhow::anyhow!("cannot listen on {}: {e}", self.listen))?;
        warn_if_public(&self.listen);
        tracing::info!("NDJSON receiver listening on tcp {}", self.listen);
        let stop = shutdown_signal(shutdown);
        tokio::pin!(stop);
        loop {
            tokio::select! {
                _ = &mut stop => break,
                acc = l.accept() => {
                    let (s, _) = acc?;
                    let sink = sink.clone();
                    let src = self.source.clone();
                    tokio::spawn(async move {
                        if let Err(e) = pump(s, &src, &sink).await {
                            sink.status.error(e);
                        }
                    });
                }
            }
        }
        Ok(())
    }
}

/// The MVP has no authentication or TLS: say so when binding beyond loopback.
pub fn warn_if_public(addr: &str) {
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr).trim_matches(['[', ']']);
    let local = host == "localhost" || host.parse::<std::net::IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false);
    if !local {
        tracing::warn!("{addr} is not a loopback address; WartLog has no authentication or TLS, so anyone who can reach it can send logs");
    }
}
