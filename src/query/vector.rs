// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Vector search: cosine similarity, a flat ANN index, and hybrid
//! combination with text results.

use crate::core::NodeId;
use rayon::prelude::*;
use std::collections::HashMap;

/// A dense embedding.
/// The L2 norm is computed once at construction and cached: cosine
/// similarity against a large index reuses it on every comparison, so a
/// flat scan over N vectors costs one sqrt (the query's) instead of
/// 2N. `Embedding` is immutable by construction (only [`Embedding::new`]
/// builds one, and `dims`/`as_slice` are read-only), so the cache is
/// always consistent.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    dims: Vec<f32>,
    /// Precomputed L2 norm of `dims`.
    norm: f32,
}

impl Embedding {
    /// Build from raw floats.
    #[must_use]
    pub fn new(dims: Vec<f32>) -> Self {
        let norm = dims.iter().map(|x| x * x).sum::<f32>().sqrt();
        Self { dims, norm }
    }

    /// The vector's dimensionality.
    #[must_use]
    pub fn dims(&self) -> usize {
        self.dims.len()
    }

    /// The raw values.
    #[must_use]
    pub fn as_slice(&self) -> &[f32] {
        &self.dims
    }

    /// Cosine similarity in `[-1, 1]`; `0.0` when either vector is zero.
    #[must_use]
    pub fn cosine_similarity(&self, other: &Embedding) -> f32 {
        if self.dims.len() != other.dims.len() {
            return 0.0;
        }
        let dot: f32 = self.dims.iter().zip(&other.dims).map(|(a, b)| a * b).sum();
        let norms = self.norm * other.norm;
        if norms <= f32::EPSILON {
            0.0
        } else {
            dot / norms
        }
    }
}

/// One nearest-neighbour result.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorHit {
    pub node: NodeId,
    /// Cosine similarity in `[-1, 1]`.
    pub score: f32,
}

/// A flat (exact) vector index.
#[derive(Debug, Default)]
pub struct VectorIndex {
    vectors: HashMap<NodeId, Embedding>,
}

impl VectorIndex {
    /// A fresh, empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or replace) the embedding for a node.
    pub fn insert(&mut self, node: NodeId, embedding: Embedding) {
        self.vectors.insert(node, embedding);
    }

    /// Number of indexed vectors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    /// The embedding for a node, if indexed.
    #[must_use]
    pub fn get(&self, node: NodeId) -> Option<&Embedding> {
        self.vectors.get(&node)
    }

    /// Top-`k` nearest neighbours to `query` (exact cosine scan).
    /// The similarity scan is pure CPU over in-memory vectors, so it
    /// runs in parallel; the deterministic tie-break sort afterwards is
    /// unchanged.
    /// Deterministic tie-break by node id.
    pub fn search(&self, query: &Embedding, k: usize) -> Vec<VectorHit> {
        let mut hits: Vec<VectorHit> = self
            .vectors
            .par_iter()
            .map(|(node, emb)| VectorHit {
                node: *node,
                score: emb.cosine_similarity(query),
            })
            .collect();
        // Descending score, then ascending id for determinism.
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.node.to_raw().cmp(&b.node.to_raw()))
        });
        hits.truncate(k);
        hits
    }
}

/// Combine text and vector results: keep nodes that matched either,
/// scoring by max(similarity, text-present bonus).
/// `text_ids` are the ids matched by a text/kind/regex predicate;
/// `vector_hits` are nearest-neighbour results. Nodes in both lists rank
/// above nodes in only one.
#[must_use]
pub fn hybrid_combine(
    text_ids: &std::collections::BTreeSet<NodeId>,
    vector_hits: &[VectorHit],
    k: usize,
) -> Vec<NodeId> {
    let mut ranked: Vec<(NodeId, f32)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Vector hits first, with a boost when they also matched text.
    for hit in vector_hits {
        let boost = if text_ids.contains(&hit.node) {
            2.0
        } else {
            0.0
        };
        ranked.push((hit.node, hit.score + boost));
        seen.insert(hit.node);
    }
    // Text-only matches follow (boosted).
    for id in text_ids {
        if !seen.contains(id) {
            ranked.push((*id, 1.0));
            seen.insert(*id);
        }
    }
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.to_raw().cmp(&b.0.to_raw()))
    });
    ranked.into_iter().take(k).map(|(id, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emb(v: &[f32]) -> Embedding {
        Embedding::new(v.to_vec())
    }

    #[test]
    fn cosine_similarity_is_one_for_identical() {
        let a = emb(&[1.0, 0.0, 0.0]);
        assert!((a.cosine_similarity(&a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_is_zero_for_orthogonal() {
        let a = emb(&[1.0, 0.0]);
        let b = emb(&[0.0, 1.0]);
        assert!((a.cosine_similarity(&b)).abs() < 1e-6);
    }

    #[test]
    fn search_returns_nearest() {
        let mut index = VectorIndex::new();
        index.insert(NodeId::from_raw(1), emb(&[1.0, 0.0]));
        index.insert(NodeId::from_raw(2), emb(&[0.0, 1.0]));
        index.insert(NodeId::from_raw(3), emb(&[0.9, 0.1]));
        let hits = index.search(&emb(&[1.0, 0.0]), 2);
        assert_eq!(hits[0].node, NodeId::from_raw(1));
        assert_eq!(hits[1].node, NodeId::from_raw(3));
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn search_is_deterministic_on_ties() {
        let mut index = VectorIndex::new();
        index.insert(NodeId::from_raw(5), emb(&[1.0]));
        index.insert(NodeId::from_raw(2), emb(&[1.0]));
        let hits = index.search(&emb(&[1.0]), 2);
        assert_eq!(hits[0].node, NodeId::from_raw(2), "lower id first on tie");
        assert_eq!(hits[1].node, NodeId::from_raw(5));
    }

    #[test]
    fn dimension_mismatch_scores_zero() {
        let a = emb(&[1.0, 2.0]);
        let b = emb(&[1.0]);
        assert!((a.cosine_similarity(&b)).abs() < 1e-9);
    }

    #[test]
    fn hybrid_combine_ranks_overlap_first() {
        let text: std::collections::BTreeSet<NodeId> = [NodeId::from_raw(1), NodeId::from_raw(2)]
            .into_iter()
            .collect();
        let vector = vec![
            VectorHit {
                node: NodeId::from_raw(1),
                score: 0.8,
            },
            VectorHit {
                node: NodeId::from_raw(3),
                score: 0.9,
            },
        ];
        let combined = hybrid_combine(&text, &vector, 3);
        // Node 1 matched both -> highest.
        assert_eq!(combined[0], NodeId::from_raw(1));
    }
}
