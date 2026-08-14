// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Typed query IR — the intermediate representation between the parser
//! and the executor.

use crate::adapter::FieldValue;
use serde::{Deserialize, Serialize};

/// Comparison operator for typed-field queries (`field:name op value`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldOp {
    /// `=` — equal.
    Eq,
    /// `!=` — not equal.
    Ne,
    /// `>` — strictly greater.
    Gt,
    /// `>=` — greater or equal.
    Gte,
    /// `<` — strictly less.
    Lt,
    /// `<=` — less or equal.
    Lte,
}

impl std::fmt::Display for FieldOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eq => write!(f, "="),
            Self::Ne => write!(f, "!="),
            Self::Gt => write!(f, ">"),
            Self::Gte => write!(f, ">="),
            Self::Lt => write!(f, "<"),
            Self::Lte => write!(f, "<="),
        }
    }
}

/// Traversal direction over typed edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraverseDirection {
    /// Follow edges from source to destination (`->`).
    Outgoing,
    /// Follow edges from destination to source (`<-`).
    Incoming,
}

/// A parsed, typed query expression.
/// The IR is deliberately small: it covers the operations the corpus
/// actually supports (full-text, prefix, field, kind, edge traversal) and
/// leaves room for extension without breaking the public shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QueryExpr {
    /// Match every node (used for `*` / empty query).
    All,
    /// Full-text match against the search index.
    Text(String),
    /// Prefix match against the label.
    Prefix(String),
    /// Label field equality (`field:value` on labels) — reachable only
    /// through the typed API. The query language refuses unknown
    /// selectors (labels carry no structured fields), reserving
    /// `kind:`, `re:`, `prefix:`, and `text:`.
    FieldEquals {
        /// The selector before the colon.
        field: String,
        /// The value the label must equal exactly.
        value: String,
    },
    /// Typed-field comparison (`field:name op value`).
    /// Semantics: nodes that carry a field `name` whose value compares
    /// to `value` with `op`. Nodes without the field never match.
    FieldCmp {
        /// The typed field's name.
        field: String,
        /// The comparison operator.
        op: FieldOp,
        /// The literal to compare stored values against.
        value: FieldValue,
    },
    /// Node kind match (`kind:text` / `kind:full_page`).
    Kind(String),
    /// A regular-expression match on the label.
    Regex(String),
    /// Logical conjunction.
    And(Box<QueryExpr>, Box<QueryExpr>),
    /// Logical disjunction.
    Or(Box<QueryExpr>, Box<QueryExpr>),
    /// Logical negation.
    Not(Box<QueryExpr>),
    /// Traverse from matched nodes along typed edges (`->type:(...)`,
    /// `<-type:(...)`, with an optional depth `->type:N:(...)` and an
    /// optional type union `->(a|b):(...)`).
    /// Semantics: the set of nodes reachable from the inner match in at
    /// most `depth` hops (each hop following an edge whose type is in
    /// `edge_types`, in the given direction). The starting match set
    /// itself is not part of the result. Depth 1 is the single-hop
    /// traversal of earlier versions.
    Traverse {
        /// The match set to traverse out of.
        inner: Box<QueryExpr>,
        /// Which way to follow the edges.
        direction: TraverseDirection,
        /// The edge types to follow (at least one).
        edge_types: Vec<String>,
        /// Maximum number of hops (at least 1).
        depth: usize,
    },
}

impl QueryExpr {
    /// Parse a query string into the IR.
    pub fn parse(text: &str) -> crate::query::Result<Self> {
        crate::query::parser::Parser::new(text).parse()
    }

    /// True when the expression can match no nodes by construction
    /// (e.g. `not all`).
    pub fn is_impossible(&self) -> bool {
        match self {
            Self::Not(inner) => matches!(**inner, Self::All),
            Self::And(a, b) => a.is_impossible() || b.is_impossible(),
            Self::Or(a, b) => a.is_impossible() && b.is_impossible(),
            Self::Text(t) => t.is_empty(),
            Self::Prefix(p) => p.is_empty(),
            _ => false,
        }
    }

