// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Recursive-descent parser for the query language.
//! Grammar (informal):
//! ```text
//! expr := or
//! or := and ( ("or" | "||") and )*
//! and := unary ( ("and" | "&&") unary )*
//! unary := "not" unary | primary
//! primary := "(" expr ")" | atom
//! atom := "*"
//! | term
//! | field ":" value (kind: / field:)
//! | "->" ident ":" "(" expr ")" (traversal)
//! | "re" ":" quoted (regex)
//! | quoted
//! term := bare word or quoted string
//! ```

use crate::adapter::FieldValue;
use crate::query::error::{QueryError, Result};
use crate::query::ir::{FieldOp, QueryExpr};

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    LParen,
    RParen,
    Colon,
    Arrow, // ->
    And,
    Or,
    Not,
    Star,
    Eq,  // =
    Ne,  // !=
    Gt,  // >
    Gte, // >=
    Lt,  // <
    Lte, // <=
    End,
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            chars: s.chars().peekable(),
            pos: 0,
        }
    }

    fn bump(&mut self) -> Option<char> {
        self.pos += 1;
        self.chars.next()
    }

    fn peek(&mut self) -> Option<&char> {
        self.chars.peek()
    }

    /// Tokenize the whole input up front (one pass, no backtracking).
    /// A long char match, but each arm is a tiny, independent rule —
    /// splitting it would scatter the lexer grammar.
    #[allow(clippy::too_many_lines)]
    fn tokenize(&mut self) -> Result<Vec<(Tok, usize)>> {
        let mut out = Vec::new();
        loop {
            // skip whitespace
            while matches!(self.peek(), Some(c) if c.is_whitespace()) {
                self.bump();
            }
            let start = self.pos;
            let Some(c) = self.peek().copied() else {
                out.push((Tok::End, self.pos));
                return Ok(out);
            };
            match c {
                '(' => {
                    self.bump();
                    out.push((Tok::LParen, start));
                }
                ')' => {
                    self.bump();
                    out.push((Tok::RParen, start));
                }
                ':' => {
                    self.bump();
                    out.push((Tok::Colon, start));
                }
                '*' => {
                    self.bump();
                    out.push((Tok::Star, start));
                }
                '-' => {
                    self.bump();
                    if self.peek() == Some(&'>') {
                        self.bump();
                        out.push((Tok::Arrow, start));
                    } else {
                        return Err(QueryError::parse(start, "expected '->'"));
                    }
                }
                '=' => {
                    self.bump();
                    out.push((Tok::Eq, start));
                }
                '!' => {
                    self.bump();
                    if self.peek() == Some(&'=') {
                        self.bump();
                        out.push((Tok::Ne, start));
                    } else {
                        return Err(QueryError::parse(start, "expected '!='"));
                    }
                }
                '>' => {
                    self.bump();
                    if self.peek() == Some(&'=') {
                        self.bump();
                        out.push((Tok::Gte, start));
                    } else {
                        out.push((Tok::Gt, start));
                    }
                }
                '<' => {
                    self.bump();
                    if self.peek() == Some(&'=') {
                        self.bump();
                        out.push((Tok::Lte, start));
                    } else {
                        out.push((Tok::Lt, start));
                    }
                }
                '"' | '\'' => {
                    self.bump();
                    let mut s = String::new();
                    loop {
                        match self.bump() {
                            Some(ch) if ch == c => break,
                            Some(ch) => s.push(ch),
                            None => {
                                return Err(QueryError::parse(start, "unterminated string"));
                            }
                        }
                    }
                    out.push((Tok::Str(s), start));
                }
                '&' => {
                    self.bump();
                    if self.peek() == Some(&'&') {
                        self.bump();
                        out.push((Tok::And, start));
                    } else {
                        return Err(QueryError::parse(start, "expected '&&'"));
                    }
                }
                '|' => {
                    self.bump();
                    if self.peek() == Some(&'|') {
                        self.bump();
                        out.push((Tok::Or, start));
                    } else {
                        return Err(QueryError::parse(start, "expected '||'"));
                    }
                }
                _ if c.is_alphanumeric() || c == '_' => {
                    let mut s = String::new();
                    while let Some(ch) = self.peek().copied() {
                        if ch.is_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
                            s.push(ch);
                            self.bump();
                        } else {
                            break;
                        }
                    }
                    let tok = match s.as_str() {
                        "and" => Tok::And,
                        "or" => Tok::Or,
                        "not" => Tok::Not,
                        _ => Tok::Ident(s),
                    };
                    out.push((tok, start));
                }
                _ => {
                    return Err(QueryError::parse(
                        start,
                        format!("unexpected character '{c}'"),
                    ));
                }
            }
        }
    }
}

pub struct Parser {
    toks: Vec<(Tok, usize)>,
    idx: usize,
    /// Lexer error captured at construction; surfaced on [`Parser::parse`].
    lex_error: Option<QueryError>,
}

