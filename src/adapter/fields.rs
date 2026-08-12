// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Typed node fields — the form-declared semantics of the engine.
//! Adapters attach typed values ([`FieldValue`]) to nodes; the storage
//! layer persists them and the query language compares them
//! (`field:name op value`).

use crate::core::NodeId;
use serde::{Deserialize, Serialize};

/// A typed value attached to a node by an adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FieldValue {
    /// UTF-8 text. Ordered lexicographically.
    Str(String),
    /// Signed 64-bit integer.
    Int(i64),
    /// IEEE-754 double.
    Float(f64),
    /// Boolean.
    Bool(bool),
    /// Unix seconds (UTC). Query literals use `YYYY-MM-DD`.
    Date(i64),
}

impl FieldValue {
    /// Parse a query literal: integers, floats, `YYYY-MM-DD` dates,
    /// `true`/`false`, and everything else as a string.
    #[must_use]
    pub fn from_literal(s: &str) -> Self {
        if let Ok(v) = s.parse::<i64>() {
            return Self::Int(v);
        }
        if let Ok(v) = s.parse::<f64>() {
            return Self::Float(v);
        }
        if let Some(secs) = parse_date_seconds(s) {
            return Self::Date(secs);
        }
        match s {
            "true" => return Self::Bool(true),
            "false" => return Self::Bool(false),
            _ => {}
        }
        Self::Str(s.to_string())
    }

    /// True for numeric kinds (int, float, date).
    #[must_use]
    pub fn is_numeric(&self) -> bool {
        matches!(self, Self::Int(_) | Self::Float(_) | Self::Date(_))
    }
}

impl std::fmt::Display for FieldValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Str(s) => write!(f, "{s}"),
            Self::Int(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Bool(v) => write!(f, "{v}"),
            Self::Date(v) => match date_display(*v) {
                Some(s) => write!(f, "{s}"),
                None => write!(f, "date({v})"),
            },
        }
    }
}

/// One typed field attached to a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeField {
    /// The node this field belongs to.
    pub node_id: NodeId,
    /// Field name (e.g. `"expiry"`, `"status"`, `"amount"`).
    pub name: String,
    /// The typed value.
    pub value: FieldValue,
}

/// Parse a `YYYY-MM-DD` date literal to unix seconds (UTC, naive).
fn parse_date_seconds(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let year: i64 = parts[0].parse().ok()?;
    let month: i64 = parts[1].parse().ok()?;
    let day: i64 = parts[2].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400)
}

/// Render unix seconds as `YYYY-MM-DD` (UTC, naive). `None` for dates
/// before the epoch — a representable failure, not an invariant
/// violation.
#[allow(clippy::unnecessary_wraps)] // pre-epoch dates are representable failures
fn date_display(secs: i64) -> Option<String> {
    let days = secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days)?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
/// Total for all i64 inputs — no failure mode.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400); // [0, 399]
    let mp = (m + 9).rem_euclid(12); // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Civil date for days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> Option<(i64, i64, i64)> {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_literal_roundtrips() {
        let secs = parse_date_seconds("2026-01-01").expect("date parses");
        assert_eq!(secs, 1_767_225_600);
        assert_eq!(date_display(secs).as_deref(), Some("2026-01-01"));
        // Negative (pre-1970) still roundtrips.
        let secs = parse_date_seconds("1969-07-20").expect("date parses");
        assert_eq!(date_display(secs).as_deref(), Some("1969-07-20"));
    }

    #[test]
    fn date_literal_rejects_garbage() {
        assert!(parse_date_seconds("2026-13-01").is_none());
        assert!(parse_date_seconds("2026-00-10").is_none());
        assert!(parse_date_seconds("not-a-date").is_none());
        assert!(parse_date_seconds("20260101").is_none());
    }

    #[test]
    fn literal_kinds_are_detected() {
        assert_eq!(FieldValue::from_literal("42"), FieldValue::Int(42));
        assert_eq!(FieldValue::from_literal("4.5"), FieldValue::Float(4.5));
        assert_eq!(
            FieldValue::from_literal("2026-01-01"),
            FieldValue::Date(1_767_225_600)
        );
        assert_eq!(FieldValue::from_literal("true"), FieldValue::Bool(true));
        assert_eq!(
            FieldValue::from_literal("active"),
            FieldValue::Str("active".to_string())
        );
    }

    #[test]
    fn display_renders_dates_and_scalars() {
        assert_eq!(FieldValue::Date(1_767_225_600).to_string(), "2026-01-01");
        assert_eq!(FieldValue::Int(7).to_string(), "7");
        assert_eq!(FieldValue::Str("x".into()).to_string(), "x");
        assert_eq!(FieldValue::Bool(false).to_string(), "false");
    }

    #[test]
    fn numeric_detection() {
        assert!(FieldValue::Int(1).is_numeric());
        assert!(FieldValue::Float(1.0).is_numeric());
        assert!(FieldValue::Date(1).is_numeric());
        assert!(!FieldValue::Str("x".into()).is_numeric());
        assert!(!FieldValue::Bool(true).is_numeric());
    }
}
