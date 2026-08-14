// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Query-layer errors.

use thiserror::Error;

/// Errors produced by parsing and executing queries.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QueryError {
    /// The query text could not be tokenized or parsed.
    #[error("parse error at byte {position}: {message}")]
    Parse {
        /// Byte offset into the query text where parsing failed.
        position: usize,
        /// Human-readable reason.
        message: String,
    },

    /// A referenced field, edge type, or node kind is unknown.
    #[error("unknown {what}: {name}")]
    Unknown {
        /// What was unknown ("field", "edge type", "kind", ...).
        what: String,
        /// The offending name.
        name: String,
    },

    /// A regular expression failed to compile.
    #[error("invalid regular expression: {0}")]
    InvalidRegex(String),

    /// The query referenced an unsupported feature.
    #[error("unsupported query feature: {0}")]
    Unsupported(String),

    /// The corpus is locked by another writer (transient contention;
    /// retryable). Mirrors [`crate::storage::CorpusError::Locked`] so a
    /// caller can classify "retry the whole operation" at the query
    /// layer too, instead of digging the cause out of an opaque string.
    #[error("corpus locked: {0}")]
    Locked(String),

    /// The corpus failed a structural invariant (wrong identity or
    /// schema, an id that cannot exist). Not retryable — the file is
    /// damaged or foreign.
    #[error("corpus corrupt: {0}")]
    Corrupt(String),

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
        match &err {
            crate::storage::StorageError::Corpus(crate::storage::CorpusError::Locked(m)) => {
                Self::Locked(m.clone())
            }
            crate::storage::StorageError::Corpus(
                crate::storage::CorpusError::WrongApplicationId { .. }
                | crate::storage::CorpusError::SchemaVersion { .. }
                | crate::storage::CorpusError::InvalidNodeId(_),
            )
            | crate::storage::StorageError::CorpusHeader(_) => Self::Corrupt(err.to_string()),
            crate::storage::StorageError::Sqlite(e) if is_lock_contention(e) => {
                Self::Locked(e.to_string())
            }
            _ => Self::Execution(err.to_string()),
        }
    }
}

/// True for SQLite error codes that mean "another writer holds the
/// lock right now" (`SQLITE_BUSY` / `SQLITE_LOCKED`). The storage
/// layer classifies these as [`crate::storage::CorpusError::Locked`];
/// this predicate lets the query layer apply the same classification to
/// direct `rusqlite` errors (e.g. from its own prepared statements).
pub(crate) fn is_lock_contention(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_lock_contention_maps_to_locked() {
        let err = QueryError::from(crate::storage::StorageError::Corpus(
            crate::storage::CorpusError::Locked("busy".into()),
        ));
        assert_eq!(err, QueryError::Locked("busy".into()));
    }

    #[test]
    fn corruption_maps_to_corrupt() {
        let err = QueryError::from(crate::storage::StorageError::Corpus(
            crate::storage::CorpusError::WrongApplicationId {
                expected: 1,
                found: 2,
            },
        ));
        assert!(matches!(err, QueryError::Corrupt(_)));
    }

    #[test]
    fn other_storage_errors_stay_generic() {
        let err = QueryError::from(crate::storage::StorageError::Io(std::io::Error::other(
            "disk exploded",
        )));
        assert!(matches!(err, QueryError::Execution(_)));
    }

    #[test]
    fn contention_detection_classifies_busy_and_locked() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        );
        let locked = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            Some("database table is locked".into()),
        );
        assert!(is_lock_contention(&busy));
        assert!(is_lock_contention(&locked));
        assert!(!is_lock_contention(&rusqlite::Error::InvalidQuery));
    }
}
