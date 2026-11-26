# WartLog MVP Design

Status: draft for review
Date: 2026-11-25
Owner: Tim (tmcqueen)

## 1. Intent

WartLog is a desktop logging and tracing service: one local place that accepts structured logs and traces from just about anywhere, stores them in its own durable binary format, and lets you find what you need quickly in a viewer that is pleasant to use.

Why it exists: Tim works across many kinds of projects (desktop, web, cloud hosted, Docker, Kubernetes) and each has its own logging story, good or bad, that he has to fumble through. Recent tools have been disappointing; Venator in particular had an awful UI, a search that was impossible to learn even with the docs, and results that were no better than dumping logs to stdout. WartLog is the tool Tim wants to use from now on, built so he can tweak it to exactly what he wants.

The honest bar to beat is a terminal: `journalctl -f | grep`, `kubectl logs -f`, and stdout. If finding the right lines in WartLog is harder than that, WartLog has failed, no matter how good the storage is.

### First scenarios (from Tim)

1. **Aspire projects.** A local, high performance OTLP sink for .NET Aspire apps, keeping logs and traces across runs and projects in one searchable place.
2. **Kubernetes.** Read the logs of a current Kubernetes project in a tool he is comfortable with, instead of `kubectl logs`.
3. **journald.** Never type `journalctl -xefu foo.service` again. Pick the unit, follow it, filter it, done.
4. **Anything else.** Any program, in any language, can send structured lines with no library.

## 2. Goals, non-goals, anti-goals

### Goals (MVP)

- Ingest logs and spans from OTLP (gRPC and HTTP), journald, Kubernetes pod logs, and newline-delimited JSON (NDJSON) over a UNIX socket, TCP and HTTP.
- Store everything in a versioned, checksummed, self-describing binary format whose indexes can always be rebuilt from the data files alone.
- Query with a defined subset of KQL, with time-range pruning that keeps common queries fast on a laptop.
- A Qt viewer (KDE/Wayland native look) where common tasks need no typing: click to filter, live tail, context around a record, group by message template, logs linked to their trace.
- Setting up a source is simple: sensible defaults work out of the box, and the viewer shows the exact snippet a sender needs.
- A small CLI (`wartlog`) for queries, tail, status, and piping any program's output in.

### Non-goals (MVP)

- Metrics storage. OTLP metrics are accepted and dropped so senders do not error.
- Trace waterfall/timeline view (phase 2). The MVP stores spans and shows a flat span list.
- Full KQL. No `join`, `let`, `union` across arbitrary tables, `make-series`, `render`, `materialize`, or user functions.
- Multi-user or server deployment, remote access, authentication, TLS/HTTPS.
- Windows and macOS builds. Keep code portable where it costs nothing, but test only on Linux.
- Named pipes, shared memory, syslog, GELF, Fluent Forward, Loki/Elasticsearch/Splunk compatibility endpoints, file tailing, Docker (see roadmap).
- A dynamic plugin ABI.
- Alerting.

### Anti-goals (failure even if it technically works)

- Becoming a general purpose Graylog or Loki clone at the cost of being a great single-user desktop tool.
- A UI where basic filtering requires reading documentation or knowing KQL.
- Being slower than `journalctl | grep` for the everyday "show me recent errors from X" case.
- Storage that cannot recover from its own data after an index is lost or a file is damaged.

## 3. Constraints

