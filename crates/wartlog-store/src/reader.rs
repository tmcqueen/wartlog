//! Read-only view of a store at one moment. Takes no locks and never writes.

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wartlog_model::Record;

use crate::block::BlockView;
use crate::format::SegmentHeader;
use crate::segment::{BlockMeta, Segment};
use crate::tix::Tix;

pub struct SegmentHandle {
    pub seg: Segment,
    pub header: SegmentHeader,
    pub tix: Option<Tix>,
    pub blocks: Vec<BlockMeta>,
    /// damaged byte ranges found while scanning (only known when there was no term index)
    pub damaged: Vec<(u64, u64)>,
}

impl SegmentHandle {
    pub fn open(path: &Path) -> Result<SegmentHandle> {
        let seg = Segment::open(path)?;
        let header = seg.header()?;
        let tix = Tix::open(path, &header.id, seg.len())?;
        let (blocks, damaged) = match &tix {
            Some(t) => ((0..t.blocks).map(|i| t.block(i)).collect(), vec![]),
            None => {
                let s = seg.scan(false);
                (s.blocks, s.damaged)
            }
        };
        Ok(SegmentHandle { seg, header, tix, blocks, damaged })
    }

    pub fn path(&self) -> &Path {
        &self.seg.path
    }

    pub fn read_block(&self, i: usize) -> Result<BlockView> {
        self.seg.read_block(&self.blocks[i])
    }

    pub fn min_time(&self) -> i64 {
        self.blocks.iter().map(|b| b.min_time).min().unwrap_or(i64::MAX)
    }
    pub fn max_time(&self) -> i64 {
        self.blocks.iter().map(|b| b.max_time).max().unwrap_or(i64::MIN)
    }
    pub fn max_seq(&self) -> u64 {
        self.blocks.iter().map(|b| b.last_seq).max().unwrap_or(0)
    }
    pub fn max_observed(&self) -> i64 {
        self.blocks.iter().map(|b| b.max_observed).max().unwrap_or(i64::MIN)
    }
}

/// Seqs that are already sealed, used to drop WAL records that also exist in segments.
/// Sealed segments hold every seq up to `before_last`; in the last (possibly open) segment, each
/// source's blocks are sealed in seq order, so a source's highest sealed seq bounds it.
#[derive(Default, Debug)]
pub struct SealedSeqs {
    pub before_last: u64,
    pub per_source: HashMap<u64, u64>,
    pub max: u64,
}

impl SealedSeqs {
    pub fn from_handles(segs: &[SegmentHandle]) -> SealedSeqs {
        let mut s = SealedSeqs::default();
        if let Some((last, rest)) = segs.split_last() {
            s.before_last = rest.iter().map(|h| h.max_seq()).max().unwrap_or(0);
            for b in &last.blocks {
                for src in &b.sources {
                    let e = s.per_source.entry(*src).or_default();
                    *e = (*e).max(b.last_seq);
                }
            }
            s.max = s.before_last.max(last.max_seq());
        }
        s
    }

    pub fn contains(&self, r: &Record) -> bool {
        r.seq <= self.before_last || self.per_source.get(&r.source.fingerprint()).is_some_and(|&m| r.seq <= m)
    }
}

/// A consistent read-only view: every segment present at open time, plus WAL records not yet sealed.
pub struct Snapshot {
    pub root: PathBuf,
    pub segments: Vec<Arc<SegmentHandle>>,
    /// unsealed records, in seq order
    pub wal: Vec<Record>,
    pub catalog: crate::catalog::Catalog,
    /// segments that could not be opened (path, error)
    pub errors: Vec<(PathBuf, String)>,
}

impl Snapshot {
    pub fn open(root: &Path) -> Result<Snapshot> {
        // refuse a store format newer than this build (compatibility policy)
        crate::manifest::Manifest::load(root)?;
        // WAL before segments: a record sealed (and its WAL file deleted) in between is then found
        // in the segment, so nothing falls through the gap; overlap is removed below.
        let wal_records = crate::wal::read_all(root);
        let mut segments = Vec::new();
        let mut errors = Vec::new();
        for p in crate::list_segments(root) {
            match SegmentHandle::open(&p) {
                Ok(h) => segments.push(h),
                // a segment just created by the writer may not have its header yet
                Err(e) => errors.push((p, e.to_string())),
            }
        }
        let sealed = SealedSeqs::from_handles(&segments);
        let mut wal: Vec<Record> = wal_records.into_iter().filter(|r| !sealed.contains(r)).collect();
        wal.sort_by_key(|r| r.seq);
        wal.dedup_by_key(|r| r.seq);
        // a reader never writes: without a catalog file, derive it (slowly) from the data
        let mut catalog = crate::catalog::Catalog::load(root).unwrap_or_else(|| crate::catalog::Catalog::rebuild(root));
        for r in &wal {
            catalog.add(&r.source);
        }
        Ok(Snapshot { root: root.to_path_buf(), catalog, segments: segments.into_iter().map(Arc::new).collect(), wal, errors })
    }

    pub fn block_count(&self) -> usize {
        self.segments.iter().map(|s| s.blocks.len()).sum()
    }

    pub fn record_count(&self) -> u64 {
        self.segments.iter().flat_map(|s| s.blocks.iter()).map(|b| b.record_count as u64).sum::<u64>() + self.wal.len() as u64
    }

    pub fn max_seq(&self) -> u64 {
        let s = self.segments.iter().map(|s| s.max_seq()).max().unwrap_or(0);
        s.max(self.wal.last().map(|r| r.seq).unwrap_or(0))
    }

    /// Finds a record by seq: WAL first, then a search over block seq ranges.
    pub fn get(&self, seq: u64) -> Result<Option<Record>> {
        if let Ok(i) = self.wal.binary_search_by_key(&seq, |r| r.seq) {
            return Ok(Some(self.wal[i].clone()));
        }
        for s in self.segments.iter().rev() {
            for (i, b) in s.blocks.iter().enumerate() {
                if b.first_seq <= seq && seq <= b.last_seq {
                    let v = s.read_block(i)?;
                    let found = v.iter().find(|r| r.seq() == seq).map(|r| r.to_record());
                    if found.is_some() {
                        return Ok(found);
                    }
                }
            }
        }
        Ok(None)
    }

    pub fn disk_bytes(&self) -> u64 {
        fn size(d: &Path) -> u64 {
            std::fs::read_dir(d)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| match e.metadata() {
                            Ok(m) if m.is_dir() => size(&e.path()),
                            Ok(m) => m.len(),
                            Err(_) => 0,
                        })
                        .sum()
                })
                .unwrap_or(0)
        }
        size(&self.root)
    }
}
