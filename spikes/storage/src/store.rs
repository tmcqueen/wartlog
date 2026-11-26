//! Segment writer, block scanner, and the rebuildable block index (.widx).

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::format::*;

pub const IDX_MAGIC: &[u8; 8] = b"WARTIDX\0";

pub struct Options {
    /// one open block per source (stream) instead of one shared block
    pub per_source: bool,
    /// seal a source's block when its event-time span exceeds this
    pub block_age_ns: i64,
    pub block_target: usize,
    pub segment_target: u64,
    pub zstd_level: i32,
    /// build the .wtix term index when a segment closes
    pub term_index: bool,
}

/// One block as found in a segment, with its raw summary bytes.
#[derive(Clone)]
pub struct BlockLoc {
    pub offset: u64,
    pub block_seq: u64,
    pub total_len: u32,
    pub record_count: u32,
    pub uncompressed_len: u32,
    pub compressed_len: u32,
    pub payload_crc: u32,
    pub summary: Vec<u8>,
}

pub fn segments_dir(root: &Path) -> PathBuf { root.join("segments") }

pub fn list_segments(root: &Path) -> Result<Vec<PathBuf>> {
    let dir = segments_dir(root);
    let mut v: Vec<PathBuf> = match fs::read_dir(&dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|e| e == "wseg").unwrap_or(false)).collect(),
        Err(_) => vec![],
    };
    v.sort();
    Ok(v)
}

pub fn idx_path(seg: &Path) -> PathBuf { seg.with_extension("widx") }

// ---------------- writer ----------------

pub struct Writer {
    root: PathBuf,
    opts: Options,
    seg: Option<OpenSegment>,
    prev_id: [u8; 16],
    builders: HashMap<u64, BlockBuilder>,
    pub blocks_written: u64,
    pub bytes_written: u64,
    counter: u64,
}

struct OpenSegment {
    path: PathBuf,
    id: [u8; 16],
    file: BufWriter<File>,
    len: u64,
    next_block_seq: u64,
    entries: Vec<BlockLoc>,
}

fn new_id(counter: u64) -> [u8; 16] {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    let a = hash(&[now.to_le_bytes(), counter.to_le_bytes(), (std::process::id() as u64).to_le_bytes()].concat());
    let b = hash(&a.to_le_bytes());
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&now.to_be_bytes()); // time-ordered like UUIDv7
    id[8..].copy_from_slice(&(a ^ b).to_le_bytes());
    id
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

impl Writer {
    pub fn create(root: &Path, opts: Options) -> Result<Writer> {
        fs::create_dir_all(segments_dir(root))?;
        Ok(Writer {
            root: root.to_path_buf(), opts, seg: None, prev_id: [0; 16],
            builders: HashMap::new(), blocks_written: 0, bytes_written: 0, counter: 0,
        })
    }

    pub fn push(&mut self, r: &Record) -> Result<()> {
        let key = if self.opts.per_source { r.source.fingerprint() } else { 0 };
        let stale = self.builders.get(&key).map(|b| !b.is_empty() && r.time_ns - b.summary.min_time > self.opts.block_age_ns).unwrap_or(false);
        if stale { self.flush_block(key)?; }
        let b = self.builders.entry(key).or_insert_with(BlockBuilder::new);
        b.push(r);
        if b.approx_len() >= self.opts.block_target { self.flush_block(key)?; }
        Ok(())
    }

    fn open_segment(&mut self) -> Result<()> {
        self.counter += 1;
        let id = new_id(self.counter);
        let path = segments_dir(&self.root).join(format!("{}.wseg", hex(&id)));
        let mut file = BufWriter::with_capacity(1 << 20, File::create(&path)?);
        let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as i64;
        let h = encode_segment_header(id, self.prev_id, created);
        file.write_all(&h)?;
        self.seg = Some(OpenSegment { path, id, file, len: h.len() as u64, next_block_seq: 0, entries: vec![] });
        Ok(())
    }

