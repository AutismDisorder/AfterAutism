// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! The query language and executor: full-text, prefix, kind, regex,
//! typed-field comparisons, typed-edge traversal, and boolean
//! composition — parsed into a typed IR and executed against the corpus
//! with deterministic ordering and keyset pagination.

pub mod error;
pub mod exec;
pub mod ir;
pub mod parser;
pub mod prepared;
pub mod vector;

pub use error::{QueryError, Result};
pub use exec::{ExecOptions, execute, query};
pub use ir::{QueryExpr, QueryResult};
