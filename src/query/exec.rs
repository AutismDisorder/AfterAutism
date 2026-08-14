// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Query executor — runs a typed IR against a corpus.

use crate::adapter::FieldValue;
use crate::core::NodeId;
use crate::query::error::{QueryError, Result, is_lock_contention};
use crate::query::ir::{FieldOp, QueryExpr, QueryResult, TraverseDirection};
use crate::storage::Corpus;
use rayon::prelude::*;
use std::collections::BTreeSet;

/// How query results are ordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResultOrder {
    /// Ascending node id — the engine's deterministic default, and the
    /// only ordering composite expressions support.
    #[default]
    ById,
    /// bm25 ascending rank (lower = better), node id as the tiebreak.
    /// Applies to full-text and prefix atoms; other atoms evaluate in
    /// [`ResultOrder::ById`] order and carry no ranks.
    ByRank,
}

/// Execution options.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Page size; `None` = no pagination.
    pub limit: Option<usize>,
    /// Keyset cursor from a previous page (id-ordered results).
    pub after: Option<NodeId>,
    /// Result ordering. Defaults to [`ResultOrder::ById`].
    pub order: ResultOrder,
    /// Rank keyset cursor from a previous rank-ordered page, paired with
    /// `after` (which is the id tiebreak of the same row). Consulted
    /// only when `order` is [`ResultOrder::ByRank`].
    pub after_rank: Option<f64>,
}

/// Execute a query expression against a corpus.
///
/// Single-atom queries (full-text, prefix, kind, field, all) page in
/// SQL: the total is an exact `COUNT` and nothing is silently
/// truncated. Full-text, prefix, kind, and `all` pages are true keyset
/// windows (`id > cursor` on the id index — bounded work per page).
/// Field-comparison pages are windows over the indexed value scan: the
/// match range is still scanned per request (as in every previous
/// version), but results are no longer materialized and filtered in
/// Rust. Composite expressions (boolean combinations, regex,
/// traversal) evaluate the full match set in memory first and then
/// apply the same keyset semantics.
pub fn execute(corpus: &Corpus, expr: &QueryExpr, opts: &ExecOptions) -> Result<QueryResult> {
    match expr {
        QueryExpr::Text(_) | QueryExpr::Prefix(_) => execute_text_atom(corpus, expr, opts),
        QueryExpr::All
        | QueryExpr::Kind(_)
        | QueryExpr::FieldEquals { .. }
        | QueryExpr::FieldCmp { .. } => execute_atom(corpus, expr, opts),
        _ => execute_full(corpus, expr, opts),
    }
}

/// Composite (and regex / traversal) execution: full match set first,
/// then the same keyset semantics the SQL paths apply. Result order is
/// always ascending node id here — rank only exists for text atoms.
fn execute_full(corpus: &Corpus, expr: &QueryExpr, opts: &ExecOptions) -> Result<QueryResult> {
    let matched = eval(corpus, expr)?;

    // Deterministic order: ascending by raw id.
    let mut ids: Vec<NodeId> = matched.into_iter().collect();
    ids.sort_by_key(|n| n.to_raw());

    let total = ids.len() as u64;

    // Keyset pagination: skip ids <= cursor, then take `limit`.
    let start = match opts.after {
        Some(cursor) => ids.partition_point(|n| n.to_raw() <= cursor.to_raw()),
        None => 0,
    };

    let (page, next_after) = match opts.limit {
        Some(limit) => {
            let end = (start + limit).min(ids.len());
            // A zero-size page has no cursor (this also fixes the
            // historical underflow for `limit = 0`).
            let next = if limit > 0 && start + limit < ids.len() {
                Some(ids[start + limit - 1])
            } else {
                None
            };
            (ids[start..end].to_vec(), next)
        }
        None => (ids[start..].to_vec(), None),
    };

    Ok(QueryResult {
        node_ids: page,
        total: Some(total),
        next_after,
        next_after_rank: None,
        ranks: None,
    })
}

