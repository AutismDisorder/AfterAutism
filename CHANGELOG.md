# Changelog

All notable changes to the `afterautism` engine.

## 0.0.2 — safe multi-process access

- Every connection now waits up to 5 s for a momentarily held lock
  instead of failing instantly with `SQLITE_BUSY`.
- All write transactions (`write_batch`, schema init, migrations) now
  begin `IMMEDIATE`: the write lock is taken at `BEGIN`, before any
  WAL read snapshot exists. Without this, a deferred transaction
  upgrading against a stale snapshot fails with `SQLITE_BUSY_SNAPSHOT`
  the moment the other writer commits — a failure the busy wait cannot
  retry.
- Lock contention that outlasts the wait is now typed: `SQLITE_BUSY` /
  `SQLITE_LOCKED` surface as [`CorpusError::Locked`] (previously a
  generic sqlite error), so callers can retry the whole operation.
- The multi-process model is documented on the `storage` module: WAL
  readers run concurrently, writes serialize, commits are atomic
  renames.
- Tests pin all three: the busy wait is set on every connection, a
  contender writer waits out a held lock and commits, and contention
  classifies as `Locked`.

## 0.0.1 — initial release

The first release of the engine: access to data — any filetype in (via
the `Adapter` contract), the right items out (one deterministic query
language over text, typed fields, and typed edges), reliably (typed,
versioned, migratable storage).

**Modules**

- `core` — opaque ids, the error model, `NetworkGate` (offline by default).
- `adapter` — the extension contract: `Adapter`, `BatchBuilder`, typed
  `FieldValue`s (string / number / date / boolean), nodes, edges.
- `storage` — versioned, migratable corpus (SQLite + FTS5): atomic
  staging commits, schema migrations, zstd-compressed payloads,
  backup/restore, keyset pagination.
- `query` — full-text (bm25), prefix, regex (parallel scan), typed-field
  comparisons, typed-edge traversal, boolean composition, deterministic
  ordering and pagination.
- `topology` — typed-edge filtering (emphasis masks) and deterministic
  graph algorithms over the visible subgraph.

**Dependencies**

- Runtime: `serde`, `thiserror`, `rusqlite`, `zstd`, `rayon`, `regex`.
- Dev-only: `serde_json`, `tempfile`, `criterion` (benchmarks).

**Measured**

- 50k-contract corpus: point reads ~8 µs, page ~57 µs, full-text ~13 ms,
  typed-field queries ~44–66 ms, 506 B/contract index+metadata,
  908 B/contract with compressed document bodies. Access paths are
  cache-bound and identical on a real HDD; commit/checkpoint is 2.7x
  slower on the HDD (full environment and caveats in `BENCHMARKS.md`).

**License**: AGPL-3.0-or-later.
