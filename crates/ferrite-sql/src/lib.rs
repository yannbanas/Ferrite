//! Owned by Agent 2, alongside `ferrite-catalog`. Lexer + parser for the
//! Ferrite v1 SQL dialect subset (see this crate's README for the exact
//! scope) producing an AST consumed by `ferrite-planner`. Depends on
//! nothing but `ferrite-common` — this crate has no notion of storage or
//! execution.
//!
//! The parser is a hand-written recursive-descent front end with
//! precedence climbing for expressions. It is written on the assumption
//! that its input is hostile: every failure path returns a
//! [`ParseError`], nothing indexes or unwraps optimistically, numeric
//! literals that do not fit are rejected rather than wrapped, and grammar
//! recursion is depth-capped so nested input cannot exhaust the stack.
//!
//! ```
//! use ferrite_sql::{ast::Statement, parse_statement};
//!
//! let stmt = parse_statement("SELECT id FROM users WHERE age >= 18").unwrap();
//! assert!(matches!(stmt, Statement::Query(_)));
//!
//! assert!(parse_statement("SELECT FROM").is_err());
//! ```

use thiserror::Error;

pub mod ast;
pub mod lexer;
mod parser;

pub use parser::{parse, parse_statement};

/// A lexing or parsing failure, carrying the byte offset in the original
/// query text so callers can point at the offending token.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message} (at offset {offset})")]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl From<ParseError> for ferrite_common::FerriteError {
    fn from(err: ParseError) -> Self {
        ferrite_common::FerriteError::Parse(err.to_string())
    }
}
