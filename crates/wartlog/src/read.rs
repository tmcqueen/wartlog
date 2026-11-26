use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use wartlog_model::{Record, Value};
use wartlog_query::QueryStats;

use crate::{Error, ErrorKind, Result};

/// A consistent read-only view of a store: every segment present when it was opened, plus the
/// records still in the WAL. Takes no locks and never writes, so it works whether or not a writer
/// is running. Open a new one to see newer records. `Sync`: threads may query one snapshot.
pub struct Snapshot {
    inner: wartlog_store::Snapshot,
}

/// Defaults suit running inside someone else's process: a small thread pool and a row cap.
#[derive(Clone, Debug)]
pub struct QueryOptions {
    /// cap on rows when the query has no `take`, `top` or aggregation (default 10 000)
    pub limit: Option<u64>,
    /// query threads (default 2; the query runs in the caller's process)
    pub threads: usize,
    /// inclusive time range in ns since the epoch, applied when the query has none on Timestamp
    pub time_range: Option<(Option<i64>, Option<i64>)>,
    /// set to true from another thread to stop a running query
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for QueryOptions {
    fn default() -> Self {
        QueryOptions { limit: Some(10_000), threads: 2, time_range: None, cancel: None }
    }
}

/// One result row: `values` line up with `Rows::columns`.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// the record's seq, when the row is still one record (not an aggregate)
    pub id: Option<u64>,
    pub values: Vec<Value>,
}

/// A query result, read row by row.
///
/// The rows are produced by the query engine before the first one is returned (aggregations
/// stream internally; plain row results are bounded by `QueryOptions::limit`). The cursor shape
/// lets a later engine hand rows out as they are found without changing this API.
pub struct Rows {
    columns: Vec<String>,
    rows: std::iter::Zip<std::vec::IntoIter<Option<u64>>, std::vec::IntoIter<Vec<Value>>>,
    stats: QueryStats,
}

impl Rows {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }

    pub fn stats(&self) -> &QueryStats {
        &self.stats
    }

    /// Rows not yet returned.
    pub fn remaining(&self) -> usize {
        self.rows.len()
    }
}

impl Iterator for Rows {
    type Item = Row;
    fn next(&mut self) -> Option<Row> {
        self.rows.next().map(|(id, values)| Row { id, values })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rows.size_hint()
    }
}

impl ExactSizeIterator for Rows {}

impl Snapshot {
    pub fn open(root: impl AsRef<Path>) -> Result<Snapshot> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(Error::new(ErrorKind::NotFound, format!("no WartLog store at {}", root.display())));
        }
        Ok(Snapshot { inner: wartlog_store::Snapshot::open(root)? })
    }

    /// Runs a KQL query (`logs | ...` or `spans | ...`).
    pub fn query(&self, kql: &str, opts: &QueryOptions) -> Result<Rows> {
        let qo = wartlog_query::QueryOptions {
            limit: opts.limit,
            threads: Some(opts.threads.max(1)),
            time_range: opts.time_range,
            cancel: opts.cancel.clone(),
            now: None,
        };
        let r = wartlog_query::run(kql, &self.inner, &qo)?;
        let mut ids = r.ids;
        ids.resize(r.rows.len(), None);
        Ok(Rows { columns: r.columns, rows: ids.into_iter().zip(r.rows), stats: r.stats })
    }

    /// The record with this seq, if the store still holds it.
    pub fn get(&self, seq: u64) -> Result<Option<Record>> {
        Ok(self.inner.get(seq)?)
    }

    /// Highest seq in the snapshot.
    pub fn max_seq(&self) -> u64 {
        self.inner.max_seq()
    }

    pub fn record_count(&self) -> u64 {
        self.inner.record_count()
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Segments that could not be read (for example a newer format), with the reason.
    pub fn unreadable_segments(&self) -> Vec<(std::path::PathBuf, String)> {
        self.inner.errors.clone()
    }
}

/// The record as JSON: the shape `wl show --json` and the API use.
pub fn record_json(r: &Record) -> String {
    serde_json::to_string(r).unwrap_or_default()
}
