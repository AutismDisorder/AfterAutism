// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Corpus persistence (SQLite + FTS5): versioned, migratable storage
//! with atomic staging commits.

use crate::adapter::trait_def::IngestBatch;
use crate::adapter::{Edge, EdgeType, FieldValue, Node, NodeField, NodeKind};
use crate::core::NodeId;
use crate::storage::error::{CorpusError, Result, StorageError};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::Path;

/// A full-text search hit with its FTS5 relevance rank and a snippet.
/// `search_nodes` returns raw nodes; consumers that need ranking
/// or highlight context use [`Corpus::search_hits`].
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// The matched node.
    pub node: Node,
    /// FTS5 bm25 rank (lower is better).
    pub rank: f64,
    /// A short snippet around the first match (empty when unavailable).
    pub snippet: String,
}

/// Current schema version for the `SQLite` database.
/// Increment when making incompatible schema changes.
/// Schema history:
/// - 2: unique edge index — re-writing the same batch does not
///   duplicate edges.
/// - 3: the `node_fields` table (typed fields).
///
/// Older corpora migrate in place on open.
const SCHEMA_VERSION: u32 = crate::storage::migration::SCHEMA_VERSION;

/// Application id written into the `SQLite` header
/// (`PRAGMA application_id`) to identify an `afterautism-storage` corpus
/// and reject foreign DB files.
/// the flat 16-byte `CorpusHeader` in `header.rs` is the spec for a
/// future non-`SQLite` payload format and cannot layer over `SQLite`
/// (which owns a 100-byte header at offset 0). The correct version gate
/// for the `SQLite` corpus is `application_id` (identity) +
/// `user_version` (schema version), both part of `SQLite`'s own header.
const CORPUS_APPLICATION_ID: i32 = 0x4154_4152; // b"ATAR" */
/// Open a connection to the corpus database, creating schema if needed.
fn open_corpus(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )?;

    // version gate: read the identity + schema version FIRST.
    // A fresh file (application_id 0) is initialized here; a foreign
    // SQLite file (any other application_id) is rejected; a corpus from
    // an older schema version is migrated in place; a newer one is
    // refused.
    let app_id: i32 = {
        let mut stmt = conn.prepare("PRAGMA application_id")?;
        stmt.query_row([], |row| row.get(0))?
    }; // stmt dropped here
    let version: u32 = {
        let mut stmt = conn.prepare("PRAGMA user_version")?;
        stmt.query_row([], |row| row.get(0))?
    }; // stmt dropped here

    if app_id == 0 && version == 0 {
        // Fresh file: stamp identity + schema version and create schema.
        init_schema(&mut conn)?;
    } else if app_id != CORPUS_APPLICATION_ID {
        return Err(StorageError::Corpus(CorpusError::WrongApplicationId {
            expected: CORPUS_APPLICATION_ID,
            found: app_id,
        }));
    } else if version > SCHEMA_VERSION {
        // Newer engine wrote this corpus; we cannot read it safely.
        return Err(StorageError::Corpus(CorpusError::SchemaVersion {
            expected: SCHEMA_VERSION,
            found: version,
        }));
    } else if version < SCHEMA_VERSION {
        // Older corpus: run the idempotent migration chain, then stamp
        // the new schema version so the next open is a no-op.
        crate::storage::migration::migrate(&mut conn)?;
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    }

    Ok(conn)
}

/// Initialize the database schema (nodes, edges, FTS5).
/// One `CREATE` per object: the body is a declarative DDL list, not
/// imperative logic, so the length is inherent.
#[allow(clippy::too_many_lines)]
fn init_schema(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;

    // Nodes table: stores node metadata
    tx.execute(
        "CREATE TABLE nodes (
            id       INTEGER PRIMARY KEY,
            label    TEXT NOT NULL,
            kind     TEXT NOT NULL CHECK (kind IN ('text', 'full_page')),
            adapter  TEXT NOT NULL,
            created  INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            updated  INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        )",
        [],
    )?;

    // Edges table: typed edges between nodes
    tx.execute(
        "CREATE TABLE edges (
            id          INTEGER PRIMARY KEY,
            from_node   INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            to_node     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            edge_type   TEXT NOT NULL,
            created     INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        )",
        [],
    )?;

    // Index for efficient edge lookups
    tx.execute("CREATE INDEX idx_edges_from ON edges(from_node)", [])?;
    tx.execute("CREATE INDEX idx_edges_to ON edges(to_node)", [])?;
    tx.execute("CREATE INDEX idx_edges_type ON edges(edge_type)", [])?;

    // unique edge index makes idempotent re-writes of the same
    // batch a no-op instead of duplicating edges .
    tx.execute(
        "CREATE UNIQUE INDEX idx_edges_unique
            ON edges(from_node, to_node, edge_type)",
        [],
    )?;

    // FTS5 virtual table for full-text search on node labels
    tx.execute(
        "CREATE VIRTUAL TABLE nodes_fts USING fts5(
            label,
            content='nodes',
            content_rowid='id'
        )",
        [],
    )?;

    // Triggers to keep FTS5 in sync with nodes table
    tx.execute(
        "CREATE TRIGGER nodes_ai AFTER INSERT ON nodes BEGIN
            INSERT INTO nodes_fts(rowid, label) VALUES (new.id, new.label);
        END",
        [],
    )?;
    tx.execute(
        "CREATE TRIGGER nodes_ad AFTER DELETE ON nodes BEGIN
            INSERT INTO nodes_fts(nodes_fts, rowid, label) VALUES ('delete', old.id, old.label);
        END",
        [],
    )?;
    tx.execute(
        "CREATE TRIGGER nodes_au AFTER UPDATE ON nodes BEGIN
            INSERT INTO nodes_fts(nodes_fts, rowid, label) VALUES ('delete', old.id, old.label);
            INSERT INTO nodes_fts(rowid, label) VALUES (new.id, new.label);
        END",
        [],
    )?;

    // Schema version table for migrations
    // compressed payload storage (zstd), separate from labels.
    tx.execute(
        "CREATE TABLE IF NOT EXISTS node_payloads (
            node_id INTEGER PRIMARY KEY,
            payload BLOB NOT NULL
        )",
        [],
    )?;
    // typed node fields (form-declared semantics). One row per
    // (node, field-name); the kind column tags which value column is
    // live. The name index keeps field lookups O(fields-with-name).
    tx.execute(
        "CREATE TABLE node_fields (
            node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            name        TEXT NOT NULL,
            kind        TEXT NOT NULL CHECK (kind IN ('str','int','float','bool','date')),
            value_int   INTEGER,
            value_float REAL,
            value_text  TEXT,
            PRIMARY KEY (node_id, name)
        )",
        [],
    )?;
    tx.execute("CREATE INDEX idx_node_fields_name ON node_fields(name)", [])?;
    tx.execute(
        "CREATE TABLE schema_version (
            version INTEGER PRIMARY KEY,
            applied INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
        )",
        [],
    )?;
    tx.execute(
        "INSERT INTO schema_version (version) VALUES (?)",
        params![SCHEMA_VERSION],
    )?;

    // Stamp identity + schema version for the read-first version gate.
    // application_id identifies the file as an afterautism corpus;
    // user_version carries the schema version for quick checks.
    tx.execute_batch(&format!(
        "PRAGMA application_id = {CORPUS_APPLICATION_ID};
         PRAGMA user_version = {SCHEMA_VERSION};"
    ))?;

    tx.commit()?;
    Ok(())
}