/// Evaluate the expression to the full matching node-id set (no paging).
pub fn eval(corpus: &Corpus, expr: &QueryExpr) -> Result<BTreeSet<NodeId>> {
    match expr {
        QueryExpr::All => {
            // All nodes: walk the id space via node_count + get_nodes is
            // not possible; use a bounded id scan is wrong. Instead the
            // corpus exposes all ids via a dedicated method.
            let ids = corpus.all_node_ids().map_err(QueryError::from)?;
            Ok(ids.into_iter().collect())
        }
        QueryExpr::Text(t) => {
            // Full match set — no silent cap. `execute` pages text atoms
            // in SQL; this path feeds composite evaluation, where a
            // truncated set would silently corrupt `total` and set
            // algebra.
            let nodes = corpus
                .search_nodes(&fts_phrase_term(t), i64::MAX as usize)
                .map_err(QueryError::from)?;
            Ok(nodes.into_iter().map(|n| n.id).collect())
        }
        QueryExpr::Prefix(p) => {
            // An empty prefix can match no nodes by construction (see
            // [`QueryExpr::is_impossible`]); short-circuit it instead of
            // sending a bare `*` into FTS5, which is a syntax error.
            // This also keeps `prefix:""` returning the empty set, the
            // same result it produced before `prefix:` existed (it
            // parsed as an exact-label match on the empty string).
            if p.is_empty() {
                return Ok(BTreeSet::new());
            }
            let nodes = corpus
                .search_nodes(&fts_phrase_prefix(p), i64::MAX as usize)
                .map_err(QueryError::from)?;
            Ok(nodes.into_iter().map(|n| n.id).collect())
        }
        QueryExpr::Kind(k) => {
            // Composite evaluation needs ids only; the id-only query
            // skips label materialization.
            let ids = corpus.node_ids_by_kind(k).map_err(QueryError::from)?;
            Ok(ids.into_iter().collect())
        }
        QueryExpr::FieldEquals { field: _, value } => {
            // `field:value` on the label: exact-label search (id-only
            // form — labels are not needed for set algebra).
            let ids = corpus.node_ids_by_label(value).map_err(QueryError::from)?;
            Ok(ids.into_iter().collect())
        }
        QueryExpr::FieldCmp { field, op, value } => {
            // Typed-field comparison: nodes carrying `field` whose value
            // compares to `value` with `op`. Missing fields never match.
            // Evaluated as an indexed SQL range scan over the schema-v4
            // partial value indexes; the semantics are pinned to the
            // in-memory comparator below by a parity test covering every
            // kind/operator pairing (including cross-kind numerics and
            // NaN), so the results are byte-for-byte what the previous
            // full-scan produced — just indexed and streaming.
            eval_field_cmp(corpus, field, *op, value)
        }
        QueryExpr::Regex(pattern) => {
            let re =
                regex::Regex::new(pattern).map_err(|e| QueryError::InvalidRegex(e.to_string()))?;
            // Regex scanning is pure CPU over labels; stream them from
            // SQLite in bounded batches so a large corpus never holds
            // every label in memory at once. Batches run in parallel;
            // the BTreeSet keeps output deterministic.
            const BATCH: usize = 4096;
            let mut stmt = corpus
                .conn()
                .prepare("SELECT id, label FROM nodes ORDER BY id")
                .map_err(sql_error_to_query)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(sql_error_to_query)?;
            let mut matched: BTreeSet<NodeId> = BTreeSet::new();
            let mut batch: Vec<(NodeId, String)> = Vec::with_capacity(BATCH);
            for row in rows {
                let (id_i64, label) = row.map_err(sql_error_to_query)?;
                let id = u64::try_from(id_i64)
                    .map_err(|_| QueryError::Corrupt(format!("invalid node id: {id_i64}")))?;
                batch.push((NodeId::from_raw(id), label));
                if batch.len() == BATCH {
                    let hits: Vec<NodeId> = batch
                        .par_iter()
                        .filter(|(_, label)| re.is_match(label))
                        .map(|(id, _)| *id)
                        .collect();
                    matched.extend(hits);
                    batch.clear();
                }
            }
            let hits: Vec<NodeId> = batch
                .par_iter()
                .filter(|(_, label)| re.is_match(label))
                .map(|(id, _)| *id)
                .collect();
            matched.extend(hits);
            Ok(matched)
        }
        QueryExpr::And(a, b) => {
            let la = eval(corpus, a)?;
            let lb = eval(corpus, b)?;
            Ok(la.intersection(&lb).copied().collect())
        }
        QueryExpr::Or(a, b) => {
            let la = eval(corpus, a)?;
            let lb = eval(corpus, b)?;
            Ok(la.union(&lb).copied().collect())
        }
        QueryExpr::Not(a) => {
            let inner = eval(corpus, a)?;
            let all = eval(corpus, &QueryExpr::All)?;
            Ok(all.difference(&inner).copied().collect())
        }
        QueryExpr::Traverse {
            inner,
            direction,
            edge_types,
            depth,
        } => {
            // Breadth-first hop expansion. The starting match set is
            // not part of the result (single-hop semantics of earlier
            // versions = depth 1); each hop follows edges whose type
            // is in `edge_types` in the given direction. The BTreeSet
            // keeps the result deterministic regardless of expansion
            // order.
            let inner_ids = eval(corpus, inner)?;
            let mut seen: BTreeSet<NodeId> = BTreeSet::new();
            let mut frontier: Vec<NodeId> = inner_ids.into_iter().collect();
            for _ in 0..*depth {
                if frontier.is_empty() {
                    break;
                }
                let grouped = match direction {
                    TraverseDirection::Outgoing => corpus.get_outgoing_edges_for(&frontier),
                    TraverseDirection::Incoming => corpus.get_incoming_edges_for(&frontier),
                }
                .map_err(QueryError::from)?;
                let mut next = Vec::new();
                for (_, edges) in grouped {
                    for edge in edges {
                        if !edge_types
                            .iter()
                            .any(|t| t.as_str() == edge.edge_type.as_str())
                        {
                            continue;
                        }
                        let target = match direction {
                            TraverseDirection::Outgoing => edge.to,
                            TraverseDirection::Incoming => edge.from,
                        };
                        if seen.insert(target) {
                            next.push(target);
                        }
                    }
                }
                frontier = next;
            }
            Ok(seen)
        }
    }
}

/// Deterministic cross-kind field comparison. `None` = incomparable.
/// Rules: same-kind values compare naturally; integers, floats, and
/// dates compare numerically (via `f64` when kinds differ); strings
/// compare lexicographically with other strings; booleans compare
/// `false < true`; every other pairing is incomparable (ordering
/// predicates never match, equality is false unless kinds agree).
///
/// Kept (test-only) as the authoritative in-memory reference for the
/// SQL range scans in [`eval_field_cmp`]: the parity test runs both
/// against the same corpus and demands identical match sets.
#[cfg(test)]
#[allow(clippy::cast_precision_loss)] // cross-kind i64 -> f64 is documented
fn field_cmp(a: &FieldValue, b: &FieldValue) -> Option<std::cmp::Ordering> {
    use FieldValue::{Bool, Date, Float, Int, Str};
    match (a, b) {
        (Str(x), Str(y)) => Some(x.cmp(y)),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        // Integers and dates are both i64 values on the same scale; any
        // pairing compares directly.
        (Int(x) | Date(x), Int(y) | Date(y)) => Some(x.cmp(y)),
        (Float(x), Float(y)) => x.partial_cmp(y),
        // Cross numeric-kind comparisons go through f64 (documented:
        // large i64 values lose precision — exact equality across kinds
        // is deliberately not guaranteed).
        (Int(x) | Date(x), Float(y)) => (*x as f64).partial_cmp(y),
        (Float(x), Int(y) | Date(y)) => x.partial_cmp(&(*y as f64)),
        _ => None,
    }
}

/// Evaluate one typed-field predicate against a stored value
/// (the test reference for [`eval_field_cmp`]).
#[cfg(test)]
fn field_matches(stored: &FieldValue, op: FieldOp, literal: &FieldValue) -> bool {
    use FieldOp::{Eq, Gt, Gte, Lt, Lte, Ne};
    use std::cmp::Ordering::{Equal, Greater, Less};
    let ord = field_cmp(stored, literal);
    match op {
        Eq => ord == Some(Equal),
        Ne => ord != Some(Equal),
        Gt => ord == Some(Greater),
        Gte => matches!(ord, Some(Greater | Equal)),
        Lt => ord == Some(Less),
        Lte => matches!(ord, Some(Less | Equal)),
    }
}

