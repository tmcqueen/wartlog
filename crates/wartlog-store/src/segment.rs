//! Reading segment files: block discovery by scanning (with resync past damage) and block reads.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::block::{decompress, BlockBuilder, BlockView};
use crate::format::*;

/// Everything planning needs about one block, from the term index table or from a scan.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockMeta {
    pub offset: u64,
    pub block_seq: u64,
    pub codec: u8,
    pub compressed_len: u32,
    pub uncompressed_len: u32,
    pub record_count: u32,
    pub payload_crc: u32,
    pub min_time: i64,
    pub max_time: i64,
    pub severities: u32,
    pub span_count: u32,
    pub first_seq: u64,
    pub last_seq: u64,
    pub max_observed: i64,
    /// sorted source fingerprints
    pub sources: Vec<u64>,
}

impl BlockMeta {
    pub fn from_summary(offset: u64, h: &BlockHeader, s: &Summary) -> BlockMeta {
        BlockMeta {
            offset,
            block_seq: h.block_seq,
            codec: h.codec,
            compressed_len: h.compressed_len,
            uncompressed_len: h.uncompressed_len,
            record_count: h.record_count,
            payload_crc: h.payload_crc,
            min_time: s.min_time,
            max_time: s.max_time,
            severities: s.severities,
            span_count: s.span_count,
            first_seq: s.first_seq,
            last_seq: s.last_seq,
            max_observed: s.max_observed,
            sources: s.sources.clone(),
        }
    }

    pub fn header(&self) -> BlockHeader {
        BlockHeader {
            codec: self.codec,
            block_seq: self.block_seq,
            record_count: self.record_count,
            uncompressed_len: self.uncompressed_len,
            compressed_len: self.compressed_len,
            summary_len: 0,
            payload_crc: self.payload_crc,
            summary_crc: 0,
        }
    }

    pub fn has_logs(&self) -> bool {
        self.record_count > self.span_count
    }
}

/// Result of scanning a segment.
#[derive(Debug, Default)]
pub struct Scan {
    pub header: Option<SegmentHeader>,
    pub blocks: Vec<BlockMeta>,
    /// byte ranges skipped because they did not hold a valid block
    pub damaged: Vec<(u64, u64)>,
    /// end of the last valid block (where a writer may append)
    pub valid_len: u64,
    /// summaries that failed their CRC and were recomputed from the payload
    pub recomputed_summaries: usize,
}

/// Memory-mapped segment file. A growing (open) segment is mapped up to its length at open time.
pub struct Segment {
    pub path: PathBuf,
    map: memmap2::Mmap,
}

impl Segment {
    pub fn open(path: &Path) -> Result<Segment> {
        let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
        // SAFETY: segments are append-only and never modified in place by WartLog; a reader maps
        // only the bytes present at open time and validates every structure it reads.
        let map = unsafe { memmap2::Mmap::map(&f)? };
        Ok(Segment { path: path.to_path_buf(), map })
    }

