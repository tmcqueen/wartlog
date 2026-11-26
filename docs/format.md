# WartLog on-disk format (version 1)

This is the normative description of what `wartlog-store` writes. If the code and this file
disagree, it is a bug in one of them. All integers are little-endian. All checksums are CRC32C
(Castagnoli). Nothing is transmuted from file bytes: every structure is parsed with bounds checks.
Terms such as WAL, seq, sealing, torn tail and postings are defined in the [glossary](glossary.md).

## Store layout

```
$XDG_DATA_HOME/wartlog/            (default ~/.local/share/wartlog; directories are 0700, files 0600)
├── WARTLOG                        store manifest: format version and owner (see below)
├── LOCK                           flock held by the single writer (wartlogd, or a library writer)
├── sources.wcat                   source catalog (rebuildable)
├── state/cursors.toml             collector resume positions (journald cursors, Kubernetes timestamps)
├── wal/<first seq, 20 digits>.wal write-ahead log files
└── segments/YYYY/MM/DD/<uuid v7>.wseg   segments (UUID v7 names sort by creation time)
                       <uuid v7>.wtix   term index for the segment (rebuildable)
```

The daemon's store keeps this name. A store created by the library (private mode) is named
`<name>.wartlogx/` by default. The name is a convention only: ownership is in the manifest, and
renaming a directory changes nothing.

Readers (`Snapshot`) take no locks. They read the WAL first, then list the segments, then drop
WAL records whose seq is already sealed. A record sealed between the two steps is therefore found
in its segment, and never missed.

## Records and seqs

Every accepted record gets a `seq`: a u64 that increases by one per record across the whole store.
It is the record id in the API, valid from the moment the record is accepted (while it is still in
the WAL) until retention deletes it.

## Segment file (`.wseg`)

```
SegmentHeader (64 bytes)
Block, Block, ...           appended; the last block may be torn after a crash
```

SegmentHeader:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | magic `WARTSEG\0` |
| 8 | 2 | version (1) |
| 10 | 2 | writer kind: 0 unknown, 1 wartlogd, 2 library private store, 3 library exclusive mode |
| 12 | 4 | header length (64) |
| 16 | 16 | segment id (UUID v7 bytes, same as the file name) |
| 32 | 16 | previous segment id (zeros for the first) |
| 48 | 8 | created, ns since the epoch |
| 56 | 4 | reserved, 0 |
| 60 | 4 | CRC32C of bytes 0..60 |

The writer kind is informational only. Nothing refuses a segment because of it (a daemon store may
hold segments written in exclusive mode), and an unknown value is shown as a number. Segments
written before the field existed hold 0. `wl index verify`, `wl store info` and `wl status` show it.

A segment is closed when it reaches 256 MiB or has been open for an hour (defaults), or on a clean
shutdown. Closing builds its term index.

## Block

```
BlockHeader (48) | payload (compressed_len) | Summary (summary_len) | BlockTrailer (16)
```

BlockHeader:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | magic `WBLK` |
| 4 | 2 | version (1) |
| 6 | 1 | codec: 0 none, 1 zstd |
| 7 | 1 | reserved |
| 8 | 8 | block_seq (per segment, from 0) |
| 16 | 4 | record count |
| 20 | 4 | uncompressed payload length |
| 24 | 4 | compressed payload length |
| 28 | 4 | summary length |
| 32 | 4 | CRC32C of the compressed payload |
| 36 | 4 | CRC32C of the summary |
| 40 | 4 | reserved |
| 44 | 4 | CRC32C of bytes 0..44 |

BlockTrailer: u32 total block length (header through trailer), u32 low 32 bits of block_seq,
u32 CRC32C of the previous 8 bytes, u32 magic `KLBW`. A backward scan uses the trailer to find the
header, and confirms it by the header's own CRC.

Blocks are **per source**: the writer keeps one open block per source fingerprint and seals it at
256 KiB of uncompressed payload or 5 seconds of wall-clock age (defaults). Sealing by wall-clock
age, not event time, keeps backfills from producing one block per second of history.

### Summary (uncompressed, so pruning never decompresses)

```
i64 min_time, i64 max_time            event time range (ns)
u64 first_seq, u64 last_seq
u32 log_count, u32 span_count
u32 severities                        bit n set when a record has severity number n
u32 reserved
i64 max_observed                      newest receive time (retention by age uses this)
u32 n, n x u64                        source fingerprints, sorted
u32 n, n x u64                        xxh3 hashes of attribute keys, sorted
```

