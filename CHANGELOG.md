# Changelog

All notable changes to the `afterautism` engine.

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

- 50k-contract corpus: point reads ~7 µs, page ~49 µs, full-text ~15 ms,
  typed-field queries ~44–51 ms, 506 B/contract index+metadata,
  908 B/contract with compressed document bodies (2016 laptop; see
  `BENCHMARKS.md`).

**License**: AGPL-3.0-or-later.