    /// A short canonical render (useful for logs and `explain`).
    pub fn display(&self) -> String {
        match self {
            Self::All => "*".to_string(),
            Self::Text(t) => format!("text:{t}"),
            Self::Prefix(p) => format!("prefix:{p}"),
            Self::FieldEquals { field, value } => format!("{field}:{value}"),
            Self::FieldCmp { field, op, value } => format!("field:{field} {op} {value}"),
            Self::Kind(k) => format!("kind:{k}"),
            Self::Regex(r) => format!("re:{r}"),
            Self::And(a, b) => format!("({} and {})", a.display(), b.display()),
            Self::Or(a, b) => format!("({} or {})", a.display(), b.display()),
            Self::Not(a) => format!("not {}", a.display()),
            Self::Traverse {
                inner,
                direction,
                edge_types,
                depth,
            } => {
                let arrow = match direction {
                    TraverseDirection::Outgoing => "->",
                    TraverseDirection::Incoming => "<-",
                };
                let types = if edge_types.len() == 1 {
                    edge_types[0].clone()
                } else {
                    format!("({})", edge_types.join("|"))
                };
                let hops = if *depth == 1 {
                    String::new()
                } else {
                    format!("{depth}:")
                };
                format!("{arrow}{types}:{hops}({})", inner.display())
            }
        }
    }
}

/// A paginated result set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    /// Matching node ids in deterministic order.
    pub node_ids: Vec<crate::core::NodeId>,
    /// Total matches before pagination (when known). Always known for
    /// the query language: computed exactly, never from a truncated
    /// result set.
    pub total: Option<u64>,
    /// Keyset cursor: pass back as `after` on the next page (id-ordered
    /// results).
    pub next_after: Option<crate::core::NodeId>,
    /// Rank cursor for rank-ordered results: pass back as `after_rank`
    /// together with `next_after` on the next page. `None` unless the
    /// query was executed with
    /// [`crate::query::exec::ResultOrder::ByRank`] over a full-text or
    /// prefix atom.
    pub next_after_rank: Option<f64>,
    /// bm25 rank per returned node (lower = better), aligned with
    /// `node_ids`. `Some` only for rank-ordered full-text / prefix
    /// queries; `None` otherwise.
    pub ranks: Option<Vec<f64>>,
}

impl QueryResult {
    /// True when no nodes matched.
    pub fn is_empty(&self) -> bool {
        self.node_ids.is_empty()
    }

    /// Number of returned matches on this page.
    pub fn len(&self) -> usize {
        self.node_ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_roundtrip_readable() {
        let e = QueryExpr::parse("alice and kind:text").expect("parse");
        let rendered = e.display();
        assert_eq!(rendered, "(text:alice and kind:text)");
        // The canonical render parses back to the same expression.
        assert_eq!(QueryExpr::parse(&rendered).expect("re-parse"), e);
    }

    #[test]
    fn traverse_display_roundtrips() {
        for text in [
            "->parent:(alice)",
            "<-parent:(alice)",
            "->a:3:(alice)",
            "<-(a|b):2:(alice)",
        ] {
            let e = QueryExpr::parse(text).expect("parse");
            assert_eq!(QueryExpr::parse(&e.display()).expect("re-parse"), e);
        }
    }

    #[test]
    fn impossible_detection() {
        assert!(QueryExpr::Not(Box::new(QueryExpr::All)).is_impossible());
        assert!(!QueryExpr::Text("x".into()).is_impossible());
    }

    #[test]
    fn serde_roundtrip() {
        let e = QueryExpr::parse("a or (->parent:(b))").expect("parse");
        let json = serde_json::to_string(&e).expect("serialize");
        let back: QueryExpr = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(e, back);
    }
}
