// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! The corpus: versioned, migratable persistence (SQLite + FTS5),
//! atomic staging commits, compressed payloads, backup/restore.

pub mod compression;
pub mod corpus;
pub mod error;
pub mod header;
pub mod migration;

pub use corpus::{Corpus, StagingCorpus};
pub use error::{CorpusError, CorpusHeaderError, StorageError};
pub use header::{CorpusHeader, HEADER_LEN, MAGIC, SUPPORTED_MAJOR};
pub use migration::{Migration, SCHEMA_VERSION, migrate, migrations};
