// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Filter predicates → per-node emphasis masks. Filtering is a visual
//! transform, not a set reduction: the visible node count never changes.

use crate::adapter::EdgeType;
use crate::core::NodeId;
use crate::topology::graph::VisibleGraph;
use rayon::prelude::*;
use std::collections::HashMap;

/// Per-node visual emphasis parameters produced by the filter engine.
///
/// - `scale_factor`: multiplies the node's size (1.0 = no change;
///   1.05 = +5% for matched nodes)
/// - `luma_factor`: multiplies the node's luminance (1.0 = no change;
///   0.07 = near-black for unmatched nodes)
///
/// The default (no filter) is `(1.0, 1.0)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmphasisMask {
    /// Size multiplier for the node's base geometry.
    pub scale_factor: f32,
    /// Luminance multiplier for the node's rendered color.
    pub luma_factor: f32,
}

impl Default for EmphasisMask {
    fn default() -> Self {
        Self {
            scale_factor: 1.0,
            luma_factor: 1.0,
        }
    }
}

impl EmphasisMask {
    /// Emphasis for a **matched** node: +5% scale, full luminance.
    #[must_use]
    pub fn matched() -> Self {
        Self {
            scale_factor: 1.05,
            luma_factor: 1.0,
        }
    }

    /// Emphasis for an **unmatched** node: base scale, near-black luma.
    #[must_use]
    pub fn unmatched() -> Self {
        Self {
            scale_factor: 1.0,
            luma_factor: 0.07,
        }
    }
}

/// A filter predicate over the typed-edge visible graph.
/// Predicates are evaluated per-node against the `VisibleGraph`. The
/// evaluation is pure and has no side effects.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterPredicate {
    /// True if the node has at least one edge of the given type
    /// (incoming OR outgoing).
    EdgeType(EdgeType),

    /// True if the node's label contains the query string (case-insensitive
    /// substring match).
    TextSearch(String),

    /// Boolean AND of two predicates.
    And(Box<FilterPredicate>, Box<FilterPredicate>),

    /// Boolean OR of two predicates.
    Or(Box<FilterPredicate>, Box<FilterPredicate>),

    /// Boolean NOT of a predicate.
    Not(Box<FilterPredicate>),
}

impl FilterPredicate {
    /// Evaluate this predicate against a single node in the visible graph.
    /// Returns `true` if the node matches the predicate.
    pub fn eval<S: ::std::hash::BuildHasher>(
        &self,
        graph: &VisibleGraph,
        node: &NodeId,
        node_labels: &HashMap<NodeId, String, S>,
    ) -> bool {
        match self {
            FilterPredicate::EdgeType(edge_type) => {
                // Check outgoing edges
                for &idx in graph.outgoing_edges(node) {
                    if let Some(edge) = graph.edge(idx) {
                        if edge.edge_type == *edge_type {
                            return true;
                        }
                    }
                }
                // Check incoming edges
                for &idx in graph.incoming_edges(node) {
                    if let Some(edge) = graph.edge(idx) {
                        if edge.edge_type == *edge_type {
                            return true;
                        }
                    }
                }
                false
            }
            FilterPredicate::TextSearch(query) => {
                let label = node_labels.get(node).map_or("", |s| s.as_str());
                // allocation-free case-insensitive search. The old
                // `to_lowercase()` created two Strings per node per filter
                // pass — 2M allocations at 1M nodes, breaking the scale
                // contract's filter budget. See `contains_ignore_case`.
                contains_ignore_case(label, query)
            }
            FilterPredicate::And(a, b) => {
                a.eval(graph, node, node_labels) && b.eval(graph, node, node_labels)
            }
            FilterPredicate::Or(a, b) => {
                a.eval(graph, node, node_labels) || b.eval(graph, node, node_labels)
            }
            FilterPredicate::Not(inner) => !inner.eval(graph, node, node_labels),
        }
    }
}

/// Evaluate a filter predicate over the entire visible graph, producing
/// an `EmphasisMask` per visible node.
/// This is the core filter application function. Cost is O(visible) because
/// it iterates only over `graph.visible_nodes()` and evaluates the predicate
/// per-node. Predicate evaluation is pure, so the per-node pass runs in
/// parallel (rayon); the result map is the same set of masks as before.
/// Returns a map from `NodeId` to its computed `EmphasisMask`.
/// `S` (the label map's hasher) must be `Sync` so the parallel pass can
/// share the label map across worker threads; every standard hasher
/// (`RandomState` etc.) satisfies this.
pub fn apply_filter<S: ::std::hash::BuildHasher + Sync>(
    graph: &VisibleGraph,
    predicate: Option<&FilterPredicate>,
    node_labels: &HashMap<NodeId, String, S>,
) -> HashMap<NodeId, EmphasisMask> {
    graph
        .visible_nodes()
        .par_iter()
        .map(|node| {
            let mask = match predicate {
                Some(p) if p.eval(graph, node, node_labels) => EmphasisMask::matched(),
                Some(_) => EmphasisMask::unmatched(),
                None => EmphasisMask::default(),
            };
            (*node, mask)
        })
        .collect()
}

