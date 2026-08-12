// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Ordered, idempotent schema migrations. The runner applies schema
//! changes in version order inside transactions, recording applied
//! versions in `schema_version`; re-opening an older corpus upgrades it
//! in place.

use crate::storage::error::Result;
use rusqlite::{Connection, params};

/// A schema migration: `from` the schema version before it runs, applying
/// `sql` (and optional `name` for logging).
pub struct Migration {
    /// The schema version this migration *produces*.
    pub to_version: u32,
    /// Human-readable name (logs, errors).
    pub name: &'static str,
    /// The SQL to run, wrapped in a transaction by the runner.
    pub sql: &'static str,
}

/// The migrations the engine knows about, in application order:
/// schema v2 added the unique edge index; schema v3 added the typed
/// node-fields table. The table is authoritative: appending a migration
/// bumps [`SCHEMA_VERSION`] and adds an entry here.
pub fn migrations() -> Vec<Migration> {
    vec![
        Migration {
            to_version: 2,
            name: "v2: unique edge index for idempotent refresh",
            sql: "CREATE UNIQUE INDEX IF NOT EXISTS idx_edges_unique
                  ON edges(from_node, to_node, edge_type)",
        },
        Migration {
            to_version: 3,
            name: "v3: typed node fields (form-declared semantics)",
            sql: "CREATE TABLE IF NOT EXISTS node_fields (
                    node_id     INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                    name        TEXT NOT NULL,
                    kind        TEXT NOT NULL CHECK (kind IN ('str','int','float','bool','date')),
                    value_int   INTEGER,
                    value_float REAL,
                    value_text  TEXT,
                    PRIMARY KEY (node_id, name)
                  );
                  CREATE INDEX IF NOT EXISTS idx_node_fields_name ON node_fields(name)",
        },
    ]
}

/// Current schema version after all known migrations.
pub const SCHEMA_VERSION: u32 = 3;

/// Apply all pending migrations to `conn`.
/// Idempotent: applied versions are read from `schema_version` first.
/// Each migration runs inside its own transaction; a failure leaves the
/// schema at the last successful version.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    // Read the current applied version.
    let current: i64 = conn.query_row(
        "SELECT version FROM schema_version ORDER BY version DESC LIMIT 1",
        [],
        |row| row.get(0),
    )?;

    for migration in migrations() {
        if i64::from(migration.to_version) <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO schema_version (version, applied) VALUES (?, strftime('%s', 'now'))",
            params![migration.to_version],
        )?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("memory conn");
        conn.execute_batch(
            "CREATE TABLE nodes (id INTEGER PRIMARY KEY, label TEXT);
             CREATE TABLE edges (
                 id INTEGER PRIMARY KEY,
                 from_node INTEGER,
                 to_node INTEGER,
                 edge_type TEXT
             );
             CREATE TABLE schema_version (
                 version INTEGER PRIMARY KEY,
                 applied INTEGER
             );
             INSERT INTO schema_version (version) VALUES (1);",
        )
        .expect("fixture schema");
        conn
    }

    #[test]
    fn applies_pending_migration() {
        let mut conn = fresh_conn();
        migrate(&mut conn).expect("migrate");
        // The unique index now exists.
        let idx_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_edges_unique'",
                [],
                |r| r.get(0),
            )
            .expect("index check");
        assert_eq!(idx_count, 1);
        // And the v3 node-fields table exists.
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='node_fields'",
                [],
                |r| r.get(0),
            )
            .expect("table check");
        assert_eq!(table_count, 1);
        // And schema_version records v3.
        let v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(v, 3);
    }

    #[test]
    fn is_idempotent_on_second_run() {
        let mut conn = fresh_conn();
        migrate(&mut conn).expect("first");
        migrate(&mut conn).expect("second");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 3, "v1 fixture + v2 + v3 applied exactly once");
    }

    #[test]
    fn skips_when_already_current() {
        let mut conn = fresh_conn();
        conn.execute("INSERT INTO schema_version (version) VALUES (2)", [])
            .expect("pre-applied");
        migrate(&mut conn).expect("migrate no-op");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_version WHERE version=2",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(count, 1, "no duplicate v2 row");
    }
}
