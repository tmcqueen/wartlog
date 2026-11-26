# Storage spike findings (2026-11-25)

Run on Tim's KDE laptop (4 CPUs, 16 GiB, systemd 259) against his user journal, and in the cloud against 2 million synthetic entries loaded into a real journal file with `systemd-journal-remote`.

## Results

| | Real user journal | Synthetic |
|---|---|---|
| Entries | 65,392 (about 3 KB of JSON each) | 2,000,000 |
| journald on disk | 71.2 MB | 690 MB |
| WartLog store | 2.5 MB (28x smaller) | 81 MB (8.5x smaller) |
| Bytes per record | 40 | 42 |
| Import speed (from `journalctl -o json`) | 16.9k records/s, 52 MB/s of JSON | 113k records/s, 86 MB/s |
| Rare term, `-g` vs `has` | 0.239 s vs 0.010 s | 5.76 s vs 0.017 s |
| Rare term, `-g` vs `contains` (substring scan) | 0.242 s vs 0.020 s | 5.60 s vs 0.113 s |
| Unit + time window | 0.026 s vs 0.012 s | 0.082 s vs 0.028 s |
| `-p 3` | 0.027 s vs 0.017 s | 1.10 s vs 0.14 s |

Every query returned the same rows from both tools.

## What the spike proved

1. **The format round-trips.** Segments, checksummed zstd blocks, per-block string tables and the 72-byte fixed record header store and return every entry.
2. **Pruning works.** A rare-term query decodes 1 block out of 106 (real data) or 1 out of 1,998 (synthetic). A unit plus time window query decodes 5 out of 106.
3. **Indexes rebuild from the logs alone, byte-identical.** Rebuilding took 1 ms (real) and 29 ms (synthetic).
4. **Queries work with no index files.** Reading block summaries from the segments gave the same pruning and speed as the `.widx` files. A full decode with no pruning was still faster than `journalctl -g`.
5. **Damage stays local.** Three 16-byte corruptions cost exactly three blocks: 1,648 of 65,392 records on real data, and about 3,000 of 2 million synthetic records. Everything else stayed readable.

## What it taught us (feed into the real design)

- **At desktop journal sizes, both tools are instant.** Tim's whole user journal is 65k entries, and journalctl answers indexed field queries in about 30 ms. WartLog's real wins are free-text search (12 to 24x here, about 340x at 2M entries), disk size, and holding many sources in one store. It doesn't win on raw speed for small, field-indexed lookups.
- **The `.widx` files barely matter on a warm cache with one segment.** Their value is avoiding one small read per block across many segments on a cold cache. Treat them as a cache, as the spec already allows, and measure cold-cache, multi-segment cases before tuning them.
- **Bloom filters are the biggest overhead.** About 3 KB per block is 13% of the real store, and it's stored twice (segment and index). Tune the bits per term and consider sizing by block term count. Also check whether the index really needs a full copy of each summary.
- **Block size sets the blast radius of damage.** With about 600 real records per block, a 16-byte hit loses about 2.5% of a small journal. Two improvements: a damaged summary currently drops a healthy block (recompute the summary from the payload instead), and index validation by segment id and length can't detect damage in place (the payload CRC catches it at read time, which is fine, but say so in the spec).
- **JSON parsing dominates import of real entries.** Real KDE journal entries carry many fields (about 3 KB each). The real collector reads through sd-journal, not `journalctl -o json`, so this number understates ingest speed. Benchmark the real path in milestone 1.
- **Interning per block pays off.** Repeated field values and messages compress to about 40 bytes per record, even though each entry has dozens of fields.

## Not tested

The WAL and crash recovery, cold-cache runs (needs sudo), many segments, concurrent ingest and query, templates, traces.

## Head-to-head with SQLite (2M synthetic entries, cloud box, 4 CPUs)

SQLite (bundled 3.x through rusqlite) uses one `logs` table with promoted columns (time, severity, unit, message) and the remaining fields as a JSON text column. It has B-tree indexes on `time`, `(unit, time)` and `(severity, time)`, plus an FTS5 index on `message`. Both loaders are Rust with the same serde_json parsing (`spikes/sqlite-compare`). SQLite times are measured in-process. WartLog times include process start and about 10 ms spent loading 2,000 block summaries, which a long-running daemon would keep in memory.

