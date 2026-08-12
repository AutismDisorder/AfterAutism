// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! The source contract: what an adapter ingests from — [`Source`] with
//! conditional-fetch metadata ([`SourceMeta`]).

use serde::{Deserialize, Serialize};

/// Conditional-request metadata for one source.
/// Persisted between refresh cycles and used for conditional requests
/// (If-None-Match / If-Modified-Since). A 304 Not Modified means the
/// source is unchanged — no body, no strip, no storage write.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceMeta {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// A source that an adapter ingests, or that the refresh coordinator
/// refreshes.
/// The `key` is the stable identifier (URL, file path, record id). The
/// `meta` is persisted between refresh cycles and used for conditional
/// requests. Binary formats (PDF, DOCX, XLSX) are supported: the adapter
/// receives the source and reads its own content — the contract carries
/// identity + metadata, not a lossy `&str` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub key: String,
    pub meta: SourceMeta,
}

impl Source {
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            meta: SourceMeta::default(),
        }
    }

    /// Create a source with pre-existing metadata (e.g., loaded from
    /// the staging index on startup).
    #[must_use]
    pub fn with_meta(key: impl Into<String>, meta: SourceMeta) -> Self {
        Self {
            key: key.into(),
            meta,
        }
    }
}
