//! WartLog sources (spec section 8). Receivers accept pushed data (OTLP, NDJSON); collectors pull
//! it (journald, Kubernetes). Every source hands normalized records to a `Sink`.

pub mod cursors;
pub mod http;
#[cfg(feature = "journald")]
pub mod journald;
#[cfg(feature = "kubernetes")]
pub mod k8s;
pub mod listeners;
/// NDJSON normalization lives in the model crate so the library can use it without the receivers.
pub use wartlog_model::ndjson;
#[cfg(feature = "otlp")]
pub mod otlp;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use wartlog_model::Record;

pub use cursors::CursorStore;

/// Cancelled when the daemon shuts down or the source is removed.
pub type Shutdown = tokio::sync::watch::Receiver<bool>;

pub async fn shutdown_signal(mut s: Shutdown) {
    while !*s.borrow() {
        if s.changed().await.is_err() {
            return;
        }
    }
}

/// Records on their way to the writer. `ack` (if any) receives the highest durable seq once the
/// batch is fsynced, so collectors can commit cursors safely.
pub struct Batch {
    pub records: Vec<Record>,
    pub ack: Option<oneshot::Sender<u64>>,
    /// receives the seqs given to the batch (first, last), or why it was not fully written.
    /// Sent as soon as the records are accepted, before they are durable.
    pub seqs: Option<oneshot::Sender<Result<(u64, u64), String>>>,
}

/// Per-source counters shown by `status`.
#[derive(Debug, Default)]
pub struct SourceStatus {
    pub received: AtomicU64,
    pub dropped: AtomicU64,
    pub errors: AtomicU64,
    pub last_seen_ns: AtomicI64,
    pub last_error: Mutex<Option<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub name: String,
    pub kind: String,
    pub received: u64,
    pub dropped: u64,
    pub errors: u64,
    pub last_seen_ns: i64,
    pub last_error: Option<String>,
}

impl SourceStatus {
    pub fn error(&self, e: impl std::fmt::Display) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = Some(e.to_string());
    }
}

/// Every source's status, by name.
#[derive(Clone, Default)]
pub struct StatusRegistry {
    inner: Arc<Mutex<BTreeMap<String, (String, Arc<SourceStatus>)>>>,
}

impl StatusRegistry {
    pub fn register(&self, name: &str, kind: &str) -> Arc<SourceStatus> {
        let mut m = self.inner.lock().unwrap();
        m.entry(name.to_string()).or_insert_with(|| (kind.to_string(), Arc::new(SourceStatus::default()))).1.clone()
    }
    pub fn remove(&self, name: &str) {
        self.inner.lock().unwrap().remove(name);
    }
    pub fn snapshot(&self) -> Vec<StatusSnapshot> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(n, (k, s))| StatusSnapshot {
                name: n.clone(),
                kind: k.clone(),
                received: s.received.load(Ordering::Relaxed),
                dropped: s.dropped.load(Ordering::Relaxed),
                errors: s.errors.load(Ordering::Relaxed),
                last_seen_ns: s.last_seen_ns.load(Ordering::Relaxed),
                last_error: s.last_error.lock().unwrap().clone(),
            })
            .collect()
    }
}

/// Where sources send records. Cloning is cheap; each clone may carry its own status.
#[derive(Clone)]
pub struct Sink {
    tx: mpsc::Sender<Batch>,
    pub status: Arc<SourceStatus>,
}

impl Sink {
    pub fn new(tx: mpsc::Sender<Batch>, status: Arc<SourceStatus>) -> Sink {
        Sink { tx, status }
    }

    pub fn with_status(&self, status: Arc<SourceStatus>) -> Sink {
        Sink { tx: self.tx.clone(), status }
    }

    fn count(&self, n: usize) {
        self.status.received.fetch_add(n as u64, Ordering::Relaxed);
        self.status.last_seen_ns.store(wartlog_model::time::now_ns(), Ordering::Relaxed);
    }

    /// Sends a batch, waiting while the writer is busy (backpressure).
    pub async fn send(&self, records: Vec<Record>) -> anyhow::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        self.count(records.len());
        self.tx.send(Batch { records, ack: None, seqs: None }).await.map_err(|_| anyhow::anyhow!("writer stopped"))
    }

    /// Sends a batch and waits until it is durable. Returns the durable seq.
    pub async fn send_durable(&self, records: Vec<Record>) -> anyhow::Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.count(records.len());
        self.tx.send(Batch { records, ack: Some(tx), seqs: None }).await.map_err(|_| anyhow::anyhow!("writer stopped"))?;
        rx.await.map_err(|_| anyhow::anyhow!("writer stopped before the batch was durable"))
    }

    /// Never waits: drops (and counts) the batch when the writer is full. For datagram transports.
    pub fn try_send(&self, records: Vec<Record>) -> bool {
        let n = records.len();
        match self.tx.try_send(Batch { records, ack: None, seqs: None }) {
            Ok(()) => {
                self.count(n);
                true
            }
            Err(_) => {
                self.status.dropped.fetch_add(n as u64, Ordering::Relaxed);
                false
            }
        }
    }

    /// True when the writer channel has no free capacity (HTTP receivers answer 503 then).
    pub fn is_full(&self) -> bool {
        self.tx.capacity() == 0
    }
}

#[async_trait::async_trait]
pub trait Receiver: Send {
    fn name(&self) -> &str;
    async fn run(self: Box<Self>, sink: Sink, shutdown: Shutdown) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
pub trait Collector: Send {
    fn name(&self) -> &str;
    async fn run(self: Box<Self>, sink: Sink, cursors: CursorStore, shutdown: Shutdown) -> anyhow::Result<()>;
}
