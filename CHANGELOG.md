# Changelog

All notable changes to the `afterautism` engine.

## 0.0.3 — deletes, scalable pagination, ranking, and richer traversal

- **Deletion** — [`storage::Corpus::remove_nodes`] and
  [`storage::StagingCorpus::remove_nodes`] remove nodes atomically,
  cascading through typed fields, typed edges, the full-text index,
  payloads, and embeddings in one transaction. Incremental refreshes
  can now converge both directions: upsert what exists, remove what no
  longer does. Unknown ids are a no-op.
- **Literal full-text terms** — full-text and prefix terms are quoted
  for FTS5 before matching. Labels containing `-`, `.`, `(`, `)` or
  `"`, and searches for the bare operator words `and` / `or` / `not`,
  previously became FTS5 operators (e.g. `foo-bar` matched
  `foo NOT bar`) or FTS5 syntax errors; they now match literally.
  Multi-word terms keep the engine's implicit-AND semantics, so
  previously-working queries return identical results.
- **`prefix:` reachable from the language** — the IR and executor
  implemented it, but the parser could not produce it; it now matches
  node labels by token prefix, with the same literal-term quoting as
  full-text terms.
- **Exact, scalable pagination** — single-atom queries (full-text,
  prefix, kind, field, all) now page in SQL: the total is an exact
  `COUNT`, the page is a `id > cursor` keyset window, and nothing is
  silently truncated (the executor's old one-million-match cap is
  gone; composite expressions evaluate the full set). Full-text,
  prefix, kind, and `all` pages are bounded keyset windows on the id
  index; field-comparison pages are windows over the indexed value
  scan (the match range is still scanned per request, as in previous
  versions — but results are no longer materialized and filtered in
  Rust). A parity test pins the new paths bit-for-bit against the
  historical algorithm.
- **Indexed typed-field comparisons (schema v4)** — `field:name op
  value` runs as an indexed SQL range scan over per-kind partial value
  indexes (`idx_node_fields_cmp_int` / `_date` / `_bool` / `_float` /
  `_text`) instead of loading every value of the field into memory.
  Results are identical to the previous in-memory comparison for every
  kind/operator pairing — including cross-kind numeric comparisons,
  incomparable-kind `!=` matching, NaN literals, and missing fields —
  pinned by a parity test that runs both implementations against the
  same corpus.
- **bm25 ranking through the language** — [`query::exec::ResultOrder::ByRank`]
  orders full-text / prefix results by bm25 (lower = better, node id
  tiebreak) with a `(rank, id)` keyset for paging; results carry their
  ranks. Default ordering stays ascending node id.
- **Traversal** — the language now supports incoming traversal
  (`<-type:(...)`), a hop-count (`->type:2:(...)`, depth ≥ 1), and
  edge-type unions (`->(a|b):(...)`). Depth-1 single-type outgoing
  traversal behaves exactly as before.
- **Selector discipline** — the query language refuses unknown
  `field:value` selectors with a typed error instead of silently
  ignoring the selector; `kind:`, `re:`, `prefix:`, and the new
  explicit `text:` selector are reserved. (Label-field equality
  remains available through the typed API.)
- **Embedding persistence (schema v5)** — the corpus stores vector
  embeddings durably ([`storage::Corpus::put_embeddings`] /
  [`storage::Corpus::embeddings`], little-endian f32, validated on
  read), cascading on node deletion, and
  [`query::vector::VectorIndex::from_pairs`] rebuilds the search index
  from stored pairs. The language deliberately gains no `near:` atom:
  the engine is offline and owns no embedding model — adapters supply
  vectors.
- **Movable corpora** — [`storage::Corpus::checkpoint`] folds the WAL
  into the main file so a corpus can be copied or moved as a single
  file; the storage module documents the `-wal`/`-shm` sibling rule.
- **Typed corruption errors** — structural failures (foreign
  `application_id`, unsupported schema, impossible node ids) surface as
  [`query::QueryError::Corrupt`] instead of a generic execution string.
- **Typed lock errors** — lock contention during query execution now
  surfaces as the typed, retryable [`query::QueryError::Locked`]
  (mirroring [`storage::CorpusError::Locked`]) instead of a generic
  execution string.
- **`missing_docs` enforced** — every public item is documented, and
  the lint keeps it that way.
- **Hardening** — deterministic mutation tests (header truncation at
  every length, single-bit flips across every header byte, hostile
  query-input battery, seeded expression-generator round-trips) and
  a `limit = 0` pagination bug fixed.
- **Documentation** — `docs/ENGINE.md` teaches the engine from zero
  (concepts, verified walkthrough, performance, guarantees, limits,
  FAQ); `docs/QUERY.md` is the language reference; the README demo
  query fixed to actually match (`>=` on a date boundary).
- **Hot-path tightening** — restore skips the redundant pre-copy clear
  (the backup copy replaces the page image anyway); composite kind and
  label evaluation fetches ids only instead of materializing labels;
  regex scans stream labels in bounded parallel batches instead of
  holding the whole corpus in memory.
- **Internal hygiene** — per-check `DomainPolicy` matching is
  allocation-free; the consumer workspace (`product-adapters`)
  depends on the engine at the repository root instead of a stray
  vendored `3.0.0` directory.

### Breaking changes in 0.0.3

- `QueryExpr::Traverse` carries `direction`, `edge_types`, and `depth`
  instead of a single `edge_type`; `QueryResult` and `ExecOptions`
  gained fields (`ranks`, `next_after_rank`, `order`, `after_rank`).
  Struct-literal users of these types must update.
- Unknown `field:value` selectors are now hard parse errors.
- Schema version bumped to v5; older corpora migrate in place on open
  (v4 corpora gain the embeddings table).

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
