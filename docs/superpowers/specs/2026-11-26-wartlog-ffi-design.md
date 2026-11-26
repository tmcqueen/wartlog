# WartLog Library and C ABI Design

Status: design agreed with Tim, 2026-11-26. Built 2026-11-26 (section 12 lists the differences). When it is built, [docs/format.md](../../format.md) gains the store manifest and the writer kind field described in section 4, and this file records any differences. Unfamiliar terms are defined in the [glossary](../../glossary.md).
Date: 2026-11-26
Owner: Tim (tmcqueen)

## 1. Intent

Tim wants to write log readers and writers in other languages (C# through P/Invoke first) on top of the Rust code that already exists, instead of reimplementing the format, the KQL engine and the crash recovery in each language. So WartLog ships a shared library, `libwartlog.so`, with a C ABI that can read and write stores.

Keeping one implementation of the encoding is the main point: a hand-written writer in another language is the likeliest way to get subtly broken segments.

## 2. Goals and non-goals

Goals:

- Read any store from another language, with or without `wartlogd` running: KQL queries, record lookup by seq.
- Write records from another language, either through `wartlogd` or into a store the library owns.
- Never let a library writer and `wartlogd` write the same store at the same time, and never let either quietly take over a store that belongs to the other.
- A stable ABI: new versions only add functions.

Non-goals:

- Multiple concurrent writers on one store. The single-writer design (one `flock` on `LOCK`, store-wide seqs, one owner for the WAL, open blocks, retention and term index builds) stays as it is.
- Exposing blocks, string tables, `Rec` views or any internal struct layout across the ABI.
- A single-file portable log format (a separate idea, see section 10).

## 3. Writer modes

A writer is opened in exactly one mode, chosen by the caller in the open call. There is no automatic mode that takes the lock when the daemon happens to be stopped: if `wartlogd` then started or restarted, it would fail on the lock and collection would stop with no visible cause, and a second library user could not write at all.

| Mode | What it does | Typical use |
|---|---|---|
| `WL_WRITER_SHARED` | Connects to the daemon's API socket and sends each batch as an `Ingest` request (section 6). The daemon keeps the lock; the library never touches the files. | Apps writing to Tim's main store |
| `WL_WRITER_PRIVATE` | Embeds `wartlog_store::Writer` for a store the library owns, with its own background thread for `sync`, `tick` (sealing, segment close, index builds) and retention, because a C# host will not call those. | Test harnesses, importers, an app that keeps its own log |
| `WL_WRITER_EXCLUSIVE` | Embeds `Writer` on a daemon-owned store. Takes `LOCK`; fails if `wartlogd` holds it. Same contract as `wl index rebuild`. | Bulk import or maintenance with the daemon stopped |

If the real need is "write even when the daemon is down", the library may offer to start the daemon (`systemctl --user start wartlogd`) and then connect in shared mode, rather than taking the lock.

## 4. Store ownership

### Store manifest (authoritative)

A store is a directory (`LOCK`, `wal/`, `segments/`, `sources.wcat`), so ownership is marked on the store, not on its files. A new file at the store root, `WARTLOG`, holds:

- the store format version
- `origin`: `daemon` or `external`
- creation time
- the creating program (name and version)

The manifest is written once when the store is created, atomically (temporary name, then rename), and changed only by adoption (below). A store with no manifest is treated as `origin = daemon`, which covers every store that exists today (including `~/.local/share/wartlog`). The exact encoding is left to the build, in the style of `sources.wcat` (magic, version, fields, CRC32C) or as a small TOML file; format.md records the choice.

This also gives the format a store-level version, which is part of the compatibility policy listed in section 8.

### Folder naming convention

When the library creates a store it names the directory `<name>.wartlogx/` by default ("WartLog eXternal"), so library stores are easy to spot and to associate with the viewer. The name is a convention only and decides nothing: renaming a directory must not change who owns it. The daemon's default root keeps its current name.

### Open rules (refuse, never warn)

A C ABI has no reliable channel for warnings, so every mismatch is a hard error with its own code.

| Open call | Daemon store | External store |
|---|---|---|
| Read (snapshot) | allowed | allowed |
| `WL_WRITER_SHARED` | allowed (needs `wartlogd` running) | error `WL_E_NO_DAEMON`: no daemon serves it |
| `WL_WRITER_PRIVATE` | error `WL_E_DAEMON_STORE` | allowed (creates the store if absent) |
| `WL_WRITER_EXCLUSIVE` | allowed only when the lock is free, else `WL_E_LOCKED` | allowed |

The lock still decides concurrency in every case; the manifest decides ownership.

`wartlogd` refuses to start on a store whose manifest says `external`, unless told to adopt it (`wartlogd --adopt`, or a `wl store adopt` command), which rewrites `origin` to `daemon`. This stops a config typo from turning a test store into the live sink.

### Writer kind in the segment header (informational only)

Segment header bytes 10 and 11 are a reserved `u16`, written as 0 today, covered by the header CRC (bytes 0 to 60) and ignored by the decoder. They become a writer kind, set when the segment is created:

| Value | Meaning |
|---:|---|
| 0 | unknown (every segment written before this change) |
| 1 | `wartlogd` |
| 2 | library, private store |
| 3 | library, exclusive mode |

Nothing refuses to open a segment because of this field: a daemon store may legitimately hold segments written in exclusive mode, and the daemon reads, indexes and ages them out like any other. `wl index verify` and status output show it, which helps when investigating a damaged segment. Existing readers open new segments unchanged because they do not read the field.

Considered and rejected (2026-11-26):

- **Different extensions per writer** (`*.wartlog`/`*.wartlogx` files, then `*.wseg`/`*.wsegx`). Both writers produce identical files, and every reader would treat them the same, so every place that lists segments would have to look for both extensions. Retention missing one would let a store grow past its cap silently. The store is a directory, so there is no single file to name.
- **Enforcing ownership through the segment header** (daemon writes 0x3736 at offset 10 and errors on anything else). No writer ever reopens a sealed segment for writing; both create new ones, so the check would never trip, while the real hazards (duplicate seqs, a second WAL, retention deleting under another writer) are store-level. "Anything else is an error" would also reject every existing segment (all 0) and any store holding exclusive-mode imports. WAL files have no header to mark.

## 5. The C ABI

Crate `crates/wartlog-ffi`, `crate-type = ["cdylib", "staticlib"]`, header generated by cbindgen into `include/wartlog.h`. Sketch, names final at build time:

```c
uint32_t    wl_abi_version(void);
const char *wl_last_error(void);              /* thread-local, valid until the next call on that thread */

/* read: works with or without the daemon */
int  wl_snapshot_open(const char *root, wl_snapshot **out);
int  wl_query(wl_snapshot *, const char *kql, const wl_query_opts *, wl_cursor **out);
int  wl_cursor_columns(wl_cursor *, uint32_t *count);
int  wl_cursor_column_name(wl_cursor *, uint32_t col, const char **p, size_t *len);
int  wl_cursor_next(wl_cursor *, wl_row **row);   /* 1 = row, 0 = done, < 0 = error */
int  wl_row_type(wl_row *, uint32_t col, wl_value_type *t);
int  wl_row_get_str(wl_row *, uint32_t col, const char **p, size_t *len);
int  wl_row_get_i64(wl_row *, uint32_t col, int64_t *v);
int  wl_row_get_f64(wl_row *, uint32_t col, double *v);
int  wl_row_get_bool(wl_row *, uint32_t col, int *v);
int  wl_row_id(wl_row *, uint64_t *seq);          /* when the row is one record */
const char *wl_row_json(wl_row *);
int  wl_record_get(wl_snapshot *, uint64_t seq, wl_record **out);
const char *wl_record_json(wl_record *);
void wl_record_free(wl_record *);
void wl_cursor_free(wl_cursor *);
void wl_snapshot_free(wl_snapshot *);

/* write */
int  wl_writer_open(const char *root, wl_writer_mode mode, const wl_writer_opts *, wl_writer **out);
int  wl_writer_push_json(wl_writer *, const char *line, size_t len, uint64_t *seq);  /* NDJSON normalization */
int  wl_source_new(wl_source_kind kind, wl_source **out);
int  wl_source_set_attr(wl_source *, const char *key, size_t klen, const char *val, size_t vlen);
void wl_source_free(wl_source *);
wl_record_builder *wl_record_new(const wl_source *);
int  wl_record_set_time(wl_record_builder *, int64_t ns);
int  wl_record_set_severity(wl_record_builder *, uint8_t number, const char *text, size_t len);
int  wl_record_set_body(wl_record_builder *, const char *p, size_t len);
int  wl_record_set_template(wl_record_builder *, const char *p, size_t len);
int  wl_record_set_trace(wl_record_builder *, const uint8_t trace_id[16], const uint8_t span_id[8]);
int  wl_record_set_attr_str(wl_record_builder *, const char *k, size_t kl, const char *v, size_t vl);
int  wl_record_set_attr_i64(wl_record_builder *, const char *k, size_t kl, int64_t v);
int  wl_record_set_attr_f64(wl_record_builder *, const char *k, size_t kl, double v);
int  wl_record_set_attr_bool(wl_record_builder *, const char *k, size_t kl, int v);
int  wl_writer_push(wl_writer *, wl_record_builder *, uint64_t *seq);  /* consumes the builder */
int  wl_writer_flush(wl_writer *, uint64_t *durable_seq);              /* blocks until durable */
int  wl_writer_close(wl_writer *);                                     /* seals and frees */
```

Rules:

- Every handle is opaque. Records go in through a builder and rows come out through a cursor, so no internal layout is part of the ABI.
- Every function returns a status code (0 ok, negative `WL_E_*`), with detail in `wl_last_error`. Rust panics are caught at the boundary and become `WL_E_PANIC`.
- Strings are UTF-8, passed as pointer plus length. Strings the library returns stay owned by the library and are valid until the owning handle is freed or the next call on it.
- Handles are not thread-safe unless documented: a snapshot may be shared by threads for reading; a writer and a cursor belong to one thread at a time.
- `wl_abi_version` lets wrappers refuse a library that is too old. New versions only add functions and enum values.
- Spans can be written through the builder later (`wl_span_new`); logs come first.

Later, optional: `wl_query_arrow` returning results through the Arrow C Data Interface for bulk reads, which C# (Apache.Arrow), Python and Go read without copying.

### C# wrapper

The first real consumer, used to prove the ABI: a `WartLog` NuGet package with `[LibraryImport]` declarations, UTF-8 marshalling, one `SafeHandle` subclass per handle type whose release calls the matching `wl_*_free`, and `libwartlog.so` under `runtimes/linux-x64/native`. Status codes become exceptions carrying `wl_last_error`.

## 6. Daemon changes

- **`Request::Ingest { records, durable }`** on the API socket, answered by `Response::Ingested { first_seq, last_seq, durable_seq }`. With `durable`, the reply waits until the batch is fsynced (the daemon already has `send_durable` for its collectors). This is the shared-mode write path, and gives any program acknowledged writes with record ids; today NDJSON is fire-and-forget.
- **Manifest handling:** write `WARTLOG` when creating a store, refuse an `external` store unless adopting it, and set writer kind 1 in new segment headers.
- **`docs/protocol.md`**, describing framing, the handshake and the CBOR shape of each message, so clients that do not use the library can speak the protocol. Lower priority than the library.

## 7. Library internals that differ from Rust callers

- **Open segment read with `pread`, not `mmap`.** If a mapped file is truncated (crash recovery trims the last segment), touching the pages raises SIGBUS, which kills a .NET or Python host. Sealed segments never change and may stay mapped.
- **No tokio in the library.** The daemon client uses blocking UNIX sockets, so the library does not start an async runtime inside the host process.
- **A cap on query threads** in `wl_query_opts` (default: a small number, not every core), since the query runs inside someone else's process.
- **A streaming cursor.** Query results are materialized today except for aggregations; the cursor should stream rows so a large result does not have to fit in memory.
- **Private and exclusive writers run their own maintenance thread** (sync every 250 ms, `tick`, retention), and `wl_writer_close` seals everything so a clean shutdown leaves no WAL to recover.

## 8. Also needed in the format

A compatibility policy in format.md, written before anything outside the repo reads the files: what a reader does with a newer store, segment or block version (refuse), with an unknown value tag or source kind (keep the record, show the raw value or kind number), and that reserved fields are written as zero and ignored on read unless a later version gives them a meaning (as section 4 does for bytes 10 and 11).

## 9. Store verification (`wl store verify`)

Readers tolerate blocks out of time order and overlapping seq ranges across sources (both normal with per-source blocks), but two orderings are load-bearing. `SealedSeqs` (wartlog-store `reader.rs`) assumes that every seq in a sealed segment is sealed once a later segment exists, and that within the newest segment each source's blocks are sealed in seq order. If either breaks, readers hide unsealed WAL records, and `Writer::open` recovery skips them and then deletes the WAL files: silent loss. Duplicate seqs (two writers) make `get(seq)` return whichever copy it finds first and make dedup drop real records.

The library writers reuse `Writer`, so they keep these rules, but nothing checks them today (`wl index verify` only checks term indexes). A new read-only `wl store verify`, also exposed as `wl_store_verify` in the ABI, reports:

- a source whose seqs go backwards from one block to the next (in file order, across segments)
- a seq that appears more than once
- a sealed segment holding a seq higher than the newest segment's highest seq

It exits non-zero on any finding, prints the segment, block offset and seqs involved, and never modifies the store.

## 10. Not in this design

A single-file portable log (one segment with its index appended, for mailing or attaching to a bug, and a natural output for redaction export). Useful, but a separate format and only built if wanted.

## 11. Build order

1. Format compatibility policy, store manifest and writer kind (format.md, store crate, daemon refusal and adoption).
2. `wl store verify` (section 9), so every later step, and any writer in another language, can be checked against the seq rules.
3. `Request::Ingest` with seq and durability reply.
4. A small Rust facade over store, query and the daemon client (the curated API the FFI wraps), with `pread` for the open segment and a streaming cursor.
5. `wartlog-ffi` with all three writer modes, the cbindgen header and C tests.
6. The C# wrapper and a sample reader and writer.
7. `docs/protocol.md`; Arrow output if bulk reads need it.

## 12. Build notes (differences from the design above)

Steps 1 and 2 (2026-11-26):

- The manifest is binary, in the style of `sources.wcat` (layout in format.md), not TOML: the
  store crate has no TOML dependency, and every other store file is CRC-checked binary.
  `wl store info` prints it.
- Exclusive mode never creates a store (`StoreError::NotFound`): it exists for daemon stores, and a
  new store should be made in private mode or by wartlogd.
- Ownership is checked before the lock, so a private writer on a daemon store gets "daemon store"
  even while wartlogd holds the lock, instead of "locked".
- A store without a manifest keeps having none: nothing writes one into an existing store.
- Value tags: there was no length for an unknown tag, so a reader could not skip it. Tags 9 and up
  are now reserved for types whose payload is a u32 string ref; a reader shows an unknown one as
  the raw bytes. Any other payload width needs a new segment version.
- `wl store verify` also checks seq order inside each block, and reports WAL records that count as
  sealed but are in no segment (the records recovery would silently delete). `--quick` checks
  block metadata only.
- `wl status` shows the store origin and segment counts per writer kind; `wl index verify` shows
  each segment's writer kind.

Steps 3 to 6 (2026-11-26):

- `Request::Ingest` goes through the writer thread's normal batch queue (`Batch.seqs` carries the
  reply), so API writes and collector writes share one ordering and one fsync schedule. An empty
  `Ingest` with `durable` is how a shared writer flushes.
