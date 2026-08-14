// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! The corpus: versioned, migratable persistence (SQLite + FTS5),
//! atomic staging commits, compressed payloads, backup/restore.
//!
//! # Concurrency (multi-process access)
//!
//! The corpus is safe to share across processes and threads, with no
//! configuration:
//!
//! - Every open runs in WAL mode: any number of readers may run
//!   concurrently with each other and with one writer; writes
//!   serialize.
//! - Every connection waits up to 5 s for a momentarily held lock
//!   instead of failing instantly; if the lock is still held after the
//!   wait, the operation fails with [`CorpusError::Locked`] — the
//!   caller can retry the whole operation.
//! - Commits are atomic: `commit_to` swaps the staging file into the
//!   live path with an atomic rename. Concurrent commits cannot
//!   corrupt — the last commit wins, and readers that opened before
//!   the swap keep reading their own coherent snapshot.
//! - The live corpus file carries the newest transactions once the WAL
//!   is folded in (a commit checkpoints it); before then, a bare copy
//!   of the file may miss recent writes.
//! - To copy or move a corpus file safely, call
//!   [`corpus::Corpus::checkpoint`] first (folds the WAL into the main
//!   file), or copy the `-wal` and `-shm` siblings together with it.
//!
//! # Determinism note
//!
//! Reads are deterministic. The `created` / `updated` timestamps
//! stamped by writes are wall-clock values, informational only — they
//! are not part of the read contract.

pub mod compression;
pub mod corpus;
pub mod error;
pub mod header;
pub mod migration;

pub use corpus::{Corpus, StagingCorpus};
pub use error::{CorpusError, CorpusHeaderError, StorageError};
pub use header::{CorpusHeader, HEADER_LEN, MAGIC, SUPPORTED_MAJOR};
pub use migration::{Migration, SCHEMA_VERSION, migrate, migrations};