    pub fn flush_block(&mut self, key: u64) -> Result<()> {
        let builder = match self.builders.remove(&key) { Some(b) if !b.is_empty() => b, _ => return Ok(()) };
        if self.seg.is_none() { self.open_segment()?; }
        let count = builder.summary.count;
        let (payload, summary) = builder.finish();
        let seg = self.seg.as_mut().unwrap();
        let seq = seg.next_block_seq;
        let bytes = encode_block(seq, count, &payload, &summary, self.opts.zstd_level);
        let h = BlockHeader::decode(&bytes[..BLOCK_HEADER_LEN]).unwrap();
        let sum_off = BLOCK_HEADER_LEN + h.compressed_len as usize;
        seg.entries.push(BlockLoc {
            offset: seg.len, block_seq: seq, total_len: bytes.len() as u32, record_count: count,
            uncompressed_len: h.uncompressed_len, compressed_len: h.compressed_len, payload_crc: h.payload_crc,
            summary: bytes[sum_off..sum_off + h.summary_len as usize].to_vec(),
        });
        seg.file.write_all(&bytes)?;
        seg.len += bytes.len() as u64;
        seg.next_block_seq += 1;
        self.blocks_written += 1;
        self.bytes_written += bytes.len() as u64;
        if seg.len >= self.opts.segment_target { self.close_segment()?; }
        Ok(())
    }