/// Quote a user text term for FTS5's `MATCH` mini-language.
///
/// FTS5 has no escape character inside bare words: `-`, `*`, `(`, `)`,
/// `"` and the bare operator words (`and`, `or`, `not`) all change a
/// query's meaning, and several are outright syntax errors. Quoting is
/// the only way to ask for a literal token match. Each whitespace token
/// is quoted separately so multi-word terms keep the engine's implicit
/// AND semantics, and embedded quotes are doubled (FTS5's in-phrase
/// escape). For single-token terms this is bit-identical to the old raw
/// form; it only changes queries that FTS5 used to misparse.
fn fts_phrase_term(term: &str) -> String {
    term.split_whitespace()
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote a user prefix term for FTS5, keeping the `*` wildcard outside
/// the quotes (FTS5's phrase-prefix form `"tok"*`). The empty-prefix
/// case never reaches this (the executor short-circuits it), but the
/// fallback keeps the old raw form rather than panicking.
fn fts_phrase_prefix(prefix: &str) -> String {
    let mut toks: Vec<&str> = prefix.split_whitespace().collect();
    let Some(last) = toks.pop() else {
        // Degenerate empty prefix: keep the historic raw `*` form so
        // this input behaves exactly as before.
        return "*".to_string();
    };
    let mut parts: Vec<String> = toks
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    parts.push(format!("\"{}\"*", last.replace('"', "\"\"")));
    parts.join(" ")
}

/// Evaluate one typed-field predicate as an indexed SQL range scan.
///
/// The predicate set replicates `field_cmp` exactly, kind by kind:
/// - int/date literals: `value_int` rows of kind `int`/`date`, plus
///   `float` rows compared through f64;
/// - float literals: `float` rows, plus `int`/`date` rows cast to REAL;
/// - string literals: `value_text` rows of kind `str` (SQLite's BINARY
///   collation is byte-wise, matching Rust's `str` ordering);
/// - bool literals: `value_int` rows of kind `bool`.
///
/// Ordering and equality predicates are emitted as one `UNION ALL`
/// branch per stored kind, each with an equality `kind =` predicate —
/// the per-kind partial indexes (schema v4) are only used for equality
/// kind predicates, so each branch becomes an index range scan over
/// exactly its kind's rows. The branches are disjoint, so `UNION ALL`
/// is duplicate-free.
///
/// `!=` is `NOT (equal-set)`: an incomparable stored value (different
/// kind) is never equal, so it satisfies `!=` exactly as the in-memory
/// comparator says. A NaN *literal* is incomparable with everything and
/// is handled up front; a NaN *stored* value cannot exist (SQLite nulls
/// NaN on write), and a NULL value never matches any predicate. Nodes
/// without the field have no row and never match. The SQL carries no
/// `ORDER BY`: results land in a `BTreeSet` and `execute` re-sorts by
/// node id, so output determinism is exactly the old design's. The
/// schema-v4 partial indexes (`idx_node_fields_cmp_*`) turn each branch
/// into an index range scan instead of loading every value of the field
/// into memory.
/// One entry per stored kind: (condition after `name = ? AND`,
/// condition params). Branch kind predicates are disjoint, so
/// UNION ALL is duplicate-free. `!=` is the complement of the
/// equality set — any stored kind is a candidate, so it stays one
/// name-indexed scan.
fn field_cmp_branches(
    op: FieldOp,
    value: &FieldValue,
) -> Vec<(String, Vec<Box<dyn rusqlite::ToSql>>)> {
    let sym = op.to_string();
    match value {
        FieldValue::Int(x) | FieldValue::Date(x) => {
            let x = *x;
            if op == FieldOp::Ne {
                vec![(
                    "(NOT ((kind IN ('int','date') AND value_int = ?) \
                     OR (kind = 'float' AND value_float = ?)))"
                        .to_string(),
                    vec![Box::new(x) as Box<dyn rusqlite::ToSql>, Box::new(x as f64)],
                )]
            } else {
                vec![
                    (
                        format!("(kind = 'int' AND value_int {sym} ?)"),
                        vec![Box::new(x) as Box<dyn rusqlite::ToSql>],
                    ),
                    (
                        format!("(kind = 'date' AND value_int {sym} ?)"),
                        vec![Box::new(x) as Box<dyn rusqlite::ToSql>],
                    ),
                    (
                        format!("(kind = 'float' AND value_float {sym} ?)"),
                        vec![Box::new(x as f64) as Box<dyn rusqlite::ToSql>],
                    ),
                ]
            }
        }
        FieldValue::Float(y) => {
            let y = *y;
            if op == FieldOp::Ne {
                vec![(
                    "(NOT ((kind = 'float' AND value_float = ?) \
                     OR (kind IN ('int','date') AND CAST(value_int AS REAL) = ?)))"
                        .to_string(),
                    vec![Box::new(y) as Box<dyn rusqlite::ToSql>, Box::new(y)],
                )]
            } else {
                // The CAST branches compare on an expression of the
                // indexed column, so they cannot range-scan the value
                // index; they still hit the per-kind index for the
                // `name`/`kind` prefix and filter the cast.
                vec![
                    (
                        format!("(kind = 'float' AND value_float {sym} ?)"),
                        vec![Box::new(y) as Box<dyn rusqlite::ToSql>],
                    ),
                    (
                        format!("(kind = 'int' AND CAST(value_int AS REAL) {sym} ?)"),
                        vec![Box::new(y) as Box<dyn rusqlite::ToSql>],
                    ),
                    (
                        format!("(kind = 'date' AND CAST(value_int AS REAL) {sym} ?)"),
                        vec![Box::new(y) as Box<dyn rusqlite::ToSql>],
                    ),
                ]
            }
        }
        FieldValue::Str(s) => {
            if op == FieldOp::Ne {
                vec![(
                    "(NOT (kind = 'str' AND value_text = ?))".to_string(),
                    vec![Box::new(s.clone()) as Box<dyn rusqlite::ToSql>],
                )]
            } else {
                vec![(
                    format!("(kind = 'str' AND value_text {sym} ?)"),
                    vec![Box::new(s.clone()) as Box<dyn rusqlite::ToSql>],
                )]
            }
        }
        FieldValue::Bool(b) => {
            let b = i64::from(*b);
            if op == FieldOp::Ne {
                vec![(
                    "(NOT (kind = 'bool' AND value_int = ?))".to_string(),
                    vec![Box::new(b) as Box<dyn rusqlite::ToSql>],
                )]
            } else {
                vec![(
                    format!("(kind = 'bool' AND value_int {sym} ?)"),
                    vec![Box::new(b) as Box<dyn rusqlite::ToSql>],
                )]
            }
        }
    }
}

/// Evaluate one typed-field predicate over the full match set.
///
/// The branch SQL from [`field_cmp_branches`] is joined with
/// `UNION ALL` (duplicate-free — the branch kinds are disjoint). A
/// NaN *literal* is incomparable with everything: `!=` matches every
/// stored value of the field, every other operator matches nothing.
fn eval_field_cmp(
    corpus: &Corpus,
    field: &str,
    op: FieldOp,
    value: &FieldValue,
) -> Result<BTreeSet<NodeId>> {
    if let FieldValue::Float(y) = value {
        if y.is_nan() {
            if op != FieldOp::Ne {
                return Ok(BTreeSet::new());
            }
            let mut stmt = corpus
                .conn()
                .prepare("SELECT node_id FROM node_fields WHERE name = ?")
                .map_err(sql_error_to_query)?;
            let rows = stmt
                .query_map([field], |row| row.get::<_, i64>(0))
                .map_err(sql_error_to_query)?;
            return collect_node_ids(rows);
        }
    }

    let branches = field_cmp_branches(op, value);

    let mut sql_parts = Vec::with_capacity(branches.len());
    let mut all_params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    for (cond, mut branch_params) in branches {
        sql_parts.push(format!(
            "SELECT node_id FROM node_fields WHERE name = ? AND {cond}"
        ));
        all_params.push(Box::new(field.to_owned()));
        all_params.append(&mut branch_params);
    }

    let mut stmt = corpus
        .conn()
        .prepare(&sql_parts.join(" UNION ALL "))
        .map_err(sql_error_to_query)?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(all_params.iter().map(|p| p.as_ref())),
            |row| row.get::<_, i64>(0),
        )
        .map_err(sql_error_to_query)?;
    collect_node_ids(rows)
}

