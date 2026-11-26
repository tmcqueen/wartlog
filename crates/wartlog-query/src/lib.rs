//! WartLog's KQL subset (spec section 9): lexer, parser, planner with index pushdown, and a
//! parallel executor over store snapshots.

pub mod ast;
pub mod eval;
pub mod exec;
pub mod lexer;
pub mod parser;

pub use exec::{run, run_query, QueryOptions, QueryResult, QueryStats, TailFilter, LOG_COLUMNS, SPAN_COLUMNS};
pub use parser::{parse, parse_expr};

/// Words the parser treats specially in expressions; an attribute with one of these names must be
/// written `Attributes["name"]`.
pub const KEYWORDS: &[&str] = &[
    "and", "or", "not", "has", "contains", "startswith", "endswith", "hasprefix", "in", "between", "by", "asc", "desc", "true", "false", "null", "logs",
    "spans", "matches", "regex", "nulls", "first", "last",
];

pub fn is_keyword(w: &str) -> bool {
    let w = w.trim_start_matches('!');
    KEYWORDS.contains(&w) || w.ends_with("_cs") || eval::is_function(w) || eval::is_aggregate(w)
}

/// A query error with the byte range it refers to (for underlining in the viewer).
#[derive(Clone, Debug, PartialEq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[error("{message}")]
pub struct QueryError {
    pub message: String,
    pub start: usize,
    pub end: usize,
}

impl QueryError {
    pub fn new(m: impl Into<String>) -> QueryError {
        QueryError { message: m.into(), start: 0, end: 0 }
    }
    pub fn at(start: usize, end: usize, m: impl Into<String>) -> QueryError {
        QueryError { message: m.into(), start, end }
    }
}
