//! Source catalog (`sources.wcat`): every source descriptor seen in the store, keyed by fingerprint.
//! Written by the writer (temp file + rename), read by everyone, rebuildable from the segments.
//!
//! Layout: magic "WARTCAT\0", u16 version, u16 pad, u32 count, then per source:
//!   u64 fingerprint, u8 kind, u16 attr_count, attr_count x (u32 len, key bytes, u32 len, value bytes)
//! then u32 crc32c of everything before it.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wartlog_model::{Source, SourceKind};

use crate::format::*;

const MAGIC: &[u8; 8] = b"WARTCAT\0";

pub fn catalog_path(root: &Path) -> PathBuf {
    root.join("sources.wcat")
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    pub sources: BTreeMap<u64, Arc<Source>>,
}

impl Catalog {
    pub fn get(&self, fp: u64) -> Option<&Arc<Source>> {
        self.sources.get(&fp)
    }

    /// Adds a source; returns true when it was new.
    pub fn add(&mut self, s: &Arc<Source>) -> bool {
        let fp = s.fingerprint();
        if self.sources.contains_key(&fp) {
            return false;
        }
        self.sources.insert(fp, s.clone());
        true
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = MAGIC.to_vec();
        put_u16(&mut b, 1);
        put_u16(&mut b, 0);
        put_u32(&mut b, self.sources.len() as u32);
        for (fp, s) in &self.sources {
            put_u64(&mut b, *fp);
            b.push(s.kind.code());
            put_u16(&mut b, s.attrs.len() as u16);
            for (k, v) in &s.attrs {
                put_u32(&mut b, k.len() as u32);
                b.extend_from_slice(k.as_bytes());
                put_u32(&mut b, v.len() as u32);
                b.extend_from_slice(v.as_bytes());
            }
        }
        let crc = crc32c::crc32c(&b);
        put_u32(&mut b, crc);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Catalog> {
        if b.len() < 20 || &b[..8] != MAGIC {
            bail!("not a source catalog");
        }
        let n = b.len();
        if crc32c::crc32c(&b[..n - 4]) != get_u32(b, n - 4) {
            bail!("source catalog CRC mismatch");
        }
        let mut c = Cursor::new(&b[..n - 4]);
        c.take(8)?;
        c.u16()?;
        c.u16()?;
        let count = c.u32()?;
        let mut cat = Catalog::default();
        let s = |c: &mut Cursor| -> Result<String> {
            let l = c.u32()? as usize;
            Ok(String::from_utf8_lossy(c.take(l)?).into_owned())
        };
        for _ in 0..count {
            let fp = c.u64()?;
            let kind = SourceKind::from_u8(c.u8()?).context("bad source kind")?;
            let na = c.u16()?;
            let mut attrs = Vec::new();
            for _ in 0..na {
                let k = s(&mut c)?;
                let v = s(&mut c)?;
                attrs.push((k, v));
            }
            cat.sources.insert(fp, Arc::new(Source { kind, attrs }));
        }
        Ok(cat)
    }

    pub fn load(root: &Path) -> Option<Catalog> {
        std::fs::read(catalog_path(root)).ok().and_then(|b| Catalog::decode(&b).ok())
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let tmp = root.join("sources.wcat.tmp");
        crate::write_file_synced(&tmp, &self.encode())?;
        std::fs::rename(&tmp, catalog_path(root))?;
        Ok(())
    }

    /// Rebuilds from the source tables of every block.
    pub fn rebuild(root: &Path) -> Catalog {
        let mut cat = Catalog::default();
        for p in crate::list_segments(root) {
            if let Ok(h) = crate::reader::SegmentHandle::open(&p) {
                for i in 0..h.blocks.len() {
                    if let Ok(v) = h.read_block(i) {
                        for s in &v.sources {
                            cat.add(s);
                        }
                    }
                }
            }
        }
        for r in crate::wal::read_all(root) {
            cat.add(&r.source);
        }
        cat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut c = Catalog::default();
        c.add(&Arc::new(Source::new(SourceKind::Journald, vec![("systemd.unit".into(), "a.service".into())])));
        c.add(&Arc::new(Source::new(SourceKind::Otlp, vec![("service.name".into(), "api".into())])));
        let b = c.encode();
        assert_eq!(Catalog::decode(&b).unwrap(), c);
        let mut d = b.clone();
        d[25] ^= 1;
        assert!(Catalog::decode(&d).is_err());
    }
}