/// Collect `node_id` rows into a deterministically ordered id set,
/// validating each id the way the storage layer does.
fn collect_node_ids(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<i64>>,
) -> Result<BTreeSet<NodeId>> {
    let mut out = BTreeSet::new();
    for row in rows {
        let id_i64 = row.map_err(sql_error_to_query)?;
        let id = u64::try_from(id_i64).map_err(|_| {
            QueryError::Corrupt(format!("invalid node id in node_fields: {id_i64}"))
        })?;
        out.insert(NodeId::from_raw(id));
    }
    Ok(out)
}

/// Classify a direct `rusqlite` error the way the storage layer does:
/// lock contention is the retryable [`QueryError::Locked`], everything
/// else is a plain execution failure.
fn sql_error_to_query(e: rusqlite::Error) -> QueryError {
    if is_lock_contention(&e) {
        QueryError::Locked(e.to_string())
    } else {
        QueryError::Execution(e.to_string())
    }
}

/// Convert an `after` cursor (exclusive lower bound) to the signed form
/// SQLite stores node ids in.
fn cursor_i64(opts: &ExecOptions) -> Result<i64> {
    match opts.after {
        Some(id) => i64::try_from(id.to_raw())
            .map_err(|_| QueryError::Corrupt(format!("node id out of range: {}", id.to_raw()))),
        None => Ok(0),
    }
}

/// Rows to fetch for a page: `limit + 1` (the extra row tells us whether
/// a next page exists), or SQLite's "no limit" sentinel for unpaginated
/// queries.
fn fetch_count(opts: &ExecOptions) -> i64 {
    match opts.limit {
        Some(l) => i64::try_from(l.saturating_add(1)).unwrap_or(i64::MAX),
        None => -1,
    }
}

/// Slice a fetched keyset window into a page + next cursor, exactly
/// matching the in-memory pagination semantics the executor always had.
fn finish_page(fetched: Vec<NodeId>, total: i64, opts: &ExecOptions) -> Result<QueryResult> {
    let (page, next_after) = match opts.limit {
        Some(l) => {
            let next = (l > 0 && fetched.len() > l).then(|| fetched[l - 1]);
            (fetched.into_iter().take(l).collect(), next)
        }
        None => (fetched, None),
    };
    Ok(QueryResult {
        node_ids: page,
        total: Some(u64::try_from(total).unwrap_or(0)),
        next_after,
        next_after_rank: None,
        ranks: None,
    })
}

/// SQL-level keyset execution for id-ordered atoms (`all`, `kind:`,
/// `field:value`, `field:name op value`). The total is an exact SQL
/// `COUNT` — never a truncated set size — and the page is a
/// `id > cursor` window over the indexed scan.
fn execute_atom(corpus: &Corpus, expr: &QueryExpr, opts: &ExecOptions) -> Result<QueryResult> {
    let conn = corpus.conn();
    match expr {
        QueryExpr::All => {
            let total: i64 = conn
                .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
                .map_err(sql_error_to_query)?;
            let fetched = fetch_ids(
                conn,
                "SELECT id FROM nodes WHERE id > ? ORDER BY id LIMIT ?",
                &[Box::new(cursor_i64(opts)?), Box::new(fetch_count(opts))],
            )?;
            finish_page(fetched, total, opts)
        }
        QueryExpr::Kind(k) => {
            let total: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM nodes WHERE kind = ?",
                    [k.clone()],
                    |r| r.get(0),
                )
                .map_err(sql_error_to_query)?;
            let fetched = fetch_ids(
                conn,
                "SELECT id FROM nodes WHERE kind = ? AND id > ? ORDER BY id LIMIT ?",
                &[
                    Box::new(k.clone()),
                    Box::new(cursor_i64(opts)?),
                    Box::new(fetch_count(opts)),
                ],
            )?;
            finish_page(fetched, total, opts)
        }
        QueryExpr::FieldEquals { field: _, value } => {
            let total: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM nodes WHERE label = ?",
                    [value.clone()],
                    |r| r.get(0),
                )
                .map_err(sql_error_to_query)?;
            let fetched = fetch_ids(
                conn,
                "SELECT id FROM nodes WHERE label = ? AND id > ? ORDER BY id LIMIT ?",
                &[
                    Box::new(value.clone()),
                    Box::new(cursor_i64(opts)?),
                    Box::new(fetch_count(opts)),
                ],
            )?;
            finish_page(fetched, total, opts)
        }
        QueryExpr::FieldCmp { field, op, value } => field_cmp_page(conn, opts, field, *op, value),
        _ => unreachable!("execute_atom only receives atoms"),
    }
}

/// Page a typed-field comparison: exact `COUNT` over the branch union,
/// and a keyset window with the cursor pushed into every branch so each
/// stays an indexed range scan.
fn field_cmp_page(
    conn: &rusqlite::Connection,
    opts: &ExecOptions,
    field: &str,
    op: FieldOp,
    value: &FieldValue,
) -> Result<QueryResult> {
    // A NaN literal: `!=` matches every stored value, everything else
    // matches nothing (the in-memory comparator's `None` ordering).
    if let FieldValue::Float(y) = value {
        if y.is_nan() {
            if op != FieldOp::Ne {
                return finish_page(Vec::new(), 0, opts);
            }
            let total: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM node_fields WHERE name = ?",
                    [field],
                    |r| r.get(0),
                )
                .map_err(sql_error_to_query)?;
            let fetched = fetch_ids(
                conn,
                "SELECT node_id FROM node_fields WHERE name = ? AND node_id > ? ORDER BY node_id LIMIT ?",
                &[
                    Box::new(field.to_owned()),
                    Box::new(cursor_i64(opts)?),
                    Box::new(fetch_count(opts)),
                ],
            )?;
            return finish_page(fetched, total, opts);
        }
    }

    let total: i64 = {
        let branches = field_cmp_branches(op, value);
        let union = branches
            .iter()
            .map(|(cond, _)| format!("SELECT node_id FROM node_fields WHERE name = ? AND {cond}"))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for (_, mut bp) in branches {
            params.push(Box::new(field.to_owned()));
            params.append(&mut bp);
        }
        let mut stmt = conn
            .prepare(&format!("SELECT COUNT(*) FROM ({union})"))
            .map_err(sql_error_to_query)?;
        stmt.query_row(
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |r| r.get(0),
        )
        .map_err(sql_error_to_query)?
    };

    let cursor = cursor_i64(opts)?;
    let branches = field_cmp_branches(op, value);
    let mut page_parts = Vec::with_capacity(branches.len());
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    for (cond, mut bp) in branches {
        page_parts.push(format!(
            "SELECT node_id FROM node_fields WHERE name = ? AND {cond} AND node_id > ?"
        ));
        params.push(Box::new(field.to_owned()));
        params.append(&mut bp);
        params.push(Box::new(cursor));
    }
    let stmt = conn
        .prepare(&format!(
            "{} ORDER BY node_id LIMIT ?",
            page_parts.join(" UNION ALL ")
        ))
        .map_err(sql_error_to_query)?;
    let mut all_params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let limit_box = Box::new(fetch_count(opts));
    all_params.push(limit_box.as_ref());
    let fetched = fetch_ids_from(stmt, rusqlite::params_from_iter(all_params))?;
    finish_page(fetched, total, opts)
}

