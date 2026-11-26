//! Write-ahead log: records not yet sealed into a block.
//!
//! A WAL file is a sequence of frames: u32 len, u32 crc32c(bytes), bytes, u32 len. The bytes are a
//! one-record block payload, so the WAL shares the block encoding. Files are named by the first seq
//! they hold and deleted once every record in them is sealed.

use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use wartlog_model::Record;

use crate::block::{BlockBuilder, BlockView};
use crate::format::get_u32;

pub fn wal_dir(root: &Path) -> PathBuf {
    root.join("wal")
}

pub fn encode_frame(r: &Record) -> Vec<u8> {
    let mut b = BlockBuilder::new();
    b.push(r);
    let (p, _) = b.finish();
    let mut out = Vec::with_capacity(p.len() + 12);
    out.extend_from_slice(&(p.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32c::crc32c(&p).to_le_bytes());
    out.extend_from_slice(&p);
    out.extend_from_slice(&(p.len() as u32).to_le_bytes());
    out
}

/// Decodes every intact frame; stops at the first torn or damaged frame. Returns (records, valid bytes).
pub fn decode_frames(b: &[u8]) -> (Vec<Record>, usize) {
    let mut out = Vec::new();
    let mut o = 0;
    while o + 12 <= b.len() {
        let len = get_u32(b, o) as usize;
        if o + 12 + len > b.len() {
            break;
        }
        let crc = get_u32(b, o + 4);
        let p = &b[o + 8..o + 8 + len];
        if crc32c::crc32c(p) != crc || get_u32(b, o + 8 + len) as usize != len {
            break;
        }
        match BlockView::parse(p.to_vec(), 1) {
            Ok(v) => out.push(v.rec(0).to_record()),
            Err(_) => break,
        }
        o += 12 + len;
    }
    (out, o)
}

/// WAL files in seq order.
pub fn list(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(wal_dir(root))
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "wal")).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Reads every intact record from every WAL file.
pub fn read_all(root: &Path) -> Vec<Record> {
    let mut out = Vec::new();
    for p in list(root) {
        if let Ok(b) = std::fs::read(&p) {
            out.extend(decode_frames(&b).0);
        }
    }
    out
}

/// The WAL file being appended to.
pub struct WalFile {
    pub path: PathBuf,
    w: BufWriter<File>,
    pub max_seq: u64,
    pub bytes: u64,
    pub created: std::time::Instant,
    dirty: bool,
}

impl WalFile {
    pub fn create(root: &Path, first_seq: u64) -> Result<WalFile> {
        std::fs::create_dir_all(wal_dir(root))?;
        let path = wal_dir(root).join(format!("{first_seq:020}.wal"));
        let f = OpenOptions::new().create(true).append(true).open(&path)?;
        crate::set_private(&path);
        Ok(WalFile { path, w: BufWriter::with_capacity(1 << 20, f), max_seq: 0, bytes: 0, created: std::time::Instant::now(), dirty: false })
    }

    pub fn append(&mut self, r: &Record) -> Result<()> {
        let f = encode_frame(r);
        self.w.write_all(&f)?;
        self.bytes += f.len() as u64;
        self.max_seq = self.max_seq.max(r.seq);
        self.dirty = true;
        Ok(())
    }

    /// Flushes and fdatasyncs if anything was written since the last sync.
    pub fn sync(&mut self) -> Result<()> {
        if self.dirty {
            self.w.flush()?;
            self.w.get_ref().sync_data()?;
            self.dirty = false;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.bytes == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::tests::sample;

    #[test]
    fn frames_round_trip_and_torn_tail() {
        let mut b = Vec::new();
        for i in 1..=5 {
            b.extend(encode_frame(&sample(i)));
        }
        let (r, n) = decode_frames(&b);
        assert_eq!(r.len(), 5);
        assert_eq!(n, b.len());
        assert_eq!(r[2], sample(3));
        let (r, _) = decode_frames(&b[..b.len() - 3]);
        assert_eq!(r.len(), 4);
        let mut d = b.clone();
        let l = d.len();
        d[l - 20] ^= 1;
        assert_eq!(decode_frames(&d).0.len(), 4);
    }
}
