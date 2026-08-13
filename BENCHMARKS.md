# Benchmarks — the access paths

Measured with Criterion 0.5 on the **final v0.0.1 code**. All timings
are medians of 100 samples with 95% confidence intervals, after 1 s of
warm-up and 3 s of measurement per benchmark.

## Environment (the machine the numbers came from)

| Component | Specification |
|---|---|
| CPU | Intel Core i7-7500U (Kaby Lake-U, 14 nm) |
| Cores / threads | 2 cores / 4 threads |
| Base / boost clock | 2.70 GHz / 3.50 GHz |
| L1 cache | 64 KiB per core (32 KiB data + 32 KiB instruction) |
| L2 cache | 256 KiB per core (512 KiB total) |
| L3 cache | 4 MiB shared |
| CPU governor | `powersave` (laptop power management; ~79% of max frequency during the run) |
| RAM | 7.6 GiB total (7,989,332 kB), 2 GiB swap |
| Storage (benchmarks, primary run) | **tmpfs (RAM-backed)** — corpora in `/tmp` |
| Storage (benchmarks, disk run) | **TOSHIBA MQ01ABD050** 2.5" rotational HDD (ROTA=1, ~5400 rpm), partition `/dev/sda3`, **ntfs3** filesystem, corpora in `AFTERAUTISM_BENCH_DIR` |
| Raw sequential write (measured) | tmpfs **2.8 GB/s** vs HDD **97.5 MB/s** (29x) |
| OS | Void Linux, kernel 6.18.42_1, x86_64 |
| Toolchain | rustc 1.97.1 (2026-07-14), cargo 1.97.0, edition 2024 |
| Build profile | `bench` (optimized, `opt-level = 3`, no debug assertions) |
| Benchmark date | 2026-08-12 14:31 UTC |

**Honesty note on the disk.** The primary run places corpora on a
RAM-backed tmpfs, so the storage numbers measure **CPU + memory +
page cache**. A second, full run with corpora on the real disk (the
rotational HDD above) produced the comparison below. In short: the
measured access paths are identical on disk because WAL +
`synchronous = NORMAL` makes write *latency* cache-bound — the disk's
real cost lands on commit/checkpoint, which is 2.7x slower on the HDD.

Treat the absolute numbers as *this machine, this filesystem*; the
relative shape (point reads ≈ µs, queries ≈ tens of ms at 50k items)
transfers to any hardware.

## Workload

A contracts corpus: **50,000 nodes** (labels), **59,999 typed edges**
(`parent`/`amendment`), **200,000 typed fields** (status / expiry /
amount / counterparty), FTS5-indexed labels; ~1/7 of labels carry the
term `renewable` for full-text tests. Vector search: 10,000 embeddings
× 64 dims. Topology: a 10,000-node visible graph.

## Storage

- `write_batch` 5k nodes + 20k fields: **400 ms** [398–405] → 12.5k elements/s
- `get_node` single (50k corpus): **7.9 µs** [7.7–8.1]
- `get_nodes` batched ×100: **105 µs** [102–106] (~1 µs/node)
- `field_values("expiry")` (50k rows): **48.6 ms** [48.0–50.0]
- `page_nodes` limit 100: **56.9 µs** [54.7–58.8]
- `set_payload` 160 KiB (zstd): **43.5 µs** [43.4–43.7]
- `payload` read + decompress 160 KiB: **79.2 µs** [78.8–79.7]

Storage efficiency (committed on-disk size, measured):
- Index + metadata only (labels, fields, edges, FTS5; no bodies):
  **24.1 MB = 506 B/contract**.
- With realistic ~4 KiB boilerplate bodies (zstd-compressed):
  **45.4 MB = 908 B/contract** (506 B index + ~400 B body).

## Query (50k-contract corpus)

- full-text `renewable` (~7.1k hits, bm25, total count): **13.0 ms** [13.0–13.0]
- prefix `acme*` (matches all 50k): **93.7 ms** [92.4–95.8]
- regex `^acme master agreement 4` (parallel scan, ~10k hits): **21.5 ms** [21.0–22.7]
- `field:status = active` (16.7k hits): **65.9 ms** [61.3–71.4]
- `field:expiry > 2027-01-01` (~25k hits): **54.3 ms** [52.5–60.2]
- boolean combo (3 field scans + 2 intersections): **151 ms** [148–153]
- `->parent:(...)` batched traversal (~1k sources): **58.4 ms** [57.7–59.7]
- paged query, limit 100 + total: **50.4 ms** [49.9–50.8]

## Vector search (10k embeddings, dim 64)

- top-10 nearest neighbours: **1.38 ms** [1.38–1.39]

## Topology (10k-node visible graph)