/// Corpus handle for reading and writing.
pub struct Corpus {
    path: std::path::PathBuf,
    conn: Connection,
}

impl std::fmt::Debug for Corpus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Corpus")
            .field("path", &self.path)
            .field("conn", &"<sqlite connection>")
            .finish()
    }
}

impl Corpus {
    /// Open an existing corpus or create a new one.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = open_corpus(&path)?;
        Ok(Self { path, conn })
    }

    /// Create a new corpus (fails if exists).
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            return Err(StorageError::Corpus(CorpusError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "corpus already exists",
            ))));
        }
        let conn = open_corpus(&path)?;
        Ok(Self { path, conn })
    }

    /// Get the database connection (for read-only operations).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Get the corpus file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write a batch of nodes and edges (used by refresh staging).
    /// Both INSERT statements are prepared once and reused per row, so a
    /// large batch does not re-compile SQL for every node/edge. Typed
    /// fields are written in the same transaction: each node's field set
    /// is replaced (DELETE + re-INSERT), so idempotent refreshes converge.
    pub fn write_batch(&mut self, batch: &IngestBatch, adapter_id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;

        // Insert nodes.
        // `ON CONFLICT DO UPDATE` instead of `INSERT OR REPLACE`:
        // REPLACE is DELETE+INSERT, which churns the FTS5 triggers and
        // rowids on every idempotent refresh. Upsert preserves rowids.
        // persist the node's actual kind (schema accepts 'text'
        // and 'full_page') instead of hardcoding 'text'.
        {
            let mut insert_node = tx.prepare_cached(
                "INSERT INTO nodes (id, label, kind, adapter) VALUES (?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    label = excluded.label,
                    kind = excluded.kind,
                    adapter = excluded.adapter,
                    updated = strftime('%s', 'now')",
            )?;
            for node in &batch.nodes {
                let node_id = node.id.to_raw();
                let node_id_i64 = i64::try_from(node_id)
                    .map_err(|_| StorageError::Corpus(CorpusError::InvalidNodeId(node_id)))?;
                insert_node.execute(params![
                    node_id_i64,
                    node.label,
                    node.kind.as_str(),
                    adapter_id
                ])?;
            }
        }

        // Insert edges.
        {
            let mut insert_edge = tx.prepare_cached(
                "INSERT OR IGNORE INTO edges (from_node, to_node, edge_type) VALUES (?, ?, ?)",
            )?;
            for edge in &batch.edges {
                let from_id = edge.from.to_raw();
                let from_id_i64 = i64::try_from(from_id)
                    .map_err(|_| StorageError::Corpus(CorpusError::InvalidNodeId(from_id)))?;
                let to_id = edge.to.to_raw();
                let to_id_i64 = i64::try_from(to_id)
                    .map_err(|_| StorageError::Corpus(CorpusError::InvalidNodeId(to_id)))?;
                insert_edge.execute(params![from_id_i64, to_id_i64, edge.edge_type.as_str()])?;
            }
        }

        // Typed fields. The kind column tags which value column is
        // live; bool and date both ride on value_int (0/1 and unix
        // seconds respectively).
        {
            let mut delete_fields =
                tx.prepare_cached("DELETE FROM node_fields WHERE node_id = ?")?;
            let mut insert_field = tx.prepare_cached(
                "INSERT INTO node_fields (node_id, name, kind, value_int, value_float, value_text)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )?;
            for node in &batch.nodes {
                let node_id = node.id.to_raw();
                let node_id_i64 = i64::try_from(node_id)
                    .map_err(|_| StorageError::Corpus(CorpusError::InvalidNodeId(node_id)))?;
                delete_fields.execute(params![node_id_i64])?;
            }
            for field in &batch.fields {
                let node_id = field.node_id.to_raw();
                let node_id_i64 = i64::try_from(node_id)
                    .map_err(|_| StorageError::Corpus(CorpusError::InvalidNodeId(node_id)))?;
                let (kind, value_int, value_float, value_text) = match &field.value {
                    FieldValue::Str(s) => ("str", None, None, Some(s.as_str())),
                    FieldValue::Int(v) => ("int", Some(*v), None, None),
                    FieldValue::Float(v) => ("float", None, Some(*v), None),
                    FieldValue::Bool(v) => ("bool", Some(i64::from(*v)), None, None),
                    FieldValue::Date(v) => ("date", Some(*v), None, None),
                };
                insert_field.execute(params![
                    node_id_i64,
                    field.name,
                    kind,
                    value_int,
                    value_float,
                    value_text
                ])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Convert a `(id, label, kind)` row into a [`Node`], reading the
    /// stored kind.
    fn node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
        let id_i64: i64 = row.get(0)?;
        let converted_id = u64::try_from(id_i64).map_err(|_| {
            #[allow(clippy::cast_sign_loss)]
            rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                CorpusError::InvalidNodeId(id_i64 as u64),
            )))
        })?;
        let kind_str: String = row.get(2)?;
        // The schema CHECK constrains kind to ('text','full_page'); a
        // value that fails to parse means corrupt data — treat it as
        // text rather than crashing, and surface via the kind column.
        let kind = NodeKind::parse(&kind_str).unwrap_or(NodeKind::Text);
        Ok(Node {
            id: NodeId::from_raw(converted_id),
            label: row.get(1)?,
            kind,
        })
    }

    /// Get a node by ID.
    pub fn get_node(&self, node_id: NodeId) -> Result<Option<Node>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, label, kind FROM nodes WHERE id = ?")?;
        let node = stmt
            .query_row(
                params![i64::try_from(node_id.to_raw()).map_err(|_| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                        CorpusError::InvalidNodeId(node_id.to_raw()),
                    )))
                })?],
                Self::node_from_row,
            )
            .optional()?;
        Ok(node)
    }

    /// Get nodes by a list of IDs (for visible window).
    pub fn get_nodes(&self, node_ids: &[NodeId]) -> Result<Vec<Node>> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = node_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!("SELECT id, label, kind FROM nodes WHERE id IN ({placeholders})");
        let mut stmt = self.conn.prepare(&query)?;

        // Convert node_ids to i64 values and bind them with an iterator
        // (no per-id `Box<dyn ToSql>` allocations).
        let id_values: Vec<i64> = node_ids
            .iter()
            .map(|id| {
                i64::try_from(id.to_raw()).map_err(|_| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                        CorpusError::InvalidNodeId(id.to_raw()),
                    )))
                })
            })
            .collect::<std::result::Result<_, _>>()?;

        let nodes = stmt.query_map(rusqlite::params_from_iter(id_values.iter()), |row| {
            Self::node_from_row(row)
        })?;

        let mut result = Vec::new();
        for node in nodes {
            result.push(node?);
        }
        Ok(result)
    }

    /// All typed fields attached to one node.
    pub fn node_fields(&self, node_id: NodeId) -> Result<Vec<NodeField>> {
        let node_id_i64 = i64::try_from(node_id.to_raw()).map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                CorpusError::InvalidNodeId(node_id.to_raw()),
            )))
        })?;
        let mut stmt = self.conn.prepare(
            "SELECT name, kind, value_int, value_float, value_text
             FROM node_fields WHERE node_id = ? ORDER BY name",
        )?;
        let rows = stmt.query_map(params![node_id_i64], |row| {
            let name: String = row.get(0)?;
            let value = Self::field_value_from_row(row)?;
            Ok(NodeField {
                node_id,
                name,
                value,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// All typed fields stored under `name`, as `(node id, value)` pairs
    /// in ascending node-id order.
    /// The query executor filters these in Rust; the `name` index keeps
    /// the lookup at O(fields-with-name). Missing fields simply do not
    /// appear (comparison predicates never match a missing field).
    pub fn field_values(&self, name: &str) -> Result<Vec<(NodeId, FieldValue)>> {
        let mut stmt = self.conn.prepare(
            "SELECT node_id, kind, value_int, value_float, value_text
             FROM node_fields WHERE name = ? ORDER BY node_id",
        )?;
        let rows = stmt.query_map(params![name], |row| {
            let id_i64: i64 = row.get(0)?;
            let id = u64::try_from(id_i64).map_err(|_| {
                #[allow(clippy::cast_sign_loss)]
                rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                    CorpusError::InvalidNodeId(id_i64 as u64),
                )))
            })?;
            let value = Self::field_value_from_row(row)?;
            Ok((NodeId::from_raw(id), value))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Convert a `node_fields` value row `(kind, value_int, value_float,
    /// value_text)` into a [`FieldValue`].
    fn field_value_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FieldValue> {
        let kind: String = row.get(1)?;
        match kind.as_str() {
            "str" => Ok(FieldValue::Str(row.get(4)?)),
            "int" => Ok(FieldValue::Int(row.get(2)?)),
            "float" => Ok(FieldValue::Float(row.get(3)?)),
            "bool" => Ok(FieldValue::Bool(
                row.get::<_, Option<i64>>(2)?.unwrap_or(0) != 0,
            )),
            "date" => Ok(FieldValue::Date(row.get(2)?)),
            other => Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                StorageError::Corpus(CorpusError::Other(format!("unknown field kind {other:?}"))),
            ))),
        }
    }

    /// All node ids in the corpus (ascending).
    /// supports `QueryExpr::All` and `not` without a full scan of
    /// node rows.
    pub fn all_node_ids(&self) -> Result<Vec<NodeId>> {
        let mut stmt = self.conn.prepare("SELECT id FROM nodes ORDER BY id")?;
        let ids = stmt.query_map([], |row| {
            let id_i64: i64 = row.get(0)?;
            // ids are INTEGER PRIMARY KEY — always non-negative.
            Ok(u64::try_from(id_i64).unwrap_or(u64::MAX))
        })?;
        let mut out = Vec::new();
        for id in ids {
            out.push(NodeId::from_raw(id?));
        }
        Ok(out)
    }

    /// All nodes in the corpus (ascending by id).
    pub fn all_nodes(&self) -> Result<Vec<Node>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, label, kind FROM nodes ORDER BY id")?;
        let rows = stmt.query_map([], Self::node_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Nodes whose stored kind equals `kind` (`text` / `full_page`).
    pub fn nodes_by_kind(&self, kind: &str) -> Result<Vec<Node>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, label, kind FROM nodes WHERE kind = ? ORDER BY id")?;
        let rows = stmt.query_map(params![kind], Self::node_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Nodes whose label equals the given value (exact match).
    /// Used by `field:value` style queries on the label.
    pub fn nodes_by_label(&self, field: &str, value: &str) -> Result<Vec<Node>> {
        let _ = field; // labels are the only searchable field today
        let mut stmt = self
            .conn
            .prepare("SELECT id, label, kind FROM nodes WHERE label = ? ORDER BY id")?;
        let rows = stmt.query_map(params![value], Self::node_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Convert an `edges` row `(from_node, to_node, edge_type)` into an
    /// [`Edge`], shared by the single-node and batched edge queries.
    #[allow(clippy::similar_names)]
    fn edge_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Edge> {
        let src_i64: i64 = row.get(0)?;
        let dst_i64: i64 = row.get(1)?;
        let src_u64 = u64::try_from(src_i64).map_err(|_| {
            // src_i64 is negative or too large here; the cast is lossy but the value is already invalid
            #[allow(clippy::cast_sign_loss)]
            rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                CorpusError::InvalidNodeId(src_i64 as u64),
            )))
        })?;
        let dst_u64 = u64::try_from(dst_i64).map_err(|_| {
            // dst_i64 is negative or too large here; the cast is lossy but the value is already invalid
            #[allow(clippy::cast_sign_loss)]
            rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                CorpusError::InvalidNodeId(dst_i64 as u64),
            )))
        })?;
        Ok(Edge {
            from: NodeId::from_raw(src_u64),
            to: NodeId::from_raw(dst_u64),
            edge_type: EdgeType::new(row.get::<_, String>(2)?),
        })
    }

    /// Get outgoing edges for a node.
    #[allow(clippy::similar_names)]
    pub fn get_outgoing_edges(&self, node_id: NodeId) -> Result<Vec<Edge>> {
        let mut stmt = self
            .conn
            .prepare("SELECT from_node, to_node, edge_type FROM edges WHERE from_node = ?")?;
        let node_id_i64 = i64::try_from(node_id.to_raw()).map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                CorpusError::InvalidNodeId(node_id.to_raw()),
            )))
        })?;
        let edges = stmt.query_map(params![node_id_i64], Self::edge_from_row)?;

        let mut result = Vec::new();
        for edge in edges {
            result.push(edge?);
        }
        Ok(result)
    }

    /// Get outgoing edges for a set of nodes with one query.
    /// Batches the traversal fan-out: the query executor calls this once
    /// per `traverse` step instead of preparing a statement per source
    /// node. Rows are ordered by `from_node`, and results are grouped by
    /// source node in ascending id order. Duplicate ids in `node_ids`
    /// are deduplicated by the `IN` clause.
    pub fn get_outgoing_edges_for(&self, node_ids: &[NodeId]) -> Result<Vec<(NodeId, Vec<Edge>)>> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = node_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT from_node, to_node, edge_type FROM edges WHERE from_node IN ({placeholders}) ORDER BY from_node"
        );
        let mut stmt = self.conn.prepare(&query)?;
        let id_values: Vec<i64> = node_ids
            .iter()
            .map(|id| {
                i64::try_from(id.to_raw()).map_err(|_| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                        CorpusError::InvalidNodeId(id.to_raw()),
                    )))
                })
            })
            .collect::<std::result::Result<_, _>>()?;

        let rows = stmt.query_map(
            rusqlite::params_from_iter(id_values.iter()),
            Self::edge_from_row,
        )?;
        let mut grouped: Vec<(NodeId, Vec<Edge>)> = Vec::new();
        for row in rows {
            let edge = row?;
            match grouped.last_mut() {
                Some((from, edges)) if *from == edge.from => edges.push(edge),
                _ => grouped.push((edge.from, vec![edge])),
            }
        }
        Ok(grouped)
    }

    /// Get incoming edges for a node.
    #[allow(clippy::similar_names)]
    pub fn get_incoming_edges(&self, node_id: NodeId) -> Result<Vec<Edge>> {
        let mut stmt = self
            .conn
            .prepare("SELECT from_node, to_node, edge_type FROM edges WHERE to_node = ?")?;
        let node_id_i64 = i64::try_from(node_id.to_raw()).map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                CorpusError::InvalidNodeId(node_id.to_raw()),
            )))
        })?;
        let edges = stmt.query_map(params![node_id_i64], Self::edge_from_row)?;

        let mut result = Vec::new();
        for edge in edges {
            result.push(edge?);
        }
        Ok(result)
    }

    /// Full-text search on node labels.
    pub fn search_nodes(&self, query: &str, limit: usize) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.label, n.kind FROM nodes_fts
             JOIN nodes n ON nodes_fts.rowid = n.id
             WHERE nodes_fts MATCH ?
             ORDER BY rank
             LIMIT ?",
        )?;
        let limit_i64 = i64::try_from(limit).map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                CorpusError::InvalidNodeId(limit as u64),
            )))
        })?;
        let nodes = stmt.query_map(params![query, limit_i64], Self::node_from_row)?;

        let mut result = Vec::new();
        for node in nodes {
            result.push(node?);
        }
        Ok(result)
    }

    /// Full-text search returning ranked hits with snippets.
    /// Uses FTS5 `bm25` ranking (lower = better) and `snippet()` for
    /// highlight context.
    pub fn search_hits(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.id, n.label, n.kind,
                    bm25(nodes_fts) AS rank,
                    snippet(nodes_fts, 0, '[', ']', '…', 24) AS snippet
             FROM nodes_fts
             JOIN nodes n ON nodes_fts.rowid = n.id
             WHERE nodes_fts MATCH ?
             ORDER BY rank
             LIMIT ?",
        )?;
        let limit_i64 = i64::try_from(limit).map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                CorpusError::InvalidNodeId(limit as u64),
            )))
        })?;
        let rows = stmt.query_map(params![query, limit_i64], |row| {
            let node = Self::node_from_row(row)?;
            let rank: f64 = row.get(3)?;
            let snippet: String = row.get(4)?;
            Ok(SearchHit {
                node,
                rank,
                snippet,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Iterate all nodes with keyset pagination (ascending by id).
    /// Returns up to `limit` nodes with ids strictly greater than
    /// `after`, plus the next cursor.
    pub fn page_nodes(
        &self,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<(Vec<Node>, Option<NodeId>)> {
        let limit_i64 = i64::try_from(limit).map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                CorpusError::InvalidNodeId(limit as u64),
            )))
        })?;
        let mut nodes = Vec::new();
        if let Some(cursor) = after {
            let cursor_i64 = i64::try_from(cursor.to_raw()).map_err(|_| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                    CorpusError::InvalidNodeId(cursor.to_raw()),
                )))
            })?;
            let mut stmt = self
                .conn
                .prepare("SELECT id, label, kind FROM nodes WHERE id > ? ORDER BY id LIMIT ?")?;
            for row in stmt.query_map(params![cursor_i64, limit_i64], Self::node_from_row)? {
                nodes.push(row?);
            }
        } else {
            let mut stmt = self
                .conn
                .prepare("SELECT id, label, kind FROM nodes ORDER BY id LIMIT ?")?;
            for row in stmt.query_map(params![limit_i64], Self::node_from_row)? {
                nodes.push(row?);
            }
        }
        let next = nodes.last().map(|n| n.id);
        Ok((nodes, next))
    }

    /// Back up the corpus to `dest` (`SQLite` online backup API).
    /// Safe to run while the corpus is open; produces a consistent
    /// snapshot. `dest` must not already exist.
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        let mut dest_conn = Connection::open(dest)?;
        let backup = rusqlite::backup::Backup::new(&self.conn, &mut dest_conn)?;
        backup.run_to_completion(32, std::time::Duration::from_millis(100), None)?;
        Ok(())
    }

    /// Restore this corpus from a backup file, replacing current contents.
    /// Opens `src` read-only, verifies it is a valid corpus, and copies
    /// every table into this corpus. Destructive: the current corpus is
    /// overwritten. Prefer restoring into a fresh `Corpus` unless you are
    /// certain.
    pub fn restore_from(&mut self, src: &Path) -> Result<()> {
        let src_conn =
            Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // Verify the source is one of ours before copying.
        let app_id: i64 = src_conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let expected = self
            .conn
            .query_row("PRAGMA application_id", [], |r| r.get::<_, i64>(0))?;
        if app_id != expected {
            return Err(StorageError::Corpus(CorpusError::WrongApplicationId {
                expected: i32::try_from(expected).unwrap_or(-1),
                found: i32::try_from(app_id).unwrap_or(-1),
            }));
        }
        self.conn
            .execute_batch("DELETE FROM nodes; DELETE FROM edges;")?;
        let backup = rusqlite::backup::Backup::new(&src_conn, &mut self.conn)?;
        backup.run_to_completion(32, std::time::Duration::from_millis(100), None)?;
        Ok(())
    }

    /// Store a compressed payload for a node (zstd).
    /// Payloads are the heavy text of a node (full document text, page
    /// content, etc.) — separate from the indexed label. They compress
    /// well for document corpora; short payloads take a raw fast path.
    pub fn set_payload(&self, node_id: NodeId, payload: &[u8]) -> Result<()> {
        let compressed = crate::storage::compression::compress(payload, 64)
            .map_err(|e| StorageError::Corpus(CorpusError::Other(e.to_string())))?;
        let node_i64 = i64::try_from(node_id.to_raw()).map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                CorpusError::InvalidNodeId(node_id.to_raw()),
            )))
        })?;
        self.conn.execute(
            "INSERT INTO node_payloads (node_id, payload) VALUES (?, ?)
             ON CONFLICT(node_id) DO UPDATE SET payload = excluded.payload",
            params![node_i64, compressed],
        )?;
        Ok(())
    }

    /// Read a node's compressed payload back (decompressed).
    pub fn payload(&self, node_id: NodeId) -> Result<Option<Vec<u8>>> {
        let node_i64 = i64::try_from(node_id.to_raw()).map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(StorageError::Corpus(
                CorpusError::InvalidNodeId(node_id.to_raw()),
            )))
        })?;
        let compressed: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT payload FROM node_payloads WHERE node_id = ?",
                params![node_i64],
                |row| row.get(0),
            )
            .optional()?;
        match compressed {
            Some(bytes) => {
                let raw = crate::storage::compression::decompress(&bytes, 64 * 1024 * 1024)
                    .map_err(|e| StorageError::Corpus(CorpusError::Other(e.to_string())))?;
                Ok(Some(raw))
            }
            None => Ok(None),
        }
    }

    /// Get total node count.
    pub fn node_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))?;
        // COUNT(*) is always non-negative
        #[allow(clippy::cast_sign_loss)]
        Ok(count as u64)
    }

    /// Get total edge count.
    pub fn edge_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))?;
        // COUNT(*) is always non-negative
        #[allow(clippy::cast_sign_loss)]
        Ok(count as u64)
    }
}

