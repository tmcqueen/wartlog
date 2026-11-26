# Glossary

Plain-language definitions of the terms used in WartLog's README, the
[design spec](superpowers/specs/2026-11-25-wartlog-mvp-design.md) and the
[on-disk format](format.md). Most of them come from databases, logging or OpenTelemetry, and the
docs use them without explaining them. Terms are grouped by topic, and within a group each entry
only leans on entries above it.

## How data is stored

**Record.** One stored item: a log line or a span. Everything below is about how records get from
a sender onto disk and back.

**seq (sequence number).** A 64-bit number the writer gives each record as it arrives, one higher
than the last, across the whole store. It is the record's id: the CLI, viewer and API use it to
open a record or show its context.

**fsync.** The system call that forces data the operating system is holding in memory out to the
physical disk. Until a write has been fsynced, a power cut can lose it, even though the program
already "wrote" it. fsync is slow, so programs batch writes and fsync now and then.

**Write-ahead log (WAL).** A small append-only file that each incoming record is written to
*before* it goes anywhere else. WartLog fsyncs it at least every 250 ms. If `wartlogd` crashes or
the power goes out, it replays the WAL on the next start, so records it already accepted are not
lost. Once those records are safely inside a sealed block, the WAL file holding them is deleted.
Postgres, SQLite in WAL mode and most databases use the same idea. Files live in `wal/`.

**Torn write, torn tail.** What is left when a crash interrupts a write halfway: the end of a file
holds a partial frame or block. WartLog detects it with checksums and ignores it. A torn tail never
breaks the data before it.

**Frame.** One length-prefixed, checksummed chunk in the WAL. Each frame holds one record, and the
length is repeated at the end so the file can be read backwards too.

**Block.** The basic unit of stored data: up to a few thousand records from one source, compressed
together. A block decodes on its own with nothing but its own bytes, so damage to one block never
spreads to another. It has a header, the compressed records, an uncompressed summary and a
trailer.

**Sealing a block.** Finishing it: the writer compresses the records collected so far, appends the
block to the current segment and fsyncs it. A block is sealed when it reaches 256 KiB of
uncompressed records or has been open for 5 seconds. After sealing, a block never changes.

**Segment (`.wseg`).** A data file made of many blocks appended one after another. The **open
segment** is the one currently being appended to. It is **closed** when it reaches 256 MiB, has
been open for an hour, or the daemon shuts down cleanly. Closed segments never change, which is
what lets readers use them without any locking. Segments live under `segments/YYYY/MM/DD/`.

**Block summary.** A small uncompressed section at the end of each block: its time range, first and
last seq, which severities and sources it contains, and which attribute keys appear. Queries read
summaries to decide whether a block could possibly match, without decompressing it.

**Pruning.** Skipping data a query cannot match, using summaries and the term index, instead of
reading it. Almost all of WartLog's query speed comes from pruning. The query stats report how
many blocks were considered, pruned and scanned.

**Scan.** The opposite of pruning: decompress a block and check each record. Correct for any query,
but slower.

**Retention.** Deleting old data so the store does not grow forever. Every minute the writer
deletes the oldest closed segments while the store is over its size cap (10 GiB by default) or a
segment's newest record arrived more than 30 days ago.

## Indexes

**Index.** Extra data that lets a query find things faster without reading everything. In WartLog
every index can be rebuilt from the segments, so losing one costs speed, never data.

**Token.** A run of letters and digits in a message, lowercased. `Connection to db-01 failed`
has the tokens `connection`, `to`, `db`, `01` and `failed`. This is how KQL's `has` operator
splits text.

