//! Metadata objects: schemas, tables, columns, indexes and sequences.
//!
//! These are *definitions* only: they describe what exists, never what data it
//! holds. The storage engine consumes them (it keeps an `Arc<TableDef>` per
//! relation) while the catalog stays the single source of truth for names,
//! OIDs and column layout.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use parking_lot::RwLock;
use vsql_types::DataType;

use crate::oid::Oid;

/// Access method of an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IndexMethod {
    /// Ordered index: supports equality and range predicates.
    BTree,
    /// Hash index: equality only.
    Hash,
}

impl IndexMethod {
    /// PostgreSQL's `pg_am.amname`.
    pub fn am_name(&self) -> &'static str {
        match self {
            IndexMethod::BTree => "btree",
            IndexMethod::Hash => "hash",
        }
    }
}

/// A column definition.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    /// Column name, already normalised by the parser.
    pub name: String,
    /// Declared type.
    pub data_type: DataType,
    /// `false` when the column was declared `NOT NULL`.
    pub nullable: bool,
    /// `DEFAULT` expression rendered as SQL text.
    ///
    /// The expression is stored as text because it is only evaluated when a row
    /// omits the column; keeping the rendered form avoids holding parser ASTs
    /// (`DEFAULT now()` must be re-evaluated for every insert anyway).
    pub default: Option<String>,
    /// Zero-based position in the table's column list.
    pub ordinal: usize,
    /// `true` when the column was dropped with `ALTER TABLE ... DROP COLUMN`.
    pub dropped: bool,
}

impl ColumnDef {
    /// Creates a simple nullable column at `ordinal`.
    pub fn new(name: impl Into<String>, data_type: DataType, ordinal: usize) -> Self {
        ColumnDef {
            name: name.into(),
            data_type,
            nullable: true,
            default: None,
            ordinal,
            dropped: false,
        }
    }

    /// Marks the column `NOT NULL`.
    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    /// Attaches a default expression (SQL text).
    pub fn with_default(mut self, expr: impl Into<String>) -> Self {
        self.default = Some(expr.into());
        self
    }
}

/// A table (or view) definition.
#[derive(Debug)]
pub struct TableDef {
    /// `pg_class.oid`.
    pub oid: Oid,
    /// OID of the containing schema.
    pub schema_oid: Oid,
    /// Table name.
    pub name: String,
    /// Columns, in physical order.
    pub columns: Vec<ColumnDef>,
    /// Column indices forming the primary key, if any.
    pub primary_key: Vec<usize>,
    /// OIDs of indexes on this table.
    ///
    /// Behind a lock because `CREATE INDEX` must attach itself to a table that
    /// other sessions are already reading.
    pub indexes: RwLock<Vec<Oid>>,
    /// `true` for `pg_catalog` relations.
    pub is_system: bool,
    /// Role that owns the table, as `ALTER TABLE ... OWNER TO` set it.
    ///
    /// `None` means "the bootstrap superuser", which is who owns everything the
    /// engine creates itself: the role that ran `CREATE TABLE` is not consulted,
    /// so a definition stays identical no matter which session created it.
    pub owner: Option<String>,
    /// Number of live rows, maintained by the storage engine.
    ///
    /// Kept here (rather than in the storage engine) so the planner can read an
    /// accurate row count without taking the storage lock.
    pub row_count: AtomicU64,
    /// `CHECK` constraints declared on this table, in declaration order.
    pub checks: Vec<CheckDef>,
    /// `FOREIGN KEY` constraints declared on this table, in declaration order.
    pub foreign_keys: Vec<ForeignKeyDef>,
}

/// A `CHECK` constraint.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckDef {
    /// `pg_constraint.oid`.
    pub oid: Oid,
    /// Constraint name; PostgreSQL's default is `{table}_{column}_check` for a
    /// column-level check.
    pub name: String,
    /// The condition, as SQL text.
    ///
    /// Like a stored `DEFAULT`, the expression is re-parsed and evaluated for
    /// every written row, which keeps the catalog free of parser ASTs.
    pub expr: String,
}