/// Staging corpus for atomic refresh.
/// Implements / : refresh applies to a staging index,
/// then swaps atomically on commit.
pub struct StagingCorpus {
    path: std::path::PathBuf,
    conn: Connection,
}

impl std::fmt::Debug for StagingCorpus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagingCorpus")
            .field("path", &self.path)
            .field("conn", &"<sqlite connection>")
            .finish()
    }
}

impl StagingCorpus {
    /// Create a new staging corpus.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            fs::remove_file(&path)?;
        }
        let conn = open_corpus(&path)?;
        Ok(Self { path, conn })
    }

    /// Write a batch to staging.
    /// uses the same checked `i64` conversion and `ON CONFLICT DO
    /// UPDATE` upsert as the live corpus write path (the previous `as i64`
    /// cast could silently wrap node ids above `i64::MAX`).
    pub fn write_batch(&mut self, batch: &IngestBatch, adapter_id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;

        for node in &batch.nodes {
            let node_id = i64::try_from(node.id.to_raw())
                .map_err(|_| StorageError::Corpus(CorpusError::InvalidNodeId(node.id.to_raw())))?;
            tx.execute(
                "INSERT INTO nodes (id, label, kind, adapter) VALUES (?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    label = excluded.label,
                    kind = excluded.kind,
                    adapter = excluded.adapter,
                    updated = strftime('%s', 'now')",
                // persist the node's real kind — this is the ingest
                // path (desktop + coordinator), and it previously hardcoded
                // "text", silently downgrading full_page nodes.
                params![node_id, node.label, node.kind.as_str(), adapter_id,],
            )?;
        }

        for edge in &batch.edges {
            let from_id = i64::try_from(edge.from.to_raw()).map_err(|_| {
                StorageError::Corpus(CorpusError::InvalidNodeId(edge.from.to_raw()))
            })?;
            let to_id = i64::try_from(edge.to.to_raw())
                .map_err(|_| StorageError::Corpus(CorpusError::InvalidNodeId(edge.to.to_raw())))?;
            tx.execute(
                "INSERT OR IGNORE INTO edges (from_node, to_node, edge_type) VALUES (?, ?, ?)",
                params![from_id, to_id, edge.edge_type.as_str()],
            )?;
        }

        // Typed fields: same replace-the-set semantics as the
        // live corpus write path, so idempotent refreshes converge.
        {
            let mut delete_fields =
                tx.prepare_cached("DELETE FROM node_fields WHERE node_id = ?")?;
            let mut insert_field = tx.prepare_cached(
                "INSERT INTO node_fields (node_id, name, kind, value_int, value_float, value_text)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )?;
            for node in &batch.nodes {
                let node_id = i64::try_from(node.id.to_raw()).map_err(|_| {
                    StorageError::Corpus(CorpusError::InvalidNodeId(node.id.to_raw()))
                })?;
                delete_fields.execute(params![node_id])?;
            }
            for field in &batch.fields {
                let node_id = i64::try_from(field.node_id.to_raw()).map_err(|_| {
                    StorageError::Corpus(CorpusError::InvalidNodeId(field.node_id.to_raw()))
                })?;
                let (kind, value_int, value_float, value_text) = match &field.value {
                    FieldValue::Str(s) => ("str", None, None, Some(s.as_str())),
                    FieldValue::Int(v) => ("int", Some(*v), None, None),
                    FieldValue::Float(v) => ("float", None, Some(*v), None),
                    FieldValue::Bool(v) => ("bool", Some(i64::from(*v)), None, None),
                    FieldValue::Date(v) => ("date", Some(*v), None, None),
                };
                insert_field.execute(params![
                    node_id,
                    field.name,
                    kind,
                    value_int,
                    value_float,
                    value_text
                ])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Commit staging to live corpus (atomic swap).
    pub fn commit_to(self, live_path: &Path) -> Result<()> {
        // Checkpoint WAL to ensure all data is flushed to main DB file before rename
        self.conn.execute_batch("PRAGMA wal_checkpoint(FULL)")?;

        // Atomic rename on POSIX, best-effort on Windows
        #[cfg(unix)]
        {
            fs::rename(&self.path, live_path)?;
        }
        #[cfg(windows)]
        {
            // On Windows, try replace_file or fallback to copy+delete
            if live_path.exists() {
                fs::remove_file(live_path)?;
            }
            fs::rename(&self.path, live_path)?;
        }
        Ok(())
    }

    /// Discard staging corpus.
    pub fn discard(self) -> Result<()> {
        if self.path.exists() {
            fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{Edge, EdgeType, Node, NodeKind};
    use crate::core::NodeId;
    use tempfile::tempdir;

    fn make_test_batch() -> IngestBatch {
        let n1 = NodeId::from_raw(1);
        let n2 = NodeId::from_raw(2);
        let n3 = NodeId::from_raw(3);

        IngestBatch {
            nodes: vec![
                Node {
                    id: n1,
                    label: "Google Search".to_string(),
                    kind: NodeKind::Text,
                },
                Node {
                    id: n2,
                    label: "Wikipedia Page".to_string(),
                    kind: NodeKind::Text,
                },
                Node {
                    id: n3,
                    label: "GitHub Repository".to_string(),
                    kind: NodeKind::Text,
                },
            ],
            edges: vec![
                Edge {
                    from: n1,
                    to: n2,
                    edge_type: EdgeType::new("hyperlink"),
                },
                Edge {
                    from: n2,
                    to: n3,
                    edge_type: EdgeType::new("citation"),
                },
            ],
            fields: vec![],
        }
    }

    #[test]
    fn corpus_fields_roundtrip() {
        let dir = tempdir().unwrap();
        let live = dir.path().join("fields.live");
        let staging_path = dir.path().join("fields.staging");

        let mut staging = StagingCorpus::create(&staging_path).unwrap();
        let mut b = crate::adapter::BatchBuilder::new();
        let n = b.add_node("contract");
        b.add_full_page_node("contract page");
        b.add_field(n, "expiry", FieldValue::Date(1_767_225_600));
        b.add_field(n, "status", FieldValue::Str("active".into()));
        b.add_field(n, "amount", FieldValue::Float(12_500.0));
        let batch = b.build();
        staging.write_batch(&batch, "fields").unwrap();
        staging.commit_to(&live).unwrap();
        let corpus = Corpus::open(&live).unwrap();

        // The get_nodes path preserves the real kind.
        let node = corpus.get_nodes(&[NodeId::from_raw(2)]).unwrap();
        assert_eq!(node[0].kind, NodeKind::FullPage, "kind survives get_nodes");

        // Fields read back per node.
        let fields = corpus.node_fields(NodeId::from_raw(1)).unwrap();
        assert_eq!(fields.len(), 3);
        let expiry = fields.iter().find(|f| f.name == "expiry").unwrap();
        assert_eq!(expiry.value, FieldValue::Date(1_767_225_600));

        // Fields read back by name.
        let values = corpus.field_values("status").unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].0, NodeId::from_raw(1));
        assert_eq!(values[0].1, FieldValue::Str("active".to_string()));

        // Idempotent re-write converges: writing a batch without the
        // "amount" field replaces the stored set.
        let mut b = crate::adapter::BatchBuilder::new();
        let n = b.add_node("contract");
        b.add_field(n, "expiry", FieldValue::Date(1_767_225_600));
        let mut staging = StagingCorpus::create(dir.path().join("fields2.staging")).unwrap();
        staging.write_batch(&b.build(), "fields").unwrap();
        let live2 = dir.path().join("fields2.live");
        staging.commit_to(&live2).unwrap();
        let corpus = Corpus::open(&live2).unwrap();
        let fields = corpus.node_fields(NodeId::from_raw(1)).unwrap();
        assert_eq!(fields.len(), 1, "re-write replaces the field set");
        assert_eq!(fields[0].name, "expiry");
    }

    #[test]
    fn corpus_migrates_v2_to_v3() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("migrate.corpus");

        // Simulate a v2 corpus: create fresh (v3), then drop the
        // node_fields table and rewind both version stamps.
        let corpus = Corpus::create(&path).unwrap();
        drop(corpus);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE node_fields;
             DELETE FROM schema_version;
             INSERT INTO schema_version (version) VALUES (2);
             PRAGMA user_version = 2;",
        )
        .unwrap();
        drop(conn);

        // Reopening migrates v2 -> v3 in place.
        let corpus = Corpus::open(&path).unwrap();
        assert_eq!(corpus.node_count().unwrap(), 0);

        // Fields are writable after migration.
        let mut b = crate::adapter::BatchBuilder::new();
        let n = b.add_node("contract");
        b.add_field(n, "status", FieldValue::Str("active".into()));
        let mut staging = StagingCorpus::create(dir.path().join("mig.staging")).unwrap();
        staging.write_batch(&b.build(), "mig").unwrap();
        let live = dir.path().join("mig.live");
        staging.commit_to(&live).unwrap();
        let corpus = Corpus::open(&live).unwrap();
        assert_eq!(corpus.field_values("status").unwrap().len(), 1);
    }

    #[test]
    fn corpus_create_and_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.corpus");

        // Create new corpus
        let _corpus = Corpus::create(&path).unwrap();
        assert!(path.exists());

        // Open existing corpus
        let corpus2 = Corpus::open(&path).unwrap();
        assert_eq!(corpus2.node_count().unwrap(), 0);
    }

    #[test]
    fn corpus_write_and_read_batch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.corpus");
        let mut corpus = Corpus::create(&path).unwrap();

        let batch = make_test_batch();
        corpus.write_batch(&batch, "web").unwrap();

        assert_eq!(corpus.node_count().unwrap(), 3);
        assert_eq!(corpus.edge_count().unwrap(), 2);

        // Read nodes back
        let n1 = NodeId::from_raw(1);
        let node = corpus.get_node(n1).unwrap().unwrap();
        assert_eq!(node.label, "Google Search");
        assert_eq!(node.kind, NodeKind::Text);

        // Read edges
        let outgoing = corpus.get_outgoing_edges(n1).unwrap();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].edge_type.as_str(), "hyperlink");
    }

    #[test]
    fn corpus_get_nodes_visible_window() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.corpus");
        let mut corpus = Corpus::create(&path).unwrap();

        let batch = make_test_batch();
        corpus.write_batch(&batch, "web").unwrap();

        let n1 = NodeId::from_raw(1);
        let n2 = NodeId::from_raw(2);
        let nodes = corpus.get_nodes(&[n1, n2]).unwrap();
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn corpus_fts5_search() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.corpus");
        let mut corpus = Corpus::create(&path).unwrap();

        let batch = make_test_batch();
        corpus.write_batch(&batch, "web").unwrap();

        // Search for "Google"
        let results = corpus.search_nodes("Google", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].label, "Google Search");

        // Search for "Page"
        let results = corpus.search_nodes("Page", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].label, "Wikipedia Page");

        // Search with no results
        let results = corpus.search_nodes("nonexistent", 10).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn corpus_edge_lookups() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.corpus");
        let mut corpus = Corpus::create(&path).unwrap();

        let batch = make_test_batch();
        corpus.write_batch(&batch, "web").unwrap();

        let n2 = NodeId::from_raw(2);

        // Outgoing from n2
        let outgoing = corpus.get_outgoing_edges(n2).unwrap();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].edge_type.as_str(), "citation");

        // Incoming to n2
        let incoming = corpus.get_incoming_edges(n2).unwrap();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].edge_type.as_str(), "hyperlink");
    }

    #[test]
    fn staging_corpus_create_write_commit() {
        let dir = tempdir().unwrap();
        let staging_path = dir.path().join("staging.corpus");
        let live_path = dir.path().join("live.corpus");

        // Create staging
        let mut staging = StagingCorpus::create(&staging_path).unwrap();

        // Write batch
        let batch = make_test_batch();
        staging.write_batch(&batch, "web").unwrap();

        // Commit to live
        staging.commit_to(&live_path).unwrap();

        // Verify live corpus
        let live = Corpus::open(&live_path).unwrap();
        assert_eq!(live.node_count().unwrap(), 3);
        assert_eq!(live.edge_count().unwrap(), 2);
    }

    #[test]
    fn staging_corpus_discard() {
        let dir = tempdir().unwrap();
        let staging_path = dir.path().join("staging.corpus");

        let staging = StagingCorpus::create(&staging_path).unwrap();
        staging.discard().unwrap();

        assert!(!staging_path.exists());
    }

    #[test]
    fn parameterized_queries_only() {
        // This test documents that we only use parameterized queries.
        // The implementation above uses ? placeholders exclusively.
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.corpus");
        let mut corpus = Corpus::create(&path).unwrap();

        let batch = make_test_batch();
        corpus.write_batch(&batch, "web").unwrap();

        // Verify with a query that uses parameters
        let n1 = NodeId::from_raw(1);
        let node = corpus.get_node(n1).unwrap().unwrap();
        assert_eq!(node.id, n1);
    }

    #[test]
    fn schema_version_enforced() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.corpus");

        // Create corpus with current schema
        Corpus::create(&path).unwrap();

        // Manually change user_version to simulate future schema
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 999").unwrap();
        }

        // Opening should fail with schema version mismatch
        let result = Corpus::open(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            StorageError::Corpus(CorpusError::SchemaVersion { expected, found }) => {
                assert_eq!(expected, SCHEMA_VERSION);
                assert_eq!(found, 999);
            }
            _ => panic!("Expected SchemaVersion error"),
        }
    }

    #[test]
    fn idempotent_write_batch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.corpus");
        let mut corpus = Corpus::create(&path).unwrap();

        let batch = make_test_batch();

        // Write once
        corpus.write_batch(&batch, "web").unwrap();
        assert_eq!(corpus.node_count().unwrap(), 3);

        // Write again (same IDs) - should be idempotent (INSERT OR REPLACE)
        corpus.write_batch(&batch, "web").unwrap();
        assert_eq!(corpus.node_count().unwrap(), 3);
        assert_eq!(corpus.edge_count().unwrap(), 2);
    }

    #[test]
    fn corpus_identity_and_version_gate() {
        // a corpus carries SQLite's application_id so foreign DB
        // files are rejected, and a foreign file's wrong id fails open.
        let dir = tempdir().expect("tempdir");
        let corpus_path = dir.path().join("ident.corpus");

        {
            let mut corpus = Corpus::create(&corpus_path).expect("create corpus");
            corpus
                .write_batch(&make_test_batch(), "test-adapter")
                .expect("write batch");
        }

        // Reopen: same application_id, schema version 1 -> accepted.
        {
            let corpus = Corpus::open(&corpus_path).expect("reopen own corpus");
            assert_eq!(corpus.node_count().expect("node count"), 3);
        }

        // A foreign SQLite file (different application_id) must be rejected.
        let foreign = dir.path().join("foreign.db");
        {
            let conn = rusqlite::Connection::open(&foreign).expect("open foreign");
            conn.execute_batch(
                "PRAGMA application_id = 12345;
                 CREATE TABLE unrelated (id INTEGER PRIMARY KEY, v TEXT);",
            )
            .expect("init foreign schema");
        }
        let err = Corpus::open(&foreign).expect_err("foreign file must be rejected");
        eprintln!("foreign-file rejection error: {err}");
    }

    #[test]
    fn search_hits_return_rank_and_snippet() {
        use crate::adapter::BatchBuilder;
        use std::path::PathBuf;

        let dir = std::env::temp_dir().join(format!(
            "aa-searchhits-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let live = PathBuf::from(format!("{}.live", dir.display()));
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        b.add_node("the quick brown fox");
        b.add_node("a slow lazy dog");
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");

        let hits = corpus.search_hits("fox", 10).expect("search");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].rank <= 0.0, "bm25 rank is negative, lower better");
        assert!(!hits[0].snippet.is_empty(), "snippet present");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&live);
    }

    #[test]
    fn page_nodes_iterates_all() {
        use crate::adapter::BatchBuilder;
        use std::path::PathBuf;

        let dir = std::env::temp_dir().join(format!(
            "aa-pagenodes-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let live = PathBuf::from(format!("{}.live", dir.display()));
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        for i in 0..7 {
            b.add_node(format!("node {i}"));
        }
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");

        let (page1, c1) = corpus.page_nodes(None, 3).expect("p1");
        assert_eq!(page1.len(), 3);
        let (page2, c2) = corpus.page_nodes(c1, 3).expect("p2");
        assert_eq!(page2.len(), 3);
        let (page3, c3) = corpus.page_nodes(c2, 3).expect("p3");
        assert_eq!(page3.len(), 1);
        // Keyset cursor = last id seen; the caller stops when a page
        // returns fewer than `limit`. Asking past the end yields nothing.
        let (empty, _) = corpus.page_nodes(c3, 3).expect("p4");
        assert!(empty.is_empty(), "past-the-end page is empty");

        // No overlap between pages.
        let mut seen = std::collections::HashSet::new();
        for n in page1.iter().chain(page2.iter()).chain(page3.iter()) {
            assert!(seen.insert(n.id), "duplicate id across pages");
        }

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&live);
    }

    #[test]
    fn backup_and_restore_roundtrip() {
        use crate::adapter::BatchBuilder;
        use std::path::PathBuf;

        let dir = std::env::temp_dir().join(format!(
            "aa-backup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let live = PathBuf::from(format!("{}.live", dir.display()));
        let backup = PathBuf::from(format!("{}.backup", dir.display()));

        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        b.add_node("original data");
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let mut corpus = Corpus::open(&live).expect("open");

        corpus.backup_to(&backup).expect("backup");

        // Mutate the live corpus, then restore. A fresh BatchBuilder
        // restarts ids at 1, so the second node must use a distinct
        // start id to be a new record (id 1 would upsert over it).
        let mut b2 = BatchBuilder::with_start_id(1000);
        b2.add_node("newer data");
        let batch2 = b2.build();
        corpus.write_batch(&batch2, "test").expect("write2");
        assert_eq!(corpus.node_count().expect("count"), 2);

        corpus.restore_from(&backup).expect("restore");
        assert_eq!(
            corpus.node_count().expect("count-after"),
            1,
            "restored to backup state"
        );
        let node = corpus.all_nodes().expect("all")[0].clone();
        assert_eq!(node.label, "original data");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&live);
        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn payload_compresses_and_roundtrips() {
        use crate::adapter::BatchBuilder;
        use std::path::PathBuf;

        let dir = std::env::temp_dir().join(format!(
            "aa-payload-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let live = PathBuf::from(format!("{}.live", dir.display()));
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        let id = b.add_node("contract");
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");

        // Long, repetitive document text — compresses well.
        let long = vec![b'a'; 50_000];
        corpus.set_payload(id, &long).expect("set payload");
        let got = corpus.payload(id).expect("read payload");
        assert_eq!(got.as_deref(), Some(long.as_slice()), "round-trip lossless");

        // Short payload fast path.
        corpus.set_payload(id, b"short").expect("set short");
        assert_eq!(
            corpus.payload(id).expect("read short").as_deref(),
            Some(&b"short"[..])
        );

        // Nonexistent node -> None.
        assert!(
            corpus
                .payload(NodeId::from_raw(9_999))
                .expect("missing")
                .is_none()
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&live);
    }

    #[test]
    fn full_page_kind_survives_roundtrip() {
        use crate::adapter::BatchBuilder;
        use std::path::PathBuf;

        let dir = std::env::temp_dir().join(format!(
            "aa-kind-roundtrip-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let live = PathBuf::from(format!("{}.live", dir.display()));

        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        b.add_full_page_node("page one");
        b.add_node("text node");
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");

        let corpus = Corpus::open(&live).expect("open");
        // Read the full-page node back and confirm the kind was preserved.
        let full_page = corpus.nodes_by_kind("full_page").expect("query full_page");
        assert_eq!(full_page.len(), 1, "one full_page node survives");
        assert_eq!(full_page[0].kind, NodeKind::FullPage);

        let text = corpus.nodes_by_kind("text").expect("query text");
        assert_eq!(text.len(), 1);
        assert_eq!(text[0].kind, NodeKind::Text);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&live);
    }
}