/// Case-insensitive substring search without per-call allocation.
/// Fast path: if both `haystack` and `needle` are ASCII, scans byte
/// windows with `eq_ignore_ascii_case`. Otherwise falls back to a
/// lowercased comparison (allocates, but only for non-ASCII input).
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle.is_ascii() {
        let hay = haystack.as_bytes();
        let needle_bytes = needle.as_bytes();
        if needle_bytes.len() > hay.len() {
            return false;
        }
        hay.windows(needle_bytes.len())
            .any(|w| w.eq_ignore_ascii_case(needle_bytes))
    } else {
        haystack.to_lowercase().contains(&needle.to_lowercase())
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::implicit_hasher)]
mod tests {
    use super::*;
    use crate::topology::graph::VisibleGraph;

    #[test]
    fn contains_ignore_case_matches_ascii_folding() {
        assert!(contains_ignore_case("Hello World", "hello"));
        assert!(contains_ignore_case("Hello World", "WORLD"));
        assert!(!contains_ignore_case("Hello World", "planet"));
        assert!(contains_ignore_case("", ""));
        assert!(contains_ignore_case("abc", ""));
        assert!(
            !contains_ignore_case("ab", "abc"),
            "needle longer than haystack"
        );
    }

    #[test]
    fn contains_ignore_case_handles_non_ascii() {
        // Non-ASCII falls back to the lowercased path; must still match.
        assert!(contains_ignore_case("Café au lait", "CAFÉ"));
        assert!(!contains_ignore_case("Café", "thé"));
    }
    use crate::adapter::{Edge, EdgeType};
    use crate::core::NodeId;
    use std::collections::HashMap;

    fn make_test_graph() -> (VisibleGraph, HashMap<NodeId, String>) {
        let n1 = NodeId::from_raw(1);
        let n2 = NodeId::from_raw(2);
        let n3 = NodeId::from_raw(3);
        let n4 = NodeId::from_raw(4);

        let visible = [n1, n2, n3, n4];
        let edges = [
            Edge {
                from: n1,
                to: n2,
                edge_type: EdgeType::new("hyperlink"),
            },
            Edge {
                from: n2,
                to: n3,
                edge_type: EdgeType::new("citation"),
            },
            Edge {
                from: n3,
                to: n4,
                edge_type: EdgeType::new("hyperlink"),
            },
            Edge {
                from: n1,
                to: n3,
                edge_type: EdgeType::new("cluster"),
            },
        ];

        let graph = VisibleGraph::from_nodes_and_edges(visible, edges);

        let mut labels = HashMap::new();
        labels.insert(n1, "Google Search".to_string());
        labels.insert(n2, "Wikipedia Page".to_string());
        labels.insert(n3, "GitHub Repository".to_string());
        labels.insert(n4, "Stack Overflow Answer".to_string());

        (graph, labels)
    }

    #[test]
    fn emphasis_mask_default_is_identity() {
        let mask = EmphasisMask::default();
        assert_eq!(mask.scale_factor, 1.0);
        assert_eq!(mask.luma_factor, 1.0);
    }

    #[test]
    fn emphasis_mask_matched_is_plus5_scale_full_luma() {
        let mask = EmphasisMask::matched();
        assert_eq!(mask.scale_factor, 1.05);
        assert_eq!(mask.luma_factor, 1.0);
    }

    #[test]
    fn emphasis_mask_unmatched_is_base_scale_near_black_luma() {
        let mask = EmphasisMask::unmatched();
        assert_eq!(mask.scale_factor, 1.0);
        assert_eq!(mask.luma_factor, 0.07);
    }

    #[test]
    fn apply_filter_with_no_predicate_returns_default_for_all_nodes() {
        let (graph, labels) = make_test_graph();
        let masks = apply_filter(&graph, None, &labels);

        assert_eq!(masks.len(), 4);
        for mask in masks.values() {
            assert_eq!(mask.scale_factor, 1.0);
            assert_eq!(mask.luma_factor, 1.0);
        }
    }

    #[test]
    fn apply_filter_edge_type_matches_nodes_with_that_edge() {
        let (graph, labels) = make_test_graph();
        let pred = FilterPredicate::EdgeType(EdgeType::new("hyperlink"));
        let masks = apply_filter(&graph, Some(&pred), &labels);

        // n1 has outgoing hyperlink to n2
        // n3 has outgoing hyperlink to n4
        // n2 has incoming hyperlink from n1
        // n4 has incoming hyperlink from n3
        // All 4 nodes have at least one hyperlink edge (in or out)
        for node in graph.visible_nodes() {
            let mask = masks.get(node).expect("every visible node has a mask");
            assert_eq!(mask.scale_factor, 1.05, "node {node} should be matched");
            assert_eq!(mask.luma_factor, 1.0);
        }
    }