**Term.** One entry in the term index: a body token of 3 or more characters, an attribute
key and value, or a trace id. See the "Terms" table in [format.md](format.md#term-index-wtix).

**Term index (`.wtix`).** One file per closed segment that maps each term to the list of blocks
containing it, plus a table of every block's summary. A query for a rare word reads the index,
finds the two blocks that contain the word, and skips the rest. It is built when the segment
closes and is never changed after that.

**Postings, posting list.** For one term, the list of blocks that contain it. The name comes from
search engines.

**FST (finite state transducer).** A compact, sorted data structure for looking up strings, used
here to find a term's postings quickly. WartLog uses the `fst` crate's format. You only need to
know it is a very small, fast dictionary.

**Cardinality.** How many distinct values something has. `Level` has low cardinality (a handful of
values); a request id has high cardinality (nearly every record is different). The term index only
stores attribute values for low-cardinality keys, because indexing every request id would make the
index as big as the data.

**Bloom filter.** A compact structure that answers "is this item definitely *not* in the set?"
with occasional false "maybe" answers. The original design put one in every block. The storage
spike showed they pruned nothing the term index did not already prune, so WartLog has none. They
still appear in the spec and spike notes for that reason.

**Catalog (`sources.wcat`).** The list of every source the store has seen, with its attributes.
It can be rebuilt from the segments.

**Rebuildable, byte-identical rebuild.** An index can be deleted and regenerated from the segments,
and the regenerated file has exactly the same bytes as the original. That makes it easy to test
that indexes are correct.

## Reading while writing

**Single writer.** Only one process, `wartlogd`, ever writes a store. It holds a `flock` (an
exclusive file lock from the operating system) on the `LOCK` file to make sure of it.

**Reader, snapshot.** Any other process (`wl`, the viewer) opens the store read-only and takes no
locks. A snapshot is a reader's view of which segments and WAL records exist at one moment. Readers
work with the daemon stopped.

**Atomic rename.** Writing a new file under a temporary name and then renaming it over the old
one. On Linux the rename happens all at once, so a reader sees either the old file or the new one,
never a half-written one.

**Memory map (mmap).** Asking the operating system to make a file appear as memory, so a program
reads it like an array and the OS loads pages from disk only as they are touched.

## File format details

**Checksum, CRC32C.** A short number computed from a piece of data. If even one bit of the data
changes, the checksum almost certainly changes too, so a reader can tell damaged data from good
data. CRC32C is the specific checksum WartLog uses everywhere.

**Magic.** A fixed byte string at the start of a structure (such as `WARTSEG\0` or `WBLK`) so a
reader can recognize it and find it again after damage.

**Little-endian.** The byte order for numbers on disk: least significant byte first, the native
order on x86 and ARM.

**ns since the epoch.** Timestamps are stored as nanoseconds since 1970-01-01 00:00:00 UTC (the
Unix epoch).

**zstd.** Zstandard, the compression algorithm used for block payloads. Fast to decompress, with
good compression on repetitive log text.

**String interning.** Storing each distinct string once per block and referring to it by number
everywhere else. Log data repeats the same strings constantly, so this saves a lot of space.

**xxh3.** A very fast non-cryptographic hash function. WartLog uses it for source fingerprints and
attribute key hashes.

**UUID v7.** A 128-bit unique id whose first bits are a timestamp, so ids created later sort
later. Segment files are named with one, which makes the file names sort by creation time.

**varint.** A variable-length integer encoding: small numbers take one byte, bigger ones take more.
Used in the term index postings.

## Logs, traces and sources

**OpenTelemetry (OTel).** An open standard for logs, traces and metrics, with SDKs for most
languages. WartLog's record model follows it.

**OTLP.** The OpenTelemetry Protocol, the wire format OTel SDKs use to send data. WartLog accepts it
over gRPC (port 4317) and HTTP (port 4318), the standard ports.

**gRPC.** An RPC framework over HTTP/2, used by one flavor of OTLP.

**Trace, span.** A trace is the whole story of one request as it moves through services. A span is
one timed step inside it (a database call, an HTTP handler), with a start, an end, a name and a
status. Spans in one trace share a **trace id**, and each has its own **span id** and usually a
parent span. A log record written during a span can carry both ids, which is how logs link to
their trace.

**Severity number.** OpenTelemetry's 1 to 24 scale for log levels: 1-4 trace, 5-8 debug, 9-12 info,
13-16 warn, 17-20 error, 21-24 fatal, and 0 for unspecified. The `Level` column turns it back into a
word.

**Event time and observed time.** Event time is when the thing happened, according to the sender.
Observed time is when WartLog received it. They differ for backfilled history and for senders with
wrong clocks. Retention uses observed time, so importing a month-old log does not get it deleted
immediately.

**Message template.** A log message with holes, like `Resolved host {Host} to {Address}`, stored
separately from the values that fill the holes. Grouping by template collapses thousands of
similar lines into one row with a count. The **templates view** and `wl templates` do this.

**Serilog, CLEF.** Serilog is a popular .NET logging library that writes message templates. CLEF
(Compact Log Event Format) is its one-JSON-object-per-line output, with keys like `@t`, `@mt` and
`@l`. WartLog reads CLEF as NDJSON.

**NDJSON.** Newline-delimited JSON: one JSON object per line. The simplest way for any program to
send structured logs. Lines that are not JSON are kept as plain text.

**Preamble.** An optional first line, `@wartlog source=NAME`, on an NDJSON socket connection that
names the source. `wl send --source` writes it for you.

**Source.** "Who sent this": a kind (otlp, journald, k8s, ndjson) plus identifying attributes such
as the service or unit name. The same service maps to the same source across restarts.

**Source fingerprint.** A 64-bit hash of a source's kind and identifying attributes. It is how
blocks, summaries and the catalog refer to a source cheaply.

**Receiver, collector.** Two ways data gets in. A receiver listens and waits for senders to push to
it (OTLP, NDJSON). A collector goes out and reads from somewhere (journald, Kubernetes).

**Cursor.** A collector's saved position, such as a journald cursor or the last Kubernetes log
timestamp, kept in `state/cursors.toml`. A collector saves its cursor only after the records
before it are fsynced in the WAL, so a restart never skips records. After a crash a few may
repeat.

**Backfill.** Importing history that already existed before WartLog started watching, such as the
last 24 hours of the journal on first run. For live tail, backfill is the recent records shown
before new ones start streaming.

**Backpressure.** When the writer cannot keep up, it pushes back on senders instead of dropping
data: gRPC and HTTP senders get a retry error, sockets stop being read.

**journald, unit.** journald is systemd's logging service, read with `journalctl`. A unit is a
systemd service or other managed thing, such as `foo.service`, and is usually the journald source
name.

**Aspire.** Microsoft's .NET tooling for running a multi-service app locally. It has a dashboard
that receives OTLP, so WartLog can forward OTLP on to it.

## Querying

**KQL.** Kusto Query Language, Microsoft's query language for log data (Azure Data Explorer, Log
Analytics). Queries read left to right as a pipeline: `logs | where Level == "error" | take 20`.
WartLog implements a subset.