- Linux desktop, KDE Plasma on Wayland is the primary environment.
- Rust for all components. Qt for the viewer.
- Logs stored as explicitly laid out binary structures with external indexes. If an index is lost or corrupted, it is recreated from the logs.
- Query language is KQL (Microsoft's Kusto Query Language), as a subset.
- Personal tool first: Tim owns the source and extends it by changing code, so clarity of the code matters more than generic extensibility.
- Logs often contain secrets. Everything stays local: files are mode 0600, the API socket lives under `$XDG_RUNTIME_DIR`, network listeners bind loopback by default, and WartLog sends no telemetry of its own.

## 4. Approaches considered

Three forks were decided with Tim. All three picks below were his choice.

### Fork 1: process shape

- **Chosen: daemon plus viewer.** `wartlogd` is a Rust systemd user service with no Qt dependency (it links libsystemd for journald) that receives, collects, stores and answers queries. The viewer, the CLI, and later the MCP server are clients of its local API. Collection continues with the viewer closed, which a sink must do, and a UI crash cannot stop ingestion.
- Rejected: single app with the engine inside the Qt process. Closing the window would stop collection.

### Fork 2: viewer technology

- **Chosen: Rust with cxx-qt and QML, using Kirigami.** One language across the repo; API and record types are shared directly with the daemon. Cost: cxx-qt is younger than the C++ Qt path, so expect rough edges and fewer examples.
- Rejected: a C++ Qt app (two languages, duplicated protocol types). Rejected: a non-Qt Rust UI such as Slint or egui (Tim wants Qt and a native KDE feel).

### Fork 3: KQL implementation

- **Chosen: our own Rust parser and executor for a defined KQL subset, checked against Microsoft's Kusto.Language in tests.** Kusto.Language (C#, `microsoft/Kusto-Query-Language`) is a parser and semantic analyzer only; it has no execution engine, so an executor has to be written regardless. Our parser follows Microsoft's grammar, and a CI conformance suite runs every test query through Kusto.Language to make sure both accept and reject the same queries.
- Rejected: hosting Kusto.Language in the daemon via .NET NativeAOT. The existing Rust wrapper (`kql-language-tools` 0.1.0) exposes validation and completions, not the syntax tree an executor needs, and it would add a .NET toolchain to every build.
- Rejected: existing Rust KQL crates (`kqlparser`, `rust-kql`). Early stage; their own README says only very simple queries execute.
- Possible later: Kusto.Language completions in the viewer's query bar if ours prove weaker. Check its license before shipping it.

### Other decisions made with Tim

- Think in **protocols**, not transports. MVP transports are UNIX socket, TCP, UDP (reserved, unused by MVP protocols) and HTTP. HTTPS, named pipes and shared memory are deferred.
- **Tracing scope:** spans are stored and indexed in the MVP; logs link to their trace; the trace view is a flat span list. Waterfall view is phase 2.
- **Plugins:** no plugin ABI. Every source implements the same internal Rust trait and is compiled in behind a Cargo feature, so sources look like plugins in the code and the seam can be promoted later.
- **MCP server:** wanted in the MVP or close after. Placed at phase 1.5 (see roadmap). The query API is designed with it in mind.
- **Storage inspiration:** Tim shared a commercial binary trace format he likes (log file, sparse index, segment journal). WartLog keeps its good ideas and improves on them (section 7). It does not copy that format, and the sample files are not committed anywhere because they contain a third party's personal data.

## 5. Architecture

```
  Aspire apps ──OTLP gRPC/HTTP──┐
  any program ──NDJSON (unix/tcp/http)──┐      ┌──────── wartlogd (Rust, systemd user service) ────────┐
                                 │      │      │                                                       │
                                 ▼      ▼      │  Receivers ─┐                                         │
                              ┌────────────┐   │             ├─► Normalizer ─► Writer ─► WAL + blocks  │
  journald  ◄──── read ────── │ Collectors │───┤  Collectors ┘        │           │      (segments)    │
  Kubernetes API ◄─ watch ─── └────────────┘   │                      ▼           ▼                    │
                                               │               Live tail bus   Index builder           │
                                               │                      │           │                    │
                                               │                      ▼           ▼                    │
                                               │                   Query engine (KQL subset)           │
                                               │                      │                                │
                                               │             API server (UNIX socket)                  │
                                               └──────────────────────┼────────────────────────────────┘
                                                                      │
                                     ┌────────────────┬───────────────┼────────────────┐
                                     ▼                ▼               ▼                ▼
                               Qt viewer        `wartlog` CLI   MCP (phase 1.5)   OTLP forward
                               (cxx-qt/QML)                                       (to Aspire dashboard)
```

### Repository layout (Cargo workspace)

| Crate | Purpose |
|---|---|
| `crates/wartlog-model` | Canonical record types (log record, span, source descriptor, typed values). No I/O. |
| `crates/wartlog-store` | On-disk format: WAL, segment writer, block reader, indexes, rebuild, retention. |
| `crates/wartlog-query` | KQL subset lexer, parser, planner, executor. Depends on model and store. |
| `crates/wartlog-sources` | Receivers and collectors behind the `Receiver` and `Collector` traits, one Cargo feature each. |
| `crates/wartlog-api` | API message types, framing, and an async client used by the viewer, CLI and MCP. |
| `crates/wartlogd` | Daemon binary: config, wiring, systemd notify, API server. |
| `crates/wartlog-cli` | `wartlog` binary: `query`, `tail`, `send`, `sources`, `status`, `index`. Later `mcp`. |
| `crates/wartlog-viewer` | Qt viewer with cxx-qt and QML (Kirigami). |
| `tools/kql-conformance` | Small .NET console app wrapping Kusto.Language. CI only, never shipped. |
| `docs/format.md` | The normative on-disk format spec, kept in sync with `wartlog-store`. |

## 6. Data model

WartLog's internal model follows the OpenTelemetry log and trace data models, so OTLP maps directly and other sources map into the same shape.

### Log record

| Field | Type | Notes |
|---|---|---|
| `time` | i64 ns since Unix epoch | Event time. If the source gives none, equals `observed_time`. |
| `observed_time` | i64 ns | When WartLog received it. |
| `severity_number` | u8, 0 to 24 | OTel scale: 1-4 trace, 5-8 debug, 9-12 info, 13-16 warn, 17-20 error, 21-24 fatal. 0 unspecified. |
| `severity_text` | string, optional | As the source spelled it. |
| `body` | typed value | Usually a string. For template messages, the rendered text is not stored; it is rendered from template plus args. |
| `template` | string, optional | Message template, for example `Resolved host {} to {}` or a CLEF `@mt` with named holes. See "Templates" below. |
| `args` | list of typed values | Template arguments in order (named holes keep their names as attributes too). |
| `attributes` | map of string to typed value | Record-level fields. |
| `source` | reference to a source descriptor | See below. |
| `trace_id` | 16 bytes, optional | |
| `span_id` | 8 bytes, optional | |
| `seq` | u64 | Ingest sequence number, assigned by the writer, strictly increasing across the whole store. It is the record's id (see section 10). |

Thread identity uses the OTel attribute `thread.id` (journald `TID` maps to it, falling back to `_PID`). "Same thread" in context views means same source and same `thread.id`.

### Templates

Supported dialects: positional `{}`, and .NET/Serilog message templates (`{Name}`, `{Name:format}`, `{Name,alignment}`, `{@Name}`, `{$Name}`). Sources of templates: CLEF `@mt`, the .NET OTLP exporter's `{OriginalFormat}` attribute (the body then holds the rendered text and is discarded in favor of template plus args), and NDJSON `template`. If a record has both a rendered message (`@m`) and a template, the template and args are kept and the rendered message is dropped. Rendering is best effort for format specifiers: numbers and dates honor common .NET format strings, anything unrecognized renders the raw value. The renderer is one function in `wartlog-model`, used both at write time (for the Bloom filter) and at read time (for `Body`), so both always agree.

### Span

Same envelope as a log record (`time` is the span start, `source`, `trace_id`, `span_id`, `attributes`) plus `parent_span_id` (8 bytes, optional), `end_time` (i64 ns), `name` (string), `kind` (u8: internal, server, client, producer, consumer), `status` (u8: unset, ok, error) and `status_message`. Span events are stored as log records carrying the span's trace and span ids, with attribute `wartlog.span_event = true`.

### Source descriptor

A source is "who sent this": a kind (`otlp`, `journald`, `k8s`, `ndjson`) plus resource attributes (for example `service.name`, `service.instance.id`, `k8s.namespace.name`, `k8s.pod.name`, `systemd.unit`, `host.name`). Its identity is a 64-bit fingerprint (xxh3) of the kind plus its identifying attributes only, so the same service maps to the same source across runs, restarts and rollouts. Per-run values such as `service.instance.id`, process ids and pod names are kept as attributes on each record, not in the identity.

| Kind | Identifying attributes | Display name |
|---|---|---|
| `otlp` | `service.namespace` (if set), `service.name`, `host.name` | `service.name` |
| `journald` | journal (user or system), `systemd.unit` or else `syslog.identifier`, `host.name` | unit name |
| `k8s` | `k8s.cluster.name`, `k8s.namespace.name`, owning workload name (Deployment, StatefulSet, DaemonSet, Job; else the pod name), `k8s.container.name` | `namespace/workload/container` |
| `ndjson` | configured source name, else the record's `service` value, else the listener name | that name |

The viewer's source tree is built from descriptors; individual pods and instances are a drill-down filter, not separate sources.

### Typed values

`null`, `bool`, `i64`, `f64`, `string`, `bytes`, `timestamp` (i64 ns), `duration` (i64 ns), `json` (a string holding JSON). OTLP arrays and maps (`AnyValue` nesting) are stored as `json` values in the MVP and are queryable with `parse_json` later. (Builder call: revisit if nested values turn out common.)

## 7. Storage

### What we keep from the format Tim likes, and what we improve

Kept: a rich header written once instead of per record; records framed so a reader can walk forwards and backwards; aggressive string interning; message templates stored separately from their arguments; nanosecond timestamps; a sparse external index bound to its data file by UUID; a chain of rolled segment files.

Improved: checksums on everything; blocks that decode on their own (no back-references across the file, so damage stays local and rebuilds parallelize); block summaries that let queries skip data without decoding it; typed arguments; many sources per file; no platform quirks (plain 16-byte UUIDs, segment chain stored in segment headers, not a loose text file).

### Layout on disk

Data lives in `$XDG_DATA_HOME/wartlog/` (default `~/.local/share/wartlog/`):

```
wartlog/
  wal/current.wal              uncompressed framed records not yet sealed into a block
  segments/
    2026/11/25/<segment-uuid>.wseg   sealed, append-only data
    2026/11/25/<segment-uuid>.widx   block index for that segment (rebuildable)
    2026/11/25/<segment-uuid>.wtrc   trace index for that segment (rebuildable)
  catalog.wcat                 list of segments with time ranges and sources (rebuildable)
  state/cursors.toml           journald and Kubernetes resume positions
```

All integers are little-endian. All structures are written with explicit field layout (the Rust types use `#[repr(C)]` with no implicit padding, verified by size assertions and read and written via explicit byte encoding, never by transmuting a whole file region). Checksums are CRC32C.

### Write path

1. The writer task receives normalized records from a bounded channel.
2. Each record is appended to the WAL as a self-contained frame: `u32 len`, `u32 crc32c`, record bytes with all strings inline, `u32 len` again (for backward scanning). The WAL is fsynced at most every 250 ms (configurable). Live tail subscribers are notified at the same time.
3. When the active block reaches its size target (default 256 KiB uncompressed) or age limit (default 5 s), it is encoded (section below), compressed with zstd (level 3), appended to the current segment, and fsynced; then the WAL is truncated.
4. A segment closes at 256 MiB or 1 hour, whichever comes first. A new segment records the previous segment's UUID in its header.

Crash recovery: on start, the daemon scans the WAL, keeps every frame whose CRC verifies, discards a torn tail, and seals the recovered records into a block. Worst-case loss on power failure is the last WAL fsync interval.

### Segment file (`.wseg`)

```
SegmentHeader (64 bytes)
  magic        [u8; 8]  = "WARTSEG\0"
  version      u16      = 1
  flags        u16
  header_len   u32      = 64
  segment_id   [u8; 16] UUID v7
  prev_segment [u8; 16] UUID of the previous segment, zero if none
  created_ns   i64
  reserved     [u8; 4]
  header_crc   u32      CRC32C of the preceding 60 bytes
Block*
```

### Block

```
BlockHeader (48 bytes)
  magic            u32 = "WBLK"
  version          u16 = 1
  codec            u8  (0 none, 1 zstd)
  flags            u8
  block_seq        u64  monotonically increasing within the segment
  record_count     u32
  uncompressed_len u32
  compressed_len   u32
  summary_len      u32
  payload_crc      u32  CRC32C of the compressed payload
  summary_crc      u32  CRC32C of the summary
  reserved         u32
  header_crc       u32  CRC32C of the preceding 44 bytes
Payload  (compressed_len bytes)
Summary  (summary_len bytes, uncompressed)
BlockTrailer (16 bytes)
  total_len  u32   header + payload + summary + trailer, for backward scanning
  block_seq  u32   low 32 bits, sanity check
  crc        u32   CRC32C of total_len and block_seq
  magic      u32 = "KLBW"
```

Recovery scan: walk block headers forward using the lengths. If a header fails its CRC, search forward byte by byte for the next `WBLK` magic whose header CRC verifies. A damaged block is skipped and reported; every other block still decodes, because blocks are self-contained.

### Decompressed payload

```
StringTable   u32 count, then for each: u32 len, UTF-8 bytes   (index 0..count-1)
SourceTable   u32 count, then for each: u64 fingerprint, u8 kind, u16 attr_count, attrs
Records       record_count records in seq order, each starting at an 8-byte aligned offset
```

Strings and sources are interned per block, so any block decodes with nothing but its own bytes. References are `u32` indexes into the block's tables; `0xFFFFFFFF` means none.

### Record (fixed part, 72 bytes, then variable part)

```
  record_len      u32   total length including variable part
  kind            u8    1 log, 2 span
  severity        u8    OTel severity number
  flags           u16   has_trace, has_span, has_template, body_is_string, ...
  time_ns         i64
  observed_ns     i64
  source_ref      u32
  template_ref    u32   string index or none
  attr_count      u16
  arg_count       u16
  body_ref        u32   string index when body_is_string, else the body is the first variable value
  trace_id        [u8; 16]
  span_id         [u8; 8]
  seq             u64   ingest sequence number (record id)
Variable part:
  span extension (when kind = 2): parent_span_id [u8;8], end_ns i64, name_ref u32, kind u8, status u8, u16 pad, status_message_ref u32
  args:       arg_count TypedValues
  attributes: attr_count pairs of (key_ref u32, TypedValue)
TypedValue: u8 tag, then payload (i64/f64/timestamp/duration 8 bytes, bool 1 byte, string/bytes/json u32 ref)
```

The fixed part means time, severity, source and trace filters read fields at known offsets in a decompressed block without parsing the rest of the record.

### Block summary

Stored uncompressed after the payload, and copied into the index, so pruning never needs to decompress:

- `min_time_ns`, `max_time_ns`, `first_seq`, `last_seq`, log count, span count.
- Severity bitmap (`u32`, one bit per severity number).
- Sorted list of source fingerprints present.
- Sorted list of attribute key hashes present.
- A split-block Bloom filter (default 4 KiB) over lowercase terms of the **rendered** body (template plus args, rendered at write time, so non-string args such as `404` and terms formed by joining template text and args are covered), string args, and string attribute values. Terms follow KQL's `has` tokenization (alphanumeric runs); only terms of 3 or more characters are inserted, and the planner never prunes on a shorter term, because KQL `has` still matches short terms by scanning.
- A Bloom filter over trace ids present (default 512 bytes).

### Indexes (all rebuildable)

- **Block index (`.widx`)**, one per segment: header with the segment UUID, the last block sequence it covers and the segment length it saw; then one entry per block: file offset, block sequence, and the block summary. Query planning reads only these.
- **Trace index (`.wtrc`)**, one per segment: sorted `(trace_id, block_seq)` pairs, so "open this trace" is a binary search per segment.
- **Catalog (`catalog.wcat`)**: one row per segment with its time range, size, and source fingerprints, plus the source descriptors themselves.

Validation: on start and before use, each index is checked against its segment (UUID, covered block sequence, length, CRC). Any mismatch, missing file, or CRC failure triggers a rebuild of that index from the segment by scanning blocks. Rebuilds run in the background in parallel; queries over a segment with no valid index fall back to scanning it. `wartlog index rebuild` forces a full rebuild. Index contents are fully determined by the segment: block index entries in block order, trace index pairs sorted by `(trace_id, block_seq)` and deduplicated, catalog rows sorted by segment creation time then UUID, source descriptors sorted by fingerprint. Incremental updates must produce the same bytes as a rebuild. A required test: delete every index and the catalog, rebuild, and get byte-identical index files and identical query results.

### Retention

Delete the oldest whole segments when total size exceeds a cap (default 10 GiB) or a segment's newest `observed_time` (when it was written, not event time, so backfilled history is not deleted the moment it arrives) is older than a maximum age (default 30 days). Both are configurable. Per-source retention is a later feature.

## 8. Sources

### Traits

```rust
#[async_trait]
pub trait Receiver: Send {
    fn name(&self) -> &str;
    async fn run(self: Box<Self>, sink: Sink, shutdown: Shutdown) -> Result<()>;
}

#[async_trait]
pub trait Collector: Send {
    fn name(&self) -> &str;
    async fn run(self: Box<Self>, sink: Sink, cursors: CursorStore, shutdown: Shutdown) -> Result<()>;
}
```

`Sink` accepts batches of normalized records and applies backpressure when the writer's bounded channel is full. Receivers on reliable transports slow their senders (gRPC and HTTP return retryable errors, sockets stop reading); datagram transports drop and count. Every source reports counters (received, dropped, errors, last seen) to `status`.

### MVP receivers

| Receiver | Default listen | Notes |
|---|---|---|
| OTLP/gRPC | `127.0.0.1:4317` | Logs and traces services (tonic). Metrics service accepted and dropped. |
| OTLP/HTTP | `127.0.0.1:4318` | `/v1/logs`, `/v1/traces` (protobuf and JSON), `/v1/metrics` accepted and dropped. |
| NDJSON over HTTP | same server as OTLP/HTTP | `POST /ingest`, one JSON object or plain text line per line. |
| NDJSON over UNIX socket | `$XDG_RUNTIME_DIR/wartlog/ingest.sock` | Stream socket; one record per line. |
| NDJSON over TCP | `127.0.0.1:5170` | Same framing. |

Ports 4317 and 4318 are the OTLP standard. If another collector already holds them, WartLog logs a clear error and the user changes the port in config.

**NDJSON normalization.** Recognized keys, case-insensitive: time from `time`, `timestamp`, `ts`, `@t`; severity from `level`, `severity`, `lvl`, `@l`; body from `msg`, `message`, `@m`; template from `@mt` (CLEF, so Serilog's compact format works unchanged); `trace_id`/`traceId`/`@tr`; `span_id`/`spanId`/`@sp`; source from `service`, `service.name`, `app`, or the connection's configured name. Everything else becomes attributes. A line that is not JSON becomes a record with that line as the body.

**Delivery and duplicates.** Collectors commit their cursor only after the records it covers are fsynced in the WAL, so a crash never loses records past a saved cursor and a clean restart produces no duplicates. After a crash, a few duplicates are acceptable. The Kubernetes collector resumes with `sinceTime` one second before the last seen timestamp and drops lines whose `(pod, container, timestamp, line hash)` it already wrote in that overlap window.

**OTLP forwarding.** Optional per receiver: `forward = [{ endpoint = "...", headers = {...} }]`. WartLog re-sends the raw OTLP request it received to each forward target, asynchronously and best effort, so the Aspire dashboard keeps working while apps send to WartLog. (The Aspire dashboard has no forwarding of its own, which is why the tee lives here.)

**Containers reaching the host.** Aspire container resources and Docker containers cannot reach `127.0.0.1` on the host. Config allows extra bind addresses (for example the Docker bridge address). Binding a non-loopback address logs a warning, since the MVP has no authentication or TLS.

### MVP collectors

**journald.** Reads the user journal always, and the system journal when the user can (membership in `systemd-journal`; the viewer explains this when access is denied). Uses the sd-journal API through libsystemd (binding crate is the builder's call). Follows live by default. Mapping: `MESSAGE` to body, `PRIORITY` to severity (0 emerg to 24, 3 err to 17, 4 warning to 13, 6 info to 9, 7 debug to 5, and so on), `_SYSTEMD_UNIT` or `_SYSTEMD_USER_UNIT` and `SYSLOG_IDENTIFIER` into the source descriptor, `MESSAGE_ID`, `_PID`, `_COMM`, `_EXE`, `CODE_FILE`, `CODE_LINE`, `CODE_FUNC` and other fields to attributes. The `-x` catalog explanation is looked up at view time from `MESSAGE_ID`, not stored. The journal cursor is saved in `state/cursors.toml` (committed as described under "Delivery and duplicates"). On first run, import the last 24 hours (configurable). Optional include and exclude lists of units.

**Kubernetes.** Uses kube-rs with the user's kubeconfig. Each configured target names a context, namespaces, and an optional label selector. The collector watches pods, opens a follow log stream per container (`follow=true`, `timestamps=true`, resuming from the last seen timestamp), and closes streams when pods go away. JSON lines go through NDJSON normalization; other lines become bodies. Resource attributes: `k8s.cluster.name` (context), `k8s.namespace.name`, `k8s.pod.name`, `k8s.container.name`, `k8s.node.name`, and configured pod labels. Limits: only container stdout and stderr, and only the history the kubelet still holds. Nothing is installed in the cluster.

## 9. Query

### Shape

Two tables: `logs` and `spans`. A query with no table name means `logs`. Columns:

| Column | Meaning |
|---|---|
| `Timestamp`, `ObservedTimestamp` | datetime |
| `SeverityNumber`, `SeverityText`, `Level` | `Level` is a derived string: trace, debug, info, warn, error, fatal |
| `Body` | rendered message text (template plus args when there is a template) |
| `Template` | the template string, or empty |
| `Source`, `SourceKind` | source display name and kind |
| `Service`, `Unit`, `Namespace`, `Pod`, `Container` | convenience aliases for common resource attributes |
| `TraceId`, `SpanId` | hex strings |
| `Attributes["key"]`, `Resource["key"]` | dynamic access to record and resource attributes |
| spans only: `Name`, `ParentSpanId`, `EndTimestamp`, `Duration`, `Kind`, `Status` | |

### Supported subset (MVP)

- Tabular operators: `where`, `project`, `project-away`, `extend`, `take`/`limit`, `sort by`/`order by`, `top`, `summarize` (with `count()`, `countif()`, `dcount()`, `min()`, `max()`, `avg()`, `sum()`, and `by` including `bin()`), `distinct`, `count`, `search`.
- Predicates: `==`, `!=`, `<`, `<=`, `>`, `>=`, `and`, `or`, `not()`, `has`, `!has`, `contains`, `!contains`, `startswith`, `endswith`, `in`, `!in`, `between`, `matches regex`, `isempty()`, `isnotempty()`.
- Scalar functions: `ago()`, `now()`, `datetime()`, `timespan` literals (`5m`, `1h`), `bin()`, `tostring()`, `toint()`, `tolong()`, `todouble()`, `tolower()`, `toupper()`, `strlen()`, `iff()`.
- Anything else fails with a clear error naming the unsupported operator or function, never a silent partial result.

### Execution

1. Parse to an AST with source spans (for error messages and completions).
2. Plan: extract the time range (from `Timestamp` predicates or the viewer's time picker, which is injected as a range when the query has none) and the pushdown predicates: severity, source, attribute keys present, `has` terms, trace id.
3. Prune: read the catalog and block indexes, keep only blocks whose summaries can match.
4. Scan candidate blocks in parallel (rayon), newest first by default, decode, apply row predicates.
5. Run the remaining operators, streaming result batches back to the client.

Live tail runs a query restricted to streaming operators (`where`, `project`, `project-away`, `extend`) against the live bus after an initial backfill.

### Conformance

`tools/kql-conformance` declares WartLog's schema to Kusto.Language and checks every query in `crates/wartlog-query/tests/queries/` for syntax and semantic diagnostics. Queries that rely on the implicit `logs` table are prefixed with `logs | ` before being sent to Kusto.Language, since KQL itself requires a table. CI fails if Kusto.Language and WartLog disagree on whether a query is valid. Expected results for the same queries run against fixture data are golden files.

## 10. Daemon API

- Transport: stream UNIX socket at `$XDG_RUNTIME_DIR/wartlog/api.sock`, directory mode 0700.
- Framing: `u32` length plus a CBOR-encoded message (serde). The first exchange is a version handshake.
- Requests:
  - `query { kql, time_range, limit, order }` returns a stream of row batches, then stats (blocks considered, pruned, scanned, rows, elapsed).
  - `tail { kql, backfill }` returns a stream that stays open.
  - `sources.list`, `sources.add`, `sources.remove`, `sources.update`. The daemon is the only writer of the config file (see section 12).
  - `schema { time_range }` returns columns and attribute keys seen per source, with recent distinct values for completion.
  - `record.get { id }` and `record.context { id, before, after, same: source | trace | thread }`.
  - `trace.get { trace_id }` returns spans and logs for a trace.
  - `templates.top { kql, time_range, limit }` returns templates with counts.
  - `status` returns per-source counters, ingest rate, disk use, index health.
  - `index.rebuild { segment? }`.
- Record ids are the ingest `seq`, valid from the moment a record is accepted, including while it is still in the WAL. The daemon resolves a seq through the WAL (unsealed) or by binary search over block `first_seq`/`last_seq` ranges (sealed). Live tail rows can therefore be opened for details and context immediately.

The MCP server (phase 1.5) maps onto `query`, `schema`, `templates.top`, `trace.get`, `record.context` and `sources.list`, so it needs no new daemon features. Every response that can be large already supports row and byte limits for that reason.

## 11. Viewer

Built with cxx-qt and QML using Kirigami so it follows the Plasma theme. The UI talks only to the daemon API; it never reads data files.

### Main window

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ [Last 15 min ▾]  [ logs | where Unit == "foo.service" and Level == "error"  ]│
│                    completions: Unit Level Service Pod has contains ...      │
├───────────────┬──────────────────────────────────────────────────────────────┤
│ Sources       │ 18:22:07.113  ERROR  foo.service  Connection to {db} failed  │
│ ▾ journald    │ 18:22:07.090  WARN   foo.service  Retrying in {5s}           │
│   foo.service │ 18:21:59.412  INFO   api (k8s)    GET /health 200 in {3ms}   │
│   kwin        │ ...                                   ● Live  (paused: 214 new)│
│ ▾ Kubernetes  ├──────────────────────────────────────────────────────────────┤
│   dev/api-7f..│ Details: Body, Template, every attribute as a clickable chip │
│ ▾ OTLP        │ [Only this] [Exclude] [Show context ±30s] [Open trace]       │
│   orders-svc  │                                                              │
│ Saved views   │                                                              │
│   foo errors  │                                                              │
└───────────────┴──────────────────────────────────────────────────────────────┘
```

### Behaviors

- **Click to filter, then read the query.** Clicking a source, a severity chip, or "Only this"/"Exclude" on any field edits the query text visibly. Typing KQL is never required for common tasks; the query bar teaches it.
- **Completion** for tables, columns, operators, functions, attribute keys and recent values, from the `schema` request.
- **Live tail** is on by default for a source view. Scrolling up pauses it and shows a count of new records; returning to the bottom resumes.
- **Virtualized table** backed by a Rust list model; only visible rows are rendered. Template arguments are highlighted inside the rendered message.
- **Context**: from any record, show what the same source, thread or trace did in the surrounding window.
- **Templates view**: group the current query's results by template with counts and sparklines, to collapse noise.
- **Trace link**: records with a trace id open a trace panel listing its spans (name, service, start, duration, status) and its logs in time order.
- **Saved views**: a named query plus time range, listed in the sidebar.
- **journald niceties**: unit picker with search, `-x` catalog text shown inline in details.
- **Add source**: a dialog per source kind that writes the config and shows the sender's snippet (for example the `OTEL_EXPORTER_OTLP_ENDPOINT` value, or a `wartlog send` command line).
- Keyboard first: `Ctrl+K` focuses the query bar, `Ctrl+L` toggles live tail, `/` searches within results, `Esc` clears the selection.

## 12. Configuration and setup

- Config file: `$XDG_CONFIG_HOME/wartlog/wartlog.toml`. The daemon watches it and hot-reloads sources, and is its only programmatic writer: the viewer's source dialogs call the `sources.*` API, and the daemon edits the file preserving comments (toml_edit). Hand edits by the user are expected and picked up on reload.
- Out of the box, with no config file: OTLP gRPC and HTTP on loopback, the NDJSON UNIX socket, and the user journal collector are enabled. Kubernetes targets and the system journal are added explicitly.
- Install: a systemd user unit (`wartlogd.service`, `Type=notify`) and a `.desktop` file for the viewer. Packaging format is the builder's call; `cargo install` plus `wartlog setup` (installs the unit and enables it) is enough for the MVP.

Example:

```toml
[storage]
max_size = "10GiB"
max_age = "30d"

[[receiver]]
type = "otlp-grpc"
listen = "127.0.0.1:4317"
forward = [{ endpoint = "http://localhost:18889", headers = { "x-otlp-api-key" = "..." } }]

[[collector]]
type = "journald"
journals = ["user", "system"]
backfill = "24h"

[[collector]]
type = "kubernetes"
context = "kind-dev"
namespaces = ["orders"]
label_selector = "app.kubernetes.io/part-of=shop"
```

The simplest possible source, from any shell:

```sh
my-app 2>&1 | wartlog send --source my-app
```

## 13. CLI

`wartlog query '<kql>' [--since 1h] [--json]`, `wartlog tail '<kql>'`, `wartlog send --source NAME` (reads stdin lines, NDJSON or plain text), `wartlog sources`, `wartlog status`, `wartlog index rebuild [--segment ID]`, `wartlog setup`. Later: `wartlog mcp`.

## 14. Roadmap

**MVP milestones**, in build order. Each ends with something Tim can use.

1. **Store and journald.** Model, WAL, segments, blocks, block index, rebuild, journald collector, a minimal daemon API (handshake and `query` only), and `wartlog query` with `where`/`take`/`sort` only. Real data from day one, and every client goes through the daemon from the start.
2. **Query and API.** Full KQL subset, pruning, conformance suite, the rest of the daemon API, `wartlog tail`, `wartlog send` and the NDJSON receivers.
3. **Viewer.** Main window, live tail, click to filter, completion, details, context, templates view, saved views.
4. **OTLP.** gRPC and HTTP receivers, spans, trace index, trace panel, forwarding to the Aspire dashboard.
5. **Kubernetes.** Collector, source tree entries, add-source dialogs for every kind.
6. **Hardening.** Crash and corruption test suite, retention, performance pass against the targets below.

**Phase 1.5:** MCP server (`wartlog mcp`, stdio, Rust `rmcp` SDK). Read-only tools: run a KQL query, list sources and fields, top templates, fetch a trace with its logs, record context. Results capped by rows and bytes and summaries preferred over raw dumps. Configurable redaction rules (regexes on attribute keys and values) applied to MCP output. Streamable HTTP transport later if remote agents need it.

**Phase 2:** trace waterfall view; Docker collector (Engine API socket); syslog (RFC 5424 and 3164 over UDP, TCP with RFC 6587 framing, UNIX); GELF (UDP with chunking, TCP, HTTP); file tailing; TLS for network receivers; Kusto.Language completions in the viewer if needed; `AddWartLog()` Aspire AppHost extension; per-source retention.

**Phase 3, only on demand:** named pipes, shared memory ring buffer with a client library, Fluent Forward, Loki/Elasticsearch/Splunk HEC/Seq compatibility endpoints, an in-cluster Kubernetes forwarder, WASM decoder plugins.

## 15. Performance targets

Targets for a typical developer laptop, measured in milestone 6, not guarantees:

- Sustained ingest of 50,000 records per second through OTLP/gRPC without drops.
- Live tail latency under 250 ms from receipt to viewer.
- A time-bounded `where` query with a selective term over 1 GiB of stored data returns first results in under 1 second.
- Daemon idle memory under 100 MiB.

## 16. Testing

- Format round trips: property tests (proptest) that encode and decode random records and blocks.
- Corruption: flip random bits, truncate segments and WAL at random offsets, delete or damage indexes; every undamaged block must still decode and every index must rebuild identically.
- Crash recovery: kill the daemon with SIGKILL during heavy ingest, restart, verify no corruption and loss bounded by the WAL fsync interval.
- KQL: parser unit tests, conformance against Kusto.Language, golden results over fixture data.
- Sources: OTLP tests using the OpenTelemetry Rust SDK exporter as the sender; NDJSON and CLEF fixtures; journald tests against a test journal file; Kubernetes tests against a kind cluster (optional CI job).
- Viewer: Rust model logic unit tested; UI smoke test launching the viewer against a seeded daemon.
- Benchmarks (criterion) for ingest, block encode and decode, and pruning.

## 17. Decisions left to the builder

Note: the block trailer carries only the low 32 bits of `block_seq`; backward scanning confirms a match by reading the header the trailer points to and checking its CRC.

These were settled as reasonable defaults and are marked as such; adjust if evidence says otherwise: exact block and segment size targets, zstd level, Bloom filter sizes and token rules, CBOR as the API encoding, the libsystemd binding crate, packaging, the NDJSON TCP port, the first-run journald backfill window, and storing nested OTLP values as JSON strings.
