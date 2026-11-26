//! The single writer: assigns seqs, appends to the WAL, keeps one open block per source, seals
//! blocks into segments, closes segments (building their term index), and enforces retention.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wartlog_model::{time::now_ns, Record};

use crate::block::{encode_block, BlockBuilder};
use crate::format::{SegmentHeader, UnsupportedVersion, WriterKind};
use crate::manifest::{is_new_store, Manifest, Origin};
use crate::StoreError;
use crate::reader::{SealedSeqs, SegmentHandle};
use crate::segment::Segment;
use crate::wal::WalFile;

/// Who is writing, which decides the stores a writer may open (ownership rules, docs/format.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WriteMode {
    /// wartlogd: daemon stores only (an external store needs `adopt`); creates a daemon store
    #[default]
    Daemon,
    /// the library on a store it owns: external stores only; creates an external store
    Private,
    /// the library on any existing store, with wartlogd stopped; never creates a store
    Exclusive,
}

impl WriteMode {
    pub fn writer_kind(self) -> WriterKind {
        match self {
            WriteMode::Daemon => WriterKind::Daemon,
            WriteMode::Private => WriterKind::Private,
            WriteMode::Exclusive => WriterKind::Exclusive,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WriterOptions {
    pub mode: WriteMode,
    /// daemon mode: turn an external store into a daemon store instead of refusing it
    pub adopt: bool,
    /// recorded in the manifest of a store this writer creates ("name version")
    pub creator: String,
    /// seal a source's block at this uncompressed size
    pub block_target: usize,
    /// seal a source's block this long (wall clock) after its first record
    pub block_age: Duration,
    pub segment_target: u64,
    pub segment_age: Duration,
    pub zstd_level: i32,
    /// start a new WAL file after this long (old files are deleted once sealed)
    pub wal_rotate: Duration,
    /// build term indexes on a background thread (tests turn this off for determinism)
    pub background_index: bool,
    pub retention_max_bytes: Option<u64>,
    pub retention_max_age: Option<Duration>,
}

impl Default for WriterOptions {
    fn default() -> Self {
        WriterOptions {
            mode: WriteMode::Daemon,
            adopt: false,
            creator: format!("wartlog-store {}", env!("CARGO_PKG_VERSION")),
            block_target: 256 * 1024,
            block_age: Duration::from_secs(5),
            segment_target: 256 << 20,
            segment_age: Duration::from_secs(3600),
            zstd_level: 3,
            wal_rotate: Duration::from_secs(10),
            background_index: true,
            retention_max_bytes: Some(10 << 30),
            retention_max_age: Some(Duration::from_secs(30 * 86400)),
        }
    }
}

struct OpenBlock {
    b: BlockBuilder,
    started: Instant,
}

struct OpenSegment {
    path: PathBuf,
    file: File,
    id: [u8; 16],
    len: u64,
    next_block_seq: u64,
    created: Instant,
}

pub struct Writer {
    root: PathBuf,
    _lock: File,
    opts: WriterOptions,
    next_seq: u64,
    open: HashMap<u64, OpenBlock>,
    seg: Option<OpenSegment>,
    prev_seg_id: [u8; 16],
    wal: WalFile,
    old_wals: Vec<(PathBuf, u64)>,
    indexing: Vec<JoinHandle<()>>,
    durable_seq: u64,
    stats: WriterStats,
    catalog: crate::catalog::Catalog,
}

#[derive(Clone, Debug, Default)]
pub struct WriterStats {
    pub records: u64,
    pub blocks_sealed: u64,
    pub segments_closed: u64,
    pub recovered_from_wal: u64,
    pub segments_deleted: u64,
}

/// Takes the store lock (an exclusive flock on `LOCK`), failing if another writer holds it.
pub fn lock_store(root: &Path) -> Result<File> {
    crate::create_private_dir(root)?;
    let f = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(crate::lock_path(root))?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::Locked(root.to_path_buf()).into()),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Applies the ownership rules for `mode` (refuse, never warn). Returns the manifest to write
/// when the store is new. Called before and again after taking the lock.
fn check_ownership(root: &Path, opts: &WriterOptions) -> Result<Option<Manifest>> {
    let exists = root.is_dir();
    let new = !exists || is_new_store(root);
    let manifest = if exists { Manifest::load(root)? } else { None };
    match opts.mode {
        WriteMode::Daemon => {
            if new {
                return Ok(Some(Manifest::new(Origin::Daemon, &opts.creator)));
            }
            if manifest.as_ref().is_some_and(|m| m.origin == Origin::External) {
                if !opts.adopt {
                    return Err(StoreError::ExternalStore(root.to_path_buf()).into());
                }
                let mut m = manifest.unwrap();
                m.origin = Origin::Daemon;
                return Ok(Some(m));
            }
            Ok(None)
        }
        WriteMode::Private => {
            if new {
                return Ok(Some(Manifest::new(Origin::External, &opts.creator)));
            }
            // no manifest on an existing store means a daemon store
            match manifest {
                Some(m) if m.origin == Origin::External => Ok(None),
                _ => Err(StoreError::DaemonStore(root.to_path_buf()).into()),
            }
        }
        WriteMode::Exclusive => {
            if new {
                return Err(StoreError::NotFound(root.to_path_buf()).into());
            }
            Ok(None)
        }
    }
}

impl Writer {
    /// Opens a store for writing: takes the lock, closes a segment left open by a crash, and seals
    /// every WAL record that is not in a segment yet.
    pub fn open(root: &Path, opts: WriterOptions) -> Result<Writer> {
        // ownership first, so a private writer on a daemon store gets DaemonStore, not Locked
        check_ownership(root, &opts)?;
        let lock = lock_store(root)?;
        // again under the lock: the store may have been created in between
        if let Some(m) = check_ownership(root, &opts)? {
            m.save(root)?;
        }
        crate::create_private_dir(&crate::segments_dir(root))?;
        crate::create_private_dir(&crate::wal::wal_dir(root))?;

        let paths = crate::list_segments(root);
        let mut handles = Vec::new();
        for p in &paths {
            match SegmentHandle::open(p) {
                Ok(h) => handles.push(h),
                Err(e) if e.downcast_ref::<UnsupportedVersion>().is_some() => {
                    // writing on would reuse its seqs: refuse the whole store
                    let v = e.downcast::<UnsupportedVersion>().unwrap();
                    return Err(anyhow::Error::new(StoreError::NewerFormat(v)).context(format!("segment {}", p.display())));
                }
                Err(e) => {
                    // an empty or headerless file is a segment the crashed writer had just created
                    if std::fs::metadata(p).map(|m| m.len() < crate::format::SEG_HEADER_LEN as u64).unwrap_or(false) {
                        let _ = std::fs::remove_file(p);
                    } else {
                        tracing::warn!("segment {} unreadable: {e}", p.display());
                    }
                }
            }
        }
        // a segment without a term index was open when the last writer stopped: trim a torn tail and index it
        if let Some(last) = handles.last() {
            if last.tix.is_none() {
                let path = last.path().to_path_buf();
                let scan = Segment::open(&path)?.scan(false);
                let f = OpenOptions::new().write(true).open(&path)?;
                if scan.valid_len < f.metadata()?.len() && scan.damaged.len() <= 1 {
                    f.set_len(scan.valid_len)?;
                    f.sync_all()?;
                }
                drop(f);
                crate::tix::write(&path)?;
                let h = SegmentHandle::open(&path)?;
                *handles.last_mut().unwrap() = h;
            }
        }
        let sealed = SealedSeqs::from_handles(&handles);
        let prev_seg_id = handles.last().map(|h| h.header.id).unwrap_or([0; 16]);

        let mut recovered: Vec<Record> = crate::wal::read_all(root).into_iter().filter(|r| !sealed.contains(r)).collect();
        recovered.sort_by_key(|r| r.seq);
        recovered.dedup_by_key(|r| r.seq);
        let max_wal = recovered.last().map(|r| r.seq).unwrap_or(0);
        let next_seq = sealed.max.max(max_wal) + 1;
        let old: Vec<PathBuf> = crate::wal::list(root);

        let catalog = crate::catalog::Catalog::load(root).unwrap_or_else(|| {
            let c = crate::catalog::Catalog::rebuild(root);
            let _ = c.save(root);
            c
        });
        let wal = WalFile::create(root, next_seq)?;
        let mut w = Writer {
            root: root.to_path_buf(),
            _lock: lock,
            opts,
            next_seq,
            open: HashMap::new(),
            seg: None,
            prev_seg_id,
            wal,
            old_wals: Vec::new(),
            indexing: Vec::new(),
            durable_seq: next_seq - 1,
            stats: WriterStats::default(),
            catalog,
        };
        if !recovered.is_empty() {
            tracing::info!("recovering {} records from the WAL", recovered.len());
            w.stats.recovered_from_wal = recovered.len() as u64;
            for r in &recovered {
                w.add_to_block(r)?;
            }
            w.seal_all()?;
        }
        for p in old {
            if p != w.wal.path {
                let _ = std::fs::remove_file(p);
            }
        }
        Ok(w)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn stats(&self) -> &WriterStats {
        &self.stats
    }
    /// Highest seq known to be fsynced in the WAL or a segment.
    pub fn durable_seq(&self) -> u64 {
        self.durable_seq
    }
    pub fn catalog(&self) -> &crate::catalog::Catalog {
        &self.catalog
    }
    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// Accepts a record: assigns its seq, appends it to the WAL and to its source's open block.
    /// Returns the seq. The record is durable after the next `sync`.
    pub fn push(&mut self, mut r: Record) -> Result<u64> {
        r.seq = self.next_seq;
        self.next_seq += 1;
        if r.observed_ns == 0 {
            r.observed_ns = now_ns();
        }
        self.wal.append(&r)?;
        self.add_to_block(&r)?;
        self.stats.records += 1;
        Ok(r.seq)
    }

    fn add_to_block(&mut self, r: &Record) -> Result<()> {
        let fp = r.source.fingerprint();
        if self.catalog.add(&r.source) {
            self.catalog.save(&self.root)?;
        }
        let ob = self.open.entry(fp).or_insert_with(|| OpenBlock { b: BlockBuilder::new(), started: Instant::now() });
        ob.b.push(r);
        if ob.b.approx_len() >= self.opts.block_target {
            self.seal(&[fp])?;
        }
        Ok(())
    }

    /// fsyncs the WAL. Returns the highest durable seq.
    pub fn sync(&mut self) -> Result<u64> {
        self.wal.sync()?;
        self.durable_seq = self.next_seq - 1;
        Ok(self.durable_seq)
    }

    /// Periodic work: seals aged blocks, rotates the WAL, closes an aged segment, deletes sealed WAL files.
    pub fn tick(&mut self) -> Result<()> {
        let age = self.opts.block_age;
        let aged: Vec<u64> = self.open.iter().filter(|(_, ob)| ob.started.elapsed() >= age).map(|(k, _)| *k).collect();
        if !aged.is_empty() {
            self.seal(&aged)?;
        }
        if self.seg.as_ref().is_some_and(|s| s.created.elapsed() >= self.opts.segment_age) {
            self.close_segment()?;
        }
        if !self.wal.is_empty() && self.wal.created.elapsed() >= self.opts.wal_rotate {
            self.rotate_wal()?;
        }
        self.drop_sealed_wals();
        self.indexing.retain(|h| !h.is_finished());
        Ok(())
    }

    fn rotate_wal(&mut self) -> Result<()> {
        self.wal.sync()?;
        let new = WalFile::create(&self.root, self.next_seq)?;
        let old = std::mem::replace(&mut self.wal, new);
        self.old_wals.push((old.path.clone(), old.max_seq));
        Ok(())
    }

    fn drop_sealed_wals(&mut self) {
        let min_open = self.open.values().filter_map(|ob| ob.b.first_seq()).min().unwrap_or(self.next_seq);
        self.old_wals.retain(|(p, max)| {
            if *max < min_open {
                let _ = std::fs::remove_file(p);
                false
            } else {
                true
            }
        });
    }

    fn ensure_segment(&mut self) -> Result<&mut OpenSegment> {
        if self.seg.is_none() {
            let id = *uuid::Uuid::now_v7().as_bytes();
            let now = now_ns();
            let (y, m, d) = wartlog_model::time::civil_from_days(now.div_euclid(86400 * 1_000_000_000));
            let dir = crate::segments_dir(&self.root).join(format!("{y:04}")).join(format!("{m:02}")).join(format!("{d:02}"));
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{}.wseg", uuid::Uuid::from_bytes(id).simple()));
            let mut file = OpenOptions::new().create_new(true).append(true).open(&path)?;
            crate::set_private(&path);
            let hdr = SegmentHeader { id, prev: self.prev_seg_id, created_ns: now, writer: self.opts.mode.writer_kind() }.encode();
            file.write_all(&hdr)?;
            file.sync_all()?;
            crate::sync_dir(&dir);
            self.seg = Some(OpenSegment { path, file, id, len: hdr.len() as u64, next_block_seq: 0, created: Instant::now() });
        }
        Ok(self.seg.as_mut().unwrap())
    }

    /// Seals the open blocks of these sources into the current segment (one fsync for all).
    fn seal(&mut self, sources: &[u64]) -> Result<()> {
        let level = self.opts.zstd_level;
        let mut blocks = Vec::new();
        for fp in sources {
            if let Some(ob) = self.open.remove(fp) {
                if !ob.b.is_empty() {
                    blocks.push(ob.b.finish());
                }
            }
        }
        if blocks.is_empty() {
            return Ok(());
        }
        // the WAL must be durable before its records count as sealed elsewhere
        self.wal.sync()?;
        let seg = self.ensure_segment()?;
        let mut buf = Vec::new();
        for (p, s) in &blocks {
            buf.extend(encode_block(seg.next_block_seq, p, s, level));
            seg.next_block_seq += 1;
        }
        seg.file.write_all(&buf)?;
        seg.file.sync_data()?;
        seg.len += buf.len() as u64;
        let full = seg.len >= self.opts.segment_target;
        self.stats.blocks_sealed += blocks.len() as u64;
        self.durable_seq = self.next_seq - 1;
        if full {
            self.close_segment()?;
        }
        Ok(())
    }

    /// Seals every open block.
    pub fn seal_all(&mut self) -> Result<()> {
        let all: Vec<u64> = self.open.keys().copied().collect();
        self.seal(&all)
    }

    /// Seals everything and closes the current segment, building its term index.
    pub fn close_segment(&mut self) -> Result<()> {
        let all: Vec<u64> = self.open.keys().copied().collect();
        if !all.is_empty() {
            // seal without recursing into close_segment
            let target = self.opts.segment_target;
            self.opts.segment_target = u64::MAX;
            let r = self.seal(&all);
            self.opts.segment_target = target;
            r?;
        }
        let Some(seg) = self.seg.take() else { return Ok(()) };
        seg.file.sync_all()?;
        drop(seg.file);
        self.prev_seg_id = seg.id;
        self.stats.segments_closed += 1;
        // every record so far is sealed: all WAL files before the current one can go
        for (p, _) in self.old_wals.drain(..) {
            let _ = std::fs::remove_file(p);
        }
        let path = seg.path;
        if self.opts.background_index {
            self.indexing.push(std::thread::spawn(move || {
                if let Err(e) = crate::tix::write(&path) {
                    tracing::error!("term index for {} failed: {e:#}", path.display());
                }
            }));
        } else {
            crate::tix::write(&path)?;
        }
        self.enforce_retention()?;
        Ok(())
    }

    /// Deletes the oldest sealed segments while the store is over its size cap, or while a segment's
    /// newest observed time is older than the age cap. The open segment is never deleted.
    pub fn enforce_retention(&mut self) -> Result<Vec<PathBuf>> {
        let open = self.seg.as_ref().map(|s| s.path.clone());
        let segs: Vec<PathBuf> = crate::list_segments(&self.root).into_iter().filter(|p| Some(p) != open.as_ref()).collect();
        let size_of = |p: &Path| -> u64 {
            std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) + std::fs::metadata(crate::tix::tix_path(p)).map(|m| m.len()).unwrap_or(0)
        };
        let mut total: u64 = crate::list_segments(&self.root).iter().map(|p| size_of(p)).sum();
        let cutoff = self.opts.retention_max_age.map(|a| now_ns() - a.as_nanos() as i64);
        let mut deleted = Vec::new();
        for p in segs {
            let over_size = self.opts.retention_max_bytes.is_some_and(|m| total > m);
            let too_old = match cutoff {
                Some(c) => SegmentHandle::open(&p).map(|h| h.max_observed() < c).unwrap_or(false),
                None => false,
            };
            if !(over_size || too_old) {
                break;
            }
            let sz = size_of(&p);
            // index first, so a reader never sees an index without its segment for long
            let _ = std::fs::remove_file(crate::tix::tix_path(&p));
            std::fs::remove_file(&p).with_context(|| format!("delete {}", p.display()))?;
            total = total.saturating_sub(sz);
            self.stats.segments_deleted += 1;
            deleted.push(p);
        }
        Ok(deleted)
    }

    /// Waits for background index builds.
    pub fn wait_for_indexing(&mut self) {
        for h in self.indexing.drain(..) {
            let _ = h.join();
        }
    }

    /// Clean shutdown: seal everything, close the segment, index it, and remove the WAL.
    pub fn close(mut self) -> Result<()> {
        self.close_segment()?;
        self.wait_for_indexing();
        self.wal.sync()?;
        let _ = std::fs::remove_file(&self.wal.path);
        for (p, _) in self.old_wals.drain(..) {
            let _ = std::fs::remove_file(p);
        }
        Ok(())
    }
}

/// Rebuilds every term index from the segments (takes the store lock).
pub fn rebuild_indexes(root: &Path) -> Result<usize> {
    let _lock = lock_store(root)?;
    let mut n = 0;
    for p in crate::list_segments(root) {
        let _ = std::fs::remove_file(crate::tix::tix_path(&p));
        crate::tix::write(&p)?;
        n += 1;
    }
    Ok(n)
}