| | SQLite | WartLog | WartLog, 1 thread |
|---|---|---|---|
| On disk | 1.24 GB (1.16 GB without FTS) | 84 MB | |
| Load | 11.7 s insert + 3.0 s B-trees + 5.2 s FTS = 19.9 s | 15.9 s | |
| Rare term (`MATCH` vs `has`) | <0.1 ms | 16 ms | 16 ms |
| Substring (`LIKE` vs `contains`) | 420 ms | 121 ms | 446 ms |
| Unit + time window | 6 ms | 33 ms | 68 ms |
| Unit, whole log | 61 ms | 151 ms | 541 ms |
| Errors and worse | 327 ms | 147 ms | 465 ms |
| Everything in a time window (200k rows) | 373 ms | 299 ms | 753 ms |

Takeaways:

- **SQLite wins exact lookups.** A real inverted index (FTS5) and B-trees beat per-block Bloom filters and summaries for point queries. WartLog's wins on broad scans come mostly from parallelism; on one thread it loses most rows.
- **WartLog wins on size by 15x**, which matters for retention: 10 GiB of WartLog holds about as much as 150 GiB of this SQLite layout. It also keeps damage local and has indexes that can be thrown away and rebuilt.
- **SQLite's load numbers are flattering.** Its indexes were built in bulk after the insert. A live sink maintains them on every insert, which costs more.
- **Idea worth testing:** because WartLog's indexes are disposable, they could use real inverted structures, either our own (term to block list per segment) or SQLite itself as the index store over our segments. That would combine compact, durable segments with point lookups below a millisecond.

## Round 2: term index and per-source blocks (2M synthetic entries)

Two changes, measured separately:

- **Term index (`.wtix`, `src/tix.rs`)**: one file per segment, memory-mapped and used without parsing. It holds a 56-byte fixed table entry per block (location, time range, severities) and an FST dictionary of body terms (`t:`) and low-cardinality field values (`f:key\0value`). Each entry points to a varint list of block numbers. Fields with more distinct values than 10% of records (ids, timestamps) are left out automatically. A query that uses it never touches the `.widx` file.
- **Per-source blocks**: the writer keeps one open block per source and seals it at 256 KiB or 10 minutes of event time.

Store sizes: v1 84 MB; v1 plus term index 90 MB (the index is 5.7 MB); v2 (per-source blocks plus term index) **74 MB**, because similar lines compress better together (zstd ratio 9.9 vs 8.0). The term index rebuilds byte-identical (`index verify`).

In-process query time, including materializing every row as JSON (the same work the SQLite tool does), median of 5:

| Query | SQLite | v1 | v2 | v2, 1 thread | rows |
|---|---|---|---|---|---|
| Rare term | <0.05 ms | ~6 ms | **0.9 ms** | | 1 |
| Rare term in time window | <0.05 ms | | **0.8 ms** | | 1 |
| Unit + time window | 5.7 ms | ~23 ms | **3.3 ms** | | 1,952 |
| Unit, whole log | 61 ms | ~140 ms | **20 ms** | 63 ms | 20,038 |
| Errors and worse | 327 ms | ~135 ms | **96 ms** | 366 ms | 40,145 |
| Everything in a time window | 373 ms | ~280 ms | **180 ms** | 682 ms | 200,132 |
| Substring (`LIKE` / `contains`) | 420 ms | ~100 ms | **81 ms** | 315 ms | 1 |

Blocks decoded in v2: rare term 1 of 2,276; unit + window 5; unit over the whole log 34 (it was all 1,998 in v1). Row counts match SQLite in every query. A fresh process adds about 5 ms of startup on top of these.

Takeaways:

- **Per-source blocks fixed the unit lookups.** They turned "decode every block" into "decode the blocks of that unit". This was the bigger of the two changes.
- **The term index made rare terms sub-millisecond.** SQLite is still faster here, because it jumps to one row and we decompress one 256 KB block. That remaining gap is about 0.9 ms, below anything a person notices. Smaller blocks or per-block record offsets would close it if it ever mattered.
- **Unit + window, and unit over the whole log, now beat SQLite even single-threaded** (63 vs 61 ms is a tie). Broad scans still depend on parallelism.
- **Import cost, fixed.** The first version made import 33 s instead of 16 s. Rewriting the index build to work from each block's string table (tokenize each distinct message once and dedupe field pairs per block, instead of materializing every record) cut the build from 20.9 s to 4.0 s, and to 4.5 s on a single thread. So the waste was the problem, not the lack of threads. Import with per-source blocks and the term index now takes **16.6 s** (15.0 s without the index), with byte-identical output.
- **Opening the store without a daemon is practical.** Every number above comes from a fresh process that memory-maps the index, with no resident state.

## Round 3: DuckDB on Parquet (Tim's theme park dataset, valkyrie, 4 CPUs, 15 GB)

DuckDB 2.0.1 queries the Parquet files directly. WartLog imports DuckDB's NDJSON export (`duck-bench.sh`, generic `--format ndjson` importer), ordered by event time, with per-source blocks (source = attraction, venue or channel) and the term index. Both sides are timed as fresh processes that print every matching row as JSON, median of 5. Row counts matched in every query.

