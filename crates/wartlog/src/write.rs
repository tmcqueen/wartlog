use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wartlog_api::blocking::BlockingClient;
use wartlog_api::proto::{Request, Response};
use wartlog_model::Record;
use wartlog_store::Origin;

use crate::{Error, ErrorKind, Result};

/// How a writer reaches the store. Chosen explicitly: there is no mode that takes the lock when
/// the daemon happens to be stopped (wartlogd would then fail to start, with no visible cause).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    /// Through the running wartlogd (an `Ingest` request per push). The daemon keeps the lock;
    /// the library never touches the files. Fails with `NoDaemon` if no daemon serves the store.
    Shared,
    /// A store the library owns (`<name>.wartlogx`, origin external), created if absent. Refuses
    /// a daemon store (`DaemonStore`).
    Private,
    /// A daemon store with wartlogd stopped: takes the lock, fails with `Locked` while the daemon
    /// runs. For bulk imports and maintenance. Never creates a store.
    Exclusive,
}

#[derive(Clone, Debug)]
pub struct WriterOptions {
    pub mode: WriteMode,
    /// shared mode: the daemon's API socket (default: `default_socket()`)
    pub socket: Option<PathBuf>,
    /// recorded in the manifest of a store this writer creates
    pub creator: String,
    /// private and exclusive: delete the oldest segments past this size (default: none)
    pub retention_max_bytes: Option<u64>,
    /// private and exclusive: delete segments whose newest record is older (default: none)
    pub retention_max_age: Option<Duration>,
    /// private and exclusive: how often the maintenance thread fsyncs the WAL (default 250 ms)
    pub sync_every: Duration,
}

impl WriterOptions {
    pub fn new(mode: WriteMode) -> WriterOptions {
        WriterOptions {
            mode,
            socket: None,
            creator: format!("wartlog {}", crate::VERSION),
            retention_max_bytes: None,
            retention_max_age: None,
            sync_every: Duration::from_millis(250),
        }
    }
}

/// Writes records to a store. Pushes return the record's seq (its id) at once; a record is
/// durable once `flush` returns a durable seq at or above it (private and exclusive writers also
/// fsync on their own every `sync_every`). `close` seals everything; dropping a writer closes it
/// too, but cannot report errors.
///
/// Not `Sync`: use it from one thread at a time (it is `Send`).
pub struct Writer {
    inner: Option<Inner>,
}

enum Inner {
    Shared { client: BlockingClient },
    Embedded(Embedded),
}

struct Embedded {
    /// None only after close took it
    w: Option<Arc<Mutex<wartlog_store::Writer>>>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    failed: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

const RETENTION_EVERY: Duration = Duration::from_secs(60);

impl Embedded {
    fn open(root: &Path, opts: &WriterOptions, mode: wartlog_store::WriteMode) -> Result<Embedded> {
        let so = wartlog_store::WriterOptions {
            mode,
            adopt: false,
            creator: opts.creator.clone(),
            retention_max_bytes: opts.retention_max_bytes,
            retention_max_age: opts.retention_max_age,
            ..Default::default()
        };
        let w = Arc::new(Mutex::new(wartlog_store::Writer::open(root, so)?));
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let failed = Arc::new(Mutex::new(None));
        let (w2, stop2, failed2, every) = (w.clone(), stop.clone(), failed.clone(), opts.sync_every.max(Duration::from_millis(10)));
        // the host will not call sync, tick or retention, so a thread does
        let thread = std::thread::Builder::new()
            .name("wartlog-maint".into())
            .spawn(move || {
                let mut last_retention = Instant::now();
                loop {
                    {
                        let (m, cv) = &*stop2;
                        let g = m.lock().unwrap();
                        let (g, _) = cv.wait_timeout_while(g, every, |s| !*s).unwrap();
                        if *g {
                            return;
                        }
                    }
                    let mut w = w2.lock().unwrap_or_else(|p| p.into_inner());
                    let r = w.sync().and_then(|_| w.tick()).and_then(|_| {
                        if last_retention.elapsed() >= RETENTION_EVERY {
                            last_retention = Instant::now();
                            w.enforce_retention().map(|_| ())
                        } else {
                            Ok(())
                        }
                    });
                    if let Err(e) = r {
                        *failed2.lock().unwrap() = Some(format!("background maintenance failed: {e:#}"));
                    }
                }
            })
            .map_err(|e| Error::new(ErrorKind::Other, format!("cannot start the maintenance thread: {e}")))?;
        Ok(Embedded { w: Some(w), stop, failed, thread: Some(thread) })
    }

    fn w(&self) -> std::sync::MutexGuard<'_, wartlog_store::Writer> {
        let w = self.w.as_ref().expect("writer present until close");
        w.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn check(&self) -> Result<()> {
        match self.failed.lock().unwrap().take() {
            Some(e) => Err(Error::new(ErrorKind::Other, e)),
            None => Ok(()),
        }
    }