/// Run a keyset `SELECT id ...` statement and collect the rows in SQL
/// order (ascending id).
fn fetch_ids(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[Box<dyn rusqlite::ToSql>],
) -> Result<Vec<NodeId>> {
    let mut stmt = conn.prepare(sql).map_err(sql_error_to_query)?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |row| row.get::<_, i64>(0),
        )
        .map_err(sql_error_to_query)?;
    let mut out = Vec::new();
    for row in rows {
        let id_i64 = row.map_err(sql_error_to_query)?;
        let id = u64::try_from(id_i64)
            .map_err(|_| QueryError::Corrupt(format!("invalid node id: {id_i64}")))?;
        out.push(NodeId::from_raw(id));
    }
    Ok(out)
}

/// Shared tail of [`fetch_ids`] for callers that already prepared the
/// statement themselves.
fn fetch_ids_from(
    mut stmt: rusqlite::Statement<'_>,
    params: impl rusqlite::Params,
) -> Result<Vec<NodeId>> {
    let rows = stmt
        .query_map(params, |row| row.get::<_, i64>(0))
        .map_err(sql_error_to_query)?;
    let mut out = Vec::new();
    for row in rows {
        let id_i64 = row.map_err(sql_error_to_query)?;
        let id = u64::try_from(id_i64)
            .map_err(|_| QueryError::Corrupt(format!("invalid node id: {id_i64}")))?;
        out.push(NodeId::from_raw(id));
    }
    Ok(out)
}

/// Full-text and prefix atoms: id-ordered keyset pages by default, or
/// bm25-ranked pages under [`ResultOrder::ByRank`] with a `(rank, id)`
/// keyset. Totals are exact FTS5 `COUNT`s either way.
fn execute_text_atom(corpus: &Corpus, expr: &QueryExpr, opts: &ExecOptions) -> Result<QueryResult> {
    let conn = corpus.conn();
    let term = match expr {
        QueryExpr::Text(t) => fts_phrase_term(t),
        QueryExpr::Prefix(p) => {
            if p.is_empty() {
                return finish_page(Vec::new(), 0, opts);
            }
            fts_phrase_prefix(p)
        }
        _ => unreachable!("execute_text_atom only receives text/prefix"),
    };

    let total: i64 = {
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM nodes_fts WHERE nodes_fts MATCH ?")
            .map_err(sql_error_to_query)?;
        stmt.query_row([term.clone()], |r| r.get(0))
            .map_err(sql_error_to_query)?
    };

    match opts.order {
        ResultOrder::ById => {
            let fetched = fetch_ids(
                conn,
                "SELECT n.id FROM nodes_fts JOIN nodes n ON nodes_fts.rowid = n.id
                 WHERE nodes_fts MATCH ? AND n.id > ? ORDER BY n.id LIMIT ?",
                &[
                    Box::new(term),
                    Box::new(cursor_i64(opts)?),
                    Box::new(fetch_count(opts)),
                ],
            )?;
            finish_page(fetched, total, opts)
        }
        ResultOrder::ByRank => {
            // bm25: lower = better. Keyset cursor is the (rank, id)
            // pair of the last page row; the first page carries no
            // cursor clause.
            let cursor = cursor_i64(opts)?;
            let (sql, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = match opts.after_rank {
                Some(rank) => (
                    "SELECT n.id, bm25(nodes_fts) AS rank
                         FROM nodes_fts JOIN nodes n ON nodes_fts.rowid = n.id
                         WHERE nodes_fts MATCH ?
                           AND (bm25(nodes_fts) > ? OR (bm25(nodes_fts) = ? AND n.id > ?))
                         ORDER BY rank, n.id LIMIT ?"
                        .into(),
                    vec![
                        Box::new(term),
                        Box::new(rank),
                        Box::new(rank),
                        Box::new(cursor),
                        Box::new(fetch_count(opts)),
                    ],
                ),
                None => (
                    "SELECT n.id, bm25(nodes_fts) AS rank
                         FROM nodes_fts JOIN nodes n ON nodes_fts.rowid = n.id
                         WHERE nodes_fts MATCH ?
                         ORDER BY rank, n.id LIMIT ?"
                        .into(),
                    vec![Box::new(term), Box::new(fetch_count(opts))],
                ),
            };
            let mut stmt = conn.prepare(&sql).map_err(sql_error_to_query)?;
            let rows = stmt
                .query_map(
                    rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?)),
                )
                .map_err(sql_error_to_query)?;
            let mut fetched: Vec<(NodeId, f64)> = Vec::new();
            for row in rows {
                let (id_i64, rank) = row.map_err(sql_error_to_query)?;
                let id = u64::try_from(id_i64)
                    .map_err(|_| QueryError::Corrupt(format!("invalid node id: {id_i64}")))?;
                fetched.push((NodeId::from_raw(id), rank));
            }
            let (page, next_after, next_after_rank, ranks) = match opts.limit {
                Some(l) => {
                    let has_more = fetched.len() > l;
                    let last = has_more.then(|| fetched[l - 1]);
                    let page_rows: Vec<(NodeId, f64)> = fetched.into_iter().take(l).collect();
                    let page: Vec<NodeId> = page_rows.iter().map(|(id, _)| *id).collect();
                    let ranks: Vec<f64> = page_rows.into_iter().map(|(_, r)| r).collect();
                    (
                        page,
                        last.map(|(id, _)| id),
                        last.map(|(_, rank)| rank),
                        ranks,
                    )
                }
                None => {
                    let ids: Vec<NodeId> = fetched.iter().map(|(id, _)| *id).collect();
                    let ranks: Vec<f64> = fetched.iter().map(|(_, r)| *r).collect();
                    (ids, None, None, ranks)
                }
            };
            Ok(QueryResult {
                node_ids: page,
                total: Some(u64::try_from(total).unwrap_or(0)),
                next_after,
                next_after_rank,
                ranks: Some(ranks),
            })
        }
    }
}

