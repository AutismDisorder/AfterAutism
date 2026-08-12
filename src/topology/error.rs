// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors
//! Topology engine error types.

use thiserror::Error;

/// Errors specific to the topology engine.
#[derive(Debug, Error)]
pub enum TopologyError {
    /// A filter predicate referenced an edge type not present in the
    /// adapter's catalog. This is a logic error in the filter expression.
    #[error("unknown edge type in filter predicate: {0}")]
    UnknownEdgeType(String),

    /// The filter predicate was malformed or could not be parsed.
    #[error("invalid filter predicate: {0}")]
    InvalidPredicate(String),

    /// The visible subgraph contained a node that had no entry in the
    /// node catalog — a synchronization bug in the topology layer.
    #[error("node {0} referenced in visible subgraph but not in catalog")]
    MissingNode(String),
}