### Payload (after decompression)

```
StringTable   u32 count, then per string: u32 len, bytes          (per-block interning)
SourceTable   u32 count, then per source: u64 fingerprint, u8 kind, u16 attr count,
              attr count x (u32 key string ref, u32 value string ref)
padding to 8 bytes
Records       each 8-byte aligned
```

Source kinds: 1 otlp, 2 journald, 3 k8s, 4 ndjson. A source fingerprint is xxh3-64 of the kind byte
followed by each identifying attribute as `key\0value\0`, attributes sorted by key.

Record, fixed part (72 bytes):

| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | record length including padding |
| 4 | 1 | kind: 1 log, 2 span |
| 5 | 1 | severity number (OpenTelemetry, 0..24) |
| 6 | 2 | flags: 1 trace id, 2 span id, 4 template, 8 body is a string ref, 16 severity text |
| 8 | 8 | event time, ns |
| 16 | 8 | observed (receive) time, ns |
| 24 | 4 | source ref (index into SourceTable) |
| 28 | 4 | template string ref, or 0xFFFFFFFF |
| 32 | 2 | attribute count |
| 34 | 2 | template argument count |
| 36 | 4 | body string ref (empty when a template is present: the body is rendered on read) |
| 40 | 16 | trace id (zeros when absent) |
| 56 | 8 | span id (zeros when absent) |
| 64 | 8 | seq |

Variable part, in order:

1. Span extension (28 bytes, spans only): 8 parent span id, i64 end time, u32 name ref,
   u8 span kind, u8 status (0 unset, 1 ok, 2 error), u16 flags (1 = has parent), u32 status message ref.
2. Severity text: u32 string ref, when flag 16 is set.
3. Template arguments, `arg count` values.
4. Attributes, `attribute count` x (u32 key string ref, value).

A value is a type tag then its payload: 0 null; 1 bool (1 byte); 2 i64; 3 f64; 6 timestamp (i64 ns);
7 duration (i64 ns); 4 string, 5 bytes, 8 JSON text (each a u32 string ref). Nested OTLP values
(arrays, maps) are stored as JSON text. Every tag from 9 up is reserved for future types whose
payload is a u32 string ref, so a reader that does not know the tag can skip it and show the raw
bytes (see the compatibility policy). A future type with any other payload width needs a new segment
version.

## Write-ahead log (`wal/*.wal`)

A WAL file is a sequence of frames: `u32 len, u32 CRC32C(bytes), bytes, u32 len`. The bytes are a
one-record block payload (the same encoding as above). The writer fsyncs at least every 250 ms, and
before acknowledging a batch to a collector that waits for durability (journald, Kubernetes), so
those collectors commit their cursors only after the records are safe.