**Kusto.Language.** Microsoft's open source KQL parser. It has no engine to run queries; the spec
plans to use it in tests to check that WartLog accepts the same queries.

**`has` versus `contains`.** `has` matches whole tokens: `Body has "timeout"` matches
`connection timeout` but not `timeouts`. `contains` matches any substring. `has` can use the term
index to prune; `contains` cannot, so it scans more blocks and is slower.

**Live tail.** Following new records as they arrive, like `tail -f` or `journalctl -f`.

**Pushdown.** Taking the parts of a query that can be checked against summaries or the term index
(time range, severity, source, `has` terms, trace id) and using them to prune before any record is
decoded.

## Daemon, CLI and viewer

**Daemon.** A long-running background program, here `wartlogd`, run as a systemd user service.

**Unix socket.** A local-only socket that is a file on disk. Only programs on the same machine can
connect, and file permissions control who can.

**CBOR.** Concise Binary Object Representation, a binary format with the same shapes as JSON
(maps, arrays, strings, numbers). The daemon API uses it.

**XDG directories.** The Linux desktop convention for where files go: `$XDG_DATA_HOME`
(`~/.local/share`) for data, `$XDG_CONFIG_HOME` (`~/.config`) for config, `$XDG_RUNTIME_DIR`
(`/run/user/<uid>`) for sockets.

**Qt Quick, QML, cxx-qt.** Qt Quick is Qt's framework for modern UIs, written in QML, a declarative
UI language. cxx-qt is the bridge that lets QML talk to Rust code.

**Kirigami.** KDE's UI framework on top of Qt Quick. The spec planned it; the build used plain Qt
Quick Controls with the KDE desktop style instead (spec section 18).

**MCP.** Model Context Protocol, a standard way to give AI assistants tools. A `wl mcp` server is on
the roadmap.

## Measuring performance

**RSS (resident set size).** How much physical memory a process is actually using right now. Peak
RSS is the most it used at any point.

**Page cache.** File data the operating system keeps in spare memory after reading it. A **warm
cache** means the files were read recently and come from memory; a **cold cache** means they come
from disk, which is slower. The README benchmarks are warm-cache numbers.

**Median of 5.** Each query is run five times and the middle time is reported, so one slow outlier
does not distort the result.
