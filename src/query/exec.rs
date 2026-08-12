// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Query executor — runs a typed IR against a corpus.

use crate::adapter::FieldValue;
use crate::core::NodeId;
use crate::query::error::{QueryError, Result};
use crate::query::ir::{FieldOp, QueryExpr, QueryResult};
use crate::storage::Corpus;
use rayon::prelude::*;
use std::collections::BTreeSet;

/// Execution options.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Page size; `None` = no pagination.
    pub limit: Option<usize>,
    /// Keyset cursor from a previous page.
    pub after: Option<NodeId>,
}

/// Execute a query expression against a corpus.
pub fn execute(corpus: &Corpus, expr: &QueryExpr, opts: &ExecOptions) -> Result<QueryResult> {
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

    let next_after = match opts.limit {
        Some(limit) => {
            if start + limit < ids.len() {
                Some(ids[start + limit - 1])
            } else {
                None
            }
        }
        None => None,
    };

    let page: Vec<NodeId> = match opts.limit {
        Some(limit) => ids[start..(start + limit).min(ids.len())].to_vec(),
        None => ids,
    };

    Ok(QueryResult {
        node_ids: page,
        total: Some(total),
        next_after,
    })
}

/// Evaluate the expression to the full matching node-id set (no paging).
pub fn eval(corpus: &Corpus, expr: &QueryExpr) -> Result<BTreeSet<NodeId>> {
    match expr {
        QueryExpr::All => {
            // All nodes: walk the id space via node_count + get_nodes is
            // not possible; use a bounded id scan is wrong. Instead the
            // corpus exposes all ids via a dedicated method.
            let ids = corpus
                .all_node_ids()
                .map_err(|e| QueryError::Execution(e.to_string()))?;
            Ok(ids.into_iter().collect())
        }
        QueryExpr::Text(t) => {
            let nodes = corpus
                .search_nodes(t, 1_000_000)
                .map_err(|e| QueryError::Execution(e.to_string()))?;
            Ok(nodes.into_iter().map(|n| n.id).collect())
        }
        QueryExpr::Prefix(p) => {
            let nodes = corpus
                .search_nodes(&format!("{p}*"), 1_000_000)
                .map_err(|e| QueryError::Execution(e.to_string()))?;
            Ok(nodes.into_iter().map(|n| n.id).collect())
        }
        QueryExpr::Kind(k) => {
            let nodes = corpus
                .nodes_by_kind(k)
                .map_err(|e| QueryError::Execution(e.to_string()))?;
            Ok(nodes.into_iter().map(|n| n.id).collect())
        }
        QueryExpr::FieldEquals { field, value } => {
            // `field:value` on the label: model as exact-label search.
            let nodes = corpus
                .nodes_by_label(field, value)
                .map_err(|e| QueryError::Execution(e.to_string()))?;
            Ok(nodes.into_iter().map(|n| n.id).collect())
        }
        QueryExpr::FieldCmp { field, op, value } => {
            // Typed-field comparison: nodes carrying `field`
            // whose value compares to `value` with `op`. Missing fields
            // never match. The per-field scan is pure CPU over
            // in-memory values, so it runs in parallel (rayon), like the
            // regex scan; the result set is sorted afterwards (BTreeSet)
            // so output stays deterministic.
            let rows = corpus
                .field_values(field)
                .map_err(|e| QueryError::Execution(e.to_string()))?;
            let matched: Vec<NodeId> = rows
                .par_iter()
                .filter(|(_, stored)| field_matches(stored, *op, value))
                .map(|(id, _)| *id)
                .collect();
            Ok(matched.into_iter().collect())
        }
        QueryExpr::Regex(pattern) => {
            let re =
                regex::Regex::new(pattern).map_err(|e| QueryError::InvalidRegex(e.to_string()))?;
            // Regex scanning is pure CPU over in-memory labels; run it in
            // parallel across the corpus. The id set is sorted afterwards
            // (BTreeSet), so result determinism is unchanged.
            let nodes = corpus.all_nodes()?;
            let matched: Vec<NodeId> = nodes
                .par_iter()
                .filter(|node| re.is_match(&node.label))
                .map(|node| node.id)
                .collect();
            Ok(matched.into_iter().collect())
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
        QueryExpr::Traverse { inner, edge_type } => {
            let inner_ids = eval(corpus, inner)?;
            // Batch the fan-out: one `IN (...)` query for all source
            // nodes instead of preparing a statement per node.
            let source_ids: Vec<NodeId> = inner_ids.into_iter().collect();
            let grouped = corpus
                .get_outgoing_edges_for(&source_ids)
                .map_err(|e| QueryError::Execution(e.to_string()))?;
            let mut out = BTreeSet::new();
            for (_, edges) in grouped {
                for edge in edges {
                    if edge.edge_type.as_str() == edge_type {
                        out.insert(edge.to);
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Deterministic cross-kind field comparison. `None` = incomparable.
/// Rules: same-kind values compare naturally; integers, floats, and
/// dates compare numerically (via `f64` when kinds differ); strings
/// compare lexicographically with other strings; booleans compare
/// `false < true`; every other pairing is incomparable (ordering
/// predicates never match, equality is false unless kinds agree).
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

/// Evaluate one typed-field predicate against a stored value.
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
                after: None,
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
}
