// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! The records adapters produce: [`Node`], [`Edge`], [`EdgeType`],
//! [`NodeKind`].

use crate::core::NodeId;
use serde::{Deserialize, Serialize};

/// The kind of payload a node carries.
/// [`NodeKind::FullPage`] was added so adapters whose
/// [`crate::adapter::Adapter::supports_full_page`] returns `true` can mark the
/// nodes they emit — the storage schema has always accepted
/// `('text', 'full_page')` but no adapter could produce a full-page node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Stripped pure-text payload.
    Text,
    /// Full-page payload (e.g. a rendered web page). The heavy payload is
    /// fetched lazily on visibility; the node only carries the marker.
    FullPage,
}

impl NodeKind {
    /// The storage-schema string for this kind (`text` / `full_page`).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::FullPage => "full_page",
        }
    }

    /// Parse a storage-schema kind string back into a [`NodeKind`].
    /// Unknown strings yield [`None`] (the storage layer rejects them via
    /// its `CHECK` constraint; parsers should treat them as corrupt data).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "full_page" => Some(Self::FullPage),
            _ => None,
        }
    }
}

/// One record produced by an adapter.
///
/// `Node` is lightweight metadata; the heavy payload (full text) is
/// stored separately and fetched on demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Stable identifier for the node within the corpus. Equality and
    /// hashing key on this identifier.
    pub id: NodeId,
    /// Display label. UTF-8 short title.
    pub label: String,
    /// What the payload would be if loaded — `text` or `full_page`.
    /// Determines how the node renders.
    pub kind: NodeKind,
}

/// A typed edge between two nodes — the adjacency the topology engine
/// reasons over.
/// Edge type is opaque to the engine; adapters catalogue their own edge
/// types via `Adapter::edge_types`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// The source node (edge tail).
    pub from: NodeId,
    /// The destination node (edge head).
    pub to: NodeId,
    /// The opaque, adapter-assigned edge type.
    pub edge_type: EdgeType,
}

/// The opaque edge-type identifier. Adapters assign these strings; the
/// engine treats them genically.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EdgeType(pub String);

impl EdgeType {
    /// Build a typed edge identifier from any string-like value.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The edge type's string form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_to_snake_case_node_kind() {
        // The on-disk JSON for the corpus format pins the serde form of
        // NodeKind; cross-version reads depend on the exact spelling.
        assert_eq!(serde_json::to_string(&NodeKind::Text).unwrap(), "\"text\"");
    }

    #[test]
    fn edge_type_round_trips() {
        let et = EdgeType::new("hyperlink");
        assert_eq!(et.as_str(), "hyperlink");
        let json = serde_json::to_string(&et).unwrap();
        assert_eq!(json, "\"hyperlink\"");
        let back: EdgeType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, et);
    }
}
