// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Convenience prelude: re-exports the most-used engine types.
//! Import with `use afterautism::prelude::*;`.

pub use crate::adapter::{
    Adapter, BatchBuilder, Edge, EdgeType, FieldValue, IngestBatch, Node, NodeField, NodeKind,
    Source, SourceMeta,
};
pub use crate::core::{AdapterId, NetworkGate, NetworkPolicy, NodeId};
pub use crate::query::exec::{ExecOptions, ResultOrder};
pub use crate::query::ir::TraverseDirection;
pub use crate::query::{QueryExpr, QueryResult};
pub use crate::storage::{Corpus, StagingCorpus};
pub use crate::topology::VisibleGraph;
