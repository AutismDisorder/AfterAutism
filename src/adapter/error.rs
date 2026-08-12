// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Errors produced by adapter implementations.

use thiserror::Error;

/// Errors that can occur while an adapter ingests a source.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// The adapter does not implement ingestion for this source.
    #[error("this adapter does not support ingestion")]
    Unsupported,

    /// Failed to read the source (local path, stream, etc.).
    #[error("failed to read source: {0}")]
    Io(#[from] std::io::Error),

    /// The source content could not be parsed.
    #[error("failed to parse content: {0}")]
    Parse(String),

    /// The source type / key is not something this adapter handles.
    #[error("unsupported source type: {0}")]
    UnsupportedSource(String),

    /// Any other adapter-specific failure.
    #[error("adapter error: {0}")]
    Other(String),
}