/// A `FOREIGN KEY` constraint.
///
/// The engine records the reference as metadata — `pg_constraint` reports the
/// referencing and referenced columns — and enforces it on insert and update
/// by probing the referenced table.
#[derive(Clone, Debug, PartialEq)]
pub struct ForeignKeyDef {
    /// `pg_constraint.oid`.
    pub oid: Oid,
    /// Constraint name; PostgreSQL's default is `{table}_{column}_fkey`.
    pub name: String,
    /// Zero-based positions of the referencing columns.
    pub columns: Vec<usize>,
    /// OID of the referenced table.
    pub ref_table: Oid,
    /// Zero-based positions of the referenced columns.
    pub ref_columns: Vec<usize>,
}

impl TableDef {
    /// Builds a table definition.
    pub fn new(
        oid: Oid,
        schema_oid: Oid,
        name: impl Into<String>,
        columns: Vec<ColumnDef>,
    ) -> Self {
        TableDef {
            oid,
            schema_oid,
            name: name.into(),
            columns,
            primary_key: Vec::new(),
            indexes: RwLock::new(Vec::new()),
            is_system: false,
            owner: None,
            row_count: AtomicU64::new(0),
            checks: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    /// Number of columns, including dropped ones (kept so ordinals stay stable).
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Looks up a column by name.
    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns
            .iter()
            .find(|c| !c.dropped && c.name.eq_ignore_ascii_case(name))
    }

    /// Resolves a column name to its ordinal.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| !c.dropped && c.name.eq_ignore_ascii_case(name))
    }

    /// `true` when the column at `index` participates in the primary key.
    pub fn is_primary_key(&self, index: usize) -> bool {
        self.primary_key.contains(&index)
    }

    /// Snapshot of this table's index OIDs.
    pub fn index_oids(&self) -> Vec<Oid> {
        self.indexes.read().clone()
    }

    /// Attaches an index to this table.
    pub fn add_index(&self, oid: Oid) {
        self.indexes.write().push(oid);
    }

    /// Detaches an index from this table.
    pub fn remove_index(&self, oid: Oid) {
        self.indexes.write().retain(|existing| *existing != oid);
    }

    /// Live row count, used for cost estimation and `pg_class.reltuples`.
    pub fn rows(&self) -> u64 {
        self.row_count.load(Ordering::Relaxed)
    }

    /// Adds `delta` to the live row count.
    pub fn add_rows(&self, delta: i64) {
        if delta >= 0 {
            self.row_count.fetch_add(delta as u64, Ordering::Relaxed);
        } else {
            self.row_count
                .fetch_sub(delta.unsigned_abs(), Ordering::Relaxed);
        }
    }

    /// Sets the live row count.
    pub fn set_rows(&self, value: u64) {
        self.row_count.store(value, Ordering::Relaxed);
    }
}

/// An index definition.
#[derive(Clone, Debug)]
pub struct IndexDef {
    /// `pg_class.oid` for the index relation.
    pub oid: Oid,
    /// The indexed table.
    pub table_oid: Oid,
    /// Index name.
    pub name: String,
    /// Indexed column ordinals, in key order.
    pub columns: Vec<usize>,
    /// `true` for `UNIQUE` indexes (uniqueness is enforced on insert).
    pub unique: bool,
    /// `true` when the index backs a `PRIMARY KEY`.
    pub primary: bool,
    /// Access method.
    pub method: IndexMethod,
}

impl IndexDef {
    /// Builds an index definition.
    pub fn new(
        oid: Oid,
        table_oid: Oid,
        name: impl Into<String>,
        columns: Vec<usize>,
        method: IndexMethod,
    ) -> Self {
        IndexDef {
            oid,
            table_oid,
            name: name.into(),
            columns,
            unique: false,
            primary: false,
            method,
        }
    }

