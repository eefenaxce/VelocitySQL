//! Storage-engine errors.

/// Errors raised by the in-memory storage engine.
///
/// Constraint violations are reported here (not in the executor) because the
/// storage engine is the only place that sees every write, including ones from
/// the WAL replay path in milestone M2.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// A `UNIQUE`/`PRIMARY KEY` index rejected the row.
    #[error("duplicate key value violates unique constraint \"{index}\"")]
    UniqueViolation {
        /// Name of the violated index.
        index: String,
    },
    /// A `NOT NULL` column received `NULL`.
    #[error(
        "null value in column \"{column}\" of relation \"{table}\" violates not-null constraint"
    )]
    NotNullViolation {
        /// Relation name.
        table: String,
        /// Column name.
        column: String,
    },
    /// The row has the wrong arity for the table.
    #[error("row has {actual} values but relation \"{table}\" has {expected} columns")]
    ArityMismatch {
        /// Relation name.
        table: String,
        /// Expected number of values.
        expected: usize,
        /// Actual number of values.
        actual: usize,
    },
    /// The relation is not known to the engine.
    #[error("relation \"{0}\" does not exist")]
    UndefinedTable(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}
