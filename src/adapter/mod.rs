// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! The adapter contract: an `Adapter` turns a `Source` (any filetype)
//! into an [`IngestBatch`] of typed nodes, edges, and fields that the
//! engine can store and query.

pub mod builder;
pub mod error;
pub mod fields;
pub mod record;
pub mod source;
pub mod trait_def;

pub use builder::BatchBuilder;
pub use error::AdapterError;
pub use fields::{FieldValue, NodeField};
pub use record::{Edge, EdgeType, Node, NodeKind};
pub use source::{Source, SourceMeta};
pub use trait_def::{ADAPTER_ABI_VERSION, Adapter, AdapterCapabilities, IngestBatch};
