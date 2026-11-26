//! Messages and framing. A frame is a `u32` big-endian length followed by that many bytes of CBOR.
//! The client speaks first with `Hello`; the server answers `Hello` or `Error`.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use wartlog_model::{Record, Value};
use wartlog_query::QueryStats;

/// Frames above this are refused (a row batch is capped well below it).
pub const MAX_FRAME: usize = 64 << 20;

/// Inclusive time range in ns since the epoch; either end may be open.
pub type TimeRange = (Option<i64>, Option<i64>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContextScope {
    #[default]
    Source,
    Thread,
    Trace,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello { version: u32, client: String },
    /// Rows come back in batches, then `Done`.
    Query { kql: String, time_range: Option<TimeRange>, limit: Option<u64> },
    /// `backfill` most recent matching rows first (as `Rows`), then new rows as they arrive. Stays
    /// open until the client closes the connection.
    Tail { kql: String, backfill: u64 },
    SourcesList,
    /// `config` is the TOML of one `[[receiver]]` or `[[collector]]` table, without the header,
    /// `kind` is `receiver` or `collector`.
    SourcesAdd { kind: String, config: String },
    SourcesRemove { name: String },
    SourcesUpdate { name: String, config: String },
    Schema { time_range: Option<TimeRange> },
    RecordGet { id: u64 },
    RecordContext { id: u64, before: u32, after: u32, same: ContextScope },
    TraceGet { trace_id: String },
    TemplatesTop { kql: String, time_range: Option<TimeRange>, limit: u32 },
    Status,
    IndexRebuild { segment: Option<String> },
    /// Writes records (their `seq` is ignored and assigned by the daemon), answered by `Ingested`.
    /// With `durable`, the reply waits until the batch is fsynced. Records keep their order and
    /// get consecutive seqs.
    Ingest { records: Vec<Record>, durable: bool },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// record id of each row, when the row is one record
    pub ids: Vec<Option<u64>>,
}

impl Table {
    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }

    pub fn from_result(r: wartlog_query::QueryResult) -> (Table, QueryStats) {
        (Table { columns: r.columns, rows: r.rows, ids: r.ids }, r.stats)
    }
}

/// A configured source (receiver or collector) and its counters.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub name: String,
    /// `receiver` or `collector`
    pub role: String,
    /// `otlp-grpc`, `http`, `ndjson-unix`, `ndjson-tcp`, `journald`, `kubernetes`
    pub kind: String,
    /// the table as TOML
    pub config: String,
    pub enabled: bool,
    /// running, stopped, or failed: <reason>
    pub state: String,
    pub received: u64,
    pub dropped: u64,
    pub errors: u64,
    pub last_seen_ns: i64,
    pub last_error: Option<String>,
    /// how a program sends to this source (endpoint, socket path, command line)
    pub hint: String,
}