    /// Marks the index `UNIQUE`.
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Marks the index as the primary key's index.
    pub fn primary_key(mut self) -> Self {
        self.unique = true;
        self.primary = true;
        self
    }
}

/// A schema (namespace).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaDef {
    /// `pg_namespace.oid`.
    pub oid: Oid,
    /// Schema name.
    pub name: String,
    /// `true` for `pg_catalog`/`information_schema`.
    pub is_system: bool,
}

/// A sequence object.
#[derive(Debug)]
pub struct SequenceDef {
    /// `pg_class.oid`.
    pub oid: Oid,
    /// Containing schema.
    pub schema_oid: Oid,
    /// Sequence name.
    pub name: String,
    /// The `START WITH` value, as `pg_sequence.seqstart` reports it.
    ///
    /// Kept separately from `last_value` because the latter changes on every
    /// `nextval` while the start value is part of the sequence's identity.
    pub start: i64,
    /// Next value to hand out.
    pub last_value: AtomicI64,
    /// Step.
    pub increment: i64,
    /// `MINVALUE`.
    pub min_value: i64,
    /// `MAXVALUE`.
    pub max_value: i64,
}

impl SequenceDef {
    /// Builds a sequence starting at `start`.
    pub fn new(
        oid: Oid,
        schema_oid: Oid,
        name: impl Into<String>,
        start: i64,
        increment: i64,
    ) -> Self {
        SequenceDef {
            oid,
            schema_oid,
            name: name.into(),
            start,
            last_value: AtomicI64::new(start - increment),
            increment,
            min_value: i64::MIN,
            max_value: i64::MAX,
        }
    }

    /// Atomically advances the sequence and returns the new value.
    pub fn nextval(&self) -> i64 {
        self.last_value.fetch_add(self.increment, Ordering::SeqCst) + self.increment
    }

    /// The most recent value handed out by `nextval`, for snapshot persistence.
    pub fn last_value(&self) -> i64 {
        self.last_value.load(Ordering::SeqCst)
    }
}

/// A database definition.
///
/// VelocitySQL mirrors PostgreSQL's hierarchy: one *cluster* (this process)
/// holds several databases, each of which owns its own schemas, relations and
/// OIDs. `DatabaseDef` is the cluster-level record of one of them; the runtime
/// state (catalog, rows, views) lives in `vsql-executor`, because that is where
/// the storage engine is.
///
/// Object OIDs are only unique *within* a database 鈥?the same is true in
/// PostgreSQL 鈥?while `DatabaseDef::oid` is unique across the cluster, because
/// `pg_database` is a shared catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseDef {
    /// `pg_database.oid`, unique across the cluster.
    pub oid: Oid,
    /// Database name, already normalised to lower case.
    pub name: String,
    /// `pg_database.datdba`: the role that owns the database.
    pub owner: String,
    /// `pg_database.datistemplate`: usable as a `CREATE DATABASE` template.
    pub is_template: bool,
}

impl DatabaseDef {
    /// Builds a database definition.
    pub fn new(oid: Oid, name: impl Into<String>, owner: impl Into<String>) -> Self {
        DatabaseDef {
            oid,
            name: name.into(),
            owner: owner.into(),
            is_template: false,
        }
    }

    /// Marks the database as a template.
    pub fn as_template(mut self) -> Self {
        self.is_template = true;
        self
    }
}
/// The maximum identifier length: PostgreSQL's `NAMEDATALEN - 1`, in bytes.
pub const MAX_IDENTIFIER_BYTES: usize = 63;

