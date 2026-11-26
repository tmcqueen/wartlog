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
