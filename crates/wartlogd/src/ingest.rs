//! The writer thread: the only owner of the store `Writer`. It takes batches from every source,
//! fsyncs the WAL at least every 250 ms (sooner when a collector waits for an ack), publishes new
//! records to the live tail bus, and runs periodic sealing and retention.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot};
use wartlog_model::Record;
use wartlog_sources::Batch;
use wartlog_store::Writer;

pub const SYNC_EVERY: Duration = Duration::from_millis(250);
const RETENTION_EVERY: Duration = Duration::from_secs(60);

pub enum Cmd {
    /// Rebuild term indexes (all sealed segments, or one by file name); replies with the count.
    RebuildIndex { segment: Option<String>, reply: oneshot::Sender<anyhow::Result<usize>> },
    Shutdown,
}

#[derive(Default)]
pub struct IngestStats {
    pub last_seq: AtomicU64,
    pub durable_seq: AtomicU64,
    pub records: AtomicU64,
    pub errors: AtomicU64,
    pub last_error: Mutex<Option<String>>,
    /// (second, records) for the last few seconds
    rate: Mutex<VecDeque<(u64, u64)>>,
}

impl IngestStats {
    fn count(&self, n: u64) {
        let sec = wartlog_model::time::now_ns() as u64 / 1_000_000_000;
        let mut r = self.rate.lock().unwrap();
        match r.back_mut() {
            Some((s, c)) if *s == sec => *c += n,
            _ => r.push_back((sec, n)),
        }
        while r.len() > 12 {
            r.pop_front();
        }
    }

    /// Records per second over the last 10 whole seconds.
    pub fn rate(&self) -> f64 {
        let now = wartlog_model::time::now_ns() as u64 / 1_000_000_000;
        let r = self.rate.lock().unwrap();
        let n: u64 = r.iter().filter(|(s, _)| *s < now && *s + 10 >= now).map(|(_, c)| c).sum();
        n as f64 / 10.0
    }

    fn error(&self, e: impl std::fmt::Display) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = Some(e.to_string());
    }
}

pub struct Ingest {
    pub writer: Writer,
    pub rx: mpsc::Receiver<Batch>,
    pub cmds: std::sync::mpsc::Receiver<Cmd>,
    pub tail: broadcast::Sender<Arc<Record>>,
    pub stats: Arc<IngestStats>,
}

impl Ingest {
    /// Runs until `Cmd::Shutdown` or until every sender is gone; then closes the store cleanly.
    pub fn run(mut self, rt: tokio::runtime::Handle) -> anyhow::Result<()> {
        let mut acks: Vec<oneshot::Sender<u64>> = Vec::new();
        let mut last_sync = Instant::now();
        let mut last_tick = Instant::now();
        let mut last_retention = Instant::now() - RETENTION_EVERY + Duration::from_secs(5);
        let mut unsynced = false;
        let mut stopping = false;
        self.stats.last_seq.store(self.writer.last_seq(), Ordering::Relaxed);
        self.stats.durable_seq.store(self.writer.durable_seq(), Ordering::Relaxed);
        loop {
            // wait for the first batch, at most until the next sync is due
            let wait = SYNC_EVERY.saturating_sub(last_sync.elapsed()).max(Duration::from_millis(5));
            let first = if stopping {
                self.rx.try_recv().ok()
            } else {
                match rt.block_on(async { tokio::time::timeout(wait, self.rx.recv()).await }) {
                    Ok(Some(b)) => Some(b),
                    Ok(None) => {
                        stopping = true;
                        None
                    }
                    Err(_) => None,
                }
            };
            let mut n = 0usize;
            let mut next = first;
            while let Some(b) = next {
                self.write(b, &mut acks);
                unsynced = true;
                n += 1;
                // drain what is already queued, bounded so syncs stay timely
                next = if n < 256 { self.rx.try_recv().ok() } else { None };
            }
            if unsynced && (!acks.is_empty() || last_sync.elapsed() >= SYNC_EVERY || stopping) {
                match self.writer.sync() {
                    Ok(d) => {
                        self.stats.durable_seq.store(d, Ordering::Relaxed);
                        for a in acks.drain(..) {
                            let _ = a.send(d);
                        }
                    }
                    // dropping the acks tells collectors the batch is not durable; they retry
                    Err(e) => {
                        tracing::error!("WAL sync failed: {e:#}");
                        self.stats.error(format!("WAL sync failed: {e:#}"));
                        acks.clear();
                    }
                }
                unsynced = false;
                last_sync = Instant::now();
            }
            if last_tick.elapsed() >= SYNC_EVERY {
                if let Err(e) = self.writer.tick() {
                    tracing::error!("store maintenance failed: {e:#}");
                    self.stats.error(format!("{e:#}"));
                }
                last_tick = Instant::now();
            }
            if last_retention.elapsed() >= RETENTION_EVERY {
                match self.writer.enforce_retention() {
                    Ok(d) if !d.is_empty() => tracing::info!("retention deleted {} segment(s)", d.len()),
                    Ok(_) => {}
                    Err(e) => self.stats.error(format!("retention: {e:#}")),
                }
                last_retention = Instant::now();
            }
            while let Ok(c) = self.cmds.try_recv() {
                match c {
                    Cmd::Shutdown => stopping = true,
                    Cmd::RebuildIndex { segment, reply } => {
                        let _ = reply.send(self.rebuild(segment.as_deref()));
                    }
                }
            }
            if stopping && self.rx.is_empty() && !unsynced {
                break;
            }
        }
        tracing::info!("closing the store");
        self.writer.close()
    }

    fn write(&mut self, b: Batch, acks: &mut Vec<oneshot::Sender<u64>>) {
        let tail = self.tail.receiver_count() > 0;
        let n = b.records.len() as u64;
        let first = self.writer.last_seq() + 1;
        let mut failed: Option<String> = None;
        for r in b.records {
            let copy = tail.then(|| r.clone());
            match self.writer.push(r) {
                Ok(seq) => {
                    if let Some(mut c) = copy {
                        c.seq = seq;
                        let _ = self.tail.send(Arc::new(c));
                    }
                }
                Err(e) => {
                    tracing::error!("write failed: {e:#}");
                    self.stats.error(format!("write failed: {e:#}"));
                    failed.get_or_insert_with(|| format!("write failed: {e:#}"));
                }
            }
        }
        if let Some(tx) = b.seqs {
            let _ = tx.send(match failed {
                Some(e) => Err(e),
                None => Ok((first, self.writer.last_seq())),
            });
        }
        self.stats.records.fetch_add(n, Ordering::Relaxed);
        self.stats.count(n);
        self.stats.last_seq.store(self.writer.last_seq(), Ordering::Relaxed);
        if let Some(a) = b.ack {
            acks.push(a);
        }
    }

    fn rebuild(&mut self, segment: Option<&str>) -> anyhow::Result<usize> {
        // close the open segment so every segment is complete
        self.writer.close_segment()?;
        self.writer.wait_for_indexing();
        let mut n = 0;
        for p in wartlog_store::list_segments(self.writer.root()) {
            let name = p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
            if segment.is_some_and(|s| !name.starts_with(s)) {
                continue;
            }
            wartlog_store::tix::write(&p)?;
            n += 1;
        }
        if let (0, Some(s)) = (n, segment) {
            anyhow::bail!("no segment matches {s}");
        }
        Ok(n)
    }
}
