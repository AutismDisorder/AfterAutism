// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Storage-layer error types: `StorageError`, `CorpusError`,
//! `CorpusHeaderError`.

use thiserror::Error;

/// Errors specific to corpus-header parsing and validation.
/// The split from [`StorageError`] exists so a caller that only wants to
/// peek at a header (e.g., an import tool deciding whether a corpus is
/// readable) can match against this enum directly without dragging the
/// whole storage error surface.
#[derive(Debug, Error)]
pub enum CorpusHeaderError {
    /// The leading 8 bytes of the file do not match the canonical
    /// `afterautism-storage` corpus magic. Usually signals an unrelated file
    /// mis-adopting the `.corpus` extension.
    #[error("corpus magic mismatch: expected {expected:?}, found {found:?}")]
    WrongMagic {
        /// The canonical magic the reader requires.
        expected: [u8; 8],
        /// What the file actually had at offset 0.
        found: [u8; 8],
    },

    /// The supplied input had fewer than `HEADER_LEN` bytes available
    /// before EOF. The header is a fixed-size prefix at the start of the
    /// corpus; a truncated prefix cannot be parsed.
    #[error("corpus header truncated: needed {needed} bytes, had {have}")]
    Truncated {
        /// Bytes the fixed-size header requires.
        needed: usize,
        /// Bytes actually available before EOF.
        have: usize,
    },

    /// The file claims a higher *major* version than the parser was
    /// built against. Per / the
    /// reader must **refuse** a future major — accepting it would risk
    /// silent corruption or misinterpretation of an incompatible
    /// on-disk layout. (Minor versions forward and backward are fine:
    /// see [`crate::storage::header::CorpusHeader`].)
    #[error(
        "corpus future major version unsupported: file has major={file}, reader supports up to major={supported}"
    )]
    FutureVersion {
        /// Major version the file claims.
        file: u8,
        /// Highest major version this reader understands.
        supported: u8,
    },
}

/// Errors from the corpus storage layer (`SQLite`, FTS5, atomic swap).
#[derive(Debug, Error)]
pub enum CorpusError {
    /// Any other corpus-level failure (e.g. compression layer).
    #[error("corpus error: {0}")]
    Other(String),

    /// `SQLite` error from rusqlite.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// I/O error during file operations (e.g., atomic swap).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The file is not an `afterautism` corpus (`SQLite` `application_id`
    /// mismatch).
    #[error("not an afterautism corpus: application_id {found}, expected {expected}")]
    WrongApplicationId {
        /// The `application_id` this engine stamps.
        expected: i32,
        /// The foreign id found in the file.
        found: i32,
    },

    /// Schema version mismatch or unsupported schema version.
    #[error("schema version mismatch: expected {expected}, found {found}")]
    SchemaVersion {
        /// The schema version this engine supports.
        expected: u32,
        /// The version found in the file.
        found: u32,
    },

    /// Atomic swap failed (staging to live).
    #[error("atomic swap failed: {0}")]
    AtomicSwap(String),

    /// Corpus is locked by another process.
    #[error("corpus locked: {0}")]
    Locked(String),

    /// Corpus file not found.
    #[error("corpus not found: {0}")]
    NotFound(String),

    /// Node ID cannot be represented as a signed 64-bit integer (e.g., unknown sentinel `u64::MAX`).
    #[error("node ID out of range for signed 64-bit integer: {0}")]
    InvalidNodeId(u64),
}

/// Top-level storage error. Wraps `CorpusHeaderError` and `CorpusError`.
#[derive(Debug, Error)]
pub enum StorageError {
    /// A corpus header parse/validate failure. The inner value is the
    /// concrete header-level reason (magic mismatch, truncation, future
    /// version, etc.).
    #[error(transparent)]
    CorpusHeader(#[from] CorpusHeaderError),

    /// Corpus storage layer error (`SQLite`, FTS5, atomic swap).
    #[error(transparent)]
    Corpus(#[from] CorpusError),

    /// `SQLite` error from rusqlite (direct conversion).
    /// Lock contention is classified: `SQLITE_BUSY`/`SQLITE_LOCKED`
    /// convert to [`CorpusError::Locked`] (see the `From` impl), so a
    /// caller sees the retryable contention kind instead of a generic
    /// sqlite error.
    #[error(transparent)]
    Sqlite(rusqlite::Error),

    /// I/O error (convenience for direct I/O operations).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<rusqlite::Error> for StorageError {
    fn from(e: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(ref failure, _) = e {
            match failure.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    return StorageError::Corpus(CorpusError::Locked(e.to_string()));
                }
                _ => {}
            }
        }
        StorageError::Sqlite(e)
    }
}

/// Convenience result alias for storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;