Files rotate every 10 seconds and are named by the first seq they hold. A file is deleted once its
highest seq is below the lowest seq still in an open block. Recovery replays every intact frame
(stopping at the first torn or damaged one), skips seqs already sealed (every seq up to the newest
segment's predecessor's maximum, plus each source's maximum in the newest segment), and seals the
rest.

## Term index (`.wtix`)

Built when a segment closes (on a background thread by default), written to a temporary name and
renamed into place. Never modified afterwards. Always rebuildable from the segment, and a rebuild is
byte-identical.

```
Header (64)       magic "WARTTIX\0", u16 version (1), u16 reserved, u32 block count,
                  16 segment id, u64 segment length covered, u64 FST length, u64 postings length,
                  u32 reserved, u32 CRC32C of bytes 0..60
Block table       block count x 88-byte entries (below)
FST               term -> offset into postings (a finite state transducer: a compact sorted map,
                  in the `fst` crate's format)
Postings          per term: varint count, then varint deltas of block numbers
u32               CRC32C of everything before it (checked by `wl index verify`, not on open)
```

A reader uses the index only if the header's segment id and covered length match the segment;
otherwise it scans the segment (correct, just slower).

Block table entry: u64 offset, u32 compressed length, u32 uncompressed length, u32 record count,
u32 payload CRC, i64 min time, i64 max time, u32 severities, u32 span count, u64 first seq,
u64 last seq, i64 max observed, u64 source fingerprint (0 when the block has more than one source),
u64 block_seq. With the table, opening a segment does not read its blocks.

Terms:

| Term | Meaning |
|---|---|
| `t:<token>` | a lowercase token of 3 or more characters from the rendered body (KQL `has` tokenization) |
| `f:<key>\0<value>` | an attribute value (lowercased text, at most 64 bytes), for keys with few distinct values in the segment: at most the larger of records/10 and 1024 |
| `k:<key>` | the key's values are all indexed under `f:` (empty postings); a key without `k:` cannot be pruned by value |
| `x:<16 bytes>` | a trace id |

The cardinality rule keeps request ids and similar keys out of the index. It is a heuristic and
the known weak spot: a key that is low-cardinality in one segment and high in the next is pruned
in one and scanned in the other. Answers are the same either way.

There are no Bloom filters. The storage spike measured them on real data and they pruned nothing
the term index did not already prune (see `spikes/storage/FINDINGS.md`).

## Store manifest (`WARTLOG`)

```
magic "WARTLOG\0", u16 manifest version (1), u16 store format version (1),
u8 origin (1 daemon, 2 external), u8 reserved, u16 reserved, i64 created (ns since the epoch),
u32 len + creator (UTF-8, "name version", e.g. "wartlogd 0.1.0"), u32 CRC32C of everything before it
```

Written once when a store is created (temporary name, fsync, rename), and rewritten only by
adoption. A store without a manifest is a daemon store made before the manifest existed; nothing
writes one into it later.

Ownership is decided by the manifest, concurrency by `LOCK`. Every mismatch is refused, never
warned about:

| Writer | daemon store (or no manifest) | external store | no store yet |
|---|---|---|---|
| wartlogd | allowed | refused, unless started with `--adopt` | creates a daemon store |
| library, shared mode | through wartlogd only | refused: no daemon serves it | refused |
| library, private mode | refused | allowed | creates an external store |
| library, exclusive mode | allowed when the lock is free | allowed when the lock is free | refused |

Adoption (`wartlogd --adopt` or `wl store adopt`, which takes the lock) rewrites `origin` to daemon.
Readers open any store.

## Source catalog (`sources.wcat`)

`WARTCAT\0`, u16 version (1), u16 reserved, u32 count, then per source: u64 fingerprint, u8 kind,
u16 attr count, per attr u32 len + key bytes, u32 len + value bytes; then u32 CRC32C of everything
before it. The writer rewrites it (atomically) when a new source appears. A reader that finds it
missing or damaged derives it from the segments.

## Seq rules

Readers tolerate blocks out of time order and seq ranges that overlap across sources (both normal
with per-source blocks), but three rules are load-bearing:

1. Each source's seqs increase from one block to the next, in file order, across segments, and
   inside each block.
2. A seq is stored once.
3. No segment before the newest holds a seq above the newest segment's highest seq.

WAL recovery and readers count a WAL record as sealed when its seq is at most the highest seq of
the segments before the newest, or at most its source's highest seq in the newest segment. If rule 1
or 3 breaks, unsealed records are hidden, and the next writer open skips them and deletes the WAL
files. If rule 2 breaks, a lookup by seq returns whichever copy it finds first. `wl store verify`
checks all three, and also reports WAL records counted as sealed that no segment holds. It never
modifies the store (`--quick` reads block metadata only and skips the duplicate and WAL checks).

## Compatibility policy

Version 1 is the only version so far. A reader of this version:

- **Refuses** a newer store format version (manifest), manifest version, or segment version. A
  newer segment is listed as unreadable and skipped by readers; a writer refuses to open the store,
  since writing on would reuse that segment's seqs. Block versions change only together with the
  segment version.
- **Keeps** a record with an unknown value tag (shown as the raw bytes of its u32 string ref) or
  an unknown source kind (shown as `kind:<n>`, for example in `SourceKind`).
- **Shows** an unknown writer kind as a number and never acts on it.
- Writes reserved fields as zero and ignores them on read, unless a later version gives them a
  meaning (as segment header bytes 10 and 11 became the writer kind).

New versions only add: a reader that knows version N reads every version up to N.

## Retention

Every 60 seconds the writer deletes the oldest closed segments (index first, then segment) while
the store is over its size cap, or while a segment's newest observed time is older than the age
cap. Defaults: 10 GiB and 30 days. The open segment is never deleted.
