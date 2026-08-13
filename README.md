# afterautism

Access to data. Adapters turn any filetype into a typed, versioned,
queryable corpus; one deterministic query language retrieves exactly the
right items — by full-text, structured fields, or typed-edge traversal.

```toml
[dependencies]
afterautism = "0.0.2"
```

```rust,no_run
use afterautism::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Build a typed batch: nodes, typed edges, typed fields.
    let mut b = BatchBuilder::new();
    let contract = b.add_node("acme master agreement");
    b.add_field(contract, "status", FieldValue::Str("active".into()));
    b.add_field(contract, "expiry", FieldValue::Date(1_767_225_600)); // 2026-01-01
    let amendment = b.add_node("acme amendment 1");
    b.add_edge(contract, amendment, EdgeType::new("amendment"));

    // Commit atomically into a versioned corpus.
    let mut staging = StagingCorpus::create("corpus.db")?;
    staging.write_batch(&b.build(), "demo")?;
    let live = std::path::Path::new("corpus.live");
    staging.commit_to(live)?;
    let corpus = Corpus::open(live)?;

    // The right items out.
    let res = afterautism::query::query(
        &corpus,
        "field:status = active and field:expiry > 2026-01-01",
        &afterautism::query::ExecOptions::default(),
    )?;
    println!("matched {}", res.total.unwrap_or(0));
    Ok(())
}
```

## The engine

- **Input surface** — the `Adapter` contract: implementations declare
  the typed model (nodes, typed edges, typed fields) of any filetype.
  Format adapters live in consuming workspaces, not in the engine.
- **Corpus** — versioned, migratable storage (SQLite + FTS5): atomic
  staging commits, schema migrations, compressed payloads, backup and
  restore, deterministic reads, safe multi-process access (WAL with a
  bounded lock wait — no configuration).
- **Query** — one language over text + fields + graph: full-text with
  bm25 ranking, prefix, regex, `field:name op value` comparisons
  (strings, numbers, dates, booleans), `->edge_type:(...)` traversal,
  boolean composition, keyset pagination.
- **Topology** — typed-edge filtering as a visual transform (emphasis
  masks) and graph algorithms over the visible subgraph.
- **Safety** — offline by default (`NetworkGate`); hard refusals on
  malformed input; decompression bomb guards; schema versions that
  cannot silently drift.

## License

AGPL-3.0-or-later