    pub fn len(&self) -> u64 {
        self.map.len() as u64
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn bytes(&self) -> &[u8] {
        &self.map
    }

    pub fn header(&self) -> Result<SegmentHeader> {
        SegmentHeader::decode(&self.map)
    }

    /// Walks every block. With `verify_payload`, also checks payload CRCs (needed to recompute
    /// a damaged summary and for verification; planning does not need it).
    pub fn scan(&self, verify_payload: bool) -> Scan {
        scan_bytes(&self.map, verify_payload)
    }

    /// Reads, verifies and decodes one block.
    pub fn read_block(&self, m: &BlockMeta) -> Result<BlockView> {
        let start = m.offset as usize + BLOCK_HEADER_LEN;
        let end = start + m.compressed_len as usize;
        let payload = self.map.get(start..end).context("block beyond end of segment")?;
        let p = decompress(&m.header(), payload)?;
        BlockView::parse(p, m.record_count)
    }
}

pub fn scan_bytes(b: &[u8], verify_payload: bool) -> Scan {
    let mut out = Scan::default();
    let header = match SegmentHeader::decode(b) {
        Ok(h) => h,
        Err(_) => return out,
    };
    out.header = Some(header);
    let mut off = SEG_HEADER_LEN;
    out.valid_len = off as u64;
    while off + BLOCK_HEADER_LEN <= b.len() {
        match try_block(b, off, verify_payload) {
            Some((meta, total, recomputed)) => {
                if recomputed {
                    out.recomputed_summaries += 1;
                }
                out.blocks.push(meta);
                off += total;
                out.valid_len = off as u64;
            }
            None => {
                // resync: find the next offset holding a valid block
                let from = off;
                let mut next = None;
                let mut i = off + 1;
                while i + BLOCK_HEADER_LEN <= b.len() {
                    if get_u32(b, i) == BLOCK_MAGIC && try_block(b, i, verify_payload).is_some() {
                        next = Some(i);
                        break;
                    }
                    i += 1;
                }
                match next {
                    Some(n) => {
                        out.damaged.push((from as u64, n as u64));
                        off = n;
                    }
                    None => {
                        // a torn tail is normal for a segment being written; report it only
                        // when there is more than a partial block worth of bytes
                        if b.len() > from {
                            out.damaged.push((from as u64, b.len() as u64));
                        }
                        break;
                    }
                }
            }
        }
    }
    out
}

fn try_block(b: &[u8], off: usize, verify_payload: bool) -> Option<(BlockMeta, usize, bool)> {
    let h = BlockHeader::decode(&b[off..])?;
    let total = h.total_len();
    if off + total > b.len() {
        return None;
    }
    let t0 = off + total - BLOCK_TRAILER_LEN;
    if decode_trailer(&b[t0..], h.block_seq)? as usize != total {
        return None;
    }
    let p0 = off + BLOCK_HEADER_LEN;
    let payload = &b[p0..p0 + h.compressed_len as usize];
    let s0 = p0 + h.compressed_len as usize;
    let sum = &b[s0..s0 + h.summary_len as usize];
    let summary_ok = crc32c::crc32c(sum) == h.summary_crc;
    if summary_ok && !verify_payload {
        let s = Summary::decode(sum).ok()?;
        return Some((BlockMeta::from_summary(off as u64, &h, &s), total, false));
    }
    // verify the payload; recompute a damaged summary from it (the block's records are intact)
    let p = decompress(&h, payload).ok()?;
    let s = if summary_ok {
        Summary::decode(sum).ok()?
    } else {
        recompute_summary(p, h.record_count).ok()?
    };
    Some((BlockMeta::from_summary(off as u64, &h, &s), total, !summary_ok))
}

fn recompute_summary(p: Vec<u8>, count: u32) -> Result<Summary> {
    let v = BlockView::parse(p, count)?;
    let mut b = BlockBuilder::new();
    for r in v.iter() {
        b.push(&r.to_record());
    }
    Ok(b.finish().1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{encode_block, tests::sample};

    fn segment_bytes(blocks: usize) -> Vec<u8> {
        let mut out = SegmentHeader { id: [1; 16], prev: [0; 16], created_ns: 0 }.encode();
        let mut seq = 1;
        for bs in 0..blocks {
            let mut b = BlockBuilder::new();
            for _ in 0..20 {
                b.push(&sample(seq));
                seq += 1;
            }
            let (p, s) = b.finish();
            out.extend(encode_block(bs as u64, &p, &s, 1));
        }
        out
    }

    #[test]
    fn scan_finds_all_blocks_and_survives_damage() {
        let b = segment_bytes(5);
        let s = scan_bytes(&b, true);
        assert_eq!(s.blocks.len(), 5);
        assert!(s.damaged.is_empty());
        assert_eq!(s.valid_len as usize, b.len());

        // damage block 2's header: blocks 0,1,3,4 survive
        let mut d = b.clone();
        let off = s.blocks[2].offset as usize;
        d[off + 10] ^= 0xff;
        let s2 = scan_bytes(&d, true);
        assert_eq!(s2.blocks.iter().map(|m| m.block_seq).collect::<Vec<_>>(), vec![0, 1, 3, 4]);
        assert_eq!(s2.damaged.len(), 1);

        // torn tail
        let s3 = scan_bytes(&b[..b.len() - 7], false);
        assert_eq!(s3.blocks.len(), 4);
        assert_eq!(s3.valid_len, s.blocks[4].offset);
    }

    #[test]
    fn damaged_summary_is_recomputed() {
        let b = segment_bytes(2);
        let s = scan_bytes(&b, true);
        let m = &s.blocks[0];
        let mut d = b.clone();
        // the summary sits right after the payload
        let sum_off = m.offset as usize + BLOCK_HEADER_LEN + m.compressed_len as usize;
        d[sum_off + 2] ^= 0xff;
        let s2 = scan_bytes(&d, true);
        assert_eq!(s2.blocks.len(), 2);
        assert_eq!(s2.recomputed_summaries, 1);
        assert_eq!(s2.blocks[0], s.blocks[0]);
    }
}
