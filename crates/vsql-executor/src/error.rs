//! Execution errors.
//!
//! Every layer below (parser, catalog, storage, types) contributes its own
//! error type; this enum flattens them so a session can hand a single
//! PostgreSQL-shaped message to the protocol layer without inspecting sources.

use vsql_catalog::CatalogError;
use vsql_parser::ParserError;
use vsql_storage::StorageError;
use vsql_types::TypeError;

/// An error raised while planning or executing a statement.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    /// The SQL text could not be parsed or lowered.
    #[error("{0}")]
    Parser(#[from] ParserError),
    /// A catalog lookup failed.
    #[error("{0}")]
    Catalog(#[from] CatalogError),
    /// The storage engine rejected the write.
    #[error("{0}")]
    Storage(#[from] StorageError),
    /// A value could not be converted or compared.
    #[error("{0}")]
    Type(#[from] TypeError),

    /// A column name is not visible in the current scope.
    #[error("column \"{0}\" does not exist")]
    UndefinedColumn(String),
    /// A column name matches more than one relation in the scope.
    #[error("column reference \"{0}\" is ambiguous")]
    AmbiguousColumn(String),
    /// A qualifier does not name anything in the `FROM` clause.
    #[error("missing FROM-clause entry for table \"{0}\"")]
    MissingFromEntry(String),
    /// A scalar function was called with an unknown name.
    #[error("function {0} does not exist")]
    UndefinedFunction(String),
    /// An aggregate was used where PostgreSQL forbids one.
    #[error("aggregate functions are not allowed in {0}")]
    AggregateNotAllowed(String),
    /// A column outside `GROUP BY` was selected without an aggregate.
    #[error(
        "column \"{0}\" must appear in the GROUP BY clause or be used in an aggregate function"
    )]
    UngroupedColumn(String),
    /// `SELECT DISTINCT` with an `ORDER BY` key that is not selected.
    #[error("for SELECT DISTINCT, ORDER BY expressions must appear in select list")]
    DistinctOrderBy,
    /// Integer division by zero.
    #[error("division by zero")]
    DivisionByZero,
    /// A subquery used as an expression produced more than one row.
    #[error("more than one row returned by a subquery used as an expression")]
    SubqueryTooManyRows,
    /// `LIMIT`/`OFFSET` was not a non-negative constant integer.
    #[error("argument of LIMIT must not contain variables")]
    InvalidLimit,
    /// A statement produced no plan (e.g. an empty statement list).
    #[error("empty statement")]
    EmptyStatement,
    /// PostgreSQL forbids this statement inside a transaction block.
    #[error("{0} cannot run inside a transaction block")]
    ActiveSqlTransaction(String),
    /// A configuration parameter has a fixed value and cannot be `SET`.
    #[error("parameter \"{0}\" cannot be changed")]
    CannotChangeParameter(String),
    /// `$n` was evaluated without a value bound to it — the 1-based position of
    /// the parameter is carried for the message.
    #[error("there is no parameter ${0}")]
    UndefinedParameter(usize),

    /// A construct VelocitySQL recognises but does not implement yet.
    #[error("feature not supported: {0}")]
    NotSupported(String),
    /// A `CHECK` constraint rejected the row: PostgreSQL's class 23 constraint
    /// violation, `check_violation`.
    #[error("{0}")]
    CheckViolation(String),
    /// Anything else, already phrased like a PostgreSQL message.
    #[error("{0}")]
    Other(String),
}

impl ExecError {
    /// The five-character SQLSTATE PostgreSQL would attach to this error.
    ///
    /// The protocol layer needs it to fill the `C` field of an
    /// `ErrorResponse`; drivers branch on it.
    pub fn sqlstate(&self) -> &'static str {
        match self {
            // A statement PostgreSQL cannot even parse is `syntax_error`; one it
            // recognises but refuses to run is `feature_not_supported`.
            ExecError::Parser(ParserError::NotSupported(_)) => "0A000",
            ExecError::Parser(_) => "42601",
            ExecError::Catalog(CatalogError::UndefinedTable(_)) => "42P01",
            ExecError::Catalog(CatalogError::RelationExists(_)) => "42P07",
            ExecError::Catalog(CatalogError::UndefinedSchema(_)) => "3F000",
            ExecError::Catalog(CatalogError::UndefinedColumn { .. }) => "42703",
            ExecError::Catalog(CatalogError::DuplicateColumn(_)) => "42701",
            ExecError::Catalog(CatalogError::UndefinedConstraint { .. }) => "42704",
            ExecError::Catalog(CatalogError::DatabaseExists(_)) => "42P04",
            ExecError::Catalog(CatalogError::UndefinedDatabase(_)) => "3D000",
            ExecError::Catalog(CatalogError::CannotDropCurrentDatabase) => "55006",
            ExecError::Catalog(CatalogError::CannotDropCurrentRole) => "55006",
            ExecError::Catalog(CatalogError::CannotDropSuperuser(_)) => "55006",
            ExecError::Catalog(CatalogError::UndefinedRole(_)) => "42704",
            ExecError::Catalog(CatalogError::RoleExists(_)) => "42710",
            ExecError::Catalog(CatalogError::PermissionDenied(_)) => "42501",
            ExecError::Catalog(CatalogError::PasswordAuthenticationFailed(_)) => "28P01",
            ExecError::Catalog(CatalogError::RoleCannotLogin(_)) => "28000",
            ExecError::Catalog(CatalogError::DatabaseInUse(_)) => "55006",
            ExecError::Catalog(CatalogError::ReservedDatabaseName(_)) => "42939",
            ExecError::Catalog(CatalogError::IdentifierTooLong(_)) => "42622",
            ExecError::Catalog(_) => "42P01",
            ExecError::Storage(StorageError::UniqueViolation { .. }) => "23505",
            ExecError::Storage(StorageError::NotNullViolation { .. }) => "23502",
            ExecError::Storage(_) => "XX000",
            ExecError::Type(TypeError::InvalidInputSyntax { .. }) => "22P02",
            ExecError::Type(TypeError::OutOfRange { .. }) => "22003",
            ExecError::Type(_) => "42883",
            ExecError::UndefinedColumn(_) | ExecError::MissingFromEntry(_) => "42703",
            ExecError::AmbiguousColumn(_) => "42702",
            ExecError::UndefinedFunction(_) => "42883",
            ExecError::UngroupedColumn(_) => "42803",
            ExecError::AggregateNotAllowed(_) => "42803",
            ExecError::DistinctOrderBy => "42P10",
            ExecError::DivisionByZero => "22012",
            ExecError::SubqueryTooManyRows => "21000",
            ExecError::InvalidLimit => "2201W",
            ExecError::EmptyStatement => "42601",
            ExecError::ActiveSqlTransaction(_) => "25001",
            ExecError::CannotChangeParameter(_) => "55P02",
            ExecError::UndefinedParameter(_) => "42P02",
            ExecError::NotSupported(_) => "0A000",
            ExecError::CheckViolation(_) => "23514",
            ExecError::Other(_) => "XX000",
        }
    }

    /// The 1-based source offset of a syntax error, when known.
    ///
    /// Forwarded as the `POSITION` field of a PostgreSQL `ErrorResponse`, which
    /// is what lets clients point at the offending token instead of at the whole
    /// statement.
    pub fn position(&self) -> Option<usize> {
        match self {
            ExecError::Parser(error) => error.position(),
            _ => None,
        }
    }

    /// Builds a generic error from any displayable payload.
    pub fn other(message: impl Into<String>) -> Self {
        ExecError::Other(message.into())
    }

    /// Builds a `feature not supported` error.
    pub fn not_supported(what: impl Into<String>) -> Self {
        ExecError::NotSupported(what.into())
    }
}