    fn close(mut self) -> Result<()> {
        {
            let (m, cv) = &*self.stop;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let w = Arc::try_unwrap(self.w.take().expect("writer present until close"))
            .map_err(|_| Error::new(ErrorKind::Other, "writer still in use"))?
            .into_inner()
            .unwrap_or_else(|p| p.into_inner());
        w.close()?;
        Ok(())
    }
}

impl Writer {
    /// Opens a writer on the store at `root` (shared mode: the store wartlogd serves, checked).
    pub fn open(root: impl AsRef<Path>, opts: WriterOptions) -> Result<Writer> {
        let root = root.as_ref();
        let inner = match opts.mode {
            WriteMode::Shared => Inner::Shared { client: connect_shared(root, &opts)? },
            WriteMode::Private => Inner::Embedded(Embedded::open(root, &opts, wartlog_store::WriteMode::Private)?),
            WriteMode::Exclusive => Inner::Embedded(Embedded::open(root, &opts, wartlog_store::WriteMode::Exclusive)?),
        };
        Ok(Writer { inner: Some(inner) })
    }

    fn inner(&mut self) -> Result<&mut Inner> {
        self.inner.as_mut().ok_or_else(|| Error::new(ErrorKind::InvalidArgument, "the writer is closed"))
    }

    /// Writes one record; returns its seq. The record's own `seq` is ignored.
    pub fn push(&mut self, r: Record) -> Result<u64> {
        let (_, last) = self.push_batch(vec![r])?;
        Ok(last)
    }

    /// Writes records in order with consecutive seqs; returns (first, last). For an empty batch
    /// `last < first`. In shared mode this is one round trip, so batch when throughput matters.
    pub fn push_batch(&mut self, records: Vec<Record>) -> Result<(u64, u64)> {
        match self.inner()? {
            Inner::Shared { client } => match client.call_one(&Request::Ingest { records, durable: false })? {
                Response::Ingested { first_seq, last_seq, .. } => Ok((first_seq, last_seq)),
                other => Err(Error::new(ErrorKind::Other, format!("unexpected reply {other:?}"))),
            },
            Inner::Embedded(e) => {
                e.check()?;
                let mut w = e.w();
                let first = w.last_seq() + 1;
                for r in records {
                    w.push(r)?;
                }
                Ok((first, w.last_seq()))
            }
        }
    }

    /// Normalizes an NDJSON (or plain text) line as wartlogd's NDJSON receivers do, and writes it.
    pub fn push_json(&mut self, line: &str, default_source: &str) -> Result<u64> {
        self.push(crate::record_from_json(line, default_source))
    }

    /// Blocks until everything pushed so far is durable; returns the durable seq.
    pub fn flush(&mut self) -> Result<u64> {
        match self.inner()? {
            Inner::Shared { client } => match client.call_one(&Request::Ingest { records: vec![], durable: true })? {
                Response::Ingested { durable_seq, .. } => Ok(durable_seq),
                other => Err(Error::new(ErrorKind::Other, format!("unexpected reply {other:?}"))),
            },
            Inner::Embedded(e) => {
                e.check()?;
                Ok(e.w().sync()?)
            }
        }
    }

    /// Highest seq given out so far.
    pub fn last_seq(&mut self) -> Result<u64> {
        match self.inner()? {
            Inner::Shared { client } => match client.call_one(&Request::Ingest { records: vec![], durable: false })? {
                Response::Ingested { last_seq, .. } => Ok(last_seq),
                other => Err(Error::new(ErrorKind::Other, format!("unexpected reply {other:?}"))),
            },
            Inner::Embedded(e) => Ok(e.w().last_seq()),
        }
    }

    /// Flushes and closes. Private and exclusive writers seal every block and close the segment,
    /// so a clean close leaves no WAL to recover.
    pub fn close(mut self) -> Result<()> {
        self.close_inner()
    }

    fn close_inner(&mut self) -> Result<()> {
        match self.inner.take() {
            None => Ok(()),
            Some(Inner::Shared { mut client }) => {
                client.call_one(&Request::Ingest { records: vec![], durable: true })?;
                Ok(())
            }
            Some(Inner::Embedded(e)) => e.close(),
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.close_inner();
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        // only reached when close was not called (or failed before stopping the thread)
        let (m, cv) = &*self.stop;
        *m.lock().unwrap() = true;
        cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Shared mode: connect, and make sure the daemon on the socket serves this store.
fn connect_shared(root: &Path, opts: &WriterOptions) -> Result<BlockingClient> {
    if root.is_dir() && wartlog_store::Manifest::origin_of(root)? == Origin::External {
        return Err(Error::new(ErrorKind::NoDaemon, format!("{} is an external store: no wartlogd serves it (use private mode)", root.display())));
    }
    let sock = opts.socket.clone().unwrap_or_else(crate::default_socket);
    let mut c = BlockingClient::connect(&sock, &opts.creator).map_err(|e| Error::new(ErrorKind::NoDaemon, format!("{e:#}")))?;
    let served = match c.call_one(&Request::Status)? {
        Response::Status { status } => PathBuf::from(status.store_root),
        other => return Err(Error::new(ErrorKind::Other, format!("unexpected reply {other:?}"))),
    };
    let same = match (root.canonicalize(), served.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => root == served,
    };
    if !same {
        return Err(Error::new(ErrorKind::NoDaemon, format!("the wartlogd on {} serves {}, not {}", sock.display(), served.display(), root.display())));
    }
    Ok(c)
}