impl Parser {
    pub fn new(text: &str) -> Self {
        match Lexer::new(text).tokenize() {
            Ok(toks) => Self {
                toks,
                idx: 0,
                lex_error: None,
            },
            Err(e) => Self {
                toks: vec![(Tok::End, 0)],
                idx: 0,
                lex_error: Some(e),
            },
        }
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.idx].0
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.idx].0.clone();
        if self.idx + 1 < self.toks.len() {
            self.idx += 1;
        }
        t
    }

    fn pos(&self) -> usize {
        self.toks[self.idx].1
    }

    pub fn parse(&mut self) -> Result<QueryExpr> {
        if let Some(e) = &self.lex_error {
            return Err(e.clone());
        }
        let e = self.parse_or()?;
        if !matches!(self.peek(), Tok::End) {
            return Err(QueryError::parse(self.pos(), "trailing input"));
        }
        Ok(e)
    }

    fn parse_or(&mut self) -> Result<QueryExpr> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Tok::Or) {
            self.bump();
            let right = self.parse_and()?;
            left = QueryExpr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<QueryExpr> {
        let mut left = self.parse_unary()?;
        while matches!(self.peek(), Tok::And) {
            self.bump();
            let right = self.parse_unary()?;
            left = QueryExpr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<QueryExpr> {
        if matches!(self.peek(), Tok::Not) {
            self.bump();
            let inner = self.parse_unary()?;
            return Ok(QueryExpr::Not(Box::new(inner)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<QueryExpr> {
        // Traversal: -> edge_type : (expr)
        if matches!(self.peek(), Tok::Arrow) {
            self.bump();
            let Tok::Ident(edge_type) = self.bump() else {
                return Err(QueryError::parse(
                    self.pos(),
                    "expected edge type after '->'",
                ));
            };
            if !matches!(self.peek(), Tok::Colon) {
                return Err(QueryError::parse(
                    self.pos(),
                    "expected ':' after edge type",
                ));
            }
            self.bump();
            if !matches!(self.peek(), Tok::LParen) {
                return Err(QueryError::parse(
                    self.pos(),
                    "expected '(' after '->type:'",
                ));
            }
            self.bump();
            let inner = self.parse_or()?;
            if !matches!(self.peek(), Tok::RParen) {
                return Err(QueryError::parse(self.pos(), "expected ')'"));
            }
            self.bump();
            return Ok(QueryExpr::Traverse {
                inner: Box::new(inner),
                edge_type,
            });
        }

        match self.bump() {
            Tok::LParen => {
                let inner = self.parse_or()?;
                if !matches!(self.peek(), Tok::RParen) {
                    return Err(QueryError::parse(self.pos(), "expected ')'"));
                }
                self.bump();
                Ok(inner)
            }
            Tok::Star => Ok(QueryExpr::All),
            Tok::Str(s) => Ok(QueryExpr::Text(s)),
            Tok::Ident(s) => {
                // `field:name op value` — typed-field comparison.
                // Otherwise `ident:value` is a field equality (`kind:`,
                // `re:`, or label equality); a bare ident is a text term.
                if s == "field" && matches!(self.peek(), Tok::Colon) {
                    self.bump(); // ':'
                    let Tok::Ident(name) = self.bump() else {
                        return Err(QueryError::parse(
                            self.pos(),
                            "expected field name after 'field:'",
                        ));
                    };
                    let op = match self.bump() {
                        Tok::Eq => FieldOp::Eq,
                        Tok::Ne => FieldOp::Ne,
                        Tok::Gt => FieldOp::Gt,
                        Tok::Gte => FieldOp::Gte,
                        Tok::Lt => FieldOp::Lt,
                        Tok::Lte => FieldOp::Lte,
                        _ => {
                            return Err(QueryError::parse(
                                self.pos(),
                                "expected comparison operator (=, !=, >, >=, <, <=)",
                            ));
                        }
                    };
                    let value = match self.bump() {
                        Tok::Str(v) => FieldValue::Str(v),
                        Tok::Ident(v) => FieldValue::from_literal(&v),
                        _ => {
                            return Err(QueryError::parse(self.pos(), "expected field value"));
                        }
                    };
                    Ok(QueryExpr::FieldCmp {
                        field: name,
                        op,
                        value,
                    })
                } else if matches!(self.peek(), Tok::Colon) {
                    self.bump();
                    match self.bump() {
                        Tok::Str(v) | Tok::Ident(v) => match s.as_str() {
                            "kind" => Ok(QueryExpr::Kind(v)),
                            "re" => Ok(QueryExpr::Regex(v)),
                            _ => Ok(QueryExpr::FieldEquals { field: s, value: v }),
                        },
                        _ => Err(QueryError::parse(self.pos(), "expected value after ':'")),
                    }
                } else {
                    Ok(QueryExpr::Text(s))
                }
            }
            Tok::End => Err(QueryError::parse(self.pos(), "unexpected end of query")),
            other => Err(QueryError::parse(
                self.pos(),
                format!("unexpected token {other:?}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> QueryExpr {
        Parser::new(s).parse().expect("parse")
    }

    #[test]
    fn bare_term_is_text() {
        assert_eq!(p("alice"), QueryExpr::Text("alice".into()));
    }

    #[test]
    fn quoted_phrase_is_text() {
        assert_eq!(p("\"hello world\""), QueryExpr::Text("hello world".into()));
    }

    #[test]
    fn and_chain() {
        assert_eq!(
            p("a and b and c"),
            QueryExpr::And(
                Box::new(QueryExpr::And(
                    Box::new(QueryExpr::Text("a".into())),
                    Box::new(QueryExpr::Text("b".into())),
                )),
                Box::new(QueryExpr::Text("c".into())),
            )
        );
    }

    #[test]
    fn or_precedence() {
        // a or b and c == a or (b and c)
        let e = p("a or b and c");
        assert!(matches!(e, QueryExpr::Or(_, _)));
        if let QueryExpr::Or(l, r) = e {
            assert!(matches!(*l, QueryExpr::Text(_)));
            assert!(matches!(*r, QueryExpr::And(_, _)));
        }
    }

    #[test]
    fn not_negates() {
        assert_eq!(
            p("not alice"),
            QueryExpr::Not(Box::new(QueryExpr::Text("alice".into())))
        );
    }

    #[test]
    fn kind_field() {
        assert_eq!(p("kind:text"), QueryExpr::Kind("text".into()));
    }

    #[test]
    fn traversal() {
        assert_eq!(
            p("->parent:(alice)"),
            QueryExpr::Traverse {
                inner: Box::new(QueryExpr::Text("alice".into())),
                edge_type: "parent".into(),
            }
        );
    }

    #[test]
    fn empty_query_is_all() {
        assert_eq!(p("*"), QueryExpr::All);
    }

    #[test]
    fn unmatched_paren_errors() {
        let e = Parser::new("(a").parse();
        assert!(e.is_err());
    }

    #[test]
    fn symbols_and_or() {
        assert!(matches!(p("a && b"), QueryExpr::And(_, _)));
        assert!(matches!(p("a || b"), QueryExpr::Or(_, _)));
    }

    #[test]
    fn field_cmp_eq_string() {
        assert_eq!(
            p("field:status = active"),
            QueryExpr::FieldCmp {
                field: "status".into(),
                op: FieldOp::Eq,
                value: FieldValue::Str("active".into()),
            }
        );
    }

    #[test]
    fn field_cmp_quoted_string() {
        assert_eq!(
            p("field:status = \"in review\""),
            QueryExpr::FieldCmp {
                field: "status".into(),
                op: FieldOp::Eq,
                value: FieldValue::Str("in review".into()),
            }
        );
    }

    #[test]
    fn field_cmp_numeric_ops() {
        assert_eq!(
            p("field:amount >= 50000"),
            QueryExpr::FieldCmp {
                field: "amount".into(),
                op: FieldOp::Gte,
                value: FieldValue::Int(50_000),
            }
        );
        assert_eq!(
            p("field:amount < 4.5"),
            QueryExpr::FieldCmp {
                field: "amount".into(),
                op: FieldOp::Lt,
                value: FieldValue::Float(4.5),
            }
        );
        assert!(matches!(
            p("field:amount != 0"),
            QueryExpr::FieldCmp {
                op: FieldOp::Ne,
                ..
            }
        ));
    }

    #[test]
    fn field_cmp_date_literal() {
        assert_eq!(
            p("field:expiry > 2026-01-01"),
            QueryExpr::FieldCmp {
                field: "expiry".into(),
                op: FieldOp::Gt,
                value: FieldValue::Date(1_767_225_600),
            }
        );
    }

    #[test]
    fn field_cmp_bool_literal() {
        assert_eq!(
            p("field:renewed = true"),
            QueryExpr::FieldCmp {
                field: "renewed".into(),
                op: FieldOp::Eq,
                value: FieldValue::Bool(true),
            }
        );
    }

    #[test]
    fn field_cmp_composes_with_boolean_ops() {
        assert!(matches!(
            p("field:status = active and kind:text"),
            QueryExpr::And(_, _)
        ));
        assert!(matches!(
            p("not field:expiry < 2026-01-01"),
            QueryExpr::Not(_)
        ));
    }

    #[test]
    fn field_cmp_missing_operator_errors() {
        let e = Parser::new("field:status active").parse();
        assert!(e.is_err(), "operator required between name and value");
    }

    #[test]
    fn field_cmp_missing_name_errors() {
        let e = Parser::new("field: = x").parse();
        assert!(e.is_err(), "field name required after 'field:'");
    }

    #[test]
    fn field_without_colon_is_text_term() {
        // `field` alone is an ordinary text term, not a keyword.
        assert_eq!(p("field"), QueryExpr::Text("field".into()));
    }
}