All 117,255,655 ride events (8 yearly files):

| Query | DuckDB | WartLog | rows |
|---|---|---|---|
| One guest's rides | 277 ms | 11 ms | 31 |
| One attraction, one day | 95 ms | 12 ms | 1,420 |
| Express lane, one day (sample hit an empty day) | 59 ms | 8 ms | 0 |
| One attraction, whole table | 5.97 s | 9.60 s | 5,263,278 |

One year (2025): rides 16M rows, transactions 2.9M, complaints 65k. WartLog took 8 to 27 ms for one guest, household, attraction-day, venue-day or rare complaint word, and DuckDB took 90 to 130 ms. DuckDB won the broad scans: rarest payment method over the whole table (338 vs 518 ms) and one attraction over the whole year (0.90 vs 1.14 s).

Sizes: Parquet 858 MB, WartLog 2.68 GB (3.1x larger, 22.9 bytes per record), NDJSON 24.8 GB. Import ran at 167k records/s (12 minutes, including DuckDB's sort and JSON export).

Takeaways:

- **Point lookups favor WartLog by 7 to 25x.** DuckDB has no index that helps a random `guest_id`, so it reads the column across the whole table; WartLog's per-block summaries and term index decode a handful of blocks. Part of DuckDB's time is a fixed floor (about 60 ms for an empty result: process start plus Parquet metadata), but the guest lookup grows with the table and WartLog's doesn't.
- **Broad scans favor DuckDB by about 1.5x.** Columnar vectorized execution beats decoding row records and building JSON per row. This is DuckDB's home turf; WartLog's answer is the aggregations the viewer runs, which should work on columns of decoded values rather than rows of JSON.
- **Parquet is 3.1x smaller, and most of the gap is ours to remove.** The Bloom filters are about 8 KB per block across 75k blocks, stored twice (segment summary and `.widx`): about 1.2 GB of the 2.68 GB. The compressed payload alone is 1.28 GB (1.5x Parquet). With the term index doing the pruning, Bloom filters should become optional or much smaller. Closing the rest needs columnar encoding inside a block (delta-encoded times and ids), which zstd on rows only partly recovers.
- **The term index build ran out of memory.** It collected every distinct field value as a string, ids and timestamps included, before dropping high-cardinality fields: about 6.5 GB per 256 MiB segment, killed by the kernel while DuckDB's sort was resident. Fixed (two passes; cardinality counted as hashes with a cap): peak RSS on 1M rides went from 585 MB to 75 MB with identical output. Not yet re-measured on the full set. The build also took 299 s for 8 segments, which needs a look.
- **Sealing blocks by event time is wrong for bulk import.** A 10-minute event-time cap on per-source blocks makes tiny blocks for sparse sources in historical data (one ride every 20 minutes per attraction gives 1 record per block). The import used a huge cap. The real writer should seal on wall-clock age for live data and on size for backfill.

### Round 3b: Bloom filters off (`--no-bloom`), with the memory fix

All 117M rides again, term index built during import:

| | Bloom on | Bloom off |
|---|---|---|
| Store | 2.68 GB (22.9 B/record) | **1.45 GB (12.4 B/record), 1.7x Parquet** |
| Import peak RSS | 6.4 GB (term index build) | **2.37 GB** |
| Import wall time | 702 s + 299 s separate index build | 1,039 s with the index inline |
| One guest (34 rows) | 4.5 ms in-process, 29 of 75,191 blocks | 4.1 ms, same 29 blocks |

Queries with Bloom off (fresh processes, against DuckDB): one guest 11 ms vs 288 ms; one attraction-day 9 vs 96 ms; one attraction over all years (792k rows) **1.49 s vs 4.18 s**. Per-source blocks mean WartLog reads only that attraction's blocks, while DuckDB scans every row group. (The earlier 5.26M-row attraction was dominated by JSON output.)

Takeaways:

- **Bloom filters bought nothing here.** The term index did all the pruning. My prediction that the guest lookup would fall back to a full scan was wrong: real guests ride repeatedly, so `guest_id` has fewer distinct values than 10% of a segment's rows and got indexed. My synthetic data had near-unique guests, so it didn't.
- **That cardinality rule is fragile.** Whether an id field gets indexed depends on segment size and how often values repeat, and it can differ from one segment to the next. The real design should decide per field (config or type: index ids exactly, with small postings) and keep a compact filter only for fields deliberately left out.
- **Import is bound by single-threaded JSON parsing (about 24 MB/s) plus the inline index build.** On a many-core machine, parse in parallel and build each segment's term index on a background thread.
