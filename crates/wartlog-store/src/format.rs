//! Fixed binary structures (spec section 7, and docs/format.md). Little-endian, explicit byte
//! encoding, CRC32C checksums. Nothing is transmuted from file bytes.

use anyhow::{bail, Result};

pub const SEG_MAGIC: &[u8; 8] = b"WARTSEG\0";
pub const SEG_VERSION: u16 = 1;
pub const SEG_HEADER_LEN: usize = 64;
pub const BLOCK_MAGIC: u32 = u32::from_le_bytes(*b"WBLK");
pub const TRAILER_MAGIC: u32 = u32::from_le_bytes(*b"KLBW");
pub const BLOCK_VERSION: u16 = 1;
pub const BLOCK_HEADER_LEN: usize = 48;
pub const BLOCK_TRAILER_LEN: usize = 16;
pub const RECORD_FIXED_LEN: usize = 72;
pub const SPAN_EXT_LEN: usize = 28;
pub const NONE_REF: u32 = u32::MAX;

pub const CODEC_NONE: u8 = 0;
pub const CODEC_ZSTD: u8 = 1;

// record flags
pub const F_TRACE: u16 = 1;
pub const F_SPAN: u16 = 2;
pub const F_TEMPLATE: u16 = 4;
pub const F_BODY_STRING: u16 = 8;
pub const F_SEVERITY_TEXT: u16 = 16;

// span extension flags
pub const SF_PARENT: u16 = 1;

// typed value tags
pub const T_NULL: u8 = 0;
pub const T_BOOL: u8 = 1;
pub const T_I64: u8 = 2;
pub const T_F64: u8 = 3;
pub const T_STR: u8 = 4;
pub const T_BYTES: u8 = 5;
pub const T_TIMESTAMP: u8 = 6;
pub const T_DURATION: u8 = 7;
pub const T_JSON: u8 = 8;

pub fn put_u16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}
pub fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
pub fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}
pub fn put_i64(b: &mut Vec<u8>, v: i64) {
    b.extend_from_slice(&v.to_le_bytes());
}
pub fn get_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
pub fn get_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub fn get_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
pub fn get_i64(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Bounds-checked little-endian reader for parsing untrusted bytes.
pub struct Cursor<'a> {
    pub b: &'a [u8],
    pub o: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Cursor { b, o: 0 }
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.o + n > self.b.len() {
            bail!("truncated structure at byte {}", self.o);
        }
        let s = &self.b[self.o..self.o + n];
        self.o += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into()?))
    }
    pub fn align8(&mut self) {
        self.o = (self.o + 7) & !7;
    }
}

// ---------- segment header ----------

#[derive(Clone, Debug, PartialEq)]
pub struct SegmentHeader {
    pub id: [u8; 16],
    pub prev: [u8; 16],
    pub created_ns: i64,
}

impl SegmentHeader {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(SEG_HEADER_LEN);
        b.extend_from_slice(SEG_MAGIC);
        put_u16(&mut b, SEG_VERSION);
        put_u16(&mut b, 0);
        put_u32(&mut b, SEG_HEADER_LEN as u32);
        b.extend_from_slice(&self.id);
        b.extend_from_slice(&self.prev);
        put_i64(&mut b, self.created_ns);
        b.extend_from_slice(&[0u8; 4]);
        let crc = crc32c::crc32c(&b);
        put_u32(&mut b, crc);
        debug_assert_eq!(b.len(), SEG_HEADER_LEN);
        b
    }

    pub fn decode(b: &[u8]) -> Result<SegmentHeader> {
        if b.len() < SEG_HEADER_LEN || &b[0..8] != SEG_MAGIC {
            bail!("not a WartLog segment");
        }
        if crc32c::crc32c(&b[0..60]) != get_u32(b, 60) {
            bail!("segment header CRC mismatch");
        }
        let v = get_u16(b, 8);
        if v != SEG_VERSION {
            bail!("unsupported segment version {v}");
        }
        Ok(SegmentHeader {
            id: b[16..32].try_into().unwrap(),
            prev: b[32..48].try_into().unwrap(),
            created_ns: get_i64(b, 48),
        })
    }
}

// ---------- block header and trailer ----------

#[derive(Clone, Debug, PartialEq)]
pub struct BlockHeader {
    pub codec: u8,
    pub block_seq: u64,
    pub record_count: u32,
    pub uncompressed_len: u32,
    pub compressed_len: u32,
    pub summary_len: u32,
    pub payload_crc: u32,
    pub summary_crc: u32,
}

impl BlockHeader {
    pub fn total_len(&self) -> usize {
        BLOCK_HEADER_LEN + self.compressed_len as usize + self.summary_len as usize + BLOCK_TRAILER_LEN
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(BLOCK_HEADER_LEN);
        put_u32(&mut b, BLOCK_MAGIC);
        put_u16(&mut b, BLOCK_VERSION);
        b.push(self.codec);
        b.push(0);
        put_u64(&mut b, self.block_seq);
        put_u32(&mut b, self.record_count);
        put_u32(&mut b, self.uncompressed_len);
        put_u32(&mut b, self.compressed_len);
        put_u32(&mut b, self.summary_len);
        put_u32(&mut b, self.payload_crc);
        put_u32(&mut b, self.summary_crc);
        put_u32(&mut b, 0);
        let crc = crc32c::crc32c(&b);
        put_u32(&mut b, crc);
        b
    }

