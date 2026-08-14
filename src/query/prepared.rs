// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Prepared queries and plan profiles: parse once, execute many, and
//! inspect the estimated plan before running.

use crate::query::error::Result;
use crate::query::exec::{ExecOptions, execute};
use crate::query::ir::{QueryExpr, QueryResult};
use crate::storage::Corpus;
use std::time::Instant;

/// A parsed query, reusable across corpora and executions.
/// Parsing happens once at construction; [`PreparedQuery::execute`] runs
/// the stored IR against any corpus.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedQuery {
    expr: QueryExpr,
    /// The original text (for diagnostics).
    source: String,
}

impl PreparedQuery {
    /// Parse `text` into a prepared query.
    pub fn prepare(text: &str) -> Result<Self> {
        let expr = QueryExpr::parse(text)?;
        Ok(Self {
            expr,
            source: text.to_string(),
        })
    }

    /// The parsed expression.
    #[must_use]
    pub fn expr(&self) -> &QueryExpr {
        &self.expr
    }

    /// The original query text.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Execute against a corpus.
    pub fn execute(&self, corpus: &Corpus, opts: &ExecOptions) -> Result<QueryResult> {
        execute(corpus, &self.expr, opts)
    }

    /// A plan estimate (what the query will touch) without executing.
    pub fn explain(&self, corpus: &Corpus) -> Result<PlanProfile> {
        explain(corpus, &self.expr)
    }
}

/// A lightweight plan profile for a query.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanProfile {
    /// Estimated work categories the executor will perform.
    pub plan: Vec<PlanStep>,
}

/// One step in the estimated execution plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanStep {
    /// Full scan of the node table.
    FullScan,
    /// FTS5 full-text lookup.
    FtsLookup,
    /// Prefix lookup via FTS.
    PrefixLookup,
    /// Exact label match (indexed).
    LabelLookup,
    /// Typed-field comparison (indexed by field name).
    FieldLookup,
    /// Kind filter (indexed by kind column).
    KindLookup,
    /// Regular-expression scan over all labels.
    RegexScan,
    /// Typed-edge traversal (indexed by from-node).
    EdgeTraversal(String),
    /// Set intersection.
    Intersection,
    /// Set union.
    Union,
    /// Set difference (negation).
    Difference,
    /// Identity (matches everything).
    AllNodes,
}

impl std::fmt::Display for PlanStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FullScan => write!(f, "full-scan"),
            Self::FtsLookup => write!(f, "fts"),
            Self::PrefixLookup => write!(f, "prefix"),
            Self::LabelLookup => write!(f, "label"),
            Self::FieldLookup => write!(f, "field"),
            Self::KindLookup => write!(f, "kind"),
            Self::RegexScan => write!(f, "regex-scan"),
            Self::EdgeTraversal(t) => write!(f, "traverse:{t}"),
            Self::Intersection => write!(f, "and"),
            Self::Union => write!(f, "or"),
            Self::Difference => write!(f, "not"),
            Self::AllNodes => write!(f, "all"),
        }
    }
}

/// Estimate the execution plan for an expression (no execution).
pub fn explain(_corpus: &Corpus, expr: &QueryExpr) -> Result<PlanProfile> {
    let mut plan = Vec::new();
    collect_plan(expr, &mut plan);
    Ok(PlanProfile { plan })
}