/// Convenience: parse + execute in one call.
pub fn query(corpus: &Corpus, text: &str, opts: &ExecOptions) -> Result<QueryResult> {
    let expr = QueryExpr::parse(text)?;
    execute(corpus, &expr, opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{BatchBuilder, EdgeType};
    use crate::storage::StagingCorpus;
    use std::collections::HashSet;

    fn corpus_with_data() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "aa-query-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let live = dir.with_extension("live");
        (dir, live)
    }

    fn build() -> Corpus {
        let (dir, live) = corpus_with_data();
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        let root = b.add_node("alice");
        let b1 = b.add_node("bob");
        let b2 = b.add_node("carol");
        b.add_edge(root, b1, EdgeType::new("parent"));
        b.add_edge(root, b2, EdgeType::new("parent"));
        b.add_edge(b1, b2, EdgeType::new("friend"));
        b.add_full_page_node("webpage: alice's home");
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");
        // keep the temp dirs alive for the corpus's lifetime (tests clean
        // up at process end)
        let _leak = Box::leak(Box::new((dir, live)));
        corpus
    }

    #[test]
    fn text_query_finds_nodes() {
        let corpus = build();
        let res = query(&corpus, "alice", &ExecOptions::default()).expect("query");
        assert!(!res.is_empty());
        assert!(res.total.unwrap() >= 1);
    }

    #[test]
    fn and_and_or_work() {
        let corpus = build();
        let res = query(&corpus, "alice and bob", &ExecOptions::default()).expect("query");
        assert!(res.is_empty(), "no node is both alice and bob");
        // FTS "alice" also matches the full-page node label, so the union
        // is alice + bob + webpage-alice.
        let res = query(&corpus, "alice or bob", &ExecOptions::default()).expect("query");
        assert_eq!(res.total.unwrap(), 3);
    }

    #[test]
    fn traversal_follows_edges() {
        let corpus = build();
        let res = query(&corpus, "->parent:(alice)", &ExecOptions::default()).expect("query");
        assert_eq!(res.total.unwrap(), 2, "alice has two parent edges");
    }

    #[test]
    fn kind_filter_works() {
        let corpus = build();
        let res = query(&corpus, "kind:full_page", &ExecOptions::default()).expect("query");
        assert_eq!(res.total.unwrap(), 1);
    }

    #[test]
    fn pagination_is_stable() {
        let corpus = build();
        // Bound the match set with regex so pagination counts are exact
        // (FTS would also match the full-page node's label).
        let page1 = query(
            &corpus,
            "re:\"^(alice|bob|carol)$\"",
            &ExecOptions {
                limit: Some(2),
                ..Default::default()
            },
        )
        .expect("q1");
        assert_eq!(page1.node_ids.len(), 2);
        assert!(page1.next_after.is_some());
        let page2 = query(
            &corpus,
            "re:\"^(alice|bob|carol)$\"",
            &ExecOptions {
                limit: Some(2),
                after: page1.next_after,
                ..Default::default()
            },
        )
        .expect("q2");
        assert_eq!(page2.node_ids.len(), 1);
        // no overlap between pages
        let s1: HashSet<_> = page1.node_ids.iter().copied().collect();
        let s2: HashSet<_> = page2.node_ids.iter().copied().collect();
        assert!(s1.is_disjoint(&s2));
    }

    #[test]
    fn regex_matches_label() {
        let corpus = build();
        let res = query(&corpus, "re:\"^alice$\"", &ExecOptions::default()).expect("query");
        assert_eq!(res.total.unwrap(), 1);
    }

    #[test]
    fn not_excludes() {
        let corpus = build();
        let res = query(&corpus, "not kind:full_page", &ExecOptions::default()).expect("query");
        assert_eq!(res.total.unwrap(), 3);
    }

    #[test]
    fn field_queries_match_typed_values() {
        let dir = std::env::temp_dir().join(format!(
            "aa-field-q-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let live = dir.with_extension("live");
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        let c1 = b.add_node("acme master agreement");
        let c2 = b.add_node("acme amendment 1");
        let c3 = b.add_node("globex master agreement");
        b.add_field(c1, "expiry", FieldValue::Date(1_767_225_600)); // 2026-01-01
        b.add_field(c1, "status", FieldValue::Str("active".into()));
        b.add_field(c1, "amount", FieldValue::Float(12_500.0));
        b.add_field(c2, "expiry", FieldValue::Date(1_800_000_000));
        b.add_field(c2, "status", FieldValue::Str("in review".into()));
        b.add_field(c3, "expiry", FieldValue::Date(1_900_000_000));
        b.add_field(c3, "status", FieldValue::Str("active".into()));
        b.add_field(c3, "amount", FieldValue::Float(500.0));
        staging.write_batch(&b.build(), "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");
        let _leak = Box::leak(Box::new((dir, live)));

        // Equality on a string field.
        let res = query(&corpus, "field:status = active", &ExecOptions::default()).expect("q");
        assert_eq!(res.total.unwrap(), 2, "c1 and c3 are active");

        // Date comparison: expiring after mid-2026.
        let res = query(
            &corpus,
            "field:expiry > 2026-06-01",
            &ExecOptions::default(),
        )
        .expect("q");
        assert_eq!(res.total.unwrap(), 2, "c2 and c3 expire later");

        // Numeric comparison.
        let res = query(&corpus, "field:amount >= 10000", &ExecOptions::default()).expect("q");
        assert_eq!(res.total.unwrap(), 1, "only c1 is >= 10000");

        // Not-equal.
        let res = query(&corpus, "field:status != active", &ExecOptions::default()).expect("q");
        assert_eq!(res.total.unwrap(), 1, "only c2");

        // Missing fields never match.
        let res = query(&corpus, "field:owner = alice", &ExecOptions::default()).expect("q");
        assert_eq!(res.total.unwrap(), 0);

        // Composes with boolean operators.
        let res = query(
            &corpus,
            "field:status = active and field:expiry > 2026-06-01",
            &ExecOptions::default(),
        )
        .expect("q");
        assert_eq!(res.total.unwrap(), 1, "only c3 is active and expires later");
    }

    #[test]
    fn text_terms_with_fts_metacharacters_match_literally() {
        // Labels whose tokens FTS5 would otherwise parse as query
        // syntax: `foo-bar` became `foo NOT bar`, a bare `and` was an
        // FTS5 syntax error, and so on. Terms are quoted before
        // matching, so these now match literally — while plain terms
        // and multi-word (implicit AND) queries behave exactly as
        // before.
        let (dir, live) = corpus_with_data();
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        let hyphen = b.add_node("foo-bar");
        let words = b.add_node("salt and pepper");
        let op_word = b.add_node("and");
        let parens = b.add_node("rock (paper)");
        let plain = b.add_node("plain");
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");
        let _leak = Box::leak(Box::new((dir, live)));

        let ids = |text: &str| -> Vec<NodeId> {
            query(&corpus, text, &ExecOptions::default())
                .expect("query")
                .node_ids
        };

        // Hyphenated term is literal, not `foo NOT bar`.
        assert_eq!(ids("foo-bar"), vec![hyphen]);
        // A bare operator word is a literal token match.
        assert_eq!(ids("\"and\""), vec![words, op_word]);
        // Multi-word quoted term keeps implicit-AND semantics.
        assert_eq!(ids("\"salt and pepper\""), vec![words]);
        // Parentheses are literal tokens.
        assert_eq!(ids("\"rock (paper)\""), vec![parens]);
        // Ordinary terms are unchanged.
        assert_eq!(ids("plain"), vec![plain]);
    }

    #[test]
    fn prefix_queries_match_label_prefixes() {
        let (dir, live) = corpus_with_data();
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        let alice = b.add_node("alice");
        let alicia = b.add_node("alicia");
        let hyphen = b.add_node("foo-bar");
        b.add_node("bob");
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");
        let _leak = Box::leak(Box::new((dir, live)));

        let ids = |text: &str| -> Vec<NodeId> {
            query(&corpus, text, &ExecOptions::default())
                .expect("query")
                .node_ids
        };

        assert_eq!(ids("prefix:ali"), vec![alice, alicia]);
        assert_eq!(ids("prefix:alicia"), vec![alicia]);
        assert_eq!(ids("prefix:zz"), vec![]);
        // Empty prefix matches nothing (as `is_impossible` declares).
        assert_eq!(ids("prefix:\"\""), vec![]);
        // Prefix tokens are quoted too, so `-` stays literal.
        assert_eq!(ids("prefix:foo-b"), vec![hyphen]);
    }

    #[test]
    fn field_comparison_sql_parity_with_in_memory_reference() {
        // The SQL range scans in `eval_field_cmp` must return exactly
        // the set the in-memory comparator returns — for every operator
        // and literal kind, against stored values of every kind
        // (including NaN, whose stored form is never equal).
        let (dir, live) = corpus_with_data();
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        let stored: Vec<FieldValue> = vec![
            FieldValue::Int(7),
            FieldValue::Int(-3),
            FieldValue::Float(2.5),
            FieldValue::Date(1_767_225_600),
            FieldValue::Str("medium".into()),
            FieldValue::Str("alpha".into()),
            FieldValue::Bool(true),
            FieldValue::Bool(false),
        ];
        for (i, v) in stored.iter().enumerate() {
            let n = b.add_node(format!("node{i}"));
            b.add_field(n, "score", v.clone());
        }
        // A node carrying a different field only: missing `score` rows
        // must never match, `!=` included.
        let other = b.add_node("other");
        b.add_field(other, "unrelated", FieldValue::Int(1));
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");
        let _leak = Box::leak(Box::new((dir, live)));

        let literals: Vec<FieldValue> = vec![
            FieldValue::Int(7),
            FieldValue::Int(4),
            FieldValue::Float(2.5),
            FieldValue::Date(1_767_225_600),
            FieldValue::Str("medium".into()),
            FieldValue::Str("zeta".into()),
            FieldValue::Bool(true),
            // A NaN literal is incomparable with everything: `!=`
            // matches every stored value, other operators match
            // nothing. (A NaN cannot be *stored* — SQLite nulls it.)
            FieldValue::Float(f64::NAN),
        ];
        for literal in &literals {
            for op in [
                FieldOp::Eq,
                FieldOp::Ne,
                FieldOp::Gt,
                FieldOp::Gte,
                FieldOp::Lt,
                FieldOp::Lte,
            ] {
                // Reference: the original in-memory semantics.
                let rows = corpus.field_values("score").expect("field values");
                let expected: BTreeSet<NodeId> = rows
                    .iter()
                    .filter(|(_, stored)| field_matches(stored, op, literal))
                    .map(|(id, _)| *id)
                    .collect();

                let expr = QueryExpr::FieldCmp {
                    field: "score".into(),
                    op,
                    value: literal.clone(),
                };
                let actual = eval(&corpus, &expr).expect("sql eval");
                assert_eq!(
                    actual, expected,
                    "SQL vs in-memory mismatch for op={op:?} literal={literal:?}"
                );
            }
        }

        // Missing field: no node matches, for every operator.
        for op in [
            FieldOp::Eq,
            FieldOp::Ne,
            FieldOp::Gt,
            FieldOp::Gte,
            FieldOp::Lt,
            FieldOp::Lte,
        ] {
            let expr = QueryExpr::FieldCmp {
                field: "missing".into(),
                op,
                value: FieldValue::Int(0),
            };
            assert!(eval(&corpus, &expr).expect("sql eval").is_empty());
        }
    }

    /// The historical in-memory pagination algorithm, kept verbatim as
    /// the reference the SQL keyset paths must match bit-for-bit.
    fn reference_page(corpus: &Corpus, expr: &QueryExpr, opts: &ExecOptions) -> QueryResult {
        let matched = eval(corpus, expr).expect("eval");
        let mut ids: Vec<NodeId> = matched.into_iter().collect();
        ids.sort_by_key(|n| n.to_raw());
        let total = ids.len() as u64;
        let start = match opts.after {
            Some(cursor) => ids.partition_point(|n| n.to_raw() <= cursor.to_raw()),
            None => 0,
        };
        let (page, next_after) = match opts.limit {
            Some(limit) => {
                let end = (start + limit).min(ids.len());
                let next = if limit > 0 && start + limit < ids.len() {
                    Some(ids[start + limit - 1])
                } else {
                    None
                };
                (ids[start..end].to_vec(), next)
            }
            None => (ids[start..].to_vec(), None),
        };
        QueryResult {
            node_ids: page,
            total: Some(total),
            next_after,
            next_after_rank: None,
            ranks: None,
        }
    }

    #[test]
    fn sql_keyset_pagination_matches_reference_exactly() {
        // Every atom path pages in SQL now; the results must be
        // indistinguishable from the historical full-eval + slice
        // algorithm — same page, same total, same cursor.
        let (dir, live) = corpus_with_data();
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        for i in 0..25 {
            let n = b.add_node(format!(
                "node{i:02} {}",
                if i % 2 == 0 { "even" } else { "odd" }
            ));
            b.add_field(n, "score", FieldValue::Int(i));
        }
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");
        let _leak = Box::leak(Box::new((dir, live)));

        let atoms: Vec<QueryExpr> = vec![
            QueryExpr::All,
            QueryExpr::Text("even".into()),
            QueryExpr::Prefix("node".into()),
            QueryExpr::Kind("text".into()),
            QueryExpr::FieldEquals {
                field: "x".into(),
                value: "node07 odd".into(),
            },
            QueryExpr::FieldCmp {
                field: "score".into(),
                op: FieldOp::Gte,
                value: FieldValue::Int(10),
            },
            QueryExpr::FieldCmp {
                field: "score".into(),
                op: FieldOp::Ne,
                value: FieldValue::Str("active".into()),
            },
            // NaN literal: `!=` matches every stored value — the paged
            // path has its own SQL for it.
            QueryExpr::FieldCmp {
                field: "score".into(),
                op: FieldOp::Ne,
                value: FieldValue::Float(f64::NAN),
            },
        ];
        for expr in &atoms {
            for (limit, after) in [
                (None, None),
                (Some(0), None),
                (Some(3), None),
                (Some(3), Some(NodeId::from_raw(5))),
                (Some(100), Some(NodeId::from_raw(3))),
            ] {
                let opts = ExecOptions {
                    limit,
                    after,
                    ..Default::default()
                };
                let got = execute(&corpus, expr, &opts).expect("execute");
                let want = reference_page(&corpus, expr, &opts);
                assert_eq!(
                    got.node_ids, want.node_ids,
                    "page mismatch for {expr:?} limit={limit:?} after={after:?}"
                );
                assert_eq!(
                    got.total, want.total,
                    "total mismatch for {expr:?} limit={limit:?} after={after:?}"
                );
                assert_eq!(
                    got.next_after, want.next_after,
                    "cursor mismatch for {expr:?} limit={limit:?} after={after:?}"
                );
                assert_eq!(got.ranks, None);
                assert_eq!(got.next_after_rank, None);
            }
        }
    }

    #[test]
    fn ranked_text_queries_order_by_bm25_and_page_by_rank_cursor() {
        let (dir, live) = corpus_with_data();
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        b.add_node("renewable");
        b.add_node("renewable renewable");
        b.add_node("renewable renewable renewable");
        b.add_node("renewable renewable renewable renewable");
        b.add_node("unrelated");
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");
        let _leak = Box::leak(Box::new((dir, live)));

        // Full ranked result: more occurrences rank first (lower bm25).
        let res = query(
            &corpus,
            "renewable",
            &ExecOptions {
                order: ResultOrder::ByRank,
                ..Default::default()
            },
        )
        .expect("ranked");
        assert_eq!(res.total, Some(4));
        let ranks = res.ranks.expect("ranks populated");
        assert_eq!(ranks.len(), 4);
        assert!(
            ranks.windows(2).all(|w| w[0] <= w[1]),
            "ranks ascending (better first)"
        );
        assert_eq!(
            res.node_ids,
            vec![
                NodeId::from_raw(4),
                NodeId::from_raw(3),
                NodeId::from_raw(2),
                NodeId::from_raw(1),
            ],
            "4 occurrences, then 3, 2, 1"
        );

        // Keyset paging by (rank, id): walk all 4 one at a time.
        let mut seen = Vec::new();
        let mut after: Option<NodeId> = None;
        let mut after_rank: Option<f64> = None;
        for _ in 0..4 {
            let page = query(
                &corpus,
                "renewable",
                &ExecOptions {
                    limit: Some(1),
                    after,
                    order: ResultOrder::ByRank,
                    after_rank,
                },
            )
            .expect("page");
            assert_eq!(page.node_ids.len(), 1);
            assert_eq!(page.total, Some(4), "total is exact on every page");
            seen.push(page.node_ids[0]);
            after = page.next_after;
            after_rank = page.next_after_rank;
        }
        assert_eq!(seen, res.node_ids, "paged walk matches the full result");
        // The final page carries no cursor: the walk is exhausted (and
        // passing `None` back would start from the first page again).
        assert!(after.is_none());
        assert!(after_rank.is_none());

        // Default order stays id-ordered.
        let by_id = query(&corpus, "renewable", &ExecOptions::default()).expect("by id");
        assert_eq!(by_id.ranks, None);
        assert_eq!(
            by_id.node_ids,
            vec![
                NodeId::from_raw(1),
                NodeId::from_raw(2),
                NodeId::from_raw(3),
                NodeId::from_raw(4),
            ]
        );
    }

    #[test]
    fn traversal_direction_depth_and_union_semantics() {
        let (dir, live) = corpus_with_data();
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        let n1 = b.add_node("n1");
        let n2 = b.add_node("n2");
        let n3 = b.add_node("n3");
        let n4 = b.add_node("n4");
        let n5 = b.add_node("n5");
        let n6 = b.add_node("n6");
        b.add_edge(n1, n2, EdgeType::new("a"));
        b.add_edge(n2, n3, EdgeType::new("b"));
        b.add_edge(n3, n4, EdgeType::new("a"));
        b.add_edge(n4, n5, EdgeType::new("c"));
        b.add_edge(n6, n1, EdgeType::new("b"));
        b.add_edge(n5, n2, EdgeType::new("b"));
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");
        let _leak = Box::leak(Box::new((dir, live)));

        let q = |text: &str| -> Vec<NodeId> {
            query(&corpus, text, &ExecOptions::default())
                .expect("query")
                .node_ids
        };

        // Outgoing one hop (historic semantics preserved).
        assert_eq!(q("->a:(prefix:n1)"), vec![n2]);
        // Outgoing depth 2 along a.
        assert_eq!(q("->a:2:(prefix:n1)"), vec![n2]);
        // Union of a|b, depth 2, from n1: a->n2, then b->n3.
        assert_eq!(q("->(a|b):2:(prefix:n1)"), vec![n2, n3]);
        // Incoming one hop along b: into n1 comes n6.
        assert_eq!(q("<-b:(prefix:n1)"), vec![n6]);
        // Incoming depth 2 along b from n3: n2 (via 2->3), then n5
        // (via the 5->2 b edge).
        assert_eq!(q("<-b:2:(prefix:n3)"), vec![n2, n5]);
        // The start set itself is never part of the result.
        assert!(!q("->a:(prefix:n1)").contains(&n1));
    }

    #[test]
    fn limit_zero_never_panics() {
        let corpus = build();
        for text in [
            "*",
            "alice",
            "prefix:a",
            "kind:text",
            "field:status = active",
        ] {
            let res = query(
                &corpus,
                text,
                &ExecOptions {
                    limit: Some(0),
                    ..Default::default()
                },
            )
            .expect("limit 0");
            assert!(res.node_ids.is_empty());
            assert!(res.next_after.is_none());
        }
    }
}