- The facade crate is named `wartlog` (crates/wartlog). It exposes `Snapshot`, a `Rows` cursor,
  `Writer` with `WriteMode::{Shared, Private, Exclusive}`, `ErrorKind`, `verify_store`. Library
  defaults differ from the daemon's: 2 query threads, a 10 000 row cap, and no retention for
  private and exclusive writers unless asked (a library should not delete data by surprise; the
  daemon applies its own retention when it next runs).
- Shared mode checks that the daemon on the socket serves the requested store (by `Status`), and
  refuses an external store up front, both as `NoDaemon`.
- `pread` applies to every segment without a valid term index, not only "the open one": that is
  the only set a reader can tell apart, and it covers the segment crash recovery trims.
- Not done: the streaming cursor. Plain row results are still produced in full before the first
  row is returned (bounded by the row cap); aggregations already stream internally. The cursor API
  is shaped so this can change without touching the ABI. Spans cannot be written yet.
- `wartlog-api` builds without tokio (`default-features = false`) and has a blocking client;
  NDJSON normalization moved to `wartlog-model` so the library does not pull in the receivers.
- C ABI additions beyond the sketch: `wl_version`, `wl_last_error_span` (query error position),
  `wl_string_free`, `wl_default_store`, `wl_snapshot_stats`, `wl_cursor_remaining`,
  `wl_cursor_stats_json`, `wl_record_builder_free`. Option structs carry a `size` field. Modes and
  source kinds are passed as `int` (an out-of-range enum value from C would be undefined behaviour
  in Rust); `wl_row_get_str` renders any non-null value as text.
- C# wrapper in `bindings/dotnet` (WartLog package, xunit tests, a sample with write, read and
  shared commands). Rows are copied into managed values as they are read.
- `docs/protocol.md` and Arrow output (build step 7) are not done.