/// A source seen in the data (not necessarily configured): the catalog entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SeenSource {
    pub fingerprint: u64,
    pub kind: String,
    pub display: String,
    pub attrs: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyInfo {
    pub key: String,
    /// recent distinct values (capped), most frequent first when known
    pub values: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Schema {
    pub tables: Vec<String>,
    pub log_columns: Vec<String>,
    pub span_columns: Vec<String>,
    pub operators: Vec<String>,
    pub functions: Vec<String>,
    pub aggregates: Vec<String>,
    pub sources: Vec<SeenSource>,
    pub keys: Vec<KeyInfo>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TemplateCount {
    /// the template, or the body for records without one
    pub template: String,
    pub count: u64,
    pub first_ns: i64,
    pub last_ns: i64,
    /// counts over the range in equal buckets, oldest first
    pub sparkline: Vec<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub pid: u32,
    pub uptime_s: u64,
    pub store_root: String,
    pub disk_bytes: u64,
    pub records: u64,
    pub segments: usize,
    pub blocks: usize,
    pub wal_records: u64,
    pub last_seq: u64,
    pub durable_seq: u64,
    /// records per second, averaged over the last 10 seconds
    pub ingest_rate: f64,
    pub segments_indexed: usize,
    pub segments_unindexed: usize,
    pub sources: Vec<SourceInfo>,
    pub config_path: String,
    pub config_error: Option<String>,
    pub warnings: Vec<String>,
    /// `daemon` or `external` (store manifest)
    #[serde(default)]
    pub store_origin: String,
    /// segment count per writer kind (segment header), e.g. ("daemon", 12), ("exclusive", 1)
    #[serde(default)]
    pub segment_writers: Vec<(String, usize)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello { version: u32, server: String },
    /// `start`/`end` point into the KQL text when the error is about it
    Error { message: String, start: Option<usize>, end: Option<usize> },
    Rows { table: Table },
    /// live tail only: the client fell behind and `missed` records were skipped
    Lagged { missed: u64 },
    Done { stats: QueryStats },
    Sources { configured: Vec<SourceInfo>, seen: Vec<SeenSource> },
    Schema { schema: Schema },
    Record { record: Option<Record> },
    Context { table: Table, center: Option<usize> },
    Trace { spans: Table, logs: Table },
    Templates { templates: Vec<TemplateCount> },
    Status { status: Status },
    Ok { message: String },
    /// Reply to `Ingest`: the batch holds seqs `first_seq..=last_seq` (`last_seq < first_seq` for an
    /// empty batch); every seq up to `durable_seq` is fsynced.
    Ingested { first_seq: u64, last_seq: u64, durable_seq: u64 },
}

impl Response {
    pub fn error(e: impl std::fmt::Display) -> Response {
        Response::Error { message: e.to_string(), start: None, end: None }
    }

    pub fn query_error(e: &wartlog_query::QueryError) -> Response {
        let has_span = e.end > e.start;
        Response::Error { message: e.message.clone(), start: has_span.then_some(e.start), end: has_span.then_some(e.end) }
    }

    /// True for the last message of a reply (everything but `Rows` and `Lagged`).
    pub fn is_final(&self) -> bool {
        !matches!(self, Response::Rows { .. } | Response::Lagged { .. })
    }
}

pub fn encode<T: Serialize>(m: &T) -> anyhow::Result<Vec<u8>> {
    let mut b = vec![0u8; 4];
    ciborium::into_writer(m, &mut b)?;
    let n = b.len() - 4;
    anyhow::ensure!(n <= MAX_FRAME, "message too large ({n} bytes)");
    b[..4].copy_from_slice(&(n as u32).to_be_bytes());
    Ok(b)
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, m: &T) -> anyhow::Result<()> {
    let b = encode(m)?;
    w.write_all(&b).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one frame; `None` on a clean end of stream.
pub async fn read_frame<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(r: &mut R) -> anyhow::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let n = u32::from_be_bytes(len) as usize;
    anyhow::ensure!(n <= MAX_FRAME, "frame too large ({n} bytes)");
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).await?;
    Ok(Some(ciborium::from_reader(&b[..])?))
}

/// Splits a table into batches of about `max_bytes` (estimated) for streaming.
pub fn batches(t: Table, max_rows: usize) -> Vec<Table> {
    if t.rows.len() <= max_rows {
        return vec![t];
    }
    let mut out = Vec::new();
    let mut rows = t.rows.into_iter();
    let mut ids = t.ids.into_iter();
    loop {
        let r: Vec<_> = rows.by_ref().take(max_rows).collect();
        if r.is_empty() {
            break;
        }
        let i: Vec<_> = ids.by_ref().take(r.len()).collect();
        out.push(Table { columns: t.columns.clone(), rows: r, ids: i });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip() {
        let msgs = vec![
            Request::Hello { version: 1, client: "test".into() },
            Request::Query { kql: "logs | take 5".into(), time_range: Some((Some(1), None)), limit: None },
            Request::RecordContext { id: 9, before: 5, after: 5, same: ContextScope::Thread },
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_frame(&mut buf, m).await.unwrap();
        }
        let mut r = &buf[..];
        for m in &msgs {
            let got: Request = read_frame(&mut r).await.unwrap().unwrap();
            assert_eq!(&got, m);
        }
        assert!(read_frame::<_, Request>(&mut r).await.unwrap().is_none());

        let resp = Response::Rows {
            table: Table { columns: vec!["a".into()], rows: vec![vec![Value::I64(1)], vec![Value::Str("x".into())], vec![Value::Null]], ids: vec![Some(1), None, None] },
        };
        let b = encode(&resp).unwrap();
        let back: Response = read_frame(&mut &b[..]).await.unwrap().unwrap();
        assert_eq!(back, resp);
        assert_eq!(batches(Table { columns: vec![], rows: vec![vec![]; 5], ids: vec![None; 5] }, 2).len(), 3);
    }
}