fn collect_plan(expr: &QueryExpr, plan: &mut Vec<PlanStep>) {
    match expr {
        QueryExpr::All => plan.push(PlanStep::AllNodes),
        QueryExpr::Text(_) => plan.push(PlanStep::FtsLookup),
        QueryExpr::Prefix(_) => plan.push(PlanStep::PrefixLookup),
        QueryExpr::Kind(_) => plan.push(PlanStep::KindLookup),
        QueryExpr::FieldEquals { .. } => plan.push(PlanStep::LabelLookup),
        QueryExpr::FieldCmp { .. } => plan.push(PlanStep::FieldLookup),
        QueryExpr::Regex(_) => plan.push(PlanStep::RegexScan),
        QueryExpr::And(a, b) => {
            collect_plan(a, plan);
            collect_plan(b, plan);
            plan.push(PlanStep::Intersection);
        }
        QueryExpr::Or(a, b) => {
            collect_plan(a, plan);
            collect_plan(b, plan);
            plan.push(PlanStep::Union);
        }
        QueryExpr::Not(a) => {
            collect_plan(a, plan);
            plan.push(PlanStep::Difference);
        }
        QueryExpr::Traverse {
            inner,
            direction,
            edge_types,
            depth,
        } => {
            collect_plan(inner, plan);
            let arrow = match direction {
                crate::query::ir::TraverseDirection::Outgoing => "->",
                crate::query::ir::TraverseDirection::Incoming => "<-",
            };
            let types = if edge_types.len() == 1 {
                edge_types[0].clone()
            } else {
                format!("({})", edge_types.join("|"))
            };
            let spec = if *depth == 1 {
                format!("{arrow}{types}:")
            } else {
                format!("{arrow}{types}:{depth}:")
            };
            plan.push(PlanStep::EdgeTraversal(spec));
        }
    }
}

/// Execute with a timing profile attached.
#[derive(Debug, Clone, PartialEq)]
pub struct TimedResult {
    /// The query result.
    pub result: QueryResult,
    /// Elapsed time.
    pub elapsed: std::time::Duration,
}

/// Execute a prepared query and time it.
pub fn execute_timed(corpus: &Corpus, expr: &QueryExpr, opts: &ExecOptions) -> Result<TimedResult> {
    let start = Instant::now();
    let result = execute(corpus, expr, opts)?;
    Ok(TimedResult {
        result,
        elapsed: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::error::QueryError;

    #[test]
    fn prepare_parses_once() {
        let p = PreparedQuery::prepare("alice and kind:text").expect("prepare");
        assert_eq!(p.source(), "alice and kind:text");
        assert!(matches!(p.expr(), QueryExpr::And(_, _)));
    }

    #[test]
    fn prepared_query_rejects_bad_syntax() {
        let e = PreparedQuery::prepare("(alice");
        assert!(e.is_err());
        assert!(matches!(e, Err(QueryError::Parse { .. })));
    }

    #[test]
    fn explain_reports_plan() {
        let p = PreparedQuery::prepare("->parent:(alice or bob)").expect("prepare");
        // Executing requires a corpus; explain does not.
        // Build plan directly.
        let mut plan = Vec::new();
        collect_plan(p.expr(), &mut plan);
        assert!(plan.contains(&PlanStep::FtsLookup));
        assert!(plan.contains(&PlanStep::Union));
        assert!(plan.contains(&PlanStep::EdgeTraversal("->parent:".into())));
    }

    #[test]
    fn plan_steps_display() {
        assert_eq!(PlanStep::FtsLookup.to_string(), "fts");
        assert_eq!(
            PlanStep::EdgeTraversal("link".into()).to_string(),
            "traverse:link"
        );
    }

    #[test]
    fn timed_execution_runs() {
        use crate::adapter::BatchBuilder;
        use crate::storage::StagingCorpus;
        use std::path::PathBuf;

        let dir = std::env::temp_dir().join(format!(
            "aa-timed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let live = PathBuf::from(format!("{}.live", dir.display()));
        let mut staging = StagingCorpus::create(&dir).expect("staging");
        let mut b = BatchBuilder::new();
        b.add_node("alpha");
        b.add_node("beta");
        let batch = b.build();
        staging.write_batch(&batch, "test").expect("write");
        staging.commit_to(&live).expect("commit");
        let corpus = Corpus::open(&live).expect("open");

        let expr = QueryExpr::parse("alpha or beta").expect("parse");
        let timed = execute_timed(&corpus, &expr, &ExecOptions::default()).expect("timed");
        assert_eq!(timed.result.total.unwrap(), 2);
        assert!(!timed.elapsed.is_zero() || timed.result.len() == 2);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&live);
    }
}
