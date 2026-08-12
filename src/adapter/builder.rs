// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! [`BatchBuilder`] — ergonomic batch construction with automatic,
//! collision-free node ids.

use crate::adapter::fields::{FieldValue, NodeField};
use crate::adapter::record::{Edge, EdgeType, Node, NodeKind};
use crate::adapter::trait_def::IngestBatch;
use crate::core::NodeId;

/// Builds an [`IngestBatch`] with automatically-allocated node ids.
/// Node ids are assigned sequentially starting at `start_id` (default 1)
/// and are unique within the finished batch by construction. Adapters
/// that must produce stable, content-derived ids (e.g. for idempotent
/// refresh across runs) should allocate ids themselves instead of using
/// this builder.
#[derive(Debug, Clone)]
pub struct BatchBuilder {
    batch: IngestBatch,
    next_id: u64,
}

impl Default for BatchBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BatchBuilder {
    /// Create a builder whose first node gets `NodeId::from_raw(1)`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            batch: IngestBatch::new(),
            next_id: 1,
        }
    }

    /// Create a builder whose first node gets `NodeId::from_raw(start_id)`.
    /// Use a nonzero `start_id` when building batches for a corpus that
    /// already contains nodes, so the fresh ids do not collide.
    #[must_use]
    pub fn with_start_id(start_id: u64) -> Self {
        Self {
            batch: IngestBatch::new(),
            next_id: start_id,
        }
    }

    /// Allocate the next node id without inserting a node. Useful when a
    /// node id must be known before its node is pushed (e.g. edges that
    /// reference nodes added later).
    #[must_use]
    pub fn next_id(&mut self) -> NodeId {
        let id = NodeId::from_raw(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Push a text node with the given label, returning its id.
    pub fn add_node(&mut self, label: impl Into<String>) -> NodeId {
        let id = self.next_id();
        self.batch.push_node(Node {
            id,
            label: label.into(),
            kind: NodeKind::Text,
        });
        id
    }

    /// Push a full-page node with the given label, returning its id.
    /// adapters whose [`crate::adapter::Adapter::supports_full_page`] returns
    /// `true` use this so the corpus can mark the payload kind; text-only
    /// adapters keep using [`Self::add_node`].
    pub fn add_full_page_node(&mut self, label: impl Into<String>) -> NodeId {
        let id = self.next_id();
        self.batch.push_node(Node {
            id,
            label: label.into(),
            kind: NodeKind::FullPage,
        });
        id
    }

    /// Push a typed edge between two nodes.
    pub fn add_edge(&mut self, from: NodeId, to: NodeId, edge_type: EdgeType) {
        self.batch.push_edge(Edge {
            from,
            to,
            edge_type,
        });
    }

    /// Attach a typed field to a previously added node.
    /// Fields carry form-declared semantics: the storage layer persists
    /// them and the query language compares them (`field:name op value`).
    /// The field is included in the finished batch.
    pub fn add_field(
        &mut self,
        node: NodeId,
        name: impl Into<String>,
        value: FieldValue,
    ) -> &mut Self {
        self.batch.push_field(NodeField {
            node_id: node,
            name: name.into(),
            value,
        });
        self
    }

    /// Finish building the batch.
    #[must_use]
    pub fn build(self) -> IngestBatch {
        self.batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_unique_sequential_ids() {
        let mut b = BatchBuilder::new();
        let n1 = b.add_node("one");
        let n2 = b.add_node("two");
        assert_ne!(n1, n2);
        assert_eq!(n1.to_raw(), 1);
        assert_eq!(n2.to_raw(), 2);
    }

    #[test]
    fn adds_typed_fields_to_batch() {
        let mut b = BatchBuilder::new();
        let n1 = b.add_node("contract");
        b.add_field(n1, "expiry", FieldValue::Date(1_767_225_600));
        b.add_field(n1, "status", FieldValue::Str("active".into()));
        b.add_field(n1, "amount", FieldValue::Float(12_500.0));
        let batch = b.build();
        assert_eq!(batch.fields.len(), 3);
        assert_eq!(batch.fields[0].node_id, n1);
        assert_eq!(batch.fields[0].name, "expiry");
        assert_eq!(batch.fields[0].value, FieldValue::Date(1_767_225_600));
    }

    #[test]
    fn ids_are_unique_in_built_batch() {
        let mut b = BatchBuilder::new();
        let n1 = b.add_node("one");
        let n2 = b.add_node("two");
        b.add_edge(n1, n2, EdgeType::new("link"));
        let batch = b.build();
        assert_eq!(batch.nodes.len(), 2);
        assert_eq!(batch.edges.len(), 1);
        let mut ids: Vec<u64> = batch.nodes.iter().map(|n| n.id.to_raw()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), batch.nodes.len(), "node ids must be unique");
    }

    #[test]
    fn full_page_node_is_marked() {
        let mut b = BatchBuilder::new();
        let id = b.add_full_page_node("page");
        let batch = b.build();
        assert_eq!(batch.nodes.len(), 1);
        assert_eq!(batch.nodes[0].id, id);
        assert_eq!(batch.nodes[0].kind, NodeKind::FullPage);
        assert_eq!(batch.nodes[0].kind.as_str(), "full_page");
    }

    #[test]
    fn start_id_avoids_corpus_collision() {
        let mut b = BatchBuilder::with_start_id(1000);
        assert_eq!(b.add_node("x").to_raw(), 1000);
        assert_eq!(b.add_node("y").to_raw(), 1001);
    }
}
