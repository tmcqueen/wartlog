//! Read-only store verification (`wl store verify`, `wl_store_verify`): the seq rules readers and
//! WAL recovery depend on (docs/format.md, "Seq rules"). Never modifies the store, takes no lock.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::format::WriterKind;
use crate::reader::{SealedSeqs, SegmentHandle};

#[derive(Clone, Debug, PartialEq)]
pub enum Finding {
    /// a source's seqs go backwards from one block to the next (file order, across segments)
    SeqBackwards { source: u64, segment: PathBuf, offset: u64, first_seq: u64, prev_segment: PathBuf, prev_offset: u64, prev_last_seq: u64 },
    /// a source's seqs go backwards inside one block
    SeqBackwardsInBlock { source: u64, segment: PathBuf, offset: u64, seq: u64, prev_seq: u64 },
    /// a seq stored more than once (two writers, or a bad recovery)
    DuplicateSeq { seq: u64, segment: PathBuf, offset: u64 },
    /// a segment before the newest holds a seq above the newest segment's highest seq
    SealedAboveNewest { segment: PathBuf, max_seq: u64, newest: PathBuf, newest_max_seq: u64 },
    /// a WAL record readers and recovery treat as sealed although no segment holds it: it is
    /// invisible, and the next writer open deletes it
    HiddenWalRecord { seq: u64 },
    /// a segment that cannot be opened, or a block that does not decode
    Unreadable { segment: PathBuf, offset: Option<u64>, error: String },
}

