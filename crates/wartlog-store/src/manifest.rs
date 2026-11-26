//! The store manifest (`WARTLOG` at the store root): store format version and who owns the store.
//!
//! Layout (little-endian):
//!   magic "WARTLOG\0", u16 manifest version (1), u16 store format version, u8 origin
//!   (1 daemon, 2 external), u8 reserved, u16 reserved, i64 created (ns since the epoch),
//!   u32 len + creator bytes (UTF-8, "name version"), then u32 CRC32C of everything before it.
//!
//! Written once when a store is created (temporary name, fsync, rename) and changed only by
//! adoption. A store without a manifest is a daemon store: that covers every store made before
//! the manifest existed.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::format::*;
use crate::StoreError;

const MAGIC: &[u8; 8] = b"WARTLOG\0";
pub const MANIFEST_VERSION: u16 = 1;
/// The store format version this build reads and writes. A store with a newer one is refused.
pub const STORE_VERSION: u16 = 1;

pub fn manifest_path(root: &Path) -> PathBuf {
    root.join("WARTLOG")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// written by wartlogd (or by the library in exclusive mode)
    Daemon,
    /// created by the library for its own use; wartlogd refuses it unless adopted
    External,
}

impl Origin {
    pub fn name(self) -> &'static str {
        match self {
            Origin::Daemon => "daemon",
            Origin::External => "external",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub store_version: u16,
    pub origin: Origin,
    pub created_ns: i64,
    /// creating program, "name version"
    pub creator: String,
}

impl Manifest {
    pub fn new(origin: Origin, creator: &str) -> Manifest {
        Manifest { store_version: STORE_VERSION, origin, created_ns: wartlog_model::time::now_ns(), creator: creator.to_string() }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = MAGIC.to_vec();
        put_u16(&mut b, MANIFEST_VERSION);
        put_u16(&mut b, self.store_version);
        b.push(match self.origin {
            Origin::Daemon => 1,
            Origin::External => 2,
        });
        b.push(0);
        put_u16(&mut b, 0);
        put_i64(&mut b, self.created_ns);
        put_u32(&mut b, self.creator.len() as u32);
        b.extend_from_slice(self.creator.as_bytes());
        let crc = crc32c::crc32c(&b);
        put_u32(&mut b, crc);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Manifest> {
        if b.len() < 32 || &b[..8] != MAGIC {
            bail!("not a WartLog store manifest");
        }
        let n = b.len();
        if crc32c::crc32c(&b[..n - 4]) != get_u32(b, n - 4) {
            bail!("store manifest CRC mismatch");
        }
        let mut c = Cursor::new(&b[..n - 4]);
        c.take(8)?;
        let mv = c.u16()?;
        if mv > MANIFEST_VERSION {
            return Err(UnsupportedVersion { what: "store manifest", version: mv, supported: MANIFEST_VERSION }.into());
        }
        let store_version = c.u16()?;
        let origin = match c.u8()? {
            1 => Origin::Daemon,
            2 => Origin::External,
            o => bail!("store manifest has unknown origin {o}"),
        };
        c.u8()?;
        c.u16()?;
        let created_ns = c.i64()?;
        let l = c.u32()? as usize;
        let creator = String::from_utf8_lossy(c.take(l)?).into_owned();
        Ok(Manifest { store_version, origin, created_ns, creator })
    }

    /// Reads the manifest. `Ok(None)` when there is none (a daemon store made before manifests).
    /// A store version newer than this build is an error (`StoreError::NewerFormat`).
    pub fn load(root: &Path) -> Result<Option<Manifest>> {
        let p = manifest_path(root);
        let b = match std::fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", p.display())),
        };
        let m = Manifest::decode(&b).with_context(|| format!("store manifest {}", p.display()))?;
        if m.store_version > STORE_VERSION {
            return Err(StoreError::NewerFormat(UnsupportedVersion { what: "store", version: m.store_version, supported: STORE_VERSION }).into());
        }
        Ok(Some(m))
    }

    /// The effective origin: a store without a manifest is a daemon store.
    pub fn origin_of(root: &Path) -> Result<Origin> {
        Ok(Manifest::load(root)?.map(|m| m.origin).unwrap_or(Origin::Daemon))
    }

    /// Writes atomically: temporary name, fsync, rename, fsync the directory.
    pub fn save(&self, root: &Path) -> Result<()> {
        let tmp = root.join("WARTLOG.tmp");
        crate::write_file_synced(&tmp, &self.encode())?;
        std::fs::rename(&tmp, manifest_path(root))?;
        crate::sync_dir(root);
        Ok(())
    }
}

/// True when the directory holds no store data yet (no manifest, segments, WAL or catalog).
/// A `LOCK` file alone does not count: opening for writing creates it before anything else.
pub fn is_new_store(root: &Path) -> bool {
    !manifest_path(root).exists()
        && crate::list_segments(root).is_empty()
        && crate::wal::list(root).is_empty()
        && !crate::catalog::catalog_path(root).exists()
}

/// Turns an external store into a daemon store, so wartlogd will write it. Takes the store lock,
/// so it fails while any writer has the store open.
pub fn adopt(root: &Path) -> Result<Manifest> {
    if !root.is_dir() {
        return Err(StoreError::NotFound(root.to_path_buf()).into());
    }
    let _lock = crate::writer::lock_store(root)?;
    let mut m = match Manifest::load(root)? {
        Some(m) => m,
        None => Manifest::new(Origin::Daemon, &format!("wartlog-store {}", env!("CARGO_PKG_VERSION"))),
    };
    m.origin = Origin::Daemon;
    m.save(root)?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_damage() {
        let m = Manifest { store_version: 1, origin: Origin::External, created_ns: 123, creator: "test 0.1".into() };
        let b = m.encode();
        assert_eq!(Manifest::decode(&b).unwrap(), m);
        let mut d = b.clone();
        d[20] ^= 1;
        assert!(Manifest::decode(&d).is_err());
    }

    #[test]
    fn newer_store_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let m = Manifest { store_version: STORE_VERSION + 1, origin: Origin::Daemon, created_ns: 1, creator: "future".into() };
        m.save(dir.path()).unwrap();
        let e = Manifest::load(dir.path()).unwrap_err();
        assert!(matches!(e.downcast_ref::<StoreError>(), Some(StoreError::NewerFormat(_))));
    }

    #[test]
    fn missing_manifest_is_a_daemon_store() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Manifest::load(dir.path()).unwrap(), None);
        assert_eq!(Manifest::origin_of(dir.path()).unwrap(), Origin::Daemon);
        assert!(is_new_store(dir.path()));
    }
}
