//! Plain Bloom filter with double hashing, sized at ~10 bits per distinct term (about 1% false positives).

use anyhow::{bail, Result};

#[derive(Clone, Debug, Default)]
pub struct Bloom {
    k: u8,
    bits: Vec<u64>,
}

impl Bloom {
    pub fn with_capacity(n: usize) -> Bloom {
        let want = (n.max(1) * 10).next_power_of_two().max(1024);
        Bloom { k: 7, bits: vec![0; want / 64] }
    }

    fn positions(&self, h: u64) -> impl Iterator<Item = usize> + '_ {
        let m = (self.bits.len() * 64) as u64;
        let h1 = h;
        let h2 = h.rotate_left(32) | 1;
        (0..self.k as u64).map(move |i| (h1.wrapping_add(i.wrapping_mul(h2)) % m) as usize)
    }

    pub fn insert(&mut self, h: u64) {
        let ps: Vec<usize> = self.positions(h).collect();
        for p in ps { self.bits[p / 64] |= 1 << (p % 64); }
    }

    pub fn may_contain(&self, h: u64) -> bool {
        if self.bits.is_empty() { return true; }
        self.positions(h).all(|p| self.bits[p / 64] & (1 << (p % 64)) != 0)
    }

    pub fn byte_len(&self) -> usize { self.bits.len() * 8 }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&((self.bits.len() * 8) as u32).to_le_bytes());
        out.push(self.k);
        for w in &self.bits { out.extend_from_slice(&w.to_le_bytes()); }
    }

    pub fn decode(b: &[u8]) -> Result<(Bloom, usize)> {
        let n = u32::from_le_bytes(b[0..4].try_into()?) as usize;
        if n % 8 != 0 || b.len() < 5 + n { bail!("bad bloom"); }
        let k = b[4];
        let bits = (0..n / 8).map(|i| u64::from_le_bytes(b[5 + i * 8..13 + i * 8].try_into().unwrap())).collect();
        Ok((Bloom { k, bits }, 5 + n))
    }
}
