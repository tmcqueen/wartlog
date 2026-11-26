//! WartLog storage engine (spec section 7, normative details in docs/format.md).
//!
//! One process writes a store (`Writer`, holding the store lock); any number of processes read it
//! (`Snapshot`) with no locks, whether or not a writer is running.

pub mod block;
pub mod catalog;
pub mod format;
pub mod reader;
pub mod segment;
pub mod tix;
pub mod wal;
pub mod writer;

pub use block::{BlockView, Rec};
pub use catalog::Catalog;
pub use reader::{SegmentHandle, Snapshot};
pub use segment::BlockMeta;
pub use writer::{Writer, WriterOptions};

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Default store location: `$XDG_DATA_HOME/wartlog` or `~/.local/share/wartlog`.
pub fn default_root() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("wartlog");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/wartlog")
}

pub fn segments_dir(root: &Path) -> PathBuf {
    root.join("segments")
}

pub fn lock_path(root: &Path) -> PathBuf {
    root.join("LOCK")
}

/// Every segment file, oldest first (file names are UUID v7, which sort by creation time).
pub fn list_segments(root: &Path) -> Vec<PathBuf> {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|e| e == "wseg") {
                    out.push(p);
                }
            }
        }
    }
    let mut v = Vec::new();
    walk(&segments_dir(root), &mut v);
    v.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    v
}

/// Writes a file and fsyncs it, mode 0600.
pub fn write_file_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    set_private(path);
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Logs often contain secrets: files are 0600 and directories 0700.
pub fn set_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

pub fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    set_private(path);
    Ok(())
}

/// fsyncs a directory so a rename or create inside it is durable.
pub fn sync_dir(path: &Path) {
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.sync_all();
    }
}