    #[test]
    fn apply_filter_edge_type_excludes_nodes_without_that_edge() {
        let (graph, labels) = make_test_graph();
        let pred = FilterPredicate::EdgeType(EdgeType::new("citation"));
        let masks = apply_filter(&graph, Some(&pred), &labels);

        // Only n2 (outgoing citation) and n3 (incoming citation) match
        let n2 = NodeId::from_raw(2);
        let n3 = NodeId::from_raw(3);

        assert_eq!(masks[&n2], EmphasisMask::matched());
        assert_eq!(masks[&n3], EmphasisMask::matched());

        // n1 and n4 should be unmatched
        let n1 = NodeId::from_raw(1);
        let n4 = NodeId::from_raw(4);
        assert_eq!(masks[&n1], EmphasisMask::unmatched());
        assert_eq!(masks[&n4], EmphasisMask::unmatched());
    }

    #[test]
    fn apply_filter_text_search_matches_substring_case_insensitive() {
        let (graph, labels) = make_test_graph();
        let pred = FilterPredicate::TextSearch("wiki".to_string());
        let masks = apply_filter(&graph, Some(&pred), &labels);

        let n2 = NodeId::from_raw(2); // "Wikipedia Page"
        assert_eq!(masks[&n2], EmphasisMask::matched());

        // Others should not match
        let n1 = NodeId::from_raw(1);
        let n3 = NodeId::from_raw(3);
        let n4 = NodeId::from_raw(4);
        assert_eq!(masks[&n1], EmphasisMask::unmatched());
        assert_eq!(masks[&n3], EmphasisMask::unmatched());
        assert_eq!(masks[&n4], EmphasisMask::unmatched());
    }

    #[test]
    fn apply_filter_and_combines_predicates() {
        let (graph, labels) = make_test_graph();
        let pred = FilterPredicate::And(
            Box::new(FilterPredicate::EdgeType(EdgeType::new("hyperlink"))),
            Box::new(FilterPredicate::TextSearch("google".to_string())),
        );
        let masks = apply_filter(&graph, Some(&pred), &labels);

        // Only n1 has both hyperlink edge AND "google" in label
        let n1 = NodeId::from_raw(1);
        assert_eq!(masks[&n1], EmphasisMask::matched());

        let n2 = NodeId::from_raw(2);
        let n3 = NodeId::from_raw(3);
        let n4 = NodeId::from_raw(4);
        assert_eq!(masks[&n2], EmphasisMask::unmatched());
        assert_eq!(masks[&n3], EmphasisMask::unmatched());
        assert_eq!(masks[&n4], EmphasisMask::unmatched());
    }

    #[test]
    fn apply_filter_or_combines_predicates() {
        let (graph, labels) = make_test_graph();
        let pred = FilterPredicate::Or(
            Box::new(FilterPredicate::EdgeType(EdgeType::new("citation"))),
            Box::new(FilterPredicate::TextSearch("overflow".to_string())),
        );
        let masks = apply_filter(&graph, Some(&pred), &labels);

        // n2 or n3 (citation) OR n4 (stackoverflow)
        let n2 = NodeId::from_raw(2);
        let n3 = NodeId::from_raw(3);
        let n4 = NodeId::from_raw(4);
        assert_eq!(masks[&n2], EmphasisMask::matched());
        assert_eq!(masks[&n3], EmphasisMask::matched());
        assert_eq!(masks[&n4], EmphasisMask::matched());

        // n1 matches neither
        let n1 = NodeId::from_raw(1);
        assert_eq!(masks[&n1], EmphasisMask::unmatched());
    }

    #[test]
    fn apply_filter_not_inverts_predicate() {
        let (graph, labels) = make_test_graph();
        let pred = FilterPredicate::Not(Box::new(FilterPredicate::EdgeType(EdgeType::new(
            "hyperlink",
        ))));
        let masks = apply_filter(&graph, Some(&pred), &labels);

        // All nodes had hyperlink edges in the original test, so NOT should
        // make all unmatched. But wait - let me check: in make_test_graph,
        // all 4 nodes have at least one hyperlink edge.
        for node in graph.visible_nodes() {
            let mask = masks.get(node).expect("every visible node has a mask");
            assert_eq!(*mask, EmphasisMask::unmatched());
        }
    }

    #[test]
    fn visible_node_count_unchanged_after_filter() {
        // : filter is a visual transform, not a set reduction.
        // The number of visible nodes before and after filter must be identical.
        let (graph, labels) = make_test_graph();
        let initial_count = graph.node_count();

        let pred = FilterPredicate::TextSearch("nonexistent".to_string());
        let masks = apply_filter(&graph, Some(&pred), &labels);

        // All nodes should have a mask (even if unmatched)
        assert_eq!(masks.len(), initial_count);
    }

    #[test]
    fn filter_cost_is_o_visible_not_o_corpus() {
        // This test documents the architectural / :
        // apply_filter iterates only over visible nodes.
        let (graph, labels) = make_test_graph();

        // The implementation only calls graph.visible_nodes() which returns
        // a reference to the HashSet of visible nodes. It does NOT iterate
        // over any corpus-scale data structure.
        let pred = FilterPredicate::TextSearch("test".to_string());
        let masks = apply_filter(&graph, Some(&pred), &labels);

        assert_eq!(masks.len(), graph.visible_nodes().len());
        // The test passes by construction — if apply_filter ever started
        // taking a corpus-scale parameter, this test would need to change.
    }
}
