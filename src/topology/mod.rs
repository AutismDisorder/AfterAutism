// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Typed-edge graph reasoning: the visible-subgraph structure
//! ([`VisibleGraph`]), filter predicates producing per-node emphasis
//! masks, and deterministic graph algorithms.

pub mod algorithms;
pub mod error;
pub mod filter;
pub mod graph;

pub use algorithms::{
    Direction, bfs, connected_components, degrees, dfs, has_cycle, largest_component, neighbors,
    shortest_path,
};
pub use error::TopologyError;
pub use filter::{EmphasisMask, FilterPredicate, apply_filter};
pub use graph::{TypedEdge, VisibleGraph};
