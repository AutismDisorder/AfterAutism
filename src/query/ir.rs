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
    /// Label field equality (`field:value` on labels).
    FieldEquals { field: String, value: String },
    /// Typed-field comparison (`field:name op value`).
    /// Semantics: nodes that carry a field `name` whose value compares
    /// to `value` with `op`. Nodes without the field never match.
    FieldCmp {
        field: String,
        op: FieldOp,
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
    /// Traverse from matched nodes along an edge type (`->type:`).
    /// Semantics: nodes reachable from the inner match in one hop along
    /// edges of the given type.
    Traverse {
        inner: Box<QueryExpr>,
        edge_type: String,
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
            Self::Traverse { inner, edge_type } => {
                format!("({} ->{edge_type}:)", inner.display())
            }
        }
    }
}

/// A paginated result set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    /// Matching node ids in deterministic order.
    pub node_ids: Vec<crate::core::NodeId>,
    /// Total matches before pagination (when known).
    pub total: Option<u64>,
    /// Keyset cursor: pass back as `after` on the next page.
    pub next_after: Option<crate::core::NodeId>,
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
        assert_eq!(e.display(), "(text:alice and kind:text)");
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
