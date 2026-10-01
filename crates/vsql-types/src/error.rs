//! Type-system level errors.

use crate::data_type::DataType;

/// Errors raised by value conversion / casting / comparison.
///
/// Messages are intentionally shaped like PostgreSQL's `ERROR:` lines so the
/// protocol layer can forward them almost verbatim.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TypeError {
    /// A textual value could not be parsed as the requested type.
    #[error("invalid input syntax for type {ty}: \"{input}\"")]
    InvalidInputSyntax {
        /// Target type name (PostgreSQL spelling).
        ty: String,
        /// Offending literal.
        input: String,
    },
    /// Two values cannot be compared without an explicit cast.
    #[error("operator does not exist: {left} {op} {right}")]
    UndefinedComparison {
        /// Left operand type.
        left: String,
        /// Operator symbol.
        op: String,
        /// Right operand type.
        right: String,
    },
    /// A cast between two types has no implementation.
    #[error("cannot cast type {from} to {to}")]
    CannotCast {
        /// Source type (PostgreSQL spelling).
        from: String,
        /// Target type (PostgreSQL spelling).
        to: String,
    },
    /// A numeric value does not fit the target type.
    #[error("value out of range for type {ty}")]
    OutOfRange {
        /// Target type (PostgreSQL spelling).
        ty: String,
    },
    /// Generic arithmetic failure (division by zero, overflow, ...).
    #[error("{0}")]
    Other(String),
}

impl TypeError {
    /// Builds an [`TypeError::Other`] from any displayable payload.
    pub fn other(msg: impl Into<String>) -> Self {
        TypeError::Other(msg.into())
    }

    /// Builds an [`TypeError::CannotCast`] from two data types.
    pub fn cannot_cast(from: &DataType, to: &DataType) -> Self {
        TypeError::CannotCast {
            from: from.format_type(),
            to: to.format_type(),
        }
    }
}