    fn close_segment(&mut self) -> Result<()> {
        if let Some(mut seg) = self.seg.take() {
            seg.file.flush()?;
            seg.file.get_ref().sync_all()?;
            write_index(&idx_path(&seg.path), &seg.id, seg.len, &seg.entries)?;
            if self.opts.term_index { crate::tix::write(&seg.path)?; }
            self.prev_id = seg.id;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<(u64, u64)> {
        let mut keys: Vec<u64> = self.builders.keys().copied().collect();
        keys.sort_unstable();
        for k in keys { self.flush_block(k)?; }
        self.close_segment()?;
        Ok((self.blocks_written, self.bytes_written))
    }
}

// ---------------- index ----------------

pub fn encode_index(seg_id: &[u8; 16], seg_len: u64, entries: &[BlockLoc]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(IDX_MAGIC);
    put_u16(&mut b, 1);
    put_u16(&mut b, 0);
    put_u32(&mut b, entries.len() as u32);
    b.extend_from_slice(seg_id);
    put_u64(&mut b, entries.last().map(|e| e.block_seq + 1).unwrap_or(0)); // blocks covered
    put_u64(&mut b, seg_len);
    for e in entries {
        put_u64(&mut b, e.offset);
        put_u64(&mut b, e.block_seq);
        put_u32(&mut b, e.total_len);
        put_u32(&mut b, e.record_count);
        put_u32(&mut b, e.uncompressed_len);
        put_u32(&mut b, e.compressed_len);
        put_u32(&mut b, e.payload_crc);
        put_u32(&mut b, e.summary.len() as u32);
        b.extend_from_slice(&e.summary);
    }
    let crc = crc32c::crc32c(&b);
    put_u32(&mut b, crc);
    b
}

pub fn write_index(path: &Path, seg_id: &[u8; 16], seg_len: u64, entries: &[BlockLoc]) -> Result<()> {
    let tmp = path.with_extension("widx.tmp");
    fs::write(&tmp, encode_index(seg_id, seg_len, entries))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Loads an index and checks it against its segment. Returns None if missing or invalid.
pub fn load_index(seg: &Path) -> Result<Option<Vec<BlockLoc>>> {
    let b = match fs::read(idx_path(seg)) { Ok(b) => b, Err(_) => return Ok(None) };
    if b.len() < 48 || &b[0..8] != IDX_MAGIC { return Ok(None); }
    let n = b.len();
    if crc32c::crc32c(&b[..n - 4]) != get_u32(&b, n - 4) { return Ok(None); }
    let count = get_u32(&b, 12) as usize;
    let id: [u8; 16] = b[16..32].try_into().unwrap();
    let seg_len = get_u64(&b, 40);
    let f = File::open(seg)?;
    let actual_len = f.metadata()?.len();
    let mut hdr = [0u8; SEG_HEADER_LEN];
    f.read_exact_at(&mut hdr, 0)?;
    if decode_segment_header(&hdr).ok() != Some(id) || actual_len != seg_len { return Ok(None); }
    let mut o = 48;
    let mut v = Vec::with_capacity(count);
    for _ in 0..count {
        let sl = get_u32(&b, o + 36) as usize;
        v.push(BlockLoc {
            offset: get_u64(&b, o), block_seq: get_u64(&b, o + 8), total_len: get_u32(&b, o + 16),
            record_count: get_u32(&b, o + 20), uncompressed_len: get_u32(&b, o + 24),
            compressed_len: get_u32(&b, o + 28), payload_crc: get_u32(&b, o + 32),
            summary: b[o + 40..o + 40 + sl].to_vec(),
        });
        o += 40 + sl;
    }
    Ok(Some(v))
}

// ---------------- scanning without an index ----------------

pub struct ScanReport {
    pub blocks: Vec<BlockLoc>,
    pub damaged_regions: Vec<(u64, u64)>,
}

/// Walks a segment's blocks by their headers, reading only header + summary per block.
/// If `verify_payload`, also checks each payload CRC (reads the whole file).
/// On a bad header, resyncs by searching for the next block magic whose header CRC verifies.
pub fn scan_segment(seg: &Path, verify_payload: bool) -> Result<([u8; 16], u64, ScanReport)> {
    let f = File::open(seg).with_context(|| format!("open {}", seg.display()))?;
    let len = f.metadata()?.len();
    let mut hdr = [0u8; SEG_HEADER_LEN];
    f.read_exact_at(&mut hdr, 0)?;
    let id = decode_segment_header(&hdr)?;
    let mut off = SEG_HEADER_LEN as u64;
    let mut blocks = Vec::new();
    let mut damaged = Vec::new();
    let mut hb = [0u8; BLOCK_HEADER_LEN];
    while off + (BLOCK_HEADER_LEN + BLOCK_TRAILER_LEN) as u64 <= len {
        f.read_exact_at(&mut hb, off)?;
        let good = BlockHeader::decode(&hb).and_then(|h| {
            let total = h.total_len() as u64;
            if off + total > len { return None; }
            let mut tr = [0u8; BLOCK_TRAILER_LEN];
            f.read_exact_at(&mut tr, off + total - BLOCK_TRAILER_LEN as u64).ok()?;
            if get_u32(&tr, 0) as u64 != total || get_u32(&tr, 12) != TRAILER_MAGIC
                || crc32c::crc32c(&tr[..8]) != get_u32(&tr, 8) { return None; }
            let mut sum = vec![0u8; h.summary_len as usize];
            f.read_exact_at(&mut sum, off + (BLOCK_HEADER_LEN + h.compressed_len as usize) as u64).ok()?;
            if crc32c::crc32c(&sum) != h.summary_crc { return None; }
            if verify_payload {
                let mut p = vec![0u8; h.compressed_len as usize];
                f.read_exact_at(&mut p, off + BLOCK_HEADER_LEN as u64).ok()?;
                if crc32c::crc32c(&p) != h.payload_crc { return None; }
            }
            Some(BlockLoc {
                offset: off, block_seq: h.block_seq, total_len: total as u32, record_count: h.record_count,
                uncompressed_len: h.uncompressed_len, compressed_len: h.compressed_len,
                payload_crc: h.payload_crc, summary: sum,
            })
        });
        match good {
            Some(b) => { off += b.total_len as u64; blocks.push(b); }
            None => {
                // resync: find the next magic after this offset
                let start = off;
                let next = find_next_block(&f, off + 1, len)?;
                damaged.push((start, next.unwrap_or(len)));
                match next { Some(n) => off = n, None => break }
            }
        }
    }
    Ok((id, len, ScanReport { blocks, damaged_regions: damaged }))
}

fn find_next_block(f: &File, mut off: u64, len: u64) -> Result<Option<u64>> {
    let magic = BLOCK_MAGIC.to_le_bytes();
    let mut buf = vec![0u8; 1 << 20];
    while off + BLOCK_HEADER_LEN as u64 <= len {
        let n = ((len - off) as usize).min(buf.len());
        f.read_exact_at(&mut buf[..n], off)?;
        let mut i = 0;
        while i + 4 <= n {
            if buf[i..i + 4] == magic {
                let cand = off + i as u64;
                let mut hb = [0u8; BLOCK_HEADER_LEN];
                if cand + BLOCK_HEADER_LEN as u64 <= len {
                    f.read_exact_at(&mut hb, cand)?;
                    if BlockHeader::decode(&hb).is_some() { return Ok(Some(cand)); }
                }
            }
            i += 1;
        }
        if n < 4 { break; }
        off += (n - 3) as u64;
    }
    Ok(None)
}

/// Rebuilds a segment's index purely from the segment file.
pub fn rebuild_index(seg: &Path) -> Result<(usize, Vec<(u64, u64)>)> {
    let (id, len, rep) = scan_segment(seg, true)?;
    write_index(&idx_path(seg), &id, len, &rep.blocks)?;
    Ok((rep.blocks.len(), rep.damaged_regions))
}

/// Reads and decompresses one block's payload, verifying its CRC.
pub fn read_payload(f: &File, b: &BlockLoc) -> Result<Vec<u8>> {
    let mut p = vec![0u8; b.compressed_len as usize];
    f.read_exact_at(&mut p, b.offset + BLOCK_HEADER_LEN as u64)?;
    if crc32c::crc32c(&p) != b.payload_crc { bail!("payload CRC mismatch at offset {}", b.offset); }
    Ok(zstd::bulk::decompress(&p, b.uncompressed_len as usize)?)
}
