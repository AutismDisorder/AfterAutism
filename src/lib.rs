// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! # The `afterautism` engine — one crate.
//! The engine's value is **access to data**: any filetype in (via
//! adapters), the right items out (via an expressive, deterministic
//! query language), reliably (typed, versioned storage).
//! Pipeline:
//! ```text
//! adapter (any filetype) -> typed corpus (nodes + edges + fields)
//! -> query language | topology filter
//! ```
//! - [`adapter`] — the extension contract: an adapter turns a filetype
//! into typed nodes, typed edges, and typed fields, making it queryable.
//! - [`core`] — ids, the error model, and the `NetworkGate` (offline by
//! default).
//! - [`storage`] — the versioned, migratable corpus (SQLite + FTS5),
//! atomic staging commits, compressed payloads, backup/restore.
//! - [`query`] — the query language: full-text, prefix, regex, kind,
//! typed-field comparisons, typed-edge traversal, boolean composition,
//! deterministic ordering and keyset pagination.
//! - [`topology`] — typed-edge filtering (emphasis masks) and graph
//! algorithms over the visible subgraph.
//! The input surface is the [`adapter`] contract itself: format
//! adapters (CSV, Markdown, PDF, ...) implement that contract and live
//! in consuming workspaces, not in the engine.
//!
//! ## Crate layout
//!
//! One crate, one version. The engine has no other crates; anything not
//! serving the in→out pipeline above does not belong here.

#![forbid(unsafe_code)]
#![warn(rust_2018_idioms)]
#![doc = include_str!("../README.md")]

pub mod adapter;
pub mod core;
pub mod prelude;
pub mod query;
pub mod storage;
pub mod topology;

/// Convenience prelude (see [`prelude`]).
pub use crate::adapter::{
    Adapter, BatchBuilder, Edge, EdgeType, FieldValue, IngestBatch, Node, NodeField, NodeKind,
    Source, SourceMeta,
};
pub use crate::core::{AdapterId, NetworkGate, NetworkPolicy, NodeId};
pub use crate::storage::{Corpus, StagingCorpus};
pub use crate::topology::VisibleGraph;