    /// None when the bytes are not a valid header (bad magic, version or CRC).
    pub fn decode(b: &[u8]) -> Option<BlockHeader> {
        if b.len() < BLOCK_HEADER_LEN || get_u32(b, 0) != BLOCK_MAGIC {
            return None;
        }
        if crc32c::crc32c(&b[0..44]) != get_u32(b, 44) || get_u16(b, 4) != BLOCK_VERSION {
            return None;
        }
        Some(BlockHeader {
            codec: b[6],
            block_seq: get_u64(b, 8),
            record_count: get_u32(b, 16),
            uncompressed_len: get_u32(b, 20),
            compressed_len: get_u32(b, 24),
            summary_len: get_u32(b, 28),
            payload_crc: get_u32(b, 32),
            summary_crc: get_u32(b, 36),
        })
    }
}

pub fn encode_trailer(total_len: u32, block_seq: u64) -> [u8; BLOCK_TRAILER_LEN] {
    let mut t = Vec::with_capacity(16);
    put_u32(&mut t, total_len);
    put_u32(&mut t, block_seq as u32);
    let crc = crc32c::crc32c(&t);
    put_u32(&mut t, crc);
    put_u32(&mut t, TRAILER_MAGIC);
    t.try_into().unwrap()
}

/// Returns total_len if the trailer is valid.
pub fn decode_trailer(t: &[u8], block_seq: u64) -> Option<u32> {
    if t.len() < BLOCK_TRAILER_LEN || get_u32(t, 12) != TRAILER_MAGIC {
        return None;
    }
    if crc32c::crc32c(&t[0..8]) != get_u32(t, 8) || get_u32(t, 4) != block_seq as u32 {
        return None;
    }
    Some(get_u32(t, 0))
}

// ---------- block summary ----------

/// Stored uncompressed after the payload, so pruning never decompresses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub min_time: i64,
    pub max_time: i64,
    pub first_seq: u64,
    pub last_seq: u64,
    pub log_count: u32,
    pub span_count: u32,
    /// bit n set when a record has severity number n
    pub severities: u32,
    pub max_observed: i64,
    /// sorted source fingerprints
    pub sources: Vec<u64>,
    /// sorted attribute key hashes
    pub keys: Vec<u64>,
}

impl Summary {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64 + 8 * (self.sources.len() + self.keys.len()));
        put_i64(&mut b, self.min_time);
        put_i64(&mut b, self.max_time);
        put_u64(&mut b, self.first_seq);
        put_u64(&mut b, self.last_seq);
        put_u32(&mut b, self.log_count);
        put_u32(&mut b, self.span_count);
        put_u32(&mut b, self.severities);
        put_u32(&mut b, 0);
        put_i64(&mut b, self.max_observed);
        put_u32(&mut b, self.sources.len() as u32);
        for s in &self.sources {
            put_u64(&mut b, *s);
        }
        put_u32(&mut b, self.keys.len() as u32);
        for k in &self.keys {
            put_u64(&mut b, *k);
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Summary> {
        let mut c = Cursor::new(b);
        let min_time = c.i64()?;
        let max_time = c.i64()?;
        let first_seq = c.u64()?;
        let last_seq = c.u64()?;
        let log_count = c.u32()?;
        let span_count = c.u32()?;
        let severities = c.u32()?;
        c.u32()?;
        let max_observed = c.i64()?;
        let n = c.u32()? as usize;
        let sources = (0..n).map(|_| c.u64()).collect::<Result<_>>()?;
        let n = c.u32()? as usize;
        let keys = (0..n).map(|_| c.u64()).collect::<Result<_>>()?;
        Ok(Summary { min_time, max_time, first_seq, last_seq, log_count, span_count, severities, max_observed, sources, keys })
    }

    pub fn record_count(&self) -> u32 {
        self.log_count + self.span_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_round_trip_and_reject_damage() {
        let h = SegmentHeader { id: [7; 16], prev: [1; 16], created_ns: 42 };
        let mut b = h.encode();
        assert_eq!(SegmentHeader::decode(&b).unwrap(), h);
        b[20] ^= 1;
        assert!(SegmentHeader::decode(&b).is_err());

        let bh = BlockHeader { codec: 1, block_seq: 9, record_count: 3, uncompressed_len: 100, compressed_len: 50, summary_len: 80, payload_crc: 1, summary_crc: 2 };
        let mut e = bh.encode();
        assert_eq!(e.len(), BLOCK_HEADER_LEN);
        assert_eq!(BlockHeader::decode(&e).unwrap(), bh);
        e[17] ^= 1;
        assert!(BlockHeader::decode(&e).is_none());
        let t = encode_trailer(123, 9);
        assert_eq!(decode_trailer(&t, 9), Some(123));
        assert_eq!(decode_trailer(&t, 10), None);
    }

    #[test]
    fn summary_round_trip() {
        let s = Summary { min_time: -5, max_time: 9, first_seq: 1, last_seq: 4, log_count: 3, span_count: 1, severities: 0b1010, max_observed: 77, sources: vec![1, 2], keys: vec![3] };
        assert_eq!(Summary::decode(&s.encode()).unwrap(), s);
        assert!(Summary::decode(&s.encode()[..20]).is_err());
    }
}
