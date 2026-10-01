//! Front-end errors, phrased like PostgreSQL's `ERROR:` lines.

use sqlparser::parser::ParserError as SqlParserError;

/// An error produced while turning SQL text into VelocitySQL's AST.
///
/// The variants carry the fields the protocol layer needs to emit a proper
/// PostgreSQL `ErrorResponse` (message, optional position, optional detail).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParserError {
    /// The text is not valid SQL.
    #[error("{message}")]
    Syntax {
        /// Full message, e.g. `syntax error at or near "FROM"`.
        message: String,
        /// 1-based character offset of the offending token, when known.
        position: Option<usize>,
    },
    /// The SQL is valid but uses something VelocitySQL does not implement yet.
    #[error("feature not supported: {0}")]
    NotSupported(String),
    /// A literal could not be decoded (bad escape, malformed number, ...).
    #[error("{0}")]
    Invalid(String),
}

impl ParserError {
    /// Builds a `syntax error at or near "<token>"` error.
    pub fn syntax_at(token: &str, position: Option<usize>) -> Self {
        ParserError::Syntax {
            message: format!("syntax error at or near \"{token}\""),
            position,
        }
    }

    /// The `POSITION` field of a PostgreSQL error response, if any.
    pub fn position(&self) -> Option<usize> {
        match self {
            ParserError::Syntax { position, .. } => *position,
            _ => None,
        }
    }

    /// Converts a `sqlparser-rs` failure into a PostgreSQL-shaped message.
    ///
    /// `sqlparser` reports `Expected: <x>, found: <y>`; PostgreSQL reports
    /// `syntax error at or near "<y>"`. Rewriting the former into the latter
    /// keeps client-side error handling (and our own regress tests) identical
    /// to PostgreSQL's.
    pub fn from_sqlparser(err: &SqlParserError) -> Self {
        let raw = err.to_string();
        let message = raw.strip_prefix("sql parser error: ").unwrap_or(&raw);
        if let Some((_, found)) = message.split_once(", found: ") {
            // `sqlparser` reports `found: <token> at Line: n, Column: m`; only the
            // token belongs in the message, otherwise the location leaks into the
            // quoted span and clients display `at or near "  at Line: 1"`.
            let token = found
                .split(',')
                .next()
                .unwrap_or(found)
                .split(" at Line: ")
                .next()
                .unwrap_or(found)
                .trim()
                .trim_matches('"');
            if !token.is_empty() {
                let token = if token.eq_ignore_ascii_case("EOF") {
                    "end of input".to_string()
                } else {
                    token.to_string()
                };
                return ParserError::syntax_at(&token, None);
            }
        }
        // Nothing to point at: keep the parser's own wording rather than
        // inventing a token that was never there.
        ParserError::Syntax {
            message: message.to_string(),
            position: None,
        }
    }
}