/// Folds an identifier the way PostgreSQL does and rejects what it never
/// accepts: empty names, names longer than 63 bytes, and `pg_`-prefixed names,
/// which are reserved for the system.
///
/// Truncation is deliberately *not* imitated: a database or role that cannot be
/// named back is worse than a rejected statement.
pub fn normalise_identifier(name: &str) -> Result<String, crate::error::CatalogError> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return Err(crate::error::CatalogError::Other(
            "identifier cannot be empty".into(),
        ));
    }
    if name.len() > MAX_IDENTIFIER_BYTES {
        return Err(crate::error::CatalogError::IdentifierTooLong(name));
    }
    if name.starts_with("pg_") {
        return Err(crate::error::CatalogError::ReservedDatabaseName(name));
    }
    Ok(name)
}

/// A role: the identity a session acts as (`pg_authid` / `pg_roles`).
///
/// Roles are *cluster* state, like databases: `CREATE ROLE` made in one database
/// is visible from every other, which is exactly PostgreSQL's behaviour.
///
/// Passwords are stored as a SCRAM verifier string in PostgreSQL's own format
/// (`SCRAM-SHA-256$<iterations>:<salt>$<stored key>:<server key>`), so a dump of
/// this field is portable into a real `pg_authid`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleDef {
    /// `pg_authid.oid`, unique across the cluster.
    pub oid: Oid,
    /// Role name, folded to lower case.
    pub name: String,
    /// `rolsuper`: bypasses every privilege check.
    pub superuser: bool,
    /// `rolcanlogin`: the role may be used to connect.
    pub can_login: bool,
    /// `rolcreatedb`: the role may run `CREATE DATABASE`.
    pub create_db: bool,
    /// `rolcreaterole`: the role may run `CREATE ROLE`.
    pub create_role: bool,
    /// `rolpassword`, in PostgreSQL's stored format.
    pub password: Option<String>,
}

impl RoleDef {
    /// A role with PostgreSQL's defaults: `NOLOGIN`, no privileges, no password.
    pub fn new(oid: Oid, name: impl Into<String>) -> Self {
        RoleDef {
            oid,
            name: name.into(),
            superuser: false,
            can_login: false,
            create_db: false,
            create_role: false,
            password: None,
        }
    }

    /// Grants `SUPERUSER`.
    pub fn superuser(mut self) -> Self {
        self.superuser = true;
        self
    }

    /// Grants `LOGIN`.
    pub fn login(mut self) -> Self {
        self.can_login = true;
        self
    }

    /// Grants `CREATEDB`.
    pub fn create_db(mut self) -> Self {
        self.create_db = true;
        self
    }

    /// Grants `CREATEROLE`.
    pub fn create_role(mut self) -> Self {
        self.create_role = true;
        self
    }

    /// Sets a password, hashing it with SCRAM-SHA-256.
    pub fn set_password(&mut self, password: &str) {
        self.password = Some(crate::scram::ScramVerifier::new(password).to_stored());
    }

    /// Removes the password (`PASSWORD NULL`).
    pub fn clear_password(&mut self) {
        self.password = None;
    }

    /// The stored verifier, when the role has a password.
    pub fn verifier(&self) -> Option<crate::scram::ScramVerifier> {
        self.password
            .as_deref()
            .and_then(crate::scram::ScramVerifier::parse)
    }

    /// `true` when `password` matches the stored verifier.
    ///
    /// A role without a password never authenticates: an absent verifier is not
    /// an empty password.
    pub fn check_password(&self, password: &str) -> bool {
        match self.verifier() {
            Some(verifier) => verifier.verify_password(password),
            None => false,
        }
    }

    /// The role's attributes, as `\du` and `pg_roles` report them.
    pub fn attributes(&self) -> Vec<&'static str> {
        let mut attributes = Vec::new();
        if self.superuser {
            attributes.push("Superuser");
        }
        if self.can_login {
            attributes.push("Login");
        }
        if self.create_db {
            attributes.push("Create DB");
        }
        if self.create_role {
            attributes.push("Create role");
        }
        if attributes.is_empty() {
            attributes.push("Cannot login");
        }
        attributes
    }
}
