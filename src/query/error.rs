// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Query-layer errors.

use thiserror::Error;

/// Errors produced by parsing and executing queries.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QueryError {
    /// The query text could not be tokenized or parsed.
    #[error("parse error at byte {position}: {message}")]
    Parse { position: usize, message: String },

    /// A referenced field, edge type, or node kind is unknown.
    #[error("unknown {what}: {name}")]
    Unknown { what: String, name: String },

    /// A regular expression failed to compile.
    #[error("invalid regular expression: {0}")]
    InvalidRegex(String),

    /// The query referenced an unsupported feature.
    #[error("unsupported query feature: {0}")]
    Unsupported(String),

    /// Executing the query against the corpus failed.
    #[error("execution failed: {0}")]
    Execution(String),
}

impl QueryError {
    /// A parse error at a byte position.
    pub fn parse(position: usize, message: impl Into<String>) -> Self {
        Self::Parse {
            position,
            message: message.into(),
        }
    }

    /// An unknown-field error.
    pub fn unknown(what: impl Into<String>, name: impl Into<String>) -> Self {
        Self::Unknown {
            what: what.into(),
            name: name.into(),
        }
    }
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, QueryError>;

impl From<crate::storage::StorageError> for QueryError {
    fn from(err: crate::storage::StorageError) -> Self {
        Self::Execution(err.to_string())
    }
}
