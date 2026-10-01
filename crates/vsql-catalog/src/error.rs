//! Catalog errors.

/// Errors raised by catalog operations.
///
/// Messages match PostgreSQL's wording so they can be forwarded to clients
/// unchanged and compared against `pg` behaviour in the compatibility tests.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    /// A relation with this name already exists.
    #[error("relation \"{0}\" already exists")]
    RelationExists(String),
    /// No relation with this name.
    #[error("relation \"{0}\" does not exist")]
    UndefinedTable(String),
    /// A schema with this name already exists.
    #[error("schema \"{0}\" already exists")]
    SchemaExists(String),
    /// No schema with this name.
    #[error("schema \"{0}\" does not exist")]
    UndefinedSchema(String),
    /// The same column was declared twice.
    #[error("column \"{0}\" specified more than once")]
    DuplicateColumn(String),
    /// An index with this name already exists.
    #[error("relation \"{0}\" already exists")]
    IndexExists(String),
    /// A referenced column does not exist on the table.
    #[error("column \"{column}\" of relation \"{table}\" does not exist")]
    UndefinedColumn {
        /// Table name.
        table: String,
        /// Column name.
        column: String,
    },
    /// `ALTER TABLE ... DROP CONSTRAINT` named a constraint that is not there.
    #[error("constraint \"{constraint}\" of relation \"{table}\" does not exist")]
    UndefinedConstraint {
        /// Table name.
        table: String,
        /// Constraint name.
        constraint: String,
    },
    /// No database with this name.
    #[error("database \"{0}\" does not exist")]
    UndefinedDatabase(String),
    /// A database with this name already exists.
    #[error("database \"{0}\" already exists")]
    DatabaseExists(String),
    /// `DROP DATABASE` targeted the database the session is connected to.
    #[error("cannot drop the currently open database")]
    CannotDropCurrentDatabase,
    /// `DROP ROLE` targeted the role the session acts as.
    #[error("current user cannot be dropped")]
    CannotDropCurrentRole,
    /// The bootstrap superuser may not be dropped: without it the engine has no
    /// way back in, and VelocitySQL has no single-user recovery mode.
    #[error("cannot drop the bootstrap superuser \"{0}\"")]
    CannotDropSuperuser(String),
    /// Another session is still attached to the database.
    #[error("database \"{0}\" is being accessed by other users")]
    DatabaseInUse(String),
    /// Names starting with `pg_` are reserved for the system.
    #[error("unacceptable database name \"{0}\"")]
    ReservedDatabaseName(String),
    /// PostgreSQL's `NAMEDATALEN - 1` limit is 63 bytes.
    #[error("identifier \"{0}\" is too long (maximum 63 bytes)")]
    IdentifierTooLong(String),
    /// No role with this name.
    #[error("role \"{0}\" does not exist")]
    UndefinedRole(String),
    /// A role with this name already exists.
    #[error("role \"{0}\" already exists")]
    RoleExists(String),
    /// The session's role is not allowed to perform the operation.
    #[error("permission denied to {0}")]
    PermissionDenied(String),
    /// Authentication failed; deliberately the same message for an unknown role
    /// and for a wrong password, so the server cannot be used to enumerate users.
    #[error("password authentication failed for user \"{0}\"")]
    PasswordAuthenticationFailed(String),
    /// The role exists but has no `LOGIN` attribute.
    #[error("role \"{0}\" is not permitted to log in")]
    RoleCannotLogin(String),
    /// The statement referenced an object kind VelocitySQL has no catalog for.
    #[error("{0}")]
    Other(String),
}