impl Finding {
    /// One line for humans.
    pub fn describe(&self) -> String {
        let n = |p: &Path| p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        match self {
            Finding::SeqBackwards { source, segment, offset, first_seq, prev_segment, prev_offset, prev_last_seq } => format!(
                "seq goes backwards for source {source:016x}: block at {}+{offset} starts at seq {first_seq}, after block at {}+{prev_offset} ending at seq {prev_last_seq}",
                n(segment),
                n(prev_segment)
            ),
            Finding::SeqBackwardsInBlock { source, segment, offset, seq, prev_seq } => {
                format!("seq goes backwards for source {source:016x} inside block {}+{offset}: {seq} after {prev_seq}", n(segment))
            }
            Finding::DuplicateSeq { seq, segment, offset } => format!("seq {seq} appears more than once (again in block {}+{offset})", n(segment)),
            Finding::SealedAboveNewest { segment, max_seq, newest, newest_max_seq } => format!(
                "segment {} holds seq {max_seq}, above the newest segment {} (highest seq {newest_max_seq})",
                n(segment),
                n(newest)
            ),
            Finding::HiddenWalRecord { seq } => format!("WAL record seq {seq} counts as sealed but no segment holds it (hidden, and recovery would delete it)"),
            Finding::Unreadable { segment, offset: Some(o), error } => format!("block {}+{o} unreadable: {error}", n(segment)),
            Finding::Unreadable { segment, offset: None, error } => format!("segment {} unreadable: {error}", n(segment)),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub segments: usize,
    pub blocks: usize,
    /// records read (full verification only)
    pub records: u64,
    pub wal_records: usize,
    /// segments per writer kind (header bytes 10..12)
    pub writers: Vec<(WriterKind, usize)>,
    /// every finding up to `max_findings`
    pub findings: Vec<Finding>,
    /// all findings, including those past `max_findings`
    pub finding_count: u64,
    /// true when blocks were read (duplicate and hidden-WAL checks need it)
    pub full: bool,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.finding_count == 0
    }

    fn add(&mut self, f: Finding, max: usize) {
        self.finding_count += 1;
        if self.findings.len() < max {
            self.findings.push(f);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    /// read every block (duplicates, in-block order, hidden WAL records); otherwise block metadata only
    pub full: bool,
    pub max_findings: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { full: true, max_findings: 1000 }
    }
}

/// Seen-seq set: a bitmap when seqs are dense, a sorted list otherwise.
struct Seen {
    base: u64,
    bits: Vec<u64>,
    sparse: Option<Vec<u64>>,
}

impl Seen {
    fn new(min: u64, max: u64, records: u64) -> Seen {
        let range = max.saturating_sub(min).saturating_add(1);
        if range <= records.saturating_mul(4).saturating_add(1 << 20) && range < 1 << 34 {
            Seen { base: min, bits: vec![0; range.div_ceil(64) as usize], sparse: None }
        } else {
            Seen { base: 0, bits: Vec::new(), sparse: Some(Vec::new()) }
        }
    }

    /// Returns false when the seq was already present (bitmap mode; sparse mode answers later).
    fn insert(&mut self, seq: u64) -> bool {
        if let Some(v) = &mut self.sparse {
            v.push(seq);
            return true;
        }
        let Some(i) = seq.checked_sub(self.base).map(|i| i as usize) else { return true };
        let Some(w) = self.bits.get_mut(i / 64) else { return true };
        let m = 1u64 << (i % 64);
        let fresh = *w & m == 0;
        *w |= m;
        fresh
    }

    fn contains(&self, seq: u64) -> bool {
        if let Some(v) = &self.sparse {
            return v.binary_search(&seq).is_ok();
        }
        let Some(i) = seq.checked_sub(self.base).map(|i| i as usize) else { return false };
        self.bits.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }
}

/// Checks the seq rules over every segment and the WAL. Read-only.
pub fn verify_store(root: &Path, opts: &Options) -> Report {
    let max = opts.max_findings;
    let mut rep = Report { full: opts.full, ..Default::default() };
    // WAL before segments, as Snapshot does, so a record sealed in between is still found
    let wal = crate::wal::read_all(root);
    let mut handles = Vec::new();
    for p in crate::list_segments(root) {
        match SegmentHandle::open(&p) {
            Ok(h) => handles.push(h),
            Err(e) => rep.add(Finding::Unreadable { segment: p, offset: None, error: format!("{e:#}") }, max),
        }
    }
    rep.segments = handles.len();
    let mut writers: HashMap<WriterKind, usize> = HashMap::new();
    for h in &handles {
        *writers.entry(h.header.writer).or_default() += 1;
    }
    let mut w: Vec<_> = writers.into_iter().collect();
    w.sort_by_key(|(k, _)| k.code());
    rep.writers = w;

    // rule 1, from metadata: per source, each block starts after the previous one ended
    let mut last_of: HashMap<u64, (usize, u64, u64)> = HashMap::new(); // source -> (segment index, offset, last seq)
    for (si, h) in handles.iter().enumerate() {
        for b in &h.blocks {
            rep.blocks += 1;
            if b.record_count == 0 {
                continue;
            }
            for src in &b.sources {
                if let Some(&(psi, poff, plast)) = last_of.get(src) {
                    if b.first_seq <= plast {
                        rep.add(
                            Finding::SeqBackwards {
                                source: *src,
                                segment: h.path().to_path_buf(),
                                offset: b.offset,
                                first_seq: b.first_seq,
                                prev_segment: handles[psi].path().to_path_buf(),
                                prev_offset: poff,
                                prev_last_seq: plast,
                            },
                            max,
                        );
                    }
                }
                let e = last_of.entry(*src).or_insert((si, b.offset, b.last_seq));
                if b.last_seq >= e.2 {
                    *e = (si, b.offset, b.last_seq);
                }
            }
        }
    }

    // rule 3: no earlier segment holds a seq above the newest (non-empty) segment's highest
    if let Some(ni) = handles.iter().rposition(|h| !h.blocks.is_empty()) {
        let newest_max = handles[ni].max_seq();
        for h in &handles[..ni] {
            if h.max_seq() > newest_max {
                rep.add(
                    Finding::SealedAboveNewest { segment: h.path().to_path_buf(), max_seq: h.max_seq(), newest: handles[ni].path().to_path_buf(), newest_max_seq: newest_max },
                    max,
                );
            }
        }
    }

    rep.wal_records = wal.len();
    if !opts.full {
        return rep;
    }

    // rule 2 and in-block order: read every record
    let all = handles.iter().flat_map(|h| h.blocks.iter());
    let min = all.clone().map(|b| b.first_seq).min().unwrap_or(0);
    let maxs = all.clone().map(|b| b.last_seq).max().unwrap_or(0);
    let total: u64 = all.map(|b| b.record_count as u64).sum();
    let mut seen = Seen::new(min, maxs, total);
    let mut where_of: Vec<(u64, usize, u64)> = Vec::new(); // sparse mode: (seq, segment, offset)
    for (si, h) in handles.iter().enumerate() {
        for (bi, b) in h.blocks.iter().enumerate() {
            let v = match h.read_block(bi) {
                Ok(v) => v,
                Err(e) => {
                    rep.add(Finding::Unreadable { segment: h.path().to_path_buf(), offset: Some(b.offset), error: format!("{e:#}") }, max);
                    continue;
                }
            };
            let mut prev: HashMap<u64, u64> = HashMap::new();
            for r in v.iter() {
                rep.records += 1;
                let seq = r.seq();
                let src = r.source().fingerprint();
                if let Some(&p) = prev.get(&src) {
                    if seq <= p {
                        rep.add(Finding::SeqBackwardsInBlock { source: src, segment: h.path().to_path_buf(), offset: b.offset, seq, prev_seq: p }, max);
                    }
                }
                prev.insert(src, seq);
                if !seen.insert(seq) {
                    rep.add(Finding::DuplicateSeq { seq, segment: h.path().to_path_buf(), offset: b.offset }, max);
                }
                if seen.sparse.is_some() {
                    where_of.push((seq, si, b.offset));
                }
            }
        }
    }
    if let Some(v) = &mut seen.sparse {
        v.sort_unstable();
        where_of.sort_unstable();
        for w in where_of.windows(2) {
            if w[0].0 == w[1].0 {
                rep.add(Finding::DuplicateSeq { seq: w[1].0, segment: handles[w[1].1].path().to_path_buf(), offset: w[1].2 }, max);
            }
        }
    }

    // hidden WAL records: treated as sealed by SealedSeqs, but in no segment
    let sealed = SealedSeqs::from_handles(&handles);
    let mut hidden: Vec<u64> = wal.iter().filter(|r| sealed.contains(r) && !seen.contains(r.seq)).map(|r| r.seq).collect();
    hidden.sort_unstable();
    hidden.dedup();
    for seq in hidden {
        rep.add(Finding::HiddenWalRecord { seq }, max);
    }
    rep
}
