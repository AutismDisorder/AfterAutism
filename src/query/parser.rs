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
//! | field ":" value (kind: / re: / prefix: / text: — other selectors are refused)
//! | ("->" | "<-") (ident | "(" ident ("|" ident)* ")") (":" int)? ":" "(" expr ")" (traversal)
//! | "re" ":" quoted (regex)
//! | "prefix" ":" value (label prefix match)
//! | "text" ":" value (explicit full-text term)
//! | quoted
//! term := bare word or quoted string
//! ```

use crate::adapter::FieldValue;
use crate::query::error::{QueryError, Result};
use crate::query::ir::{FieldOp, QueryExpr, TraverseDirection};

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    LParen,
    RParen,
    Colon,
    Arrow,   // ->
    ArrowIn, // <-
    And,
    Or,
    Pipe, // | (edge-type union inside traversal specs)
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
                    } else if self.peek() == Some(&'-') {
                        self.bump();
                        out.push((Tok::ArrowIn, start));
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
                        out.push((Tok::Pipe, start));
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

/// Recursive-descent parser for the query language. Construct with
/// [`Parser::new`], run with [`Parser::parse`]. Lexing happens eagerly
/// in `new` (one pass, no backtracking); parse errors carry the byte
/// position of the offending token.
pub struct Parser {
    toks: Vec<(Tok, usize)>,
    idx: usize,
    /// Lexer error captured at construction; surfaced on [`Parser::parse`].
    lex_error: Option<QueryError>,
}

impl Parser {
    /// Tokenize `text` up front; a lex error is captured and surfaced
    /// on the next [`Parser::parse`] call.
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

    /// Peek the token `k` positions ahead without advancing (clamped to
    /// the end-of-input token).
    fn peek_at(&self, k: usize) -> &Tok {
        self.toks
            .get(self.idx + k)
            .map_or(&self.toks[self.toks.len() - 1].0, |t| &t.0)
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

    /// Parse the token stream into a [`QueryExpr`], or fail with a
    /// byte-positioned [`QueryError::Parse`].
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
        // Traversal: [-> | <-] [ident | (ident | ident ...)] [":" N ":"] ":" "(" expr ")"
        if matches!(self.peek(), Tok::Arrow | Tok::ArrowIn) {
            let direction = match self.bump() {
                Tok::Arrow => TraverseDirection::Outgoing,
                Tok::ArrowIn => TraverseDirection::Incoming,
                _ => unreachable!("peeked an arrow token"),
            };
            // Edge type, or a parenthesized union of edge types.
            let mut edge_types: Vec<String> = Vec::new();
            if matches!(self.peek(), Tok::LParen) {
                self.bump();
                loop {
                    let Tok::Ident(t) = self.bump() else {
                        return Err(QueryError::parse(self.pos(), "expected edge type in union"));
                    };
                    edge_types.push(t);
                    if matches!(self.peek(), Tok::Pipe) {
                        self.bump();
                        continue;
                    }
                    break;
                }
                if !matches!(self.peek(), Tok::RParen) {
                    return Err(QueryError::parse(
                        self.pos(),
                        "expected ')' after edge-type union",
                    ));
                }
                self.bump();
            } else {
                let Tok::Ident(t) = self.bump() else {
                    return Err(QueryError::parse(
                        self.pos(),
                        "expected edge type after arrow",
                    ));
                };
                edge_types.push(t);
            }
            // Optional hop count: `:N` between the type and the final
            // colon (the colon after N is the final separator itself, so
            // `->a:2:(x)` carries exactly the colons shown).
            // Disambiguated by lookahead (a numeric ident) so
            // `->parent:(...)` keeps its historic meaning.
            let mut depth: usize = 1;
            if matches!(self.peek(), Tok::Colon) {
                let depth_form =
                    matches!(self.peek_at(1), Tok::Ident(s) if s.parse::<usize>().is_ok());
                if depth_form {
                    self.bump(); // ':'
                    let Tok::Ident(s) = self.bump() else {
                        unreachable!("lookahead verified a numeric ident")
                    };
                    let n: usize = s.parse().expect("lookahead verified numeric");
                    if n == 0 {
                        return Err(QueryError::parse(
                            self.pos(),
                            "traversal depth must be >= 1",
                        ));
                    }
                    depth = n;
                    // The closing colon is consumed by the final-colon
                    // check below.
                }
            }
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
                    "expected '(' after traversal spec",
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
                direction,
                edge_types,
                depth,
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
                            "prefix" => Ok(QueryExpr::Prefix(v)),
                            "text" => Ok(QueryExpr::Text(v)),
                            // Labels carry no structured fields, so any
                            // other selector is a hard refusal — a
                            // silently-ignored selector would make two
                            // different queries mean the same thing.
                            _ => Err(QueryError::unknown("field", s)),
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

    fn p_result(s: &str) -> crate::query::Result<QueryExpr> {
        Parser::new(s).parse()
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
    fn prefix_field() {
        assert_eq!(p("prefix:ali"), QueryExpr::Prefix("ali".into()));
        assert_eq!(
            p("prefix:\"multi word\""),
            QueryExpr::Prefix("multi word".into())
        );
    }

    #[test]
    fn traversal() {
        assert_eq!(
            p("->parent:(alice)"),
            QueryExpr::Traverse {
                inner: Box::new(QueryExpr::Text("alice".into())),
                direction: TraverseDirection::Outgoing,
                edge_types: vec!["parent".into()],
                depth: 1,
            }
        );
    }

    #[test]
    fn traversal_incoming_depth_and_union() {
        assert_eq!(
            p("<-(a|b):2:(x)"),
            QueryExpr::Traverse {
                inner: Box::new(QueryExpr::Text("x".into())),
                direction: TraverseDirection::Incoming,
                edge_types: vec!["a".into(), "b".into()],
                depth: 2,
            }
        );
        assert_eq!(
            p("->a:3:(x)"),
            QueryExpr::Traverse {
                inner: Box::new(QueryExpr::Text("x".into())),
                direction: TraverseDirection::Outgoing,
                edge_types: vec!["a".into()],
                depth: 3,
            }
        );
    }

    #[test]
    fn traversal_zero_depth_is_refused() {
        assert!(p_result("->a:0:(x)").is_err());
    }

    #[test]
    fn unknown_field_selector_is_refused() {
        let e = p_result("title:alice");
        assert!(matches!(e, Err(QueryError::Unknown { what, .. }) if what == "field"));
    }

    #[test]
    fn hostile_inputs_never_panic() {
        // A fixed battery of malformed and adversarial inputs: every one
        // must produce a typed error or a valid expression — never a
        // panic. This is the poor-man's fuzz wall for the lexer/parser.
        let hostile = [
            "",
            " ",
            "((((((((((",
            "))))))))))",
            "a or",
            "and",
            "or or or",
            "not not not",
            "->->",
            "<-",
            "->(a|):(x)",
            "->(a|b",
            "->(a|b)|(x)",
            "field: = ",
            "field:x",
            "field:x = ",
            "\"unterminated",
            "'unterminated",
            "re:(",
            "prefix:",
            "kind:",
            "text:",
            "->a:99999999999999999999:(x)",
            "->a:-1:(x)",
            "a &&& b",
            "a ||| b",
            "= = =",
            "(*",
            "field:x = 999999999999999999999999999999999",
            "->(a|b|c|d|e):1:(x)",
            "\\",
            "\u{fffd}",
            "日本語のクエリ",
            "a\tb\nc",
            "->a:2:(prefix:x) and not kind:text",
            "\"a\"\"b\"",
            "'say \"hi\"'",
        ];
        for input in hostile {
            // Result is either Ok or a typed QueryError; the assertion is
            // simply that this does not panic.
            let _ = Parser::new(input).parse();
        }
    }

    #[test]
    fn seeded_expression_generator_roundtrips() {
        // Deterministic pseudo-random composition of valid atoms (tiny
        // LCG — no external dependencies): parse -> display -> parse
        // must land on the identical expression, pinning the canonical
        // render as a faithful representation of the IR.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize
        };
        let atoms = [
            "alice",
            "kind:text",
            "kind:full_page",
            "prefix:ali",
            "text:bob",
            "field:status = active",
            "field:amount >= 5",
            "field:expiry > 2026-01-01",
            "field:renewed = true",
            "field:ratio < 4.5",
            "->parent:(alice)",
            "<-parent:(alice)",
            "->(a|b):2:(bob)",
            "->a:3:(prefix:c)",
        ];
        for _ in 0..300 {
            let parts = 1 + next() % 3;
            let mut text = String::new();
            for i in 0..parts {
                if i > 0 {
                    text.push_str(if next() % 2 == 0 { " and " } else { " or " });
                }
                if next() % 5 == 0 {
                    text.push_str("not ");
                }
                text.push_str(atoms[next() % atoms.len()]);
            }
            let expr = p(&text);
            let rendered = expr.display();
            let reparsed = p(&rendered);
            assert_eq!(
                expr, reparsed,
                "round-trip mismatch for {text:?} -> {rendered:?}"
            );
        }
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
