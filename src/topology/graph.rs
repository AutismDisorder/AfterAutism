// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! [`VisibleGraph`] — the typed-edge adjacency of the visible subgraph.

use crate::adapter::{Edge, EdgeType};
use crate::core::NodeId;
use std::collections::{HashMap, HashSet};

/// A directed typed edge between two visible nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedEdge {
    /// The source node (edge tail).
    pub from: NodeId,
    /// The destination node (edge head).
    pub to: NodeId,
    /// The edge's type (opaque, adapter-assigned).
    pub edge_type: EdgeType,
}

/// The visible subgraph — a typed-edge adjacency over a subset of
/// nodes (the visible window).
///
/// Invariants:
/// - `nodes` contains exactly the ids in the visible window.
/// - `edges` only reference ids present in `nodes`.
/// - `outgoing` and `incoming` are adjacency indexes for O(1) neighbor
///   lookup during filter evaluation.
#[derive(Debug, Clone, Default)]
pub struct VisibleGraph {
    /// The set of visible node IDs. Used for membership checks.
    nodes: HashSet<NodeId>,
    /// All edges in the visible subgraph.
    edges: Vec<TypedEdge>,
    /// Adjacency index: `node_id` -> indices into `edges` for outgoing edges.
    outgoing: HashMap<NodeId, Vec<usize>>,
    /// Adjacency index: `node_id` -> indices into `edges` for incoming edges.
    incoming: HashMap<NodeId, Vec<usize>>,
}

impl VisibleGraph {
    /// Create an empty visible graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a visible graph from the given visible node IDs and edges.
    /// Edges that reference nodes not in `visible_nodes` are silently
    /// dropped (they are outside the visible window). This is by design:
    /// the topology engine only reasons over the visible subgraph.
    #[must_use]
    pub fn from_nodes_and_edges(
        visible_nodes: impl IntoIterator<Item = NodeId>,
        edges: impl IntoIterator<Item = Edge>,
    ) -> Self {
        let nodes: HashSet<_> = visible_nodes.into_iter().collect();
        let mut graph = Self {
            nodes,
            edges: Vec::new(),
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
        };

        // Filter edges to only those where both endpoints are visible.
        // This maintains the O(visible) cost bound — we only process
        // edges fully inside the visible window.
        let filtered_edges: Vec<_> = edges
            .into_iter()
            .filter(|e| graph.nodes.contains(&e.from) && graph.nodes.contains(&e.to))
            .map(|e| TypedEdge {
                from: e.from,
                to: e.to,
                edge_type: e.edge_type,
            })
            .collect();

        for (idx, edge) in filtered_edges.iter().enumerate() {
            graph.edges.push(edge.clone());
            graph.outgoing.entry(edge.from).or_default().push(idx);
            graph.incoming.entry(edge.to).or_default().push(idx);
        }

        graph
    }

    /// Returns the set of visible node IDs.
    #[must_use]
    pub fn visible_nodes(&self) -> &HashSet<NodeId> {
        &self.nodes
    }

    /// Returns all edges in the visible subgraph.
    #[must_use]
    pub fn edges(&self) -> &[TypedEdge] {
        &self.edges
    }

    /// Returns the outgoing edge indices for a node.
    #[must_use]
    pub fn outgoing_edges(&self, node: &NodeId) -> &[usize] {
        self.outgoing.get(node).map_or(&[], Vec::as_slice)
    }

    /// Returns the incoming edge indices for a node.
    #[must_use]
    pub fn incoming_edges(&self, node: &NodeId) -> &[usize] {
        self.incoming.get(node).map_or(&[], Vec::as_slice)
    }

    /// Returns the number of visible nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns the number of edges in the visible subgraph.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Check if a node is in the visible graph.
    #[must_use]
    pub fn contains_node(&self, node: &NodeId) -> bool {
        self.nodes.contains(node)
    }

    /// Get a typed edge by its index.
    #[must_use]
    pub fn edge(&self, idx: usize) -> Option<&TypedEdge> {
        self.edges.get(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{Edge, EdgeType};
    use crate::core::NodeId;

    #[test]
    fn graph_only_includes_edges_between_visible_nodes() {
        let n1 = NodeId::from_raw(1);
        let n2 = NodeId::from_raw(2);
        let n3 = NodeId::from_raw(3); // not visible

        let visible = [n1, n2];
        let edges = [
            Edge {
                from: n1,
                to: n2,
                edge_type: EdgeType::new("link"),
            },
            Edge {
                from: n2,
                to: n3,
                edge_type: EdgeType::new("link"),
            },
            Edge {
                from: n3,
                to: n1,
                edge_type: EdgeType::new("link"),
            },
        ];

        let graph = VisibleGraph::from_nodes_and_edges(visible, edges);

        assert_eq!(graph.node_count(), 2);
        // Only the n1->n2 edge has both endpoints visible
        assert_eq!(graph.edge_count(), 1);
        assert!(graph.contains_node(&n1));
        assert!(graph.contains_node(&n2));
        assert!(!graph.contains_node(&n3));
    }

    #[test]
    fn adjacency_indices_allow_o1_neighbor_lookup() {
        let n1 = NodeId::from_raw(1);
        let n2 = NodeId::from_raw(2);
        let n3 = NodeId::from_raw(3);

        let visible = [n1, n2, n3];
        let edges = [
            Edge {
                from: n1,
                to: n2,
                edge_type: EdgeType::new("link"),
            },
            Edge {
                from: n2,
                to: n3,
                edge_type: EdgeType::new("link"),
            },
            Edge {
                from: n1,
                to: n3,
                edge_type: EdgeType::new("link"),
            },
        ];

        let graph = VisibleGraph::from_nodes_and_edges(visible, edges);

        assert_eq!(graph.outgoing_edges(&n1).len(), 2);
        assert_eq!(graph.incoming_edges(&n2).len(), 1);
        assert_eq!(graph.incoming_edges(&n3).len(), 2);
    }

    #[test]
    fn empty_graph_has_zero_nodes_and_edges() {
        let graph = VisibleGraph::new();
        assert_eq!(graph.node_count(), 0);
        assert_eq!(graph.edge_count(), 0);
    }
}