- `apply_filter` TextSearch (parallel): **1.32 ms** [1.25–1.41]
- `connected_components`: **5.74 ms** [5.65–5.96]
- `shortest_path` (real 14-hop BFS): **2.1 µs** [2.0–2.1]

## Interpretation

What these numbers support:

- **Point access is fast**: single node ~8 µs, batched ~1 µs/node, page
  ~57 µs, 160 KiB payload round-trip < 80 µs.
- **Query access is interactive-grade at embedded scale**: tens of
  milliseconds over 50k contracts on a 2016-era laptop running on
  `powersave` — comfortably inside API/UI latency budgets. Regex
  (parallel), full-text (FTS5), and vector search are the strongest
  paths.
- **Storage is dense**: 506 B/contract fully indexed, 908 B/contract
  with compressed document bodies.

What they do NOT support:

- **1M+ row analytics.** Field comparisons scan all values of the named
  field per query — O(fields-with-name), parallelized, but linear.
  Extrapolated, the boolean combo (~150 ms at 50k) becomes ~3 s at 1M
  contracts. That is the honest envelope; scaling beyond hundreds of
  thousands of items requires indexed field lookups, not faster
  hardware.
- **Claims against dedicated engines.** These are absolute numbers, not
  comparisons. No benchmark against Tantivy (search) or DuckDB
  (analytics) exists; do not make comparative claims until it does.
- **Real-disk write performance.** The write numbers are tmpfs upper
  bounds (see the environment note above).

Run-to-run variance: a second full run on the same machine produced
numbers within a few percent (e.g., full-text 13.0 vs 14.8 ms, field
equality 66 vs 51 ms); the powersave governor and laptop thermals move
the parallel paths most. Report medians with their CIs, never single
runs.

## tmpfs vs real disk (rotational HDD)

A full second run of the same suite with corpora on the HDD
(`AFTERAUTISM_BENCH_DIR` set). Medians:

| Benchmark | tmpfs | HDD | Delta |
|---|---|---|---|
| `write_batch` 5k (measured latency) | 400 ms | 398 ms | ~0% (cache-bound WAL latency) |
| **build 5k + commit (checkpoint + fsync)** | **420 ms** | **1132 ms** | **2.7x slower on HDD** |
| `get_node` | 7.9 µs | 7.9 µs | ~0% |
| `get_nodes` ×100 | 105 µs | 110 µs | ~0% |
| `field_values` | 48.6 ms | 44.7 ms | ~0% (variance) |
| `page_nodes` 100 | 56.9 µs | 50.7 µs | ~0% (variance) |
| `set_payload` 160 KiB | 43.5 µs | 44.1 µs | ~0% |
| payload round-trip | 79.2 µs | 82.7 µs | ~0% |
| full-text | 13.0 ms | 13.7 ms | ~0% |
| prefix (50k hits) | 93.7 ms | 92.4 ms | ~0% |
| regex scan | 21.5 ms | 20.8 ms | ~0% |
| field equality | 65.9 ms | 49.2 ms | ~0% (variance) |
| boolean combo | 151 ms | 139 ms | ~0% (variance) |
| traversal | 58.4 ms | 60.3 ms | ~0% |
| paged query | 50.4 ms | 49.5 ms | ~0% |
| vector search | 1.38 ms | 1.48 ms | ~0% |
| `apply_filter` | 1.32 ms | 1.67 ms | ~0% (variance) |
| connected components | 5.74 ms | 5.07 ms | ~0% (variance) |

Interpretation:

- **Read and query paths are identical on disk** (within run-to-run
  variance): after a corpus is built, it lives in the page cache, and
  SQLite serves from there. This is a *warm* workload.
- **Write *latency* is identical on disk** — with WAL and
  `synchronous = NORMAL`, `write_batch`/`set_payload` commit to the WAL
  in memory and page cache without per-transaction fsync. The disk
  shows up at **checkpoint/commit time**, measured by
  `storage/build/build_5k_and_commit`: **2.7x slower on the HDD**
  (420 ms → 1132 ms). On an SSD the checkpoint cost will sit between
  the two; on NTFS specifically, expect additional overhead.
- **Cold reads are not measured here** (every bench reads a corpus it
  just built). First-touch reads from disk will be slower than these
  numbers; subsequent reads are page-cache served.
- The 29x raw-write gap (97.5 MB/s vs 2.8 GB/s) is what the engine's
  buffered WAL design absorbs; the remaining 2.7x is the honest price
  of durability on this disk.

## Rerun

```sh
cargo bench                          # tmpfs (default)
AFTERAUTISM_BENCH_DIR=/path/on/disk cargo bench   # real-disk run
cargo bench --bench query            # query paths only
```

Raw estimates (medians + confidence intervals, JSON) land in
`target/criterion/` after each run.
