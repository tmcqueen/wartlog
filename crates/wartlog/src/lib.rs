//! WartLog as a library.
//!
//! This is the stable Rust surface over the store, the query engine and the daemon client. The
//! `wartlog-*` crates it is built on expose their internals and change whenever WartLog does;
//! depend on this crate instead. The C ABI (`wartlog-ffi`) wraps exactly this API.
//!
//! Reading works with or without `wartlogd` running:
//!
//! ```no_run
//! let snap = wartlog::Snapshot::open(wartlog::default_store())?;
//! let rows = snap.query("logs | where Level == 'error' | take 20", &Default::default())?;
//! for row in rows {
//!     println!("{:?} {}", row.id, row.values[3].to_text());
//! }
//! # Ok::<(), wartlog::Error>(())
//! ```
//!
//! Writing uses one of three modes (see [`WriteMode`] and docs/format.md, "Store manifest"):
//!
//! ```no_run
//! use wartlog::{Record, Source, SourceKind, WriteMode, Writer, WriterOptions};
//! let mut w = Writer::open("/tmp/app.wartlogx", WriterOptions::new(WriteMode::Private))?;
//! let src = Source::new(SourceKind::Ndjson, vec![("source.name".into(), "my-app".into())]);
//! let seq = w.push(Record::log(src.into(), wartlog::now_ns(), 9, "hello"))?;
//! w.flush()?;
//! w.close()?;
//! # Ok::<(), wartlog::Error>(())
//! ```

mod error;
mod read;
mod write;

pub use error::{Error, ErrorKind, Result};
pub use read::{record_json, QueryOptions, Row, Rows, Snapshot};
pub use write::{WriteMode, Writer, WriterOptions};

pub use wartlog_model::time::now_ns;
pub use wartlog_model::{Kind, Record, Source, SourceKind, SpanData, Value};
pub use wartlog_query::QueryStats;
pub use wartlog_store::verify::{Finding, Report as VerifyReport};
pub use wartlog_store::{Manifest, Origin, WriterKind};

use std::path::{Path, PathBuf};

/// Version of this crate (and of the C ABI library built from it).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The daemon's store: `$XDG_DATA_HOME/wartlog` or `~/.local/share/wartlog`.
pub fn default_store() -> PathBuf {
    wartlog_store::default_root()
}

/// The daemon's API socket: `$XDG_RUNTIME_DIR/wartlog/api.sock`.
pub fn default_socket() -> PathBuf {
    wartlog_api::api_socket_path()
}

/// Checks the store's seq rules (docs/format.md, "Seq rules"). Read-only, takes no lock.
/// `quick` reads block metadata only. A report with findings is still `Ok`; see `VerifyReport::ok`.
pub fn verify_store(root: impl AsRef<Path>, quick: bool, max_findings: usize) -> Result<VerifyReport> {
    let root = root.as_ref();
    if !root.is_dir() {
        return Err(Error::new(ErrorKind::NotFound, format!("no WartLog store at {}", root.display())));
    }
    Ok(wartlog_store::verify::verify_store(root, &wartlog_store::verify::Options { full: !quick, max_findings }))
}

/// Reads a store's manifest. `None` for a daemon store made before manifests existed.
pub fn store_manifest(root: impl AsRef<Path>) -> Result<Option<Manifest>> {
    Ok(wartlog_store::Manifest::load(root.as_ref())?)
}

/// Normalizes one NDJSON (or plain text) line into a record, as wartlogd's NDJSON receivers do.
/// `default_source` names the source when the line does not.
pub fn record_from_json(line: &str, default_source: &str) -> Record {
    wartlog_model::ndjson::normalize(line, default_source, now_ns())
}
